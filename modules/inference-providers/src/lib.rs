use inori_module_api::*;
pub fn validate_profile(p: &ProviderProfile) -> Result<()> {
    if p.id.is_empty()
        || p.id.len() > 128
        || !(1024..=100_000).contains(&p.max_context_chars)
        || p.max_output_tokens == 0
        || p.max_output_tokens > 4096
        || !(1..=300).contains(&p.timeout_seconds)
    {
        bail!("invalid inference profile limits");
    }
    if p.kind == "extension" {
        if p.extension.as_deref().is_none_or(str::is_empty)
            || p.capability.as_deref().is_none_or(str::is_empty)
        {
            bail!("extension provider needs extension and capability IDs");
        }
        return Ok(());
    }
    if !matches!(p.kind.as_str(), "mock" | "ollama" | "compatible_http") {
        bail!("unsupported provider kind");
    }
    if p.kind != "mock" {
        let url = reqwest::Url::parse(&p.base_url)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || p.cloud && url.scheme() != "https"
        {
            bail!("invalid inference provider URL");
        }
    }
    Ok(())
}
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct HttpProvider {
    profile: ProviderProfile,
    client: reqwest::Client,
    credential: Option<String>,
}
impl HttpProvider {
    pub fn new(profile: ProviderProfile) -> Result<Self> {
        Self::with_credential(profile, None)
    }
    /// Explicit bootstrap injection; never stored in the serializable provider profile.
    pub fn with_credential(profile: ProviderProfile, credential: Option<String>) -> Result<Self> {
        if !matches!(profile.kind.as_str(), "mock" | "ollama" | "compatible_http") {
            bail!("unsupported provider kind");
        }
        if profile.max_context_chars < 1024
            || profile.max_context_chars > 100_000
            || profile.max_output_tokens == 0
            || profile.max_output_tokens > 4096
            || profile.timeout_seconds == 0
            || profile.timeout_seconds > 300
        {
            bail!("provider limits are outside allowed bounds");
        }
        if profile.kind != "mock" {
            let url = reqwest::Url::parse(&profile.base_url)?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
            {
                bail!("invalid provider URL");
            }
            if profile.cloud && url.scheme() != "https" {
                bail!("cloud providers require HTTPS");
            }
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(profile.timeout_seconds))
            .build()?;
        Ok(Self {
            profile,
            client,
            credential,
        })
    }
}

#[async_trait]
impl InferenceProvider for HttpProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }
    async fn generate(
        &self,
        context: GenerationContext,
        output: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        if self.profile.kind == "mock" {
            let last = context
                .messages
                .last()
                .map(|(_, text)| text.as_str())
                .unwrap_or("");
            let text = if context.purpose == "extract_memory" {
                if last.contains("好き") || last.contains("覚えて") {
                    json!({"memories": [{"text": format!("ユーザー発言: {}", last.chars().take(120).collect::<String>()), "expires_at": Value::Null}]}).to_string()
                } else {
                    "{\"memories\":[]}".into()
                }
            } else if context.purpose == "autonomy" {
                "次に会ったときに最近の興味を聞いてみよう。これは次の会話の候補として記録しておくね。".into()
            } else if last.contains("こんにちは") {
                "こんにちは！今日は何を話そうか。あなたのことも、少しずつ知っていきたいな。".into()
            } else {
                format!(
                    "「{}」だね。聞かせてくれてありがとう。もう少し教えてくれる？",
                    last.chars().take(90).collect::<String>()
                )
            };
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(8) {
                tokio::select! { _ = cancel.cancelled() => return Ok(()), result = output.send(chunk.iter().collect()) => { if result.is_err() { return Ok(()); } } }
                tokio::task::yield_now().await;
            }
            return Ok(());
        }
        let mut messages = vec![json!({"role":"system", "content":context.system})];
        messages.extend(
            context
                .messages
                .iter()
                .map(|(role, text)| json!({"role":role, "content":text})),
        );
        let base = self.profile.base_url.trim_end_matches('/');
        let (url, mut body) = if self.profile.kind == "ollama" {
            (
                format!("{base}/api/chat"),
                json!({"model":self.profile.model,"messages":messages,"stream":true,"keep_alive":"30m","options":{"num_predict":context.max_output_tokens}}),
            )
        } else {
            (
                format!("{base}/chat/completions"),
                json!({"model":self.profile.model,"messages":messages,"stream":true,"max_tokens":context.max_output_tokens}),
            )
        };
        if self.profile.kind == "ollama"
            && let Some(thinking) = self.profile.thinking
        {
            body["think"] = json!(thinking);
        }
        let mut request = self.client.post(url).json(&body);
        if let Some(name) = &self.profile.api_key_env {
            let key = std::env::var(name)
                .ok()
                .or_else(|| self.credential.clone())
                .context("configured provider credential is missing")?;
            request = request.bearer_auth(key);
        }
        let response = tokio::select! { _ = cancel.cancelled() => return Ok(()), result = request.send() => result.context("provider request failed")? };
        if !response.status().is_success() {
            bail!("provider returned HTTP {}", response.status().as_u16());
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut total = 0usize;
        loop {
            let item = tokio::select! { _ = cancel.cancelled() => return Ok(()), item = stream.next() => item };
            let Some(item) = item else { break };
            pending.extend_from_slice(&item?);
            if pending.len() > 262_144 {
                bail!("provider stream frame too large");
            }
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let line = std::str::from_utf8(&line)?.trim();
                if emit_line(line, &output, &cancel, &mut total).await? {
                    return Ok(());
                }
            }
        }
        if !pending.is_empty() {
            emit_line(
                std::str::from_utf8(&pending)?.trim(),
                &output,
                &cancel,
                &mut total,
            )
            .await?;
        }
        Ok(())
    }
}

async fn emit_line(
    line: &str,
    output: &mpsc::Sender<String>,
    cancel: &CancellationToken,
    total: &mut usize,
) -> Result<bool> {
    let line = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Ok(false);
    }
    if line == "[DONE]" {
        return Ok(true);
    }
    let value: Value = serde_json::from_str(line).context("invalid provider stream JSON")?;
    if value.get("error").is_some() {
        bail!("provider reported a stream error");
    }
    if let Some(text) = value
        .pointer("/message/content")
        .or_else(|| value.pointer("/choices/0/delta/content"))
        .or_else(|| value.get("response"))
        .and_then(Value::as_str)
    {
        *total += text.len();
        if *total > 65_536 {
            bail!("provider output limit exceeded");
        }
        if !text.is_empty() {
            tokio::select! { _ = cancel.cancelled() => return Ok(true), result = output.send(text.into()) => { if result.is_err() { return Ok(true); } } }
        }
    }
    Ok(value.get("done").and_then(Value::as_bool).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn parsers_handle_both_stream_formats() {
        let (tx, mut rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let mut count = 0;
        emit_line(
            "data: {\"choices\":[{\"delta\":{\"content\":\"こんにちは\"}}]}",
            &tx,
            &cancel,
            &mut count,
        )
        .await
        .unwrap();
        emit_line(
            "{\"message\":{\"content\":\"！\"},\"done\":false}",
            &tx,
            &cancel,
            &mut count,
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap(), "こんにちは");
        assert_eq!(rx.recv().await.unwrap(), "！");
        assert!(
            emit_line("data: [DONE]", &tx, &cancel, &mut count)
                .await
                .unwrap()
        );
    }
}

pub struct ProviderModule;
inori_module_api::module!(ProviderModule, "inference.providers");
pub fn definition() -> ModuleDescriptor {
    let mut d = descriptor(
        "inference.providers",
        "inference.generate",
        &[],
        no_settings(),
    );
    d.max_concurrency = 2;
    d
}
mod rpc;
pub use rpc::{build_providers, build_providers_with_credentials};

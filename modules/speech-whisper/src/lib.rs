use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use inori_module_api::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
async fn limited(response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > max {
            bail!("voice engine response exceeds limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn remote(url: &str) -> Result<bool> {
    let url = reqwest::Url::parse(url)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("invalid voice engine URL");
    }
    Ok(!matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    ))
}

pub struct Whisper {
    url: String,
    http: reqwest::Client,
    remote: bool,
}
pub fn validate_url(url: &str) -> Result<()> {
    remote(url)?;
    Ok(())
}
impl Whisper {
    pub fn new(url: String) -> Result<Self> {
        let remote = remote(&url)?;
        Ok(Self {
            url: url.trim_end_matches('/').into(),
            remote,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()?,
        })
    }
}
module!(Whisper, "speech.whisper");
#[async_trait::async_trait]
impl SpeechRecognizer for Whisper {
    fn remote(&self) -> bool {
        self.remote
    }
    async fn transcribe(&self, wav: Vec<u8>, cancel: CancellationToken) -> Result<String> {
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav)
                    .file_name("speech.wav")
                    .mime_str("audio/wav")?,
            )
            .text("response_format", "json")
            .text("language", "ja");
        let task = async {
            let response = self
                .http
                .post(format!("{}/inference", self.url))
                .multipart(form)
                .send()
                .await
                .context("whisper.cpp request failed")?;
            if !response.status().is_success() {
                bail!("whisper.cpp transcription failed");
            }
            let value: Value = serde_json::from_slice(&limited(response, 65_536).await?)?;
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .context("transcription response has no text")?
                .trim()
                .to_string();
            inori_domain::validate_text(&text, inori_domain::MAX_TEXT_BYTES)?;
            Ok(text)
        };
        tokio::select! {_=cancel.cancelled()=>bail!("transcription cancelled"),result=task=>result}
    }
}
pub fn definition() -> ModuleDescriptor {
    let mut d = descriptor("speech.whisper", "speech.recognize", &[], no_settings());
    d.max_concurrency = 4;
    d
}

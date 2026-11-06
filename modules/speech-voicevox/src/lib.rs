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

pub struct Voicevox {
    url: String,
    http: reqwest::Client,
    remote: bool,
    speaker: u32,
    cancellable: bool,
}
pub fn validate_url(url: &str) -> Result<()> {
    remote(url)?;
    Ok(())
}
impl Voicevox {
    pub fn new(url: String, speaker: u32, cancellable: bool) -> Result<Self> {
        let remote = remote(&url)?;
        Ok(Self {
            url: url.trim_end_matches('/').into(),
            remote,
            speaker,
            cancellable,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()?,
        })
    }
}
module!(Voicevox, "speech.voicevox");
#[async_trait::async_trait]
impl SpeechSynthesizer for Voicevox {
    fn remote(&self) -> bool {
        self.remote
    }
    async fn synthesize(&self, text: &str, cancel: CancellationToken) -> Result<Vec<u8>> {
        let task = async {
            let response = self
                .http
                .post(format!("{}/audio_query", self.url))
                .query(&[("text", text), ("speaker", &self.speaker.to_string())])
                .send()
                .await
                .context("VOICEVOX query failed")?;
            if !response.status().is_success() {
                bail!("VOICEVOX audio_query failed");
            }
            let query: Value = serde_json::from_slice(&limited(response, 65_536).await?)?;
            let endpoint = if self.cancellable {
                "cancellable_synthesis"
            } else {
                "synthesis"
            };
            let response = self
                .http
                .post(format!("{}/{endpoint}", self.url))
                .query(&[("speaker", self.speaker)])
                .json(&query)
                .send()
                .await
                .context("VOICEVOX synthesis request failed")?;
            if !response.status().is_success() {
                bail!("VOICEVOX synthesis failed");
            }
            limited(response, 16 * 1024 * 1024).await
        };
        tokio::select! {_=cancel.cancelled()=>bail!("speech synthesis cancelled"),result=task=>result}
    }
}
pub fn definition() -> ModuleDescriptor {
    descriptor("speech.voicevox", "speech.synthesize", &[], no_settings())
}

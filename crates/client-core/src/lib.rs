//! UI-neutral networking and native audio. No GPUI dependency.
pub mod audio;
mod backend;
use anyhow::{Context, Result, bail};
pub use backend::*;
use futures_util::{SinkExt, StreamExt};
use inori_domain::Snapshot;
use inori_protocol::{ClientHello, EventEnvelope, PROTOCOL_VERSION, ServerCapabilities};
use reqwest::{
    Method,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_tungstenite::{Connector, tungstenite::client::IntoClientRequest};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub server_url: String,
    pub token_file: PathBuf,
    pub certificate_file: Option<PathBuf>,
    #[serde(default)]
    pub insecure_loopback: bool,
}
impl ClientConfig {
    pub fn local() -> Result<Self> {
        let base = directories_path()?;
        Ok(Self {
            server_url: "https://localhost:9473".into(),
            token_file: base.join("owner.token"),
            certificate_file: Some(base.join("server.pem")),
            insecure_loopback: false,
        })
    }
}
fn directories_path() -> Result<PathBuf> {
    Ok(directories::ProjectDirs::from("dev", "Inori", "Inori")
        .context("application data directory unavailable")?
        .data_local_dir()
        .into())
}
#[derive(Clone)]
pub struct ApiClient {
    base: String,
    token: String,
    http: reqwest::Client,
    tls: Option<Arc<rustls::ClientConfig>>,
}
impl ApiClient {
    pub fn new(config: &ClientConfig) -> Result<Self> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let url = reqwest::Url::parse(&config.server_url)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || !matches!(url.path(), "" | "/")
        {
            bail!("server URL must contain only scheme, host, and port");
        }
        let local = matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if url.scheme() != "https" && !(url.scheme() == "http" && config.insecure_loopback && local)
        {
            bail!(
                "HTTPS required; HTTP is allowed only with an explicit loopback development setting"
            );
        }
        let token = std::fs::read_to_string(&config.token_file)
            .context("device token file cannot be read")?
            .trim()
            .to_owned();
        if token.len() < 16 || token.len() > 256 {
            bail!("invalid device token file");
        }
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(120));
        let mut tls = None;
        if let Some(path) = &config.certificate_file {
            let pem = std::fs::read(path).context("paired server certificate cannot be read")?;
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                roots.add(cert?)?;
            }
            tls = Some(Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ));
        }
        Ok(Self {
            base: config.server_url.trim_end_matches('/').into(),
            token,
            http: builder.build()?,
            tls,
        })
    }
    pub async fn call<T: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> Result<Value> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let value: Value = serde_json::from_slice(&bounded_body(response, 4 * 1024 * 1024).await?)
            .context("server response was not JSON")?;
        if !status.is_success() {
            bail!(
                "{}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("server request rejected")
            );
        }
        Ok(value)
    }
    pub async fn snapshot(&self) -> Result<Snapshot> {
        Ok(serde_json::from_value(
            self.call::<Value>(Method::GET, "/v1/snapshot", None)
                .await?,
        )?)
    }
    pub async fn availability(&self) -> Result<ServerCapabilities> {
        let response = self
            .http
            .get(format!("{}/v1/capabilities", self.base))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // Previous v1 servers support text but cannot advertise voice availability.
            return Ok(ServerCapabilities {
                protocol_major: 1,
                text_chat: true,
                transcription: false,
                speech: false,
                presentation: true,
                actions: true,
                reason: Some("旧サーバーへの接続中は音声を利用できません。".into()),
            });
        }
        let response = response.error_for_status()?;
        Ok(serde_json::from_slice(
            &bounded_body(response, 65_536).await?,
        )?)
    }
    pub async fn bytes(&self, id: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(format!("{}/v1/resources/{id}", self.base))
            .send()
            .await?
            .error_for_status()?;
        bounded_body(response, 16 * 1024 * 1024).await
    }
    pub async fn upload_wav(&self, bytes: Vec<u8>) -> Result<String> {
        let response = self
            .http
            .post(format!("{}/v1/resources?audience=private", self.base))
            .header("Content-Type", "audio/wav")
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;
        let value: Value = response.json().await?;
        Ok(value
            .get("id")
            .and_then(Value::as_str)
            .context("resource response missing ID")?
            .into())
    }
    pub async fn listen(
        &self,
        session: &str,
        output: mpsc::Sender<EventEnvelope>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut url = reqwest::Url::parse(&self.base)?;
        url.set_scheme(if self.base.starts_with("https:") {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| anyhow::anyhow!("invalid WebSocket URL"))?;
        url.set_path("/v1/events");
        url.query_pairs_mut().append_pair("session_id", session);
        let mut request = url.as_str().into_client_request()?;
        let mut header = HeaderValue::from_str(&format!("Bearer {}", self.token))?;
        header.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, header);
        let connector = self.tls.clone().map(Connector::Rustls);
        let (mut socket, _) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
                .await?;
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&ClientHello {
                    kind: "client.hello".into(),
                    protocol_major: 1,
                    session_id: session.into(),
                    text: true,
                    audio_input: true,
                    audio_output: true,
                    presentation: true,
                    audio_mime_types: vec!["audio/wav".into()],
                })?
                .into(),
            ))
            .await?;
        output
            .send(EventEnvelope {
                protocol_version: PROTOCOL_VERSION.into(),
                id: "connected".into(),
                kind: "client.connected".into(),
                session_id: Some(session.into()),
                generation_id: None,
                audience: inori_protocol::Audience::Private,
                payload: json!({}),
            })
            .await?;
        loop {
            let message =
                tokio::select! {_=cancel.cancelled()=>return Ok(()),message=socket.next()=>message};
            let Some(message) = message else {
                bail!("event connection closed")
            };
            if let tokio_tungstenite::tungstenite::Message::Text(text) = message? {
                if text.len() > 1_048_576 {
                    bail!("event exceeds size limit");
                }
                let event: EventEnvelope = serde_json::from_str(&text)?;
                if event.protocol_version.split('.').next() != Some("1") {
                    bail!("unsupported event protocol");
                }
                tokio::select! {_=cancel.cancelled()=>return Ok(()),result=output.send(event)=>{result.context("event consumer closed")?;}}
            }
        }
    }
}
async fn bounded_body(response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > max {
            bail!("resource too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plaintext_and_credentials_in_urls_are_rejected() {
        let config = ClientConfig {
            server_url: "http://192.168.1.2:9473".into(),
            token_file: "missing.token".into(),
            certificate_file: None,
            insecure_loopback: true,
        };
        assert!(
            ApiClient::new(&config)
                .err()
                .unwrap()
                .to_string()
                .contains("HTTPS")
        );
    }
}

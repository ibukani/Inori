//! UI-neutral networking and native audio. No GPUI dependency.
pub mod audio;
mod backend;
use anyhow::{Context, Result, bail};
pub use backend::*;
use futures_util::{SinkExt, StreamExt};
pub use inori_app_support::ClientConfig;
use inori_domain::Snapshot;
use inori_protocol::{ClientHello, EventEnvelope, PROTOCOL_VERSION, ServerCapabilities};
use reqwest::{
    Method,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{Connector, tungstenite::client::IntoClientRequest};
use tokio_util::sync::CancellationToken;

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
        inori_app_support::validate_server_url(&config.server_url, config.insecure_loopback)?;
        let token = std::fs::read_to_string(&config.token_file)
            .context("端末トークンを読み込めません。接続用ファイルを取り込んでください")?
            .trim()
            .to_owned();
        if token.len() < 16 || token.len() > 256 {
            bail!("端末トークンの形式が不正です");
        }
        let http = inori_app_support::connection_http(config, std::time::Duration::from_secs(120))?;
        let mut tls = None;
        if let Some(path) = &config.certificate_file {
            let pem = std::fs::read(path).context("paired server certificate cannot be read")?;
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
            http,
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
        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(inori_app_support::connection_error(&e)))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("端末認証が失効または無効です。接続用ファイルを再発行してください");
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            bail!("この端末には操作する権限がありません");
        }
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
            .await
            .map_err(|e| anyhow::anyhow!(inori_app_support::connection_error(&e)))?;
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
        let response = response
            .error_for_status()
            .map_err(|e| anyhow::anyhow!(inori_app_support::connection_error(&e)))?;
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
                .await
                .map_err(|error| {
                    use tokio_tungstenite::tungstenite::Error;
                    match error {
                        Error::Http(response)
                            if response.status() == reqwest::StatusCode::UNAUTHORIZED =>
                        {
                            anyhow::anyhow!(
                                "端末認証が失効または無効です。接続用ファイルを再発行してください"
                            )
                        }
                        Error::Http(response)
                            if response.status() == reqwest::StatusCode::FORBIDDEN =>
                        {
                            anyhow::anyhow!("この端末には接続先を利用する権限がありません")
                        }
                        Error::Tls(_) => anyhow::anyhow!(
                            "証明書が一致しません。接続名と証明書を確認して再ペアリングしてください"
                        ),
                        _ => anyhow::anyhow!(
                            "サーバーへ接続できません。起動状態・URL・LAN/VPNを確認してください"
                        ),
                    }
                })?;
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

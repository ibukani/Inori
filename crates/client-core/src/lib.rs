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
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    Connector, WebSocketStream,
    tungstenite::{client::IntoClientRequest, handshake::client::Response},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ApiClient {
    base: String,
    token: String,
    http: reqwest::Client,
    tls: Option<Arc<rustls::ClientConfig>>,
    config: ClientConfig,
    local_route: Arc<tokio::sync::RwLock<Option<LocalRoute>>>,
    use_local_route: Arc<AtomicBool>,
}
#[derive(Clone)]
struct LocalRoute {
    target: SocketAddr,
    http: reqwest::Client,
    base: String,
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
            config: config.clone(),
            local_route: Arc::new(tokio::sync::RwLock::new(None)),
            use_local_route: Arc::new(AtomicBool::new(false)),
        })
    }
    async fn active_route(&self) -> (reqwest::Client, String) {
        if self.use_local_route.load(Ordering::Acquire)
            && let Some(route) = self.local_route.read().await.as_ref()
        {
            return (route.http.clone(), route.base.clone());
        }
        (self.http.clone(), self.base.clone())
    }
    async fn set_local_route(&self, target: SocketAddr) -> Result<()> {
        let http = inori_app_support::connection_http_via(
            &self.config,
            std::time::Duration::from_secs(120),
            Some(target),
        )?;
        let base = inori_app_support::url_via(&self.config.server_url, Some(target))?;
        *self.local_route.write().await = Some(LocalRoute { target, http, base });
        Ok(())
    }
    pub async fn prefer_lan_if_available(&self) {
        let Some(path) = &self.config.certificate_file else {
            return;
        };
        let Ok(pem) = std::fs::read(path) else {
            return;
        };
        let Ok(fingerprint) = inori_app_support::certificate_fingerprint(&pem) else {
            return;
        };
        if let Ok(Some(target)) = inori_app_support::discovery::discover_local(&fingerprint).await
            && self.set_local_route(target).await.is_ok()
        {
            self.use_local_route.store(true, Ordering::Release);
        }
    }
    pub async fn call<T: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> Result<Value> {
        let (http, base) = self.active_route().await;
        let mut request = http.request(method, format!("{base}{path}"));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|e| {
            if self.use_local_route.load(Ordering::Acquire) {
                self.use_local_route.store(false, Ordering::Release);
            }
            anyhow::anyhow!(inori_app_support::connection_error(&e))
        })?;
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
        let (http, base) = self.active_route().await;
        let response = http
            .get(format!("{base}/v1/capabilities"))
            .send()
            .await
            .map_err(|e| {
                if self.use_local_route.load(Ordering::Acquire) {
                    self.use_local_route.store(false, Ordering::Release);
                }
                anyhow::anyhow!(inori_app_support::connection_error(&e))
            })?;
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
        let (http, base) = self.active_route().await;
        let response = http
            .get(format!("{base}/v1/resources/{id}"))
            .send()
            .await
            .map_err(|e| {
                if self.use_local_route.load(Ordering::Acquire) {
                    self.use_local_route.store(false, Ordering::Release);
                }
                anyhow::anyhow!(inori_app_support::connection_error(&e))
            })?
            .error_for_status()?;
        bounded_body(response, 16 * 1024 * 1024).await
    }
    pub async fn upload_wav(&self, bytes: Vec<u8>) -> Result<String> {
        let (http, base) = self.active_route().await;
        let response = http
            .post(format!("{base}/v1/resources?audience=private"))
            .header("Content-Type", "audio/wav")
            .body(bytes)
            .send()
            .await
            .map_err(|e| {
                if self.use_local_route.load(Ordering::Acquire) {
                    self.use_local_route.store(false, Ordering::Release);
                }
                anyhow::anyhow!(inori_app_support::connection_error(&e))
            })?
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
        if !self.use_local_route.load(Ordering::Acquire) {
            self.prefer_lan_if_available().await;
        }
        let mut active_local = self.use_local_route.load(Ordering::Acquire);
        let local_target = self
            .local_route
            .read()
            .await
            .as_ref()
            .map(|route| route.target);
        let mut result = self
            .connect_events(session, active_local.then_some(local_target).flatten())
            .await;
        if result.is_err() && active_local {
            active_local = false;
            self.use_local_route.store(false, Ordering::Release);
            result = self.connect_events(session, None).await;
        }
        if result.is_err() && !active_local {
            if let Some(path) = &self.config.certificate_file
                && let Ok(pem) = std::fs::read(path)
                && let Ok(fingerprint) = inori_app_support::certificate_fingerprint(&pem)
                && let Ok(Some(target)) =
                    inori_app_support::discovery::discover_local(&fingerprint).await
                && self.set_local_route(target).await.is_ok()
            {
                result = self.connect_events(session, Some(target)).await;
                if result.is_ok() {
                    self.use_local_route.store(true, Ordering::Release);
                }
            }
        }
        let (mut socket, _) = result.map_err(map_websocket_error)?;
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

    async fn connect_events(
        &self,
        session: &str,
        local_target: Option<SocketAddr>,
    ) -> Result<(
        WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Response,
    )> {
        let mut url = reqwest::Url::parse(&self.base)?;
        url.set_scheme(if self.base.starts_with("https:") {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| anyhow::anyhow!("invalid WebSocket URL"))?;
        url.set_path("/v1/events");
        if let Some(target) = local_target {
            url.set_port(Some(target.port()))
                .map_err(|_| anyhow::anyhow!("LAN connection port is invalid"))?;
        }
        url.query_pairs_mut().append_pair("session_id", session);
        let mut request = url.as_str().into_client_request()?;
        let mut header = HeaderValue::from_str(&format!("Bearer {}", self.token))?;
        header.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, header);
        let connector = self.tls.clone().map(Connector::Rustls);
        let connect = async {
            if let Some(target) = local_target {
                let stream = tokio::net::TcpStream::connect(target).await?;
                Ok::<_, anyhow::Error>(
                    tokio_tungstenite::client_async_tls_with_config(
                        request, stream, None, connector,
                    )
                    .await?,
                )
            } else {
                Ok::<_, anyhow::Error>(
                    tokio_tungstenite::connect_async_tls_with_config(
                        request, None, false, connector,
                    )
                    .await?,
                )
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), connect)
            .await
            .context("connection timed out")?
    }
}

fn map_websocket_error(error: anyhow::Error) -> anyhow::Error {
    use tokio_tungstenite::tungstenite::Error;
    match error.downcast_ref::<Error>() {
        Some(Error::Http(response)) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            anyhow::anyhow!("端末認証が失効または無効です。接続用ファイルを再発行してください")
        }
        Some(Error::Http(response)) if response.status() == reqwest::StatusCode::FORBIDDEN => {
            anyhow::anyhow!("この端末には接続先を利用する権限がありません")
        }
        Some(Error::Tls(_)) => anyhow::anyhow!(
            "証明書が一致しません。接続名と証明書を確認して再ペアリングしてください"
        ),
        _ => anyhow::anyhow!("サーバーへ接続できません。起動状態・URL・LANを確認してください"),
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

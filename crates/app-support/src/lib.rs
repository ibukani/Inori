//! Shared application setup. No GUI, audio devices, character state, or server runtime.
use anyhow::{Context, Result, bail};
use inori_protocol::{ConnectionBundle, ServerCapabilities};
use reqwest::{Client, StatusCode, header::HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const DESKTOP_APP: &str = "desktop";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub server_url: String,
    pub token_file: PathBuf,
    pub certificate_file: Option<PathBuf>,
    #[serde(default)]
    pub insecure_loopback: bool,
}

impl ClientConfig {
    /// Preserve the original same-user, same-machine connection.
    pub fn local() -> Result<Self> {
        let base = server_data_directory()?;
        Ok(Self {
            server_url: "https://localhost:9473".into(),
            token_file: base.join("owner.token"),
            certificate_file: Some(base.join("server.pem")),
            insecure_loopback: false,
        })
    }
}

pub fn server_data_directory() -> Result<PathBuf> {
    Ok(directories::ProjectDirs::from("dev", "Inori", "Inori")
        .context("application data directory unavailable")?
        .data_local_dir()
        .into())
}

pub fn validate_app_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("app ID must contain 1..64 lowercase ASCII letters, digits, or hyphens");
    }
    Ok(())
}

pub fn app_directory(app_id: &str) -> Result<PathBuf> {
    validate_app_id(app_id)?;
    Ok(directories::ProjectDirs::from("dev", "Inori", "Inori")
        .context("application config directory unavailable")?
        .config_dir()
        .join("apps")
        .join(app_id))
}

pub fn client_config_path(app_id: &str) -> Result<PathBuf> {
    Ok(app_directory(app_id)?.join("client.toml"))
}

pub fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("file exceeds the allowed size");
    }
    Ok(bytes)
}

/// Explicit config > per-app config > legacy portable config > legacy local defaults.
pub fn load_client_config(
    explicit: Option<&Path>,
    saved: &Path,
    portable: &Path,
) -> Result<(ClientConfig, Option<PathBuf>)> {
    let selected = explicit
        .or_else(|| saved.exists().then_some(saved))
        .or_else(|| portable.exists().then_some(portable));
    let Some(path) = selected else {
        return Ok((ClientConfig::local()?, None));
    };
    let path = std::path::absolute(path)?;
    let mut config: ClientConfig =
        toml::from_str(std::str::from_utf8(&read_limited(&path, 64 * 1024)?)?)
            .context("接続設定を読み込めません。設定ファイルの形式を確認してください")?;
    let parent = path.parent().context("config parent unavailable")?;
    if config.token_file.is_relative() {
        config.token_file = parent.join(&config.token_file);
    }
    if let Some(cert) = &mut config.certificate_file
        && cert.is_relative()
    {
        *cert = parent.join(&*cert);
    }
    validate_server_url(&config.server_url, config.insecure_loopback)?;
    Ok((config, Some(path)))
}

pub fn validate_server_url(value: &str, insecure_loopback: bool) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).context("サーバーURLの形式を確認してください")?;
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || !(url.scheme() == "https" || (url.scheme() == "http" && insecure_loopback && local))
    {
        bail!("HTTPSの接続先を指定してください。ユーザー情報・パス・クエリーは指定できません");
    }
    Ok(url)
}

/// Files/directories are secured before any secret bytes are written.
pub fn restrict_path(path: &Path, directory: bool) -> Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("symbolic links cannot hold application credentials");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let identity = std::process::Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .creation_flags(0x08000000)
            .output()?;
        if !identity.status.success() {
            bail!("cannot determine credential owner");
        }
        // whoami may encode a Japanese account name in the OEM codepage; only the ASCII SID is used.
        let text = String::from_utf8_lossy(&identity.stdout);
        let sid = text
            .trim()
            .rsplit(',')
            .next()
            .context("user SID missing")?
            .trim_matches('"');
        if !sid.starts_with("S-1-")
            || !sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'S' || b == b'-')
        {
            bail!("invalid user SID");
        }
        let grant = format!("*{sid}:{}", if directory { "(OI)(CI)F" } else { "F" });
        let status = std::process::Command::new("icacls.exe")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &grant])
            .creation_flags(0x08000000)
            .output()?;
        if !status.status.success() {
            bail!("認証情報のファイル権限を設定できません");
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = directory;
        bail!("credential protection is unsupported on this OS");
    }
    Ok(())
}

pub fn private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    restrict_path(path, true)
}

pub fn write_private(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    if path.exists() && std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("refusing to replace a symbolic link");
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    restrict_path(temporary.path(), false)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    if replace {
        temporary.persist(path)?;
    } else {
        temporary.persist_noclobber(path)?;
    }
    Ok(())
}

pub fn save_client_config(path: &Path, config: &ClientConfig) -> Result<()> {
    validate_server_url(&config.server_url, config.insecure_loopback)?;
    let mut config = config.clone();
    config.token_file = std::path::absolute(&config.token_file)?;
    config.certificate_file = config
        .certificate_file
        .map(std::path::absolute)
        .transpose()?;
    write_private(path, toml::to_string_pretty(&config)?.as_bytes(), true)
}

pub fn read_bundle(path: &Path, app_id: &str) -> Result<ConnectionBundle> {
    let bundle: ConnectionBundle = serde_json::from_slice(&read_limited(path, 64 * 1024)?)
        .context("接続用ファイルの形式を確認してください")?;
    validate_bundle(&bundle, app_id)?;
    Ok(bundle)
}

pub fn validate_bundle(bundle: &ConnectionBundle, app_id: &str) -> Result<()> {
    validate_app_id(app_id)?;
    if bundle.schema_version != 1
        || bundle.app_id != app_id
        || bundle.device_id.is_empty()
        || bundle.device_id.len() > 128
        || !(16..=256).contains(&bundle.token.len())
        || !bundle
            .token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("接続用ファイルの版・対象アプリ・端末認証情報が一致しません");
    }
    validate_server_url(&bundle.server_url, false)?;
    reqwest::Certificate::from_pem(bundle.certificate_pem.as_bytes())
        .context("公開証明書を読み込めません")?;
    if bundle.certificate_pem.contains("PRIVATE KEY") {
        bail!("接続用ファイルに秘密鍵を含められません");
    }
    Ok(())
}

pub fn certificate_fingerprint(pem: &[u8]) -> Result<String> {
    let cert = rustls_pemfile::certs(&mut &*pem)
        .next()
        .context("certificate missing")??;
    Ok(format!("{:x}", Sha256::digest(cert.as_ref())))
}

pub fn verify_certificate_name(pem: &[u8], server_url: &str) -> Result<()> {
    use rustls::client::danger::ServerCertVerifier;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let url = validate_server_url(server_url, false)?;
    let host = url
        .host_str()
        .context("host missing")?
        .trim_matches(['[', ']']);
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
    let certs = rustls_pemfile::certs(&mut &*pem).collect::<std::result::Result<Vec<_>, _>>()?;
    let cert = certs.first().context("certificate missing")?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone())?;
    let verifier =
        rustls::client::WebPkiServerVerifier::builder(std::sync::Arc::new(roots)).build()?;
    verifier.verify_server_cert(cert, &certs[1..], &name, &[], rustls::pki_types::UnixTime::now())
        .map_err(|_| anyhow::anyhow!("証明書が接続名または有効期限と一致しません。元の接続名を使うか、証明書更新と再ペアリングを行ってください"))?;
    Ok(())
}

pub struct PreparedConnection {
    directory: tempfile::TempDir,
    pub config: ClientConfig,
}

impl PreparedConnection {
    /// Connection checks must succeed before the caller commits a new default.
    pub fn commit(self, destination: &Path) -> Result<ClientConfig> {
        save_client_config(destination, &self.config)?;
        let _ = self.directory.keep();
        Ok(self.config)
    }
}

pub fn prepare_bundle(
    bundle: &ConnectionBundle,
    app_id: &str,
    directory: &Path,
) -> Result<PreparedConnection> {
    validate_bundle(bundle, app_id)?;
    private_directory(directory)?;
    let temporary = tempfile::Builder::new()
        .prefix("connection-")
        .tempdir_in(directory)?;
    restrict_path(temporary.path(), true)?;
    let config = ClientConfig {
        server_url: bundle.server_url.clone(),
        token_file: temporary.path().join("device.token"),
        certificate_file: Some(temporary.path().join("server.pem")),
        insecure_loopback: false,
    };
    write_private(&config.token_file, bundle.token.as_bytes(), false)?;
    write_private(
        config.certificate_file.as_ref().unwrap(),
        bundle.certificate_pem.as_bytes(),
        false,
    )?;
    Ok(PreparedConnection {
        directory: temporary,
        config,
    })
}

pub fn connection_http(config: &ClientConfig, timeout: Duration) -> Result<Client> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    validate_server_url(&config.server_url, config.insecure_loopback)?;
    let bytes = read_limited(&config.token_file, 512)
        .context("端末トークンを読み込めません。接続用ファイルを取り込んでください")?;
    let token = std::str::from_utf8(&bytes)?.trim();
    if !(16..=256).contains(&token.len()) {
        bail!("端末トークンの形式が不正です");
    }
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))?;
    authorization.set_sensitive(true);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    let mut builder = Client::builder()
        .default_headers(headers)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(path) = &config.certificate_file {
        let pem = read_limited(path, 64 * 1024).context("サーバー証明書を読み込めません")?;
        builder = builder.tls_certs_only([reqwest::Certificate::from_pem(&pem)?]);
    }
    Ok(builder.build()?)
}

pub fn connection_error(error: &reqwest::Error) -> &'static str {
    // Do not return the underlying error's URL, headers, or remote response body.
    if error.status() == Some(StatusCode::UNAUTHORIZED) {
        return "端末認証が失効または無効です。接続用ファイルを再発行してください";
    }
    if error.status() == Some(StatusCode::FORBIDDEN) {
        return "この端末には接続先を利用する権限がありません";
    }
    if error.is_timeout() {
        "接続がタイムアウトしました。サーバーの稼働とLAN/VPNを確認してください"
    } else {
        use std::error::Error;
        let mut source = error.source();
        while let Some(cause) = source {
            if cause.downcast_ref::<rustls::Error>().is_some() {
                return "証明書が一致しません。接続名と証明書を確認し、必要なら再ペアリングしてください";
            }
            source = cause.source();
        }
        "サーバーへ接続できません。起動状態・URL・ファイアウォールを確認してください"
    }
}

pub async fn probe(config: &ClientConfig) -> Result<ServerCapabilities> {
    let client = connection_http(config, Duration::from_secs(5))?;
    let mut response = client
        .get(format!(
            "{}/v1/capabilities",
            config.server_url.trim_end_matches('/')
        ))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(connection_error(&e)))?;
    match response.status() {
        StatusCode::UNAUTHORIZED => {
            bail!("端末認証が失効または無効です。接続用ファイルを再発行してください")
        }
        StatusCode::FORBIDDEN => bail!("この端末には接続先を利用する権限がありません"),
        status if !status.is_success() => bail!(
            "サーバーの機能確認に失敗しました（HTTP {}）",
            status.as_u16()
        ),
        _ => {}
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!(connection_error(&e)))?
    {
        if bytes.len() + chunk.len() > 64 * 1024 {
            bail!("機能情報が大きすぎます");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("サーバーの機能情報が対応形式ではありません")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_precedence_relative_paths_and_atomic_failure() {
        let work = tempfile::tempdir().unwrap();
        let saved = work.path().join("saved.toml");
        let portable = work.path().join("portable.toml");
        let mut config = ClientConfig::local().unwrap();
        config.server_url = "https://portable:9473".into();
        config.token_file = "device.token".into();
        config.certificate_file = None;
        std::fs::write(&portable, toml::to_string(&config).unwrap()).unwrap();
        let (loaded, _) = load_client_config(None, &saved, &portable).unwrap();
        assert_eq!(loaded.token_file, work.path().join("device.token"));
        config.server_url = "https://saved:9473".into();
        save_client_config(&saved, &config).unwrap();
        assert_eq!(
            load_client_config(None, &saved, &portable)
                .unwrap()
                .0
                .server_url,
            config.server_url
        );
        assert_eq!(
            load_client_config(Some(&portable), &saved, &portable)
                .unwrap()
                .0
                .server_url,
            "https://portable:9473"
        );
        assert!(write_private(&saved, b"bad", false).is_err());
        assert!(load_client_config(None, &saved, &portable).is_ok());
        for url in [
            "http://192.168.0.2:9473",
            "https://user:pass@host",
            "https://host/?token=x",
            "https://host/#x",
        ] {
            assert!(validate_server_url(url, false).is_err());
        }
    }

    #[test]
    fn bundle_roundtrip_cancel_and_app_isolation() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let bundle = ConnectionBundle {
            schema_version: 1,
            server_url: "https://localhost:9473".into(),
            certificate_pem: cert.cert.pem(),
            device_id: "synthetic".into(),
            app_id: "desktop".into(),
            token: "synthetic-token-123456789".into(),
        };
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("client.toml");
        let prepared = prepare_bundle(&bundle, "desktop", work.path()).unwrap();
        let token_path = prepared.config.token_file.clone();
        drop(prepared);
        assert!(!token_path.exists());
        assert!(!path.exists());
        assert!(prepare_bundle(&bundle, "other", work.path()).is_err());
        let prepared = prepare_bundle(&bundle, "desktop", work.path()).unwrap();
        let config = prepared.commit(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(config.token_file).unwrap(),
            bundle.token
        );
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("token_file")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(work.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

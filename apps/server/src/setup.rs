use anyhow::{Context, Result, bail};
use clap::Args;
use inori_app_support::{self as support, ClientConfig};
use inori_protocol::{ConnectionBundle, CreateDeviceRequest, CreateDeviceResponse};
use inori_runtime::Repository;
use inori_server::{ServerConfig, issue_device, resolve_config, validate_character};
use inori_storage_sqlite::SqliteRepository;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Args, Default)]
pub struct SetupOptions {
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long)]
    pub lan: bool,
    #[arg(long)]
    pub server_url: Option<String>,
    #[arg(long, value_parser = ["mock", "ollama", "compatible_http"])]
    pub provider: Option<String>,
    #[arg(long)]
    pub provider_url: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub api_key_env: Option<String>,
    /// Read a key from a protected local file, never a command-line value.
    #[arg(long)]
    pub api_key_file: Option<PathBuf>,
    #[arg(long)]
    pub cloud: bool,
    #[arg(long)]
    pub allow_private: bool,
    #[arg(long)]
    pub autonomy_cloud: bool,
    #[arg(long)]
    pub whisper_url: Option<String>,
    #[arg(long)]
    pub voicevox_url: Option<String>,
    #[arg(long)]
    pub allow_private_remote_voice: bool,
    #[arg(long)]
    pub autostart: bool,
    #[arg(long)]
    pub start: bool,
    #[arg(long)]
    pub check_engines: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSettings {
    pub listen: SocketAddr,
    pub server_url: String,
    pub tls_names: Vec<String>,
}
impl Default for LaunchSettings {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9473".parse().unwrap(),
            server_url: "https://localhost:9473".into(),
            tls_names: vec![],
        }
    }
}

pub fn read_launch(data: &Path) -> Result<LaunchSettings> {
    let path = data.join("launch.toml");
    if path.exists() {
        Ok(toml::from_str(std::str::from_utf8(
            &support::read_limited(&path, 65536)?,
        )?)?)
    } else {
        Ok(LaunchSettings::default())
    }
}

pub fn load_config(path: Option<&Path>) -> Result<ServerConfig> {
    let Some(path) = path else {
        return Ok(ServerConfig::default());
    };
    let mut config: ServerConfig = toml::from_str(std::str::from_utf8(&support::read_limited(
        path,
        1024 * 1024,
    )?)?)?;
    let parent = std::path::absolute(path)?
        .parent()
        .context("config parent missing")?
        .to_path_buf();
    for extension in &mut config.extensions {
        if extension.is_relative() {
            *extension = parent.join(&*extension);
        }
    }
    Ok(config)
}

pub fn load_credentials(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = support::read_limited(path, 64 * 1024).context("cannot read credential file")?;
    let parsed: BTreeMap<String, String> = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| toml::from_str(text).ok())
        .context("credential file has invalid format (values hidden)")?;
    if parsed.iter().any(|(k, v)| {
        k.is_empty() || v.is_empty() || v.len() > 8192 || v.chars().any(char::is_control)
    }) {
        bail!("credential file contains an invalid entry (values hidden)");
    }
    Ok(parsed)
}

pub fn lock(data: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("server.lock"))?;
    file.try_lock()
        .context("サーバーが稼働中です。設定・初期化・移行の前に停止してください")?;
    Ok(file)
}

pub fn ensure_certificate(data: &Path, names: &[String]) -> Result<()> {
    let cert = data.join("server.pem");
    let key = data.join("server.key");
    match (cert.exists(), key.exists()) {
        (true, true) => return Ok(()),
        (false, false) => {}
        _ => bail!(
            "証明書と秘密鍵の一方がありません。自動再生成はしません。元の組を復旧してください"
        ),
    }
    let mut all = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    all.extend_from_slice(names);
    all.sort();
    all.dedup();
    let generated = rcgen::generate_simple_self_signed(all)?;
    support::write_private(
        &key,
        generated.signing_key.serialize_pem().as_bytes(),
        false,
    )?;
    if let Err(error) = support::write_private(&cert, generated.cert.pem().as_bytes(), false) {
        let _ = std::fs::remove_file(&key);
        return Err(error);
    }
    Ok(())
}

pub async fn ensure_owner(repo: &dyn Repository, data: &Path) -> Result<()> {
    if repo.devices().await?.is_empty() {
        // Write the recoverable token before registering its hash.
        let path = data.join("owner.token");
        let created = !path.exists();
        let token = if created {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            support::write_private(&path, token.as_bytes(), false)?;
            token
        } else {
            let token = String::from_utf8(support::read_limited(&path, 256)?)?
                .trim()
                .to_owned();
            if !(16..=256).contains(&token.len())
                || !token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                bail!("未完了の所有者トークンを読み込めません。値は変更していません");
            }
            token
        };
        let result = repo
            .register_device(
                inori_domain::Device {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "Owner".into(),
                    audience: inori_domain::Audience::Private,
                    admin: true,
                    revoked: false,
                },
                &inori_server::token_hash(&token),
            )
            .await;
        if result.is_err() && created {
            let _ = std::fs::remove_file(path);
        }
        result?;
    }
    Ok(())
}

pub fn prompt(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    std::io::stdout().flush()?;
    let mut value = String::new();
    if std::io::stdin().read_line(&mut value)? == 0 {
        bail!("セットアップを中断しました");
    }
    let value = value.trim();
    Ok(if value.is_empty() {
        default.into()
    } else {
        value.into()
    })
}
fn yes(label: &str, default: bool) -> Result<bool> {
    loop {
        match prompt(label, if default { "y" } else { "n" })?.as_str() {
            "y" | "Y" | "yes" => return Ok(true),
            "n" | "N" | "no" => return Ok(false),
            _ => println!("y または n を入力してください"),
        }
    }
}

pub struct SetupResult {
    pub data: PathBuf,
    pub start: bool,
    pub autostart: bool,
}

fn default_setup_directory(
    data: PathBuf,
    explicit: bool,
    autostart: bool,
    service_data: PathBuf,
) -> Result<PathBuf> {
    if autostart && !explicit {
        if data != service_data
            && data.join("inori.sqlite").exists()
            && !service_data.join("inori.sqlite").exists()
        {
            bail!(
                "既存の保存先 {} にデータがあります。別の新規DBへ切り替えません。サーバー停止後、--data-dir \"{}\" service install --migrate-data を管理者/rootで実行してください",
                data.display(),
                data.display()
            );
        }
        return Ok(service_data);
    }
    Ok(data)
}

pub async fn run(
    data: PathBuf,
    explicit_data: bool,
    config_path: Option<&Path>,
    listen: Option<SocketAddr>,
    names: Vec<String>,
    mut options: SetupOptions,
) -> Result<SetupResult> {
    let interactive = !options.non_interactive;
    if interactive && !std::io::stdin().is_terminal() {
        bail!("非対話実行には --non-interactive を指定してください");
    }
    if interactive {
        println!("Inori サーバーの初期設定（未指定の外部サービスには送信しません）");
        options.autostart = yes(
            "OS起動時の自動起動を登録しますか（管理者権限が必要）",
            options.autostart,
        )?;
    }
    let default_data = if options.autostart {
        default_setup_directory(data, explicit_data, true, crate::service::data_directory()?)?
    } else {
        data
    };
    let data = if interactive {
        PathBuf::from(prompt("保存先", &default_data.to_string_lossy())?)
    } else {
        default_data
    };
    let data = std::path::absolute(data)?;
    if options.autostart {
        crate::service::require_administrator()?;
    }
    if options.autostart && data != crate::service::data_directory()? {
        bail!("既存データの常駐化には service install --migrate-data を使ってください");
    }
    let saved_config = data.join("server.toml");
    let source = config_path.or_else(|| saved_config.exists().then_some(saved_config.as_path()));
    let mut config = load_config(source)?;
    let mut launch = read_launch(&data)?;
    if let Some(address) = listen {
        launch.listen = address;
    }
    launch.tls_names.extend(names);
    if interactive {
        options.lan = yes(
            "別のPCからLAN/VPNで接続しますか",
            options.lan || !launch.listen.ip().is_loopback(),
        )?;
    }
    if options.lan {
        if listen.is_none() {
            launch.listen = format!("0.0.0.0:{}", launch.listen.port()).parse()?;
        }
        let url = match options.server_url.take() {
            Some(value) => value,
            None if interactive => prompt(
                "接続するサーバーURL（証明書へ登録します）",
                "https://home-server:9473",
            )?,
            None => bail!("LAN設定には --server-url が必要です"),
        };
        launch.server_url = url;
    } else if let Some(url) = options.server_url.take() {
        launch.server_url = url;
    } else if interactive && listen.is_none() {
        launch.listen = format!("127.0.0.1:{}", launch.listen.port()).parse()?;
        launch.server_url = format!("https://localhost:{}", launch.listen.port());
    }
    let url = support::validate_server_url(&launch.server_url, false)?;
    if url.port_or_known_default() != Some(launch.listen.port()) {
        bail!("サーバーURLと待受ポートが一致しません");
    }
    launch.tls_names.push(
        url.host_str()
            .context("host missing")?
            .trim_matches(['[', ']'])
            .to_owned(),
    );
    if interactive && source.is_none() && options.provider.is_none() {
        options.provider = Some(prompt(
            "推論エンジン（mock / ollama / compatible_http）",
            "mock",
        )?);
    }
    let mut credentials = load_credentials(&data.join("server.secrets.toml"))?;
    if let Some(kind) = &options.provider {
        if !matches!(kind.as_str(), "mock" | "ollama" | "compatible_http") {
            bail!("推論エンジンの種類を確認してください");
        }
        if kind != "mock" {
            let endpoint = match options.provider_url {
                Some(value) => value,
                None if interactive => prompt(
                    "エンジンのURL",
                    if kind == "ollama" {
                        "http://127.0.0.1:11434"
                    } else {
                        "https://api.example.com/v1"
                    },
                )?,
                None => bail!("--provider-url が必要です"),
            };
            let model = match options.model {
                Some(value) => value,
                None if interactive => prompt("利用する既存モデル名", "")?,
                None => bail!("--model が必要です"),
            };
            if model.is_empty() {
                bail!("モデル名を指定してください");
            }
            if interactive {
                options.cloud = yes("クラウドへ送信する接続ですか", options.cloud)?;
                options.allow_private = yes(
                    "この接続先へ私的な会話と文脈を送ることを許可しますか",
                    options.allow_private,
                )?;
                options.autonomy_cloud =
                    yes("クラウドでの無人推論を許可しますか", options.autonomy_cloud)?;
                if options.api_key_env.is_none()
                    && yes("APIキーを設定しますか", kind == "compatible_http")?
                {
                    options.api_key_env = Some(prompt("APIキーの環境変数名", "INORI_AI_API_KEY")?);
                }
            }
            if let Some(key_name) = &options.api_key_env {
                let key = match options.api_key_file {
                    Some(path) => Some(
                        String::from_utf8(support::read_limited(&path, 8192)?)?
                            .trim()
                            .to_owned(),
                    ),
                    None if interactive && std::env::var_os(key_name).is_none() => {
                        Some(rpassword::prompt_password("APIキー（表示しません）: ")?)
                    }
                    None => None,
                };
                if let Some(key) = key {
                    if key.is_empty() || key.chars().any(char::is_control) {
                        bail!("APIキーの形式が不正です（値は非表示）");
                    }
                    credentials.insert(key_name.clone(), key);
                }
                if std::env::var_os(key_name).is_none() && !credentials.contains_key(key_name) {
                    bail!(
                        "設定したAPIキーがありません。環境変数または --api-key-file を指定してください"
                    );
                }
            }
            // A setup profile is additive. Existing profiles and the persisted selection survive.
            let mut profile = inori_runtime::ProviderProfile {
                id: "setup-ai".into(),
                kind: kind.clone(),
                base_url: endpoint,
                model,
                api_key_env: options.api_key_env,
                cloud: options.cloud,
                allow_private: options.allow_private,
                ..Default::default()
            };
            // An explicit new setup must not replace a model still selected by an existing DB.
            let mut suffix = 1;
            while let Some(existing) = config.providers.iter().find(|p| p.id == profile.id) {
                if serde_json::to_value(existing)? == serde_json::to_value(&profile)? {
                    break;
                }
                suffix += 1;
                profile.id = format!("setup-ai-{suffix}");
            }
            let new_provider_id = profile.id.clone();
            if !config.providers.iter().any(|p| p.id == profile.id) {
                config.providers.push(profile);
            }
            if !data.join("inori.sqlite").exists() {
                config.character.active_provider = new_provider_id;
            }
            config.autonomy.cloud_enabled = options.autonomy_cloud;
        }
    }
    if interactive && yes("既存の音声エンジンを設定しますか", false)? {
        options.whisper_url = Some(prompt(
            "whisper.cpp URL（空欄は無効）",
            config.voice.whisper_url.as_deref().unwrap_or(""),
        )?);
        options.voicevox_url = Some(prompt(
            "VOICEVOX URL（空欄は無効）",
            config.voice.voicevox_url.as_deref().unwrap_or(""),
        )?);
        options.allow_private_remote_voice = yes(
            "別PCの音声エンジンへ私的な音声・発話文を送ることを許可しますか",
            config.voice.allow_private_remote,
        )?;
        config.voice.allow_private_remote = options.allow_private_remote_voice;
    }
    if let Some(value) = options.whisper_url {
        config.voice.whisper_url = (!value.is_empty()).then_some(value);
    }
    if let Some(value) = options.voicevox_url {
        config.voice.voicevox_url = (!value.is_empty()).then_some(value);
    }
    if options.allow_private_remote_voice {
        config.voice.allow_private_remote = true;
    }
    if config.voice.whisper_url.is_some() || config.voice.voicevox_url.is_some() {
        // Preserve explicit compositions, adding only the requested voice modules and bindings.
        if let Some(composition) = &mut config.composition {
            for (enabled, module, port) in [
                (
                    config.voice.whisper_url.is_some(),
                    "speech.whisper",
                    "recognition",
                ),
                (
                    config.voice.voicevox_url.is_some(),
                    "speech.voicevox",
                    "synthesis",
                ),
            ] {
                if enabled {
                    if !composition.enabled.iter().any(|id| id == module) {
                        composition.enabled.push(module.into());
                    }
                    if composition
                        .enabled
                        .iter()
                        .any(|id| id == "channel.standard")
                        && !composition
                            .bindings
                            .iter()
                            .any(|b| b.module == "channel.standard" && b.port == port)
                    {
                        composition
                            .bindings
                            .push(inori_protocol::CapabilityBinding {
                                module: "channel.standard".into(),
                                port: port.into(),
                                provider: module.into(),
                                major: 1,
                            });
                    }
                    if port == "synthesis"
                        && composition
                            .enabled
                            .iter()
                            .any(|id| id == "dialogue.standard")
                        && !composition
                            .bindings
                            .iter()
                            .any(|b| b.module == "dialogue.standard" && b.port == port)
                    {
                        composition
                            .bindings
                            .push(inori_protocol::CapabilityBinding {
                                module: "dialogue.standard".into(),
                                port: port.into(),
                                provider: module.into(),
                                major: 1,
                            });
                    }
                }
            }
        }
    }
    let graph = resolve_config(&config)?;
    if options.autostart {
        // OS services do not inherit the terminal environment after a reboot.
        for profile in &config.providers {
            if let Some(name) = &profile.api_key_env
                && !credentials.contains_key(name)
                && let Ok(value) = std::env::var(name)
            {
                if value.is_empty() || value.chars().any(char::is_control) {
                    bail!("APIキーの形式が不正です（値は非表示）");
                }
                credentials.insert(name.clone(), value);
            }
        }
    }
    let character = SqliteRepository::peek_character(&data.join("inori.sqlite")).await?;
    validate_character(&config, character.as_ref(), &graph)?;
    println!(
        "保存先: {}\n待受: {}\n接続先: {}",
        data.display(),
        launch.listen,
        launch.server_url
    );
    if interactive && !yes("この内容で保存・初期化しますか", true)? {
        bail!("セットアップを中断しました（設定は保存していません）");
    }
    if !data.exists() {
        support::private_directory(&data)?;
    }
    let _guard = lock(&data)?;
    support::restrict_path(&data, true)?;
    let _listener = std::net::TcpListener::bind(launch.listen)
        .context("待受ポートを利用できません。別のポートを指定してください")?;
    ensure_certificate(&data, &launch.tls_names)?;
    support::verify_certificate_name(
        &support::read_limited(&data.join("server.pem"), 65536)?,
        &launch.server_url,
    )?;
    let repo =
        SqliteRepository::open_locked(&data.join("inori.sqlite"), config.character.clone()).await?;
    ensure_owner(&repo, &data).await?;
    repo.close().await;
    support::write_private(
        &saved_config,
        toml::to_string_pretty(&config)?.as_bytes(),
        true,
    )?;
    support::write_private(
        &data.join("launch.toml"),
        toml::to_string_pretty(&launch)?.as_bytes(),
        true,
    )?;
    if !credentials.is_empty() {
        support::write_private(
            &data.join("server.secrets.toml"),
            toml::to_string(&credentials)?.as_bytes(),
            true,
        )?;
    }
    drop(_listener);
    drop(_guard);
    if !launch.listen.ip().is_loopback() {
        println!(
            "LAN/VPN内からTCP {}への接続を許可してください。ファイアウォール・ルーター設定は変更していません。",
            launch.listen.port()
        );
    }
    if options.check_engines
        || (interactive
            && config.providers.iter().any(|p| p.kind != "mock")
            && yes(
                "設定したエンジンのメタデータ接続を確認しますか（推論・録音はしません）",
                true,
            )?)
    {
        check_engines(&config, &credentials).await?;
    }
    let start = if interactive {
        yes("今すぐ起動して接続を確認しますか", true)?
    } else {
        options.start
    };
    println!(
        "設定を保存しました。診断: inori-server --data-dir \"{}\" doctor",
        data.display()
    );
    Ok(SetupResult {
        data,
        start,
        autostart: options.autostart,
    })
}

/// Metadata endpoints only: no prompts, private context, microphone, synthesis or model pulls.
pub async fn check_engines(
    config: &ServerConfig,
    credentials: &BTreeMap<String, String>,
) -> Result<()> {
    let graph = resolve_config(config)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut checks: Vec<(&str, String, Option<String>)> = Vec::new();
    if graph.enabled("inference.providers") {
        for profile in &config.providers {
            if matches!(profile.kind.as_str(), "mock" | "extension") {
                continue;
            }
            let key = profile
                .api_key_env
                .as_ref()
                .map(|name| {
                    std::env::var(name)
                        .ok()
                        .or_else(|| credentials.get(name).cloned())
                        .context("APIキーが未設定です（値は非表示）")
                })
                .transpose()?;
            checks.push((
                &profile.id,
                format!(
                    "{}{}",
                    profile.base_url.trim_end_matches('/'),
                    if profile.kind == "ollama" {
                        "/api/tags"
                    } else {
                        "/models"
                    }
                ),
                key,
            ));
        }
    }
    if graph.enabled("speech.whisper")
        && let Some(url) = &config.voice.whisper_url
    {
        checks.push((
            "whisper.cpp",
            format!("{}/health", url.trim_end_matches('/')),
            None,
        ));
    }
    if graph.enabled("speech.voicevox")
        && let Some(url) = &config.voice.voicevox_url
    {
        checks.push((
            "VOICEVOX",
            format!("{}/version", url.trim_end_matches('/')),
            None,
        ));
    }
    for (label, url, key) in checks {
        let mut request = client.get(url);
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|_| {
            anyhow::anyhow!(
                "エンジン {label} に接続できません。起動状態と接続設定を確認してください"
            )
        })?;
        if !response.status().is_success() {
            bail!(
                "エンジン {label} の接続確認に失敗しました（HTTP {}）",
                response.status().as_u16()
            );
        }
        println!("エンジン {label}: メタデータ接続は正常（推論・録音・合成は未実施）");
    }
    Ok(())
}

pub fn owner_config(data: &Path, url: &str) -> ClientConfig {
    ClientConfig {
        server_url: url.into(),
        token_file: data.join("owner.token"),
        certificate_file: Some(data.join("server.pem")),
        insecure_loopback: false,
    }
}

pub async fn issue_over_https(
    data: &Path,
    url: &str,
    request: &CreateDeviceRequest,
) -> Result<CreateDeviceResponse> {
    let client =
        support::connection_http(&owner_config(data, url), std::time::Duration::from_secs(5))?;
    let mut response = client
        .post(format!("{}/v1/devices", url.trim_end_matches('/')))
        .json(request)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(support::connection_error(&e)))?;
    if !response.status().is_success() {
        bail!(
            "端末発行に失敗しました（HTTP {}）",
            response.status().as_u16()
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!(support::connection_error(&e)))?
    {
        if body.len() + chunk.len() > 8192 {
            bail!("端末発行応答が大きすぎます");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("端末発行応答が不正です")
}

pub async fn revoke_over_https(data: &Path, url: &str, id: &str) -> Result<()> {
    let id = uuid::Uuid::parse_str(id).context("端末IDの形式が不正です")?;
    let client =
        support::connection_http(&owner_config(data, url), std::time::Duration::from_secs(5))?;
    let response = client
        .delete(format!("{}/v1/devices/{id}", url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(support::connection_error(&e)))?;
    if !response.status().is_success() {
        bail!(
            "端末の失効に失敗しました（HTTP {}）",
            response.status().as_u16()
        );
    }
    Ok(())
}

pub async fn doctor(
    data: &Path,
    config: &ServerConfig,
    launch: &LaunchSettings,
    connect: bool,
) -> Result<()> {
    let graph = resolve_config(config)?;
    validate_character(
        config,
        SqliteRepository::peek_character(&data.join("inori.sqlite"))
            .await?
            .as_ref(),
        &graph,
    )?;
    println!(
        "設定: 正常\n保存先: {}\n接続先: {}",
        data.display(),
        launch.server_url
    );
    if !data.join("inori.sqlite").exists() {
        bail!("未初期化です。setup を実行してください（診断では作成しません）");
    }
    let cert =
        support::read_limited(&data.join("server.pem"), 65536).context("公開証明書がありません")?;
    println!(
        "証明書 SHA256: {}",
        support::certificate_fingerprint(&cert)?
    );
    support::verify_certificate_name(&cert, &launch.server_url)?;
    let credentials = load_credentials(&data.join("server.secrets.toml"))?;
    for profile in &config.providers {
        if let Some(key) = &profile.api_key_env
            && std::env::var_os(key).is_none()
            && !credentials.contains_key(key)
        {
            bail!("推論エンジンのAPIキーが未設定です（値は非表示）");
        }
    }
    if connect {
        let caps = support::probe(&owner_config(data, &launch.server_url)).await?;
        println!(
            "TLS・認証: 正常 / チャット={} 音声認識={} 音声出力={}",
            caps.text_chat, caps.transcription, caps.speech
        );
    }
    println!("推論・音声エンジンへの要求や録音は行っていません。");
    Ok(())
}

pub async fn export_connection(
    data: &Path,
    config: &ServerConfig,
    server_url: &str,
    app_id: &str,
    destination: &Path,
    request: CreateDeviceRequest,
) -> Result<()> {
    support::validate_app_id(app_id)?;
    support::validate_server_url(server_url, false)?;
    if destination.exists() {
        bail!("接続用ファイルは既に存在します。別の保存先を指定してください");
    }
    let certificate_pem =
        String::from_utf8(support::read_limited(&data.join("server.pem"), 65536)?)?;
    support::verify_certificate_name(certificate_pem.as_bytes(), server_url)?;
    let provisional = ConnectionBundle {
        schema_version: 1,
        server_url: server_url.into(),
        certificate_pem: certificate_pem.clone(),
        device_id: "pending".into(),
        app_id: app_id.into(),
        token: "placeholder-token-for-validation".into(),
    };
    support::validate_bundle(&provisional, app_id)?;
    let guard = lock(data);
    let (issued, local_repo): (CreateDeviceResponse, Option<SqliteRepository>) = match guard
        .as_ref()
    {
        Ok(_) => {
            if !data.join("inori.sqlite").exists() {
                bail!("setup を先に実行してください");
            }
            let repo =
                SqliteRepository::open_locked(&data.join("inori.sqlite"), config.character.clone())
                    .await?;
            let issued = issue_device(&repo, request).await?;
            (issued, Some(repo))
        }
        Err(_) => {
            // Only authenticated HTTPS is used while the live server owns the storage lock.
            (issue_over_https(data, server_url, &request).await?, None)
        }
    };
    let bundle = ConnectionBundle {
        device_id: issued.device_id.clone(),
        token: issued.token,
        ..provisional
    };
    let result = support::write_private(destination, &serde_json::to_vec_pretty(&bundle)?, false);
    if result.is_err() {
        if let Some(repo) = &local_repo {
            repo.revoke_device(&issued.device_id).await?;
        } else {
            let client = support::connection_http(
                &owner_config(data, server_url),
                std::time::Duration::from_secs(5),
            )?;
            let _ = client
                .delete(format!(
                    "{}/v1/devices/{}",
                    server_url.trim_end_matches('/'),
                    issued.device_id
                ))
                .send()
                .await;
        }
    }
    if let Some(repo) = local_repo {
        repo.close().await;
    }
    result?;
    println!(
        "接続用ファイル: {}\n認証情報を含みます。対象端末へ安全な経路で渡してください。",
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn synthetic_directory() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }
    #[test]
    fn malformed_credentials_never_echo_the_secret() {
        let work = synthetic_directory();
        let path = work.path().join("synthetic.secrets.toml");
        std::fs::write(&path, "KEY = 'synthetic-secret\ninvalid").unwrap();
        let error = load_credentials(&path).unwrap_err();
        assert!(!format!("{error:?}").contains("synthetic-secret"));
        assert!(error.to_string().contains("hidden"));
    }
    #[test]
    fn opting_into_autostart_does_not_silently_fork_existing_character_storage() {
        let work = synthetic_directory();
        let original = work.path().join("manual");
        let service = work.path().join("service");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(original.join("inori.sqlite"), b"synthetic presence marker").unwrap();
        assert!(default_setup_directory(original.clone(), false, true, service.clone()).is_err());
        assert_eq!(
            default_setup_directory(service.clone(), true, true, service.clone()).unwrap(),
            service
        );
        std::fs::create_dir_all(&service).unwrap();
        std::fs::write(service.join("inori.sqlite"), b"synthetic service marker").unwrap();
        assert_eq!(
            default_setup_directory(original, false, true, service.clone()).unwrap(),
            service
        );
    }
    #[tokio::test]
    async fn interrupted_owner_provisioning_reuses_the_original_token_once() {
        let work = synthetic_directory();
        let repo = SqliteRepository::open(&work.path().join("inori.sqlite"), Default::default())
            .await
            .unwrap();
        let token = "synthetic-interrupted-owner-token";
        support::write_private(&work.path().join("owner.token"), token.as_bytes(), false).unwrap();
        ensure_owner(&repo, work.path()).await.unwrap();
        ensure_owner(&repo, work.path()).await.unwrap();
        assert_eq!(repo.devices().await.unwrap().len(), 1);
        assert!(
            repo.device_by_hash(&inori_server::token_hash(token))
                .await
                .unwrap()
                .unwrap()
                .admin
        );
        assert_eq!(
            std::fs::read_to_string(work.path().join("owner.token")).unwrap(),
            token
        );
        repo.close().await;
    }
}

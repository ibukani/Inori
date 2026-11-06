use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use inori_domain::{Audience, Device};
use inori_runtime::{Repository, SystemClock};
use inori_server::{
    App, ServerConfig, build_service, load_extensions, resolve_config, router, selected_extensions,
    token_hash, validate_character,
};
use inori_storage_sqlite::SqliteRepository;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about = "Inori headless character server")]
struct Args {
    #[arg(long, env = "INORI_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:9473")]
    listen: SocketAddr,
    #[arg(long)]
    insecure_loopback: bool,
    #[arg(long)]
    tls_name: Vec<String>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Serve,
    ValidateConfig,
    Backup {
        destination: PathBuf,
    },
    Restore {
        source: PathBuf,
    },
    CreateDevice {
        #[arg(long)]
        name: String,
        #[arg(long)]
        public: bool,
        #[arg(long)]
        admin: bool,
    },
    RevokeDevice {
        id: String,
    },
}
fn data_directory() -> Result<PathBuf> {
    Ok(directories::ProjectDirs::from("dev", "Inori", "Inori")
        .context("cannot determine application data directory")?
        .data_local_dir()
        .to_path_buf())
}
fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
async fn new_device(
    repo: &dyn Repository,
    data: &Path,
    name: String,
    public: bool,
    admin: bool,
) -> Result<PathBuf> {
    if public && admin {
        bail!("public devices cannot administer private state");
    }
    let id = Uuid::new_v4().to_string();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let directory = data.join("devices");
    tokio::fs::create_dir_all(&directory).await?;
    let path = directory.join(format!("{id}.token"));
    private_file(&path, token.as_bytes())?;
    repo.register_device(
        Device {
            id,
            name,
            audience: if public {
                Audience::Public
            } else {
                Audience::Private
            },
            admin,
            revoked: false,
        },
        &token_hash(&token),
    )
    .await?;
    Ok(path)
}

async fn serve_until_shutdown<F>(future: F, shutdown: CancellationToken) -> Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::pin!(future);
    tokio::select! {
        result=&mut future=>result?,
        _=shutdown.cancelled()=>{let _=tokio::time::timeout(std::time::Duration::from_secs(5),&mut future).await;}
    }
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "inori_server=info,inori_runtime=info".into()),
        )
        .init();
    let args = Args::parse();
    let data = args
        .data_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(data_directory)?;
    let database = data.join("inori.sqlite");
    if let Some(Command::Backup { destination }) = &args.command {
        SqliteRepository::backup_file(&database, destination).await?;
        println!("Backup written: {}", destination.display());
        return Ok(());
    }
    let mut config = match &args.config {
        Some(path) => toml::from_str::<ServerConfig>(&tokio::fs::read_to_string(path).await?)?,
        None => ServerConfig::default(),
    };
    if let Some(path) = &args.config {
        let parent = path.parent().unwrap_or(Path::new("."));
        for extension in &mut config.extensions {
            if extension.is_relative() {
                *extension = parent.join(&*extension);
            }
        }
    }
    let serving = args.command.is_none() || matches!(args.command, Some(Command::Serve));
    if serving || matches!(args.command, Some(Command::ValidateConfig)) {
        let graph = resolve_config(&config)?;
        let character = SqliteRepository::peek_character(&database).await?;
        validate_character(&config, character.as_ref(), &graph)?;
        if matches!(args.command, Some(Command::ValidateConfig)) {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"valid":true,"fingerprint":graph.fingerprint,"order":graph.order,"bindings":graph.config.bindings})
                )?
            );
            return Ok(());
        }
    }
    tokio::fs::create_dir_all(&data).await?;
    if args.insecure_loopback && !args.listen.ip().is_loopback() {
        bail!("unencrypted transport is restricted to loopback");
    }
    if let Some(Command::Restore { source }) = &args.command {
        if database.exists() {
            bail!("restore requires a fresh data directory; existing data is never overwritten");
        }
        tokio::fs::copy(source, &database).await?;
    }
    let _lock = if serving {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(data.join("server.lock"))?;
        file.try_lock()
            .context("another server is using this data directory")?;
        Some(file)
    } else {
        None
    };
    tracing::info!("opening character storage");
    let repo = Arc::new(if serving {
        SqliteRepository::open_locked(&database, config.character.clone()).await?
    } else {
        SqliteRepository::open(&database, config.character.clone()).await?
    });
    tracing::info!("character storage ready");
    match args.command {
        Some(Command::Backup { destination }) => {
            repo.backup(&destination).await?;
            println!("Backup written: {}", destination.display());
            return Ok(());
        }
        Some(Command::Restore { .. }) => {
            println!("Restored into fresh directory: {}", data.display());
            return Ok(());
        }
        Some(Command::CreateDevice {
            name,
            public,
            admin,
        }) => {
            let path = new_device(repo.as_ref(), &data, name, public, admin).await?;
            println!("Device token file: {}", path.display());
            return Ok(());
        }
        Some(Command::RevokeDevice { id }) => {
            repo.revoke_device(&id).await?;
            println!("Device revoked");
            return Ok(());
        }
        _ => {}
    }
    if repo.devices().await?.is_empty() {
        let path = new_device(repo.as_ref(), &data, "Owner".into(), false, true).await?;
        private_file(&data.join("owner.token"), &tokio::fs::read(path).await?)?;
    }
    let graph = resolve_config(&config)?;
    let extensions = load_extensions(&selected_extensions(&config, &graph)?).await?;
    let service = build_service(&config, repo, Arc::new(SystemClock), &extensions).await?;
    let shutdown = CancellationToken::new();
    let app = Arc::new(App {
        service: service.clone(),
        extensions,
        data_dir: data.clone(),
        shutdown: shutdown.clone(),
    });
    let routes = router(app.clone());
    let signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    let cleanup_app = app.clone();
    let cleanup = tokio::spawn(async move {
        cleanup_app.shutdown.cancelled().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            cleanup_app.service.stop().await;
            for extension in cleanup_app.extensions.values() {
                extension.process.lock().await.stop().await;
            }
        })
        .await;
    });
    let result = if args.insecure_loopback {
        tracing::info!(listen=%args.listen,data=%data.display(),"development HTTP server (loopback only)");
        let listener = tokio::net::TcpListener::bind(args.listen).await?;
        let signal = shutdown.clone();
        service.start_workers();
        serve_until_shutdown(
            axum::serve(listener, routes)
                .with_graceful_shutdown(async move {
                    signal.cancelled().await;
                })
                .into_future(),
            shutdown.clone(),
        )
        .await
    } else {
        let certificate = data.join("server.pem");
        let key = data.join("server.key");
        if !certificate.exists() && !key.exists() {
            let mut names = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
            names.extend(args.tls_name);
            let generated = rcgen::generate_simple_self_signed(names)?;
            private_file(&certificate, generated.cert.pem().as_bytes())?;
            private_file(&key, generated.signing_key.serialize_pem().as_bytes())?;
        }
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&certificate, &key).await?;
        let handle = axum_server::Handle::new();
        let signal = shutdown.clone();
        let stop = handle.clone();
        tokio::spawn(async move {
            signal.cancelled().await;
            stop.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        tracing::info!(listen=%args.listen,certificate=%certificate.display(),data=%data.display(),"HTTPS server; pair with this certificate and a device token");
        let listener = std::net::TcpListener::bind(args.listen)?;
        listener.set_nonblocking(true)?;
        service.start_workers();
        serve_until_shutdown(
            axum_server::from_tcp_rustls(listener, tls)
                .handle(handle)
                .serve(routes.into_make_service()),
            shutdown.clone(),
        )
        .await
    };
    shutdown.cancel();
    let _ = cleanup.await;
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn a_listener_that_ignores_shutdown_cannot_extend_the_five_second_deadline() {
        let stop = CancellationToken::new();
        stop.cancel();
        let started = tokio::time::Instant::now();
        serve_until_shutdown(std::future::pending(), stop)
            .await
            .unwrap();
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(5));
    }
}

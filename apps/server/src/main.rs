use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use inori_domain::{Audience, Device};
use inori_runtime::{Repository, SystemClock};
use inori_server::{
    App, load_extensions, resolve_config, router, selected_extensions, token_hash,
    validate_character,
};
use inori_storage_sqlite::SqliteRepository;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
mod service;
mod setup;

#[derive(Clone, Parser)]
#[command(version, about = "Inori headless character server")]
struct Args {
    #[arg(long, env = "INORI_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    listen: Option<SocketAddr>,
    #[arg(long)]
    insecure_loopback: bool,
    #[arg(long)]
    tls_name: Vec<String>,
    #[arg(long, global = true)]
    secrets_file: Option<PathBuf>,
    #[cfg(windows)]
    #[arg(long, hide = true)]
    service_run: bool,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Clone, Subcommand)]
enum Command {
    Serve,
    ValidateConfig,
    Setup(setup::SetupOptions),
    Doctor {
        #[arg(long)]
        offline: bool,
        /// Check only explicitly configured engine metadata, without inference or recording.
        #[arg(long, conflicts_with = "offline")]
        engines: bool,
    },
    ExportConnection {
        destination: PathBuf,
        #[arg(long, default_value = "desktop")]
        app: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        admin: bool,
        #[arg(long)]
        public: bool,
    },
    Service {
        #[command(subcommand)]
        action: service::ServiceCommand,
    },
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
    inori_app_support::server_data_directory()
}
fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    inori_app_support::write_private(path, bytes, false)
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

fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "inori_server=info,inori_runtime=info".into()),
        )
        .init();
    #[cfg(windows)]
    let args = Args::parse();
    #[cfg(windows)]
    if args.service_run {
        return service::dispatch();
    }
    run_runtime(CancellationToken::new(), service::ReadyNotifier::default())
}
fn run_runtime(shutdown: CancellationToken, ready: service::ReadyNotifier) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(run(Args::parse(), shutdown, ready))
}
async fn run(
    mut args: Args,
    shutdown: CancellationToken,
    ready: service::ReadyNotifier,
) -> Result<()> {
    let mut data = args
        .data_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(data_directory)?;
    if let Some(Command::Setup(options)) = args.command.clone() {
        let result = setup::run(
            data,
            args.data_dir.is_some(),
            args.config.as_deref(),
            args.listen,
            args.tls_name.clone(),
            options,
        )
        .await?;
        data = result.data;
        if result.autostart {
            service::install(&data, false).await?;
            if result.start {
                service::manage(service::ServiceCommand::Start, &data).await?;
            }
            return Ok(());
        }
        if !result.start {
            return Ok(());
        }
        args.command = Some(Command::Serve);
        args.config = Some(data.join("server.toml"));
    }
    if let Some(Command::Service { action }) = args.command.clone() {
        return service::manage(action, &data).await;
    }
    let mut launch = setup::read_launch(&data)?;
    if let Some(listen) = args.listen {
        launch.listen = listen;
    }
    let listen = launch.listen;
    args.tls_name.extend(launch.tls_names.clone());
    let database = data.join("inori.sqlite");
    if let Some(Command::Backup { destination }) = &args.command {
        SqliteRepository::backup_file(&database, destination).await?;
        println!("Backup written: {}", destination.display());
        return Ok(());
    }
    let saved_config = data.join("server.toml");
    let config = setup::load_config(
        args.config
            .as_deref()
            .or_else(|| saved_config.exists().then_some(saved_config.as_path())),
    )?;
    // Keep the original live create-device/revoke-device CLI usable without bypassing the lock.
    if database.exists()
        && matches!(
            args.command,
            Some(Command::CreateDevice { .. } | Command::RevokeDevice { .. })
        )
        && setup::lock(&data).is_err()
    {
        match &args.command {
            Some(Command::CreateDevice {
                name,
                public,
                admin,
            }) => {
                let issued = setup::issue_over_https(
                    &data,
                    &launch.server_url,
                    &inori_protocol::CreateDeviceRequest {
                        name: name.clone(),
                        audience: if *public {
                            inori_protocol::Audience::Public
                        } else {
                            inori_protocol::Audience::Private
                        },
                        admin: *admin,
                    },
                )
                .await?;
                let directory = data.join("devices");
                inori_app_support::private_directory(&directory)?;
                let path = directory.join(format!("{}.token", issued.device_id));
                if let Err(error) = private_file(&path, issued.token.as_bytes()) {
                    let _ = setup::revoke_over_https(&data, &launch.server_url, &issued.device_id)
                        .await;
                    return Err(error);
                }
                println!("Device token file: {}", path.display());
                return Ok(());
            }
            Some(Command::RevokeDevice { id }) => {
                setup::revoke_over_https(&data, &launch.server_url, id).await?;
                println!("Device revoked");
                return Ok(());
            }
            _ => unreachable!(),
        }
    }
    if let Some(Command::Doctor { offline, engines }) = args.command {
        setup::doctor(&data, &config, &launch, !offline).await?;
        if engines {
            setup::check_engines(
                &config,
                &setup::load_credentials(&data.join("server.secrets.toml"))?,
            )
            .await?;
        }
        return Ok(());
    }
    if let Some(Command::ExportConnection {
        destination,
        app,
        name,
        server,
        admin,
        public,
    }) = &args.command
    {
        return setup::export_connection(
            &data,
            &config,
            server.as_deref().unwrap_or(&launch.server_url),
            app,
            destination,
            inori_protocol::CreateDeviceRequest {
                name: name.clone(),
                audience: if *public {
                    inori_protocol::Audience::Public
                } else {
                    inori_protocol::Audience::Private
                },
                admin: *admin,
            },
        )
        .await;
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
    inori_app_support::private_directory(&data)?;
    if args.insecure_loopback && !listen.ip().is_loopback() {
        bail!("unencrypted transport is restricted to loopback");
    }
    if let Some(Command::Restore { source }) = &args.command {
        if database.exists() {
            bail!("restore requires a fresh data directory; existing data is never overwritten");
        }
        tokio::fs::copy(source, &database).await?;
    }
    let _lock = Some(setup::lock(&data)?);
    tracing::info!("opening character storage");
    let repo = Arc::new(SqliteRepository::open_locked(&database, config.character.clone()).await?);
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
    setup::ensure_owner(repo.as_ref(), &data).await?;
    let graph = resolve_config(&config)?;
    let extensions = load_extensions(&selected_extensions(&config, &graph)?).await?;
    let credentials = setup::load_credentials(
        &args
            .secrets_file
            .unwrap_or_else(|| data.join("server.secrets.toml")),
    )?;
    let service = inori_server::build_service_with_credentials(
        &config,
        repo,
        Arc::new(SystemClock),
        &extensions,
        &credentials,
    )
    .await?;
    let app = Arc::new(App {
        service: service.clone(),
        extensions,
        data_dir: data.clone(),
        shutdown: shutdown.clone(),
    });
    let routes = router(app.clone());
    let signal = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
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
        tracing::info!(listen=%listen,data=%data.display(),"development HTTP server (loopback only)");
        let listener = tokio::net::TcpListener::bind(listen).await?;
        let signal = shutdown.clone();
        service.start_workers();
        ready.notify()?;
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
        setup::ensure_certificate(&data, &args.tls_name)?;
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&certificate, &key).await?;
        let handle = axum_server::Handle::new();
        let signal = shutdown.clone();
        let stop = handle.clone();
        tokio::spawn(async move {
            signal.cancelled().await;
            stop.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        tracing::info!(listen=%listen,certificate=%certificate.display(),data=%data.display(),"HTTPS server; pair with this certificate and a device token");
        let listener = std::net::TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        service.start_workers();
        ready.notify()?;
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

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Context, Result};
use clap::Parser;
use inori_app_support as support;
use inori_client_core::ClientConfig;
use std::path::PathBuf;
#[cfg(any(windows, target_os = "macos"))]
mod ui;

#[derive(Parser)]
#[command(version, about = "Inori native desktop client")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[arg(long)]
    certificate_file: Option<PathBuf>,
    #[arg(long)]
    insecure_loopback: bool,
    #[arg(long)]
    audio_devices: bool,
    #[arg(long)]
    probe_seconds: Option<u64>,
    #[arg(long)]
    import_connection: Option<PathBuf>,
    /// Diagnose TLS/authentication without starting the GUI or audio devices.
    #[arg(long)]
    doctor: bool,
}
fn main() -> Result<()> {
    let args = Args::parse();
    if args.audio_devices {
        println!("{}", inori_client_core::audio::devices()?);
        return Ok(());
    }
    let portable = std::env::current_exe()?.with_file_name("inori-client.toml");
    let saved = support::client_config_path(support::DESKTOP_APP)?;
    let destination = args
        .config
        .as_ref()
        .map(std::path::absolute)
        .transpose()?
        .unwrap_or(saved.clone());
    let (mut config, startup_error) =
        match support::load_client_config(args.config.as_deref(), &saved, &portable) {
            Ok((config, _)) => (config, None),
            Err(error) if !args.doctor => (ClientConfig::local()?, Some(error.to_string())),
            Err(error) => return Err(error),
        };
    if let Some(url) = args.server {
        config.server_url = url;
    }
    if let Some(path) = args.token_file {
        config.token_file = path;
    }
    if let Some(path) = args.certificate_file {
        config.certificate_file = Some(path);
    }
    if args.insecure_loopback {
        config.insecure_loopback = true;
        if config.server_url.starts_with("http:") {
            config.certificate_file = None;
        }
    }
    if args.doctor {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            if let Some(path) = &args.import_connection {
                let bundle = support::read_bundle(path, support::DESKTOP_APP)?;
                let work = tempfile::tempdir()?;
                let temporary =
                    support::prepare_bundle(&bundle, support::DESKTOP_APP, work.path())?;
                support::probe(&temporary.config).await?;
            } else {
                support::probe(&config).await?;
            }
            anyhow::Ok(())
        })?;
        println!("TLS・端末認証・機能確認: 正常（設定は変更していません）");
        return Ok(());
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        ui::run(
            config,
            destination,
            args.import_connection,
            startup_error,
            args.probe_seconds,
        )
        .context("desktop failed")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = config;
        anyhow::bail!("desktop is initially supported on Windows and macOS builds only")
    }
}

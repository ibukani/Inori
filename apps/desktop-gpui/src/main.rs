#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Context, Result};
use clap::Parser;
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
    model_pack: Option<PathBuf>,
    #[arg(long)]
    audio_devices: bool,
    #[arg(long)]
    probe_seconds: Option<u64>,
}
fn main() -> Result<()> {
    let args = Args::parse();
    if args.audio_devices {
        println!("{}", inori_client_core::audio::devices()?);
        return Ok(());
    }
    let portable = std::env::current_exe()?.with_file_name("inori-client.toml");
    let selected = args
        .config
        .as_ref()
        .or_else(|| portable.exists().then_some(&portable));
    let mut config = match selected {
        Some(path) => toml::from_str::<ClientConfig>(&std::fs::read_to_string(path)?)?,
        None => ClientConfig::local()?,
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
    if args.model_pack.is_some() {
        config.model_pack = args.model_pack;
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        ui::run(config, args.probe_seconds).context("desktop failed")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = config;
        anyhow::bail!("desktop is initially supported on Windows and macOS builds only")
    }
}

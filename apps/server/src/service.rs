//! Opt-in OS service installation. Runtime always runs as a non-administrator identity.
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy, Subcommand)]
pub enum ServiceCommand {
    Install {
        /// Explicitly copy a stopped existing server into the service data directory.
        #[arg(long)]
        migrate_data: bool,
    },
    Start,
    Stop,
    Status,
    Uninstall,
}

#[derive(Default)]
pub struct ReadyNotifier {
    #[cfg(windows)]
    handle: Option<windows_service::service_control_handler::ServiceStatusHandle>,
}
impl ReadyNotifier {
    pub fn notify(&self) -> Result<()> {
        #[cfg(windows)]
        if let Some(handle) = &self.handle {
            use windows_service::service::*;
            handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: std::time::Duration::ZERO,
                process_id: None,
            })?;
        }
        Ok(())
    }
}

pub fn data_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(
            PathBuf::from(std::env::var_os("ProgramData").context("ProgramData unavailable")?)
                .join("Inori")
                .join("server"),
        )
    }
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from("/Library/Application Support/Inori/server"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(PathBuf::from("/var/lib/inori"))
    }
}
fn bin_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(
            PathBuf::from(std::env::var_os("ProgramFiles").context("ProgramFiles unavailable")?)
                .join("Inori")
                .join("server"),
        )
    }
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from("/usr/local/lib/inori"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(PathBuf::from("/opt/inori"))
    }
}

fn output(program: &str, args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .output()
        .with_context(|| format!("cannot run {program}"))
}
fn checked(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let result = output(program, args)?;
    if !result.status.success() {
        bail!(
            "{program} failed ({}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

pub fn require_administrator() -> Result<()> {
    #[cfg(windows)]
    {
        let groups = output(
            "whoami.exe",
            &[
                "/groups".as_ref(),
                "/fo".as_ref(),
                "csv".as_ref(),
                "/nh".as_ref(),
            ],
        )?;
        if !groups.status.success()
            || !String::from_utf8_lossy(&groups.stdout).contains("S-1-16-12288")
        {
            bail!(
                "常駐登録には管理者権限が必要です。管理者としてターミナルを開き、同じ service コマンドを実行してください"
            );
        }
    }
    #[cfg(unix)]
    {
        if output("id", &["-u".as_ref()])?.stdout != b"0\n" {
            bail!(
                "常駐登録にはroot権限が必要です。同じ service コマンドを sudo で実行してください"
            );
        }
    }
    Ok(())
}

fn safe_install_directory(path: &Path) -> Result<()> {
    let mut ancestor = Some(path);
    while let Some(check) = ancestor {
        if let Ok(meta) = std::fs::symlink_metadata(check)
            && meta.file_type().is_symlink()
        {
            bail!(
                "service paths must not contain symbolic links: {}",
                check.display()
            );
        }
        ancestor = check.parent();
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if std::fs::metadata(path)?.uid() != 0 {
            bail!("service installation directory must be owned by root");
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        checked(
            "icacls.exe",
            &[
                path.as_os_str(),
                "/inheritance:r".as_ref(),
                "/grant:r".as_ref(),
                "*S-1-5-32-544:(OI)(CI)F".as_ref(),
                "*S-1-5-18:(OI)(CI)F".as_ref(),
                "*S-1-5-19:(OI)(CI)RX".as_ref(),
            ],
        )?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_account() -> Result<()> {
    let existing = output("id", &["-u".as_ref(), "_inori".as_ref()])?;
    if existing.status.success() {
        let uid = String::from_utf8_lossy(&existing.stdout)
            .trim()
            .parse::<u32>()?;
        let shell = output(
            "dscl",
            &[
                ".".as_ref(),
                "-read".as_ref(),
                "/Users/_inori".as_ref(),
                "UserShell".as_ref(),
            ],
        )?;
        if uid == 0
            || !shell.status.success()
            || !String::from_utf8_lossy(&shell.stdout)
                .trim()
                .ends_with("/usr/bin/false")
        {
            bail!("既存の_inoriは専用の非ログインアカウントではありません");
        }
        return Ok(());
    }
    // Select an unused system UID instead of assuming a free fixed UID.
    let users = output(
        "dscl",
        &[
            ".".as_ref(),
            "-list".as_ref(),
            "/Users".as_ref(),
            "UniqueID".as_ref(),
        ],
    )?;
    let groups = output(
        "dscl",
        &[
            ".".as_ref(),
            "-list".as_ref(),
            "/Groups".as_ref(),
            "PrimaryGroupID".as_ref(),
        ],
    )?;
    if !users.status.success() || !groups.status.success() {
        bail!("cannot enumerate service account IDs");
    }
    let used: std::collections::BTreeSet<u32> = String::from_utf8_lossy(&users.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&groups.stdout).lines())
        .filter_map(|line| line.split_whitespace().last()?.parse().ok())
        .collect();
    let uid = (400..500)
        .find(|id| !used.contains(id))
        .context("no free system account ID")?
        .to_string();
    for args in [
        vec![".", "-create", "/Groups/_inori"],
        vec![".", "-create", "/Groups/_inori", "PrimaryGroupID", &uid],
        vec![".", "-create", "/Users/_inori"],
        vec![".", "-create", "/Users/_inori", "UniqueID", &uid],
        vec![".", "-create", "/Users/_inori", "PrimaryGroupID", &uid],
        vec![
            ".",
            "-create",
            "/Users/_inori",
            "UserShell",
            "/usr/bin/false",
        ],
        vec![
            ".",
            "-create",
            "/Users/_inori",
            "NFSHomeDirectory",
            "/var/empty",
        ],
        vec![".", "-create", "/Users/_inori", "IsHidden", "1"],
    ] {
        checked(
            "dscl",
            &args.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
        )?;
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn ensure_account() -> Result<()> {
    let existing = output("getent", &["passwd".as_ref(), "inori".as_ref()])?;
    if existing.status.success() {
        let text = String::from_utf8(existing.stdout)?;
        let fields: Vec<_> = text.trim().split(':').collect();
        if fields.len() != 7
            || fields[2] == "0"
            || !matches!(
                fields[6],
                "/usr/sbin/nologin" | "/sbin/nologin" | "/bin/false" | "/usr/bin/false"
            )
        {
            bail!("既存のinoriは専用の非ログインアカウントではありません");
        }
    } else {
        checked(
            "useradd",
            &[
                "--system".as_ref(),
                "--user-group".as_ref(),
                "--home-dir".as_ref(),
                "/var/lib/inori".as_ref(),
                "--no-create-home".as_ref(),
                "--shell".as_ref(),
                "/usr/sbin/nologin".as_ref(),
                "inori".as_ref(),
            ],
        )?;
    }
    Ok(())
}

fn grant_runtime_data(data: &Path) -> Result<()> {
    reject_links_in_tree(data)?;
    #[cfg(windows)]
    {
        checked(
            "icacls.exe",
            &[
                data.as_os_str(),
                "/grant:r".as_ref(),
                "*S-1-5-19:(OI)(CI)M".as_ref(),
                "/T".as_ref(),
                "/Q".as_ref(),
            ],
        )?;
    }
    #[cfg(unix)]
    {
        ensure_account()?;
        #[cfg(target_os = "macos")]
        let owner = "_inori:_inori";
        #[cfg(not(target_os = "macos"))]
        let owner = "inori:inori";
        checked("chown", &["-R".as_ref(), owner.as_ref(), data.as_os_str()])?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn reject_links_in_tree(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!("常駐用保存先にシンボリックリンクを含められません");
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            reject_links_in_tree(&entry?.path())?;
        }
    }
    Ok(())
}

pub async fn install(source: &Path, migrate: bool) -> Result<()> {
    require_administrator()?;
    let target = data_directory()?;
    safe_install_directory(&bin_directory()?)?;
    if std::path::absolute(source)? != target {
        if !migrate {
            bail!(
                "常駐用の保存先は {} です。既存データのコピーには --migrate-data を指定してください",
                target.display()
            );
        }
        if target.exists() && std::fs::read_dir(&target)?.next().is_some() {
            bail!("常駐用保存先は空である必要があります。既存データは上書きしません");
        }
        let _lock = crate::setup::lock(source)?;
        let config = crate::setup::load_config(Some(&source.join("server.toml")))?;
        if !config.extensions.is_empty() {
            bail!("拡張を使う環境は、専用アカウントでアクセス可能な配置を先に準備してください");
        }
        let launch = crate::setup::read_launch(source)?;
        let source_backup = source
            .join("backups")
            .join(format!("pre-service-{}.sqlite", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(source_backup.parent().unwrap())?;
        inori_storage_sqlite::SqliteRepository::backup_file(
            &source.join("inori.sqlite"),
            &source_backup,
        )
        .await?;
        inori_app_support::private_directory(&target)?;
        // Restore into the fresh target; only known credential/config files accompany the DB.
        std::fs::copy(&source_backup, target.join("inori.sqlite"))?;
        for name in [
            "server.pem",
            "server.key",
            "owner.token",
            "server.secrets.toml",
        ] {
            let path = source.join(name);
            if path.exists() {
                inori_app_support::write_private(
                    &target.join(name),
                    &inori_app_support::read_limited(&path, 65536)?,
                    false,
                )?;
            }
        }
        inori_app_support::write_private(
            &target.join("server.toml"),
            toml::to_string_pretty(&config)?.as_bytes(),
            false,
        )?;
        inori_app_support::write_private(
            &target.join("launch.toml"),
            toml::to_string_pretty(&launch)?.as_bytes(),
            false,
        )?;
    }
    if !target.join("server.toml").is_file() || !target.join("inori.sqlite").is_file() {
        bail!("常駐用保存先で setup を先に実行してください");
    }
    let _lock = crate::setup::lock(&target)?;
    let executable = bin_directory()?.join(if cfg!(windows) {
        "inori-server.exe"
    } else {
        "inori-server"
    });
    let current = std::env::current_exe()?.canonicalize()?;
    if !executable.exists() || executable.canonicalize()? != current {
        if executable.exists() && std::fs::read(&executable)? != std::fs::read(&current)? {
            bail!(
                "常駐バイナリが異なります。停止・バックアップ後に固定場所のバイナリを更新してください"
            );
        }
        if !executable.exists() {
            std::fs::copy(&current, &executable)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
    }
    grant_runtime_data(&target)?;
    #[cfg(windows)]
    windows::install(&executable, &target)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let unit = linux_unit(&executable, &target)?;
        std::fs::write("/etc/systemd/system/inori.service", unit)?;
        checked("systemctl", &["daemon-reload".as_ref()])?;
        checked("systemctl", &["enable".as_ref(), "inori.service".as_ref()])?;
    }
    #[cfg(target_os = "macos")]
    {
        std::fs::write(
            "/Library/LaunchDaemons/dev.inori.server.plist",
            mac_plist(&executable, &target)?,
        )?;
        checked(
            "chmod",
            &[
                "644".as_ref(),
                "/Library/LaunchDaemons/dev.inori.server.plist".as_ref(),
            ],
        )?;
    }
    println!("OS起動時の常駐を登録しました。保存先: {}", target.display());
    Ok(())
}

pub async fn manage(command: ServiceCommand, data: &Path) -> Result<()> {
    if let ServiceCommand::Install { migrate_data } = command {
        return install(data, migrate_data).await;
    }
    if !matches!(command, ServiceCommand::Status) {
        require_administrator()?;
    }
    #[cfg(windows)]
    windows::manage(command)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let action = match command {
            ServiceCommand::Start => "start",
            ServiceCommand::Stop => "stop",
            ServiceCommand::Status => "status",
            ServiceCommand::Uninstall => "disable",
            _ => unreachable!(),
        };
        if matches!(command, ServiceCommand::Uninstall) {
            checked(
                "systemctl",
                &[
                    "disable".as_ref(),
                    "--now".as_ref(),
                    "inori.service".as_ref(),
                ],
            )?;
            let path = Path::new("/etc/systemd/system/inori.service");
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            checked("systemctl", &["daemon-reload".as_ref()])?;
        } else {
            let result = output("systemctl", &[action.as_ref(), "inori.service".as_ref()])?;
            print!("{}", String::from_utf8_lossy(&result.stdout));
            if !result.status.success() {
                bail!("service {action}: {}", result.status);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let plist = "/Library/LaunchDaemons/dev.inori.server.plist";
        match command {
            ServiceCommand::Start => {
                if !output(
                    "launchctl",
                    &["print".as_ref(), "system/dev.inori.server".as_ref()],
                )?
                .status
                .success()
                {
                    checked(
                        "launchctl",
                        &["bootstrap".as_ref(), "system".as_ref(), plist.as_ref()],
                    )?;
                }
            }
            ServiceCommand::Stop => checked(
                "launchctl",
                &["bootout".as_ref(), "system/dev.inori.server".as_ref()],
            )?,
            ServiceCommand::Status => {
                let result = output(
                    "launchctl",
                    &["print".as_ref(), "system/dev.inori.server".as_ref()],
                )?;
                print!("{}", String::from_utf8_lossy(&result.stdout));
                if !result.status.success() {
                    bail!("service is not loaded");
                }
            }
            ServiceCommand::Uninstall => {
                let _ = output(
                    "launchctl",
                    &["bootout".as_ref(), "system/dev.inori.server".as_ref()],
                );
                if Path::new(plist).exists() {
                    std::fs::remove_file(plist)?;
                }
            }
            _ => unreachable!(),
        }
    }
    if matches!(command, ServiceCommand::Uninstall) {
        println!("常駐登録を解除しました。データ・証明書・キーは保持しています。");
    }
    if matches!(command, ServiceCommand::Start) {
        let data = data_directory()?;
        let launch = crate::setup::read_launch(&data)?;
        let config = crate::setup::owner_config(&data, &launch.server_url);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if inori_app_support::probe(&config).await.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        })
        .await
        .context(
            "サービスのTLS・認証を確認できません。常駐用保存先を指定して doctor を実行してください",
        )?;
        println!("サービス起動とTLS・端末認証を確認しました。");
    }
    Ok(())
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn linux_unit(executable: &Path, data: &Path) -> Result<String> {
    fn quoted(path: &Path) -> Result<String> {
        let value = path.to_str().context("service path must be UTF-8")?;
        if value.chars().any(char::is_control) {
            bail!("invalid service path");
        }
        Ok(format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$")
        ))
    }
    Ok(format!(
        "[Unit]\nDescription=Inori private character server\nAfter=network.target\nStartLimitIntervalSec=300\nStartLimitBurst=3\n\n[Service]\nType=simple\nUser=inori\nGroup=inori\nExecStart={} --data-dir {} serve\nRestart=on-failure\nRestartSec=60\nTimeoutStopSec=10\nUMask=0077\nNoNewPrivileges=true\n\n[Install]\nWantedBy=multi-user.target\n",
        quoted(executable)?,
        quoted(data)?
    ))
}

#[cfg(any(test, target_os = "macos"))]
fn mac_plist(executable: &Path, data: &Path) -> Result<String> {
    fn xml(path: &Path) -> Result<String> {
        let value = path.to_str().context("service path must be UTF-8")?;
        if value.chars().any(char::is_control) {
            bail!("invalid service path");
        }
        Ok(value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;"))
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>dev.inori.server</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--data-dir</string><string>{}</string><string>serve</string></array>\n<key>UserName</key><string>_inori</string><key>GroupName</key><string>_inori</string>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>60</integer><key>ExitTimeOut</key><integer>10</integer><key>Umask</key><integer>63</integer>\n</dict></plist>\n",
        xml(executable)?,
        xml(data)?
    ))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_service::{
        service::*,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    const NAME: &str = "InoriServer";
    pub fn install(executable: &Path, data: &Path) -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        match manager.open_service(NAME, ServiceAccess::QUERY_CONFIG) {
            Ok(service) => {
                let config = service.query_config()?;
                if !config
                    .executable_path
                    .to_string_lossy()
                    .contains(&executable.to_string_lossy().to_string())
                    || config.account_name.as_deref()
                        != Some(std::ffi::OsStr::new("NT AUTHORITY\\LocalService"))
                {
                    bail!("既存のInoriServer登録が異なります。既存登録を確認してください");
                }
                return Ok(());
            }
            Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {}
            Err(error) => return Err(error.into()),
        }
        let service = manager.create_service(
            &ServiceInfo {
                name: NAME.into(),
                display_name: "Inori Server".into(),
                service_type: ServiceType::OWN_PROCESS,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path: executable.into(),
                launch_arguments: vec![
                    "--service-run".into(),
                    "--data-dir".into(),
                    data.as_os_str().into(),
                ],
                dependencies: vec![],
                account_name: Some("NT AUTHORITY\\LocalService".into()),
                account_password: None,
            },
            ServiceAccess::CHANGE_CONFIG,
        )?;
        service.set_description("Private Inori server. Data is retained when uninstalled.")?;
        service.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(std::time::Duration::from_secs(86400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: std::time::Duration::from_secs(60)
                };
                3
            ]),
        })?;
        Ok(())
    }
    pub fn manage(command: ServiceCommand) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            NAME,
            ServiceAccess::QUERY_STATUS
                | match command {
                    ServiceCommand::Start => ServiceAccess::START,
                    ServiceCommand::Stop => ServiceAccess::STOP,
                    ServiceCommand::Uninstall => ServiceAccess::STOP | ServiceAccess::DELETE,
                    _ => ServiceAccess::empty(),
                },
        )?;
        match command {
            ServiceCommand::Start => {
                if service.query_status()?.current_state != ServiceState::Running {
                    service.start::<&str>(&[])?;
                }
            }
            ServiceCommand::Stop | ServiceCommand::Uninstall => {
                if service.query_status()?.current_state != ServiceState::Stopped {
                    service.stop()?;
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while service.query_status()?.current_state != ServiceState::Stopped {
                    if std::time::Instant::now() >= deadline {
                        bail!("service stop timed out; data retained");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if matches!(command, ServiceCommand::Uninstall) {
                    service.delete()?;
                }
            }
            ServiceCommand::Status => println!("{:?}", service.query_status()?.current_state),
            _ => unreachable!(),
        }
        Ok(())
    }
    windows_service::define_windows_service!(ffi_main, service_main);
    pub fn dispatch() -> Result<()> {
        windows_service::service_dispatcher::start(NAME, ffi_main)?;
        Ok(())
    }
    fn service_main(_: Vec<std::ffi::OsString>) {
        let stop = tokio_util::sync::CancellationToken::new();
        let signal = stop.clone();
        let handle = windows_service::service_control_handler::register(NAME, move |event| {
            use windows_service::service_control_handler::ServiceControlHandlerResult;
            match event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    signal.cancel();
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        });
        let Ok(handle) = handle else {
            return;
        };
        let status = |state, code| ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(code),
            checkpoint: if state == ServiceState::StartPending {
                1
            } else {
                0
            },
            wait_hint: std::time::Duration::from_secs(10),
            process_id: None,
        };
        let _ = handle.set_service_status(status(ServiceState::StartPending, 0));
        // Native readiness is reported only after TLS and the listener are prepared.
        let result = crate::run_runtime(
            stop,
            ReadyNotifier {
                handle: Some(handle),
            },
        );
        let _ = handle.set_service_status(status(
            ServiceState::Stopped,
            if result.is_ok() { 0 } else { 1 },
        ));
    }
}
#[cfg(windows)]
pub use windows::dispatch;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_templates_preserve_argument_boundaries_and_non_root_identity() {
        let unit = linux_unit(
            Path::new("/opt/inori/inori-server"),
            Path::new("/var/lib/inori data%$x"),
        )
        .unwrap();
        assert!(unit.contains("User=inori"));
        assert!(unit.contains("\"/var/lib/inori data%%$$x\""));
        assert!(unit.contains("TimeoutStopSec=10"));
        let plist = mac_plist(
            Path::new("/usr/local/lib/inori/server"),
            Path::new("/Library/Application Support/A&B"),
        )
        .unwrap();
        assert!(plist.contains("A&amp;B"));
        assert!(plist.contains("<string>_inori</string>"));
        assert!(!plist.contains("<string>root</string>"));
        assert!(linux_unit(Path::new("/bad\npath"), Path::new("/data")).is_err());
    }
}

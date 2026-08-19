//! Per-user background-agent installation.
//!
//! The installed process is deliberately a *user-session* agent, not a system
//! daemon, so it uses the current user's Docker access. Desktop installs show
//! native approval dialogs; Linux headless installs use CLI or attached-TUI
//! approvals and systemd lingering.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Stdio;

#[cfg(any(target_os = "macos", test))]
const LABEL: &str = "com.harness-hat.manager";
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT: &str = "harness-hat.service";
#[cfg(target_os = "windows")]
const WINDOWS_TASK: &str = "Harness Hat";

pub fn install(explicit_config: Option<PathBuf>, headless: bool) -> Result<()> {
    install_inner(explicit_config, headless, None)
}

/// Install using an explicitly staged daemon executable. The graphical
/// launcher uses this after copying its bundled tools into a stable per-user
/// directory, so moving the original app/ZIP cannot break startup.
pub fn install_with_daemon(
    explicit_config: Option<PathBuf>,
    headless: bool,
    daemon: PathBuf,
) -> Result<()> {
    install_inner(explicit_config, headless, Some(daemon))
}

fn install_inner(
    explicit_config: Option<PathBuf>,
    headless: bool,
    staged_daemon: Option<PathBuf>,
) -> Result<()> {
    ensure_normal_user()?;
    if headless && !cfg!(target_os = "linux") {
        bail!("hat install --headless is supported on Linux only");
    }
    let (config_path, created_config) = resolve_config_path(explicit_config)?;
    if created_config {
        println!("Created default global config: {}", config_path.display());
    }
    // Validate before making a persistent startup change. In particular this
    // catches an invalid Docker directory rather than creating a restart loop.
    crate::config::load(&config_path)?;
    let service_executable = match staged_daemon {
        Some(daemon) => daemon
            .canonicalize()
            .context("canonicalizing staged Harness Hat daemon")?,
        None => {
            let executable = std::env::current_exe()
                .context("locating the current hat executable")?
                .canonicalize()
                .context("canonicalizing the current hat executable")?;
            daemon_executable(&executable)?
        }
    };

    #[cfg(target_os = "macos")]
    install_macos(&service_executable, &config_path)?;
    #[cfg(target_os = "linux")]
    install_linux(&service_executable, &config_path, headless)?;
    #[cfg(target_os = "windows")]
    install_windows(&service_executable, &config_path)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("hat install supports macOS, Linux with systemd, and Windows only");

    if headless {
        println!("Harness Hat headless background agent installed for this user.");
    } else {
        println!("Harness Hat background agent installed for this desktop user.");
    }
    println!("Config: {}", config_path.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    ensure_normal_user()?;
    #[cfg(target_os = "macos")]
    uninstall_macos()?;
    #[cfg(target_os = "linux")]
    uninstall_linux()?;
    #[cfg(target_os = "windows")]
    uninstall_windows()?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("hat uninstall supports macOS, Linux with systemd, and Windows only");

    println!("Harness Hat background agent removed for this user.");
    Ok(())
}

fn ensure_normal_user() -> Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        bail!("hat install/uninstall must run as a normal user; do not use sudo");
    }
    Ok(())
}

fn daemon_executable(current: &Path) -> Result<PathBuf> {
    let Some(parent) = current.parent() else {
        bail!("cannot locate hat-daemon next to {}", current.display());
    };
    let daemon_name = if cfg!(target_os = "windows") {
        "hat-daemon.exe"
    } else {
        "hat-daemon"
    };
    let candidate = parent.join(daemon_name);
    if candidate.is_file() {
        Ok(candidate)
    } else {
        bail!(
            "hat-daemon was not found next to {}; reinstall Harness Hat so the background service binary is available",
            current.display()
        )
    }
}

fn resolve_config_path(explicit_config: Option<PathBuf>) -> Result<(PathBuf, bool)> {
    let (path, created) = match explicit_config {
        Some(path) => (path, false),
        None => {
            let path = crate::manager::default_home_config_path()?;
            let created = ensure_default_config(&path)?;
            (path, created)
        }
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalizing config path {}", path.display()))?;
    Ok((path, created))
}

fn ensure_default_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    crate::init::write_sample_config(path)
        .with_context(|| format!("creating default global config at {}", path.display()))?;
    Ok(true)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_definition(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("service definition has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating service directory {}", parent.display()))?;
    crate::config::atomic_write_with_lock(path, contents.as_bytes())
        .with_context(|| format!("writing service definition {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting service definition {}", path.display()))?;
    }
    Ok(())
}

fn command_status(program: &str, args: &[String], action: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {action}: {program}"))?;
    if !status.success() {
        bail!("{action} failed with status {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn install_macos(executable: &Path, config_path: &Path) -> Result<()> {
    let path = launch_agent_path()?;
    write_definition(&path, &render_launchd_plist(executable, config_path))?;
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &domain])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    command_status(
        "launchctl",
        &[
            "bootstrap".into(),
            domain.clone(),
            path.display().to_string(),
        ],
        "loading Harness Hat launch agent",
    )?;
    command_status(
        "launchctl",
        &["kickstart".into(), "-k".into(), format!("{domain}/{LABEL}")],
        "starting Harness Hat launch agent",
    )
}

#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<()> {
    let path = launch_agent_path()?;
    if !path.exists() {
        return Ok(());
    }
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &domain])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::fs::remove_file(&path).with_context(|| format!("removing launch agent {}", path.display()))
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home.join(".config/systemd/user").join(SYSTEMD_UNIT))
}

#[cfg(target_os = "linux")]
fn install_linux(executable: &Path, config_path: &Path, headless: bool) -> Result<()> {
    let path = systemd_unit_path()?;
    if headless {
        command_status(
            "loginctl",
            &["enable-linger".into()],
            "enabling systemd lingering for the current user",
        )?;
    }
    write_definition(
        &path,
        &render_systemd_unit(executable, config_path, headless),
    )?;
    // A user manager commonly inherits these at desktop login. Import them at
    // install time as well so native approval dialogs can reach the current
    // graphical session immediately; service startup remains fail-closed if a
    // desktop session is not available.
    if !headless {
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "import-environment",
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "XDG_RUNTIME_DIR",
                "DBUS_SESSION_BUS_ADDRESS",
            ])
            .status();
    }
    command_status(
        "systemctl",
        &["--user".into(), "daemon-reload".into()],
        "reloading systemd user units",
    )?;
    command_status(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            SYSTEMD_UNIT.into(),
        ],
        "enabling Harness Hat user service",
    )
}

#[cfg(target_os = "linux")]
fn uninstall_linux() -> Result<()> {
    let path = systemd_unit_path()?;
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SYSTEMD_UNIT])
        .status();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing systemd user unit {}", path.display()))?;
    }
    command_status(
        "systemctl",
        &["--user".into(), "daemon-reload".into()],
        "reloading systemd user units",
    )
}

#[cfg(target_os = "windows")]
fn install_windows(executable: &Path, config_path: &Path) -> Result<()> {
    let task_command = format!(
        "\"{}\" --config \"{}\"",
        executable.display(),
        config_path.display()
    );
    command_status(
        "schtasks",
        &[
            "/Create".into(),
            "/TN".into(),
            WINDOWS_TASK.into(),
            "/TR".into(),
            task_command,
            "/SC".into(),
            "ONLOGON".into(),
            "/RL".into(),
            "LIMITED".into(),
            "/IT".into(),
            "/F".into(),
        ],
        "creating Harness Hat scheduled task",
    )?;
    command_status(
        "powershell.exe",
        &[
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            concat!(
                "$settings = New-ScheduledTaskSettingsSet ",
                "-RestartCount 255 ",
                "-RestartInterval (New-TimeSpan -Minutes 1) ",
                "-StartWhenAvailable; ",
                "Set-ScheduledTask -TaskName 'Harness Hat' -Settings $settings | Out-Null"
            )
            .into(),
        ],
        "configuring Harness Hat scheduled task retries",
    )?;
    command_status(
        "schtasks",
        &["/Run".into(), "/TN".into(), WINDOWS_TASK.into()],
        "starting Harness Hat scheduled task",
    )
}

#[cfg(target_os = "windows")]
fn uninstall_windows() -> Result<()> {
    // Deleting a scheduled task does not stop an instance that is already
    // running. End the registered instance first, then remove the task. A
    // final image-name kill also cleans up a daemon left behind by an older
    // installation whose task definition has already disappeared.
    run_windows_quietly("schtasks", &["/End", "/TN", WINDOWS_TASK]);
    run_windows_quietly("schtasks", &["/Delete", "/TN", WINDOWS_TASK, "/F"]);
    crate::process_util::terminate_hat_daemons()
        .context("terminating running Harness Hat daemon processes")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_windows_quietly(program: &str, args: &[&str]) {
    let mut command = Command::new(program);
    crate::process_util::hide_console_window(&mut command);
    let _ = command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(any(target_os = "linux", test))]
fn render_systemd_unit(executable: &Path, config_path: &Path, headless: bool) -> String {
    let after = if headless {
        ""
    } else {
        "After=graphical-session.target\n"
    };
    let headless_arg = if headless { " --headless" } else { "" };
    format!(
        "[Unit]\nDescription=Harness Hat background agent\n{after}\n[Service]\nType=simple\nExecStart={} --config {}{headless_arg}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(executable),
        systemd_escape(config_path),
    )
}

#[cfg(any(target_os = "macos", test))]
fn render_launchd_plist(executable: &Path, config_path: &Path) -> String {
    let path = launchd_path();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n  <key>Label</key><string>{LABEL}</string>\n  <key>ProgramArguments</key><array><string>{}</string><string>--config</string><string>{}</string></array>\n  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ProcessType</key><string>Interactive</string>\n</dict></plist>\n",
        xml_escape(&executable.display().to_string()),
        xml_escape(&config_path.display().to_string()),
        xml_escape(&path),
    )
}

#[cfg(any(target_os = "macos", test))]
fn launchd_path() -> String {
    let mut paths = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/Applications/Docker.app/Contents/Resources/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];
    if let Some(current) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&current) {
            let entry = entry.to_string_lossy().into_owned();
            if !entry.is_empty() && !paths.contains(&entry) {
                paths.push(entry);
            }
        }
    }
    paths.join(":")
}

#[cfg(any(target_os = "linux", test))]
fn systemd_escape(path: &Path) -> String {
    // systemd's ExecStart parser accepts C-style double-quoted arguments.
    let escaped = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_quotes_paths_and_uses_internal_service_mode() {
        let unit = render_systemd_unit(
            Path::new("/home/me/.cargo/bin/hat"),
            Path::new("/home/me/My Config/harness-hat.toml"),
            false,
        );
        assert!(unit.contains(
            "ExecStart=\"/home/me/.cargo/bin/hat\" --config \"/home/me/My Config/harness-hat.toml\""
        ));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_unit_escapes_specifier_characters() {
        let unit =
            render_systemd_unit(Path::new("/home/me/hat%stable"), Path::new("/tmp/x"), false);
        assert!(unit.contains("hat%%stable"));
    }

    #[test]
    fn headless_systemd_unit_has_no_graphical_dependency() {
        let unit = render_systemd_unit(
            Path::new("/home/me/.cargo/bin/hat-daemon"),
            Path::new("/home/me/.config/harness-hat/harness-hat.toml"),
            true,
        );
        assert!(unit.contains(" --headless\n"));
        assert!(!unit.contains("graphical-session.target"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_escapes_arguments() {
        let plist = render_launchd_plist(
            Path::new("/Applications/Harness & Hat/hat"),
            Path::new("/Users/me/rules & config.toml"),
        );
        assert!(plist.contains("Harness &amp; Hat"));
        assert!(plist.contains("rules &amp; config.toml"));
        assert!(!plist.contains("__service"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("Docker.app/Contents/Resources/bin"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
    }

    #[test]
    fn daemon_executable_uses_sibling_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("hat");
        let daemon = dir.path().join(if cfg!(target_os = "windows") {
            "hat-daemon.exe"
        } else {
            "hat-daemon"
        });
        std::fs::write(&daemon, b"daemon").unwrap();
        assert_eq!(daemon_executable(&current).unwrap(), daemon);
    }

    #[test]
    fn missing_default_config_is_created_without_replacing_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("harness-hat.toml");

        assert!(ensure_default_config(&config_path).unwrap());
        assert!(config_path.is_file());
        assert!(!ensure_default_config(&config_path).unwrap());
    }
}

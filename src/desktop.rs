//! Claude Desktop attachment for Harness Hat workspaces.
//!
//! The native app remains a host process, but Claude Code connects to the
//! selected Hat container through key-only SSH bound to host loopback. A
//! read-only managed settings file disables Desktop capabilities that would
//! otherwise cross the container boundary.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use std::fs::{self, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const AUTHORIZED_KEY_ENV: &str = "HARNESS_HAT_DESKTOP_SSH_AUTHORIZED_KEY";
pub const LABEL_DESKTOP: &str = "dev.harness-hat.desktop";
pub const LABEL_DESKTOP_SSH_IMAGE: &str = "dev.harness-hat.desktop-ssh";
pub const MANAGED_POLICY_CONTAINER_PATH: &str = "/etc/claude-code/managed-settings.json";
const SSH_CONTAINER_PORT: &str = "2222/tcp";
pub const SSH_CONNECTION_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const SSH_DISCONNECT_GRACE: Duration = Duration::from_secs(10 * 60);
pub const SSH_INITIAL_CONNECTION_GRACE: Duration = Duration::from_secs(30 * 60);
const LAUNCHER_SERVICE_REVISION: &str = "2";

#[derive(Clone, Debug)]
pub enum LauncherReadiness {
    Ready,
    NeedsSetup(String),
    DockerMissing,
    DockerNotRunning(String),
    OpenSshMissing,
    ClaudeMissing,
    BundleIncomplete(String),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopContainer {
    pub id: String,
    pub name: String,
    pub alias: String,
    pub workspace: String,
    pub template: String,
    pub mount_target: String,
    pub desktop_enabled: bool,
}

/// Inspect everything the no-terminal launcher needs without changing host
/// state. The GUI automatically performs setup when this reports
/// [`LauncherReadiness::NeedsSetup`].
pub fn launcher_readiness() -> LauncherReadiness {
    match launcher_readiness_inner() {
        Ok(readiness) => readiness,
        Err(error) => LauncherReadiness::Error(format!("{error:#}")),
    }
}

fn launcher_readiness_inner() -> Result<LauncherReadiness> {
    if let Some(missing) = missing_bundled_executables()? {
        return Ok(LauncherReadiness::BundleIncomplete(missing));
    }
    if which::which("docker").is_err() {
        return Ok(LauncherReadiness::DockerMissing);
    }
    if let Err(error) = crate::container::ensure_docker_installed_and_running() {
        return Ok(LauncherReadiness::DockerNotRunning(error.to_string()));
    }
    if find_open_ssh_tool("ssh-keygen").is_none() || find_open_ssh_tool("ssh").is_none() {
        return Ok(LauncherReadiness::OpenSshMissing);
    }
    if !claude_is_installed() {
        return Ok(LauncherReadiness::ClaudeMissing);
    }

    let config_path = crate::manager::default_home_config_path()?;
    if !config_path.exists() {
        return Ok(LauncherReadiness::NeedsSetup(
            "Harness Hat needs to create its private configuration and background service.".into(),
        ));
    }
    let config = crate::config::load(&config_path)?;
    let expected = launcher_service_version();
    let installed = std::fs::read_to_string(launcher_service_marker(&config)).unwrap_or_default();
    if installed.trim() != expected {
        return Ok(LauncherReadiness::NeedsSetup(
            "The bundled Harness Hat background service needs to be installed or updated.".into(),
        ));
    }
    if !daemon_reachable(&config) {
        return Ok(LauncherReadiness::NeedsSetup(
            "The Harness Hat background service is not running and needs to be repaired.".into(),
        ));
    }
    Ok(LauncherReadiness::Ready)
}

/// Create the default config, install/update the per-user service from the
/// bundled daemon, and wait until its control endpoint is reachable.
pub fn install_launcher_service() -> Result<PathBuf> {
    if let Some(missing) = missing_bundled_executables()? {
        bail!("the Harness Hat app bundle is incomplete: missing {missing}");
    }
    let daemon = stage_launcher_tools()?;
    crate::service::install_with_daemon(None, false, daemon)?;
    let config_path = crate::manager::default_home_config_path()?;
    let config = crate::config::load(&config_path)?;
    crate::init::refresh_managed_docker_assets(&config.docker_dir)?;
    std::fs::create_dir_all(&config.logging.log_dir)?;
    crate::config::atomic_write_with_lock(
        &launcher_service_marker(&config),
        format!("{}\n", launcher_service_version()).as_bytes(),
    )?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if daemon_reachable(&config) {
            return Ok(config_path);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "Harness Hat installed its background service, but the service did not start within 15 seconds"
    )
}

fn stage_launcher_tools() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating Harness Hat launcher")?;
    let source = executable
        .parent()
        .context("Harness Hat launcher has no containing directory")?;
    let suffix = if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    };
    let runtime_root = if cfg!(target_os = "macos") {
        dirs::data_dir()
    } else {
        dirs::data_local_dir()
    }
    .context("cannot determine the per-user application data directory")?
    .join("harness-hat")
    .join("bin")
    .join(launcher_service_version().replace(':', "-"));
    std::fs::create_dir_all(&runtime_root)?;
    for name in [format!("hat{suffix}"), format!("hat-daemon{suffix}")] {
        let from = source.join(&name);
        let to = runtime_root.join(&name);
        std::fs::copy(&from, &to).with_context(|| {
            format!(
                "installing bundled executable {} to {}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(runtime_root.join(format!("hat-daemon{suffix}")))
}

fn launcher_service_version() -> String {
    format!(
        "{}:{}",
        env!("CARGO_PKG_VERSION"),
        LAUNCHER_SERVICE_REVISION
    )
}

fn launcher_service_marker(config: &crate::config::Config) -> PathBuf {
    config.logging.log_dir.join("launcher-service-version")
}

fn daemon_reachable(config: &crate::config::Config) -> bool {
    let url = format!(
        "http://{}:{}/healthz",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    let Ok(client) = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    matches!(client.get(url).send(), Ok(response) if response.status().is_success() || response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE)
}

fn missing_bundled_executables() -> Result<Option<String>> {
    let executable = std::env::current_exe().context("locating Harness Hat launcher")?;
    let directory = executable
        .parent()
        .context("Harness Hat launcher has no containing directory")?;
    let suffix = if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    };
    let missing = [format!("hat{suffix}"), format!("hat-daemon{suffix}")]
        .into_iter()
        .filter(|name| !directory.join(name).is_file())
        .collect::<Vec<_>>();
    Ok((!missing.is_empty()).then(|| missing.join(", ")))
}

#[cfg(not(target_os = "windows"))]
fn find_open_ssh_tool(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

#[cfg(target_os = "windows")]
fn find_open_ssh_tool(name: &str) -> Option<PathBuf> {
    which::which(name).ok().or_else(|| {
        let root = std::env::var_os("SystemRoot")?;
        let path = PathBuf::from(root).join(format!("System32/OpenSSH/{name}.exe"));
        path.is_file().then_some(path)
    })
}

fn claude_is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        Path::new("/Applications/Claude.app").is_dir()
            || dirs::home_dir().is_some_and(|home| home.join("Applications/Claude.app").is_dir())
    }
    #[cfg(target_os = "windows")]
    {
        let Some(local_app_data) = dirs::data_local_dir() else {
            return false;
        };
        [
            local_app_data.join("AnthropicClaude/Claude.exe"),
            local_app_data.join("Programs/Claude/Claude.exe"),
            local_app_data.join("Claude/Claude.exe"),
        ]
        .iter()
        .any(|path| path.is_file())
            || which::which("Claude.exe").is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    false
}

#[cfg(target_os = "macos")]
pub fn start_docker_desktop() -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .args(["-a", "Docker"])
        .status()
        .context("starting Docker Desktop")?;
    anyhow::ensure!(status.success(), "macOS could not start Docker Desktop");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn start_docker_desktop() -> Result<()> {
    let program_files = std::env::var_os("ProgramFiles")
        .context("Windows Program Files directory is unavailable")?;
    let executable = PathBuf::from(program_files).join("Docker/Docker/Docker Desktop.exe");
    anyhow::ensure!(executable.is_file(), "Docker Desktop is not installed");
    let mut command = Command::new(executable);
    crate::process_util::hide_console_window(&mut command);
    command.spawn().context("starting Docker Desktop")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn start_docker_desktop() -> Result<()> {
    bail!("Docker Desktop launch is supported only on macOS and Windows")
}

pub const MANAGED_POLICY: &str = r#"{
  "disableBrowserExternalNavigation": true,
  "browserExternalPageTools": "disabled",
  "disableClaudeAiConnectors": true,
  "deniedMcpServers": [
    { "serverName": "computer-use" },
    { "serverName": "claude-in-chrome" }
  ],
  "permissions": {
    "deny": [
      "mcp__computer-use__*",
      "mcp__claude-in-chrome__*"
    ]
  }
}
"#;

pub fn authorized_key_env(state_dir: &Path) -> Result<(String, String)> {
    let identity = ensure_identity(state_dir)?;
    let public_key = fs::read_to_string(identity.with_extension("pub"))
        .context("reading Harness Hat Claude Desktop SSH public key")?;
    let public_key = public_key.trim();
    anyhow::ensure!(
        public_key.starts_with("ssh-ed25519 ") && !public_key.contains(['\n', '\r']),
        "invalid Harness Hat Claude Desktop SSH public key"
    );
    Ok((AUTHORIZED_KEY_ENV.to_string(), public_key.to_string()))
}

pub fn is_desktop_container(container_name: &str) -> Result<bool> {
    let output = docker_output(&[
        "inspect",
        "-f",
        &format!("{{{{index .Config.Labels {:?}}}}}", LABEL_DESKTOP),
        container_name,
    ])?;
    Ok(output.trim() == "true")
}

pub fn image_supports_desktop_ssh(image: &str) -> bool {
    docker_output(&[
        "image",
        "inspect",
        "-f",
        &format!("{{{{index .Config.Labels {:?}}}}}", LABEL_DESKTOP_SSH_IMAGE),
        image,
    ])
    .is_ok_and(|output| output.trim() == "1")
}

/// Enumerate running Desktop-enabled Hat containers using only Docker labels.
pub fn running_containers() -> Result<Vec<DesktopContainer>> {
    let output = docker_output(&[
        "ps",
        "--filter",
        &format!("label={LABEL_DESKTOP}=true"),
        "--format",
        "{{.ID}}\t{{.Names}}\t{{.Labels}}",
    ])?;
    Ok(parse_running_containers(&output))
}

/// Enumerate every running Hat container. The graphical launcher uses this to
/// manage ordinary sessions as well as sessions configured for Desktop SSH.
pub fn running_hat_containers() -> Result<Vec<DesktopContainer>> {
    let output = docker_output(&[
        "ps",
        "--filter",
        &format!("label={}", crate::container::LABEL_ALIAS),
        "--format",
        "{{.ID}}\t{{.Names}}\t{{.Labels}}",
    ])?;
    Ok(parse_running_containers(&output))
}

fn parse_running_containers(output: &str) -> Vec<DesktopContainer> {
    output
        .lines()
        .filter_map(|line| {
            let mut columns = line.splitn(3, '\t');
            let (id, name, labels) = (columns.next()?, columns.next()?, columns.next()?);
            Some(DesktopContainer {
                id: id.trim().to_string(),
                name: name.trim().to_string(),
                alias: crate::container::parse_docker_label(labels, crate::container::LABEL_ALIAS)
                    .unwrap_or_default(),
                workspace: crate::container::parse_docker_label(
                    labels,
                    crate::container::LABEL_WORKSPACE,
                )
                .unwrap_or_default(),
                template: crate::container::parse_docker_label(
                    labels,
                    crate::container::LABEL_TEMPLATE,
                )
                .unwrap_or_default(),
                mount_target: crate::container::parse_docker_label(
                    labels,
                    crate::container::LABEL_MOUNT_TARGET,
                )
                .unwrap_or_default(),
                desktop_enabled: crate::container::parse_docker_label(labels, LABEL_DESKTOP)
                    .as_deref()
                    == Some("true"),
            })
        })
        .collect()
}

/// Whether Docker currently has an established TCP connection to this
/// container's SSH server. The command prints an explicit state so an empty
/// connection list is not confused with a failed `docker exec`.
pub fn ssh_connected(container_name: &str) -> Result<bool> {
    let output = docker_output(&[
        "exec",
        container_name,
        "sh",
        "-c",
        "if ss -Hnt state established '( sport = :2222 )' | grep -q .; then printf connected; else printf disconnected; fi",
    ])?;
    match output.trim() {
        "connected" => Ok(true),
        "disconnected" => Ok(false),
        state => bail!("unexpected SSH connection state from {container_name}: {state:?}"),
    }
}

/// Stop a container only after re-checking its Desktop label. This keeps the
/// graphical launcher's Stop action scoped to containers it is meant to own.
pub fn stop_container(container_name: &str) -> Result<()> {
    let alias = docker_output(&[
        "inspect",
        "-f",
        &format!(
            "{{{{index .Config.Labels {:?}}}}}",
            crate::container::LABEL_ALIAS
        ),
        container_name,
    ])?;
    anyhow::ensure!(
        !alias.trim().is_empty() && alias.trim() != "<no value>",
        "refusing to stop non-Hat container {container_name}"
    );
    docker_output(&["rm", "-f", container_name])?;
    Ok(())
}

pub fn open(
    container_name: &str,
    workspace_name: &str,
    workdir: &str,
    state_dir: &Path,
) -> Result<()> {
    let ssh_alias = prepare(container_name, workspace_name, state_dir)?;
    launch_claude()?;
    println!(
        "Claude Desktop launch requested. In Code, add or select SSH host \"{ssh_alias}\" and folder \"{workdir}\"."
    );
    println!(
        "Safety boundary: that SSH session runs in container {container_name}; separate Local, Chat, or Cowork sessions are outside Harness Hat."
    );
    Ok(())
}

/// Register a running Desktop container in the user's SSH configuration
/// without opening Claude Desktop.
pub fn prepare(container_name: &str, workspace_name: &str, state_dir: &Path) -> Result<String> {
    let identity = ensure_identity(state_dir)?;
    let port = published_ssh_port(container_name)?;
    let host_key = wait_for_host_key(container_name, Duration::from_secs(10))?;
    let ssh_alias = register_connection(workspace_name, port, &host_key, &identity, state_dir)?;
    Ok(ssh_alias)
}

/// Open Claude Desktop without claiming that it is already attached to SSH.
pub fn launch_claude() -> Result<()> {
    launch_app()
}

fn ensure_identity(state_dir: &Path) -> Result<PathBuf> {
    let dir = state_dir.join("claude-desktop");
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating Claude Desktop state directory {}", dir.display()))?;
    set_private_dir_permissions(&dir)?;
    let identity = dir.join("id_ed25519");
    let public_identity = identity.with_extension("pub");
    if identity.exists() && public_identity.exists() {
        return Ok(identity);
    }
    let ssh_keygen = find_open_ssh_tool("ssh-keygen")
        .context("ssh-keygen is required for `hat ws --desktop`; install the OpenSSH client")?;
    if identity.exists() {
        let output = Command::new(&ssh_keygen)
            .args(["-y", "-f"])
            .arg(&identity)
            .output()
            .context("recovering Claude Desktop SSH public key")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen could not recover the public key: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let key = String::from_utf8(output.stdout).context("SSH public key was not UTF-8")?;
        fs::write(&public_identity, key)
            .context("writing recovered Claude Desktop SSH public key")?;
        return Ok(identity);
    }
    if public_identity.exists() {
        fs::remove_file(&public_identity)
            .context("removing orphaned Claude Desktop SSH public key")?;
    }
    let output = Command::new(ssh_keygen)
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "harness-hat-desktop",
            "-f",
        ])
        .arg(&identity)
        .output()
        .context("generating Claude Desktop SSH identity")?;
    if !output.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(identity)
}

/// Loopback host port currently forwarding to a Desktop container's SSH service.
pub fn published_ssh_port(container_name: &str) -> Result<u16> {
    let output = docker_output(&["port", container_name, SSH_CONTAINER_PORT])?;
    output
        .lines()
        .filter_map(|line| line.rsplit_once(':').map(|(_, port)| port.trim()))
        .find_map(|port| port.parse::<u16>().ok())
        .context("Docker did not publish the Claude Desktop SSH port on loopback")
}

fn wait_for_host_key(container_name: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let path = "/home/coder/.ssh/harness-hat-desktop-host-key.pub";
    loop {
        let mut command = Command::new("docker");
        crate::process_util::hide_console_window(&mut command);
        let output = command.args(["exec", container_name, "cat", path]).output();
        if let Ok(output) = output
            && output.status.success()
        {
            let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if key.starts_with("ssh-ed25519 ") {
                return Ok(key);
            }
        }
        if Instant::now() >= deadline {
            bail!("Claude Desktop SSH service did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn register_connection(
    workspace_name: &str,
    port: u16,
    host_key: &str,
    identity: &Path,
    state_dir: &Path,
) -> Result<String> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).with_context(|| format!("creating {}", ssh_dir.display()))?;
    set_private_dir_permissions(&ssh_dir)?;
    register_connection_with_user_config(
        workspace_name,
        port,
        host_key,
        identity,
        state_dir,
        &ssh_dir.join("config"),
    )
}

fn register_connection_with_user_config(
    workspace_name: &str,
    port: u16,
    host_key: &str,
    identity: &Path,
    state_dir: &Path,
    user_config: &Path,
) -> Result<String> {
    let ssh_alias = workspace_ssh_alias(workspace_name);
    let desktop_dir = state_dir.join("claude-desktop");
    let config_dir = desktop_dir.join("ssh-config.d");
    let known_hosts_dir = desktop_dir.join("known-hosts");
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating {}", config_dir.display()))?;
    fs::create_dir_all(&known_hosts_dir)
        .with_context(|| format!("creating {}", known_hosts_dir.display()))?;
    set_private_dir_permissions(&config_dir)?;
    set_private_dir_permissions(&known_hosts_dir)?;

    let key_fields = host_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let known_hosts = known_hosts_dir.join(&ssh_alias);
    crate::config::atomic_write_with_lock(
        &known_hosts,
        format!("{ssh_alias} {key_fields}\n").as_bytes(),
    )
    .with_context(|| format!("writing {}", known_hosts.display()))?;

    let config_path = config_dir.join(format!("{ssh_alias}.conf"));
    let connection = format!(
        "Host {ssh_alias}\n  HostName 127.0.0.1\n  Port {port}\n  User coder\n  IdentityFile {}\n  IdentitiesOnly yes\n  HostKeyAlias {ssh_alias}\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  ForwardAgent no\n  ForwardX11 no\n",
        ssh_path(identity)?,
        ssh_path(&known_hosts)?,
    );
    crate::config::atomic_write_with_lock(&config_path, connection.as_bytes())
        .with_context(|| format!("writing {}", config_path.display()))?;
    ensure_user_ssh_include(user_config, &config_dir)?;
    Ok(ssh_alias)
}

fn ensure_user_ssh_include(path: &Path, config_dir: &Path) -> Result<()> {
    let existed = path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    let existing = fs::read_to_string(path).unwrap_or_default();
    let include_pattern = config_dir.join("*");
    let line = format!("Include {}", ssh_path(&include_pattern)?);
    let updated = ssh_config_with_global_include(&existing, &line);
    if updated != existing {
        file.set_len(0)?;
        file.seek(std::io::SeekFrom::Start(0))?;
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
    }
    if !existed {
        set_private_file_permissions(path)?;
    }
    Ok(())
}

fn ssh_config_with_global_include(existing: &str, include_line: &str) -> String {
    // An Include inherits the preceding Host/Match scope. Keep Hat's include
    // before every block so its generated hosts are available globally.
    let remaining = existing
        .lines()
        .filter(|line| line.trim() != include_line)
        .collect::<Vec<_>>()
        .join("\n");
    if remaining.is_empty() {
        format!("{include_line}\n")
    } else {
        format!("{include_line}\n{remaining}\n")
    }
}

/// Stable host name shown in Claude Desktop's SSH picker for a workspace.
pub fn workspace_ssh_alias(workspace_name: &str) -> String {
    use sha2::{Digest, Sha256};
    let slug = workspace_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "workspace" } else { slug };
    let slug = &slug[..slug.len().min(40)];
    let digest = hex::encode(Sha256::digest(workspace_name.as_bytes()));
    format!("hat-{slug}-{}", &digest[..8])
}

fn ssh_path(path: &Path) -> Result<String> {
    let rendered = path.to_string_lossy().replace('\\', "/");
    anyhow::ensure!(
        !rendered.contains(['\n', '\r', '"']),
        "SSH path contains unsupported characters: {}",
        path.display()
    );
    Ok(format!("\"{rendered}\""))
}

fn docker_output(args: &[&str]) -> Result<String> {
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args(args)
        .output()
        .context("running docker command")?;
    if !output.status.success() {
        bail!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn launch_app() -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .args(["-a", "Claude"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("launching Claude Desktop")?;
    anyhow::ensure!(
        status.success(),
        "Claude Desktop is not installed; install it and retry"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn launch_app() -> Result<()> {
    let local_app_data =
        dirs::data_local_dir().context("cannot determine local app data directory")?;
    let candidates = [
        local_app_data.join("AnthropicClaude/Claude.exe"),
        local_app_data.join("Programs/Claude/Claude.exe"),
        local_app_data.join("Claude/Claude.exe"),
    ];
    let executable = candidates
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| which::which("Claude.exe").ok())
        .context("Claude Desktop is not installed; install it and retry")?;
    let mut command = Command::new(executable);
    crate::process_util::hide_console_window(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launching Claude Desktop")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn launch_app() -> Result<()> {
    let _ = Stdio::null();
    bail!("Claude Desktop launching is currently supported on macOS and Windows")
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn managed_policy_disables_host_escape_tools() {
        let policy: Value = serde_json::from_str(MANAGED_POLICY).unwrap();
        assert_eq!(policy["disableBrowserExternalNavigation"], true);
        assert_eq!(policy["browserExternalPageTools"], "disabled");
        assert_eq!(policy["disableClaudeAiConnectors"], true);
        let deny = policy["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|value| value == "mcp__computer-use__*"));
        assert!(deny.iter().any(|value| value == "mcp__claude-in-chrome__*"));
    }

    #[test]
    fn workspace_alias_is_stable_safe_and_collision_resistant() {
        let first = workspace_ssh_alias("My Project");
        assert!(first.starts_with("hat-my-project-"));
        assert_eq!(first, workspace_ssh_alias("My Project"));
        assert_ne!(first, workspace_ssh_alias("my_project"));
        assert!(
            first
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        );
    }

    #[test]
    fn parses_running_desktop_container_labels() {
        let output = "abc123\tharness-hat-rust-deadbeef\tharness-hat.alias=7,harness-hat.workspace=my-project,harness-hat.template=rust,harness-hat.mount-target=/work\n";
        assert_eq!(
            parse_running_containers(output),
            vec![DesktopContainer {
                id: "abc123".into(),
                name: "harness-hat-rust-deadbeef".into(),
                alias: "7".into(),
                workspace: "my-project".into(),
                template: "rust".into(),
                mount_target: "/work".into(),
                desktop_enabled: false,
            }]
        );
    }

    #[test]
    fn repeated_registration_replaces_workspace_files_and_adds_one_include() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let identity = state.join("claude-desktop/id_ed25519");
        fs::create_dir_all(identity.parent().unwrap()).unwrap();
        fs::write(&identity, "test identity").unwrap();
        let user_config = root.path().join("home/.ssh/config");
        fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        fs::write(
            &user_config,
            "Host kiosk\n  HostName kiosk.example\nInclude \"/old/location/*\"\n",
        )
        .unwrap();

        let alias = register_connection_with_user_config(
            "My Project",
            41001,
            "ssh-ed25519 AAAAfirst comment",
            &identity,
            &state,
            &user_config,
        )
        .unwrap();
        register_connection_with_user_config(
            "My Project",
            41002,
            "ssh-ed25519 AAAAsecond comment",
            &identity,
            &state,
            &user_config,
        )
        .unwrap();

        let include = fs::read_to_string(&user_config).unwrap();
        assert_eq!(
            include
                .lines()
                .filter(|line| line.contains("ssh-config.d"))
                .count(),
            1
        );
        assert!(include.lines().next().unwrap().contains("ssh-config.d"));
        assert!(include.contains("Host kiosk\n  HostName kiosk.example"));
        let config =
            fs::read_to_string(state.join(format!("claude-desktop/ssh-config.d/{alias}.conf")))
                .unwrap();
        assert!(config.contains("Port 41002"));
        assert!(!config.contains("Port 41001"));
        let known_host =
            fs::read_to_string(state.join(format!("claude-desktop/known-hosts/{alias}"))).unwrap();
        assert!(known_host.contains("AAAAsecond"));
        assert!(!known_host.contains("AAAAfirst"));
    }
}

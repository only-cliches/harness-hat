//! `hat sh` — operate on a running session.
//!
//! Session discovery, attach, stop, and editor actions use Docker labels and
//! commands directly. The explicit `--kill-connections` action uses control
//! metadata from the selected container to ask the owning manager to cut that
//! session's scoped proxy sockets.
//!
//! Sessions only exist while the manager is running. Each session container is
//! launched as `docker run --rm -it` owned by the manager's PTY, so quitting
//! the manager (or that session's terminal) tears the container down and
//! `--rm` removes it. `hat sh` therefore only finds a session while the
//! manager that launched it is still running — run it from a second terminal
//! alongside the live manager.

use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::process::Command;

use crate::container::{
    LABEL_ALIAS, LABEL_MOUNT_TARGET, LABEL_SESSION, LABEL_SHELL, LABEL_TEMPLATE, LABEL_WORKSPACE,
    SHELL_HOME, SHELL_USER, parse_docker_label,
};

/// Entry point for the `sh` subcommand. With an id, attaches to that
/// session, optionally running `args` as a command instead of bash; `kill`
/// terminates the named session instead of attaching; without an id, prints
/// running sessions. `kill_connections` transiently drops the selected
/// session's currently-open proxy connections.
pub fn run(
    id: Option<String>,
    kill: bool,
    kill_connections: bool,
    args: Vec<OsString>,
) -> Result<i32> {
    if which::which("docker").is_err() {
        bail!(
            "docker not found in PATH — `{} sh` requires Docker",
            crate::cli::COMMAND_NAME
        );
    }
    match (id, kill, kill_connections) {
        (Some(id), true, false) => {
            terminate(&id)?;
            Ok(0)
        }
        (None, true, false) => bail!(
            "shell kill mode requires a session ID, e.g. `{} sh 42 --kill`",
            crate::cli::COMMAND_NAME,
        ),
        (Some(id), false, true) => {
            kill_network_connections(&id)?;
            Ok(0)
        }
        (None, false, true) => bail!(
            "network connection kill mode requires a session ID, e.g. `{} sh 42 --kill-connections`",
            crate::cli::COMMAND_NAME,
        ),
        (Some(id), false, false) => attach(&id, &args),
        (None, false, false) => {
            list()?;
            Ok(0)
        }
        (_, true, true) => unreachable!("clap rejects conflicting shell actions"),
    }
}

/// Open a running session's workspace in an external editor via the Dev
/// Containers "attached container" URI scheme:
/// `vscode-remote://attached-container+<hex container id>/<path>`.
///
/// The editor is an executable name, not a shell command. It must resolve on
/// PATH and is invoked with exactly `--folder-uri <URI>`.
pub fn open(id: &str, editor: crate::cli::OpenEditor) -> Result<()> {
    let session = session_for_id(id)?;
    let uri = attached_container_uri(&session);

    let display = editor.display();
    let resolved = resolve_editor(&editor)?;
    let mut command = Command::new(&resolved);
    crate::process_util::hide_console_window(&mut command);
    let status = command
        .args(["--folder-uri", &uri])
        .status()
        .with_context(|| format!("launching `{display}` from {}", resolved.display()))?;
    if !status.success() {
        bail!("`{display} --folder-uri {uri}` exited with {status}");
    }
    println!("Opened session {} in {display}.", normalize_id(id));
    Ok(())
}

fn resolve_editor(editor: &crate::cli::OpenEditor) -> Result<std::path::PathBuf> {
    let display = editor.display();
    which::which(editor.binary()).with_context(|| {
        format!("editor `{display}` was not found on PATH — install it or provide a PATH wrapper")
    })
}

fn attached_container_uri(session: &Session) -> String {
    let mount_target = session.mount_target.as_deref().unwrap_or("/workspace");
    let hex_id = hex_encode(session.container_id.as_bytes());
    format!("vscode-remote://attached-container+{hex_id}{mount_target}")
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A running harness-hat session, as seen through docker labels.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) alias: String,
    pub(crate) container_id: String,
    pub(crate) workspace: String,
    pub(crate) template: String,
    pub(crate) name: String,
    pub(crate) mount_target: Option<String>,
    pub(crate) session_token: String,
}

/// Enumerate running harness-hat sessions via `docker ps`. Returns containers
/// in docker's natural order (newest first), so callers wanting most-recent
/// behavior can take the first element.
pub(crate) fn running_sessions() -> Result<Vec<Session>> {
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args([
            "ps",
            "--filter",
            &format!("label={LABEL_ALIAS}"),
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.Labels}}",
        ])
        .output()
        .context("running docker ps")?;
    if !output.status.success() {
        bail!(
            "docker ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(parse_running_sessions(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_running_sessions(output: &str) -> Vec<Session> {
    let mut sessions = Vec::new();
    for line in output.lines() {
        let mut columns = line.splitn(3, '\t');
        let (Some(container_id), Some(name), Some(labels)) =
            (columns.next(), columns.next(), columns.next())
        else {
            continue;
        };
        let Some(alias) = parse_docker_label(labels, LABEL_ALIAS) else {
            continue;
        };
        sessions.push(Session {
            alias: normalize_id(&alias),
            container_id: container_id.trim().to_string(),
            workspace: parse_docker_label(labels, LABEL_WORKSPACE).unwrap_or_default(),
            template: parse_docker_label(labels, LABEL_TEMPLATE).unwrap_or_default(),
            name: name.trim().to_string(),
            mount_target: parse_docker_label(labels, LABEL_MOUNT_TARGET)
                .filter(|target| !target.trim().is_empty()),
            session_token: parse_docker_label(labels, LABEL_SESSION).unwrap_or_default(),
        });
    }
    sessions
}

/// Normalize a user-supplied id to the stored form. Numeric ids are rendered
/// without leading zeroes so legacy padded ids (for example `0042`) still
/// resolve to the new integer id `42`. Non-numeric or overflowing ids pass
/// through unchanged.
fn normalize_id(id: &str) -> String {
    id.parse::<u64>()
        .map(|value| value.to_string())
        .unwrap_or_else(|_| id.to_string())
}

fn attach(id: &str, extra_args: &[OsString]) -> Result<i32> {
    let session = session_for_id(id)?;
    exec_into_container(&session.name, extra_args)
}

fn shell_exec_env_vars() -> Vec<String> {
    shell_exec_env_pairs()
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

pub(crate) fn shell_exec_env_pairs() -> Vec<(String, String)> {
    shell_exec_env_pairs_with_passthrough(&[])
}

/// Like [`shell_exec_env_pairs`], but also reads `extra_passthrough` names
/// (typically a container's configured `env_passthrough` list) from this
/// process's own environment.
///
/// This exists so `env_passthrough` values are captured from the short-lived
/// CLI invocation — which always has the caller's current shell environment —
/// rather than relying on `docker run -e NAME` (no value) to read them from
/// the long-running daemon process, whose environment is frozen at whatever
/// it was when the daemon started (typically just `PATH`; see
/// `service::install`). Without this, a var exported after the daemon started
/// is invisible to it until the daemon process itself is restarted.
pub(crate) fn shell_exec_env_pairs_with_passthrough(
    extra_passthrough: &[String],
) -> Vec<(String, String)> {
    const PASSTHROUGH: [&str; 3] = ["TERM", "COLORTERM", "COLORFGBG"];
    let mut env_vars: Vec<(String, String)> = PASSTHROUGH
        .into_iter()
        .filter_map(|name| {
            env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| (name.to_string(), value))
        })
        .collect();

    if !env_vars.iter().any(|(name, _)| name == "TERM") {
        env_vars.push(("TERM".to_string(), "xterm-256color".to_string()));
    }

    for name in extra_passthrough {
        if env_vars.iter().any(|(existing, _)| existing == name) {
            continue;
        }
        if let Some(value) = env::var(name).ok().filter(|value| !value.trim().is_empty()) {
            env_vars.push((name.clone(), value));
        }
    }

    env_vars
}

fn terminate(id: &str) -> Result<()> {
    let wanted = normalize_id(id);
    let session = session_for_id(id)?;
    let name = &session.name;
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args(["rm", "-f", name])
        .output()
        .context("terminating session container with docker rm -f")?;
    if !output.status.success() {
        bail!(
            "failed to terminate session '{wanted}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    println!("Stopped session {wanted} ({name}).");
    Ok(())
}

fn kill_network_connections(id: &str) -> Result<()> {
    let wanted = normalize_id(id);
    let session = session_for_id(id)?;
    if session.session_token.is_empty() {
        bail!(
            "session {wanted} predates network disconnect support; start a new session and retry"
        );
    }
    let env = read_container_environment(&session.name)?;
    let token = env_value(&env, "HARNESS_HAT_TOKEN")
        .context("session does not expose its manager authentication token")?;
    let container_url = env_value(&env, "HARNESS_HAT_URL")
        .context("session does not expose its manager control URL")?;
    let control_url = host_control_url(container_url)?;

    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("building network disconnect client")?
        .post(format!("{control_url}/container/network/disconnect"))
        .bearer_auth(token)
        .header("x-harness-hat-session-token", &session.session_token)
        .send()
        .context("requesting network disconnect; is the manager running?")?;
    let status = response.status();
    let body = response.bytes().context("reading manager response")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<crate::server::ErrorResponse>(&body) {
            bail!("{}: {}", error.error, error.reason);
        }
        bail!(
            "manager request failed ({status}): {}",
            String::from_utf8_lossy(&body).trim()
        );
    }
    let result: crate::server::NetworkDisconnectResponse =
        serde_json::from_slice(&body).context("decoding manager response")?;
    println!(
        "Killed {} current network connection(s) for session {wanted}. Future connections remain policy-controlled.",
        result.connections_killed
    );
    Ok(())
}

fn read_container_environment(container_name: &str) -> Result<Vec<String>> {
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args(["inspect", "-f", "{{json .Config.Env}}", container_name])
        .output()
        .context("reading session control metadata with docker inspect")?;
    if !output.status.success() {
        bail!(
            "docker inspect failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("decoding session environment")
}

fn env_value<'a>(env: &'a [String], name: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|entry| entry.strip_prefix(name)?.strip_prefix('='))
}

fn host_control_url(container_url: &str) -> Result<String> {
    let mut url = url::Url::parse(container_url).context("parsing session manager URL")?;
    url.set_host(Some("127.0.0.1"))
        .map_err(|_| anyhow::anyhow!("session manager URL has an invalid host"))?;
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn session_for_id(id: &str) -> Result<Session> {
    let wanted = normalize_id(id);
    let sessions = running_sessions()?;
    let mut matches: Vec<Session> = sessions.into_iter().filter(|s| s.alias == wanted).collect();

    let session = match matches.len() {
        0 => bail!(
            "no running session with id '{wanted}'. Run `{} sh` to list sessions. \
             (Sessions only exist while the manager is running — is it still open?)",
            crate::cli::COMMAND_NAME
        ),
        1 => matches.remove(0),
        many => {
            // Aliases are deduped at launch, so this should not happen; pick the
            // first deterministically rather than failing the user outright.
            eprintln!("warning: {many} running sessions share id '{wanted}'; using the first");
            matches.remove(0)
        }
    };

    Ok(session)
}

/// Run `docker exec` against a container with optional trailing argv (or
/// `/bin/bash` when empty), with the same TTY auto-detect + signal guarding +
fn read_container_label(container_name: &str, label: &str) -> Result<String> {
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args([
            "inspect",
            "-f",
            &format!("{{{{index .Config.Labels \"{label}\"}}}}"),
            container_name,
        ])
        .output()
        .context("running docker inspect")?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "<no value>" {
        anyhow::bail!("label {label} not set on container {container_name}");
    }
    Ok(value)
}

/// host-terminal cleanup the interactive shell flow needs. Reused by
/// `hat ws`.
///
/// Don't `exec()` into docker exec. If the manager exits while this shell is
/// attached, the `docker run --rm` container dies and `docker exec` is ripped
/// out before any program inside (bash readline, vim, fzf, the user's
/// prompt) can send its DECRST cleanup sequences. The host terminal is then
/// stuck with focus reporting / bracketed paste / mouse mode / hidden cursor
/// left on. Staying as the parent lets us emit the disable sequences
/// ourselves on the way out.
///
/// Because we're no longer the docker exec process itself, Ctrl-C in the
/// terminal would be delivered to both us and the child via the foreground
/// process group. Ignore the terminal-driven signals here so they're handled
/// by `docker exec` (which forwards them through the PTY) and we survive to
/// run the cleanup.
pub(crate) fn exec_into_container(container_name: &str, extra_args: &[OsString]) -> Result<i32> {
    exec_into_container_at(container_name, extra_args, None)
}

/// Same as [`exec_into_container`], but starts the command in an explicit
/// container workdir. Workspace attach uses this to preserve the caller's
/// relative directory inside the workspace mount.
pub(crate) fn exec_into_container_at(
    container_name: &str,
    extra_args: &[OsString],
    workdir: Option<&str>,
) -> Result<i32> {
    let stdin_is_tty = std::io::stdin().is_terminal();

    // Read the shell label stamped at launch time; fall back to bash for
    // containers started before this feature existed.
    let attach_shell = read_container_label(container_name, LABEL_SHELL)
        .unwrap_or_else(|_| "/bin/bash".to_string());

    let mut command = Command::new("docker");
    command.args(docker_exec_args(
        container_name,
        &attach_shell,
        extra_args,
        workdir,
        stdin_is_tty,
        shell_exec_env_vars(),
    ));

    let _signal_guard = IgnoreTerminalSignals::install();
    let _terminal_reset = TerminalModesResetGuard::new();
    let status = command.status().context("running docker exec")?;
    Ok(status.code().unwrap_or(1))
}

fn docker_exec_args(
    container_name: &str,
    attach_shell: &str,
    extra_args: &[OsString],
    workdir: Option<&str>,
    stdin_is_tty: bool,
    env_vars: Vec<String>,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("exec"),
        OsString::from(if stdin_is_tty { "-it" } else { "-i" }),
        OsString::from("-u"),
        OsString::from(SHELL_USER),
        OsString::from("-e"),
        OsString::from(format!("HOME={SHELL_HOME}")),
    ];
    for env_var in env_vars {
        args.push(OsString::from("-e"));
        args.push(OsString::from(env_var));
    }
    if let Some(workdir) = workdir.filter(|path| !path.is_empty()) {
        args.push(OsString::from("--workdir"));
        args.push(OsString::from(workdir));
    }
    args.push(OsString::from(container_name));
    if extra_args.is_empty() {
        args.push(OsString::from(attach_shell));
    } else {
        args.extend(extra_args.iter().cloned());
    }
    args
}

struct TerminalModesResetGuard {
    enabled: bool,
}

impl TerminalModesResetGuard {
    fn new() -> Self {
        Self {
            enabled: std::io::stdout().is_terminal(),
        }
    }
}

impl Drop for TerminalModesResetGuard {
    fn drop(&mut self) {
        if self.enabled {
            restore_host_terminal_modes();
        }
    }
}

#[cfg(unix)]
struct IgnoreTerminalSignals {
    prev: [(libc::c_int, libc::sighandler_t); 3],
}

#[cfg(unix)]
impl IgnoreTerminalSignals {
    fn install() -> Self {
        let signals = [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP];
        let mut prev = [(0, 0 as libc::sighandler_t); 3];
        for (slot, sig) in prev.iter_mut().zip(signals) {
            // SAFETY: setting SIG_IGN on terminal-driven signals; the returned
            // sighandler_t is stored verbatim for later restoration.
            let old = unsafe { libc::signal(sig, libc::SIG_IGN) };
            *slot = (sig, old);
        }
        Self { prev }
    }
}

#[cfg(unix)]
impl Drop for IgnoreTerminalSignals {
    fn drop(&mut self) {
        for &(sig, old) in &self.prev {
            // SAFETY: restoring the previously-saved disposition for `sig`.
            unsafe {
                libc::signal(sig, old);
            }
        }
    }
}

#[cfg(not(unix))]
struct IgnoreTerminalSignals;

#[cfg(not(unix))]
impl IgnoreTerminalSignals {
    fn install() -> Self {
        Self
    }
}

/// Reset the terminal modes a program inside `docker exec` may have enabled but
/// never gotten to disable. We can't know which modes the inner program set, so
/// we unconditionally disable the common ones whose "stuck" state is visible to
/// the user (focus reporting prints `^[[I` / `^[[O` on focus changes; bracketed
/// paste wraps every paste in `^[[200~`; mouse reporting eats clicks; kitty key
/// encoding leaves `CSI u` text in the app when legacy tools resume; hidden
/// cursor makes shells look frozen). Sequences that have no effect when already
/// off are harmless, so this is safe to run on every exit.
fn restore_host_terminal_modes() {
    const RESET: &[u8] = concat!(
        "\x1b<u",      // kitty keyboard protocol: pop mode
        "\x1b[>4;0m",  // xterm: disable modifyOtherKeys
        "\x1b[?1004l", // focus reporting
        "\x1b[?2004l", // bracketed paste
        "\x1b[?1000l", // mouse: X10
        "\x1b[?1002l", // mouse: button-event
        "\x1b[?1003l", // mouse: any-event
        "\x1b[?1006l", // mouse: SGR encoding
        "\x1b[?1015l", // mouse: urxvt encoding
        "\x1b[?25h",   // cursor visible
        "\x1b[?1049l", // leave alternate screen
        "\x1b[?7h",    // line wrap on
        "\x1b[0m",     // reset SGR
    )
    .as_bytes();
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(RESET);
    let _ = stdout.flush();
}

fn list() -> Result<()> {
    let mut sessions = running_sessions()?;
    sessions.sort_by(|a, b| compare_ids(&a.alias, &b.alias));
    if sessions.is_empty() {
        println!(
            "No running harness-hat sessions. Sessions only exist while the manager is \
             running — launch one in the manager, then run `{} sh` from another terminal.",
            crate::cli::COMMAND_NAME
        );
        return Ok(());
    }

    let ws_width = sessions
        .iter()
        .map(|s| s.workspace.len())
        .chain(std::iter::once("WORKSPACE".len()))
        .max()
        .unwrap_or(0);
    let container_width = sessions
        .iter()
        .map(|s| s.container_id.len())
        .chain(std::iter::once("CONTAINER".len()))
        .max()
        .unwrap_or(0);
    let tpl_width = sessions
        .iter()
        .map(|s| s.template.len())
        .chain(std::iter::once("TEMPLATE".len()))
        .max()
        .unwrap_or(0);

    println!(
        "{:<8}{:<cid$}  {:<ws$}  {:<tpl$}",
        "SESSION",
        "CONTAINER",
        "WORKSPACE",
        "TEMPLATE",
        cid = container_width + 2,
        ws = ws_width + 2,
        tpl = tpl_width
    );
    for session in &sessions {
        println!(
            "{:<8}{:<cid$}  {:<ws$}  {:<tpl$}",
            session.alias,
            session.container_id,
            session.workspace,
            session.template,
            cid = container_width + 2,
            ws = ws_width + 2,
            tpl = tpl_width
        );
    }
    println!(
        "\nAttach with: {} sh <ID>\nCut network connections with: {} sh <ID> --kill-connections\nStop with: {} sh <ID> --kill",
        crate::cli::COMMAND_NAME,
        crate::cli::COMMAND_NAME,
        crate::cli::COMMAND_NAME
    );
    Ok(())
}

fn compare_ids(left: &str, right: &str) -> std::cmp::Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Session, attached_container_uri, compare_ids, docker_exec_args, env_value, hex_encode,
        host_control_url, normalize_id, parse_running_sessions, resolve_editor,
    };
    use crate::cli::OpenEditor;
    use std::ffi::OsString;

    #[test]
    fn hex_encode_matches_known_container_id() {
        assert_eq!(hex_encode(b"c84041fe7f9f"), "633834303431666537663966");
    }

    #[test]
    fn attached_container_uri_uses_hex_id_and_mount_target() {
        let session = Session {
            alias: "42".to_string(),
            container_id: "c84041fe7f9f".to_string(),
            workspace: "api".to_string(),
            template: "rust".to_string(),
            name: "hh-session".to_string(),
            mount_target: Some("/workspace".to_string()),
            session_token: "session-token".to_string(),
        };
        assert_eq!(
            attached_container_uri(&session),
            "vscode-remote://attached-container+633834303431666537663966/workspace"
        );
    }

    #[test]
    fn attached_container_uri_falls_back_to_workspace_root() {
        let session = Session {
            alias: "42".to_string(),
            container_id: "abc123".to_string(),
            workspace: "api".to_string(),
            template: "rust".to_string(),
            name: "hh-session".to_string(),
            mount_target: None,
            session_token: "session-token".to_string(),
        };
        assert_eq!(
            attached_container_uri(&session),
            "vscode-remote://attached-container+616263313233/workspace"
        );
    }

    #[test]
    fn numeric_ids_are_canonicalized_without_padding() {
        assert_eq!(normalize_id("42"), "42");
        assert_eq!(normalize_id("7"), "7");
        assert_eq!(normalize_id("0042"), "42");
    }

    #[test]
    fn non_numeric_or_long_ids_pass_through() {
        assert_eq!(normalize_id("12345"), "12345");
        assert_eq!(normalize_id("abcd"), "abcd");
        assert_eq!(normalize_id(""), "");
    }

    #[test]
    fn numeric_ids_sort_numerically() {
        assert!(compare_ids("2", "10").is_lt());
        assert!(compare_ids("10", "2").is_gt());
    }

    #[test]
    fn missing_editor_reports_path_error() {
        let editor = OpenEditor::new(OsString::from(
            "hat-editor-does-not-exist-for-path-resolution-test",
        ))
        .expect("editor name");
        let error = resolve_editor(&editor).expect_err("missing editor should fail");
        assert!(error.to_string().contains("not found on PATH"));
    }

    #[test]
    fn running_session_parser_includes_docker_container_id() {
        let sessions = parse_running_sessions(
            "a1b2c3d4e5f6\thh-session\tharness-hat.alias=0042,harness-hat.workspace=api,harness-hat.template=rust,harness-hat.session=secret\n",
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].alias, "42");
        assert_eq!(sessions[0].container_id, "a1b2c3d4e5f6");
        assert_eq!(sessions[0].workspace, "api");
        assert_eq!(sessions[0].template, "rust");
        assert_eq!(sessions[0].session_token, "secret");
    }

    #[test]
    fn session_control_metadata_helpers_parse_and_retarget_values() {
        let env = vec![
            "OTHER=value".to_string(),
            "HARNESS_HAT_TOKEN=secret".to_string(),
        ];
        assert_eq!(env_value(&env, "HARNESS_HAT_TOKEN"), Some("secret"));
        assert_eq!(env_value(&env, "MISSING"), None);
        assert_eq!(
            host_control_url("http://host.docker.internal:7878/").unwrap(),
            "http://127.0.0.1:7878"
        );
    }

    #[test]
    fn docker_exec_places_workdir_before_container_for_shell_and_command() {
        let shell_args =
            docker_exec_args("session", "/bin/bash", &[], Some("/work/src"), true, vec![]);
        assert_eq!(
            shell_args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
            vec![
                "exec",
                "-it",
                "-u",
                "1000:1000",
                "-e",
                "HOME=/home/coder",
                "--workdir",
                "/work/src",
                "session",
                "/bin/bash"
            ]
        );
        let command_args = docker_exec_args(
            "session",
            "/bin/bash",
            &[OsString::from("pwd")],
            Some("/work/src"),
            false,
            vec![],
        );
        assert_eq!(command_args[6], OsString::from("--workdir"));
        assert_eq!(command_args[8], OsString::from("session"));
        assert_eq!(command_args[9], OsString::from("pwd"));
    }
}

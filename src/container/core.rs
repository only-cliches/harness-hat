use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::Msg;
use alacritty_terminal::event_loop::Notifier;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use anyhow::Result;
/// Container session management.
///
/// Each running container gets a `ContainerSession` that owns a PTY process
/// (`docker run -it …`) and a `vt100::Parser` screen buffer updated in
/// real-time by a background reader thread.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use std::time::Instant;
use tempfile::NamedTempFile;

use crate::config::MountMode;
use crate::container::helpers::{blend_toward_bg, luma_u8, xterm_256_index_to_rgb};
use crate::fs_util::normalize_windows_extended_path;
use tracing::{instrument, warn};

const INPUT_ECHO_GRACE: Duration = Duration::from_millis(350);
/// Keep enough scrollback for normal terminal review without letting very noisy
/// commands monopolize PTY processing and rendering.
pub const TERMINAL_SCROLLBACK_LINES: usize = 10_000;

/// Docker label keys stamped on every harness-hat container so that other
/// processes (e.g. `hat sh`) can discover and identify running sessions
/// without depending on the manager being alive.
pub const LABEL_ALIAS: &str = "harness-hat.alias";
pub const LABEL_WORKSPACE: &str = "harness-hat.workspace";
pub const LABEL_TEMPLATE: &str = "harness-hat.template";
pub const LABEL_SESSION: &str = "harness-hat.session";
pub const LABEL_SHELL: &str = "harness-hat.shell";
/// Actual workspace mount target used for this session. This remains valid if
/// the template or rules change after the container was launched.
pub const LABEL_MOUNT_TARGET: &str = "harness-hat.mount-target";

/// Identity used when shelling into a running session.
pub const SHELL_USER: &str = "1000:1000";
pub const SHELL_HOME: &str = "/home/coder";

/// Build a [`WindowSize`] for a character grid. Cell pixel dimensions are left
/// at zero since the PTY is driven purely by row/column counts.
pub(crate) fn window_size(num_lines: u16, num_cols: u16) -> WindowSize {
    WindowSize {
        num_lines,
        num_cols,
        cell_width: 0,
        cell_height: 0,
    }
}

/// Extract a single label value from a docker `{{.Labels}}` line, which is a
/// comma-separated list of `key=value` pairs.
pub fn parse_docker_label(labels: &str, key: &str) -> Option<String> {
    labels.split(',').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// Live container session state and PTY plumbing for a launched container.
pub struct ContainerSession {
    pub container_name: String,
    pub container_id: String,
    pub docker_name: String,
    /// Monotonically increasing integer id used by `hat sh <alias>`.
    pub alias: String,
    pub workspace_name: String,
    pub session_token: String,
    pub mount_target: String,
    pub launched_at: Instant,
    /// Desktop-mode containers are monitored for SSH attachment so abandoned
    /// Claude sessions can be cleaned up after a reconnect grace period.
    pub desktop_mode: bool,
    pub desktop_ssh_ever_connected: bool,
    pub desktop_ssh_disconnected_at: Option<Instant>,
    pub(crate) last_desktop_ssh_check: Instant,
    /// Last time the manager reconciled this session against Docker. PTY exit
    /// events are normally immediate, but Windows can occasionally lose one.
    pub(crate) last_container_state_check: Instant,
    pub terminal_snapshot_hash: u64,
    pub terminal_changed_at: Instant,
    pub last_input_at: Arc<Mutex<Option<Instant>>>,
    pub term: Arc<FairMutex<Term<SessionEventProxy>>>,
    pub(crate) notifier: Notifier,
    pub(crate) window_size: Arc<Mutex<WindowSize>>,
    /// The container itself has stopped. This is deliberately distinct from a
    /// lost `docker run` PTY: Docker can keep the container running after the
    /// attach client goes away.
    pub exited: Arc<AtomicBool>,
    pub(crate) pty_exited: Arc<AtomicBool>,
    pub(crate) terminal_detached: Arc<AtomicBool>,
    pub has_bell: Arc<AtomicBool>,
    pub bell_count: Arc<AtomicU64>,
    pub exit_reported: bool,
    pub pty_exit_reported: bool,
    pub(crate) _scoped_proxy: Option<crate::proxy::ScopedProxyListener>,
    /// Private per-session copies of seeded mounts (e.g. `.claude.json`), kept
    /// alive for the container's lifetime and cleaned up on drop.
    pub(crate) _seed_tempfiles: Vec<NamedTempFile>,
    pub(crate) _env_tempfile: Option<NamedTempFile>,
    pub(crate) _control_tempfile: Option<NamedTempFile>,
    pub(crate) _hostdo_tempfile: Option<NamedTempFile>,
}

/// Event sink that keeps the Alacritty-backed PTY state synchronized with the
/// event loop and the UI.
#[derive(Clone)]
pub struct SessionEventProxy {
    pub(crate) sender: Arc<Mutex<Option<alacritty_terminal::event_loop::EventLoopSender>>>,
    pub(crate) window_size: Arc<Mutex<WindowSize>>,
    pub(crate) pty_exited: Arc<AtomicBool>,
    pub(crate) has_bell: Arc<AtomicBool>,
    pub(crate) bell_count: Arc<AtomicU64>,
    pub(crate) default_fg: alacritty_terminal::vte::ansi::Rgb,
    pub(crate) default_bg: alacritty_terminal::vte::ansi::Rgb,
    pub(crate) grayscale_palette: bool,
}

impl EventListener for SessionEventProxy {
    fn send_event(&self, event: Event) {
        let sender = self.sender.lock().ok().and_then(|s| s.clone());
        match event {
            Event::Bell => {
                self.has_bell.store(true, Ordering::Relaxed);
                self.bell_count.fetch_add(1, Ordering::Relaxed);
            }
            // This only says the local `docker run` client / PTY went away.
            // It does *not* prove Docker stopped the container; the TUI
            // reconciles it against `docker inspect` before marking a session
            // exited.
            Event::Exit | Event::ChildExit(_) => {
                self.pty_exited.store(true, Ordering::Relaxed);
            }
            Event::PtyWrite(s) => {
                if let Some(tx) = sender {
                    let _ = tx.send(Msg::Input(s.into_bytes().into()));
                }
            }
            Event::TextAreaSizeRequest(formatter) => {
                if let Some(tx) = sender {
                    let size = self
                        .window_size
                        .lock()
                        .map(|s| *s)
                        .unwrap_or_else(|_| window_size(24, 80));
                    let _ = tx.send(Msg::Input(formatter(size).into_bytes().into()));
                }
            }
            Event::ColorRequest(index, formatter) => {
                if let Some(tx) = sender {
                    let rgb = if index == 10 {
                        self.default_fg
                    } else if index == 11 {
                        self.default_bg
                    } else {
                        let (r, g, b) = xterm_256_index_to_rgb(index as u8);
                        let (r, g, b) = if self.grayscale_palette {
                            let y = luma_u8((r, g, b));
                            (y, y, y)
                        } else {
                            (r, g, b)
                        };
                        let blend_weight = if self.grayscale_palette { 0.45 } else { 0.35 };
                        let (r, g, b) = blend_toward_bg(
                            (r, g, b),
                            (self.default_bg.r, self.default_bg.g, self.default_bg.b),
                            blend_weight,
                        );
                        alacritty_terminal::vte::ansi::Rgb { r, g, b }
                    };
                    let _ = tx.send(Msg::Input(formatter(rgb).into_bytes().into()));
                }
            }
            Event::ClipboardLoad(_ty, formatter) => {
                if let Some(tx) = sender {
                    let _ = tx.send(Msg::Input(formatter("").into_bytes().into()));
                }
            }
            _ => {}
        }
    }
}

/// Helper struct to implement `alacritty_terminal::grid::Dimensions` for resizing the terminal view.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TermSize {
    pub(crate) cols: usize,
    pub(crate) lines: usize,
}

impl Dimensions for TermSize {
    fn columns(&self) -> usize {
        self.cols
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn last_column(&self) -> alacritty_terminal::index::Column {
        alacritty_terminal::index::Column(self.cols.saturating_sub(1))
    }
    fn topmost_line(&self) -> alacritty_terminal::index::Line {
        alacritty_terminal::index::Line(0)
    }
    fn bottommost_line(&self) -> alacritty_terminal::index::Line {
        alacritty_terminal::index::Line(self.lines.saturating_sub(1) as i32)
    }
    fn history_size(&self) -> usize {
        0
    }
}

impl ContainerSession {
    /// Checks if the container session has exited.
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Whether Docker still has this container running but the manager's
    /// original terminal attachment has gone away.
    pub fn is_terminal_detached(&self) -> bool {
        self.terminal_detached.load(Ordering::Relaxed)
    }

    pub(crate) fn pty_exited(&self) -> bool {
        self.pty_exited.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_terminal_detached(&self) {
        self.terminal_detached.store(true, Ordering::Relaxed);
    }
    /// Checks if the terminal has received a bell event.
    pub fn has_bell(&self) -> bool {
        self.has_bell.load(Ordering::Relaxed)
    }
    /// Monotonic count of terminal bell events emitted by this session.
    pub fn bell_count(&self) -> u64 {
        self.bell_count.load(Ordering::Relaxed)
    }
    /// Resets the bell status for the terminal.
    pub fn clear_bell(&self) {
        self.has_bell.store(false, Ordering::Relaxed);
    }
    /// Sends input bytes to the container's PTY, mimicking user input.
    pub fn send_input(&self, bytes: Vec<u8>) {
        if self.is_terminal_detached() {
            return;
        }
        if let Ok(mut last_input_at) = self.last_input_at.lock() {
            *last_input_at = Some(Instant::now());
        }
        self.notifier.notify(bytes);
    }

    pub fn display_name(&self) -> &str {
        self.container_name.as_str()
    }

    pub fn terminal_stable_for(&self) -> Duration {
        self.terminal_changed_at.elapsed()
    }

    pub fn refresh_terminal_snapshot(&mut self) -> bool {
        let hash = self.terminal_visible_hash();
        if hash != self.terminal_snapshot_hash {
            self.terminal_snapshot_hash = hash;
            if !self.recent_input_may_have_echoed() {
                self.terminal_changed_at = Instant::now();
            }
            true
        } else {
            false
        }
    }

    pub fn terminal_bottom_lines(&self, rows: usize) -> Vec<String> {
        let term = self.term.lock();
        terminal_bottom_lines(&*term, rows)
    }

    pub fn terminal_visible_rows(&self) -> usize {
        self.term.lock().screen_lines()
    }

    pub fn terminal_total_rows(&self) -> usize {
        self.term.lock().total_lines()
    }

    /// Friendly hint shown in the UI for shelling into this session.
    pub fn shell_in_hint(&self) -> String {
        format!("{} sh {}", crate::cli::COMMAND_NAME, self.alias)
    }

    /// Every useful `hat sh` form for this session. Keep this list next to the
    /// session identity so the TUI and reconnect messages use the same ID.
    pub fn shell_commands(&self) -> [String; 4] {
        let base = self.shell_in_hint();
        [
            format!("{base}  [attach]"),
            format!("{base} <COMMAND...>  [run a command]"),
            format!("{base} open EDITOR  [open a VS Code-compatible editor]"),
            format!("{base} --kill  [stop the session]"),
        ]
    }

    fn terminal_visible_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};

        let lines = {
            let term = self.term.lock();
            terminal_bottom_lines(&*term, term.screen_lines())
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        lines.hash(&mut hasher);
        hasher.finish()
    }

    fn recent_input_may_have_echoed(&self) -> bool {
        let last_input_at = self.last_input_at.lock().ok().and_then(|last| *last);
        recent_input_may_have_echoed(last_input_at, Instant::now())
    }

    /// Forcibly terminates the Docker container associated with this session.
    ///
    /// Uses `docker rm -f` to stop and remove the container. Non-zero exits and
    /// spawn failures are surfaced as `Err` and additionally logged at `warn!`
    /// level so an orphaned container is visible in operator logs.
    ///
    /// This is the blocking variant — `docker rm -f` can take several seconds
    /// (SIGTERM grace, then SIGKILL, then daemon-side cleanup), so callers on
    /// a UI/event-loop thread should prefer [`Self::terminate_in_background`].
    pub fn terminate(&self) -> anyhow::Result<()> {
        Self::docker_rm_force(&self.target_for_rm())
    }

    /// Spawn `docker rm -f` on a background thread and return immediately so
    /// the caller (e.g. the TUI event loop on a 'k' keypress) isn't blocked
    /// for the multi-second shutdown grace period. Failures are logged via
    /// `tracing::warn` — the caller has no way to surface them anyway because
    /// the session is typically removed from in-memory state right after.
    ///
    /// Safe at process-exit time too: once the `docker rm` subprocess is
    /// spawned, it survives our exit (reparented to init) and continues to
    /// talk to the docker daemon on its own, so we don't leak a container by
    /// not waiting.
    pub fn terminate_in_background(&self) {
        let target = self.target_for_rm();
        if target.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            let _ = Self::docker_rm_force(&target);
        });
    }

    /// Resolve the best identifier to pass to `docker rm -f`: prefer the
    /// stable container_id when known, fall back to the `--name` we chose.
    /// Returns an empty string for sessions that never recorded either (so
    /// callers can no-op).
    fn target_for_rm(&self) -> String {
        let candidate: &str = if !self.container_id.is_empty() {
            self.container_id.as_str()
        } else {
            self.docker_name.as_str()
        };
        if candidate.is_empty() || candidate == "unknown" {
            String::new()
        } else {
            candidate.to_string()
        }
    }

    fn docker_rm_force(target: &str) -> anyhow::Result<()> {
        if target.is_empty() {
            return Ok(());
        }
        let mut command = std::process::Command::new("docker");
        crate::process_util::hide_console_window(&mut command);
        let output = command
            .args(["rm", "-f", target])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(
                    target = %target,
                    status = ?out.status.code(),
                    stdout = %stdout.trim(),
                    stderr = %stderr.trim(),
                    "docker rm -f failed; container may be orphaned"
                );
                anyhow::bail!(
                    "docker rm -f {} exited with {:?}: {}",
                    target,
                    out.status.code(),
                    stderr.trim()
                );
            }
            Err(e) => {
                warn!(target = %target, error = %e, "failed to spawn docker rm");
                Err(anyhow::anyhow!(
                    "failed to spawn docker rm for {}: {}",
                    target,
                    e
                ))
            }
        }
    }

    /// Resizes the terminal window within the container.
    ///
    /// This updates the internal window size and notifies the PTY of the change,
    /// which helps the running process inside the container adjust its output.
    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        if let Ok(size) = self.window_size.lock() {
            if size.num_lines == rows && size.num_cols == cols {
                return Ok(());
            }
        }
        let ws = window_size(rows, cols);
        if let Ok(mut s) = self.window_size.lock() {
            *s = ws;
        }
        self.notifier.on_resize(ws);
        let mut term = self.term.lock();
        term.resize(TermSize {
            cols: cols as usize,
            lines: rows as usize,
        });
        Ok(())
    }

    /// Generates a human-readable label for the TUI tab, combining container and workspace names.
    pub fn tab_label(&self) -> String {
        format!("{} @ {}", self.display_name(), self.workspace_name)
    }
}

fn recent_input_may_have_echoed(last_input_at: Option<Instant>, now: Instant) -> bool {
    last_input_at.is_some_and(|last_input_at| now.duration_since(last_input_at) <= INPUT_ECHO_GRACE)
}

pub(crate) fn terminal_bottom_lines<T: EventListener>(term: &Term<T>, rows: usize) -> Vec<String> {
    let total_rows = term.total_lines();
    if total_rows == 0 || term.columns() == 0 {
        return Vec::new();
    }
    let rows = if rows == 0 {
        total_rows
    } else {
        rows.min(total_rows).max(1)
    };
    let cols = term.columns();
    let bottom = term.bottommost_line().0;
    let start = bottom - rows as i32 + 1;
    let grid = term.grid();

    (start..=bottom)
        .map(|line| {
            let mut out = String::with_capacity(cols);
            for col in 0..cols {
                let cell = &grid[Point::new(Line(line), Column(col))];
                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let ch = if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::HIDDEN)
                {
                    ' '
                } else {
                    cell.c
                };
                out.push(ch);
            }
            out.trim_end().to_string()
        })
        .collect()
}

/// Rewrites loopback addresses (`127.0.0.1`, `localhost`, `0.0.0.0`) to
/// `host.docker.internal` for reliable container-to-host communication.
///
/// Parses the URL with `url::Url` and rewrites only the **host** component —
/// the naive string `replace()` this superseded would clobber any path/query
/// segment containing `localhost` (e.g. `http://api/?host=localhost`). When
/// parsing fails we fall back to the original string unchanged so we don't
/// silently corrupt non-URL inputs.
#[instrument(level = "trace", skip(input))]
pub(crate) fn loopback_to_host_docker(input: &str) -> String {
    let Ok(parsed) = url::Url::parse(input) else {
        return input.to_string();
    };
    let host = match parsed.host_str() {
        Some(h @ ("localhost" | "127.0.0.1" | "0.0.0.0")) => h,
        _ => return input.to_string(),
    };
    // Replace only the host token in the original string, preserving the rest
    // (scheme, userinfo, port, path) byte-for-byte. We deliberately do NOT use
    // `parsed.set_host(...).to_string()`: `url::Url` normalizes an authority-only
    // URL like `http://127.0.0.1:7878` into `http://127.0.0.1:7878/`, adding a
    // trailing slash. The container's strict-mode init parses the port out of
    // `HARNESS_HAT_URL` with naive shell (`${hostport##*:}`), so that slash
    // becomes part of the port (`7878/`), iptables rejects it, and the
    // container exits 2. `url::Url` confirms `host` is the authority host, and
    // our userinfo (a hex token) never equals a loopback literal, so replacing
    // the first occurrence targets the authority.
    input.replacen(host, "host.docker.internal", 1)
}

/// Convert an arbitrary project or container name into a Docker-safe name.
#[instrument(level = "trace", skip(input))]
pub fn sanitize_docker_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "container".to_string()
    } else {
        out
    }
}

pub(crate) fn mount_mode_arg(mode: &MountMode) -> &'static str {
    match mode {
        MountMode::Ro => "ro",
        MountMode::Rw => "rw",
    }
}

pub(crate) fn docker_bind_mount_args(
    source: &str,
    target: &str,
    mode: &MountMode,
) -> Result<Vec<String>> {
    let source = normalize_windows_extended_path(source);
    validate_docker_mount_value(&source, "mount source")?;
    validate_docker_mount_value(target, "mount target")?;

    let mut spec = format!("type=bind,source={source},target={target}");
    if matches!(mode, MountMode::Ro) {
        spec.push_str(",readonly");
    }

    Ok(vec!["--mount".to_string(), spec])
}

fn validate_docker_mount_value(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "{label} must not be empty");
    anyhow::ensure!(
        !value.contains(','),
        "{label} contains ',' which cannot be represented safely in docker --mount: {value:?}"
    );
    anyhow::ensure!(
        !value.contains('\n') && !value.contains('\r') && !value.contains('\0'),
        "{label} contains a control character: {value:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::vte::ansi::Rgb;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    #[test]
    fn docker_bind_mount_strips_windows_extended_length_prefix() {
        use crate::config::MountMode;
        use crate::container::core::docker_bind_mount_args;

        assert_eq!(
            docker_bind_mount_args(r"\\?\C:\Users\example\repo", "/workspace", &MountMode::Rw,)
                .expect("mount args"),
            vec![
                "--mount".to_string(),
                r"type=bind,source=C:\Users\example\repo,target=/workspace".to_string(),
            ]
        );
        assert_eq!(
            docker_bind_mount_args(r"\\?\UNC\server\share\repo", "/workspace", &MountMode::Ro,)
                .expect("UNC mount args")[1],
            r"type=bind,source=\\server\share\repo,target=/workspace,readonly"
        );
    }

    #[test]
    fn loopback_to_host_docker_rewrites_host_without_adding_slash() {
        use crate::container::core::loopback_to_host_docker;
        // Regression: must NOT add a trailing slash. The container's strict-mode
        // init parses the port out of HARNESS_HAT_URL with naive shell, so a
        // normalized `http://host:7878/` would yield port `7878/` and break
        // iptables (container exits 2).
        assert_eq!(
            loopback_to_host_docker("http://127.0.0.1:7878"),
            "http://host.docker.internal:7878"
        );
        assert_eq!(
            loopback_to_host_docker("http://0.0.0.0:7878"),
            "http://host.docker.internal:7878"
        );
        assert_eq!(
            loopback_to_host_docker("http://localhost:28781"),
            "http://host.docker.internal:28781"
        );
        // Preserves userinfo and port (proxy URL shape).
        assert_eq!(
            loopback_to_host_docker("http://harness-hat:deadbeef@127.0.0.1:50419"),
            "http://harness-hat:deadbeef@host.docker.internal:50419"
        );
        // A genuine trailing slash / path is preserved as-is.
        assert_eq!(
            loopback_to_host_docker("http://127.0.0.1:7878/"),
            "http://host.docker.internal:7878/"
        );
        // Non-loopback hosts are untouched, even if a query mentions localhost.
        assert_eq!(
            loopback_to_host_docker("http://api.example.com/?h=localhost"),
            "http://api.example.com/?h=localhost"
        );
    }

    #[test]
    fn sanitize_docker_name_works() {
        use super::sanitize_docker_name;
        assert_eq!(sanitize_docker_name("my project"), "my-project");
        assert_eq!(sanitize_docker_name("my@proj!ect"), "my-proj-ect");
        assert_eq!(sanitize_docker_name(""), "container");
    }

    #[test]
    fn parse_docker_label_extracts_value() {
        let labels = "com.example=1,harness-hat.alias=42,harness-hat.workspace=my proj";
        assert_eq!(
            super::parse_docker_label(labels, "harness-hat.alias").as_deref(),
            Some("42")
        );
        assert_eq!(
            super::parse_docker_label(labels, "harness-hat.workspace").as_deref(),
            Some("my proj")
        );
        assert_eq!(super::parse_docker_label(labels, "missing"), None);
    }

    #[test]
    fn session_event_proxy_sets_bell_only_on_terminal_bell_event() {
        let has_bell = Arc::new(AtomicBool::new(false));
        let bell_count = Arc::new(AtomicU64::new(0));
        let proxy = super::SessionEventProxy {
            sender: Arc::new(Mutex::new(None)),
            window_size: Arc::new(Mutex::new(super::window_size(24, 80))),
            pty_exited: Arc::new(AtomicBool::new(false)),
            has_bell: has_bell.clone(),
            bell_count: bell_count.clone(),
            default_fg: Rgb { r: 0, g: 0, b: 0 },
            default_bg: Rgb { r: 0, g: 0, b: 0 },
            grayscale_palette: false,
        };

        assert!(!has_bell.load(Ordering::Relaxed));
        assert_eq!(bell_count.load(Ordering::Relaxed), 0);
        proxy.send_event(Event::Bell);
        assert!(has_bell.load(Ordering::Relaxed));
        assert_eq!(bell_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn session_event_proxy_records_pty_exit_separately_from_container_state() {
        let pty_exited = Arc::new(AtomicBool::new(false));
        let proxy = super::SessionEventProxy {
            sender: Arc::new(Mutex::new(None)),
            window_size: Arc::new(Mutex::new(super::window_size(24, 80))),
            pty_exited: pty_exited.clone(),
            has_bell: Arc::new(AtomicBool::new(false)),
            bell_count: Arc::new(AtomicU64::new(0)),
            default_fg: Rgb { r: 0, g: 0, b: 0 },
            default_bg: Rgb { r: 0, g: 0, b: 0 },
            grayscale_palette: false,
        };

        proxy.send_event(Event::Exit);

        assert!(pty_exited.load(Ordering::Relaxed));
    }

    #[derive(Clone)]
    struct NoopListener;

    impl EventListener for NoopListener {
        fn send_event(&self, _event: Event) {}
    }

    fn test_term(
        cols: usize,
        lines: usize,
        history: usize,
    ) -> alacritty_terminal::term::Term<NoopListener> {
        let mut term_cfg = alacritty_terminal::term::Config::default();
        term_cfg.scrolling_history = history;
        alacritty_terminal::term::Term::new(
            term_cfg,
            &super::TermSize { cols, lines },
            NoopListener,
        )
    }

    fn write_to_term(term: &mut alacritty_terminal::term::Term<NoopListener>, bytes: &[u8]) {
        let mut parser = alacritty_terminal::vte::ansi::Processor::<
            alacritty_terminal::vte::ansi::StdSyncHandler,
        >::new();
        parser.advance(term, bytes);
    }

    #[test]
    fn terminal_bottom_lines_can_include_scrollback() {
        let mut term = test_term(12, 3, 10);
        write_to_term(&mut term, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        assert_eq!(
            super::terminal_bottom_lines(&term, 5),
            vec!["one", "two", "three", "four", "five"]
        );
    }

    #[test]
    fn terminal_bottom_lines_zero_requests_available_buffer() {
        let mut term = test_term(12, 3, 10);
        write_to_term(&mut term, b"one\r\ntwo\r\nthree\r\nfour");

        assert_eq!(
            super::terminal_bottom_lines(&term, 0),
            vec!["one", "two", "three", "four"]
        );
    }

    #[test]
    fn recent_input_echo_window_is_short_lived() {
        let now = std::time::Instant::now();

        assert!(super::recent_input_may_have_echoed(Some(now), now));
        assert!(super::recent_input_may_have_echoed(
            Some(now - super::INPUT_ECHO_GRACE),
            now,
        ));
        assert!(!super::recent_input_may_have_echoed(
            Some(now - super::INPUT_ECHO_GRACE - std::time::Duration::from_millis(1)),
            now,
        ));
        assert!(!super::recent_input_may_have_echoed(None, now));
    }
}

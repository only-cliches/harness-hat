use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64},
};
use std::time::Instant;
use tempfile::NamedTempFile;
use tracing::{debug, info, instrument, warn};

use crate::config::{ContainerDef, MountMode, container_path_file_name, container_path_string};
use crate::container::core::{
    LABEL_ALIAS, LABEL_MOUNT_TARGET, LABEL_SESSION, LABEL_SHELL, LABEL_TEMPLATE, LABEL_WORKSPACE,
    TERMINAL_SCROLLBACK_LINES, TermSize, docker_bind_mount_args, loopback_to_host_docker,
    parse_docker_label, sanitize_docker_name, terminal_bottom_lines,
};
use crate::container::helpers::detect_default_colors;
use crate::container::{ContainerSession, SessionEventProxy, read_container_id};
use crate::fs_util::{is_valid_env_name, write_env_file_entry};

const PRIMARY_PROXY_CONN_LIMIT: usize = 0;
const CODER_HOME: &str = "/home/coder";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
const CLAUDE_CODE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const CLAUDE_CODE_OAUTH_SCOPES_ENV: &str = "CLAUDE_CODE_OAUTH_SCOPES";
const CLAUDE_CODE_HOST_AUTH_ENV_VAR_ENV: &str = "CLAUDE_CODE_HOST_AUTH_ENV_VAR";
const CLAUDE_CODE_DEFAULT_OAUTH_SCOPES: &str = "user:inference";
static SESSION_ALIAS_LOCK: Mutex<()> = Mutex::new(());
static LAST_SESSION_ALIAS: AtomicU64 = AtomicU64::new(0);
const CLAUDE_CONFIG_CONTAINER_PATH: &str = "/home/coder/.claude.json";
const CODEX_HOME_CONTAINER_PATH: &str = "/home/coder/.codex";
const CODEX_SEED_CONTAINER_PATH: &str = "/run/harness-hat/codex-seed";
const CODEX_SEED_ENV: &str = "HARNESS_HAT_CODEX_SEED";
const DESKTOP_SSH_PORT: &str = "2222";

/// Return a Linux-container mount target that mirrors the host workspace path
/// as closely as possible. Absolute POSIX paths are preserved, while Windows
/// drive paths such as `C:\Users\me\repo` become `/C/Users/me/repo`.
pub(crate) fn mirrored_workspace_mount_target(workspace_path: &Path) -> Option<std::path::PathBuf> {
    let host_path = crate::fs_util::normalize_windows_extended_path(
        &workspace_path.as_os_str().to_string_lossy(),
    );
    let target = host_path.replace('\\', "/");

    let bytes = target.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        let suffix = target[3..].trim_start_matches('/');
        let target = if suffix.is_empty() {
            format!("/{drive}")
        } else {
            format!("/{drive}/{suffix}")
        };
        return Some(std::path::PathBuf::from(target));
    }

    (target.starts_with('/') && !target.starts_with("//")).then(|| std::path::PathBuf::from(target))
}

/// Launch `docker run` for a container definition and wire it to a PTY-backed
/// terminal session.
#[instrument(skip(
    ctr,
    command_argv,
    workspace_path,
    session_token,
    token,
    proxy_url,
    extra_env,
    scoped_proxy
))]
pub fn spawn(
    ctr: &ContainerDef,
    command_argv: Option<&[String]>,
    project_name: &str,
    workspace_path: &Path,
    session_token: &str,
    token: &str,
    control_url: &str,
    proxy_url: &str,
    hostdo_script_host_path: Option<&Path>,
    scoped_proxy: Option<crate::proxy::ScopedProxyListener>,
    proxy_priority: crate::proxy::SourcePriority,
    strict_network: bool,
    extra_env: &[(String, String)],
    rows: u16,
    cols: u16,
) -> Result<(ContainerSession, Vec<String>)> {
    let mount_str = container_path_string(&ctr.mount_target);

    let cidfile =
        std::env::temp_dir().join(format!("harness-hat-cid-{}.txt", uuid::Uuid::new_v4()));
    let docker_run_name = format!(
        "harness-hat-{}-{}",
        sanitize_docker_name(&ctr.name),
        uuid::Uuid::new_v4().simple()
    );
    let alias = allocate_session_alias()?;

    let container_control_url = loopback_to_host_docker(control_url);
    let container_proxy_url = loopback_to_host_docker(proxy_url);
    let container_proxy_addr = proxy_addr_without_auth(&container_proxy_url);
    let scoped_proxy_auth = scoped_proxy
        .as_ref()
        .map(|proxy| proxy.proxy_auth_token().to_string())
        .unwrap_or_default();
    let mut launch_notes = Vec::new();
    let desktop_mode = extra_env.iter().any(|(name, value)| {
        name == crate::desktop::AUTHORIZED_KEY_ENV && !value.trim().is_empty()
    });

    let mut docker_args: Vec<String> = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        docker_run_name.clone(),
        "--cidfile".to_string(),
        cidfile.display().to_string(),
    ];

    // Discovery labels so `hat sh` can find and identify this session
    // without the manager process being alive.
    //
    // `docker ps --format '{{.Labels}}'` serializes labels as a single
    // comma-separated `k=v,k=v` string, which means a label value containing
    // `,` round-trips back as two truncated entries when re-parsed by
    // `parse_docker_label`. We sanitize commas at write time so any
    // workspace/template name with a `,` survives the round trip.
    let label_alias = sanitize_label_value(alias.as_str());
    let label_workspace = sanitize_label_value(project_name);
    let label_template = sanitize_label_value(ctr.name.as_str());
    let label_session = sanitize_label_value(session_token);
    let label_shell = sanitize_label_value(ctr.attach_shell.as_deref().unwrap_or("/bin/bash"));
    let label_mount_target = sanitize_label_value(&mount_str);
    for (key, value) in [
        (LABEL_ALIAS, label_alias.as_str()),
        (LABEL_WORKSPACE, label_workspace.as_str()),
        (LABEL_TEMPLATE, label_template.as_str()),
        (LABEL_SESSION, label_session.as_str()),
        (LABEL_SHELL, label_shell.as_str()),
        (LABEL_MOUNT_TARGET, label_mount_target.as_str()),
    ] {
        docker_args.push("--label".to_string());
        docker_args.push(format!("{key}={value}"));
    }
    if desktop_mode {
        docker_args.push("--label".to_string());
        docker_args.push(format!("{}=true", crate::desktop::LABEL_DESKTOP));
        docker_args.extend_from_slice(&[
            "--publish".to_string(),
            format!("127.0.0.1::{DESKTOP_SSH_PORT}"),
        ]);
    }

    #[cfg(target_os = "linux")]
    docker_args.push("--add-host=host.docker.internal:host-gateway".to_string());

    // Block privilege escalation via setuid binaries / file capabilities. This
    // does not impede the init's downward `gosu` drop from root to uid 1000
    // (dropping privileges is always allowed), it only prevents *gaining* them
    // (M1).
    docker_args.extend_from_slice(&[
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
    ]);

    #[cfg(target_os = "linux")]
    if !strict_network {
        // Non-strict containers run unprivileged from the start and need no
        // Linux capabilities at all — drop the ~14 Docker grants by default (M1).
        docker_args.extend_from_slice(&["--cap-drop".to_string(), "ALL".to_string()]);
        docker_args.extend_from_slice(&["--user".to_string(), "1000:1000".to_string()]);
    }

    if strict_network {
        docker_args.extend_from_slice(&["--user".to_string(), "0:0".to_string()]);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            // Docker Desktop exposes `/dev/net/tun` for strict mode only when the
            // container is privileged.
            docker_args.push("--privileged".to_string());
        }

        #[cfg(target_os = "linux")]
        {
            // Strict mode runs as root only for the init script's setup phase.
            // Drop every capability and re-add the empirically-verified minimal
            // set (M1): NET_ADMIN for the iptables egress chain and tun0
            // creation/routing (tun2proxy --setup), SETUID + SETGID for the
            // downward `gosu 1000:1000` drop. Each was confirmed necessary by
            // removing it and watching the corresponding init step fail.
            docker_args.extend_from_slice(&["--cap-drop".to_string(), "ALL".to_string()]);
            for cap in ["NET_ADMIN", "SETUID", "SETGID"] {
                docker_args.extend_from_slice(&["--cap-add".to_string(), cap.to_string()]);
            }
            if Path::new("/dev/net/tun").exists() {
                docker_args.extend_from_slice(&[
                    "--device".to_string(),
                    "/dev/net/tun:/dev/net/tun".to_string(),
                ]);
            } else {
                anyhow::bail!(
                    "Strict network mode requires /dev/net/tun on the host. Cannot safely fallback to --privileged."
                );
            }
        }
    }

    push_opt_flag(&mut docker_args, "--memory", ctr.memory.as_deref());
    push_opt_flag(&mut docker_args, "--cpus", ctr.cpus.as_deref());
    push_opt_flag(&mut docker_args, "--shm-size", ctr.shm_size.as_deref());

    docker_args.extend(docker_bind_mount_args(
        &workspace_path.display().to_string(),
        &mount_str,
        &MountMode::Rw,
    )?);
    docker_args.extend_from_slice(&["-w".to_string(), mount_str.clone()]);

    let hostdo_tempfile = match hostdo_script_host_path {
        Some(path) => Some(prepare_executable_helper_script(
            path,
            "harness-hat-hostdo-",
        )?),
        None => None,
    };
    if let Some(hostdo) = hostdo_tempfile.as_ref() {
        docker_args.extend(docker_bind_mount_args(
            &hostdo.path().display().to_string(),
            "/usr/local/bin/hostdo",
            &MountMode::Ro,
        )?);
    }

    // Prepare secure env file to prevent token leakage via `ps`
    let mut env_file = tempfile::Builder::new()
        .prefix("harness-hat-env-")
        .tempfile()
        .context("failed to create temp env file")?;

    for (key, value) in &ctr.env {
        write_env_file_entry(&mut env_file, key, value)?;
    }
    for (key, value) in extra_env {
        write_env_file_entry(&mut env_file, key, value)?;
    }
    if !ctr.env.contains_key("BUILDKIT_PROGRESS")
        && !extra_env.iter().any(|(key, _)| key == "BUILDKIT_PROGRESS")
    {
        write_env_file_entry(&mut env_file, "BUILDKIT_PROGRESS", "plain")?;
    }
    if should_inject_coder_home(&ctr.mounts)
        && !ctr.env.contains_key("HOME")
        && !extra_env.iter().any(|(key, _)| key == "HOME")
    {
        write_env_file_entry(&mut env_file, "HOME", CODER_HOME)?;
    }
    if !ctr.env.contains_key("PATH") && !extra_env.iter().any(|(key, _)| key == "PATH") {
        if let Some(path) = compute_path_override(&ctr.image, &ctr.mounts)? {
            write_env_file_entry(&mut env_file, "PATH", &path)?;
        }
    }

    write_env_file_entry(&mut env_file, "HARNESS_HAT_TOKEN", token)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_SESSION_TOKEN", session_token)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_WORKSPACE", project_name)?;
    // Compatibility for older in-container integrations.
    write_env_file_entry(&mut env_file, "HARNESS_HAT_PROJECT", project_name)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_MOUNT_TARGET", &mount_str)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_URL", &container_control_url)?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_STRICT_NETWORK",
        if strict_network { "1" } else { "0" },
    )?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_SCOPED_PROXY_ADDR",
        &container_proxy_addr,
    )?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_SCOPED_PROXY_AUTH",
        &scoped_proxy_auth,
    )?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_PROXY_CONN_LIMIT",
        PRIMARY_PROXY_CONN_LIMIT.to_string(),
    )?;
    if let Some(forwards) = format_localhost_forwards(&ctr.localhost_forwards) {
        write_env_file_entry(&mut env_file, "HARNESS_HAT_LOCALHOST_FORWARDS", forwards)?;
    }
    if !strict_network {
        // Both upper- and lowercase forms, since runtimes disagree on which they read.
        for (key, value) in [
            ("HTTP_PROXY", &container_proxy_url),
            ("HTTPS_PROXY", &container_proxy_url),
            ("ALL_PROXY", &container_proxy_url),
            ("http_proxy", &container_proxy_url),
            ("https_proxy", &container_proxy_url),
            ("all_proxy", &container_proxy_url),
        ] {
            write_env_file_entry(&mut env_file, key, value)?;
        }
    }

    let host_env = |name: &str| std::env::var(name).ok();
    let explicit_claude_oauth_token_available = has_nonempty_env_source(
        CLAUDE_CODE_OAUTH_TOKEN_ENV,
        &ctr.env,
        extra_env,
        &ctr.env_passthrough,
        &host_env,
    );
    let explicit_claude_auth_available = explicit_claude_oauth_token_available
        || has_nonempty_env_source(
            ANTHROPIC_API_KEY_ENV,
            &ctr.env,
            extra_env,
            &ctr.env_passthrough,
            &host_env,
        )
        || has_nonempty_env_source(
            ANTHROPIC_AUTH_TOKEN_ENV,
            &ctr.env,
            extra_env,
            &ctr.env_passthrough,
            &host_env,
        );

    // As a convenience fallback on macOS, inject Claude Code OAuth credentials
    // from the Keychain so Claude can authenticate inside the Linux container
    // without a Keychain of its own. Skip this when the user has supplied
    // explicit auth env vars: setup-token/API-key auth is better for
    // long-running sessions, while the Keychain path only gives the container
    // the current short-lived access token.
    //
    // CLAUDE_CODE_OAUTH_TOKEN   — access token; used by both `-p` and TUI API calls.
    // CLAUDE_CODE_OAUTH_SCOPES  — required by `shouldUseClaudeAIAuth` (TUI auth path).
    // CLAUDE_CODE_HOST_AUTH_ENV_VAR — tells Claude that auth is provided by the host
    //   via the named env var, disabling Keychain / libsecret lookups that would
    //   fail inside a headless Linux container.
    //
    // Refresh token is intentionally NOT injected: the container cannot write
    // a refreshed token back to the macOS Keychain, so allowing refresh inside
    // the container rotates the host's refresh token (invalidating it) while the
    // new token lives only in the ephemeral container. Keychain-backed sessions
    // are therefore bounded by the access token lifetime; use
    // CLAUDE_CODE_OAUTH_TOKEN for long-running sessions.
    let mut injected_claude_oauth_token = false;
    let mut injected_claude_oauth_scopes = false;
    let mut injected_claude_host_auth_env_var = false;
    if !explicit_claude_auth_available {
        if let Some(creds) = read_claude_oauth_credentials_full() {
            write_env_file_entry(
                &mut env_file,
                CLAUDE_CODE_OAUTH_TOKEN_ENV,
                &creds.access_token,
            )?;
            injected_claude_oauth_token = true;
            write_env_file_entry(&mut env_file, CLAUDE_CODE_OAUTH_SCOPES_ENV, &creds.scopes)?;
            injected_claude_oauth_scopes = true;
            write_env_file_entry(
                &mut env_file,
                CLAUDE_CODE_HOST_AUTH_ENV_VAR_ENV,
                CLAUDE_CODE_OAUTH_TOKEN_ENV,
            )?;
            injected_claude_host_auth_env_var = true;
        }
    }

    // If the OAuth token is supplied by normal env passthrough instead of the
    // macOS Keychain path above, Claude still needs the host-auth marker for
    // interactive container sessions. Supply the minimum scope for setup-token
    // tokens unless the user already provided explicit scopes.
    let claude_oauth_token_available =
        injected_claude_oauth_token || explicit_claude_oauth_token_available;
    if claude_oauth_token_available {
        if !injected_claude_host_auth_env_var
            && !has_nonempty_env_source(
                CLAUDE_CODE_HOST_AUTH_ENV_VAR_ENV,
                &ctr.env,
                extra_env,
                &ctr.env_passthrough,
                &host_env,
            )
        {
            write_env_file_entry(
                &mut env_file,
                CLAUDE_CODE_HOST_AUTH_ENV_VAR_ENV,
                CLAUDE_CODE_OAUTH_TOKEN_ENV,
            )?;
        }
        if !injected_claude_oauth_scopes
            && !has_nonempty_env_source(
                CLAUDE_CODE_OAUTH_SCOPES_ENV,
                &ctr.env,
                extra_env,
                &ctr.env_passthrough,
                &host_env,
            )
        {
            write_env_file_entry(
                &mut env_file,
                CLAUDE_CODE_OAUTH_SCOPES_ENV,
                CLAUDE_CODE_DEFAULT_OAUTH_SCOPES,
            )?;
        }
    }

    docker_args.push("--env-file".to_string());
    docker_args.push(env_file.path().display().to_string());

    // `.claude.json` is rewritten in place by every Claude Code instance
    // (numStartups, project trust, etc.). Bind-mounting the host's live file
    // means the host's own Claude and the container's Claude race on the same
    // file, tearing it into `JSON Parse error: Unterminated string` corruption
    // and resetting the host config (losing the OAuth account → the container
    // appears logged out). Seeded mounts (see `ContainerMount::is_seeded`) get a private
    // per-session copy instead: the container reads the session through, then
    // owns the file privately. The handles live for the container's lifetime.
    //
    // Mounts are sorted by container path depth (shortest first) so that
    // directory mounts are applied before any file mounts nested inside them.
    // If a claude_settings source is configured, inject a seeded mount for the
    // container's settings.json so it gets a private copy without touching the
    // host file. This is built as an owned vec so it survives the borrow below.
    let claude_settings_mount: Option<crate::config::ContainerMount> = ctr
        .claude_settings
        .as_ref()
        .map(|src| crate::config::ContainerMount {
            host: src.clone(),
            container: std::path::PathBuf::from("/home/coder/.claude/settings.json"),
            mode: crate::config::MountMode::Rw,
            seed: Some(true),
            add_to_path: false,
        });
    let mut all_mounts: Vec<crate::config::ContainerMount> = ctr.mounts.clone();
    if let Some(m) = claude_settings_mount {
        all_mounts.push(m);
    }

    // Docker applies bind mounts in order, so a file mount on e.g.
    // `/home/coder/.claude/.claude.json` must come *after* the directory mount
    // on `/home/coder/.claude` — otherwise the directory mount overwrites it.
    let mut sorted_mounts: Vec<&crate::config::ContainerMount> = all_mounts.iter().collect();
    sorted_mounts.sort_by_key(|m| {
        container_path_string(&m.container)
            .split('/')
            .filter(|part| !part.is_empty())
            .count()
    });
    let mut seed_tempfiles: Vec<NamedTempFile> = Vec::new();
    if desktop_mode {
        let policy = prepare_desktop_policy()?;
        docker_args.extend(docker_bind_mount_args(
            &policy.path().display().to_string(),
            crate::desktop::MANAGED_POLICY_CONTAINER_PATH,
            &MountMode::Ro,
        )?);
        seed_tempfiles.push(policy);
        launch_notes.push(
            "Claude Desktop safety policy mounted read-only; host browser, Chrome, connectors, and computer-use tools are disabled"
                .to_string(),
        );
    }
    let has_top_level_claude_config_mount = sorted_mounts
        .iter()
        .any(|mount| container_path_string(&mount.container) == CLAUDE_CONFIG_CONTAINER_PATH);
    for mount in &sorted_mounts {
        if should_seed_codex_directory(mount) {
            if !mount.host.exists() {
                warn!(
                    host = %mount.host.display(),
                    container = CODEX_HOME_CONTAINER_PATH,
                    "skipping Codex state seed because host source does not exist"
                );
                continue;
            }

            // SQLite locking is not reliable when a Linux container opens its
            // databases through Docker Desktop's Windows bind filesystem. It is
            // also unsafe for the host and container Codex processes to share
            // the same live databases concurrently. Give Codex a container-local
            // anonymous volume and expose the host state read-only at a staging
            // path; the init script copies only portable auth/config/plugin state
            // into the volume before dropping to the coder user.
            docker_args.extend_from_slice(&[
                "--mount".to_string(),
                format!("type=volume,target={CODEX_HOME_CONTAINER_PATH}"),
            ]);
            docker_args.extend(docker_bind_mount_args(
                &mount.host.display().to_string(),
                CODEX_SEED_CONTAINER_PATH,
                &MountMode::Ro,
            )?);
            write_env_file_entry(&mut env_file, CODEX_SEED_ENV, CODEX_SEED_CONTAINER_PATH)?;
            launch_notes.push(
                "Codex state is copied into container-local storage on Windows so its SQLite databases can start safely"
                    .to_string(),
            );
            continue;
        }

        let seeded = seed_private_mount(mount, claude_oauth_token_available)?;
        if seeded.is_none() && !mount.host.exists() {
            warn!(
                host = %mount.host.display(),
                container = %container_path_string(&mount.container),
                "skipping bind mount because host source does not exist"
            );
            continue;
        }
        let host_arg = match &seeded {
            Some(tempfile) => tempfile.path().display().to_string(),
            None => mount.host.display().to_string(),
        };
        docker_args.extend(mount_bind_args(&host_arg, mount, seeded.is_some())?);

        if let Some(tempfile) = seeded {
            seed_tempfiles.push(tempfile);
        }
    }
    if claude_oauth_token_available && !has_top_level_claude_config_mount {
        let tempfile = seed_claude_oauth_onboarding_config()?;
        let host_arg = tempfile.path().display().to_string();
        seed_tempfiles.push(tempfile);
        docker_args.extend(docker_bind_mount_args(
            &host_arg,
            CLAUDE_CONFIG_CONTAINER_PATH,
            &MountMode::Rw,
        )?);
    }

    for name in &ctr.env_passthrough {
        if ctr.env.contains_key(name) || extra_env.iter().any(|(key, _)| key == name) {
            continue;
        }
        // POSIX env-name validation: leading [A-Za-z_], then [A-Za-z0-9_]*.
        // Without this, an attacker-influenced config could pass `-e
        // FOO=BAR\nINJECT=evil` and shadow other vars (or, on some shells,
        // poison the container environment with metacharacters).
        if !is_valid_env_name(name) {
            anyhow::bail!("env_passthrough entry {name:?} is not a valid POSIX env variable name");
        }
        debug!(name = %name, "passing env var through from host to container");
        docker_args.push("-e".to_string());
        docker_args.push(name.to_string());
    }

    env_file.flush().context("flushing container env file")?;

    docker_args.push(ctr.image.clone());
    if let Some(argv) = command_argv {
        docker_args.extend(argv.iter().cloned());
    }

    info!(
        "launching container: docker {}",
        docker_args
            .iter()
            .map(|a| if a.contains(' ') || a.contains('=') {
                format!("'{a}'")
            } else {
                a.clone()
            })
            .collect::<Vec<_>>()
            .join(" ")
    );

    let (fg, bg) = detect_default_colors();
    let default_fg = alacritty_terminal::vte::ansi::Rgb {
        r: fg.0,
        g: fg.1,
        b: fg.2,
    };
    let default_bg = alacritty_terminal::vte::ansi::Rgb {
        r: bg.0,
        g: bg.1,
        b: bg.2,
    };

    let window_size = crate::container::core::window_size(rows, cols);
    let window_size_arc = Arc::new(Mutex::new(window_size));

    let exited = Arc::new(AtomicBool::new(false));
    let pty_exited = Arc::new(AtomicBool::new(false));
    let terminal_detached = Arc::new(AtomicBool::new(false));
    let has_bell = Arc::new(AtomicBool::new(false));
    let bell_count = Arc::new(AtomicU64::new(0));

    let proxy = SessionEventProxy {
        sender: Arc::new(Mutex::new(None)),
        window_size: Arc::clone(&window_size_arc),
        pty_exited: Arc::clone(&pty_exited),
        has_bell: Arc::clone(&has_bell),
        bell_count: Arc::clone(&bell_count),
        default_fg,
        default_bg,
        grayscale_palette: ctr.grayscale_palette,
    };

    let mut term_cfg = TermConfig::default();
    term_cfg.scrolling_history = TERMINAL_SCROLLBACK_LINES;
    let term_size = TermSize {
        cols: cols as usize,
        lines: rows as usize,
    };
    let term = Arc::new(FairMutex::new(Term::new(
        term_cfg,
        &term_size,
        proxy.clone(),
    )));

    let options = docker_pty_options(docker_args);

    let pty = tty::new(&options, window_size, 0).context("open PTY")?;
    let event_loop = EventLoop::new(Arc::clone(&term), proxy.clone(), pty, false, false)
        .context("event loop")?;
    let sender = event_loop.channel();
    let notifier = Notifier(sender.clone());
    if let Ok(mut s) = proxy.sender.lock() {
        *s = Some(sender);
    }
    let _handle = event_loop.spawn();

    let container_id = match read_container_id(&cidfile, &docker_run_name, &pty_exited) {
        Ok(id) => id,
        Err(error) => {
            let docker_output = {
                let term = term.lock();
                terminal_bottom_lines(&*term, 8)
                    .into_iter()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ")
            };
            let _ = std::fs::remove_file(&cidfile);
            if docker_output.is_empty() {
                return Err(error).context("reading docker container id");
            }
            return Err(error).context(format!("docker run failed: {docker_output}"));
        }
    };
    let docker_name = docker_run_name.clone();
    let _ = std::fs::remove_file(&cidfile);

    Ok((
        ContainerSession {
            container_name: ctr.name.clone(),
            container_id,
            docker_name,
            alias,
            workspace_name: project_name.to_owned(),
            session_token: session_token.to_string(),
            mount_target: mount_str,
            launched_at: Instant::now(),
            desktop_mode,
            desktop_ssh_ever_connected: false,
            desktop_ssh_disconnected_at: None,
            last_desktop_ssh_check: Instant::now(),
            last_container_state_check: Instant::now(),
            terminal_snapshot_hash: 0,
            terminal_changed_at: Instant::now(),
            last_input_at: Arc::new(Mutex::new(None)),
            term,
            notifier,
            window_size: window_size_arc,
            exited,
            pty_exited,
            terminal_detached,
            has_bell,
            bell_count,
            exit_reported: false,
            pty_exit_reported: false,
            _scoped_proxy: scoped_proxy,
            _seed_tempfiles: seed_tempfiles,
            _env_tempfile: Some(env_file),
            _control_tempfile: None,
            _hostdo_tempfile: hostdo_tempfile,
        },
        launch_notes,
    ))
}

fn docker_pty_options(docker_args: Vec<String>) -> tty::Options {
    let mut options = tty::Options::default();
    options.shell = Some(tty::Shell::new("docker".to_string(), docker_args));
    options.working_directory = None;
    options.drain_on_exit = false;
    options.env = HashMap::new();

    #[cfg(target_os = "windows")]
    {
        // alacritty_terminal passes the program and argv to CreateProcessW as
        // one command-line string. Its default leaves arguments unescaped,
        // which splits Windows workspace and tempfile paths containing spaces
        // before docker.exe receives them.
        options.escape_args = true;
    }

    options
}

fn prepare_desktop_policy() -> Result<NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("harness-hat-claude-desktop-policy-")
        .tempfile()
        .context("creating Claude Desktop managed policy")?;
    file.write_all(crate::desktop::MANAGED_POLICY.as_bytes())
        .context("writing Claude Desktop managed policy")?;
    file.flush()
        .context("flushing Claude Desktop managed policy")?;
    Ok(file)
}

/// Pick the next integer id after both the largest numeric id currently used by
/// a running container and the last id allocated by this manager process. The
/// lock prevents concurrent launches in the same process from selecting the
/// same id.
fn allocate_session_alias() -> Result<String> {
    let _guard = SESSION_ALIAS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("session ID allocator lock is poisoned"))?;
    let used = running_session_aliases()?;
    let previous = LAST_SESSION_ALIAS.load(std::sync::atomic::Ordering::Relaxed);
    let next = next_session_alias(&used, previous)?;
    LAST_SESSION_ALIAS.store(next, std::sync::atomic::Ordering::Relaxed);
    Ok(next.to_string())
}

fn next_session_alias(used: &std::collections::HashSet<String>, previous: u64) -> Result<u64> {
    let largest = used
        .iter()
        .filter_map(|alias| alias.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    largest
        .max(previous)
        .checked_add(1)
        .context("session ID space exhausted")
}

/// List `harness-hat.alias` values currently in use by running containers.
///
/// Returns `Err` when `docker ps` fails — silently returning an empty set
/// would let `allocate_session_alias` happily mint an alias that collides with
/// a live container, breaking `hat sh <alias>` (ambiguous lookup).
fn running_session_aliases() -> Result<std::collections::HashSet<String>> {
    let mut command = std::process::Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args([
            "ps",
            "--filter",
            &format!("label={LABEL_ALIAS}"),
            "--format",
            "{{.Labels}}",
        ])
        .stderr(std::process::Stdio::piped())
        .output()
        .context("running docker ps to enumerate session aliases")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            status = ?output.status.code(),
            stderr = %stderr.trim(),
            "docker ps failed while enumerating session aliases"
        );
        anyhow::bail!(
            "docker ps exited with {:?}: {}",
            output.status.code(),
            stderr.trim()
        );
    }
    let mut set = std::collections::HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(alias) = parse_docker_label(line, LABEL_ALIAS) {
            set.insert(alias);
        }
    }
    Ok(set)
}

fn prepare_executable_helper_script(path: &Path, prefix: &str) -> Result<NamedTempFile> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading helper script '{}'", path.display()))?;
    let contents = normalize_script_line_endings(&contents);
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile()
        .context("creating helper script temp file")?;
    file.write_all(contents.as_bytes())
        .context("writing helper script temp file")?;
    file.flush().context("flushing helper script temp file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file
            .as_file()
            .metadata()
            .context("reading helper script temp file metadata")?
            .permissions();
        perms.set_mode(0o755);
        file.as_file()
            .set_permissions(perms)
            .context("marking helper script temp file executable")?;
    }

    Ok(file)
}

fn normalize_script_line_endings(contents: &str) -> String {
    contents.replace("\r\n", "\n")
}

fn proxy_addr_without_auth(proxy_url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(proxy_url)
        && let Some(host) = parsed.host_str()
    {
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let port = parsed.port_or_known_default().unwrap_or(80);
        return format!("{host}:{port}");
    }

    let authority = proxy_url
        .strip_prefix("http://")
        .or_else(|| proxy_url.strip_prefix("https://"))
        .unwrap_or(proxy_url);
    authority
        .rsplit_once('@')
        .map(|(_, addr)| addr)
        .unwrap_or(authority)
        .to_string()
}

fn format_localhost_forwards(forwards: &[crate::config::LocalhostForward]) -> Option<String> {
    if forwards.is_empty() {
        return None;
    }
    Some(
        forwards
            .iter()
            .map(|forward| {
                format!(
                    "{}:{}",
                    forward.container_port,
                    forward.effective_host_port()
                )
            })
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// Replace characters that `docker ps --format '{{.Labels}}'` uses as field
/// separators with `_`. Without this, `parse_docker_label` splits the value at
/// the first embedded `,` and the workspace lookup falls apart for a project
/// name like `foo, inc.`.
fn sanitize_label_value(value: &str) -> String {
    value.replace([',', '\n', '\r'], "_")
}

/// Append `flag value` to `docker_args` when `value` is present and non-blank.
/// Used for optional `docker run` flags like `--memory`, `--cpus`, `--shm-size`.
fn push_opt_flag(docker_args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
        docker_args.push(flag.to_string());
        docker_args.push(value.to_string());
    }
}

fn should_inject_coder_home(mounts: &[crate::config::ContainerMount]) -> bool {
    mounts
        .iter()
        .any(|mount| mount.container.starts_with(CODER_HOME))
}

/// Read `image`'s baked-in `PATH` via `docker image inspect` and prepend the
/// container path of every mount flagged `add_to_path = true`.
///
/// This exists because `docker run --env-file`/`-e` sets a literal value with
/// no shell expansion — there is no way to write `PATH=/x:$PATH` in config and
/// have `$PATH` resolve to the image's existing value. Each template's own
/// Dockerfile bakes a different `PATH` (cargo/bin, go/bin, …), so a static
/// config value can't safely cover every template without hardcoding all of
/// them. Reading the real value at launch keeps `add_to_path` mounts working
/// uniformly across every current and future template, including in
/// non-interactive `docker exec` passthrough commands, which never source
/// `.zshrc` and therefore never see rc-file PATH exports.
///
/// Returns `None` when no mount requests `add_to_path`, so callers can skip
/// writing a `PATH` override entirely (the image's own `PATH` already applies).
fn compute_path_override(
    image: &str,
    mounts: &[crate::config::ContainerMount],
) -> Result<Option<String>> {
    let extra_dirs: Vec<String> = mounts
        .iter()
        .filter(|m| m.add_to_path)
        .map(|m| container_path_string(&m.container))
        .collect();
    if extra_dirs.is_empty() {
        return Ok(None);
    }

    let output = std::process::Command::new("docker")
        .args([
            "image",
            "inspect",
            image,
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
        ])
        .output()
        .with_context(|| format!("running docker image inspect for {image}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "docker image inspect {image} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let existing_path = stdout
        .lines()
        .find_map(|line| line.strip_prefix("PATH="))
        .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

    let mut path = extra_dirs.join(":");
    path.push(':');
    path.push_str(existing_path);
    Ok(Some(path))
}

fn should_seed_codex_directory(mount: &crate::config::ContainerMount) -> bool {
    cfg!(target_os = "windows")
        && container_path_string(&mount.container) == CODEX_HOME_CONTAINER_PATH
        && mount.host.is_dir()
}

fn has_nonempty_env_source(
    name: &str,
    ctr_env: &HashMap<String, String>,
    extra_env: &[(String, String)],
    env_passthrough: &[String],
    host_env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    ctr_env.get(name).is_some_and(|value| !value.is_empty())
        || extra_env
            .iter()
            .any(|(key, value)| key == name && !value.is_empty())
        || (env_passthrough.iter().any(|entry| entry == name)
            && host_env(name).is_some_and(|value| !value.is_empty()))
}

/// Build the `docker run -v` arguments for one configured mount.
///
/// Emits the primary `host_arg:container:mode` bind, plus — for NON-SEEDED
/// mounts whose host path differs from the container path — a "plugin-path
/// shim": a second bind that re-exposes the host directory at its OWN
/// host-absolute path (`/Users/<you>/...:/Users/<you>/...`).
///
/// Why the shim exists: Claude Code and the other agent CLIs record absolute
/// host paths in their config metadata (e.g. `installed_plugins.json`'s
/// `installPath`). In the container `$HOME` is `/home/coder`, so a stored
/// `/Users/<you>/.claude/plugins/...` doesn't resolve, and every marketplace
/// plugin — including pgsd and its `/pgsd:*` slash commands — shows
/// "✘ failed to load". Re-exposing the directory at its host path makes those
/// stored strings resolve. It adds no new content/egress/auth surface — it is
/// the same already-mounted directory reachable under a second path.
///
/// Seeded mounts keep their private per-session copy and are NOT re-exposed.
/// `host_arg` is the already-resolved host source (a private tempfile path for
/// seeded mounts, otherwise `mount.host`). The workspace bind is emitted
/// elsewhere and never passes through here.
fn mount_bind_args(
    host_arg: &str,
    mount: &crate::config::ContainerMount,
    seeded: bool,
) -> Result<Vec<String>> {
    let container_arg = container_path_string(&mount.container);
    let mut args = docker_bind_mount_args(host_arg, &container_arg, &mount.mode)?;
    if !seeded && should_add_plugin_path_shim(mount) {
        // The plugin-path shim re-exposes the same directory at its host path so
        // stored absolute paths resolve. It is always read-only regardless of the
        // primary mount's mode: the shim exists only for path resolution, never
        // for writes, and defaulting it to rw needlessly widens write exposure of
        // whatever the user mounted (H4).
        let host_path = mount.host.display().to_string();
        args.extend(docker_bind_mount_args(
            &host_path,
            &host_path,
            &MountMode::Ro,
        )?);
    }
    Ok(args)
}

fn should_add_plugin_path_shim(mount: &crate::config::ContainerMount) -> bool {
    if cfg!(target_os = "windows") {
        return false;
    }
    mount.host != mount.container
}

/// If `mount` is configured for seeding and the host path is a regular file,
/// copy its current contents into a fresh tempfile and return it; the caller
/// bind-mounts that copy instead of the host's live file and keeps the handle
/// alive for the container's lifetime (cleaned up on session drop). For
/// `.claude.json` this seeds the session through (OAuth account, onboarding)
/// while the container's writes land on the private copy — the host file is
/// never touched.
///
/// On macOS, `.claude.json` is additionally patched with the OAuth credentials
/// from the system Keychain so that Claude Code can authenticate inside the
/// Linux container (which has no Keychain access).
///
/// When OAuth token auth is available, a missing seeded `.claude.json` host
/// file is replaced with a private minimal config so interactive Claude skips
/// the login-method onboarding screen while still using the env token.
///
/// Returns `None` (mount the host path as-is) when the mount isn't flagged for
/// seeding or the host path is not a regular file and no private Claude config
/// can be safely synthesized.
fn seed_private_mount(
    mount: &crate::config::ContainerMount,
    claude_oauth_token_available: bool,
) -> Result<Option<NamedTempFile>> {
    let container_file_name = container_path_file_name(&mount.container);
    let is_claude_config = container_file_name.as_deref() == Some(".claude.json");
    let is_claude_credentials = container_file_name.as_deref() == Some(".credentials.json");

    // When CLAUDE_CODE_OAUTH_TOKEN provides launch auth, the host's stored
    // .credentials.json holds a subscription OAuth access token that interactive
    // Claude prefers over the env token whenever an `oauthAccount` is present in
    // .claude.json (which the seeded config keeps). That stored token is usually
    // stale/expired, and its refresh token can't be rotated back to the host, so
    // the TUI's refresh attempt 401s ("Please run /login") even though `claude
    // -p` works from the env token. Seed an empty credentials file so the TUI
    // falls back to the env token. (Without an env token we leave the host's
    // credentials in place so a normally logged-in user still authenticates.)
    let force_empty_credentials = is_claude_credentials && claude_oauth_token_available;

    if !(mount.is_seeded()
        || force_empty_credentials
        || is_claude_config && claude_oauth_token_available)
    {
        return Ok(None);
    }

    let contents = if force_empty_credentials {
        b"{}".to_vec()
    } else if mount.host.is_file() {
        std::fs::read(&mount.host)
            .with_context(|| format!("reading {} to seed container copy", mount.host.display()))?
    } else if is_claude_config && claude_oauth_token_available {
        b"{}".to_vec()
    } else {
        return Ok(None);
    };

    // For .claude.json, merge in macOS Keychain credentials only when env-token
    // auth is not already active. With CLAUDE_CODE_OAUTH_TOKEN, stale account
    // metadata can make interactive Claude prefer stored OAuth state over the
    // env token.
    let contents = if is_claude_config {
        let contents = if claude_oauth_token_available {
            contents
        } else {
            inject_keychain_credentials(contents)?
        };
        if claude_oauth_token_available {
            ensure_claude_interactive_onboarding(contents)?
        } else {
            contents
        }
    } else {
        contents
    };

    let mut tempfile = tempfile::Builder::new()
        .prefix("harness-hat-seed-")
        .tempfile()
        .with_context(|| format!("creating private copy of {}", mount.host.display()))?;
    tempfile
        .write_all(&contents)
        .with_context(|| format!("writing private copy of {}", mount.host.display()))?;
    tempfile
        .flush()
        .with_context(|| format!("flushing private copy of {}", mount.host.display()))?;
    Ok(Some(tempfile))
}

fn seed_claude_oauth_onboarding_config() -> Result<NamedTempFile> {
    let contents = ensure_claude_interactive_onboarding(b"{}".to_vec())?;
    let mut tempfile = tempfile::Builder::new()
        .prefix("harness-hat-claude-config-")
        .suffix(".claude.json")
        .tempfile()
        .context("creating private Claude onboarding config")?;
    tempfile
        .write_all(&contents)
        .context("writing private Claude onboarding config")?;
    tempfile
        .flush()
        .context("flushing private Claude onboarding config")?;
    Ok(tempfile)
}

fn ensure_claude_interactive_onboarding(contents: Vec<u8>) -> Result<Vec<u8>> {
    let mut config: serde_json::Value =
        serde_json::from_slice(&contents).unwrap_or_else(|_| serde_json::json!({}));
    if !config.is_object() {
        config = serde_json::json!({});
    }
    if let Some(obj) = config.as_object_mut() {
        // When CLAUDE_CODE_OAUTH_TOKEN is the launch auth source, stale
        // claudeAiOauth tokens and oauthAccount metadata in the copied host
        // config can take precedence in interactive Claude and produce 401s
        // even though `claude -p` works from the env token.
        obj.remove("claudeAiOauth");
        obj.remove("oauthAccount");
        obj.insert(
            "hasCompletedOnboarding".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    serde_json::to_vec(&config).context("serializing Claude onboarding config")
}

/// Reads the Claude Code OAuth credentials from the macOS Keychain and merges
/// them into the given `.claude.json` bytes. Returns the original bytes
/// unchanged if the Keychain entry is absent or the JSON can't be parsed.
///
/// The `refreshToken` inside `claudeAiOauth` is stripped before merging: the
/// container cannot write a refreshed token back to the macOS Keychain, so
/// leaving it in would let Claude rotate the host's refresh token (invalidating
/// it) while the new token is lost when the container exits.
fn inject_keychain_credentials(contents: Vec<u8>) -> Result<Vec<u8>> {
    let Some(keychain_creds) = read_keychain_claude_credentials() else {
        return Ok(contents);
    };
    let mut config: serde_json::Value = match serde_json::from_slice(&contents) {
        Ok(v) => v,
        Err(_) => return Ok(contents),
    };
    if let (Some(obj), Some(kc_obj)) = (config.as_object_mut(), keychain_creds.as_object()) {
        for (key, val) in kc_obj {
            if key == "claudeAiOauth" {
                if let Some(oauth_obj) = val.as_object() {
                    let mut oauth = oauth_obj.clone();
                    oauth.remove("refreshToken");
                    obj.insert(key.clone(), serde_json::Value::Object(oauth));
                    continue;
                }
            }
            obj.insert(key.clone(), val.clone());
        }
    }
    serde_json::to_vec(&config).context("serializing .claude.json with injected credentials")
}

/// On macOS, reads the Claude Code OAuth credentials blob from the system
/// Keychain via the `security` CLI. Returns `None` if the entry is absent,
/// the command fails, or the output isn't valid JSON.
#[cfg(target_os = "macos")]
fn read_keychain_claude_credentials() -> Option<serde_json::Value> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    serde_json::from_str(raw.trim()).ok()
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_claude_credentials() -> Option<serde_json::Value> {
    None
}

struct ClaudeOAuthCredentials {
    access_token: String,
    scopes: String,
}

/// Reads the Claude Code OAuth access token and scopes from the macOS Keychain.
/// The refresh token is intentionally omitted — see `inject_keychain_credentials`.
fn read_claude_oauth_credentials_full() -> Option<ClaudeOAuthCredentials> {
    let creds = read_keychain_claude_credentials()?;
    let oauth = &creds["claudeAiOauth"];
    let access_token = oauth["accessToken"].as_str()?.to_string();
    // Scopes are stored as a JSON array; join with spaces for the env var.
    let scopes = if let Some(arr) = oauth["scopes"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        // Fall back to the minimum scope Claude needs for TUI auth.
        "user:inference".to_string()
    };
    Some(ClaudeOAuthCredentials {
        access_token,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use super::docker_pty_options;
    use super::{
        ensure_claude_interactive_onboarding, format_localhost_forwards, has_nonempty_env_source,
        mirrored_workspace_mount_target, next_session_alias, normalize_script_line_endings,
        proxy_addr_without_auth, seed_claude_oauth_onboarding_config, seed_private_mount,
        should_inject_coder_home,
    };
    use crate::config::{ContainerMount, LocalhostForward, MountMode};
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::path::PathBuf;

    #[test]
    fn session_aliases_increment_after_largest_numeric_id() {
        let used = ["1", "7", "0042", "legacy"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(next_session_alias(&used, 0).expect("next id"), 43);
    }

    #[test]
    fn session_aliases_start_at_one_and_report_exhaustion() {
        let empty = std::collections::HashSet::new();
        assert_eq!(next_session_alias(&empty, 0).expect("first id"), 1);

        let maxed = [u64::MAX.to_string()].into_iter().collect();
        assert!(next_session_alias(&maxed, 0).is_err());
    }

    fn mount(host: &str, container: &str, seed: Option<bool>) -> ContainerMount {
        ContainerMount {
            host: PathBuf::from(host),
            container: PathBuf::from(container),
            mode: MountMode::Rw,
            seed,
            add_to_path: false,
        }
    }

    #[test]
    fn helper_scripts_are_normalized_for_linux_containers() {
        assert_eq!(
            normalize_script_line_endings("#!/usr/bin/env python3\r\nprint('ok')\r\n"),
            "#!/usr/bin/env python3\nprint('ok')\n"
        );
    }

    #[test]
    fn mirrored_workspace_target_preserves_absolute_posix_path() {
        assert_eq!(
            mirrored_workspace_mount_target(std::path::Path::new("/home/user/my-project")),
            Some(PathBuf::from("/home/user/my-project"))
        );
    }

    #[test]
    fn mirrored_workspace_target_approximates_windows_drive_path() {
        assert_eq!(
            mirrored_workspace_mount_target(std::path::Path::new(r"C:\Users\user\my-project")),
            Some(PathBuf::from("/C/Users/user/my-project"))
        );
        assert_eq!(
            mirrored_workspace_mount_target(std::path::Path::new(r"\\?\c:\Users\user\my-project")),
            Some(PathBuf::from("/C/Users/user/my-project"))
        );
        assert_eq!(
            mirrored_workspace_mount_target(std::path::Path::new(r"D:\")),
            Some(PathBuf::from("/D"))
        );
    }

    #[test]
    fn mirrored_workspace_target_rejects_relative_and_unc_paths() {
        assert_eq!(
            mirrored_workspace_mount_target(std::path::Path::new("relative/path")),
            None
        );
        assert_eq!(
            mirrored_workspace_mount_target(std::path::Path::new(r"\\server\share\repo")),
            None
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn docker_pty_escapes_windows_command_line_arguments() {
        let options = docker_pty_options(vec![
            "run".to_string(),
            "--mount".to_string(),
            "type=bind,source=C:\\Users\\Example User\\repo,target=/workspace".to_string(),
        ]);

        assert!(options.escape_args);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_codex_directory_uses_container_local_state() {
        let source = tempfile::tempdir().expect("create Codex state directory");
        let codex = mount(
            source.path().to_str().expect("UTF-8 temp path"),
            "/home/coder/.codex",
            None,
        );

        assert!(super::should_seed_codex_directory(&codex));
        assert!(!super::should_seed_codex_directory(&mount(
            source.path().to_str().expect("UTF-8 temp path"),
            "/home/coder/.claude",
            None,
        )));
    }

    #[test]
    fn seed_defaults_to_claude_config_and_honors_explicit_flag() {
        // Unset: `.claude.json` seeds by default, everything else does not.
        assert!(mount("/Users/me/.claude.json", "/home/coder/.claude.json", None).is_seeded());
        assert!(
            mount(
                "/Users/me/.claude/.claude.json",
                "/home/coder/.claude/.claude.json",
                None
            )
            .is_seeded()
        );
        assert!(!mount("/Users/me/.claude", "/home/coder/.claude", None).is_seeded());

        // Explicit flag overrides the heuristic in both directions.
        assert!(
            !mount(
                "/Users/me/.claude.json",
                "/home/coder/.claude.json",
                Some(false)
            )
            .is_seeded()
        );
        assert!(mount("/Users/me/.config/x", "/home/coder/.config/x", Some(true)).is_seeded());
    }

    #[test]
    fn seed_private_mount_copies_existing_file_only() {
        // An unflagged non-config mount is left alone even if it's a real file.
        let other = tempfile::Builder::new()
            .tempfile()
            .expect("create temp source");
        let not_seeded = mount(
            other.path().to_str().unwrap(),
            "/home/coder/.codex/config.toml",
            None,
        );
        assert!(
            seed_private_mount(&not_seeded, false)
                .expect("seed non-config")
                .is_none()
        );

        // A seeded .claude.json mount is copied into a private tempfile with the
        // source fields preserved. On macOS, Keychain credentials are also merged
        // in, so we assert the source fields are present rather than byte-identical.
        let mut source = tempfile::Builder::new()
            .suffix(".claude.json")
            .tempfile()
            .expect("create temp source");
        let payload = br#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#;
        source.write_all(payload).expect("write source");
        source.flush().expect("flush source");
        let config = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude.json",
            None,
        );
        let seeded = seed_private_mount(&config, false)
            .expect("seed config")
            .expect("config file should be privatized");
        assert_ne!(seeded.path(), source.path());
        let seeded_bytes = std::fs::read(seeded.path()).expect("read copy");
        let seeded_json: serde_json::Value =
            serde_json::from_slice(&seeded_bytes).expect("seeded file is valid JSON");
        assert_eq!(
            seeded_json["oauthAccount"]["emailAddress"], "a@b.c",
            "source fields must be preserved in seeded copy"
        );

        // `seed = false` forces the shared live bind mount when no OAuth token
        // auth is involved.
        let shared = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude.json",
            Some(false),
        );
        assert!(
            seed_private_mount(&shared, false)
                .expect("seed disabled")
                .is_none()
        );

        // A missing host file is left as-is when no OAuth token is available.
        let missing = mount("/no/such/.claude.json", "/home/coder/.claude.json", None);
        assert!(
            seed_private_mount(&missing, false)
                .expect("seed missing")
                .is_none()
        );
    }

    #[test]
    fn claude_oauth_seed_marks_interactive_onboarding_complete() {
        let mut source = tempfile::Builder::new()
            .suffix(".claude.json")
            .tempfile()
            .expect("create temp source");
        source
            .write_all(
                br#"{"numStartups": 7, "claudeAiOauth": {"accessToken": "stale-token"}, "oauthAccount": {"emailAddress": "a@b.c"}}"#,
            )
            .expect("write source");
        source.flush().expect("flush source");
        let config = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude.json",
            None,
        );

        let seeded = seed_private_mount(&config, true)
            .expect("seed config")
            .expect("config file should be privatized");
        let seeded_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seeded.path()).expect("read seeded"))
                .expect("seeded file is valid JSON");

        assert_eq!(seeded_json["numStartups"], 7);
        assert_eq!(seeded_json["hasCompletedOnboarding"], true);
        assert!(seeded_json.get("claudeAiOauth").is_none());
        assert!(seeded_json.get("oauthAccount").is_none());
    }

    #[test]
    fn claude_oauth_forces_private_onboarding_copy_even_when_seed_is_false() {
        let mut source = tempfile::Builder::new()
            .suffix(".claude.json")
            .tempfile()
            .expect("create temp source");
        source
            .write_all(br#"{"numStartups": 9}"#)
            .expect("write source");
        source.flush().expect("flush source");
        let shared = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude.json",
            Some(false),
        );

        let seeded = seed_private_mount(&shared, true)
            .expect("seed config")
            .expect("OAuth token auth should force a private Claude config");
        let seeded_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seeded.path()).expect("read seeded"))
                .expect("seeded file is valid JSON");

        assert_eq!(seeded_json["numStartups"], 9);
        assert_eq!(seeded_json["hasCompletedOnboarding"], true);
    }

    #[test]
    fn missing_claude_config_is_synthesized_for_oauth_token_auth() {
        let missing = mount("/no/such/.claude.json", "/home/coder/.claude.json", None);

        let seeded = seed_private_mount(&missing, true)
            .expect("seed missing config")
            .expect("missing Claude config should be synthesized");
        let seeded_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seeded.path()).expect("read seeded"))
                .expect("seeded file is valid JSON");

        assert_eq!(seeded_json["hasCompletedOnboarding"], true);
    }

    #[test]
    fn claude_credentials_seeded_empty_when_oauth_token_auth_active() {
        let mut source = tempfile::Builder::new()
            .suffix(".credentials.json")
            .tempfile()
            .expect("create temp source");
        source
            .write_all(br#"{"claudeAiOauth":{"accessToken":"stale-and-expired"}}"#)
            .expect("write source");
        source.flush().expect("flush source");
        let shared = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude/.credentials.json",
            None,
        );

        let seeded = seed_private_mount(&shared, true)
            .expect("seed credentials")
            .expect("OAuth token auth should shadow the host credentials file");
        let seeded_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seeded.path()).expect("read seeded"))
                .expect("seeded file is valid JSON");

        // Host's stale token must not leak through; the TUI then falls back to
        // the CLAUDE_CODE_OAUTH_TOKEN env var (matching `claude -p`).
        assert_eq!(seeded_json, serde_json::json!({}));
    }

    #[test]
    fn claude_credentials_left_intact_without_oauth_token_auth() {
        let mut source = tempfile::Builder::new()
            .suffix(".credentials.json")
            .tempfile()
            .expect("create temp source");
        source
            .write_all(br#"{"claudeAiOauth":{"accessToken":"real"}}"#)
            .expect("write source");
        source.flush().expect("flush source");
        let shared = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude/.credentials.json",
            None,
        );

        // No env token: a normally logged-in user keeps their host credentials,
        // so the unseeded mount is passed through as-is.
        assert!(
            seed_private_mount(&shared, false)
                .expect("seed credentials")
                .is_none()
        );
    }

    #[test]
    fn standalone_claude_onboarding_seed_contains_minimal_config() {
        let seeded = seed_claude_oauth_onboarding_config().expect("seed onboarding config");
        let seeded_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seeded.path()).expect("read seeded"))
                .expect("seeded file is valid JSON");

        assert_eq!(
            seeded_json,
            serde_json::json!({"hasCompletedOnboarding": true})
        );
    }

    #[test]
    fn onboarding_patch_strips_stale_claude_oauth_state() {
        let patched = ensure_claude_interactive_onboarding(
            br#"{"claudeAiOauth":{"accessToken":"stale"},"oauthAccount":{"emailAddress":"a@b.c"}}"#
                .to_vec(),
        )
        .expect("patch onboarding");
        let patched_json: serde_json::Value =
            serde_json::from_slice(&patched).expect("patched file is valid JSON");

        assert_eq!(patched_json["hasCompletedOnboarding"], true);
        assert!(patched_json.get("claudeAiOauth").is_none());
        assert!(patched_json.get("oauthAccount").is_none());
    }

    #[test]
    fn onboarding_patch_recovers_from_invalid_or_non_object_json() {
        for input in [b"not json".as_slice(), b"[]".as_slice()] {
            let patched =
                ensure_claude_interactive_onboarding(input.to_vec()).expect("patch onboarding");
            let patched_json: serde_json::Value =
                serde_json::from_slice(&patched).expect("patched file is valid JSON");

            assert_eq!(
                patched_json,
                serde_json::json!({"hasCompletedOnboarding": true})
            );
        }
    }

    #[test]
    fn proxy_addr_without_auth_strips_userinfo() {
        assert_eq!(
            proxy_addr_without_auth("http://harness-hat:secret@host.docker.internal:54321"),
            "host.docker.internal:54321"
        );
    }

    #[test]
    fn proxy_addr_without_auth_formats_ipv6_hosts() {
        assert_eq!(
            proxy_addr_without_auth("http://harness-hat:secret@[::1]:54321"),
            "[::1]:54321"
        );
    }

    #[test]
    fn localhost_forwards_are_encoded_for_init_script() {
        let forwards = vec![
            LocalhostForward {
                container_port: 8081,
                host_port: None,
            },
            LocalhostForward {
                container_port: 9090,
                host_port: Some(19090),
            },
        ];
        assert_eq!(
            format_localhost_forwards(&forwards).as_deref(),
            Some("8081:8081,9090:19090")
        );
    }

    #[test]
    fn coder_home_is_injected_for_tool_home_mounts() {
        assert!(should_inject_coder_home(&[mount(
            "/host/.cache/tool",
            "/home/coder/.cache/tool",
            None
        )]));
        assert!(should_inject_coder_home(&[mount(
            "/host/.config/tool",
            "/home/coder/.config/tool",
            None
        )]));
        assert!(should_inject_coder_home(&[mount(
            "/host/.tool",
            "/home/coder/.tool",
            None
        )]));
        assert!(!should_inject_coder_home(&[mount(
            "/host/cache",
            "/workspace/cache",
            None
        )]));
    }

    #[test]
    fn env_source_detection_includes_host_passthrough_only_when_set() {
        let mut ctr_env = HashMap::new();
        let extra_env = vec![("EXTRA_TOKEN".to_string(), "extra".to_string())];
        let passthrough = vec![
            "HOST_TOKEN".to_string(),
            "MISSING_TOKEN".to_string(),
            "EMPTY_TOKEN".to_string(),
        ];
        let host_env = |name: &str| match name {
            "HOST_TOKEN" => Some("host".to_string()),
            "EMPTY_TOKEN" => Some(String::new()),
            _ => None,
        };

        assert!(has_nonempty_env_source(
            "EXTRA_TOKEN",
            &ctr_env,
            &extra_env,
            &passthrough,
            &host_env,
        ));
        assert!(has_nonempty_env_source(
            "HOST_TOKEN",
            &ctr_env,
            &extra_env,
            &passthrough,
            &host_env,
        ));
        assert!(!has_nonempty_env_source(
            "MISSING_TOKEN",
            &ctr_env,
            &extra_env,
            &passthrough,
            &host_env,
        ));
        assert!(!has_nonempty_env_source(
            "EMPTY_TOKEN",
            &ctr_env,
            &extra_env,
            &passthrough,
            &host_env,
        ));

        ctr_env.insert("CONFIG_TOKEN".to_string(), "config".to_string());
        assert!(has_nonempty_env_source(
            "CONFIG_TOKEN",
            &ctr_env,
            &extra_env,
            &passthrough,
            &host_env,
        ));
    }

    #[test]
    fn mount_bind_args_adds_plugin_path_shim_for_nonseeded_differing_mounts() {
        // Non-seeded ~/.claude (host != container): primary bind + host-path shim
        // so stored absolute plugin paths (/Users/me/.claude/plugins/...) resolve.
        // The shim is always read-only regardless of the primary mode (H4).
        let claude = mount("/Users/me/.claude", "/home/coder/.claude", None);
        let claude_args =
            super::mount_bind_args("/Users/me/.claude", &claude, false).expect("mount args");
        if cfg!(target_os = "windows") {
            assert_eq!(
                claude_args,
                vec![
                    "--mount".to_string(),
                    "type=bind,source=/Users/me/.claude,target=/home/coder/.claude".to_string(),
                ],
            );
        } else {
            assert_eq!(
                claude_args,
                vec![
                    "--mount".to_string(),
                    "type=bind,source=/Users/me/.claude,target=/home/coder/.claude".to_string(),
                    "--mount".to_string(),
                    "type=bind,source=/Users/me/.claude,target=/Users/me/.claude,readonly"
                        .to_string(),
                ],
            );
        }

        // Seeded mount: primary bind only — the private per-session copy must
        // NOT be re-exposed at the host path.
        let seeded = mount("/Users/me/.claude.json", "/home/coder/.claude.json", None);
        assert_eq!(
            super::mount_bind_args("/tmp/seed-xyz", &seeded, true).expect("mount args"),
            vec![
                "--mount".to_string(),
                "type=bind,source=/tmp/seed-xyz,target=/home/coder/.claude.json".to_string(),
            ],
        );

        // host == container: nothing to shim, it already resolves.
        let same = mount("/Users/me/.claude", "/Users/me/.claude", None);
        assert_eq!(
            super::mount_bind_args("/Users/me/.claude", &same, false)
                .expect("mount args")
                .len(),
            2
        );
    }
}

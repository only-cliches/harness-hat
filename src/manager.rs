use anyhow::Result;
use crossterm::style::Stylize;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Run the interactive workspace manager (the default `hat` action).
pub async fn run() -> Result<()> {
    // A per-user daemon owns the control port after `hat install`. In that
    // case the foreground command is an authenticated client of that daemon,
    // not a second manager competing to bind the same listener. `install`
    // always uses the global config, so favor it for an unqualified command
    // even when the current directory also has a project-local config.
    let daemon_config_path = default_home_config_path().ok().filter(|path| path.exists());
    if let Some(config_path) = daemon_config_path {
        let config = crate::config::load(&config_path)?;
        if daemon_is_reachable(&config).await {
            return run_attached_client(config_path).await;
        }
    }

    // No daemon responded. Preserve the normal cwd-first discovery behavior
    // for a standalone manager.
    let config_path = match resolve_or_prompt_config_path(None)? {
        Some(path) => path,
        None => return Ok(()),
    };
    run_inner(config_path, false, false).await
}

/// Run the terminal-free background agent installed by `hat install`.
pub async fn run_service(config_path: PathBuf, headless_mode: bool) -> Result<()> {
    run_inner(config_path, true, headless_mode).await
}

async fn run_inner(config_path: PathBuf, service_mode: bool, headless_mode: bool) -> Result<()> {
    let config = crate::config::load(&config_path)?;
    let docker_status = crate::container::DockerStatus::new();
    let docker_available = docker_status.refresh();
    if !docker_available && !service_mode {
        return Err(anyhow::anyhow!(docker_status.reason()));
    }
    if !docker_available {
        tracing::warn!(reason = %docker_status.reason(), "Docker is unavailable; daemon will keep checking in the background");
    }

    // ensure_docker_assets covers the base and default dockerfiles too.
    crate::init::ensure_docker_assets(&config.docker_dir)?;

    let telemetry_handle = crate::telemetry::init(&config)?;
    info!("loaded config from {}", config_path.display());

    let config = Arc::new(config);
    let shared_config = crate::shared_config::SharedConfig::new(config.clone());
    let state = crate::state::StateManager::open(&config.logging.log_dir)?;
    let token = state.get_or_create_token()?;

    let (stop_pending_tx, stop_pending_rx) = mpsc::channel::<crate::server::ContainerStopItem>(64);
    let (launch_pending_tx, launch_pending_rx) =
        mpsc::channel::<crate::server::WorkspaceLaunchItem>(16);
    let (restart_pending_tx, restart_pending_rx) =
        mpsc::channel::<crate::server::DaemonRestartItem>(4);
    let (approval_tx, approval_rx) = mpsc::channel::<crate::server::ApprovalControlItem>(32);
    let (exec_pending_tx, exec_pending_rx) = mpsc::channel::<crate::server::PendingItem>(64);
    let (net_pending_tx, net_pending_rx) = mpsc::channel::<crate::proxy::PendingNetworkItem>(64);
    // Bounded (H12): see comments in tui::App and ProxyState/ServerState.
    // 4096 is a few seconds of normal traffic; bursts beyond that are dropped
    // by `try_send` on the producer side rather than backing into memory.
    let (activity_tx, activity_rx) = mpsc::channel::<crate::activity::ActivityEvent>(4096);
    let (audit_tx, audit_rx) = mpsc::channel(256);
    let (tui_tx, tui_rx) = mpsc::channel::<crate::server::TuiFrameItem>(8);

    let session_registry = crate::server::SessionRegistry::default();
    // Shared factory and registry for authenticated, per-session listeners.
    // Construct it before the control server so the disconnect endpoint and
    // the TUI operate on the same live connection trackers.
    let proxy_state =
        crate::proxy::ProxyState::new(shared_config.clone(), net_pending_tx, activity_tx.clone())?;

    let control_port = config.defaults.control.server_port;
    let control_host = config.defaults.control.server_host.clone();
    let allow_remote_control = config.defaults.control.allow_remote_control;
    let control_ip = parse_bind_host(&control_host).map_err(|e| {
        anyhow::anyhow!(
            "config defaults.control.server_host {control_host:?} is not a valid IP address or 'localhost': {e}"
        )
    })?;
    if !is_loopback_bind(&control_ip) && !allow_remote_control {
        anyhow::bail!(
            "refusing to bind control server to non-loopback address {control_host}; \
             set defaults.control.allow_remote_control = true to opt in to remote control"
        );
    }
    let control_addr = format!("{control_host}:{control_port}");
    let tui_events = crate::server::TuiEventBroker::default();
    let server_state = crate::server::ServerState {
        config: shared_config.clone(),
        state: state.clone(),
        pending_tx: exec_pending_tx,
        stop_tx: stop_pending_tx,
        launch_tx: launch_pending_tx,
        restart_tx: restart_pending_tx,
        approval_tx,
        audit_tx,
        token: token.clone(),
        sessions: session_registry.clone(),
        exec_jobs: crate::server::ExecJobRegistry::default(),
        activity_tx: activity_tx.clone(),
        tui_tx,
        tui_events: tui_events.clone(),
        docker_status: docker_status.clone(),
        proxy_state: proxy_state.clone(),
    };
    let control_listener = tokio::net::TcpListener::bind(&control_addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding control server to {control_addr}: {e}"))?;
    info!("control server listening on {}", control_addr);
    let control_handle = tokio::spawn(async move {
        if let Err(e) = crate::server::run_with_listener(server_state, control_listener).await {
            error!("control server error: {e}");
        }
    });

    let docker_monitor_status = docker_status.clone();
    let docker_monitor_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            let was_available = docker_monitor_status.is_available();
            let status = docker_monitor_status.clone();
            let available = match tokio::task::spawn_blocking(move || status.refresh()).await {
                Ok(available) => available,
                Err(error) => {
                    tracing::warn!(%error, "Docker availability check task failed");
                    false
                }
            };
            if available != was_available {
                if available {
                    tracing::info!("Docker is available; daemon is ready to launch workspaces");
                } else {
                    tracing::warn!(reason = %docker_monitor_status.reason(), "Docker became unavailable; daemon will retry in 10 seconds");
                }
            }
        }
    });

    let proxy_host = config.defaults.proxy.proxy_host.clone();
    let proxy_ip = parse_bind_host(&proxy_host).map_err(|e| {
        anyhow::anyhow!(
            "config defaults.proxy.proxy_host {proxy_host:?} is not a valid IP address or 'localhost': {e}"
        )
    })?;
    if !is_loopback_bind(&proxy_ip) && !allow_remote_control {
        anyhow::bail!(
            "refusing to bind proxy to non-loopback address {proxy_host}; \
             set defaults.control.allow_remote_control = true to opt in to remote-reachable bindings"
        );
    }
    // The TUI runs on its own thread with a dedicated current-thread runtime:
    // App owns ContainerSession (whose Box<dyn MasterPty> is !Send), so it
    // must be built and driven on a single thread — but that thread must not
    // be the one running the control server and proxy, or any blocking call
    // in the TUI loop (a terminal emulator that stops draining stdout during
    // screen sharing, a slow synchronous docker invocation) freezes container
    // networking along with the UI. App is constructed inside the closure
    // because it is !Send even while empty.
    let config_path_for_tui = config_path.clone();
    let tui_result = tokio::task::spawn_blocking(move || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            let app = crate::tui::App::new(
                shared_config,
                config_path_for_tui,
                token,
                session_registry,
                exec_pending_rx,
                stop_pending_rx,
                launch_pending_rx,
                restart_pending_rx,
                approval_rx,
                net_pending_rx,
                activity_rx,
                audit_rx,
                state,
                proxy_state,
                service_mode,
                headless_mode,
            )?;
            if service_mode {
                crate::tui::run_service(app, tui_rx, tui_events).await
            } else {
                drop(tui_rx);
                crate::tui::run(app).await
            }
        })
    })
    .await;

    // The TUI loop has exited (user quit). Stop the background listeners so
    // they don't keep accepting connections (or hold an in-flight
    // `/container/stop` against a now-closed oneshot) while we tear down
    // telemetry. Aborting is sufficient: the process is exiting and both tasks
    // only own their listener + ephemeral per-connection state.
    if let Err(error) = &tui_result {
        error!(%error, "Harness Hat TUI thread panicked");
    } else if let Ok(Err(error)) = &tui_result {
        error!(%error, "Harness Hat TUI stopped");
    }
    control_handle.abort();
    docker_monitor_handle.abort();
    telemetry_handle.shutdown()?;
    tui_result.map_err(|e| anyhow::anyhow!("TUI thread panicked: {e}"))??;
    Ok(())
}

async fn daemon_is_reachable(config: &crate::config::Config) -> bool {
    let control_url = format!(
        "http://{}:{}/healthz",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(1))
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    // A healthy response or the daemon's Docker-readiness 503 proves the
    // daemon/control server is alive. Other responses may belong to an
    // unrelated process occupying the configured port.
    matches!(
        client.get(control_url).send().await,
        Ok(response)
            if response.status().is_success()
                || response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
    )
}

async fn run_attached_client(config_path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || crate::tui::run_attached(config_path))
        .await
        .map_err(|error| anyhow::anyhow!("attached TUI thread panicked: {error}"))?
}

pub fn resolve_or_prompt_config_path(explicit: Option<PathBuf>) -> Result<Option<PathBuf>> {
    match explicit {
        Some(path) => Ok(Some(path)),
        None => match discover_default_config_path() {
            Some(path) => Ok(Some(path)),
            None => create_config_from_prompt(),
        },
    }
}

pub fn discover_default_config_path() -> Option<PathBuf> {
    let cwd_candidate = PathBuf::from("harness-hat.toml");
    if cwd_candidate.exists() {
        // Canonicalize immediately so downstream callers (logging, watches,
        // tmp-file siblings) always see an absolute path that does not depend
        // on later cwd changes.
        return Some(cwd_candidate.canonicalize().unwrap_or_else(
            |_| match std::env::current_dir() {
                Ok(cwd) => cwd.join(&cwd_candidate),
                Err(_) => cwd_candidate,
            },
        ));
    }
    let home_candidate = default_home_config_path().ok()?;
    if home_candidate.exists() {
        return Some(home_candidate.canonicalize().unwrap_or(home_candidate));
    }
    None
}

/// Parse a config-supplied bind host (either an `IpAddr` literal or the
/// special string `"localhost"`) into an `IpAddr` for the purpose of deciding
/// whether the bind is loopback. Returns an error on anything else (hostnames
/// would otherwise format-as-string into `bind()`, hiding the policy intent).
fn parse_bind_host(host: &str) -> Result<IpAddr> {
    let trimmed = host.trim();
    if trimmed.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::from([127u8, 0, 0, 1]));
    }
    IpAddr::from_str(trimmed)
        .map_err(|e| anyhow::anyhow!("expected an IPv4/IPv6 literal or 'localhost': {e}"))
}

/// Loopback (127.0.0.0/8, ::1). Mirror the host's loopback set — `0.0.0.0` and
/// `::` are explicitly not loopback even though they include it.
fn is_loopback_bind(ip: &IpAddr) -> bool {
    ip.is_loopback()
}

pub fn default_home_config_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home.join(".config/harness-hat/harness-hat.toml"))
}

enum ConfigCreationChoice {
    CreateCwd,
    CreateHome,
    Cancel,
}

fn create_config_from_prompt() -> Result<Option<PathBuf>> {
    match prompt_config_creation_choice()? {
        ConfigCreationChoice::CreateHome => {
            let path = default_home_config_path()?;
            crate::init::write_sample_config(&path)?;
            println!("created config: {}", path.display());
            Ok(Some(path))
        }
        ConfigCreationChoice::CreateCwd => {
            let path = PathBuf::from("harness-hat.toml");
            crate::init::write_sample_config(&path)?;
            println!("created config: {}", path.display());
            Ok(Some(path))
        }
        ConfigCreationChoice::Cancel => {
            println!("cancelled");
            Ok(None)
        }
    }
}

fn prompt_config_creation_choice() -> Result<ConfigCreationChoice> {
    let cwd = std::env::current_dir()?;
    println!("No config file found.");
    println!(
        "1. Create default config at ~/.config/harness-hat/harness-hat.toml {}",
        "(Recommended)".dark_grey()
    );
    println!(
        "2. Create default config at {}/harness-hat.toml",
        cwd.display()
    );
    println!("3. Cancel and close");
    print!("Select an option [1-3]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice = match input.trim() {
        "1" => ConfigCreationChoice::CreateHome,
        "2" => ConfigCreationChoice::CreateCwd,
        _ => ConfigCreationChoice::Cancel,
    };
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::{ConfigCreationChoice, resolve_or_prompt_config_path};
    use std::path::PathBuf;

    #[test]
    fn config_creation_choice_variants_are_stable() {
        assert!(matches!(
            ConfigCreationChoice::CreateCwd,
            ConfigCreationChoice::CreateCwd
        ));
        assert!(matches!(
            ConfigCreationChoice::CreateHome,
            ConfigCreationChoice::CreateHome
        ));
        assert!(matches!(
            ConfigCreationChoice::Cancel,
            ConfigCreationChoice::Cancel
        ));
    }

    #[test]
    fn resolve_or_prompt_config_path_returns_explicit_without_prompting() {
        let explicit = PathBuf::from("/tmp/explicit-harness-hat.toml");
        let resolved = resolve_or_prompt_config_path(Some(explicit.clone())).expect("resolve path");
        assert_eq!(resolved, Some(explicit));
    }
}

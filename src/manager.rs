use anyhow::Result;
use crossterm::style::Stylize;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Run the interactive workspace manager (the default `hh` action).
pub async fn run(_cli: crate::cli::Cli) -> Result<()> {
    crate::container::ensure_docker_installed_and_running()?;

    let config_path = match resolve_or_prompt_config_path(None)? {
        Some(path) => path,
        None => return Ok(()),
    };

    let config = crate::config::load(&config_path)?;
    // ensure_docker_assets covers the base and default dockerfiles too.
    crate::init::ensure_docker_assets(&config.docker_dir)?;

    let telemetry_handle = crate::telemetry::init(&config)?;
    info!("loaded config from {}", config_path.display());

    let config = Arc::new(config);
    let shared_config = crate::shared_config::SharedConfig::new(config.clone());
    let state = crate::state::StateManager::open(&config.logging.log_dir)?;
    let token = state.get_or_create_token()?;

    let ca_dir = config.logging.log_dir.join("ca");
    let ca = Arc::new(crate::ca::CaStore::load_or_create(&ca_dir)?);

    let ca_cert_path = ca_dir.join("ca.crt");
    info!(
        "Harness Hat CA certificate available at {}.\n{}",
        ca_cert_path.display(),
        ca.cert_pem
    );

    let (stop_pending_tx, stop_pending_rx) = mpsc::channel::<crate::server::ContainerStopItem>(64);
    let (net_pending_tx, net_pending_rx) = mpsc::channel::<crate::proxy::PendingNetworkItem>(64);
    // Bounded (H12): see comments in tui::App and ProxyState/ServerState.
    // 4096 is a few seconds of normal traffic; bursts beyond that are dropped
    // by `try_send` on the producer side rather than backing into memory.
    let (activity_tx, activity_rx) = mpsc::channel::<crate::activity::ActivityEvent>(4096);
    let (audit_tx, audit_rx) = mpsc::channel(256);

    let session_registry = crate::server::SessionRegistry::default();

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
    let server_state = crate::server::ServerState {
        config: shared_config.clone(),
        state: state.clone(),
        stop_tx: stop_pending_tx,
        audit_tx,
        token: token.clone(),
        sessions: session_registry.clone(),
        activity_tx: activity_tx.clone(),
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

    let proxy_port = config.defaults.proxy.proxy_port;
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
    let proxy_addr = format!("{proxy_host}:{proxy_port}");
    let proxy_state = crate::proxy::ProxyState::new(
        ca.clone(),
        shared_config.clone(),
        net_pending_tx,
        activity_tx,
    )?;
    let proxy_addr_display = proxy_addr.clone();
    let proxy_state_for_server = proxy_state.clone();
    let proxy_handle = tokio::spawn(async move {
        if let Err(e) = crate::proxy::run(proxy_state_for_server, proxy_addr).await {
            error!("proxy error: {e}");
        }
    });

    let ca_cert_path_str = ca_cert_path.display().to_string();
    let app = crate::tui::App::new(
        shared_config,
        config_path.clone(),
        token,
        session_registry,
        stop_pending_rx,
        net_pending_rx,
        activity_rx,
        audit_rx,
        state,
        proxy_state,
        proxy_addr_display,
        ca_cert_path_str,
    )?;
    crate::tui::run(app).await?;

    // The TUI loop has exited (user quit). Stop the background listeners so
    // they don't keep accepting connections (or hold an in-flight
    // `/container/stop` against a now-closed oneshot) while we tear down
    // telemetry. Aborting is sufficient: the process is exiting and both tasks
    // only own their listener + ephemeral per-connection state.
    control_handle.abort();
    proxy_handle.abort();

    telemetry_handle.shutdown()?;
    Ok(())
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
        return Some(
            home_candidate
                .canonicalize()
                .unwrap_or_else(|_| home_candidate),
        );
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

/// Authenticated HTTP/CONNECT proxy enforcing policies from harness-rules.toml.
///
/// Containers route all traffic through this proxy. Plain HTTP requests are
/// intercepted and parsed directly. HTTPS and other TCP traffic is gated by
/// destination and then carried through CONNECT tunnels without decrypting it.
///
/// Network policy (auto/prompt/deny) is determined by matching the composed
/// rules against method + host + path for HTTP, or host + port for CONNECT.
use anyhow::Result;
use lru::LruCache;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, error, warn};

use crate::activity::{Activity, ActivityEvent, ActivityKind, ActivityState, payload_preview};
use crate::config::LocalhostForward;
use crate::proxy::connect::handle_connect;
use crate::proxy::helpers::{
    connect_public_tcp_with_priority, is_expected_disconnect, resolve_public_addrs_with_priority,
    write_error_any,
};
use crate::proxy::http::handle_plain_http;
use crate::shared_config::SharedConfig;
use tracing::instrument;

const REQWEST_CLIENT_CACHE_CAPACITY: usize = 256;

const FIRST_BYTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ROOT_PROXY_CONNECTION_LIMIT: usize = 256;
const SCOPED_PROXY_CONNECTION_LIMIT: usize = 128;
const SCOPED_PROXY_TOTAL_CONNECTION_LIMIT: usize = 192;
const SCOPED_PROXY_LIMITED_CONNECTION_LIMIT: usize = 64;
const ROOT_SOURCE_PROXY_CONNECTION_LIMIT: usize = 32;
const LIMITED_SOURCE_PROXY_CONNECTION_LIMIT: usize = 32;

/// A network request waiting on the TUI for an allow/deny decision.
pub struct PendingNetworkItem {
    pub approval_id: String,
    pub activity_id: String,
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    pub source_workspace: Option<String>,
    pub source_container: Option<String>,
    pub source_session_token: Option<String>,
    pub source_status: String,
    pub has_proxy_authorization: bool,
    pub method: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub response_tx: oneshot::Sender<NetworkDecision>,
    pub merged_response_txs: Vec<oneshot::Sender<NetworkDecision>>,
}

/// The result returned by the TUI for a pending network request.
#[derive(Debug, Clone, Copy)]
pub enum NetworkDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub(crate) struct FixedSourceIdentity {
    pub(crate) workspace_name: String,
    pub(crate) container: String,
    pub(crate) auth_token: String,
    pub(crate) limiter_key: String,
    pub(crate) priority: SourcePriority,
    pub(crate) localhost_forwards: Vec<LocalhostForward>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePriority {
    Primary,
    Limited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceIdentityStatus {
    #[cfg(test)]
    Ok,
    ListenerBoundSource,
    #[cfg(test)]
    MalformedAuthHeader,
    #[cfg(test)]
    UnsupportedAuthScheme,
    #[cfg(test)]
    InvalidBase64,
    #[cfg(test)]
    InvalidUtf8,
    #[cfg(test)]
    MissingUsernamePasswordDelimiter,
    #[cfg(test)]
    UnexpectedUsername,
    #[cfg(test)]
    MissingProjectContainerDelimiter,
    #[cfg(test)]
    InvalidProjectEncoding,
    #[cfg(test)]
    InvalidContainerEncoding,
}

impl SourceIdentityStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::Ok => "ok",
            Self::ListenerBoundSource => "listener_bound_source",
            #[cfg(test)]
            Self::MalformedAuthHeader => "malformed_auth_header",
            #[cfg(test)]
            Self::UnsupportedAuthScheme => "unsupported_auth_scheme",
            #[cfg(test)]
            Self::InvalidBase64 => "invalid_base64",
            #[cfg(test)]
            Self::InvalidUtf8 => "invalid_utf8",
            #[cfg(test)]
            Self::MissingUsernamePasswordDelimiter => "missing_username_password_delimiter",
            #[cfg(test)]
            Self::UnexpectedUsername => "unexpected_username",
            #[cfg(test)]
            Self::MissingProjectContainerDelimiter => "missing_project_container_delimiter",
            #[cfg(test)]
            Self::InvalidProjectEncoding => "invalid_project_encoding",
            #[cfg(test)]
            Self::InvalidContainerEncoding => "invalid_container_encoding",
        }
    }
}

// ── Proxy state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
/// Shared proxy state used by all listener tasks.
pub struct ProxyState {
    pub config: SharedConfig,
    pub pending_tx: mpsc::Sender<PendingNetworkItem>,
    // Bounded (H12); senders use `try_send` and drop with a debug log on full.
    pub activity_tx: mpsc::Sender<ActivityEvent>,
    pub(crate) fixed_source: Option<FixedSourceIdentity>,
    connection_limiter: Arc<Semaphore>,
    connection_limit: usize,
    scoped_connection_limiter: Arc<Semaphore>,
    scoped_limited_connection_limiter: Arc<Semaphore>,
    source_connection_limiters: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    /// M1: bounded LRU of per-host reqwest clients, avoids rebuilding the TLS
    /// context + connection pool on every forwarded request.
    http_client_cache: Arc<Mutex<LruCache<String, reqwest::Client>>>,
    connection_trackers: Arc<Mutex<HashMap<String, std::sync::Weak<ConnectionTracker>>>>,
    connection_tracker: Option<Arc<ConnectionTracker>>,
    connection_cancel_flag: Option<Arc<AtomicBool>>,
}

#[derive(Default)]
struct ConnectionTracker {
    next_id: AtomicU64,
    connections: Mutex<HashMap<u64, std::sync::Weak<AtomicBool>>>,
}

struct TrackedConnection {
    id: u64,
    cancel_flag: Arc<AtomicBool>,
    tracker: Arc<ConnectionTracker>,
}

impl ConnectionTracker {
    fn track(self: &Arc<Self>) -> TrackedConnection {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.connections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, Arc::downgrade(&cancel_flag));
        TrackedConnection {
            id,
            cancel_flag,
            tracker: Arc::clone(self),
        }
    }

    fn kill_current(&self) -> usize {
        let mut connections = self
            .connections
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut killed = 0;
        connections.retain(|_, weak| {
            let Some(flag) = weak.upgrade() else {
                return false;
            };
            if !flag.swap(true, Ordering::SeqCst) {
                killed += 1;
            }
            true
        });
        killed
    }
}

impl Drop for TrackedConnection {
    fn drop(&mut self) {
        self.tracker
            .connections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.id);
    }
}

pub(crate) struct ProxyConnectionPermit {
    _listener: Option<tokio::sync::OwnedSemaphorePermit>,
    _scoped_total: Option<tokio::sync::OwnedSemaphorePermit>,
    _scoped_limited: Option<tokio::sync::OwnedSemaphorePermit>,
}

pub(crate) struct SourceConnectionPermit {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ProxyState {
    pub fn new(
        config: SharedConfig,
        pending_tx: mpsc::Sender<PendingNetworkItem>,
        activity_tx: mpsc::Sender<ActivityEvent>,
    ) -> Result<Self> {
        Ok(Self {
            config,
            pending_tx,
            activity_tx,
            fixed_source: None,
            connection_limiter: Arc::new(Semaphore::new(ROOT_PROXY_CONNECTION_LIMIT)),
            connection_limit: ROOT_PROXY_CONNECTION_LIMIT,
            scoped_connection_limiter: Arc::new(Semaphore::new(
                SCOPED_PROXY_TOTAL_CONNECTION_LIMIT,
            )),
            scoped_limited_connection_limiter: Arc::new(Semaphore::new(
                SCOPED_PROXY_LIMITED_CONNECTION_LIMIT,
            )),
            source_connection_limiters: Arc::new(Mutex::new(HashMap::new())),
            http_client_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(REQWEST_CLIENT_CACHE_CAPACITY)
                    .expect("non-zero client cache cap"),
            ))),
            connection_trackers: Arc::new(Mutex::new(HashMap::new())),
            connection_tracker: None,
            connection_cancel_flag: None,
        })
    }

    pub(crate) fn with_fixed_source(
        &self,
        workspace_name: &str,
        container: &str,
        auth_token: &str,
        priority: SourcePriority,
    ) -> Self {
        self.with_fixed_source_and_forwards(
            workspace_name,
            container,
            auth_token,
            priority,
            Vec::new(),
        )
    }

    pub(crate) fn with_fixed_source_and_forwards(
        &self,
        workspace_name: &str,
        container: &str,
        auth_token: &str,
        priority: SourcePriority,
        localhost_forwards: Vec<LocalhostForward>,
    ) -> Self {
        let mut cloned = self.clone();
        cloned.fixed_source = Some(FixedSourceIdentity {
            workspace_name: workspace_name.to_string(),
            container: container.to_string(),
            auth_token: auth_token.to_string(),
            limiter_key: auth_token.to_string(),
            priority,
            localhost_forwards,
        });
        cloned.connection_limiter = Arc::new(Semaphore::new(SCOPED_PROXY_CONNECTION_LIMIT));
        cloned.connection_limit = SCOPED_PROXY_CONNECTION_LIMIT;
        cloned
    }

    pub(crate) fn try_acquire_connection(&self) -> Option<ProxyConnectionPermit> {
        let listener = if self.is_primary_source() {
            None
        } else {
            Some(self.connection_limiter.clone().try_acquire_owned().ok()?)
        };
        let scoped_limited = if self.is_limited_source() {
            Some(
                self.scoped_limited_connection_limiter
                    .clone()
                    .try_acquire_owned()
                    .ok()?,
            )
        } else {
            None
        };
        let scoped_total = if self.is_limited_source() {
            Some(
                self.scoped_connection_limiter
                    .clone()
                    .try_acquire_owned()
                    .ok()?,
            )
        } else {
            None
        };
        Some(ProxyConnectionPermit {
            _listener: listener,
            _scoped_total: scoped_total,
            _scoped_limited: scoped_limited,
        })
    }

    pub(crate) fn try_acquire_source_connection(
        &self,
        source_workspace: Option<&str>,
        source_container: Option<&str>,
    ) -> Option<SourceConnectionPermit> {
        if self.is_primary_source() {
            return Some(SourceConnectionPermit { _permit: None });
        }

        let key = self
            .fixed_source
            .as_ref()
            .map(|fixed| {
                source_connection_key(Some(&fixed.workspace_name), Some(&fixed.limiter_key))
            })
            .unwrap_or_else(|| source_connection_key(source_workspace, source_container));
        let limiter = {
            let mut limiters = self.source_connection_limiters.lock().ok()?;
            limiters
                .entry(key)
                .or_insert_with(|| Arc::new(Semaphore::new(self.source_connection_limit())))
                .clone()
        };
        Some(SourceConnectionPermit {
            _permit: Some(limiter.try_acquire_owned().ok()?),
        })
    }

    fn source_connection_limit(&self) -> usize {
        match self.fixed_source.as_ref().map(|fixed| fixed.priority) {
            Some(SourcePriority::Limited) => LIMITED_SOURCE_PROXY_CONNECTION_LIMIT,
            Some(SourcePriority::Primary) => usize::MAX,
            None => ROOT_SOURCE_PROXY_CONNECTION_LIMIT,
        }
    }

    fn is_primary_source(&self) -> bool {
        self.fixed_source
            .as_ref()
            .is_some_and(|fixed| fixed.priority == SourcePriority::Primary)
    }

    fn is_limited_source(&self) -> bool {
        self.fixed_source
            .as_ref()
            .is_some_and(|fixed| fixed.priority == SourcePriority::Limited)
    }

    fn connection_tracker(&self) -> Option<Arc<ConnectionTracker>> {
        self.connection_tracker.clone()
    }

    pub(crate) fn has_configured_localhost_forward(&self, host: &str, port: u16) -> bool {
        self.localhost_forward_host_port(host, port).is_some()
    }

    pub(crate) async fn resolve_request_addrs(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>> {
        if let Some(host_port) = self.localhost_forward_host_port(host, port) {
            return Ok(vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), host_port),
            ]);
        }
        resolve_public_addrs_with_priority(host, port, self.is_primary_source()).await
    }

    pub(crate) async fn connect_public_tcp(&self, host: &str, port: u16) -> Result<TcpStream> {
        connect_public_tcp_with_priority(host, port, self.is_primary_source()).await
    }

    fn localhost_forward_host_port(&self, host: &str, port: u16) -> Option<u16> {
        if !is_container_host_alias(host) {
            return None;
        }
        let fixed = self.fixed_source.as_ref()?;
        if let Some(forward) = fixed
            .localhost_forwards
            .iter()
            .find(|forward| forward.container_port == port)
        {
            return Some(forward.effective_host_port());
        }
        let cfg = self.config.get();
        let ctr = cfg
            .containers
            .iter()
            .find(|ctr| ctr.name == fixed.container)?;
        ctr.localhost_forwards
            .iter()
            .find(|forward| forward.container_port == port)
            .map(|forward| forward.effective_host_port())
    }

    /// Return a `reqwest::Client` keyed on `host`, with the given resolved
    /// addresses pinned via `resolve_to_addrs`. Builds a fresh client on miss
    /// (and inserts into the LRU), returns a clone of the cached client on
    /// hit. Sharing clients reuses the TLS context and connection pool.
    pub(crate) fn http_client(
        &self,
        host: &str,
        port: u16,
        addrs: &[std::net::SocketAddr],
    ) -> Result<reqwest::Client> {
        let cache_key = format!(
            "{}\0{host}:{port}",
            self.fixed_source
                .as_ref()
                .map(|source| source.auth_token.as_str())
                .unwrap_or("<root>")
        );
        if let Ok(mut cache) = self.http_client_cache.lock()
            && let Some(client) = cache.get(&cache_key)
        {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, addrs)
            .build()?;
        if let Ok(mut cache) = self.http_client_cache.lock() {
            cache.put(cache_key, client.clone());
        }
        Ok(client)
    }

    /// Discard reusable request state after a daemon config refresh. Existing
    /// scoped listeners deliberately keep running because containers depend on
    /// their injected ports; policy is read through `SharedConfig` per request.
    pub(crate) fn clear_reusable_state(&self) {
        if let Ok(mut cache) = self.http_client_cache.lock() {
            cache.clear();
        }
        if let Ok(mut limiters) = self.source_connection_limiters.lock() {
            limiters.clear();
        }
        crate::proxy::clear_dns_cache();
    }

    /// Cancel every proxy connection that was already open for one session.
    /// The listener remains active, so later connections still pass through
    /// the normal policy checks.
    pub fn kill_current_connections(&self, session_token: &str) -> Option<usize> {
        let tracker = {
            let mut trackers = self
                .connection_trackers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let tracker = trackers.get(session_token).and_then(|weak| weak.upgrade());
            if tracker.is_none() {
                trackers.remove(session_token);
            }
            tracker
        }?;
        let killed = tracker.kill_current();

        // Dropping this session's cached clients closes idle upstream HTTP
        // keep-alive sockets that are not represented by an active task.
        if let Ok(mut cache) = self.http_client_cache.lock() {
            let prefix = format!("{session_token}\0");
            let keys = cache
                .iter()
                .filter_map(|(key, _)| key.starts_with(&prefix).then_some(key.clone()))
                .collect::<Vec<_>>();
            for key in keys {
                cache.pop(&key);
            }
        }
        Some(killed)
    }
}

fn is_container_host_alias(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "localhost" | "host.docker.internal" | "host.containers.internal"
    ) {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

fn source_connection_key(source_workspace: Option<&str>, source_container: Option<&str>) -> String {
    format!(
        "{}\0{}",
        source_workspace.unwrap_or("<unknown-workspace>"),
        source_container.unwrap_or("<unknown-container>")
    )
}

impl ProxyState {
    pub(crate) fn start_network_activity(
        &self,
        source_workspace: Option<String>,
        source_container: Option<String>,
        method: &str,
        host: &str,
        path: &str,
        protocol: &str,
        headers: &[(String, String)],
        body: &[u8],
        state: ActivityState,
    ) -> Activity {
        let cancel_flag = self
            .connection_cancel_flag
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let (payload_preview, payload_truncated) = payload_preview(body);
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone());
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok());
        let mut activity = Activity::new(
            source_workspace.unwrap_or_else(|| "unknown-workspace".to_string()),
            source_container,
            ActivityKind::Network {
                method: method.to_string(),
                host: host.to_string(),
                path: path.to_string(),
                protocol: protocol.to_string(),
                payload_preview,
                payload_truncated,
                content_type,
                content_length,
            },
            state,
            cancel_flag,
        );
        activity.session_token = self
            .fixed_source
            .as_ref()
            .map(|source| source.auth_token.clone());
        let _ = self
            .activity_tx
            .try_send(ActivityEvent::Started(Box::new(activity.clone())));
        activity
    }

    pub(crate) fn activity_state(
        &self,
        id: &str,
        state: ActivityState,
        status: impl Into<Option<String>>,
    ) {
        let _ = self.activity_tx.try_send(ActivityEvent::State {
            id: id.to_string(),
            state,
            status: status.into(),
        });
    }

    pub(crate) fn activity_line(&self, id: &str, line: impl Into<String>) {
        let _ = self.activity_tx.try_send(ActivityEvent::Line {
            id: id.to_string(),
            line: line.into(),
        });
    }

    pub(crate) fn activity_finished(
        &self,
        id: &str,
        state: ActivityState,
        status: impl Into<Option<String>>,
    ) {
        let _ = self.activity_tx.try_send(ActivityEvent::Finished {
            id: id.to_string(),
            state,
            status: status.into(),
        });
    }
}

/// A scoped listener task that is aborted when dropped.
pub struct ScopedProxyListener {
    pub addr: String,
    proxy_auth_token: String,
    abort_handle: tokio::task::AbortHandle,
    session_token: String,
    root_state: ProxyState,
}

impl ScopedProxyListener {
    pub fn proxy_url(&self) -> String {
        format!("http://harness-hat:{}@{}", self.proxy_auth_token, self.addr)
    }

    pub fn proxy_auth_token(&self) -> &str {
        &self.proxy_auth_token
    }
}

impl Drop for ScopedProxyListener {
    fn drop(&mut self) {
        self.abort_handle.abort();
        self.root_state
            .connection_trackers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.session_token);
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[instrument(skip(state))]
#[instrument(skip(state, listener))]
async fn run_scoped_listener(state: ProxyState, listener: TcpListener) -> Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _peer) = accepted?;
                let Some(permit) = state.try_acquire_connection() else {
                    warn!(
                        limit = state.connection_limit,
                        "proxy connection limit reached; rejecting connection"
                    );
                    let _ = write_error_any(&mut stream, 503, "Proxy connection limit reached").await;
                    continue;
                };
                let state = state.clone();
                let tracked = state
                    .connection_tracker()
                    .expect("scoped listeners always have a connection tracker")
                    .track();
                let cancel_flag = Arc::clone(&tracked.cancel_flag);
                let mut connection_state = state.clone();
                connection_state.connection_cancel_flag = Some(Arc::clone(&cancel_flag));
                tasks.spawn(async move {
                    let _permit = permit;
                    let _tracked = tracked;
                    let result = tokio::select! {
                        result = handle_connection(stream, connection_state) => result,
                        _ = crate::activity::wait_cancelled(cancel_flag) => Ok(()),
                    };
                    if let Err(e) = result {
                        if is_expected_disconnect(&e) {
                            debug!("proxy: {e}");
                        } else {
                            error!("proxy: {e}");
                        }
                    }
                });
                // M8: opportunistically reap finished tasks to keep the JoinSet
                // from growing forever under steady accept load. The select
                // arm below only fires when select picks it, which is rare on
                // a busy listener.
                while let Some(joined) = tasks.try_join_next() {
                    if let Err(e) = joined {
                        debug!("proxy connection task ended: {e}");
                    }
                }
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(e)) = joined {
                    debug!("proxy connection task ended: {e}");
                }
            }
        }
    }
}

/// Start a per-container proxy listener bound to the supplied host/port.
#[instrument(skip(state))]
pub fn spawn_scoped_listener(
    state: &ProxyState,
    bind_host: &str,
    workspace_name: &str,
    container: &str,
    auth_token: &str,
    priority: SourcePriority,
) -> Result<ScopedProxyListener> {
    spawn_scoped_listener_with_forwards(
        state,
        bind_host,
        workspace_name,
        container,
        auth_token,
        priority,
        Vec::new(),
    )
}

/// Start a per-container proxy listener with session-local localhost-forward
/// overrides from `harness-rules.toml`.
#[instrument(skip(state, localhost_forwards))]
pub fn spawn_scoped_listener_with_forwards(
    state: &ProxyState,
    bind_host: &str,
    workspace_name: &str,
    container: &str,
    auth_token: &str,
    priority: SourcePriority,
    localhost_forwards: Vec<LocalhostForward>,
) -> Result<ScopedProxyListener> {
    let bind_addr = format!("{bind_host}:0");
    let std_listener = std::net::TcpListener::bind(&bind_addr)
        .map_err(|e| anyhow::anyhow!("proxy bind {bind_addr}: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("proxy set_nonblocking {bind_addr}: {e}"))?;
    let local_addr = std_listener.local_addr()?;
    let listener = TcpListener::from_std(std_listener)?;
    let addr = format!("{}:{}", bind_host, local_addr.port());
    let fixed_state = if localhost_forwards.is_empty() {
        state.with_fixed_source(workspace_name, container, auth_token, priority)
    } else {
        state.with_fixed_source_and_forwards(
            workspace_name,
            container,
            auth_token,
            priority,
            localhost_forwards,
        )
    };
    let tracker = Arc::new(ConnectionTracker::default());
    state
        .connection_trackers
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(auth_token.to_string(), Arc::downgrade(&tracker));
    let mut fixed_state = fixed_state;
    fixed_state.connection_tracker = Some(tracker);
    let task = tokio::spawn(async move {
        if let Err(e) = run_scoped_listener(fixed_state, listener).await {
            error!("scoped proxy server error: {e}");
        }
    });
    Ok(ScopedProxyListener {
        addr,
        proxy_auth_token: auth_token.to_string(),
        abort_handle: task.abort_handle(),
        session_token: auth_token.to_string(),
        root_state: state.clone(),
    })
}

// ── Connection dispatch ───────────────────────────────────────────────────────

async fn handle_connection(stream: TcpStream, state: ProxyState) -> Result<()> {
    // L1: `TcpStream::peek` can return short — e.g. just 1 or 2 bytes — even
    // when the client sent the full "CONNECT " token. The previous code would
    // misroute such a short peek into the TLS or plain-HTTP arm. Loop until
    // we have at least 7 bytes (the length of "CONNECT") or the first-byte
    // timeout elapses.
    let mut peek = [0u8; 8];
    let mut n = 0usize;
    let deadline = tokio::time::Instant::now() + FIRST_BYTE_TIMEOUT;
    while n < 7 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.peek(&mut peek)).await {
            Ok(Ok(0)) => break, // remote closed
            Ok(Ok(got)) => {
                if got <= n {
                    // No new bytes since last peek; yield briefly so we don't
                    // busy-loop. peek() does not advance, so a short peek
                    // followed by another peek for the same bytes is normal.
                    tokio::task::yield_now().await;
                }
                n = got;
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "proxy connection timed out waiting for first byte"
                ));
            }
        }
    }

    // Only handle explicit CONNECT requests or plain HTTP.
    // TLS interception (transparent TLS) is disabled without root cert injection.
    if n >= 7 && &peek[..7] == b"CONNECT" {
        handle_connect(stream, state).await
    } else {
        handle_plain_http(stream, state).await
    }
}

#[cfg(test)]
mod connection_tracker_tests {
    use super::*;

    #[test]
    fn kill_current_only_marks_connections_open_at_call_time() {
        let tracker = Arc::new(ConnectionTracker::default());
        let first = tracker.track();
        let second = tracker.track();

        assert_eq!(tracker.kill_current(), 2);
        assert!(first.cancel_flag.load(Ordering::SeqCst));
        assert!(second.cancel_flag.load(Ordering::SeqCst));
        assert_eq!(tracker.kill_current(), 0);

        let later = tracker.track();
        assert!(!later.cancel_flag.load(Ordering::SeqCst));
        assert_eq!(tracker.kill_current(), 1);
        assert!(later.cancel_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_a_connection_removes_it_from_the_tracker() {
        let tracker = Arc::new(ConnectionTracker::default());
        let connection = tracker.track();
        drop(connection);
        assert_eq!(tracker.kill_current(), 0);
    }
}

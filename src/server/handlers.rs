use anyhow::Result;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::stream::unfold;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tracing::{instrument, warn};

use crate::activity::ActivityEvent;
use crate::shared_config::RulesFileStatus;
use crate::shared_config::SharedConfig;
use crate::state::{AuditEntry, DecisionKind, StateManager};

/// Maximum body size accepted by control endpoints (defense-in-depth against
/// memory-amplification DoS through `Content-Length` lies on a POST).
const CONTROL_BODY_LIMIT_BYTES: usize = 8 * 1024;

/// Per-handler timeout used in lieu of a `tower_http::TimeoutLayer` (see below).
const CONTROL_HANDLER_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum number of in-flight control requests, enforced via a per-process
/// semaphore in lieu of a `tower::limit::ConcurrencyLimitLayer` (see below).
const CONTROL_CONCURRENCY_LIMIT: usize = 64;
const TUI_EVENT_CAPACITY: usize = 512;
const TUI_EVENT_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(25);

/// Bounded, ordered daemon events for an attached terminal client. A client
/// starts from a snapshot, then long-polls this log; if it falls behind the
/// retained window it reloads the snapshot instead of guessing at missed state.
#[derive(Clone, Default)]
pub struct TuiEventBroker {
    inner: Arc<Mutex<TuiEventLog>>,
    changed: Arc<Notify>,
}

#[derive(Default)]
struct TuiEventLog {
    next_sequence: u64,
    events: VecDeque<TuiEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiEvent {
    pub sequence: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct TuiEventsQuery {
    #[serde(default)]
    pub after: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TuiEventsResponse {
    pub latest: u64,
    pub reset_required: bool,
    pub events: Vec<TuiEvent>,
}

impl TuiEventBroker {
    pub fn publish(
        &self,
        kind: impl Into<String>,
        workspace: Option<String>,
        message: impl Into<String>,
    ) {
        let mut log = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        log.next_sequence = log.next_sequence.saturating_add(1);
        let sequence = log.next_sequence;
        log.events.push_back(TuiEvent {
            sequence,
            kind: kind.into(),
            workspace,
            message: message.into(),
        });
        if log.events.len() > TUI_EVENT_CAPACITY {
            log.events.pop_front();
        }
        drop(log);
        self.changed.notify_waiters();
    }

    fn since(&self, after: u64) -> TuiEventsResponse {
        let log = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let oldest = log.events.front().map(|event| event.sequence);
        let reset_required = oldest.is_some_and(|sequence| after.saturating_add(1) < sequence);
        let events = if reset_required {
            Vec::new()
        } else {
            log.events
                .iter()
                .filter(|event| event.sequence > after)
                .cloned()
                .collect()
        };
        TuiEventsResponse {
            latest: log.next_sequence,
            reset_required,
            events,
        }
    }
}

/// Error payload returned by manager control endpoints.
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub reason: String,
}

/// Request payload accepted by `POST /exec`.
#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    #[serde(default, alias = "command")]
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default, alias = "image")]
    pub image: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub detach: bool,
}

/// Response payload returned by `POST /exec`.
#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A command request waiting for developer approval in the TUI.
pub struct PendingItem {
    pub id: String,
    pub activity_id: String,
    pub cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub workspace_name: String,
    pub container_id: Option<String>,
    pub argv: Vec<String>,
    pub image: Option<String>,
    pub timeout_secs: u64,
    pub cwd: PathBuf,
    pub rule_cwd: PathBuf,
    pub reason: Option<String>,
    pub matched_command: Option<String>,
    pub response_tx: Option<oneshot::Sender<ApprovalDecision>>,
}

pub enum ApprovalDecision {
    Approve { remember: bool },
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingApprovalRecord {
    Network {
        id: String,
        workspace: Option<String>,
        method: String,
        host: String,
        port: Option<u16>,
        path: String,
    },
    Hostdo {
        id: String,
        workspace: String,
        argv: Vec<String>,
        reason: Option<String>,
        cwd: String,
        image: Option<String>,
        timeout_secs: u64,
    },
    RulesChange {
        id: String,
        path: String,
    },
}

impl PendingApprovalRecord {
    pub fn id(&self) -> &str {
        match self {
            Self::Network { id, .. } | Self::Hostdo { id, .. } | Self::RulesChange { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingApprovalsResponse {
    pub approvals: Vec<PendingApprovalRecord>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    AllowOnce,
    AllowForever,
    DenyOnce,
    DenyForever,
    Trust,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApprovalActionRequest {
    pub action: ApprovalAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalActionResponse {
    pub ok: bool,
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ApprovalControlError {
    pub code: &'static str,
    pub reason: String,
}

/// Query accepted by `GET /rules`.
#[derive(Debug, Deserialize)]
pub struct RulesStatusQuery {
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulesStatusResponse {
    pub rules: Vec<RulesFileStatus>,
}

/// One explicitly addressable configured rules-file scope. This prevents the
/// host CLI from naming arbitrary paths for trust operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum RulesTrustTarget {
    Global,
    Workspace { workspace: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesTrustRequest {
    pub target: RulesTrustTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulesTrustResponse {
    pub ok: bool,
    pub rule: RulesFileStatus,
    pub message: String,
}

pub enum ApprovalControlItem {
    List {
        response_tx: oneshot::Sender<PendingApprovalsResponse>,
    },
    Decide {
        id: String,
        action: ApprovalAction,
        response_tx:
            oneshot::Sender<std::result::Result<ApprovalActionResponse, ApprovalControlError>>,
    },
    RulesStatus {
        workspace: Option<String>,
        response_tx:
            oneshot::Sender<std::result::Result<RulesStatusResponse, ApprovalControlError>>,
    },
    TrustRules {
        target: RulesTrustTarget,
        response_tx: oneshot::Sender<std::result::Result<RulesTrustResponse, ApprovalControlError>>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecJobState {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecJobPhase {
    PendingApproval,
    PullingImage,
    StartingCommand,
    RunningCommand,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExecJobProgress {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobStatus {
    pub state: ExecJobState,
    pub job_id: String,
    #[serde(skip_serializing)]
    pub workspace_name: String,
    #[serde(skip_serializing)]
    pub session_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub timeout_secs: u64,
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<ExecJobPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ExecJobProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing)]
    pub cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[serde(skip_serializing)]
    pub stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    #[serde(skip_serializing)]
    pub created_at: Instant,
}

#[derive(Clone, Default)]
pub struct ExecJobRegistry {
    inner: Arc<Mutex<HashMap<String, ExecJobStatus>>>,
}

/// Ceiling on concurrently running (non-terminal) exec jobs across the whole
/// manager. A container flooding `/exec --detach` cannot spawn unbounded host
/// processes; once this many jobs are running, new job creation is refused (H3).
const MAX_ACTIVE_EXEC_JOBS: usize = 64;
/// Ceiling on total tracked jobs (running + finished). When exceeded, finished
/// jobs are evicted to bound the registry's memory footprint (H3).
const MAX_TOTAL_EXEC_JOBS: usize = 512;
const MAX_TOTAL_EXEC_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const FINISHED_EXEC_JOB_TTL: Duration = Duration::from_secs(24 * 60 * 60);

impl ExecJobRegistry {
    /// Insert a new job, enforcing the active-job ceiling and pruning finished
    /// jobs to bound memory. Returns `None` when the active-job ceiling is
    /// reached, which callers surface as a 503 (H3).
    pub fn insert(&self, mut status: ExecJobStatus) -> Option<ExecJobStatus> {
        if status.job_id.is_empty() {
            status.job_id = uuid::Uuid::new_v4().to_string();
        }
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);

        let active = map
            .values()
            .filter(|job| job.state == ExecJobState::Running)
            .count();
        if active >= MAX_ACTIVE_EXEC_JOBS {
            return None;
        }

        // Evict finished jobs if we are at the total ceiling. Only terminal jobs
        // are dropped, so a running job is never lost; the worst case is that a
        // client can no longer fetch the output of an old, completed job.
        if map.len() >= MAX_TOTAL_EXEC_JOBS {
            let mut finished = map
                .iter()
                .filter(|(_, job)| job.state != ExecJobState::Running)
                .map(|(id, job)| (job.created_at, id.clone()))
                .collect::<Vec<_>>();
            finished.sort_by_key(|(created_at, _)| *created_at);
            for (_, id) in finished
                .into_iter()
                .take(map.len().saturating_sub(MAX_TOTAL_EXEC_JOBS) + 1)
            {
                map.remove(&id);
            }
        }

        map.insert(status.job_id.clone(), status.clone());
        Some(status)
    }

    pub fn update(&self, job_id: &str, f: impl FnOnce(&mut ExecJobStatus)) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(status) = map.get_mut(job_id) {
            f(status);
        }
        prune_expired_finished_jobs(&mut map);
        prune_finished_output_to_budget(&mut map);
    }

    pub fn has_active_capacity(&self) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);
        map.values()
            .filter(|job| job.state == ExecJobState::Running)
            .count()
            < MAX_ACTIVE_EXEC_JOBS
    }

    pub fn get_for_session(&self, job_id: &str, session_token: &str) -> Option<ExecJobStatus> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);
        map.get(job_id)
            .filter(|job| job.session_token == session_token)
            .cloned()
    }

    pub fn list_for_session(&self, session_token: &str) -> Vec<ExecJobStatus> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);
        let mut jobs = map
            .values()
            .filter(|job| job.session_token == session_token)
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.created_at);
        jobs
    }
}

fn prune_finished_output_to_budget(map: &mut HashMap<String, ExecJobStatus>) {
    let output_bytes = |job: &ExecJobStatus| {
        job.stdout.as_ref().map_or(0, String::len) + job.stderr.as_ref().map_or(0, String::len)
    };
    let mut total = map.values().map(output_bytes).sum::<usize>();
    if total <= MAX_TOTAL_EXEC_OUTPUT_BYTES {
        return;
    }
    let mut finished = map
        .values()
        .filter(|job| job.state != ExecJobState::Running)
        .map(|job| (job.created_at, job.job_id.clone(), output_bytes(job)))
        .collect::<Vec<_>>();
    finished.sort_by_key(|(created_at, _, _)| *created_at);
    for (_, job_id, bytes) in finished {
        map.remove(&job_id);
        total = total.saturating_sub(bytes);
        if total <= MAX_TOTAL_EXEC_OUTPUT_BYTES {
            break;
        }
    }
}

fn prune_expired_finished_jobs(map: &mut HashMap<String, ExecJobStatus>) {
    let now = Instant::now();
    map.retain(|_, job| {
        job.state == ExecJobState::Running
            || now.saturating_duration_since(job.created_at) <= FINISHED_EXEC_JOB_TTL
    });
}

impl ExecJobStatus {
    pub fn without_output(mut self) -> Self {
        self.stdout = None;
        self.stderr = None;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobListResponse {
    pub jobs: Vec<ExecJobStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobOutputResponse {
    pub job_id: String,
    pub state: ExecJobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<ExecJobPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExecJobOutputQuery {
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub tail: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobKillResponse {
    pub ok: bool,
    pub job_id: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecJobSendRequest {
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobSendResponse {
    pub ok: bool,
    pub job_id: String,
    pub message: String,
}

/// Request payload accepted by the container stop endpoint.
#[derive(Debug, Deserialize)]
pub struct StopRequest {}

/// Response payload returned by the container stop endpoint.
#[derive(Debug, Serialize)]
pub struct StopResponse {
    pub ok: bool,
}

/// Response returned after cutting a session's currently-open proxy sockets.
#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkDisconnectResponse {
    pub ok: bool,
    pub connections_killed: usize,
}

/// Represents the identity of a running container session.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub workspace_name: String,
    pub container_id: String,
    pub mount_target: String,
}

/// A registry for active container sessions, mapping session tokens to their identities.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, SessionIdentity>>>,
}

impl SessionRegistry {
    pub fn insert(&self, session_token: String, identity: SessionIdentity) {
        // Recover from a poisoned mutex rather than silently no-op'ing.
        // Silent swallowing would leak the session through `remove`/`get`
        // for the lifetime of the process if any holder ever panicked.
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(session_token, identity);
    }

    pub fn remove(&self, session_token: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(session_token);
    }

    pub fn get(&self, session_token: &str) -> Option<SessionIdentity> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(session_token).cloned()
    }
}

/// Shared server state for container lifecycle requests.
#[derive(Clone)]
pub struct ServerState {
    pub config: SharedConfig,
    pub state: StateManager,
    pub pending_tx: mpsc::Sender<PendingItem>,
    pub stop_tx: mpsc::Sender<ContainerStopItem>,
    pub launch_tx: mpsc::Sender<WorkspaceLaunchItem>,
    pub restart_tx: mpsc::Sender<DaemonRestartItem>,
    pub approval_tx: mpsc::Sender<ApprovalControlItem>,
    pub audit_tx: mpsc::Sender<AuditEntry>,
    pub token: String,
    pub sessions: SessionRegistry,
    pub exec_jobs: ExecJobRegistry,
    // Bounded; see H12 comments at the construction site in manager::run.
    pub activity_tx: mpsc::Sender<ActivityEvent>,
    pub tui_tx: mpsc::Sender<TuiFrameItem>,
    pub tui_events: TuiEventBroker,
    pub docker_status: crate::container::DockerStatus,
    pub proxy_state: crate::proxy::ProxyState,
}

/// A frame request from the foreground `hat` terminal. The daemon owns the
/// real App; the client only supplies terminal input and displays its frames.
pub struct TuiFrameItem {
    pub width: u16,
    pub height: u16,
    pub input: Option<TuiInput>,
    pub full_frame: bool,
    /// The attached terminal's own cwd, sent every frame so the daemon-owned
    /// `App` can use it for actions that should reflect where the user
    /// actually is (e.g. pre-filling the new-workspace directory) instead of
    /// the daemon process's own cwd, which is meaningless for a background
    /// service (see `App::remote_client_cwd`).
    pub client_cwd: Option<String>,
    pub response_tx: oneshot::Sender<TuiFrameResponse>,
}

/// A rendered frame plus whether the daemon invalidated the client's cached
/// screen and produced a complete repaint. The flag is sent as an HTTP header
/// so an attached client writes the frame even when its bytes match its last
/// frame.
pub struct TuiFrameResponse {
    pub frame: Vec<u8>,
    pub full_frame: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiFrameRequest {
    pub width: u16,
    pub height: u16,
    #[serde(default)]
    pub input: Option<TuiInput>,
    /// The attached terminal starts with a blank alternate screen and cannot
    /// apply a delta against the daemon's previous client's screen.
    #[serde(default)]
    pub full_frame: bool,
    /// This client's own current working directory. See `TuiFrameItem::client_cwd`.
    #[serde(default)]
    pub client_cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiInput {
    Key {
        code: String,
        modifiers: u8,
    },
    Paste {
        text: String,
    },
    Mouse {
        kind: String,
        column: u16,
        row: u16,
        modifiers: u8,
    },
}

/// A container stop request waiting to be handled by the TUI.
pub struct ContainerStopItem {
    pub workspace_name: String,
    pub container_id: String,
    pub response_tx: Option<oneshot::Sender<ContainerStopDecision>>,
}

/// The decision returned by the TUI for a stop request.
pub enum ContainerStopDecision {
    Stopped,
    NotFound,
}

/// A session-preserving daemon refresh requested by the host CLI. The App owns
/// configuration and session state, so the control listener only forwards it.
pub struct DaemonRestartItem {
    pub response_tx: oneshot::Sender<std::result::Result<(), String>>,
}

#[derive(Debug, Serialize)]
pub struct DaemonRestartResponse {
    pub ok: bool,
    pub message: String,
}

/// Body accepted by `POST /workspace/launch`.
#[derive(Debug, Deserialize)]
pub struct WorkspaceLaunchRequest {
    pub workspace_name: String,
    pub template: String,
    #[serde(default)]
    pub force_rebuild: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub terminal_env: Vec<(String, String)>,
}

/// Final-success payload included in a `LaunchEvent::Launched`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceLaunchResponse {
    pub session_token: String,
    pub alias: String,
    pub docker_name: String,
    pub workspace_name: String,
    pub template: String,
    pub mount_target: String,
}

/// Events streamed back over `POST /workspace/launch`, one per NDJSON line.
/// The TUI emits these as it progresses through the launch (and any
/// intervening `docker build`); the CLI mirrors `status` / `build_output`
/// to stderr and treats `launched` / `error` as terminal.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaunchEvent {
    /// Coarse-grained milestone ("checking image", "building", "launching").
    Status { message: String },
    /// One line of `docker build` output (stdout or stderr).
    BuildOutput { line: String, is_error: bool },
    /// Launch succeeded — terminal, the stream closes after this.
    Launched(WorkspaceLaunchResponse),
    /// Launch (or its prerequisite build) failed — terminal.
    Error { reason: String },
}

/// A workspace-launch request waiting to be handled by the TUI. The TUI
/// reloads config from disk, swaps it into `SharedConfig`, looks up the named
/// workspace/template, builds the image if needed, and runs the same launch
/// path the in-TUI picker uses. Progress is streamed back through `event_tx`.
pub struct WorkspaceLaunchItem {
    pub workspace_name: String,
    pub template: String,
    pub force_rebuild: bool,
    pub cwd: Option<PathBuf>,
    pub terminal_env: Vec<(String, String)>,
    pub event_tx: mpsc::Sender<LaunchEvent>,
}

/// Process-wide semaphore enforcing `CONTROL_CONCURRENCY_LIMIT`. Kept in a
/// `OnceLock` so that `ServerState`'s public struct layout doesn't change —
/// `manager.rs` still constructs it via struct-literal syntax.
///
/// TODO(handlers): once `tower::limit::ConcurrencyLimitLayer` and
/// `tower_http::timeout::TimeoutLayer` are added as direct deps in Cargo.toml,
/// replace this manual semaphore + per-handler `tokio::time::timeout` with the
/// idiomatic tower layers wrapping the router.
fn control_concurrency_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(CONTROL_CONCURRENCY_LIMIT)))
        .clone()
}

/// Max concurrent `/exec` handler invocations. Separate from the control
/// semaphore so a burst of long-running host commands cannot starve container
/// lifecycle endpoints (stop/launch), and vice versa. A synchronous exec holds
/// a permit for the command's duration; a detached exec holds it only briefly
/// while it spawns, with the running-job ceiling (`MAX_ACTIVE_EXEC_JOBS`)
/// bounding detached concurrency (H3).
const HOSTDO_EXEC_CONCURRENCY_LIMIT: usize = 32;

pub(crate) fn hostdo_exec_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(HOSTDO_EXEC_CONCURRENCY_LIMIT)))
        .clone()
}

/// Runs the manager control server.
///
/// The server intentionally exposes only container lifecycle control.
#[instrument(skip(server_state, listener))]
pub async fn run_with_listener(
    server_state: ServerState,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    // Defense-in-depth: cap request body size so a `Content-Length: 1GiB` POST
    // can't allocate before `Json<StopRequest>` rejects it. axum's
    // `DefaultBodyLimit` is built in; tower/tower-http are *not* direct deps,
    // so the timeout + concurrency limit are enforced inside `stop_handler`
    // via `tokio::time::timeout` and `control_concurrency_semaphore()`.
    let router = Router::new()
        .route("/container/stop", post(stop_handler))
        .route(
            "/container/network/disconnect",
            post(network_disconnect_handler),
        )
        .route("/workspace/launch", post(workspace_launch_handler))
        .route("/daemon/restart", post(daemon_restart_handler))
        .route("/approvals", get(approvals_list_handler))
        .route("/approvals/{id}", post(approval_action_handler))
        .route("/rules", get(rules_status_handler))
        .route("/rules/trust", post(rules_trust_handler))
        .route("/tui/frame", post(tui_frame_handler))
        .route("/tui/events", get(tui_events_handler))
        .route("/exec", post(crate::server::core::exec_handler))
        .route(
            "/exec/jobs",
            get(crate::server::core::exec_jobs_list_handler),
        )
        .route(
            "/exec/jobs/{id}",
            get(crate::server::core::exec_job_handler),
        )
        .route(
            "/exec/jobs/{id}/output",
            get(crate::server::core::exec_job_output_handler),
        )
        .route(
            "/exec/jobs/{id}/kill",
            post(crate::server::core::exec_job_kill_handler),
        )
        .route(
            "/exec/jobs/{id}/input",
            post(crate::server::core::exec_job_send_handler),
        )
        .route("/healthz", get(healthz_handler))
        .layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT_BYTES))
        .with_state(Arc::new(server_state));

    axum::serve(listener, router).await?;
    Ok(())
}

async fn approvals_list_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }
    let (response_tx, response_rx) = oneshot::channel();
    if state
        .approval_tx
        .send(ApprovalControlItem::List { response_tx })
        .await
        .is_err()
    {
        return manager_unavailable_response();
    }
    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(response)) => Json(response).into_response(),
        _ => manager_unavailable_response(),
    }
}

async fn approval_action_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ApprovalActionRequest>,
) -> Response {
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }
    let (response_tx, response_rx) = oneshot::channel();
    if state
        .approval_tx
        .send(ApprovalControlItem::Decide {
            id,
            action: request.action,
            response_tx,
        })
        .await
        .is_err()
    {
        return manager_unavailable_response();
    }
    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(Ok(response))) => Json(response).into_response(),
        Ok(Ok(Err(error))) => {
            let status = match error.code {
                "invalid_id" => StatusCode::BAD_REQUEST,
                "not_found" => StatusCode::NOT_FOUND,
                _ => StatusCode::CONFLICT,
            };
            (
                status,
                Json(ErrorResponse {
                    error: error.code.to_string(),
                    reason: error.reason,
                }),
            )
                .into_response()
        }
        _ => manager_unavailable_response(),
    }
}

async fn rules_status_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<RulesStatusQuery>,
) -> Response {
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }
    let (response_tx, response_rx) = oneshot::channel();
    if state
        .approval_tx
        .send(ApprovalControlItem::RulesStatus {
            workspace: query.workspace,
            response_tx,
        })
        .await
        .is_err()
    {
        return manager_unavailable_response();
    }
    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(Ok(response))) => Json(response).into_response(),
        Ok(Ok(Err(error))) => approval_control_error_response(error),
        _ => manager_unavailable_response(),
    }
}

async fn rules_trust_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(request): Json<RulesTrustRequest>,
) -> Response {
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }
    let (response_tx, response_rx) = oneshot::channel();
    if state
        .approval_tx
        .send(ApprovalControlItem::TrustRules {
            target: request.target,
            response_tx,
        })
        .await
        .is_err()
    {
        return manager_unavailable_response();
    }
    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(Ok(response))) => Json(response).into_response(),
        Ok(Ok(Err(error))) => approval_control_error_response(error),
        _ => manager_unavailable_response(),
    }
}

fn approval_control_error_response(error: ApprovalControlError) -> Response {
    let status = match error.code {
        "invalid_id" | "unknown_workspace" => StatusCode::BAD_REQUEST,
        "not_found" => StatusCode::NOT_FOUND,
        _ => StatusCode::CONFLICT,
    };
    (
        status,
        Json(ErrorResponse {
            error: error.code.to_string(),
            reason: error.reason,
        }),
    )
        .into_response()
}

fn manager_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            error: "manager_unavailable".into(),
            reason: "manager is shutting down or did not answer in time".into(),
        }),
    )
        .into_response()
}

/// Liveness probe used by `hat ws` to fail fast with a clear message
/// when the manager isn't running. Intentionally no auth — the response
/// contains nothing sensitive, and requiring the token would force every CLI
/// caller to load and parse it just to print "manager not running."
async fn healthz_handler(State(state): State<Arc<ServerState>>) -> Response {
    if state.docker_status.is_available() {
        return (StatusCode::OK, "ok").into_response();
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            error: "docker_unavailable".into(),
            reason: state.docker_status.reason(),
        }),
    )
        .into_response()
}

/// Reload configuration and disposable caches without replacing the daemon,
/// listener, token, PTYs, or running containers.
pub(super) async fn daemon_restart_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    let semaphore = control_concurrency_semaphore();
    let _permit = match semaphore.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "busy".into(),
                    reason: "control server is at its concurrency limit".into(),
                }),
            )
                .into_response();
        }
    };
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }
    let (response_tx, response_rx) = oneshot::channel();
    if state
        .restart_tx
        .send(DaemonRestartItem { response_tx })
        .await
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }
    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(Ok(()))) => {
            state.tui_events.publish(
                "daemon_refreshed",
                None,
                "daemon configuration and caches refreshed; sessions preserved",
            );
            Json(DaemonRestartResponse {
                ok: true,
                message: "daemon refreshed; running sessions were preserved".into(),
            })
            .into_response()
        }
        Ok(Ok(Err(reason))) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "refresh_failed".into(),
                reason,
            }),
        )
            .into_response(),
        Ok(Err(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager stopped before completing the refresh".into(),
            }),
        )
            .into_response(),
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            Json(ErrorResponse {
                error: "timeout".into(),
                reason: "timed out waiting for daemon refresh".into(),
            }),
        )
            .into_response(),
    }
}

async fn tui_events_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<TuiEventsQuery>,
) -> Response {
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }

    let deadline = tokio::time::Instant::now() + TUI_EVENT_LONG_POLL_TIMEOUT;
    loop {
        // Register before reading so an event published between these steps
        // cannot leave the client waiting for the full long-poll timeout.
        let changed = state.tui_events.changed.notified();
        let response = state.tui_events.since(query.after);
        if response.reset_required
            || !response.events.is_empty()
            || tokio::time::Instant::now() >= deadline
        {
            return Json(response).into_response();
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            return Json(state.tui_events.since(query.after)).into_response();
        }
    }
}

/// Render the daemon-owned TUI for one foreground client tick.
async fn tui_frame_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(request): Json<TuiFrameRequest>,
) -> Response {
    if let Err(response) = require_bearer(&state, &headers) {
        return response;
    }

    if request.width == 0 || request.height == 0 {
        return (
            StatusCode::BAD_REQUEST,
            "terminal dimensions must be non-zero",
        )
            .into_response();
    }
    let (response_tx, response_rx) = oneshot::channel();
    let item = TuiFrameItem {
        width: request.width,
        height: request.height,
        input: request.input,
        full_frame: request.full_frame,
        client_cwd: request.client_cwd,
        response_tx,
    };
    match state.tui_tx.try_send(item) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "tui_busy".into(),
                    reason: "the daemon TUI is busy rendering another frame".into(),
                }),
            )
                .into_response();
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "tui_unavailable".into(),
                    reason: "the daemon TUI is shutting down".into(),
                }),
            )
                .into_response();
        }
    }
    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(frame)) => {
            let mut response = frame.frame.into_response();
            if frame.full_frame {
                response
                    .headers_mut()
                    .insert("x-harness-hat-full-frame", HeaderValue::from_static("1"));
            }
            response
        }
        Ok(Err(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "tui_unavailable".into(),
                reason: "the daemon TUI stopped responding".into(),
            }),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "tui_busy".into(),
                reason: "the daemon TUI did not render a frame in time".into(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn stop_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(_req): Json<StopRequest>,
) -> Response {
    // Per-process in-flight cap (stand-in for ConcurrencyLimitLayer). We
    // `try_acquire` so a slow-loris burst gets a fast 503 rather than tying up
    // a runtime task waiting for a permit indefinitely.
    let semaphore = control_concurrency_semaphore();
    let _permit = match semaphore.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            warn!("stop_handler rejecting request: concurrency limit reached");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "busy".into(),
                    reason: "control server is at its concurrency limit".into(),
                }),
            )
                .into_response();
        }
    };

    let identity = match require_session_identity(&state, &headers) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let (response_tx, response_rx) = oneshot::channel::<ContainerStopDecision>();
    let item = ContainerStopItem {
        workspace_name: identity.workspace_name.clone(),
        container_id: identity.container_id.clone(),
        response_tx: Some(response_tx),
    };
    if state.stop_tx.send(item).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }

    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(ContainerStopDecision::Stopped)) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.workspace_name, DecisionKind::Approved, "stopped"),
            )
            .await;
            Json(StopResponse { ok: true }).into_response()
        }
        Ok(Ok(ContainerStopDecision::NotFound)) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.workspace_name, DecisionKind::Denied, "not_found"),
            )
            .await;
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "not_found".into(),
                    reason: "no running container matched the request".into(),
                }),
            )
                .into_response()
        }
        Ok(Err(_)) | Err(_) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.workspace_name, DecisionKind::TimedOut, "timeout"),
            )
            .await;
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(ErrorResponse {
                    error: "timeout".into(),
                    reason: "timed out waiting for the container stop request".into(),
                }),
            )
                .into_response()
        }
    }
}

pub(super) async fn network_disconnect_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    let (session_token, _identity) = match require_session_context(&state, &headers) {
        Ok(context) => context,
        Err(response) => return response,
    };
    match state.proxy_state.kill_current_connections(&session_token) {
        Some(connections_killed) => Json(NetworkDisconnectResponse {
            ok: true,
            connections_killed,
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "no active network listener matched the session".into(),
            }),
        )
            .into_response(),
    }
}

/// Buffer for the NDJSON event stream. 64 entries leaves comfortable
/// headroom for fast `docker build` output bursts (one line per send) without
/// risking unbounded growth if the CLI is slow to read.
const LAUNCH_EVENT_CHANNEL_CAPACITY: usize = 64;

pub(super) async fn workspace_launch_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(req): Json<WorkspaceLaunchRequest>,
) -> Response {
    let semaphore = control_concurrency_semaphore();
    // Use a permit that lives for the duration of the streaming body, not just
    // the handler future, so a slow client holding the stream open still
    // counts against the concurrency limit. `OwnedSemaphorePermit` is moved
    // into the stream closure below.
    let permit = match semaphore.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            warn!("workspace_launch_handler rejecting request: concurrency limit reached");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "busy".into(),
                    reason: "control server is at its concurrency limit".into(),
                }),
            )
                .into_response();
        }
    };

    if let Err(resp) = require_bearer(&state, &headers) {
        return resp;
    }

    let workspace_name = req.workspace_name.trim().to_string();
    let template = req.template.trim().to_string();
    if workspace_name.is_empty() || template.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "bad_request".into(),
                reason: "workspace_name and template must be non-empty".into(),
            }),
        )
            .into_response();
    }

    let (event_tx, event_rx) = mpsc::channel::<LaunchEvent>(LAUNCH_EVENT_CHANNEL_CAPACITY);
    let item = WorkspaceLaunchItem {
        workspace_name: workspace_name.clone(),
        template: template.clone(),
        force_rebuild: req.force_rebuild,
        cwd: req.cwd.map(PathBuf::from),
        terminal_env: req.terminal_env,
        event_tx,
    };
    if state.launch_tx.send(item).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }
    state.tui_events.publish(
        "workspace_launch_requested",
        Some(workspace_name),
        format!("launch requested with template {template}"),
    );

    // Stream events as NDJSON. `event_tx` lives in the TUI's `WorkspaceLaunchItem`
    // until the launch flow completes (success, build failure, or launch failure);
    // dropping it terminates the receiver and closes the body.
    let events = state.tui_events.clone();
    let stream = unfold(
        (event_rx, permit, events),
        |(mut rx, permit, events)| async move {
            let event = rx.recv().await?;
            publish_launch_event(&events, &event);
            let mut bytes = match serde_json::to_vec(&event) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "serializing launch event failed; closing stream");
                    return None;
                }
            };
            bytes.push(b'\n');
            Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from(bytes)),
                (rx, permit, events),
            ))
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        // Tell intermediaries (and reqwest) not to buffer; we want each line
        // visible to the CLI as the build emits it.
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .expect("static response builder cannot fail")
}

fn publish_launch_event(events: &TuiEventBroker, event: &LaunchEvent) {
    match event {
        LaunchEvent::Status { message } => {
            events.publish("workspace_launch_status", None, message.clone());
        }
        LaunchEvent::BuildOutput { line, is_error } => {
            events.publish(
                if *is_error {
                    "workspace_build_error"
                } else {
                    "workspace_build_output"
                },
                None,
                line.clone(),
            );
        }
        LaunchEvent::Launched(response) => {
            events.publish(
                "workspace_launched",
                Some(response.workspace_name.clone()),
                format!("session {} is running", response.alias),
            );
        }
        LaunchEvent::Error { reason } => {
            events.publish("workspace_launch_failed", None, reason.clone());
        }
    }
}

/// Build an `AuditEntry` describing a `/container/stop` outcome. Centralized
/// here so each match arm above stays a one-liner.
fn stop_audit_entry(project: &str, decision: DecisionKind, reason: &str) -> AuditEntry {
    AuditEntry {
        workspace_name: project.to_string(),
        argv: vec!["container/stop".to_string(), reason.to_string()],
        cwd: String::new(),
        decision,
        exit_code: None,
        duration_ms: None,
        timestamp: chrono::Utc::now(),
    }
}

#[allow(clippy::result_large_err)]
pub(super) fn require_session_identity(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<SessionIdentity, Response> {
    require_session_context(state, headers).map(|(_, identity)| identity)
}

/// Constant-time `Authorization: Bearer <token>` check with no session
/// requirement. Used by endpoints (like `/workspace/launch`) called from the
/// host CLI, before any session exists.
#[allow(clippy::result_large_err)]
pub(super) fn require_bearer(state: &ServerState, headers: &HeaderMap) -> Result<(), Response> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", state.token);
    let auth_bytes = auth.as_bytes();
    let expected_bytes = expected.as_bytes();
    let auth_ok =
        auth_bytes.len() == expected_bytes.len() && bool::from(auth_bytes.ct_eq(expected_bytes));
    if !auth_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "invalid or missing token".into(),
            }),
        )
            .into_response());
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(super) fn require_session_context(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<(String, SessionIdentity), Response> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", state.token);
    // Constant-time comparison so the 64-hex-char token can't be recovered
    // byte-by-byte over a timing channel. We bail on length mismatch first —
    // the token is fixed-length, so that branch doesn't leak useful info, and
    // `ct_eq` requires equal-length inputs to make sense.
    let auth_bytes = auth.as_bytes();
    let expected_bytes = expected.as_bytes();
    let auth_ok =
        auth_bytes.len() == expected_bytes.len() && bool::from(auth_bytes.ct_eq(expected_bytes));
    if !auth_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "invalid or missing token".into(),
            }),
        )
            .into_response());
    }

    let session_token = headers
        .get("x-harness-hat-session-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    if session_token.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "missing session token".into(),
            }),
        )
            .into_response());
    }

    let identity = state.sessions.get(session_token).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "unknown session token".into(),
            }),
        )
            .into_response()
    })?;
    Ok((session_token.to_string(), identity))
}

pub(super) async fn record_audit(state: &ServerState, entry: AuditEntry) {
    if state.audit_tx.send(entry.clone()).await.is_err() {
        warn!("audit event channel is closed; continuing with durable log write");
    }
    let state_clone = state.state.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = state_clone.log_audit(&entry) {
            warn!(error = %error, "failed to write audit event to disk");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_server_state(registry: SessionRegistry, exec_jobs: ExecJobRegistry) -> ServerState {
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (stop_tx, _stop_rx) = mpsc::channel(1);
        let (launch_tx, _launch_rx) = mpsc::channel(1);
        let (restart_tx, _restart_rx) = mpsc::channel(1);
        let (approval_tx, _approval_rx) = mpsc::channel(1);
        let (audit_tx, _audit_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let (tui_tx, _tui_rx) = mpsc::channel(1);
        let (network_tx, _network_rx) = mpsc::channel(1);
        let state_dir =
            std::env::temp_dir().join(format!("harness-hat-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        ServerState {
            config: SharedConfig::new(Arc::new(crate::config::Config::default())),
            state: StateManager::open(&state_dir).expect("state"),
            pending_tx,
            stop_tx,
            launch_tx,
            restart_tx,
            approval_tx,
            audit_tx,
            token: "token".to_string(),
            sessions: registry,
            exec_jobs,
            activity_tx: activity_tx.clone(),
            tui_tx,
            tui_events: TuiEventBroker::default(),
            docker_status: crate::container::DockerStatus::new(),
            proxy_state: crate::proxy::ProxyState::new(
                SharedConfig::new(Arc::new(crate::config::Config::default())),
                network_tx,
                activity_tx.clone(),
            )
            .expect("proxy state"),
        }
    }

    #[tokio::test]
    async fn healthz_reports_when_docker_is_unavailable() {
        let state = test_server_state(SessionRegistry::default(), ExecJobRegistry::default());
        let response = healthz_handler(State(Arc::new(state))).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn session_registry_round_trips_identity() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        registry.insert(
            "other-session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "other-container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );

        let identity = registry.get("session").expect("session identity");
        assert_eq!(identity.workspace_name, "workspace");
        assert_eq!(identity.container_id, "container");

        registry.remove("session");
        assert!(registry.get("session").is_none());
    }

    #[tokio::test]
    async fn network_disconnect_requires_session_auth_and_cuts_registered_listener() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        let state = test_server_state(registry, ExecJobRegistry::default());
        let _listener = crate::proxy::spawn_scoped_listener(
            &state.proxy_state,
            "127.0.0.1",
            "workspace",
            "rust",
            "session",
            crate::proxy::SourcePriority::Primary,
        )
        .expect("scoped listener");

        let unauthorized =
            network_disconnect_handler(State(Arc::new(state.clone())), HeaderMap::new()).await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let headers = HeaderMap::from_iter([
            (
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer token"),
            ),
            (
                axum::http::header::HeaderName::from_static("x-harness-hat-session-token"),
                HeaderValue::from_static("session"),
            ),
        ]);
        let response = network_disconnect_handler(State(Arc::new(state)), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn tui_events_are_ordered_and_require_a_snapshot_after_overflow() {
        let events = TuiEventBroker::default();
        events.publish("first", Some("workspace".to_string()), "one");
        events.publish("second", None, "two");
        let response = events.since(0);
        assert_eq!(response.latest, 2);
        assert!(!response.reset_required);
        assert_eq!(response.events.len(), 2);
        assert_eq!(response.events[0].sequence, 1);
        assert_eq!(response.events[1].sequence, 2);

        for sequence in 0..TUI_EVENT_CAPACITY {
            events.publish("burst", None, sequence.to_string());
        }
        let response = events.since(0);
        assert!(response.reset_required);
        assert!(response.events.is_empty());
    }

    #[test]
    fn pending_approval_json_uses_stable_tag_and_four_digit_id() {
        let value = serde_json::to_value(PendingApprovalRecord::RulesChange {
            id: "0042".to_string(),
            path: "/tmp/harness-rules.toml".to_string(),
        })
        .expect("serialize approval");
        assert_eq!(value["kind"], "rules_change");
        assert_eq!(value["id"], "0042");
    }

    #[test]
    fn require_session_context_accepts_registered_session() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        let state = test_server_state(registry, ExecJobRegistry::default());

        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer token"));
        headers.insert(
            "x-harness-hat-session-token",
            HeaderValue::from_static("session"),
        );
        let (session_token, identity) =
            require_session_context(&state, &headers).expect("session context");
        assert_eq!(session_token, "session");
        assert_eq!(identity.workspace_name, "workspace");
    }

    #[tokio::test]
    async fn tui_frame_requires_a_daemon_token() {
        let state = Arc::new(test_server_state(
            SessionRegistry::default(),
            ExecJobRegistry::default(),
        ));
        let response = tui_frame_handler(
            State(state),
            HeaderMap::new(),
            Json(TuiFrameRequest {
                width: 80,
                height: 24,
                input: None,
                full_frame: false,
                client_cwd: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tui_frame_returns_busy_when_render_queue_is_full() {
        let (tui_tx, _tui_rx) = mpsc::channel(1);
        let (response_tx, _response_rx) = oneshot::channel();
        tui_tx
            .try_send(TuiFrameItem {
                width: 80,
                height: 24,
                input: None,
                full_frame: false,
                client_cwd: None,
                response_tx,
            })
            .expect("fill TUI queue");

        let mut server_state =
            test_server_state(SessionRegistry::default(), ExecJobRegistry::default());
        server_state.tui_tx = tui_tx;
        let response = tui_frame_handler(
            State(Arc::new(server_state)),
            HeaderMap::from_iter([(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer token"),
            )]),
            Json(TuiFrameRequest {
                width: 80,
                height: 24,
                input: None,
                full_frame: false,
                client_cwd: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn approvals_list_requires_auth_and_forwards_to_app() {
        let mut state = test_server_state(SessionRegistry::default(), ExecJobRegistry::default());
        let (approval_tx, mut approval_rx) = mpsc::channel(1);
        state.approval_tx = approval_tx;
        let responder = tokio::spawn(async move {
            let Some(ApprovalControlItem::List { response_tx }) = approval_rx.recv().await else {
                panic!("expected approval list request");
            };
            response_tx
                .send(PendingApprovalsResponse {
                    approvals: vec![PendingApprovalRecord::RulesChange {
                        id: "0042".to_string(),
                        path: "/tmp/harness-rules.toml".to_string(),
                    }],
                })
                .expect("send approval list");
        });

        let unauthorized =
            approvals_list_handler(State(Arc::new(state.clone())), HeaderMap::new()).await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let response = approvals_list_handler(
            State(Arc::new(state)),
            HeaderMap::from_iter([(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer token"),
            )]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        responder.await.expect("approval responder");
    }

    #[tokio::test]
    async fn approval_action_forwards_id_and_action() {
        let mut state = test_server_state(SessionRegistry::default(), ExecJobRegistry::default());
        let (approval_tx, mut approval_rx) = mpsc::channel(1);
        state.approval_tx = approval_tx;
        let responder = tokio::spawn(async move {
            let Some(ApprovalControlItem::Decide {
                id,
                action,
                response_tx,
            }) = approval_rx.recv().await
            else {
                panic!("expected approval decision request");
            };
            assert_eq!(id, "42");
            assert_eq!(action, ApprovalAction::AllowForever);
            response_tx
                .send(Ok(ApprovalActionResponse {
                    ok: true,
                    id: "0042".to_string(),
                    message: "approval allowed and remembered".to_string(),
                }))
                .expect("send approval result");
        });

        let response = approval_action_handler(
            State(Arc::new(state)),
            HeaderMap::from_iter([(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer token"),
            )]),
            AxumPath("42".to_string()),
            Json(ApprovalActionRequest {
                action: ApprovalAction::AllowForever,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        responder.await.expect("approval responder");
    }

    #[tokio::test]
    async fn exec_job_uuid_routes_resolve_under_axum_0_8() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        registry.insert(
            "other-session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "other-container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        let exec_jobs = ExecJobRegistry::default();
        let job_id = "151fb311-2a78-458d-b036-eb9a59e7f0ad";
        exec_jobs.insert(ExecJobStatus {
            state: ExecJobState::Complete,
            job_id: job_id.to_string(),
            workspace_name: "workspace".to_string(),
            session_token: "session".to_string(),
            container: Some("container".to_string()),
            timeout_secs: 60,
            argv: vec!["curl".to_string(), "example.com".to_string()],
            cwd: Some("/workspace".to_string()),
            phase: None,
            image: None,
            message: "Command finished with exit code 0.".to_string(),
            progress: None,
            poll_after_ms: None,
            exit_code: Some(0),
            stdout: Some("ok\n".to_string()),
            stderr: Some(String::new()),
            reason: None,
            cancel_flag: None,
            stdin_tx: None,
            created_at: Instant::now(),
        });
        let state = test_server_state(registry, exec_jobs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            run_with_listener(state, listener)
                .await
                .expect("control server")
        });
        let client = reqwest::Client::new();

        let status_response = client
            .get(format!("http://{addr}/exec/jobs/{job_id}"))
            .bearer_auth("token")
            .header("x-harness-hat-session-token", "session")
            .send()
            .await
            .expect("job status request");
        assert_eq!(status_response.status(), StatusCode::OK);
        let status_body: serde_json::Value = status_response.json().await.expect("job status json");
        assert_eq!(status_body["job_id"], job_id);
        assert!(status_body.get("stdout").is_none());

        let cross_session_response = client
            .get(format!("http://{addr}/exec/jobs/{job_id}"))
            .bearer_auth("token")
            .header("x-harness-hat-session-token", "other-session")
            .send()
            .await
            .expect("cross-session job request");
        assert_eq!(cross_session_response.status(), StatusCode::NOT_FOUND);

        let output_response = client
            .get(format!("http://{addr}/exec/jobs/{job_id}/output"))
            .bearer_auth("token")
            .header("x-harness-hat-session-token", "session")
            .send()
            .await
            .expect("job output request");
        assert_eq!(output_response.status(), StatusCode::OK);
        let output_body: serde_json::Value = output_response.json().await.expect("job output json");
        assert_eq!(output_body["job_id"], job_id);
        assert_eq!(output_body["stdout"], "ok\n");

        server.abort();
    }
}

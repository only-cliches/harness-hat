use super::*;

fn append_session_terminal(group: &mut TuiSessionGroup, session_idx: usize) {
    if !group.terminal_indices.contains(&session_idx) {
        group.terminal_indices.push(session_idx);
    }
}

impl App {
    fn watched_rules_paths(cfg: &crate::config::Config) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(cfg.workspaces.len() + 1);
        paths.push(cfg.manager.global_rules_file.clone());
        for workspace in &cfg.workspaces {
            paths.push(workspace.canonical_path.join("harness-rules.toml"));
        }
        paths.sort();
        paths.dedup();
        paths
    }

    fn content_hash_for_path(path: &Path) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                // `template` is workspace launch state, not executable or
                // network policy. Ignore only that top-level field so a
                // remembered template does not trigger a fail-closed policy
                // alert; all other content remains byte-sensitive.
                match raw.parse::<toml_edit::DocumentMut>() {
                    Ok(mut doc) => {
                        doc.remove("template");
                        doc.to_string().hash(&mut hasher);
                    }
                    Err(_) => raw.hash(&mut hasher),
                }
            }
            Err(_) => 0u8.hash(&mut hasher),
        }
        hasher.finish()
    }

    pub(crate) fn watched_file_stamp(path: &Path) -> WatchedFileStamp {
        match std::fs::metadata(path) {
            Ok(md) => {
                let (mtime_secs, mtime_nanos) = md
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| (d.as_secs(), d.subsec_nanos()))
                    .unwrap_or((0, 0));
                WatchedFileStamp {
                    exists: true,
                    size: md.len(),
                    mtime_secs,
                    mtime_nanos,
                    content_hash: Self::content_hash_for_path(path),
                }
            }
            Err(_) => WatchedFileStamp {
                exists: false,
                size: 0,
                mtime_secs: 0,
                mtime_nanos: 0,
                content_hash: 0,
            },
        }
    }

    pub(crate) fn sidebar_item_is_selectable(item: &SidebarItem) -> bool {
        !matches!(item, SidebarItem::WorkspacesHeader)
    }

    pub(crate) fn first_selectable_sidebar_idx(items: &[SidebarItem]) -> usize {
        items
            .iter()
            .position(Self::sidebar_item_is_selectable)
            .unwrap_or(0)
    }

    pub(crate) fn selected_sidebar_item_from(&self, items: &[SidebarItem]) -> Option<SidebarItem> {
        items.get(self.sidebar_idx).cloned()
    }

    pub(crate) fn restore_sidebar_selection(
        &mut self,
        selected: Option<&SidebarItem>,
        items: &[SidebarItem],
    ) {
        if items.is_empty() {
            self.sidebar_idx = 0;
            self.sidebar_offset = 0;
            self.preview_session = None;
            return;
        }

        if let Some(selected) = selected
            && let Some(idx) = items.iter().position(|item| item == selected)
        {
            self.sidebar_idx = idx;
            self.update_sidebar_preview(items);
            return;
        }

        self.sidebar_idx = self.sidebar_idx.min(items.len().saturating_sub(1));
        if !Self::sidebar_item_is_selectable(&items[self.sidebar_idx]) {
            self.sidebar_idx = Self::first_selectable_sidebar_idx(items);
        }
        self.update_sidebar_preview(items);
    }

    pub fn new(
        config: SharedConfig,
        loaded_config_path: PathBuf,
        token: String,
        session_registry: SessionRegistry,
        exec_pending_rx: mpsc::Receiver<PendingItem>,
        stop_pending_rx: mpsc::Receiver<ContainerStopItem>,
        launch_pending_rx: mpsc::Receiver<WorkspaceLaunchItem>,
        restart_pending_rx: mpsc::Receiver<crate::server::DaemonRestartItem>,
        approval_control_rx: mpsc::Receiver<crate::server::ApprovalControlItem>,
        net_pending_rx: mpsc::Receiver<PendingNetworkItem>,
        activity_rx: mpsc::Receiver<ActivityEvent>,
        audit_rx: mpsc::Receiver<AuditEntry>,
        state: StateManager,
        proxy_state: ProxyState,
        service_mode: bool,
        headless_mode: bool,
    ) -> Result<Self> {
        let cfg = config.get();
        let watched_rules_stamps = Self::watched_rules_paths(&cfg)
            .into_iter()
            .map(|path| {
                let stamp = Self::watched_file_stamp(&path);
                (path, stamp)
            })
            .collect::<std::collections::HashMap<_, _>>();

        let workspaces = cfg
            .workspaces
            .iter()
            .zip(crate::config::resolve_workspace_sidebar_hotkeys(
                &cfg.workspaces,
            ))
            .map(|p| WorkspaceStatus {
                name: p.0.name.clone(),
                sidebar_hotkey: p.1,
            })
            .collect();

        let mut log = state
            .recent_audit(200)
            .unwrap_or_default()
            .into_iter()
            .map(LogEntry::Audit)
            .collect::<VecDeque<_>>();

        log.push_front(LogEntry::Msg {
            text: format!("loaded config from {}", loaded_config_path.display()),
            is_error: false,
            timestamp: chrono::Utc::now(),
        });

        let (build_event_tx, build_event_rx) = mpsc::channel(BUILD_EVENT_CHANNEL_CAPACITY);
        let (native_dialog_tx, native_dialog_rx) = mpsc::channel(4);
        let (container_usage_tx, container_usage_rx) = mpsc::channel(32);
        let (rules_scan_tx, rules_scan_rx) = mpsc::channel(2);

        let rules_path = &cfg.manager.global_rules_file;
        let network_rule_count = crate::rules::load(rules_path)
            .map(|r| r.network.allowlist.len() + r.network.denylist.len())
            .unwrap_or(0);
        log.push_front(LogEntry::Msg {
            text: format!(
                "Loaded rules from {} (network: {})",
                rules_path.display(),
                network_rule_count
            ),
            is_error: false,
            timestamp: chrono::Utc::now(),
        });

        Ok(Self {
            config,
            loaded_config_path,
            token,
            session_registry,
            proxy_state,
            workspaces,
            pending_stop: vec![],
            pending_exec: vec![],
            pending_net: vec![],
            activities: vec![],
            log,
            log_scroll: 0,
            focus: Focus::Sidebar,
            sidebar_idx: Self::first_selectable_sidebar_idx(&[
                SidebarItem::NewSession,
                SidebarItem::NewWorkspace,
            ]),
            sidebar_offset: 0,
            session_groups: Vec::new(),
            active_session: None,
            active_activity: None,
            active_network_session: None,
            network_cursor: 0,
            preview_session: None,
            active_settings_workspace: None,
            settings_cursor: 0,
            workspace_action_workspace: None,
            workspace_action_cursor: 0,
            container_picker: None,
            build_container_idx: None,
            build_workspace_idx: None,
            build_session_group: None,
            build_cursor: 0,
            pending_force_rebuild: false,
            build_output: VecDeque::new(),
            build_scroll: 0,
            sessions: vec![],
            new_workspace: None,
            remove_workspace_confirm: None,
            base_rules_changed: None,
            next_approval_id: 0,
            exec_pending_rx,
            stop_pending_rx,
            launch_pending_rx,
            restart_pending_rx,
            approval_control_rx,
            workspace_launch_pending: None,
            net_pending_rx,
            background_channels: BackgroundUiChannels {
                native_dialog_rx,
                native_dialog_tx,
                container_usage_rx,
                container_usage_tx,
                rules_scan_rx,
                rules_scan_tx,
            },
            native_dialog_inflight: None,
            service_mode,
            headless_mode,
            activity_rx,
            audit_rx,
            build_event_rx,
            build_event_tx,
            build_task: None,
            build_finished: None,
            rules_scan_in_flight: false,
            should_quit: false,
            passthrough_mode: false,
            passthrough_exit_code_slot: None,
            log_fullscreen: false,
            terminal_fullscreen: false,
            last_terminal_esc: None,
            scroll_mode: false,
            terminal_scroll: 0,
            terminal_selection_area: None,
            selection_dragging: false,
            pending_clipboard: None,
            container_usage: HashMap::new(),
            last_base_rules_poll: std::time::Instant::now(),
            watched_rules_stamps,
            pending_base_rules_internal_write: std::collections::HashMap::new(),
            remote_client_cwd: None,
        })
    }

    pub(crate) fn tick_base_rules_file_watch(&mut self) {
        let now = std::time::Instant::now();
        if now.duration_since(self.last_base_rules_poll) < std::time::Duration::from_millis(750) {
            return;
        }
        self.last_base_rules_poll = now;

        let cfg = self.config.get();
        let watched_paths = Self::watched_rules_paths(&cfg);

        // Cheap, no-I/O bookkeeping: drop state for paths we no longer watch.
        self.watched_rules_stamps
            .retain(|path, _| watched_paths.iter().any(|watched| watched == path));
        self.pending_base_rules_internal_write
            .retain(|path, pending| {
                watched_paths.iter().any(|watched| watched == path) && now <= pending.expires_at
            });

        // Stamping each file means `fs::metadata` + a full `fs::read`/hash per
        // file, which can stall for the size of the file. Run it off the UI
        // thread and apply the result in drain_channels(); skip if a scan is
        // already running so ticks never pile up.
        if self.rules_scan_in_flight {
            return;
        }
        self.rules_scan_in_flight = true;
        let tx = self.background_channels.rules_scan_tx.clone();
        std::thread::spawn(move || {
            // Drop-guard: `rules_scan_in_flight` is only cleared when a result
            // arrives via `apply_rules_scan`. If stamping panics partway, this
            // guarantees a (possibly empty) result is still sent so the flag is
            // reset and the rules-tampering watch keeps running (H15).
            struct SendOnDrop {
                tx: Option<mpsc::Sender<Vec<(PathBuf, WatchedFileStamp)>>>,
            }
            impl Drop for SendOnDrop {
                fn drop(&mut self) {
                    if let Some(tx) = self.tx.take() {
                        let _ = tx.blocking_send(Vec::new());
                    }
                }
            }
            let mut guard = SendOnDrop { tx: Some(tx) };

            let stamps: Vec<(PathBuf, WatchedFileStamp)> = watched_paths
                .into_iter()
                .map(|path| {
                    let stamp = Self::watched_file_stamp(&path);
                    (path, stamp)
                })
                .collect();
            if let Some(tx) = guard.tx.take() {
                let _ = tx.blocking_send(stamps);
            }
        });
    }

    /// Apply a validated configuration refresh without touching session-owned
    /// PTYs, containers, approvals, builds, listeners, or authentication.
    pub(crate) fn soft_refresh(&mut self) -> Result<(), String> {
        let reloaded = crate::config::load(&self.loaded_config_path).map_err(|error| {
            format!(
                "could not reload {}: {error}",
                self.loaded_config_path.display()
            )
        })?;
        let selected_before = {
            let items = self.sidebar_items();
            self.selected_sidebar_item_from(&items)
        };
        self.config.set(std::sync::Arc::new(reloaded));
        self.refresh_workspace_statuses();
        let items = self.sidebar_items();
        self.restore_sidebar_selection(selected_before.as_ref(), &items);

        let cfg = self.config.get();
        self.watched_rules_stamps = Self::watched_rules_paths(&cfg)
            .into_iter()
            .map(|path| {
                let stamp = Self::watched_file_stamp(&path);
                (path, stamp)
            })
            .collect();
        self.pending_base_rules_internal_write.clear();
        self.rules_scan_in_flight = false;
        self.last_base_rules_poll = std::time::Instant::now();
        self.proxy_state.clear_reusable_state();
        self.push_log(
            "daemon refreshed configuration and caches; running sessions preserved",
            false,
        );
        Ok(())
    }

    /// Apply a background rules-file scan: compare each freshly computed stamp
    /// against the last known one and raise a security alert on any change that
    /// wasn't one of our own pending writes. Runs on the UI thread.
    pub(crate) fn apply_rules_scan(&mut self, stamps: Vec<(PathBuf, WatchedFileStamp)>) {
        self.rules_scan_in_flight = false;
        let now = std::time::Instant::now();

        for (path, current_stamp) in stamps {
            let prev = self
                .watched_rules_stamps
                .entry(path.clone())
                .or_insert_with(|| current_stamp.clone());
            if current_stamp == *prev {
                continue;
            }
            *prev = current_stamp;

            if let Some(pending) = self.pending_base_rules_internal_write.get(&path).cloned() {
                if now <= pending.expires_at {
                    let current = std::fs::read_to_string(&path).unwrap_or_default();
                    if current == pending.expected_content {
                        self.pending_base_rules_internal_write.remove(&path);
                        if let Err(error) = self
                            .config
                            .trust_rules_file_if_contents(&path, &pending.expected_content)
                        {
                            self.push_log(
                                format!(
                                    "failed to verify Harness Hat's rules write '{}': {error}",
                                    path.display()
                                ),
                                true,
                            );
                        }
                        continue;
                    }
                }
                self.pending_base_rules_internal_write.remove(&path);
            }

            self.config.block_rules_file(&path);
            let Some(approval_id) = self.allocate_approval_id() else {
                self.push_log(
                    format!(
                        "SECURITY ALERT: rules file changed but approval ID space is exhausted: {}",
                        path.display()
                    ),
                    true,
                );
                continue;
            };
            self.base_rules_changed = Some(BaseRulesChangedState {
                approval_id,
                path: path.clone(),
                expected_contents: std::fs::read(&path).ok(),
                dialog_dismissed: false,
            });
            self.push_log(
                format!(
                    "SECURITY ALERT: rules file changed outside CLI: {}",
                    path.display()
                ),
                true,
            );
        }
    }

    pub(crate) fn note_rules_internal_write(&mut self, path: PathBuf, expected_content: String) {
        self.pending_base_rules_internal_write.insert(
            path,
            PendingBaseRulesInternalWrite {
                expected_content,
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(2),
            },
        );
    }

    pub fn sidebar_items(&self) -> Vec<SidebarItem> {
        let cfg = self.config.get();
        let mut items = Vec::new();
        items.push(SidebarItem::NewSession);

        for (gi, group) in self.session_groups.iter().enumerate() {
            items.push(SidebarItem::Session(gi));
            if let Some(session_idx) = group.terminal_indices.first().copied()
                && session_idx < self.sessions.len()
            {
                items.extend(Self::sidebar_activity_items_for_session(
                    session_idx,
                    self.activities_for_session(session_idx),
                ));
            }
            // A session group can contain more than one terminal (for example,
            // repeated launches of the same workspace/template). Keep the
            // group row as the first terminal, then expose the remaining
            // terminals as selectable child rows.
            for session_pos in 1..group.terminal_indices.len() {
                let Some(session_idx) = group.terminal_indices.get(session_pos).copied() else {
                    continue;
                };
                if session_idx >= self.sessions.len() {
                    continue;
                }
                items.push(SidebarItem::SessionTerminal(gi, session_pos));
                items.extend(Self::sidebar_activity_items_for_session(
                    session_idx,
                    self.activities_for_session(session_idx),
                ));
            }
            if let Some(pi) = group.workspace_idx {
                items.push(SidebarItem::Settings(pi));
            }
        }

        if let Some(pi) = self.build_workspace_idx {
            if pi < cfg.workspaces.len() {
                items.push(SidebarItem::Build(pi));
            }
        }

        // Every configured workspace gets a launch entry, so a new session can
        // be started in any workspace directly from the sidebar, preceded by a
        // non-selectable section header.
        if !cfg.workspaces.is_empty() {
            items.push(SidebarItem::WorkspacesHeader);
            for pi in 0..cfg.workspaces.len() {
                items.push(SidebarItem::Launch(pi));
            }
        }

        items.push(SidebarItem::NewWorkspace);
        items
    }

    pub(crate) fn session_group_terminal(
        &self,
        group_idx: usize,
        terminal_pos: usize,
    ) -> Option<usize> {
        self.session_groups
            .get(group_idx)
            .and_then(|group| group.terminal_indices.get(terminal_pos).copied())
    }

    pub(crate) fn first_terminal_for_session_group(&self, group_idx: usize) -> Option<usize> {
        self.session_groups
            .get(group_idx)
            .and_then(|group| group.terminal_indices.first().copied())
    }

    pub(crate) fn resolve_or_create_session_group(
        &mut self,
        session_group: Option<usize>,
        workspace_idx: usize,
        ctr_idx: usize,
        configured_ctr_idx: Option<usize>,
    ) -> usize {
        if let Some(group_idx) = session_group
            && group_idx < self.session_groups.len()
        {
            return group_idx;
        }

        let cfg = self.config.get();
        let workspace_name = cfg
            .workspaces
            .get(workspace_idx)
            .map(|ws| ws.name.clone())
            .unwrap_or_else(|| format!("workspace-{workspace_idx}"));
        let templates = self.workspace_templates_for_workspace(workspace_idx);
        let template_name = templates
            .get(ctr_idx)
            .map(|ctr| ctr.name.clone())
            .unwrap_or_else(|| format!("template-{ctr_idx}"));

        self.session_groups.push(TuiSessionGroup {
            workspace_name,
            workspace_idx: Some(workspace_idx),
            template_name,
            template_idx: configured_ctr_idx,
            terminal_indices: Vec::new(),
        });

        self.session_groups.len().saturating_sub(1)
    }

    pub(crate) fn add_session_terminal(&mut self, session_group: usize, session_idx: usize) {
        if let Some(group) = self.session_groups.get_mut(session_group) {
            append_session_terminal(group, session_idx);
        }
    }

    pub(crate) fn remove_session_terminal(
        &mut self,
        session_idx: usize,
        removed_from: Option<usize>,
    ) {
        if let Some(group_idx) = removed_from {
            self.remove_terminal_from_group(group_idx, session_idx);
            self.remove_empty_session_groups();
            return;
        }

        for group_idx in 0..self.session_groups.len() {
            self.remove_terminal_from_group(group_idx, session_idx);
        }
        self.remove_empty_session_groups();
    }

    fn remove_terminal_from_group(&mut self, group_idx: usize, session_idx: usize) {
        if let Some(group) = self.session_groups.get_mut(group_idx) {
            group.terminal_indices.retain(|si| *si != session_idx);
            for idx in &mut group.terminal_indices {
                if *idx > session_idx {
                    *idx -= 1;
                }
            }
        }
    }

    fn remove_empty_session_groups(&mut self) {
        self.session_groups
            .retain(|group| !group.terminal_indices.is_empty());
    }

    pub fn selected_workspace_idx(&self) -> Option<usize> {
        match self.sidebar_items().get(self.sidebar_idx) {
            Some(SidebarItem::Session(si)) | Some(SidebarItem::SessionTerminal(si, _)) => self
                .session_groups
                .get(*si)
                .and_then(|group| group.workspace_idx),
            Some(SidebarItem::NewSession) => None,
            Some(SidebarItem::NetworkGroup(si)) => self.sessions.get(*si).and_then(|session| {
                self.config
                    .get()
                    .workspaces
                    .iter()
                    .position(|p| p.name == session.workspace_name)
            }),
            Some(SidebarItem::Activity(id)) => {
                let cfg = self.config.get();
                let project = self.activity_by_id(id)?.workspace_name.as_str();
                cfg.workspaces.iter().position(|p| p.name == project)
            }
            Some(SidebarItem::Settings(si)) => self
                .session_groups
                .get(*si)
                .and_then(|group| group.workspace_idx),
            // A workspace launch entry already carries the workspace index.
            Some(SidebarItem::Launch(pi)) => Some(*pi),
            Some(SidebarItem::Build(pi)) => Some(*pi),
            Some(SidebarItem::WorkspacesHeader) | Some(SidebarItem::NewWorkspace) => None,
            None => None,
        }
    }

    pub(crate) fn session_depth(&self, session_idx: usize) -> usize {
        let _ = session_idx;
        0
    }

    pub(crate) fn session_is_waiting(&self, session_idx: usize) -> bool {
        let Some(session) = self.sessions.get(session_idx) else {
            return false;
        };
        if session.is_exited()
            || session.is_terminal_detached()
            || self.session_is_loading(session_idx)
        {
            return false;
        }
        session.terminal_stable_for() >= std::time::Duration::from_secs(2)
    }

    pub fn pending_for_session(&self, session_idx: usize) -> Vec<usize> {
        let Some(session) = self.sessions.get(session_idx) else {
            return Vec::new();
        };
        self.pending_exec
            .iter()
            .enumerate()
            .filter(|(_, pending)| {
                pending.workspace_name == session.workspace_name
                    && pending.container_id.as_deref().is_none_or(|container| {
                        Self::container_identity_matches(
                            container,
                            &session.container_id,
                            &session.container_name,
                            &session.docker_name,
                        )
                    })
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    pub(crate) fn activity_by_id(&self, id: &str) -> Option<&Activity> {
        self.activities.iter().find(|activity| activity.id == id)
    }

    pub(crate) fn activity_by_id_mut(&mut self, id: &str) -> Option<&mut Activity> {
        self.activities
            .iter_mut()
            .find(|activity| activity.id == id)
    }

    pub(crate) fn activities_for_session(&self, session_idx: usize) -> Vec<&Activity> {
        let Some(session) = self.sessions.get(session_idx) else {
            return vec![];
        };
        self.activities
            .iter()
            .filter(|activity| {
                Self::activity_matches_session_identity(
                    activity,
                    &session.workspace_name,
                    &session.session_token,
                    &session.container_id,
                    &session.container_name,
                    &session.docker_name,
                )
            })
            .collect()
    }

    pub(crate) fn sidebar_activity_items_for_session(
        session_idx: usize,
        activities: Vec<&Activity>,
    ) -> Vec<SidebarItem> {
        let mut items = Vec::new();
        items.push(SidebarItem::NetworkGroup(session_idx));
        for activity in activities {
            if !matches!(
                &activity.kind,
                crate::activity::ActivityKind::Network { .. }
            ) {
                items.push(SidebarItem::Activity(activity.id.clone()));
            }
        }
        items
    }

    pub(crate) fn network_activities_for_session(&self, session_idx: usize) -> Vec<&Activity> {
        self.activities_for_session(session_idx)
            .into_iter()
            .filter(|activity| {
                matches!(
                    &activity.kind,
                    crate::activity::ActivityKind::Network { .. }
                )
            })
            .collect()
    }

    pub(crate) fn network_activity_count_for_session(&self, session_idx: usize) -> usize {
        self.network_activities_for_session(session_idx).len()
    }

    pub(crate) fn network_cursor_for_len(&self, session_idx: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        if self.active_network_session == Some(session_idx) {
            self.network_cursor.min(len.saturating_sub(1))
        } else {
            len.saturating_sub(1)
        }
    }

    pub(crate) fn network_cursor_for_session(&self, session_idx: usize) -> usize {
        let len = self.network_activity_count_for_session(session_idx);
        self.network_cursor_for_len(session_idx, len)
    }

    pub(crate) fn clamp_active_network_cursor(&mut self) {
        let Some(si) = self.active_network_session else {
            return;
        };
        let len = self.network_activity_count_for_session(si);
        if len == 0 {
            self.network_cursor = 0;
            return;
        }
        self.network_cursor = self.network_cursor.min(len.saturating_sub(1));
    }

    pub(crate) fn session_for_activity(&self, id: &str) -> Option<usize> {
        let activity = self.activity_by_id(id)?;
        self.sessions.iter().position(|session| {
            Self::activity_matches_session_identity(
                activity,
                &session.workspace_name,
                &session.session_token,
                &session.container_id,
                &session.container_name,
                &session.docker_name,
            )
        })
    }

    pub(crate) fn container_identity_matches(
        container: &str,
        container_id: &str,
        container_name: &str,
        docker_name: &str,
    ) -> bool {
        let normalized = container.trim();
        !normalized.is_empty()
            && ((!container_id.is_empty()
                && (container_id == normalized
                    || container_id.starts_with(normalized)
                    || normalized.starts_with(container_id)))
                || container_name == normalized
                || docker_name == normalized)
    }

    pub(crate) fn activity_matches_session_identity(
        activity: &Activity,
        project: &str,
        session_token: &str,
        container_id: &str,
        container_name: &str,
        docker_name: &str,
    ) -> bool {
        if activity.workspace_name != project {
            return false;
        }
        if let Some(activity_session_token) = activity.session_token.as_deref() {
            return !activity_session_token.is_empty() && activity_session_token == session_token;
        }
        activity.container.as_deref().is_some_and(|container| {
            Self::container_identity_matches(container, container_id, container_name, docker_name)
        })
    }

    /// Upper bound on `self.activities` (H12). The list TTL-prunes terminal
    /// entries but `Running`/`PendingApproval` entries are kept indefinitely;
    /// without a cap a noisy in-container client can grow the Vec without
    /// bound. We prefer dropping the oldest non-terminal entry over rejecting
    /// new activity events, so the UI always reflects the most recent state.
    const MAX_ACTIVITIES: usize = 1000;

    pub(crate) fn apply_activity_event(&mut self, event: ActivityEvent) {
        match event {
            ActivityEvent::Started(activity) => {
                if let Some(existing) = self.activity_by_id_mut(&activity.id) {
                    *existing = *activity;
                } else {
                    // Cap-and-prune: when we're at the ceiling, evict the
                    // oldest non-terminal activity (terminal ones already
                    // expire via the TTL). If everything is terminal, drop
                    // the very oldest entry to make room.
                    if self.activities.len() >= Self::MAX_ACTIVITIES {
                        let victim = self
                            .activities
                            .iter()
                            .position(|a| !a.state.is_terminal())
                            .unwrap_or(0);
                        self.activities.remove(victim);
                    }
                    self.activities.push(*activity);
                }
            }
            ActivityEvent::State { id, state, status } => {
                if let Some(activity) = self.activity_by_id_mut(&id) {
                    if matches!(
                        (&activity.kind, &state),
                        (
                            crate::activity::ActivityKind::Hostdo { .. },
                            crate::activity::ActivityState::Running
                        )
                    ) {
                        activity.mark_command_started(std::time::Instant::now());
                    }
                    activity.state = state;
                    activity.status = status;
                    activity.updated_at = std::time::Instant::now();
                    if activity.state.is_terminal() {
                        activity.finished_at.get_or_insert(activity.updated_at);
                    } else {
                        activity.finished_at = None;
                        activity.terminal_unselected_at = None;
                    }
                }
            }
            ActivityEvent::Line { id, line } => {
                if let Some(activity) = self.activity_by_id_mut(&id) {
                    activity.push_line(line);
                }
            }
            ActivityEvent::Finished { id, state, status } => {
                if let Some(activity) = self.activity_by_id_mut(&id) {
                    if matches!(activity.kind, crate::activity::ActivityKind::Hostdo { .. }) {
                        activity.mark_command_finished(std::time::Instant::now());
                    }
                    activity.state = state;
                    activity.status = status;
                    activity.updated_at = std::time::Instant::now();
                    activity.finished_at = Some(activity.updated_at);
                    activity.terminal_unselected_at = None;
                }
            }
        }
    }

    pub(crate) fn refresh_terminal_activity_selection(&mut self, items: &[SidebarItem]) {
        let now = std::time::Instant::now();
        let active_activity = self.active_activity.clone();
        let sidebar_activity = items.get(self.sidebar_idx).and_then(|item| match item {
            SidebarItem::Activity(id) => Some(id.clone()),
            _ => None,
        });
        let sidebar_network_session = items.get(self.sidebar_idx).and_then(|item| match item {
            SidebarItem::NetworkGroup(si) => Some(*si),
            _ => None,
        });
        let selected_network_session = self.active_network_session.or(sidebar_network_session);
        let selected_network_identity = selected_network_session.and_then(|si| {
            self.sessions.get(si).map(|session| {
                (
                    session.workspace_name.clone(),
                    session.session_token.clone(),
                    session.container_id.clone(),
                    session.container_name.clone(),
                    session.docker_name.clone(),
                )
            })
        });
        for activity in &mut self.activities {
            if !activity.state.is_terminal() {
                activity.terminal_unselected_at = None;
                continue;
            }
            let selected_by_network_group = if let (
                crate::activity::ActivityKind::Network { .. },
                Some((project, session_token, container_id, container_name, docker_name)),
            ) =
                (&activity.kind, selected_network_identity.as_ref())
            {
                Self::activity_matches_session_identity(
                    activity,
                    project,
                    session_token,
                    container_id,
                    container_name,
                    docker_name,
                )
            } else {
                false
            };
            let is_selected = active_activity.as_deref() == Some(activity.id.as_str())
                || sidebar_activity.as_deref() == Some(activity.id.as_str())
                || selected_by_network_group;
            if is_selected {
                activity.terminal_unselected_at = None;
            } else if activity.terminal_unselected_at.is_none() {
                activity.terminal_unselected_at = Some(now);
            }
        }
    }

    pub(crate) fn prune_terminal_activities(&mut self) {
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_secs(crate::activity::ACTIVITY_TERMINAL_TTL_SECS);
        let items = self.sidebar_items();
        self.refresh_terminal_activity_selection(&items);
        self.activities.retain(|activity| {
            !activity.state.is_terminal()
                || activity
                    .terminal_unselected_at
                    .map(|unselected_at| now.duration_since(unselected_at) < ttl)
                    .unwrap_or(true)
        });
        self.clamp_active_network_cursor();
    }

    pub(crate) fn refresh_session_terminal_states(&mut self) -> bool {
        let mut changed = false;
        for session in &mut self.sessions {
            changed |= session.refresh_terminal_snapshot();
        }
        changed
    }

    pub(crate) fn has_pending_approval_modal(&self) -> bool {
        !self.pending_exec.is_empty()
            || !self.pending_net.is_empty()
            || self.headless_rules_overlay_visible()
            || self.native_dialog_inflight.is_some()
    }

    pub(crate) fn headless_rules_overlay_visible(&self) -> bool {
        self.headless_mode
            && self
                .base_rules_changed
                .as_ref()
                .is_some_and(|state| !state.dialog_dismissed)
    }

    pub(crate) fn active_exec_modal_idx(&self) -> Option<usize> {
        (!self.pending_exec.is_empty()).then_some(0)
    }

    pub(crate) fn sidebar_workspace_hotkey_target(&self, hotkey: char) -> Option<usize> {
        let workspace_idx = self
            .workspaces
            .iter()
            .position(|workspace| workspace.sidebar_hotkey == Some(hotkey))?;

        let items = self.sidebar_items();
        // Prefer jumping to an active session group for this workspace; if none
        // exists, fall back to the workspace's launch entry.
        items
            .iter()
            .position(|item| matches!(item, SidebarItem::Session(si) if self.selected_workspace_idx_for_group(*si) == Some(workspace_idx)))
            .or_else(|| {
                items
                    .iter()
                    .position(|item| matches!(item, SidebarItem::Launch(pi) if *pi == workspace_idx))
            })
    }

    pub(crate) fn selected_workspace_idx_for_group(&self, group_idx: usize) -> Option<usize> {
        self.session_groups
            .get(group_idx)
            .and_then(|group| group.workspace_idx)
    }

    /// Resolve the currently hovered sidebar row to a workspace index for the
    /// Ctrl+D delete shortcut, whether the row is the not-yet-running launch
    /// entry or a running session group's row.
    pub(crate) fn sidebar_workspace_idx_for_delete(&self, items: &[SidebarItem]) -> Option<usize> {
        match items.get(self.sidebar_idx)? {
            SidebarItem::Launch(pi) => Some(*pi),
            SidebarItem::Session(si) => self.selected_workspace_idx_for_group(*si),
            _ => None,
        }
    }

    pub(crate) fn session_is_loading(&self, session_idx: usize) -> bool {
        let Some(session) = self.sessions.get(session_idx) else {
            return false;
        };
        if session.is_exited() || session.is_terminal_detached() {
            return false;
        }
        let term = session.term.lock();
        let mut content = term.renderable_content();
        !content
            .display_iter
            .any(|indexed| !indexed.cell.c.is_whitespace())
    }

    pub(crate) fn close_session(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }

        let selected_before = self.selected_sidebar_item_from(&self.sidebar_items());

        let mut indices = self.session_tree_indices(idx);
        indices.sort_unstable();
        // The high-to-low removal + per-step `remap_session_indices_after_removal`
        // below is only correct when at most one session is removed per
        // `close_session` call. `session_tree_indices` returns exactly one
        // index today, so this holds — but if it ever grows (e.g. closing a
        // parent + its forks), the remap would be applied multiple times to
        // `active_session > removed_idx`, producing an off-by-one (M24).
        debug_assert!(
            indices.len() <= 1,
            "close_session expects session_tree_indices() to yield at most one index; got {}. If this changes, switch to a single batched remap.",
            indices.len()
        );
        let tokens = indices
            .iter()
            .filter_map(|idx| {
                self.sessions
                    .get(*idx)
                    .map(|session| session.session_token.clone())
            })
            .collect::<std::collections::HashSet<_>>();
        let container_identities = indices
            .iter()
            .filter_map(|idx| {
                self.sessions.get(*idx).map(|session| {
                    [
                        session.container_id.clone(),
                        session.container_name.clone(),
                        session.docker_name.clone(),
                    ]
                })
            })
            .flatten()
            .collect::<std::collections::HashSet<_>>();
        self.deny_pending_exec_for_sessions(&tokens, &container_identities);
        self.deny_pending_network_for_sessions(&tokens, &container_identities);

        for remove_idx in indices.into_iter().rev() {
            self.remove_session_terminal(remove_idx, None);
            if let Some(tok) = self
                .sessions
                .get(remove_idx)
                .map(|s| s.session_token.clone())
            {
                self.session_registry.remove(&tok);
            }
            if let Some(session) = self.sessions.get(remove_idx) {
                self.container_usage.remove(&session.docker_name);
                if !session.is_exited() {
                    // Fire-and-forget: `docker rm -f` blocks for the SIGTERM
                    // grace + SIGKILL window (several seconds on busy
                    // workloads). Running it on the UI thread would freeze
                    // the TUI; the runtime's `is_exited` watch picks up the
                    // shutdown asynchronously.
                    session.terminate_in_background();
                }
            }
            self.sessions.remove(remove_idx);
            self.remap_session_indices_after_removal(remove_idx);
        }

        let items = self.sidebar_items();
        self.restore_sidebar_selection(selected_before.as_ref(), &items);
    }

    fn session_tree_indices(&self, idx: usize) -> Vec<usize> {
        if idx < self.sessions.len() {
            vec![idx]
        } else {
            Vec::new()
        }
    }

    fn deny_pending_network_for_sessions(
        &mut self,
        tokens: &std::collections::HashSet<String>,
        container_identities: &std::collections::HashSet<String>,
    ) {
        let mut i = 0;
        while i < self.pending_net.len() {
            let activity_id = self.pending_net[i].activity_id.clone();
            let source_container = self.pending_net[i].source_container.clone();
            let matches_activity = self
                .activity_by_id(&activity_id)
                .and_then(|activity| activity.session_token.as_deref())
                .is_some_and(|session_token| tokens.contains(session_token));
            let matches_container = source_container
                .as_deref()
                .is_some_and(|source| container_identities.contains(source));

            if matches_activity || matches_container {
                let pending = self.pending_net.remove(i);
                crate::tui::app::approvals::send_pending_network_decision_owned(
                    pending,
                    crate::proxy::NetworkDecision::Deny,
                );
                if let Some(activity) = self.activity_by_id_mut(&activity_id) {
                    activity.state = crate::activity::ActivityState::Cancelled;
                    activity.status = Some("container stopped".to_string());
                    activity.updated_at = std::time::Instant::now();
                    activity.finished_at = Some(activity.updated_at);
                    activity.terminal_unselected_at = None;
                }
            } else {
                i += 1;
            }
        }
    }

    fn deny_pending_exec_for_sessions(
        &mut self,
        tokens: &std::collections::HashSet<String>,
        container_identities: &std::collections::HashSet<String>,
    ) {
        let mut i = 0;
        while i < self.pending_exec.len() {
            let activity_id = self.pending_exec[i].activity_id.clone();
            let matches_activity = self
                .activity_by_id(&activity_id)
                .and_then(|activity| activity.session_token.as_deref())
                .is_some_and(|session_token| tokens.contains(session_token));
            let matches_container = self.pending_exec[i]
                .container_id
                .as_deref()
                .is_some_and(|source| container_identities.contains(source));

            if matches_activity || matches_container {
                self.deny_exec(i);
                if let Some(activity) = self.activity_by_id_mut(&activity_id) {
                    activity.state = crate::activity::ActivityState::Cancelled;
                    activity.status = Some("container stopped".to_string());
                    activity.updated_at = std::time::Instant::now();
                    activity.finished_at = Some(activity.updated_at);
                    activity.terminal_unselected_at = None;
                }
            } else {
                i += 1;
            }
        }
    }

    pub(crate) fn cancel_activity(&mut self, id: &str) {
        if let Some(activity) = self.activity_by_id(id) {
            activity.request_cancel();
        }
        if let Some(idx) = self
            .pending_exec
            .iter()
            .position(|item| item.activity_id == id)
        {
            self.deny_exec(idx);
            return;
        }
        if let Some(activity) = self.activity_by_id_mut(id) {
            activity.state = crate::activity::ActivityState::Cancelled;
            activity.status = Some("cancellation requested".to_string());
            activity.updated_at = std::time::Instant::now();
        }
    }

    pub(crate) fn selected_network_activity_id(&self, session_idx: usize) -> Option<ActivityId> {
        self.network_activities_for_session(session_idx)
            .get(self.network_cursor_for_session(session_idx))
            .map(|activity| activity.id.clone())
    }

    pub(crate) fn mark_session_stopped(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        for idx in self.session_tree_indices(idx) {
            let Some(session) = self.sessions.get(idx) else {
                continue;
            };
            if !session.is_exited() {
                // Fire-and-forget — see comment in `close_session`.
                session.terminate_in_background();
            }
            session
                .exited
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(crate) fn clear_terminal_fullscreen_for_removed_session(&mut self, removed_idx: usize) {
        if self.active_session == Some(removed_idx) {
            self.terminal_fullscreen = false;
            self.last_terminal_esc = None;
        }
    }

    pub(crate) fn remap_session_indices_after_removal(&mut self, removed_idx: usize) {
        self.clear_terminal_fullscreen_for_removed_session(removed_idx);
        match self.active_session {
            Some(si) if si == removed_idx => {
                self.active_session = None;
                self.focus = Focus::Sidebar;
            }
            Some(si) if si > removed_idx => {
                self.active_session = Some(si - 1);
            }
            _ => {}
        }
        match self.active_network_session {
            Some(si) if si == removed_idx => {
                self.active_network_session = None;
                self.network_cursor = 0;
                if self.focus == Focus::Network {
                    self.focus = Focus::Sidebar;
                }
            }
            Some(si) if si > removed_idx => {
                self.active_network_session = Some(si - 1);
            }
            _ => {}
        }
        match self.preview_session {
            Some(si) if si == removed_idx => {
                self.preview_session = None;
            }
            Some(si) if si > removed_idx => {
                self.preview_session = Some(si - 1);
            }
            _ => {}
        }
    }

    pub(crate) fn terminate_all_sessions(&mut self) {
        for session in &self.sessions {
            if !session.is_exited() {
                // Fire-and-forget at quit too: the spawned `docker rm`
                // process is reparented to init when we exit, so it still
                // completes and the daemon still removes the container.
                // This makes `q` snappy instead of waiting per-container.
                session.terminate_in_background();
            }
        }
    }

    pub(crate) fn kill_network_connections_for_session(&mut self, idx: usize) {
        let Some(session) = self.sessions.get(idx) else {
            return;
        };
        let token = session.session_token.clone();
        let label = session.tab_label();
        match self.proxy_state.kill_current_connections(&token) {
            Some(count) => self.push_log(
                format!(
                    "killed {count} current network connection(s) for '{label}'; future connections remain policy-controlled"
                ),
                false,
            ),
            None => self.push_log(
                format!("no active network listener found for '{label}'"),
                true,
            ),
        }
    }

    pub(crate) fn handle_stop_request(
        &mut self,
        project: &str,
        container_id: &str,
    ) -> ContainerStopDecision {
        let normalized = container_id.trim();
        // Exact-match only: prefix matches let a session-token holder ask us to
        // stop a sibling container in the same workspace by submitting a short
        // common prefix (H19). The session-token check upstream binds the call
        // to its own container; this just enforces that here as well.
        let Some(idx) = self.sessions.iter().position(|session| {
            session.workspace_name == project && session.container_id == normalized
        }) else {
            self.push_log(
                format!(
                    "killme request for workspace '{}' could not find container {}",
                    project, normalized
                ),
                true,
            );
            return ContainerStopDecision::NotFound;
        };

        let label = self.sessions[idx].tab_label();
        if self.sessions[idx].is_exited() {
            self.push_log(
                format!(
                    "killme request for '{}' ignored; container already exited",
                    label
                ),
                false,
            );
            return ContainerStopDecision::Stopped;
        }

        self.push_log(format!("killme requested for '{}'", label), false);
        self.mark_session_stopped(idx);
        if self.active_session == Some(idx) {
            self.active_session = None;
            self.focus = Focus::Sidebar;
        }
        if self.active_network_session == Some(idx) {
            self.active_network_session = None;
            self.network_cursor = 0;
        }
        ContainerStopDecision::Stopped
    }

    pub(crate) fn stop_session_from_tui(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        let label = self.sessions[idx].tab_label();
        if self.sessions[idx].is_exited() {
            self.push_log(format!("container '{}' is already stopped", label), false);
            return;
        }
        self.push_log(format!("stopping container '{}'", label), false);
        self.mark_session_stopped(idx);
    }

    pub(crate) fn container_usage_for_session(
        &mut self,
        session_idx: usize,
    ) -> Option<crate::container::ContainerUsageStats> {
        let docker_name = self.sessions.get(session_idx)?.docker_name.clone();
        let stale_after = std::time::Duration::from_secs(5);

        // `docker stats --no-stream` blocks for 1-2s while the daemon samples
        // CPU, so it must never run on the UI thread (doing so froze the TUI
        // under held-key navigation). Refresh in the background instead and
        // keep showing the last known value until the result arrives.
        let needs_refresh = match self.container_usage.get(&docker_name) {
            Some(cached) => !cached.in_flight && cached.fetched_at.elapsed() >= stale_after,
            None => true,
        };

        if needs_refresh {
            self.container_usage
                .entry(docker_name.clone())
                .and_modify(|cached| cached.in_flight = true)
                .or_insert_with(|| CachedContainerUsage {
                    fetched_at: std::time::Instant::now(),
                    stats: None,
                    in_flight: true,
                });

            let tx = self.background_channels.container_usage_tx.clone();
            let name = docker_name.clone();
            std::thread::spawn(move || {
                let stats = crate::container::inspect_container_usage(&name)
                    .ok()
                    .flatten();
                let _ = tx.blocking_send(ContainerUsageUpdate {
                    docker_name: name,
                    stats,
                });
            });
        }

        self.container_usage
            .get(&docker_name)
            .and_then(|cached| cached.stats.clone())
    }

    pub(crate) fn push_log(&mut self, text: impl Into<String>, is_error: bool) {
        let text = text.into();
        if is_error {
            // The TUI log is bounded and often hidden. Persist failures too so
            // command-line launch errors that point users at the manager log
            // are actionable after the fact.
            tracing::error!(message = %text, "manager TUI error");
        }
        // Drop consecutive duplicates: the same line repeated in a tight loop
        // (e.g. proxy fail-closed retries) otherwise floods useful entries off
        // the visible log within milliseconds (L9). Audit entries are kept
        // unaffected — only `Msg` entries with identical text + severity dedupe.
        if let Some(LogEntry::Msg {
            text: prev_text,
            is_error: prev_is_error,
            ..
        }) = self.log.front()
            && prev_text == &text
            && *prev_is_error == is_error
        {
            return;
        }
        self.log.push_front(LogEntry::Msg {
            text,
            is_error,
            timestamp: chrono::Utc::now(),
        });
        if self.log.len() > 500 {
            self.log.pop_back();
        }
    }

    pub(crate) fn log_workspace_rules_status(&mut self, project: &crate::config::WorkspaceConfig) {
        let rules_path = project.canonical_path.join("harness-rules.toml");
        if !rules_path.exists() {
            self.push_log(
                format!(
                    "Searched for rules at {} but harness-rules.toml was not found",
                    rules_path.display()
                ),
                false,
            );
            return;
        }

        match crate::rules::load(&rules_path) {
            Ok(r) => self.push_log(
                format!(
                    "Loaded rules from {} (network: {})",
                    rules_path.display(),
                    r.network.allowlist.len() + r.network.denylist.len()
                ),
                false,
            ),
            Err(e) => self.push_log(
                format!("Failed loading rules from {}: {}", rules_path.display(), e),
                true,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TuiSessionGroup, append_session_terminal};

    #[test]
    fn appending_session_terminals_keeps_existing_indices() {
        let mut group = TuiSessionGroup {
            workspace_name: "Intent Kit".to_string(),
            workspace_idx: Some(0),
            template_name: "dev".to_string(),
            template_idx: Some(0),
            terminal_indices: Vec::new(),
        };

        append_session_terminal(&mut group, 0);
        append_session_terminal(&mut group, 1);
        append_session_terminal(&mut group, 2);
        append_session_terminal(&mut group, 1);

        assert_eq!(group.terminal_indices, vec![0, 1, 2]);
    }
}

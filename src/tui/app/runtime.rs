use super::*;

impl App {
    const MAX_PENDING_NETWORK_APPROVALS: usize = 64;
    /// Per-source ceiling: a single workspace / source container cannot occupy
    /// more than this many slots in `pending_net`. Without this, one rogue
    /// session-token holder can hold every global slot for the full prompt
    /// timeout (M3). 8 is small enough that other sources still get queued
    /// before the global cap; large enough to absorb a normal burst of
    /// concurrent fetches.
    const MAX_PENDING_NETWORK_APPROVALS_PER_SOURCE: usize = 8;

    pub(crate) fn drain_channels(&mut self) -> bool {
        let mut changed = false;
        let activity_selected_before = {
            let items = self.sidebar_items();
            self.selected_sidebar_item_from(&items)
        };
        let sidebar_idx_before = self.sidebar_idx;
        let log_len_before = self.log.len();
        let activity_len_before = self.activities.len();
        let session_len_before = self.sessions.len();
        let has_dialog_before = self.native_dialog_inflight.is_some();

        for _ in 0..32 {
            match self.exec_pending_rx.try_recv() {
                Ok(mut item) => {
                    let Some(id) = self.allocate_approval_id() else {
                        if let Some(response_tx) = item.response_tx.take() {
                            let _ = response_tx.send(crate::server::ApprovalDecision::Deny);
                        }
                        self.push_log("approval ID space exhausted; denied host command", true);
                        changed = true;
                        continue;
                    };
                    item.id = id;
                    self.pending_exec.push(item);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        for _ in 0..32 {
            match self.stop_pending_rx.try_recv() {
                Ok(item) => {
                    self.pending_stop.push(item);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        // Launch requests reload config + spawn a container; bound the per-tick
        // count so a flood from the control server can't starve the TUI loop.
        for _ in 0..4 {
            match self.launch_pending_rx.try_recv() {
                Ok(item) => {
                    self.handle_workspace_launch_request(item);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        for _ in 0..2 {
            match self.restart_pending_rx.try_recv() {
                Ok(item) => {
                    let _ = item.response_tx.send(self.soft_refresh());
                    changed = true;
                }
                Err(_) => break,
            }
        }
        // Apply any native-dialog decisions that landed since the last tick
        // before enqueueing new requests, so a finished dialog frees the
        // in-flight slot and the next pending item can be prompted this tick.
        self.drain_native_dialog_results();
        for _ in 0..32 {
            match self.net_pending_rx.try_recv() {
                Ok(item) => {
                    self.enqueue_pending_network(item);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        changed |= self.drain_approval_control();
        // Rules-file changes always use a system dialog. Network and hostdo
        // approvals use one in service mode and on macOS; interactive Windows
        // and Linux sessions retain the TUI fallback.
        self.maybe_launch_native_dialog();

        changed |= self.refresh_session_terminal_states();

        for _ in 0..64 {
            match self.background_channels.container_usage_rx.try_recv() {
                Ok(update) => {
                    let needs_redraw = self
                        .container_usage
                        .get(&update.docker_name)
                        .map(|cached| cached.stats != update.stats)
                        .unwrap_or(true);
                    self.container_usage.insert(
                        update.docker_name,
                        CachedContainerUsage {
                            fetched_at: std::time::Instant::now(),
                            stats: update.stats,
                            in_flight: false,
                        },
                    );
                    if needs_redraw {
                        changed = true;
                    }
                }
                Err(_) => break,
            }
        }
        for _ in 0..8 {
            match self.background_channels.rules_scan_rx.try_recv() {
                Ok(stamps) => {
                    self.apply_rules_scan(stamps);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        for _ in 0..128 {
            match self.activity_rx.try_recv() {
                Ok(event) => {
                    self.apply_activity_event(event);
                    changed = true;
                }
                Err(_) => break,
            }
        }

        let items = self.sidebar_items();
        self.restore_sidebar_selection(activity_selected_before.as_ref(), &items);
        changed |= self.sidebar_idx != sidebar_idx_before;

        for _ in 0..32 {
            match self.audit_rx.try_recv() {
                Ok(entry) => {
                    self.log.push_front(LogEntry::Audit(entry));
                    if self.log.len() > 500 {
                        self.log.pop_back();
                    }
                    changed = true;
                }
                Err(_) => break,
            }
        }

        for _ in 0..BUILD_OUTPUT_EVENTS_PER_TICK {
            match self.build_event_rx.try_recv() {
                Ok(BuildEvent::Output { line, is_error }) => {
                    // Mirror to any in-flight `/workspace/launch` streaming
                    // response so the CLI sees the same lines the TUI is
                    // about to render. `try_send` is best-effort — if the CLI
                    // hung up, we keep updating the TUI.
                    if let Some(pending) = &self.workspace_launch_pending {
                        let _ = pending.event_tx.try_send(LaunchEvent::BuildOutput {
                            line: line.clone(),
                            is_error,
                        });
                    }
                    self.push_build_output(line, is_error);
                    changed = true;
                }
                Ok(BuildEvent::Finished {
                    label,
                    launch_workspace_idx,
                    launch_container_idx,
                    launch_session_group,
                    success,
                    cancelled,
                    exit_code,
                    error,
                    diagnostic,
                    log_path,
                    log_error,
                }) => {
                    changed = true;
                    let command = self
                        .build_task
                        .take()
                        .map(|task| task.command_display)
                        .unwrap_or_default();
                    if let Some(error) = error {
                        self.pending_force_rebuild = false;
                        self.push_log(format!("{label} failed: {error}"), true);
                        if let Some(diagnostic) = &diagnostic {
                            self.push_log(format!("  build detail: {diagnostic}"), true);
                        }
                        // Prefer the spawn error as the headline diagnostic.
                        let diagnostic_for_state = Some(error.clone());
                        self.build_finished = Some(BuildFinished {
                            command,
                            cancelled: false,
                            exit_code,
                            diagnostic: diagnostic_for_state,
                            log_path,
                            log_error,
                        });
                        self.focus = Focus::ImageBuild;
                        if let Some(pending) = self.workspace_launch_pending.take() {
                            crate::tui::app::launch::finish_launch_stream(
                                &pending.event_tx,
                                Err(format!("docker build failed: {error}")),
                            );
                        }
                        continue;
                    }
                    if cancelled {
                        self.build_workspace_idx = None;
                        self.build_session_group = None;
                        self.pending_force_rebuild = false;
                        self.push_log(format!("{label} cancelled"), true);
                        self.build_finished = Some(BuildFinished {
                            command,
                            cancelled: true,
                            exit_code,
                            diagnostic,
                            log_path,
                            log_error,
                        });
                        self.focus = Focus::ImageBuild;
                        if let Some(pending) = self.workspace_launch_pending.take() {
                            crate::tui::app::launch::finish_launch_stream(
                                &pending.event_tx,
                                Err("docker build was cancelled in the manager TUI".to_string()),
                            );
                        }
                        continue;
                    }
                    if success {
                        self.build_workspace_idx = None;
                        self.build_session_group = None;
                        self.pending_force_rebuild = false;
                        self.build_finished = None;
                        self.push_log(format!("{label} finished successfully"), false);
                        self.build_container_idx = None;
                        let pending = self.workspace_launch_pending.take();
                        if let Some(pending) = pending {
                            let _ = pending.event_tx.try_send(LaunchEvent::Status {
                                message: format!(
                                    "build finished — launching {} on {}",
                                    pending.template, pending.workspace_name,
                                ),
                            });
                            let outcome = self.do_launch_for_workspace_endpoint(
                                pending.workspace_idx,
                                pending.template_idx,
                                &pending.workspace_name,
                                &pending.template,
                                pending.cwd.clone(),
                                &pending.terminal_env,
                            );
                            crate::tui::app::launch::finish_launch_stream(
                                &pending.event_tx,
                                outcome,
                            );
                        } else {
                            self.do_launch_container_on_workspace_with_priority(
                                launch_workspace_idx,
                                launch_container_idx,
                                crate::proxy::SourcePriority::Primary,
                                launch_session_group,
                            );
                        }
                    } else {
                        let suffix = exit_code
                            .map(|code| format!(" (exit code {code})"))
                            .unwrap_or_default();
                        self.pending_force_rebuild = false;
                        self.push_log(format!("{label} failed{suffix}"), true);
                        if let Some(diagnostic) = &diagnostic {
                            self.push_log(format!("  build detail: {diagnostic}"), true);
                        }
                        let diagnostic_for_stream = diagnostic.clone();
                        self.build_finished = Some(BuildFinished {
                            command,
                            cancelled: false,
                            exit_code,
                            diagnostic,
                            log_path,
                            log_error,
                        });
                        self.focus = Focus::ImageBuild;
                        if let Some(pending) = self.workspace_launch_pending.take() {
                            let reason = match (exit_code, diagnostic_for_stream) {
                                (Some(code), Some(diag)) => {
                                    format!("docker build failed (exit code {code}): {diag}")
                                }
                                (Some(code), None) => {
                                    format!("docker build failed (exit code {code})")
                                }
                                (None, Some(diag)) => format!("docker build failed: {diag}"),
                                (None, None) => "docker build failed".to_string(),
                            };
                            crate::tui::app::launch::finish_launch_stream(
                                &pending.event_tx,
                                Err(reason),
                            );
                        }
                    }
                }
                Err(_) => break,
            }
        }

        for i in (0..self.sessions.len()).rev() {
            // The embedded terminal owns a `docker run -it` client. That
            // client can disconnect while its container survives, so an
            // Alacritty PTY exit is only a signal to inspect Docker, not proof
            // that the session itself ended.
            if self.sessions[i].pty_exited()
                && !self.sessions[i].is_exited()
                && !self.sessions[i].is_terminal_detached()
                && (!self.sessions[i].pty_exit_reported
                    || self.sessions[i].last_container_state_check.elapsed()
                        >= std::time::Duration::from_secs(2))
            {
                self.sessions[i].last_container_state_check = std::time::Instant::now();
                let label = self.sessions[i].tab_label();
                let docker_name = self.sessions[i].docker_name.clone();
                match crate::container::inspect_container_state(&docker_name) {
                    Ok(Some(state)) if state.running => {
                        self.sessions[i].mark_terminal_detached();
                        self.push_log(
                            format!(
                                "terminal connection for '{label}' closed; container is still running (reconnect with {})",
                                self.sessions[i].shell_in_hint()
                            ),
                            true,
                        );
                        changed = true;
                        continue;
                    }
                    Ok(Some(_)) | Ok(None) => {
                        self.sessions[i]
                            .exited
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(error) => {
                        if !self.sessions[i].pty_exit_reported {
                            self.sessions[i].pty_exit_reported = true;
                            self.push_log(
                                format!(
                                    "terminal connection for '{label}' closed; could not verify container state yet: {error}"
                                ),
                                true,
                            );
                            changed = true;
                        }
                        continue;
                    }
                }
            }

            // The PTY event loop normally reports when `docker run` exits.
            // On Windows, a client can occasionally terminate without that
            // event reaching us. Reconcile all live sessions at a low rate so
            // a `docker run --rm` container that has already disappeared
            // cannot remain as a false running session in the TUI.
            if !self.sessions[i].is_exited()
                && self.sessions[i].last_container_state_check.elapsed()
                    >= std::time::Duration::from_secs(2)
            {
                self.sessions[i].last_container_state_check = std::time::Instant::now();
                let docker_name = self.sessions[i].docker_name.clone();
                match crate::container::inspect_container_state(&docker_name) {
                    Ok(Some(state)) if state.running => {}
                    Ok(Some(_)) | Ok(None) => {
                        self.sessions[i]
                            .exited
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Keep the session while Docker is temporarily unavailable;
                    // a failed inspection must not be treated as an exit.
                    Err(_) => {}
                }
            }

            if self.sessions[i].desktop_mode
                && !self.sessions[i].is_exited()
                && self.sessions[i].last_desktop_ssh_check.elapsed()
                    >= crate::desktop::SSH_CONNECTION_POLL_INTERVAL
            {
                self.sessions[i].last_desktop_ssh_check = std::time::Instant::now();
                let docker_name = self.sessions[i].docker_name.clone();
                match crate::desktop::ssh_connected(&docker_name) {
                    Ok(true) => {
                        self.sessions[i].desktop_ssh_ever_connected = true;
                        self.sessions[i].desktop_ssh_disconnected_at = None;
                    }
                    Ok(false) => {
                        let launched_at = self.sessions[i].launched_at;
                        let disconnected_at = *self.sessions[i]
                            .desktop_ssh_disconnected_at
                            .get_or_insert(launched_at);
                        let grace = if self.sessions[i].desktop_ssh_ever_connected {
                            crate::desktop::SSH_DISCONNECT_GRACE
                        } else {
                            crate::desktop::SSH_INITIAL_CONNECTION_GRACE
                        };
                        if disconnected_at.elapsed() >= grace {
                            let label = self.sessions[i].tab_label();
                            self.push_log(
                                format!(
                                    "stopping Desktop container '{label}' after {} minutes without an SSH connection",
                                    grace.as_secs() / 60
                                ),
                                false,
                            );
                            self.mark_session_stopped(i);
                            changed = true;
                        }
                    }
                    // Docker can be briefly unavailable during an engine
                    // restart. A failed probe must never advance the cleanup
                    // timer or be treated as a disconnect.
                    Err(_) => {}
                }
            }

            if !self.sessions[i].is_exited() {
                continue;
            }
            let exited_for = self.sessions[i].launched_at.elapsed();
            if !self.sessions[i].exit_reported {
                changed = true;
                self.sessions[i].exit_reported = true;
                let label = self.sessions[i].tab_label();
                match crate::container::inspect_container_state(&self.sessions[i].docker_name) {
                    Ok(Some(state)) => {
                        let suffix = state
                            .exit_code
                            .map(|code| format!(" (exit code {code})"))
                            .unwrap_or_default();
                        if state.error.is_empty() {
                            self.push_log(format!("{label} exited immediately{suffix}"), true);
                        } else {
                            self.push_log(
                                format!("{label} exited immediately{suffix}: {}", state.error),
                                true,
                            );
                        }
                    }
                    Ok(None) => {
                        self.push_log(format!("{label} exited immediately"), true);
                    }
                    Err(e) => {
                        self.push_log(
                            format!(
                                "{label} exited immediately; failed to inspect exit status: {e}"
                            ),
                            true,
                        );
                    }
                }
                continue;
            }
            if exited_for < std::time::Duration::from_secs(15) {
                continue;
            }
            changed = true;
            let label = self.sessions[i].tab_label();
            self.push_log(format!("container '{}' exited", label), false);
            self.close_session(i);
            if self.active_session.is_none() && self.focus != Focus::ImageBuild {
                self.focus = Focus::Sidebar;
            }
        }

        for idx in (0..self.pending_stop.len()).rev() {
            let Some((project, container_id)) = self
                .pending_stop
                .get(idx)
                .map(|item| (item.workspace_name.clone(), item.container_id.clone()))
            else {
                continue;
            };
            let decision = self.handle_stop_request(&project, &container_id);
            if let Some(tx) = self.pending_stop[idx].response_tx.take() {
                let _ = tx.send(decision);
            }
            self.pending_stop.remove(idx);
            changed = true;
        }

        if self.focus == Focus::Terminal {
            if let Some(si) = self.active_session {
                if let Some(session) = self.sessions.get(si) {
                    session.clear_bell();
                }
            }
        }

        let selected_before_prune = {
            let items = self.sidebar_items();
            self.selected_sidebar_item_from(&items)
        };
        self.prune_terminal_activities();
        let items = self.sidebar_items();
        self.restore_sidebar_selection(selected_before_prune.as_ref(), &items);

        changed |= self.log.len() != log_len_before;
        changed |= self.activities.len() != activity_len_before;
        changed |= self.sessions.len() != session_len_before;
        changed |= has_dialog_before != self.native_dialog_inflight.is_some();
        changed
    }

    pub(crate) fn enqueue_pending_network(&mut self, mut item: crate::proxy::PendingNetworkItem) {
        if let Some(existing) = self
            .pending_net
            .iter_mut()
            .find(|pending| pending_network_merge_key_matches(pending, &item))
        {
            existing.merged_response_txs.push(item.response_tx);
            return;
        }

        // Per-source quota check (M3). "Source" = `source_workspace` when set,
        // otherwise the source container's identity — that's the strongest
        // attribution we have for unauthenticated proxy clients. Items with no
        // source at all share a single bucket so an attacker can't bypass the
        // cap by simply omitting attribution.
        let source_key = pending_network_source_key(&item);
        let per_source_count = self
            .pending_net
            .iter()
            .filter(|pending| pending_network_source_key(pending) == source_key)
            .count();
        if per_source_count >= Self::MAX_PENDING_NETWORK_APPROVALS_PER_SOURCE {
            return self.reject_pending_network_overflow(
                item,
                "too many pending network approvals from this source",
            );
        }

        if self.pending_net.len() < Self::MAX_PENDING_NETWORK_APPROVALS {
            let Some(id) = self.allocate_approval_id() else {
                return self.reject_pending_network_overflow(item, "approval ID space exhausted");
            };
            item.approval_id = id;
            self.pending_net.push(item);
            return;
        }

        self.reject_pending_network_overflow(item, "too many pending network approvals");
    }

    fn reject_pending_network_overflow(
        &mut self,
        item: crate::proxy::PendingNetworkItem,
        reason: &'static str,
    ) {
        let activity_id = item.activity_id.clone();
        crate::tui::app::approvals::send_pending_network_decision_owned(
            item,
            crate::proxy::NetworkDecision::Deny,
        );
        if let Some(activity) = self.activity_by_id_mut(&activity_id) {
            activity.state = crate::activity::ActivityState::Denied;
            activity.status = Some(reason.to_string());
            activity.updated_at = std::time::Instant::now();
            activity.finished_at = Some(activity.updated_at);
            activity.terminal_unselected_at = None;
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.focus, Focus::Terminal | Focus::Activity) {
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_mode = true;
                self.terminal_scroll = self.terminal_scroll.saturating_add(3);
                return;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_mode = true;
                self.terminal_scroll = self.terminal_scroll.saturating_sub(3);
                return;
            }
            _ => {}
        }

        let area = self.terminal_selection_area;
        let mut dragging = self.selection_dragging;
        match self.focus {
            Focus::Terminal => {
                let Some(session_idx) = self.active_session else {
                    return;
                };
                let Some(session) = self.sessions.get_mut(session_idx) else {
                    return;
                };
                let mut term = session.term.lock();
                Self::handle_terminal_selection_mouse(&mut *term, area, &mut dragging, mouse);
            }
            Focus::Activity => {
                let Some(activity_id) = self.active_activity.clone() else {
                    return;
                };
                let Some(activity) = self.activity_by_id_mut(&activity_id) else {
                    return;
                };
                let mut term = activity.terminal.term.lock();
                Self::handle_terminal_selection_mouse(&mut *term, area, &mut dragging, mouse);
            }
            _ => {}
        }
        self.selection_dragging = dragging;
    }

    fn handle_terminal_selection_mouse<T: alacritty_terminal::event::EventListener>(
        term: &mut alacritty_terminal::term::Term<T>,
        area: Option<ratatui::layout::Rect>,
        dragging: &mut bool,
        mouse: MouseEvent,
    ) {
        let point = area.and_then(|area| {
            if mouse.column < area.x
                || mouse.row < area.y
                || mouse.column >= area.right()
                || mouse.row >= area.bottom()
            {
                return None;
            }
            let viewport = alacritty_terminal::index::Point::new(
                usize::from(mouse.row - area.y),
                alacritty_terminal::index::Column::from(usize::from(mouse.column - area.x)),
            );
            Some(alacritty_terminal::term::viewport_to_point(
                term.grid().display_offset(),
                viewport,
            ))
        });

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(point) = point {
                    term.selection = Some(alacritty_terminal::selection::Selection::new(
                        alacritty_terminal::selection::SelectionType::Simple,
                        point,
                        alacritty_terminal::index::Side::Left,
                    ));
                    *dragging = true;
                } else {
                    term.selection = None;
                    *dragging = false;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                if *dragging =>
            {
                if let Some(point) = point
                    && let Some(selection) = term.selection.as_mut()
                {
                    selection.update(point, alacritty_terminal::index::Side::Right);
                }
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
                    *dragging = false;
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                term.selection = None;
                *dragging = false;
            }
            _ => {}
        }
    }

    pub(crate) fn copy_terminal_selection(&mut self) -> bool {
        let selected = match self.focus {
            Focus::Terminal => self
                .active_session
                .and_then(|idx| self.sessions.get(idx))
                .and_then(|session| session.term.lock().selection_to_string()),
            Focus::Activity => self
                .active_activity
                .as_deref()
                .and_then(|id| self.activity_by_id(id))
                .and_then(|activity| activity.terminal.term.lock().selection_to_string()),
            _ => None,
        };

        let Some(selected) = selected else {
            return false;
        };
        self.pending_clipboard = Some(selected);
        true
    }

    pub(crate) fn take_clipboard_text(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }
}

fn pending_network_merge_key_matches(
    left: &crate::proxy::PendingNetworkItem,
    right: &crate::proxy::PendingNetworkItem,
) -> bool {
    left.source_workspace == right.source_workspace
        && left.method.eq_ignore_ascii_case(&right.method)
        && left.host.eq_ignore_ascii_case(&right.host)
        && left.port == right.port
        && left.path == right.path
}

/// Stable key used to count "how many pending approvals does this source
/// already hold." Prefers an explicit `source_workspace`; falls back to the
/// source container identity, then to a shared "unknown" bucket so requests
/// with no attribution can't dodge the quota by being anonymous (M3).
fn pending_network_source_key(item: &crate::proxy::PendingNetworkItem) -> String {
    if let Some(project) = item.source_workspace.as_deref() {
        return format!("workspace:{project}");
    }
    if let Some(container) = item.source_container.as_deref() {
        return format!("container:{container}");
    }
    "unknown".to_string()
}

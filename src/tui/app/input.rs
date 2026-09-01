use super::*;

impl App {
    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if crate::tui::is_copy_key(&key) {
            self.copy_terminal_selection();
            return;
        }

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.focus == Focus::Activity
                && let Some(id) = self.active_activity.clone()
            {
                self.cancel_activity(&id);
                return;
            }
            if self.focus == Focus::Network
                && let Some(si) = self.active_network_session
                && let Some(id) = self.selected_network_activity_id(si)
            {
                self.cancel_activity(&id);
                return;
            }
            if self.build_is_running() {
                self.cancel_docker_build();
                return;
            }
            self.should_quit = true;
            return;
        }

        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }

        if self.remove_workspace_confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.finish_remove_workspace_confirm(true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.finish_remove_workspace_confirm(false)
                }
                _ => {}
            }
            return;
        }

        if self.headless_rules_overlay_visible() {
            match key.code {
                KeyCode::Char('t')
                | KeyCode::Char('T')
                | KeyCode::Char('y')
                | KeyCode::Char('Y') => {
                    let _ = self.trust_changed_rules();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.dismiss_changed_rules();
                }
                _ => {}
            }
            return;
        }

        if matches!(self.focus, Focus::Activity | Focus::Network)
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('b')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            if self.focus == Focus::Activity && self.scroll_mode && key.code == KeyCode::Esc {
                // In activity scroll mode, Esc should first exit scroll mode just
                // like terminal scroll mode.
            } else {
                self.focus_sidebar_shortcut();
                return;
            }
        }

        if self.active_exec_modal_idx().is_some() && !self.native_dialog_enabled() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.approve_exec(0, false),
                KeyCode::Char('r') | KeyCode::Char('R') => self.approve_exec(0, true),
                KeyCode::Char('n') | KeyCode::Char('N') => self.deny_exec(0),
                KeyCode::Char('d') | KeyCode::Char('D') => self.deny_exec_forever(0),
                _ => {}
            }
            return;
        }

        if !self.pending_net.is_empty() && !self.native_dialog_enabled() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.approve_net(0),
                KeyCode::Char('r') | KeyCode::Char('R') => self.approve_net_forever(0),
                KeyCode::Char('n') | KeyCode::Char('N') => self.deny_net(0),
                KeyCode::Char('d') | KeyCode::Char('D') => self.deny_net_forever(0),
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    let session_token = self.pending_net[0].source_session_token.clone();
                    if let Some(session_idx) = session_token.and_then(|token| {
                        self.sessions
                            .iter()
                            .position(|session| session.session_token == token)
                    }) {
                        self.kill_network_connections_for_session(session_idx);
                    }
                }
                _ => {}
            }
            return;
        }

        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.focus_sidebar_shortcut();
            return;
        }

        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.terminal_fullscreen {
                self.close_terminal_fullscreen();
            } else if self.focus == Focus::Terminal && self.active_session.is_some() {
                self.open_terminal_fullscreen();
            }
            return;
        }

        if self.log_fullscreen {
            match key.code {
                KeyCode::Char('o') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.log_fullscreen = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.log_scroll = self.log_scroll.saturating_add(1);
                }
                _ => {}
            }
            return;
        }

        match self.focus {
            Focus::Sidebar => self.handle_sidebar_key(key),
            Focus::Terminal => self.handle_terminal_key(key),
            Focus::Activity => self.handle_activity_key(key),
            Focus::Network => self.handle_network_key(key),
            Focus::Settings => self.handle_settings_key(key),
            Focus::ContainerPicker => self.handle_picker_key(key),
            Focus::ImageBuild => self.handle_build_key(key),
            Focus::NewWorkspace => self.handle_new_workspace_key(key),
            Focus::WorkspaceActions => self.handle_workspace_action_key(key),
        }
    }

    pub(crate) fn focus_sidebar_shortcut(&mut self) {
        self.last_terminal_esc = None;
        self.log_fullscreen = false;
        self.terminal_fullscreen = false;
        match self.focus {
            Focus::Sidebar => {}
            Focus::Terminal => {
                self.focus = Focus::Sidebar;
            }
            Focus::Activity => {
                self.active_activity = None;
                self.focus = Focus::Sidebar;
            }
            Focus::Network => {
                self.active_network_session = None;
                self.network_cursor = 0;
                self.focus = Focus::Sidebar;
            }
            Focus::Settings => {
                self.remove_workspace_confirm = None;
                self.active_settings_workspace = None;
                self.focus = Focus::Sidebar;
            }
            Focus::ContainerPicker => {
                self.container_picker = None;
                self.focus = Focus::Sidebar;
            }
            Focus::ImageBuild => {
                if self.build_is_running() {
                    self.focus = Focus::Sidebar;
                } else {
                    self.build_finished = None;
                    self.build_container_idx = None;
                    self.build_workspace_idx = None;
                    self.build_session_group = None;
                    self.focus = Focus::Sidebar;
                }
            }
            Focus::NewWorkspace => {
                self.new_workspace = None;
                self.focus = Focus::Sidebar;
            }
            Focus::WorkspaceActions => {
                self.workspace_action_workspace = None;
                self.workspace_action_cursor = 0;
                self.focus = Focus::Sidebar;
            }
        }
        let items = self.sidebar_items();
        self.update_sidebar_preview(&items);
    }

    pub(crate) fn open_log_fullscreen(&mut self) {
        self.terminal_fullscreen = false;
        self.log_fullscreen = true;
    }

    pub(crate) fn open_terminal_fullscreen(&mut self) {
        self.log_fullscreen = false;
        self.terminal_fullscreen = true;
        self.last_terminal_esc = None;
    }

    pub(crate) fn close_terminal_fullscreen(&mut self) {
        self.terminal_fullscreen = false;
        self.last_terminal_esc = None;
    }

    pub(crate) fn handle_sidebar_key(&mut self, key: KeyEvent) {
        let items = self.sidebar_items();

        if !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && let KeyCode::Char(ch) = key.code
            && let Some(idx) = self.sidebar_workspace_hotkey_target(ch.to_ascii_lowercase())
        {
            self.sidebar_idx = idx;
            self.update_sidebar_preview(&items);
            self.ensure_sidebar_visible(&items, 10);
            return;
        }
        match key.code {
            KeyCode::Up => {
                self.sidebar_move_up(&items);
            }
            KeyCode::Down => {
                self.sidebar_move_down(&items);
            }
            KeyCode::Enter => self.handle_sidebar_enter(&items),
            KeyCode::Char('d') | KeyCode::Char('D')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Delete-from-sidebar shortcut: Ctrl (not a bare letter) so it
                // never collides with a workspace's assigned jump-to hotkey,
                // which only matches an unmodified Char press (see above).
                // Works on both the not-yet-running Launch row and a running
                // Session row, so removal doesn't require starting a session
                // first — unlike the Settings-pane `x` route, which only
                // appears once a workspace has a session group.
                if let Some(pi) = self.sidebar_workspace_idx_for_delete(&items) {
                    self.prompt_remove_workspace(pi);
                }
            }
            _ => {}
        }

        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::ALT) {
            self.open_log_fullscreen();
        }
    }

    pub(crate) fn sidebar_move_up(&mut self, items: &[SidebarItem]) {
        self.sidebar_move_to_next_selectable(items, -1);
        self.update_sidebar_preview(items);
        self.ensure_sidebar_visible(items, 10); // Default height
    }

    pub(crate) fn sidebar_move_down(&mut self, items: &[SidebarItem]) {
        self.sidebar_move_to_next_selectable(items, 1);
        self.update_sidebar_preview(items);
        self.ensure_sidebar_visible(items, 10); // Default height
    }

    pub(crate) fn sidebar_move_to_next_selectable(&mut self, items: &[SidebarItem], dir: i8) {
        if items.is_empty() {
            return;
        }

        let len = items.len();
        let mut idx = self.sidebar_idx.min(len.saturating_sub(1));

        // Move at least one step, then keep stepping until we find a selectable row.
        for _ in 0..len {
            idx = if dir < 0 {
                if idx == 0 { len - 1 } else { idx - 1 }
            } else if idx >= len - 1 {
                0
            } else {
                idx + 1
            };

            if Self::sidebar_item_is_selectable(&items[idx]) {
                self.sidebar_idx = idx;
                return;
            }
        }
        // Degenerate case: everything is non-selectable (shouldn't happen).
        self.sidebar_idx = 0;
    }

    pub(crate) fn ensure_sidebar_visible(&mut self, items: &[SidebarItem], visible_height: usize) {
        if items.is_empty() || visible_height == 0 {
            return;
        }
        if self.sidebar_idx <= 1 {
            // Keep the first workspace title row visible when the first
            // selectable row is focused.
            self.sidebar_offset = 0;
            return;
        }
        if self.sidebar_idx < self.sidebar_offset {
            self.sidebar_offset = self.sidebar_idx;
        } else if self.sidebar_idx >= self.sidebar_offset + visible_height {
            self.sidebar_offset = self.sidebar_idx - visible_height + 1;
        }
    }

    pub(crate) fn update_sidebar_preview(&mut self, items: &[SidebarItem]) {
        self.preview_session = match items.get(self.sidebar_idx) {
            Some(SidebarItem::Session(si)) => self.first_terminal_for_session_group(*si),
            Some(SidebarItem::SessionTerminal(group_idx, session_pos)) => {
                self.session_group_terminal(*group_idx, *session_pos)
            }
            Some(SidebarItem::NetworkGroup(si)) => Some(*si),
            Some(SidebarItem::Activity(id)) => self.session_for_activity(id),
            _ => None,
        };
    }

    pub(crate) fn handle_sidebar_enter(&mut self, items: &[SidebarItem]) {
        match items.get(self.sidebar_idx).cloned() {
            Some(SidebarItem::NewSession) => self.open_picker(),
            Some(SidebarItem::Settings(pi)) => {
                self.active_settings_workspace = Some(pi);
                self.active_activity = None;
                self.active_network_session = None;
                self.settings_cursor = 0;
                self.focus = Focus::Settings;
            }
            Some(SidebarItem::Launch(_)) => {
                self.workspace_action_workspace =
                    items.get(self.sidebar_idx).and_then(|item| match item {
                        SidebarItem::Launch(pi) => Some(*pi),
                        _ => None,
                    });
                self.workspace_action_cursor = 0;
                self.active_activity = None;
                self.active_network_session = None;
                self.focus = Focus::WorkspaceActions;
            }
            Some(SidebarItem::Build(_)) => {
                self.active_session = None;
                self.active_activity = None;
                self.active_network_session = None;
                self.focus = Focus::ImageBuild;
                self.active_settings_workspace = None;
            }
            Some(SidebarItem::Session(si)) => {
                if let Some(session_idx) = self.first_terminal_for_session_group(si) {
                    if let Some(session) = self.sessions.get(session_idx) {
                        session.clear_bell();
                    }
                    self.active_session = Some(session_idx);
                    self.preview_session = Some(session_idx);
                    self.scroll_mode = false;
                    self.terminal_scroll = 0;
                    self.focus = Focus::Terminal;
                    self.active_activity = None;
                    self.active_network_session = None;
                    self.active_settings_workspace = None;
                }
            }
            Some(SidebarItem::SessionTerminal(group_idx, session_pos)) => {
                let Some(session_idx) = self.session_group_terminal(group_idx, session_pos) else {
                    return;
                };
                if let Some(session) = self.sessions.get(session_idx) {
                    session.clear_bell();
                }
                self.active_session = Some(session_idx);
                self.preview_session = Some(session_idx);
                self.scroll_mode = false;
                self.terminal_scroll = 0;
                self.focus = Focus::Terminal;
                self.active_activity = None;
                self.active_network_session = None;
                self.active_settings_workspace = None;
            }
            Some(SidebarItem::NetworkGroup(si)) => {
                self.active_session = Some(si);
                self.preview_session = Some(si);
                self.active_activity = None;
                self.network_cursor = self.network_cursor_for_session(si);
                self.active_network_session = Some(si);
                self.focus = Focus::Network;
                self.active_settings_workspace = None;
            }
            Some(SidebarItem::Activity(id)) => {
                self.active_session = self.session_for_activity(&id);
                self.preview_session = self.active_session;
                self.active_activity = Some(id);
                self.active_network_session = None;
                self.scroll_mode = false;
                self.terminal_scroll = 0;
                self.focus = Focus::Activity;
                self.active_settings_workspace = None;
            }
            Some(SidebarItem::NewWorkspace) => self.open_new_workspace(),
            // Non-selectable; navigation never lands here.
            Some(SidebarItem::WorkspacesHeader) => {}
            None => {}
        }
    }

    pub(crate) fn handle_workspace_action_key(&mut self, key: KeyEvent) {
        let Some(pi) = self.workspace_action_workspace else {
            self.focus = Focus::Sidebar;
            return;
        };

        let actions = self.workspace_action_rows(pi);
        if actions.is_empty() {
            self.workspace_action_workspace = None;
            self.focus = Focus::Sidebar;
            return;
        }

        if self.workspace_action_cursor >= actions.len() {
            self.workspace_action_cursor = actions.len().saturating_sub(1);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('h') => {
                self.workspace_action_workspace = None;
                self.workspace_action_cursor = 0;
                self.focus = Focus::Sidebar;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.workspace_action_cursor > 0 {
                    self.workspace_action_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.workspace_action_cursor + 1 < actions.len() {
                    self.workspace_action_cursor += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('L') => {
                self.run_workspace_action(pi);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if let Some(position) = actions
                    .iter()
                    .position(|row| row.key == 'r' || row.key == 'R')
                {
                    self.workspace_action_cursor = position;
                    self.run_workspace_action(pi);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn run_workspace_action(&mut self, workspace_idx: usize) {
        let actions = self.workspace_action_rows(workspace_idx);
        let Some(action) = actions.get(self.workspace_action_cursor).copied() else {
            self.workspace_action_workspace = None;
            self.workspace_action_cursor = 0;
            self.focus = Focus::Sidebar;
            return;
        };

        match action.action {
            WorkspaceAction::LaunchWorkspace => {
                if self.open_template_picker_for_workspace(workspace_idx) {
                    self.workspace_action_workspace = None;
                    self.workspace_action_cursor = 0;
                    self.focus = Focus::ContainerPicker;
                }
            }
            WorkspaceAction::RemoveWorkspace => {
                self.workspace_action_workspace = None;
                self.workspace_action_cursor = 0;
                self.focus = Focus::Sidebar;
                self.prompt_remove_workspace(workspace_idx);
            }
        }
    }

    pub(crate) fn handle_activity_key(&mut self, key: KeyEvent) {
        if self.scroll_mode {
            self.handle_scroll_mode_key(key);
            return;
        }

        if is_scroll_mode_toggle_key(key) {
            self.scroll_mode = true;
            return;
        }

        if key.code == KeyCode::Esc {
            self.focus_sidebar_shortcut();
        }
    }

    pub(crate) fn handle_network_key(&mut self, key: KeyEvent) {
        let Some(si) = self.active_network_session else {
            self.focus_sidebar_shortcut();
            return;
        };
        let len = self.network_activity_count_for_session(si);
        match key.code {
            KeyCode::Esc => self.focus_sidebar_shortcut(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.network_cursor = self.network_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    self.network_cursor = (self.network_cursor + 1).min(len.saturating_sub(1));
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(id) = self.selected_network_activity_id(si) {
                    self.cancel_activity(&id);
                }
            }
            KeyCode::Char('x') | KeyCode::Char('X') => {
                self.kill_network_connections_for_session(si);
            }
            _ => {}
        }
    }

    const NEW_PROJECT_ROW_COUNT: usize = 5;

    pub(crate) fn open_new_workspace(&mut self) {
        // Prefer the attached client's reported cwd over this process's own:
        // when daemon-owned (service_mode), `std::env::current_dir()` reads
        // the background service's cwd, which is meaningless (see
        // `remote_client_cwd`'s doc comment). Falls back to this process's
        // own cwd, which is correct when there's no attached remote client.
        let cwd_path = self
            .remote_client_cwd
            .as_ref()
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        let cwd = cwd_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let default_name = cwd_path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        self.new_workspace = Some(NewWorkspaceState {
            cursor: 0,
            name: default_name,
            workspace_dir: cwd,
            project_type: crate::new_project::ProjectType::None,
            error: None,
        });
        self.focus = Focus::NewWorkspace;
        self.active_session = None;
        self.active_activity = None;
        self.active_network_session = None;
        self.active_settings_workspace = None;
        self.container_picker = None;
    }

    pub(crate) fn handle_new_workspace_key(&mut self, key: KeyEvent) {
        let Some(state) = self.new_workspace.as_mut() else {
            self.focus = Focus::Sidebar;
            return;
        };

        if matches!(state.cursor, 0 | 1)
            && let KeyCode::Char(c) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.append_new_workspace_text(&c.to_string());
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.new_workspace = None;
                self.focus = Focus::Sidebar;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.cursor = state.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                state.cursor = (state.cursor + 1).min(Self::NEW_PROJECT_ROW_COUNT - 1);
            }
            KeyCode::Left => {
                if state.cursor == 2 {
                    state.project_type = state.project_type.prev();
                }
            }
            KeyCode::Right => {
                if state.cursor == 2 {
                    state.project_type = state.project_type.next();
                }
            }
            KeyCode::Backspace => match state.cursor {
                0 => {
                    state.name.pop();
                }
                1 => {
                    state.workspace_dir.pop();
                }
                _ => {}
            },
            KeyCode::Enter => match state.cursor {
                2 => state.project_type = state.project_type.next(),
                3 => self.submit_new_workspace(),
                4 => {
                    self.new_workspace = None;
                    self.focus = Focus::Sidebar;
                }
                _ => {}
            },
            _ => {}
        }
    }

    pub(crate) fn append_new_workspace_text(&mut self, text: &str) {
        let Some(state) = self.new_workspace.as_mut() else {
            return;
        };
        let cleaned = text.replace(['\r', '\n'], "");
        if cleaned.is_empty() {
            return;
        }
        match state.cursor {
            0 => state.name.push_str(&cleaned),
            1 => state.workspace_dir.push_str(&cleaned),
            _ => {}
        }
    }

    pub(crate) fn append_remote_paste(&mut self, text: &str) {
        if self.focus == Focus::NewWorkspace {
            self.append_new_workspace_text(text);
        } else if let Some(session_idx) = self.active_session
            && let Some(session) = self.sessions.get(session_idx)
        {
            session.send_input(text.as_bytes().to_vec());
        }
    }

    pub(crate) fn submit_new_workspace(&mut self) {
        let Some((name, workspace_raw, project_type)) = self.new_workspace.as_mut().map(|state| {
            state.error = None;
            (
                state.name.trim().to_string(),
                state.workspace_dir.trim().to_string(),
                state.project_type,
            )
        }) else {
            return;
        };

        if name.is_empty() {
            self.set_new_workspace_error("workspace name is required".to_string());
            return;
        }
        if workspace_raw.is_empty() {
            self.set_new_workspace_error("workspace dir is required".to_string());
            return;
        }

        let workspace_path = match crate::config::expand_path(std::path::Path::new(&workspace_raw))
        {
            Ok(p) => p,
            Err(e) => {
                self.set_new_workspace_error(format!("workspace dir is invalid: {e}"));
                return;
            }
        };
        if !workspace_path.exists() {
            self.set_new_workspace_error(format!(
                "workspace dir does not exist: {}",
                workspace_path.display()
            ));
            return;
        }
        if !workspace_path.is_dir() {
            self.set_new_workspace_error(format!(
                "workspace dir is not a directory: {}",
                workspace_path.display()
            ));
            return;
        }

        let cfg = self.config.get();
        if cfg.workspaces.iter().any(|p| p.name == name) {
            self.set_new_workspace_error(format!("workspace name already exists: '{name}'"));
            return;
        }
        let sidebar_hotkey = crate::config::select_workspace_sidebar_hotkey(&cfg.workspaces, &name);

        match crate::new_project::write_rules_if_missing(&workspace_path, project_type) {
            Ok(false) => {}
            Ok(true) => self.push_log(
                format!(
                    "created {}",
                    workspace_path.join("harness-rules.toml").display()
                ),
                false,
            ),
            Err(e) => {
                self.set_new_workspace_error(format!("failed writing harness-rules.toml: {e}"));
                return;
            }
        };

        if let Err(e) = crate::new_project::append_project_block(
            &self.loaded_config_path,
            &name,
            &workspace_path,
            sidebar_hotkey,
        ) {
            self.set_new_workspace_error(format!("failed updating config: {e}"));
            return;
        }

        let new_config = match crate::config::load(&self.loaded_config_path) {
            Ok(c) => c,
            Err(e) => {
                self.set_new_workspace_error(format!("config reload failed: {e}"));
                return;
            }
        };
        let new_pi = new_config.workspaces.iter().position(|p| p.name == name);
        self.config.set(std::sync::Arc::new(new_config));
        self.refresh_workspaces_cache();

        let hotkey_note = sidebar_hotkey
            .map(|ch| format!(" with sidebar hotkey {}", ch.to_ascii_uppercase()))
            .unwrap_or_default();
        self.push_log(format!("added workspace '{name}'{hotkey_note}"), false);
        self.new_workspace = None;
        self.focus = Focus::Sidebar;

        if let Some(pi) = new_pi {
            if let Some(pos) = self
                .sidebar_items()
                .iter()
                .position(|item| *item == SidebarItem::Launch(pi))
            {
                self.sidebar_idx = pos;
            }
        }
    }

    pub(crate) fn set_new_workspace_error(&mut self, msg: String) {
        if let Some(state) = self.new_workspace.as_mut() {
            state.error = Some(msg);
        }
    }
}

use super::*;

impl App {
    fn workspace_action_rows_for() -> Vec<WorkspaceActionRow> {
        vec![
            WorkspaceActionRow {
                key: 'l',
                label: "Launch workspace",
                desc: "Start a new container in this workspace.",
                action: WorkspaceAction::LaunchWorkspace,
            },
            WorkspaceActionRow {
                key: 'r',
                label: "Remove workspace",
                desc: "Stop running containers and remove this workspace from config.",
                action: WorkspaceAction::RemoveWorkspace,
            },
        ]
    }

    pub(crate) fn workspace_action_rows(&self, workspace_idx: usize) -> Vec<WorkspaceActionRow> {
        if self.config.get().workspaces.get(workspace_idx).is_none() {
            return Vec::new();
        }
        Self::workspace_action_rows_for()
    }

    pub(crate) fn refresh_workspaces_cache(&mut self) {
        let cfg = self.config.get();
        self.workspaces = cfg
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
    }

    pub(crate) fn settings_action_rows_for() -> Vec<SettingsActionRow> {
        vec![
            SettingsActionRow {
                key: 'r',
                label: "Inspect rules status".to_string(),
                desc: "Show global and workspace rules trust/block state in the log.",
                action: SettingsAction::InspectRules,
            },
            SettingsActionRow {
                key: 't',
                label: "Trust workspace rules".to_string(),
                desc: "Explicitly trust the current reviewed workspace rules file.",
                action: SettingsAction::TrustWorkspaceRules,
            },
            SettingsActionRow {
                key: 'g',
                label: "Trust global rules".to_string(),
                desc: "Explicitly trust the current reviewed global rules file.",
                action: SettingsAction::TrustGlobalRules,
            },
            SettingsActionRow {
                key: 'x',
                label: "Remove workspace".to_string(),
                desc: "Remove from config and stop any running containers in this workspace.",
                action: SettingsAction::RemoveWorkspace,
            },
        ]
    }

    pub(crate) fn settings_action_rows(&self, workspace_idx: usize) -> Vec<SettingsActionRow> {
        let cfg = self.config.get();
        if cfg.workspaces.get(workspace_idx).is_none() {
            return Vec::new();
        }
        Self::settings_action_rows_for()
    }

    pub(crate) fn handle_settings_key(&mut self, key: KeyEvent) {
        let Some(pi) = self.active_settings_workspace else {
            self.focus = Focus::Sidebar;
            return;
        };

        let actions_len = self.settings_action_rows(pi).len();
        if actions_len == 0 {
            self.focus = Focus::Sidebar;
            self.active_settings_workspace = None;
            return;
        }
        if self.settings_cursor >= actions_len {
            self.settings_cursor = actions_len.saturating_sub(1);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('h') => {
                self.focus = Focus::Sidebar;
                self.active_settings_workspace = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.settings_cursor > 0 {
                    self.settings_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.settings_cursor + 1 < actions_len {
                    self.settings_cursor += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => self.run_settings_action(pi),
            KeyCode::Char('r') | KeyCode::Char('R') => self.do_inspect_rules(pi),
            KeyCode::Char('t') | KeyCode::Char('T') => self.do_trust_workspace_rules(pi),
            KeyCode::Char('g') | KeyCode::Char('G') => self.do_trust_global_rules(),
            KeyCode::Char('x') | KeyCode::Char('X') => self.prompt_remove_workspace(pi),
            _ => {}
        }
    }

    pub(crate) fn run_settings_action(&mut self, pi: usize) {
        let actions = self.settings_action_rows(pi);
        let Some(row) = actions.get(self.settings_cursor) else {
            return;
        };
        match row.action {
            SettingsAction::InspectRules => self.do_inspect_rules(pi),
            SettingsAction::TrustWorkspaceRules => self.do_trust_workspace_rules(pi),
            SettingsAction::TrustGlobalRules => self.do_trust_global_rules(),
            SettingsAction::RemoveWorkspace => self.prompt_remove_workspace(pi),
        }
    }

    pub(crate) fn do_inspect_rules(&mut self, pi: usize) {
        let cfg = self.config.get();
        let Some(proj) = cfg.workspaces.get(pi) else {
            return;
        };
        match self.config.rules_status(Some(&proj.name)) {
            Ok(rules) => {
                for rule in rules {
                    let state = if rule.blocked { "BLOCKED" } else { "trusted" };
                    self.push_log(format!("rules {state}: {}", rule.path), rule.blocked);
                }
            }
            Err(error) => self.push_log(format!("failed inspecting rules: {error}"), true),
        }
    }

    pub(crate) fn do_trust_workspace_rules(&mut self, pi: usize) {
        let cfg = self.config.get();
        let Some(proj) = cfg.workspaces.get(pi) else {
            return;
        };
        let target = crate::server::RulesTrustTarget::Workspace {
            workspace: proj.name.clone(),
        };
        if let Err(error) = self.trust_rules_target(target) {
            self.push_log(
                format!("failed trusting workspace rules: {}", error.reason),
                true,
            );
        }
    }

    pub(crate) fn do_trust_global_rules(&mut self) {
        if let Err(error) = self.trust_rules_target(crate::server::RulesTrustTarget::Global) {
            self.push_log(
                format!("failed trusting global rules: {}", error.reason),
                true,
            );
        }
    }

    pub(crate) fn prompt_remove_workspace(&mut self, pi: usize) {
        let cfg = self.config.get();
        let Some(workspace) = cfg.workspaces.get(pi) else {
            return;
        };
        self.remove_workspace_confirm = Some(RemoveWorkspaceConfirmState {
            workspace_name: workspace.name.clone(),
        });
    }

    pub(crate) fn finish_remove_workspace_confirm(&mut self, confirmed: bool) {
        let Some(state) = self.remove_workspace_confirm.take() else {
            return;
        };
        if !confirmed {
            self.push_log(
                format!("workspace removal cancelled: '{}'", state.workspace_name),
                false,
            );
            return;
        }

        for idx in (0..self.sessions.len()).rev() {
            if self.sessions[idx].workspace_name == state.workspace_name {
                self.close_session(idx);
            }
        }

        match crate::new_project::remove_workspace_block(
            &self.loaded_config_path,
            &state.workspace_name,
        ) {
            Ok(false) => {
                self.push_log(
                    format!(
                        "workspace '{}' was not found in config; nothing removed",
                        state.workspace_name
                    ),
                    true,
                );
            }
            Ok(true) => {
                let new_config = match crate::config::load(&self.loaded_config_path) {
                    Ok(c) => c,
                    Err(e) => {
                        self.push_log(
                            format!(
                                "workspace '{}' removed, but failed to reload config: {}",
                                state.workspace_name, e
                            ),
                            true,
                        );
                        return;
                    }
                };
                self.config.set(std::sync::Arc::new(new_config));
                self.refresh_workspaces_cache();
                self.pending_stop
                    .retain(|item| item.workspace_name != state.workspace_name);
                self.pending_net
                    .retain(|item| item.source_workspace.as_deref() != Some(&state.workspace_name));
                self.active_settings_workspace = None;
                self.focus = Focus::Sidebar;
                self.settings_cursor = 0;
                let items = self.sidebar_items();
                self.sidebar_idx = Self::first_selectable_sidebar_idx(&items);
                self.update_sidebar_preview(&items);
                self.push_log(
                    format!("removed workspace '{}'", state.workspace_name),
                    false,
                );
            }
            Err(e) => {
                self.push_log(
                    format!(
                        "failed removing workspace '{}' from config: {}",
                        state.workspace_name, e
                    ),
                    true,
                );
            }
        }
    }

    pub(crate) fn handle_terminal_key(&mut self, key: KeyEvent) {
        if self.build_is_running() && self.active_session.is_none() {
            self.handle_build_scroll_key(key);
            return;
        }

        self.last_terminal_esc = None;

        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::ALT) {
            self.open_log_fullscreen();
            return;
        }

        match key.code {
            KeyCode::Esc => self.focus_sidebar_shortcut(),
            KeyCode::Char('k') => {
                if let Some(si) = self.active_session {
                    self.stop_session_from_tui(si);
                }
            }
            KeyCode::Char('x') | KeyCode::Char('X') => {
                if let Some(si) = self.active_session {
                    self.kill_network_connections_for_session(si);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn handle_scroll_mode_key(&mut self, key: KeyEvent) {
        let half_page = match self.focus {
            Focus::Activity => self
                .active_activity
                .as_deref()
                .and_then(|id| self.activity_by_id(id))
                .map(|a| a.terminal.term.lock().screen_lines().max(2) / 2)
                .unwrap_or(15),
            _ => self
                .active_session
                .and_then(|si| self.sessions.get(si))
                .map(|s| s.term.lock().screen_lines().max(2) / 2)
                .unwrap_or(15),
        };

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.terminal_scroll = self.terminal_scroll.saturating_add(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.terminal_scroll = self.terminal_scroll.saturating_sub(1)
            }
            KeyCode::PageUp => {
                self.terminal_scroll = self.terminal_scroll.saturating_add(half_page)
            }
            KeyCode::PageDown => {
                self.terminal_scroll = self.terminal_scroll.saturating_sub(half_page)
            }
            KeyCode::Home | KeyCode::Char('g') => self.terminal_scroll = usize::MAX,
            KeyCode::End | KeyCode::Char('G') => self.terminal_scroll = 0,
            KeyCode::Esc | KeyCode::Char('q') => self.exit_scroll_mode(),
            _ => self.exit_scroll_mode(),
        }
    }

    pub(crate) fn exit_scroll_mode(&mut self) {
        self.scroll_mode = false;
        self.terminal_scroll = 0;
    }

    pub(crate) fn handle_build_scroll_key(&mut self, key: KeyEvent) {
        let max_scroll = self.build_output.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.build_scroll = self.build_scroll.saturating_add(1).min(max_scroll)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.build_scroll = self.build_scroll.saturating_sub(1)
            }
            KeyCode::PageUp => {
                self.build_scroll = self.build_scroll.saturating_add(15).min(max_scroll)
            }
            KeyCode::PageDown => self.build_scroll = self.build_scroll.saturating_sub(15),
            KeyCode::Home | KeyCode::Char('g') => self.build_scroll = max_scroll,
            KeyCode::End | KeyCode::Char('G') => self.build_scroll = 0,
            KeyCode::Esc => self.focus = Focus::Sidebar,
            _ => {}
        }
    }

    pub(crate) fn open_picker(&mut self) {
        let cfg = self.config.get();
        let items = self.sidebar_items();
        let Some(current) = items.get(self.sidebar_idx).cloned() else {
            return;
        };
        match current {
            SidebarItem::NewSession => {
                if self.workspaces.is_empty() {
                    self.push_log("no workspaces defined in config", true);
                    return;
                }
                self.container_picker =
                    Some(ContainerPickerState::NewSessionWorkspace { cursor: 0 });
            }
            SidebarItem::Launch(pi) => {
                if pi >= cfg.workspaces.len() {
                    return;
                }
                if !self.open_template_picker_for_workspace(pi) {
                    return;
                }
            }
            _ => return,
        }

        self.focus = Focus::ContainerPicker;
    }

    pub(crate) fn open_template_picker_for_workspace(&mut self, workspace_idx: usize) -> bool {
        if workspace_idx >= self.config.get().workspaces.len() {
            return false;
        }

        let templates = self.workspace_templates_for_workspace(workspace_idx);
        if templates.is_empty() {
            self.push_log("no container templates available for this workspace", true);
            return false;
        }

        self.container_picker = Some(ContainerPickerState::NewSessionTemplate {
            workspace_idx,
            cursor: 0,
            templates,
        });
        true
    }

    pub(crate) fn handle_picker_key(&mut self, key: KeyEvent) {
        let launch_session_group: Option<usize> = None;
        let mut launch_workspace_idx: Option<usize> = None;
        let mut launch_container_idx: Option<usize> = None;
        let mut next_workspace_idx: Option<usize> = None;

        match self.container_picker.as_mut() {
            Some(ContainerPickerState::NewSessionWorkspace { cursor }) => match key.code {
                KeyCode::Esc | KeyCode::Char('h') => {
                    self.container_picker = None;
                    self.focus = Focus::Sidebar;
                    return;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    crate::tui::move_wrapping_cursor(cursor, self.workspaces.len(), -1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    crate::tui::move_wrapping_cursor(cursor, self.workspaces.len(), 1);
                }
                KeyCode::Enter | KeyCode::Char('l') if !self.workspaces.is_empty() => {
                    next_workspace_idx = Some((*cursor).min(self.workspaces.len() - 1));
                }
                _ => {}
            },
            Some(ContainerPickerState::NewSessionTemplate {
                workspace_idx,
                cursor,
                templates,
            }) => match key.code {
                KeyCode::Esc | KeyCode::Char('h') => {
                    self.container_picker = None;
                    self.focus = Focus::Sidebar;
                    return;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    crate::tui::move_wrapping_cursor(cursor, templates.len(), -1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    crate::tui::move_wrapping_cursor(cursor, templates.len(), 1);
                }
                KeyCode::Enter | KeyCode::Char('l') => {
                    launch_workspace_idx = Some(*workspace_idx);
                    if !templates.is_empty() {
                        launch_container_idx = Some((*cursor).min(templates.len() - 1));
                    }
                }
                _ => {}
            },
            None => return,
        };

        if let Some(workspace_idx) = next_workspace_idx {
            self.open_template_picker_for_workspace(workspace_idx);
            return;
        }

        if let (Some(workspace_idx), Some(ctr_idx)) = (launch_workspace_idx, launch_container_idx) {
            self.container_picker = None;
            self.focus = Focus::Sidebar;
            self.do_launch_container_on_workspace_with_priority_and_env(
                workspace_idx,
                ctr_idx,
                crate::proxy::SourcePriority::Primary,
                &[],
                launch_session_group,
                None,
            );
            return;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('h') => {
                self.container_picker = None;
                self.focus = Focus::Sidebar;
            }
            _ => {}
        }
    }

    const BUILD_ACTION_COUNT: usize = 2;

    pub(crate) fn handle_build_key(&mut self, key: KeyEvent) {
        if self.build_is_running() || self.build_finished.is_some() {
            // Let the user retry a finished build without leaving the pane.
            // INVARIANT: `build_cursor == 0` is the "rebuild same target"
            // action in `run_build_action()` (the cursor indexes into the
            // build-pane action list, where 0 means "build + launch" — see
            // `run_build_action` below). If the action list is reordered, both
            // here and in `run_build_action` must move together (L15).
            if !self.build_is_running()
                && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
            {
                self.build_cursor = 0;
                self.run_build_action();
                return;
            }
            let max_scroll = self.build_output.len();
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') => {
                    // Esc returns to sidebar without canceling a running build.
                    if !self.build_is_running() {
                        self.build_finished = None;
                    }
                    self.focus = Focus::Sidebar;
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    if self.build_is_running() {
                        self.cancel_docker_build();
                        self.build_finished = None;
                        self.focus = Focus::Sidebar;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.build_scroll = self.build_scroll.saturating_add(1).min(max_scroll)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.build_scroll = self.build_scroll.saturating_sub(1)
                }
                KeyCode::PageUp => {
                    self.build_scroll = self.build_scroll.saturating_add(15).min(max_scroll)
                }
                KeyCode::PageDown => self.build_scroll = self.build_scroll.saturating_sub(15),
                KeyCode::Home | KeyCode::Char('g') => self.build_scroll = max_scroll,
                KeyCode::End | KeyCode::Char('G') => self.build_scroll = 0,
                _ => {}
            }
            return;
        }

        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            // See INVARIANT above: cursor 0 is "rebuild + launch" in
            // `run_build_action` (L15).
            self.build_cursor = 0;
            self.run_build_action();
            return;
        }
        if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            // Cursor 1 is the "cancel" branch of `run_build_action` (L15).
            self.build_cursor = 1;
            self.run_build_action();
            return;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('h') => {
                self.build_container_idx = None;
                self.build_workspace_idx = None;
                self.build_session_group = None;
                self.focus = Focus::Sidebar;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.build_cursor > 0 {
                    self.build_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.build_cursor + 1 < Self::BUILD_ACTION_COUNT {
                    self.build_cursor += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => self.run_build_action(),
            _ => {}
        }
    }

    pub(crate) fn run_build_action(&mut self) {
        let Some(ctr_idx) = self.build_container_idx else {
            return;
        };
        let launch_workspace_idx = self
            .build_workspace_idx
            .or_else(|| self.selected_workspace_idx());
        let Some(launch_workspace_idx) = launch_workspace_idx else {
            self.push_log("cannot start build: no workspace selected", true);
            return;
        };

        let templates = self.workspace_templates_for_workspace(launch_workspace_idx);
        let Some(ctr) = templates.get(ctr_idx) else {
            self.push_log("selected container template is no longer available", true);
            return;
        };

        let cfg = self.config.get();
        let dockerfile_path = ctr.dockerfile_path.clone().unwrap_or_else(|| {
            cfg.docker_dir
                .join(format!("{}.dockerfile", ctr.image_stem))
        });
        if !dockerfile_path.exists() {
            self.push_log(
                format!(
                    "Looked for {} and didn't find it, please use a valid image name.",
                    dockerfile_path.display()
                ),
                true,
            );
            self.focus = Focus::ImageBuild;
            return;
        }

        let dockerfile_context = dockerfile_path.parent().unwrap_or(cfg.docker_dir.as_path());
        let no_cache = self.pending_force_rebuild;
        let (build_cmd, maybe_base_cmd) = Self::build_commands_for(
            &dockerfile_path,
            &ctr.image,
            dockerfile_context,
            &cfg.docker_dir,
            no_cache,
        );

        let requested = match self.build_cursor {
            0 => Some(("build + launch", build_cmd)),
            1 => {
                self.build_container_idx = None;
                self.build_workspace_idx = None;
                self.focus = Focus::Sidebar;
                return;
            }
            _ => None,
        };

        let Some((label, build_command)) = requested else {
            return;
        };

        let mut docker_commands: Vec<Vec<String>> = Vec::new();
        if let Some(base_cmd) = maybe_base_cmd {
            let base_image = Self::BASE_IMAGE_TAG;
            match docker_image_exists(base_image) {
                Ok(true) => {}
                Ok(false) => {
                    let base_dockerfile = cfg.docker_dir.join("harness-hat-base.dockerfile");
                    if !base_dockerfile.exists() {
                        self.push_log(
                            format!(
                                "Looked for {} and didn't find it, please run setup to restore the base dockerfile.",
                                base_dockerfile.display()
                            ),
                            true,
                        );
                        self.focus = Focus::ImageBuild;
                        return;
                    }
                    self.push_log(
                        format!("base image '{base_image}' not found; building it first"),
                        false,
                    );
                    docker_commands.push(base_cmd);
                }
                Err(e) => {
                    let base_dockerfile = cfg.docker_dir.join("harness-hat-base.dockerfile");
                    if !base_dockerfile.exists() {
                        self.push_log(
                            format!(
                                "Looked for {} and didn't find it, please run setup to restore the base dockerfile.",
                                base_dockerfile.display()
                            ),
                            true,
                        );
                        self.focus = Focus::ImageBuild;
                        return;
                    }
                    self.push_log(
                        format!(
                            "warning: failed to inspect docker image '{base_image}': {e}; attempting base build"
                        ),
                        true,
                    );
                    docker_commands.push(base_cmd);
                }
            }
        }

        docker_commands.push(build_command);
        let command_display = docker_commands
            .iter()
            .map(|cmd| shell_command_for_docker_args(cmd))
            .collect::<Vec<_>>()
            .join(" && ");

        self.start_docker_build(
            label,
            docker_commands,
            command_display,
            launch_workspace_idx,
            ctr_idx,
        );
    }

    pub fn build_is_running(&self) -> bool {
        self.build_task.is_some()
    }

    pub fn active_build_command(&self) -> Option<&str> {
        self.build_task
            .as_ref()
            .map(|task| task.command_display.as_str())
            .or_else(|| {
                self.build_finished
                    .as_ref()
                    .map(|finished| finished.command.as_str())
            })
    }
}

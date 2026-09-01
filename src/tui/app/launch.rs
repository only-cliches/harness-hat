use super::*;
use crate::server::{LaunchEvent, WorkspaceLaunchItem, WorkspaceLaunchResponse};
use tokio::sync::mpsc as tokio_mpsc;

fn workspace_mount_source(
    workspace: &crate::config::WorkspaceConfig,
    launch_cwd: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, String> {
    if !workspace.mount_cwd {
        return Ok(workspace.canonical_path.clone());
    }

    let source = launch_cwd.unwrap_or_else(|| workspace.canonical_path.clone());
    let source = source.canonicalize().map_err(|error| {
        format!(
            "launch directory is not accessible: {}: {error}",
            source.display()
        )
    })?;
    if source.starts_with(&workspace.canonical_path) {
        Ok(source)
    } else {
        Err(format!(
            "launch directory {} is outside workspace {}",
            source.display(),
            workspace.canonical_path.display()
        ))
    }
}

impl App {
    fn container_command_for_profile(ctr: &crate::config::ContainerDef) -> Option<Vec<String>> {
        ctr.command.clone()
    }

    pub(crate) fn workspace_templates_for_workspace(
        &mut self,
        workspace_idx: usize,
    ) -> Vec<crate::config::ContainerDef> {
        let cfg = self.config.get();
        let Some(workspace) = cfg.workspaces.get(workspace_idx) else {
            return cfg.containers.clone();
        };

        crate::config::resolve_workspace_container_templates(
            &workspace.canonical_path,
            &cfg.defaults.containers,
            &cfg.containers,
        )
        .unwrap_or_else(|e| {
            self.push_log(
                format!(
                    "failed to scan workspace templates for '{}': {}",
                    workspace.name, e
                ),
                true,
            );
            cfg.containers.clone()
        })
    }

    pub(crate) fn workspace_template_for_workspace(
        &mut self,
        workspace_idx: usize,
        template_idx: usize,
    ) -> Option<crate::config::ContainerDef> {
        let templates = self.workspace_templates_for_workspace(workspace_idx);
        templates.get(template_idx).cloned()
    }

    pub(crate) fn configured_template_idx(&self, template_idx: usize) -> Option<usize> {
        let cfg = self.config.get();
        (template_idx < cfg.containers.len()).then_some(template_idx)
    }

    pub(crate) fn workspace_templates_for_name(
        &mut self,
        workspace_name: &str,
    ) -> Vec<crate::config::ContainerDef> {
        let cfg = self.config.get();
        let workspace_idx = cfg
            .workspaces
            .iter()
            .position(|workspace| workspace.name == workspace_name);
        let Some(workspace_idx) = workspace_idx else {
            return cfg.containers.clone();
        };
        crate::config::resolve_workspace_container_templates(
            &cfg.workspaces[workspace_idx].canonical_path,
            &cfg.defaults.containers,
            &cfg.containers,
        )
        .unwrap_or_else(|e| {
            self.push_log(
                format!(
                    "failed to scan workspace templates for '{}': {}",
                    cfg.workspaces[workspace_idx].name, e
                ),
                true,
            );
            cfg.containers.clone()
        })
    }

    /// Rebuild `self.workspaces` (the sidebar workspace status list) from the
    /// current `SharedConfig`. Called after a live config reload so a newly
    /// added workspace shows up in the sidebar on the same tick.
    pub(crate) fn refresh_workspace_statuses(&mut self) {
        let cfg = self.config.get();
        self.workspaces = cfg
            .workspaces
            .iter()
            .zip(crate::config::resolve_workspace_sidebar_hotkeys(
                &cfg.workspaces,
            ))
            .map(|(workspace, hotkey)| WorkspaceStatus {
                name: workspace.name.clone(),
                sidebar_hotkey: hotkey,
            })
            .collect();
    }

    /// Handle a `/workspace/launch` request that arrived from the control
    /// server. Reloads config from disk so a freshly-appended `[[workspaces]]`
    /// entry is visible, swaps it into `SharedConfig`, refreshes the sidebar,
    /// and either launches the session straight away or kicks off a
    /// `docker build` first — streaming progress back through `event_tx` and
    /// mirroring the build output in the TUI's build pane.
    pub(crate) fn handle_workspace_launch_request(&mut self, item: WorkspaceLaunchItem) {
        let WorkspaceLaunchItem {
            workspace_name,
            template,
            force_rebuild,
            cwd,
            terminal_env,
            event_tx,
        } = item;

        let reloaded = match crate::config::load(&self.loaded_config_path) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.push_log(
                    format!(
                        "reloading config for /workspace/launch failed: {e:?}; \
                         keeping previous config in memory"
                    ),
                    true,
                );
                return finish_launch_stream(
                    &event_tx,
                    Err(format!(
                        "manager could not reload config from {}: {e}",
                        self.loaded_config_path.display()
                    )),
                );
            }
        };
        self.config.set(std::sync::Arc::new(reloaded));
        self.refresh_workspace_statuses();

        let cfg = self.config.get();
        let Some(workspace_idx) = cfg.workspaces.iter().position(|w| w.name == workspace_name)
        else {
            return finish_launch_stream(
                &event_tx,
                Err(format!(
                    "no workspace named {workspace_name:?} in {}",
                    self.loaded_config_path.display()
                )),
            );
        };
        let templates = self.workspace_templates_for_workspace(workspace_idx);
        let Some(template_idx) = templates.iter().position(|c| c.name == template) else {
            return finish_launch_stream(
                &event_tx,
                Err(format!("no container template named {template:?}")),
            );
        };
        let image = templates[template_idx].image.clone();

        let _ = event_tx.try_send(LaunchEvent::Status {
            message: format!("checking docker image {image}"),
        });

        let image_check = if force_rebuild {
            Ok(false) // treat as missing to force a build
        } else {
            docker_image_exists(&image)
        };

        match image_check {
            Ok(false) => {
                // Another build is already in flight: refuse rather than
                // clobber `build_task` / `workspace_launch_pending`.
                // `start_docker_build` would silently no-op anyway; surfacing
                // a clear error is friendlier than waiting for a timeout.
                if self.build_task.is_some() || self.workspace_launch_pending.is_some() {
                    return finish_launch_stream(
                        &event_tx,
                        Err("a docker build is already running in this manager; \
                             retry once it finishes"
                            .to_string()),
                    );
                }

                // Image missing: kick off `docker build` through the same TUI
                // path the in-TUI build prompt uses (so the TUI shows the same
                // build output the streaming response will mirror), and stash
                // a pending entry. The runtime's `BuildEvent::Output` /
                // `BuildEvent::Finished` handlers forward through `event_tx`
                // and finish the launch when the build completes.
                let _ = event_tx.try_send(LaunchEvent::Status {
                    message: format!("image {image} not found locally — building"),
                });
                self.build_workspace_idx = Some(workspace_idx);
                self.build_container_idx = Some(template_idx);
                self.build_session_group = None;
                self.build_cursor = 0; // "build + launch" branch
                self.pending_force_rebuild = force_rebuild;
                self.run_build_action();
                if self.build_task.is_none() {
                    return finish_launch_stream(
                        &event_tx,
                        Err(format!(
                            "manager could not start docker build for image '{image}' — \
                             check the manager TUI logs for the underlying error"
                        )),
                    );
                }
                self.workspace_launch_pending = Some(WorkspaceLaunchPending {
                    event_tx,
                    workspace_name,
                    template,
                    workspace_idx,
                    template_idx,
                    cwd,
                    terminal_env,
                });
            }
            other => {
                // Image present (or check failed — fall through and let docker
                // surface the real error, matching `preflight_image_or_prompt_build`'s
                // legacy behavior). Launch synchronously, finish the stream.
                if other.is_err() {
                    let _ = event_tx.try_send(LaunchEvent::Status {
                        message: format!(
                            "could not inspect image {image}; attempting launch anyway"
                        ),
                    });
                }
                let _ = event_tx.try_send(LaunchEvent::Status {
                    message: format!("launching {template} on {workspace_name}"),
                });
                let outcome = self.do_launch_for_workspace_endpoint(
                    workspace_idx,
                    template_idx,
                    &workspace_name,
                    &template,
                    cwd,
                    &terminal_env,
                );
                finish_launch_stream(&event_tx, outcome);
            }
        }
    }

    /// Run `do_launch_container_on_workspace_with_priority_and_env` and convert
    /// the "did sessions grow?" indirection into a `Result` carrying the new
    /// session's identifiers. Shared by the immediate-launch and post-build
    /// launch paths.
    pub(crate) fn do_launch_for_workspace_endpoint(
        &mut self,
        workspace_idx: usize,
        template_idx: usize,
        workspace_name: &str,
        template: &str,
        cwd: Option<PathBuf>,
        terminal_env: &[(String, String)],
    ) -> Result<WorkspaceLaunchResponse, String> {
        let before_len = self.sessions.len();
        self.do_launch_container_on_workspace_with_priority_and_env(
            workspace_idx,
            template_idx,
            crate::proxy::SourcePriority::Primary,
            terminal_env,
            None,
            cwd,
        );
        if self.sessions.len() == before_len {
            return Err(format!(
                "launch of '{template}' on '{workspace_name}' did not start a session — \
                 check the manager TUI logs"
            ));
        }
        let session = self
            .sessions
            .last()
            .expect("just verified sessions.len() grew");
        Ok(WorkspaceLaunchResponse {
            session_token: session.session_token.clone(),
            alias: session.alias.clone(),
            docker_name: session.docker_name.clone(),
            workspace_name: workspace_name.to_string(),
            template: template.to_string(),
            mount_target: session.mount_target.clone(),
        })
    }

    pub(crate) fn do_launch_container_on_workspace_with_priority(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        proxy_priority: crate::proxy::SourcePriority,
        session_group: Option<usize>,
    ) {
        self.do_launch_container_on_workspace_with_priority_and_env(
            pi,
            ctr_idx,
            proxy_priority,
            &[],
            session_group,
            None,
        );
    }

    pub(crate) fn do_launch_container_on_workspace_with_priority_and_env(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        proxy_priority: crate::proxy::SourcePriority,
        extra_env: &[(String, String)],
        session_group: Option<usize>,
        launch_cwd: Option<PathBuf>,
    ) {
        let Some(ctr) = self.workspace_template_for_workspace(pi, ctr_idx) else {
            return;
        };

        let cfg = self.config.get();

        let proj = match cfg.workspaces.get(pi) {
            Some(p) => p.clone(),
            None => return,
        };

        if let Err(error) = self
            .config
            .ensure_rules_trusted_for_workspace(Some(proj.name.as_str()))
        {
            self.push_log(
                format!(
                    "cannot launch '{}' on '{}': rules must be reviewed before launch: {error}",
                    ctr.name, proj.name
                ),
                true,
            );
            return;
        }

        let rules = match crate::config::load_composed_rules_for_workspace(
            &cfg,
            Some(proj.name.as_str()),
        ) {
            Ok(rules) => rules,
            Err(error) => {
                self.push_log(
                    format!(
                        "cannot launch '{}' on '{}': failed to load rules: {error}",
                        ctr.name, proj.name
                    ),
                    true,
                );
                return;
            }
        };
        let mirror_cwd = rules.mirror_cwd;

        if !self.preflight_image_or_prompt_build(
            pi,
            ctr_idx,
            &ctr.image,
            session_group,
            docker_image_exists,
        ) {
            return;
        }

        let mount_source_path = match workspace_mount_source(&proj, launch_cwd) {
            Ok(path) => path,
            Err(error) => {
                self.push_log(
                    format!("cannot launch '{}' on '{}': {error}", ctr.name, proj.name),
                    true,
                );
                return;
            }
        };

        let mut ctr = ctr;
        ctr.localhost_forwards = crate::config::merge_localhost_forwards(
            &ctr.localhost_forwards,
            &rules.localhost_forwards,
        );
        if mirror_cwd {
            if let Some(mount_target) =
                crate::container::mirrored_workspace_mount_target(&mount_source_path)
            {
                ctr.mount_target = mount_target;
            } else {
                self.push_log(
                    format!(
                        "mirror_cwd is enabled for '{}', but {} cannot be mirrored in a Linux container; using {}",
                        proj.name,
                        mount_source_path.display(),
                        crate::config::container_path_string(&ctr.mount_target),
                    ),
                    false,
                );
            }
        }
        self.log_workspace_rules_status(&proj);

        let control_port = cfg.defaults.control.server_port;
        let control_host = &cfg.defaults.control.server_host;
        let control_url = format!("http://{control_host}:{control_port}");
        let hostdo_script_host_path = cfg.docker_dir.join("scripts/hostdo.py");
        let proxy_host = &cfg.defaults.proxy.proxy_host;
        let session_token = uuid::Uuid::new_v4().simple().to_string();
        let scoped_proxy = match crate::proxy::spawn_scoped_listener_with_forwards(
            &self.proxy_state,
            proxy_host,
            &proj.name,
            &ctr.name,
            &session_token,
            proxy_priority,
            ctr.localhost_forwards.clone(),
        ) {
            Ok(listener) => listener,
            Err(e) => {
                self.push_log(
                    format!("cannot launch '{}' on '{}': {e}", ctr.name, proj.name),
                    true,
                );
                return;
            }
        };
        let proxy_url = scoped_proxy.proxy_url();
        let group_idx = self.resolve_or_create_session_group(
            session_group,
            pi,
            ctr_idx,
            self.configured_template_idx(ctr_idx),
        );

        self.push_log(
            format!("launching '{}' on '{}'", ctr.name, proj.name),
            false,
        );

        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((120, 40));
        let pty_cols = term_cols.saturating_sub(38).max(20);
        let pty_rows = term_rows.saturating_sub(10).max(6);

        #[cfg(target_os = "macos")]
        if cfg.defaults.proxy.strict_network {
            self.push_log(
                "strict_network on macOS requires Docker `--privileged`; harness-hat applies it automatically for this container launch",
                false,
            );
        }

        self.session_registry.insert(
            session_token.clone(),
            crate::server::SessionIdentity {
                workspace_name: proj.name.clone(),
                container_id: String::new(),
                mount_target: crate::config::container_path_string(&ctr.mount_target),
            },
        );

        let command_argv = Self::container_command_for_profile(&ctr);
        match crate::container::spawn(
            &ctr,
            command_argv.as_deref(),
            &proj.name,
            &mount_source_path,
            &session_token,
            &self.token,
            &control_url,
            &proxy_url,
            Some(hostdo_script_host_path.as_path()),
            Some(scoped_proxy),
            proxy_priority,
            cfg.defaults.proxy.strict_network,
            extra_env,
            pty_rows,
            pty_cols,
        ) {
            Ok((session, launch_notes)) => {
                let new_si = self.sessions.len();
                self.sessions.push(session);
                if let Some(s) = self.sessions.get(new_si) {
                    self.session_registry.insert(
                        s.session_token.clone(),
                        crate::server::SessionIdentity {
                            workspace_name: s.workspace_name.clone(),
                            container_id: s.container_id.clone(),
                            mount_target: s.mount_target.clone(),
                        },
                    );
                }
                self.active_session = Some(new_si);
                self.scroll_mode = false;
                self.terminal_scroll = 0;
                self.focus = Focus::Terminal;
                self.add_session_terminal(group_idx, new_si);
                for note in launch_notes {
                    self.push_log(note, false);
                }
                let pos = self
                    .sidebar_items()
                    .iter()
                    .position(|item| *item == SidebarItem::Session(group_idx));
                if let Some(pos) = pos {
                    self.sidebar_idx = pos;
                }
                self.active_session = Some(new_si);
                self.preview_session = Some(new_si);
            }
            Err(e) => {
                // The container launcher rolls back any Docker container it
                // started but could not adopt. Remove the pre-launch registry
                // entry as well: leaving it behind would authorize a token for
                // a session that never became visible or usable in the TUI.
                self.session_registry.remove(&session_token);
                self.push_log(
                    format!("launch '{}' on '{}' failed: {e}", ctr.name, proj.name),
                    true,
                );
            }
        }
    }
}

/// Emit a terminal `Launched` or `Error` event on `event_tx`. The caller's
/// owned `event_tx` clone (or a reference, since `Sender` is `Clone`) is then
/// dropped, which closes the streaming HTTP response body on the CLI side.
/// `try_send` is best-effort: if the CLI hung up partway, the manager
/// shouldn't block its event loop on a now-dead pipe.
pub(crate) fn finish_launch_stream(
    event_tx: &tokio_mpsc::Sender<LaunchEvent>,
    outcome: Result<WorkspaceLaunchResponse, String>,
) {
    let event = match outcome {
        Ok(resp) => LaunchEvent::Launched(resp),
        Err(reason) => LaunchEvent::Error { reason },
    };
    let _ = event_tx.try_send(event);
}

#[cfg(test)]
mod tests {
    use super::{App, workspace_mount_source};
    use crate::config::{ContainerDef, WorkspaceConfig, default_mount_target};

    #[test]
    fn mount_cwd_source_is_confined_to_configured_workspace() {
        let root = tempfile::tempdir().expect("workspace root");
        let nested = root.path().join("nested");
        let outside = tempfile::tempdir().expect("outside directory");
        std::fs::create_dir(&nested).expect("nested workspace directory");
        let workspace = WorkspaceConfig {
            name: "workspace".to_string(),
            canonical_path: root.path().canonicalize().expect("canonical root"),
            sidebar_hotkey: None,
            template: None,
            mount_cwd: true,
        };

        assert_eq!(
            workspace_mount_source(&workspace, Some(nested)).expect("nested source"),
            root.path()
                .join("nested")
                .canonicalize()
                .expect("canonical nested")
        );
        assert!(workspace_mount_source(&workspace, Some(outside.path().to_path_buf())).is_err());
    }

    #[test]
    fn container_command_for_template_uses_configured_override() {
        let profile = ContainerDef {
            name: "dev".to_string(),
            image: String::new(),
            image_stem: String::new(),
            dockerfile_path: None,
            profile: None,
            mount_target: default_mount_target(),
            command: Some(vec![
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "htop".to_string(),
            ]),
            grayscale_palette: false,
            starter_network_allowlist: Vec::new(),
            allowed_hosts: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            mounts: Vec::new(),
            env: std::collections::HashMap::new(),
            env_passthrough: Vec::new(),
            localhost_forwards: Vec::new(),
            memory: None,
            cpus: None,
            shm_size: None,
            attach_shell: None,
            claude_settings: None,
        };

        assert_eq!(
            App::container_command_for_profile(&profile),
            Some(vec![
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "htop".to_string()
            ])
        );
    }

    #[test]
    fn container_command_for_template_requires_explicit_command() {
        let profile = ContainerDef {
            name: "dev".to_string(),
            image: String::new(),
            image_stem: String::new(),
            dockerfile_path: None,
            profile: None,
            mount_target: default_mount_target(),
            command: None,
            grayscale_palette: true,
            starter_network_allowlist: Vec::new(),
            allowed_hosts: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            mounts: Vec::new(),
            env: std::collections::HashMap::new(),
            env_passthrough: Vec::new(),
            localhost_forwards: Vec::new(),
            memory: None,
            cpus: None,
            shm_size: None,
            attach_shell: None,
            claude_settings: None,
        };
        assert_eq!(App::container_command_for_profile(&profile), None);
    }
}

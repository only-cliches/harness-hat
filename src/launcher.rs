//! Small graphical launcher for container-backed Claude Desktop sessions.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod gui {
    use anyhow::Result;
    use eframe::egui;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    const SESSION_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

    const TEMPLATES: &[Template] = &[
        Template::new("default", "General", "A small general-purpose environment"),
        Template::new(
            "typescript",
            "Node / TypeScript",
            "Node.js, npm, Bun, JavaScript and TypeScript",
        ),
        Template::new("python", "Python", "Python, uv and common build tools"),
        Template::new("go", "Go", "The Go toolchain and common development tools"),
        Template::new("rust", "Rust", "Rust, Cargo and common Cargo utilities"),
        Template::new("kotlin", "Kotlin / JVM", "Kotlin, Java and Gradle projects"),
        Template::new("android", "Android", "Android SDK, Kotlin, Java and Gradle"),
        Template::new("php", "PHP", "PHP and Composer projects"),
        Template::new("csharp", "C# / .NET", ".NET and C# projects"),
    ];

    #[derive(Clone, Copy)]
    struct Template {
        id: &'static str,
        label: &'static str,
        description: &'static str,
    }

    impl Template {
        const fn new(id: &'static str, label: &'static str, description: &'static str) -> Self {
            Self {
                id,
                label,
                description,
            }
        }
    }

    enum LaunchState {
        Idle,
        Launching(Receiver<Result<ConnectionGuide, String>>),
        Opened(ConnectionGuide),
        Failed(String),
    }

    struct ConnectionGuide;

    enum SetupState {
        NotStarted,
        Checking(Receiver<harness_hat::desktop::LauncherReadiness>),
        Installing(Receiver<Result<PathBuf, String>>),
        Ready,
        Blocked(harness_hat::desktop::LauncherReadiness),
    }

    struct SessionRow {
        container: harness_hat::desktop::DesktopContainer,
        folder: String,
        ssh_connected: Option<bool>,
        ssh_port: Option<u16>,
    }

    pub struct Launcher {
        project: Option<PathBuf>,
        template: usize,
        template_note: String,
        state: LaunchState,
        setup: SetupState,
        sessions: Vec<SessionRow>,
        session_refresh: Option<Receiver<Result<Vec<SessionRow>, String>>>,
        last_session_refresh: Instant,
        session_error: Option<String>,
        stopping: Option<(String, Receiver<Result<(), String>>)>,
        opening_claude: Option<Receiver<Result<(), String>>>,
        show_connection_help: bool,
    }

    impl Default for Launcher {
        fn default() -> Self {
            Self {
                project: None,
                template: 0,
                template_note: "Choose a project and Hat will suggest an environment.".into(),
                state: LaunchState::Idle,
                setup: SetupState::NotStarted,
                sessions: Vec::new(),
                session_refresh: None,
                last_session_refresh: Instant::now() - SESSION_REFRESH_INTERVAL,
                session_error: None,
                stopping: None,
                opening_claude: None,
                show_connection_help: false,
            }
        }
    }

    impl Launcher {
        fn start_setup_check(&mut self, context: &egui::Context) {
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            std::thread::spawn(move || {
                let _ = tx.send(harness_hat::desktop::launcher_readiness());
                repaint.request_repaint();
            });
            self.setup = SetupState::Checking(rx);
        }

        fn start_service_install(&mut self, context: &egui::Context) {
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            std::thread::spawn(move || {
                let result = harness_hat::desktop::install_launcher_service()
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                repaint.request_repaint();
            });
            self.setup = SetupState::Installing(rx);
        }

        fn poll_setup(&mut self, context: &egui::Context) {
            if matches!(self.setup, SetupState::NotStarted) {
                self.start_setup_check(context);
                return;
            }
            let checked = match &self.setup {
                SetupState::Checking(receiver) => receiver.try_recv().ok(),
                _ => None,
            };
            if let Some(readiness) = checked {
                match readiness {
                    harness_hat::desktop::LauncherReadiness::Ready => {
                        self.setup = SetupState::Ready;
                    }
                    harness_hat::desktop::LauncherReadiness::NeedsSetup(_) => {
                        self.start_service_install(context);
                    }
                    blocked => self.setup = SetupState::Blocked(blocked),
                }
                return;
            }
            let installed = match &self.setup {
                SetupState::Installing(receiver) => receiver.try_recv().ok(),
                _ => None,
            };
            if let Some(result) = installed {
                self.setup = match result {
                    Ok(_) => SetupState::Ready,
                    Err(error) => {
                        SetupState::Blocked(harness_hat::desktop::LauncherReadiness::Error(error))
                    }
                };
            }
        }

        fn choose_project(&mut self) {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Choose a project to protect with Harness Hat")
                .pick_folder()
            else {
                return;
            };
            let path = path.canonicalize().unwrap_or(path);
            let (template, note) = suggested_template(&path);
            self.project = Some(path);
            self.template = template_index(&template);
            self.template_note = note;
            self.state = LaunchState::Idle;
        }

        fn launch(&mut self, context: &egui::Context) {
            let Some(project) = self.project.clone() else {
                return;
            };
            let template = TEMPLATES[self.template].id.to_string();
            let config = match harness_hat::manager::default_home_config_path() {
                Ok(config) if config.exists() => config,
                _ => {
                    self.state = LaunchState::Failed(
                        "Harness Hat is not set up yet. Run `hat init` and `hat install` once, then reopen this app."
                            .into(),
                    );
                    return;
                }
            };
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            std::thread::spawn(move || {
                let result = launch_workspace(project, template, config)
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                repaint.request_repaint();
            });
            self.state = LaunchState::Launching(rx);
        }

        fn poll_launch(&mut self) {
            let result = match &self.state {
                LaunchState::Launching(rx) => rx.try_recv().ok(),
                _ => None,
            };
            if let Some(result) = result {
                self.state = match result {
                    Ok(guide) => {
                        self.last_session_refresh = Instant::now() - SESSION_REFRESH_INTERVAL;
                        self.show_connection_help = true;
                        LaunchState::Opened(guide)
                    }
                    Err(error) => LaunchState::Failed(error),
                };
            }
        }

        fn refresh_sessions(&mut self, context: &egui::Context) {
            if self.session_refresh.is_some()
                || self.last_session_refresh.elapsed() < SESSION_REFRESH_INTERVAL
            {
                return;
            }
            self.last_session_refresh = Instant::now();
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            std::thread::spawn(move || {
                let result = load_session_rows().map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                repaint.request_repaint();
            });
            self.session_refresh = Some(rx);
        }

        fn poll_sessions(&mut self) {
            let result = self
                .session_refresh
                .as_ref()
                .and_then(|receiver| receiver.try_recv().ok());
            if let Some(result) = result {
                self.session_refresh = None;
                match result {
                    Ok(sessions) => {
                        self.sessions = sessions;
                        self.session_error = None;
                    }
                    Err(error) => self.session_error = Some(error),
                }
            }

            let stopped = self
                .stopping
                .as_ref()
                .and_then(|(_, receiver)| receiver.try_recv().ok());
            if let Some(result) = stopped {
                self.stopping = None;
                match result {
                    Ok(()) => {
                        self.last_session_refresh = Instant::now() - SESSION_REFRESH_INTERVAL;
                    }
                    Err(error) => self.session_error = Some(error),
                }
            }

            let opened = self
                .opening_claude
                .as_ref()
                .and_then(|receiver| receiver.try_recv().ok());
            if let Some(result) = opened {
                self.opening_claude = None;
                if let Err(error) = result {
                    self.session_error = Some(error);
                }
            }
        }

        fn stop_session(&mut self, container_name: String, context: &egui::Context) {
            if self.stopping.is_some() {
                return;
            }
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            let thread_name = container_name.clone();
            std::thread::spawn(move || {
                let result = harness_hat::desktop::stop_container(&thread_name)
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                repaint.request_repaint();
            });
            self.stopping = Some((container_name, rx));
        }

        fn open_claude(&mut self, context: &egui::Context) {
            if self.opening_claude.is_some() {
                return;
            }
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            std::thread::spawn(move || {
                let result =
                    harness_hat::desktop::launch_claude().map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                repaint.request_repaint();
            });
            self.opening_claude = Some(rx);
        }
    }

    impl eframe::App for Launcher {
        fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
            self.poll_setup(context);
            if !matches!(self.setup, SetupState::Ready) {
                egui::CentralPanel::default().show(context, |ui| self.render_setup(ui, context));
                context.request_repaint_after(Duration::from_millis(100));
                return;
            }
            self.poll_launch();
            self.poll_sessions();
            self.refresh_sessions(context);
            egui::CentralPanel::default().show(context, |ui| {
                ui.add_space(16.0);
                ui.heading("Open Claude safely");
                ui.label("Choose a project. Claude's code tools will run inside a Harness Hat container.");
                ui.add_space(18.0);

                ui.label(egui::RichText::new("PROJECT FOLDER").small().strong());
                ui.horizontal(|ui| {
                    let path = self.project.as_ref().map_or_else(
                        || "No folder selected".to_string(),
                        |path| path.display().to_string(),
                    );
                    ui.add_sized([360.0, 28.0], egui::Label::new(path).truncate());
                    if ui.button("Choose…").clicked() {
                        self.choose_project();
                    }
                });

                ui.add_space(16.0);
                ui.label(egui::RichText::new("DEVELOPMENT ENVIRONMENT").small().strong());
                ui.add_enabled_ui(self.project.is_some(), |ui| {
                    egui::ComboBox::from_id_salt("template")
                        .selected_text(TEMPLATES[self.template].label)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for (index, template) in TEMPLATES.iter().enumerate() {
                                ui.selectable_value(&mut self.template, index, template.label);
                            }
                        });
                    ui.label(TEMPLATES[self.template].description);
                    ui.label(egui::RichText::new(&self.template_note).small().weak());
                });

                ui.add_space(22.0);
                let launching = matches!(self.state, LaunchState::Launching(_));
                let start_label = if launching {
                    "Starting protected session…"
                } else {
                    "Start protected session"
                };
                if ui
                    .add_enabled(
                        self.project.is_some() && !launching,
                        egui::Button::new(start_label),
                    )
                    .clicked()
                {
                    self.launch(context);
                }

                ui.add_space(12.0);
                match &self.state {
                    LaunchState::Idle => {
                        ui.label(egui::RichText::new("Container protected • strict network policy uses your Hat configuration").small());
                    }
                    LaunchState::Launching(_) => {
                        ui.spinner();
                        ui.label(egui::RichText::new("The first launch may take several minutes while Docker builds the selected environment.").small().weak());
                        context.request_repaint_after(std::time::Duration::from_millis(100));
                    }
                    LaunchState::Opened(_) if self.show_connection_help => {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    egui::Color32::from_rgb(40, 150, 80),
                                    egui::RichText::new("Finish connecting in Claude").strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("×").on_hover_text("Close help").clicked() {
                                            self.show_connection_help = false;
                                        }
                                    },
                                );
                            });
                            ui.label("1. In Code, start a new Code session.");
                            ui.label("2. Click Local → SSH.");
                            ui.label("3. Select the matching Harness Hat connection.");
                            ui.label("4. Select the project folder shown below.");
                            ui.label(
                                egui::RichText::new(
                                    "If it is missing, choose Add SSH host… and use the SSH host shown below. Leave port and identity blank.",
                                )
                                .small(),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "The session below changes to SSH connected when it works.",
                                )
                                .small()
                                .weak(),
                            );
                        });
                    }
                    LaunchState::Opened(_) => {}
                    LaunchState::Failed(error) => {
                        ui.colored_label(egui::Color32::from_rgb(190, 55, 55), error);
                    }
                }

                ui.add_space(20.0);
                ui.separator();
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.heading("Running protected sessions");
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let opening = self.opening_claude.is_some();
                            if ui
                                .add_enabled(
                                    !opening,
                                    egui::Button::new(if opening {
                                        "Opening Claude…"
                                    } else {
                                        "Open Claude Desktop"
                                    }),
                                )
                                .clicked()
                            {
                                self.open_claude(context);
                            }
                        },
                    );
                });
                ui.label(
                    egui::RichText::new(
                        "Disconnected Desktop SSH sessions stop after 10 minutes; sessions never connected stop after 30 minutes.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(8.0);

                if self.sessions.is_empty() {
                    if self.session_refresh.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Checking Docker…");
                        });
                    } else {
                        ui.label("No protected sessions are running.");
                    }
                } else {
                    let stopping_name = self.stopping.as_ref().map(|(name, _)| name.as_str());
                    let mut stop_clicked = None;
                    let mut help_clicked = false;
                    egui::ScrollArea::vertical()
                        .max_height(210.0)
                        .show(ui, |ui| {
                            for session in &self.sessions {
                                ui.group(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(&session.container.workspace).strong());
                                            ui.label(
                                                egui::RichText::new(&session.folder).small().weak(),
                                            );
                                            let connection = match (
                                                session.container.desktop_enabled,
                                                session.ssh_connected,
                                            ) {
                                                (false, _) => "○ SSH not enabled",
                                                (true, Some(true)) => "● SSH connected",
                                                (true, Some(false)) => "○ Waiting for SSH",
                                                (true, None) => "? SSH status unavailable",
                                            };
                                            let color = if session.ssh_connected == Some(true) {
                                                egui::Color32::from_rgb(40, 150, 80)
                                            } else {
                                                egui::Color32::from_rgb(120, 120, 120)
                                            };
                                            ui.colored_label(
                                                color,
                                                format!(
                                                    "{}  •  {}  •  session {}",
                                                    connection,
                                                    session.container.template,
                                                    session.container.alias
                                                ),
                                            );
                                            if session.container.desktop_enabled {
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "Claude connection: Harness Hat — {}",
                                                        session.container.workspace
                                                    ))
                                                    .small()
                                                    .weak(),
                                                );
                                                let alias = harness_hat::desktop::workspace_ssh_alias(
                                                    &session.container.workspace,
                                                );
                                                let endpoint = session.ssh_port.map_or_else(
                                                    || "127.0.0.1:<port unavailable>".to_string(),
                                                    |port| format!("127.0.0.1:{port}"),
                                                );
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "SSH host: {alias}  •  coder@{endpoint}"
                                                    ))
                                                    .small()
                                                    .weak(),
                                                );
                                            }
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                let stopping = stopping_name
                                                    == Some(session.container.name.as_str());
                                                if ui
                                                    .add_enabled(
                                                        self.stopping.is_none(),
                                                        egui::Button::new(if stopping {
                                                            "Stopping…"
                                                        } else {
                                                            "Stop"
                                                        }),
                                                    )
                                                    .clicked()
                                                {
                                                    stop_clicked =
                                                        Some(session.container.name.clone());
                                                }
                                                if session.container.desktop_enabled
                                                    && ui.button("Help").clicked()
                                                {
                                                    help_clicked = true;
                                                }
                                            },
                                        );
                                    });
                                });
                                ui.add_space(4.0);
                            }
                        });
                    if let Some(container_name) = stop_clicked {
                        self.stop_session(container_name, context);
                    }
                    if help_clicked {
                        self.show_connection_help = true;
                        self.state = LaunchState::Opened(ConnectionGuide);
                    }
                }
                if let Some(error) = &self.session_error {
                    ui.colored_label(egui::Color32::from_rgb(190, 55, 55), error);
                }
            });
        }
    }

    impl Launcher {
        fn render_setup(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
            ui.add_space(28.0);
            ui.heading("Getting Harness Hat ready");
            ui.add_space(8.0);
            match &self.setup {
                SetupState::NotStarted | SetupState::Checking(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking Docker, Claude Desktop, OpenSSH, and the Hat service…");
                    });
                }
                SetupState::Installing(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Creating your protected environment and starting Hat…");
                    });
                    ui.label(
                        egui::RichText::new(
                            "This is a per-user setup and does not require administrator access.",
                        )
                        .small()
                        .weak(),
                    );
                }
                SetupState::Blocked(issue) => {
                    let mut retry = false;
                    match issue {
                        harness_hat::desktop::LauncherReadiness::DockerMissing => {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 55, 55),
                                "Docker Desktop is required but is not installed.",
                            );
                            ui.hyperlink_to(
                                "Download Docker Desktop",
                                "https://www.docker.com/products/docker-desktop/",
                            );
                        }
                        harness_hat::desktop::LauncherReadiness::DockerNotRunning(reason) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 110, 30),
                                "Docker Desktop is installed but is not running.",
                            );
                            ui.label(egui::RichText::new(reason).small().weak());
                            if ui.button("Start Docker Desktop").clicked() {
                                match harness_hat::desktop::start_docker_desktop() {
                                    Ok(()) => retry = true,
                                    Err(error) => {
                                        self.session_error = Some(format!("{error:#}"));
                                    }
                                }
                            }
                        }
                        harness_hat::desktop::LauncherReadiness::OpenSshMissing => {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 55, 55),
                                "The OpenSSH client is required for protected Claude sessions.",
                            );
                            #[cfg(target_os = "windows")]
                            ui.hyperlink_to(
                                "Install OpenSSH Client",
                                "https://learn.microsoft.com/windows-server/administration/openssh/openssh_install_firstuse",
                            );
                            #[cfg(target_os = "macos")]
                            ui.label("Install or restore the macOS OpenSSH tools, then try again.");
                        }
                        harness_hat::desktop::LauncherReadiness::ClaudeMissing => {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 55, 55),
                                "Claude Desktop is required but was not found.",
                            );
                            ui.hyperlink_to(
                                "Download Claude Desktop",
                                "https://claude.com/download",
                            );
                        }
                        harness_hat::desktop::LauncherReadiness::BundleIncomplete(missing) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 55, 55),
                                "This Harness Hat download is incomplete.",
                            );
                            ui.label(format!("Missing: {missing}"));
                            ui.label("Download and extract the complete Harness Hat package, then run the launcher again.");
                        }
                        harness_hat::desktop::LauncherReadiness::Error(error) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(190, 55, 55),
                                "Harness Hat could not finish setup.",
                            );
                            ui.label(error);
                        }
                        harness_hat::desktop::LauncherReadiness::NeedsSetup(reason) => {
                            ui.label(reason);
                            if ui.button("Set up Harness Hat").clicked() {
                                self.start_service_install(context);
                            }
                        }
                        harness_hat::desktop::LauncherReadiness::Ready => {}
                    }
                    ui.add_space(12.0);
                    if ui.button("Check again").clicked() {
                        retry = true;
                    }
                    if retry {
                        self.start_setup_check(context);
                    }
                    if let Some(error) = &self.session_error {
                        ui.colored_label(egui::Color32::from_rgb(190, 55, 55), error);
                    }
                }
                SetupState::Ready => {}
            }
        }
    }

    fn load_session_rows() -> Result<Vec<SessionRow>> {
        let folders = harness_hat::manager::default_home_config_path()
            .ok()
            .and_then(|path| harness_hat::config::load(&path).ok())
            .map(|config| {
                config
                    .workspaces
                    .into_iter()
                    .map(|workspace| {
                        (
                            workspace.name,
                            workspace.canonical_path.display().to_string(),
                        )
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        harness_hat::desktop::running_hat_containers()?
            .into_iter()
            .map(|container| {
                let ssh_connected = container
                    .desktop_enabled
                    .then(|| harness_hat::desktop::ssh_connected(&container.name).ok())
                    .flatten();
                let ssh_port = container
                    .desktop_enabled
                    .then(|| harness_hat::desktop::published_ssh_port(&container.name).ok())
                    .flatten();
                let folder = folders
                    .get(&container.workspace)
                    .cloned()
                    .unwrap_or_else(|| container.mount_target.clone());
                Ok(SessionRow {
                    container,
                    folder,
                    ssh_connected,
                    ssh_port,
                })
            })
            .collect()
    }

    fn launch_workspace(
        project: PathBuf,
        template: String,
        config: PathBuf,
    ) -> Result<ConnectionGuide> {
        let exit = harness_hat::workspace::run(
            Vec::new(),
            false,
            Some(template),
            None,
            false,
            false,
            Some(project.clone()),
            true,
            true,
            false,
            None,
            Some(config.clone()),
        )?;
        anyhow::ensure!(
            exit == 0,
            "the workspace launcher exited with status {exit}"
        );
        Ok(ConnectionGuide)
    }

    fn suggested_template(project: &Path) -> (String, String) {
        if let Some(saved) = saved_template(project) {
            return (
                saved.clone(),
                format!(
                    "Using the environment already saved for this workspace ({saved}). You can change it above."
                ),
            );
        }
        let detected = detect_template(project);
        if detected == "default" {
            (
                detected.into(),
                "No dominant language detected; General is a safe starting point.".into(),
            )
        } else {
            (
                detected.into(),
                format!("Suggested from files in this project: {detected}."),
            )
        }
    }

    fn saved_template(project: &Path) -> Option<String> {
        let config_path = harness_hat::manager::default_home_config_path().ok()?;
        let config = harness_hat::config::load(&config_path).ok()?;
        let workspace = config
            .workspaces
            .iter()
            .filter(|workspace| project.starts_with(&workspace.canonical_path))
            .max_by_key(|workspace| workspace.canonical_path.components().count())?;
        let saved = workspace.template.clone().or_else(|| {
            harness_hat::rules::load(&workspace.canonical_path.join("harness-rules.toml"))
                .ok()
                .and_then(|rules| rules.template)
        })?;
        TEMPLATES
            .iter()
            .any(|template| template.id == saved)
            .then_some(saved)
    }

    fn detect_template(project: &Path) -> &'static str {
        let marker = |name: &str| project.join(name).exists();
        if marker("Cargo.toml") {
            return "rust";
        }
        if marker("go.mod") || marker("go.work") {
            return "go";
        }
        if marker("tsconfig.json") || marker("package.json") {
            return "typescript";
        }
        if marker("pyproject.toml") || marker("requirements.txt") || marker("Pipfile") {
            return "python";
        }
        if marker("AndroidManifest.xml") || marker("app/src/main/AndroidManifest.xml") {
            return "android";
        }
        if marker("build.gradle.kts") || marker("settings.gradle.kts") {
            return "kotlin";
        }
        if marker("composer.json") {
            return "php";
        }
        if std::fs::read_dir(project)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| matches!(extension, "sln" | "csproj"))
            })
        {
            return "csharp";
        }

        let mut scores = [0u16; 8];
        for entry in ignore::WalkBuilder::new(project)
            .max_depth(Some(4))
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .take(2_000)
        {
            match entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
            {
                Some("rs") => scores[0] += 1,
                Some("go") => scores[1] += 1,
                Some("ts" | "tsx" | "js" | "jsx") => scores[2] += 1,
                Some("py") => scores[3] += 1,
                Some("kt" | "kts") => scores[4] += 1,
                Some("php") => scores[5] += 1,
                Some("cs") => scores[6] += 1,
                _ => {}
            }
        }
        let templates = [
            "rust",
            "go",
            "typescript",
            "python",
            "kotlin",
            "php",
            "csharp",
            "default",
        ];
        scores
            .iter()
            .enumerate()
            .max_by_key(|(_, score)| *score)
            .filter(|(_, score)| **score > 0)
            .map_or("default", |(index, _)| templates[index])
    }

    fn template_index(id: &str) -> usize {
        TEMPLATES
            .iter()
            .position(|template| template.id == id)
            .unwrap_or(0)
    }

    fn make_icon_background_transparent(icon: &mut egui::IconData) {
        let width = icon.width as usize;
        let height = icon.height as usize;
        let radius = icon.width.min(icon.height) as f32 * 224.0 / 1024.0;
        for y in 0..height {
            for x in 0..width {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let dx = if px < radius {
                    radius - px
                } else if px > icon.width as f32 - radius {
                    px - (icon.width as f32 - radius)
                } else {
                    0.0
                };
                let dy = if py < radius {
                    radius - py
                } else if py > icon.height as f32 - radius {
                    py - (icon.height as f32 - radius)
                } else {
                    0.0
                };
                if dx == 0.0 || dy == 0.0 {
                    continue;
                }
                let coverage = (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0);
                if coverage < 1.0 {
                    let pixel = &mut icon.rgba[(y * width + x) * 4..][..4];
                    pixel[..3].copy_from_slice(&[0x11, 0x18, 0x27]);
                    pixel[3] = (coverage * 255.0).round() as u8;
                }
            }
        }
    }

    pub fn run() -> Result<()> {
        super::add_gui_tool_paths();
        let mut icon =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/harness-hat-1024.png"))
                .map_err(|error| anyhow::anyhow!("loading the Harness Hat app icon: {error}"))?;
        make_icon_background_transparent(&mut icon);
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([620.0, 670.0])
                .with_min_inner_size([520.0, 520.0])
                .with_icon(icon),
            ..Default::default()
        };
        eframe::run_native(
            "Harness Hat",
            options,
            Box::new(|_| Ok(Box::<Launcher>::default())),
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() {
    if let Err(error) = gui::run() {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Harness Hat could not start")
            .set_description(format!("{error:#}"))
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("the graphical Harness Hat launcher currently supports macOS and Windows");
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn add_gui_tool_paths() {
    use std::path::PathBuf;
    let mut paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    let mut candidates = Vec::new();
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ]);
    #[cfg(target_os = "windows")]
    {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join("Docker/Docker/resources/bin"));
        }
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            candidates.push(PathBuf::from(system_root).join("System32/OpenSSH"));
        }
    }
    for candidate in candidates {
        if candidate.is_dir() && !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
    if let Ok(path) = std::env::join_paths(paths) {
        // Called before eframe starts worker threads.
        unsafe { std::env::set_var("PATH", path) };
    }
}

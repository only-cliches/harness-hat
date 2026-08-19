//! `hat ws` — attach to (or start) a session for `$PWD`.
//!
//! This subcommand needs a running manager (the `hat` default action) to do
//! anything useful: it discovers the manager via the same config file the
//! manager itself loaded, then either docker-execs into an existing session
//! for the matched workspace or asks the manager (via `POST /workspace/launch`)
//! to spin one up. New workspaces receive a `harness-rules.toml` and are
//! appended to `harness-hat.toml`; the manager reloads config on each launch
//! request so they show up in the TUI sidebar on the same tick.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::{Config, ContainerDef, WorkspaceConfig};

/// Entry point for workspace/session launch. `explicit_config` is reserved for
/// the daemon-attached client so it uses the service's exact config.
pub fn run(
    args: Vec<OsString>,
    list: bool,
    template_override: Option<String>,
    name_override: Option<String>,
    force_rebuild: bool,
    force_new: bool,
    explicit_path: Option<PathBuf>,
    launch_only: bool,
    desktop: bool,
    open_desktop: bool,
    open_editor: Option<crate::cli::OpenEditor>,
    explicit_config: Option<PathBuf>,
) -> Result<i32> {
    let config_path = crate::manager::resolve_or_prompt_config_path(explicit_config)?
        .with_context(|| {
            format!(
                "no harness-hat config available; run `{} init` to create one",
                crate::cli::COMMAND_NAME
            )
        })?;
    let mut config = crate::config::load(&config_path)?;

    if list {
        println!("{}", format_workspace_list(&config)?);
        return Ok(0);
    }

    if which::which("docker").is_err() {
        bail!(
            "docker not found in PATH — `{} ws` requires Docker",
            crate::cli::COMMAND_NAME
        );
    }

    let token_path = config.logging.log_dir.join("token");
    let token = read_manager_token(&token_path).with_context(|| {
        format!(
            "could not read manager token at {} — is the manager running?",
            token_path.display()
        )
    })?;

    let control_url = format!(
        "http://{}:{}",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    probe_manager(&control_url).with_context(|| {
        format!(
            "manager is not reachable at {control_url} — start it in another terminal with `{}`",
            crate::cli::COMMAND_NAME
        )
    })?;

    let pwd = explicit_path
        .unwrap_or(std::env::current_dir().context("getting current working directory")?)
        .canonicalize()
        .context("canonicalizing workspace directory")?;

    // Match workspace by --name flag first, then by cwd.
    let matched = if let Some(ref n) = name_override {
        config.workspaces.iter().find(|w| w.name == *n).cloned()
    } else {
        best_matching_workspace(&config, &pwd).cloned()
    };

    if let Some(matched) = matched {
        status(
            launch_only,
            format!(
                "using workspace \"{}\" at {}",
                matched.name,
                matched.canonical_path.display()
            ),
        );
        let reusable_session = if force_new {
            None
        } else {
            newest_session_for_workspace(&matched.name)?
        };
        if let Some(session) = reusable_session
            && (!desktop || crate::desktop::is_desktop_container(&session.name)?)
        {
            status(
                launch_only,
                format!("attaching to running session {}", session.alias),
            );
            let mount_target = mount_target_for_session(&session, &config);
            let workdir = workspace_workdir(&matched, &pwd, &mount_target);
            if desktop {
                if open_desktop {
                    crate::desktop::open(
                        &session.name,
                        &matched.name,
                        &workdir,
                        &config.logging.log_dir,
                    )?;
                } else {
                    crate::desktop::prepare(&session.name, &matched.name, &config.logging.log_dir)?;
                }
                return Ok(0);
            }
            if let Some(editor) = open_editor {
                crate::shell::open(&session.alias, editor)?;
                return Ok(0);
            }
            return attach_and_report(&session.name, &args, Some(&workdir));
        }
        let rules_path = workspace_rules_path(&matched.canonical_path);
        let rules = crate::rules::load(&rules_path)?;
        // Command-line selection takes precedence, followed by an explicit
        // primary-config override, then the workspace-local remembered value.
        let effective_override = template_override
            .as_deref()
            .or(matched.template.as_deref())
            .or(rules.template.as_deref());
        let template = choose_template(
            &config,
            matched.canonical_path.as_path(),
            effective_override,
            launch_only,
        )?;
        let force_rebuild = force_rebuild
            || (desktop
                && desktop_image_needs_rebuild(&config, &matched.canonical_path, &template)?);
        // Primary config values are explicit overrides. All remembered choices
        // belong to the workspace and are persisted in harness-rules.toml.
        if matched.template.is_none() && rules.template.as_deref() != Some(&template) {
            save_workspace_template(&rules_path, &template)?;
        }
        let mut terminal_env = crate::shell::shell_exec_env_pairs_with_passthrough(
            env_passthrough_for_template(&config, &template),
        );
        if desktop {
            terminal_env.push(crate::desktop::authorized_key_env(&config.logging.log_dir)?);
        }
        let launch_cwd = matched.mount_cwd.then(|| pwd.to_str()).flatten();
        let resp = post_launch(
            &control_url,
            &token,
            &matched.name,
            &template,
            force_rebuild,
            launch_cwd,
            terminal_env,
        )?;
        status(
            launch_only,
            format!("launched session {} ({})", resp.alias, resp.docker_name),
        );
        wait_for_container_running(&resp.docker_name, Duration::from_secs(15))?;
        if launch_only && !desktop {
            println!("{}", resp.alias);
            return Ok(0);
        }
        if let Some(editor) = open_editor {
            crate::shell::open(&resp.alias, editor)?;
            return Ok(0);
        }
        // `mount_cwd` deliberately mounts a subdirectory as the mount root, so
        // relative-to-workspace translation is not safe for that legacy mode.
        let workdir = if matched.mount_cwd {
            resp.mount_target.clone()
        } else {
            workspace_workdir(&matched, &pwd, &resp.mount_target)
        };
        if desktop {
            if open_desktop {
                crate::desktop::open(
                    &resp.docker_name,
                    &matched.name,
                    &workdir,
                    &config.logging.log_dir,
                )?;
            } else {
                crate::desktop::prepare(&resp.docker_name, &matched.name, &config.logging.log_dir)?;
            }
            return Ok(0);
        }
        return attach_and_report(&resp.docker_name, &args, Some(&workdir));
    }

    let base = pwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace");
    let workspace_name = dedupe_workspace_name(&config, base);
    status(
        launch_only,
        format!(
            "no configured workspace matches {} — adding \"{}\" to {}",
            pwd.display(),
            workspace_name,
            config_path.display()
        ),
    );
    if crate::new_project::write_rules_if_missing(&pwd, crate::new_project::ProjectType::None)? {
        status(
            launch_only,
            format!("created {}", pwd.join("harness-rules.toml").display()),
        );
    }
    crate::new_project::append_project_block(&config_path, &workspace_name, &pwd, None)?;
    config.workspaces.push(WorkspaceConfig {
        name: workspace_name.clone(),
        canonical_path: pwd.clone(),
        sidebar_hotkey: None,
        template: None,
        mount_cwd: false,
    });
    let template = choose_template(
        &config,
        pwd.as_path(),
        template_override.as_deref(),
        launch_only,
    )?;
    let force_rebuild =
        force_rebuild || (desktop && desktop_image_needs_rebuild(&config, &pwd, &template)?);
    save_workspace_template(&workspace_rules_path(&pwd), &template)?;
    let mut terminal_env = crate::shell::shell_exec_env_pairs_with_passthrough(
        env_passthrough_for_template(&config, &template),
    );
    if desktop {
        terminal_env.push(crate::desktop::authorized_key_env(&config.logging.log_dir)?);
    }
    let resp = post_launch(
        &control_url,
        &token,
        &workspace_name,
        &template,
        force_rebuild,
        None,
        terminal_env,
    )?;
    status(
        launch_only,
        format!("launched session {} ({})", resp.alias, resp.docker_name),
    );
    wait_for_container_running(&resp.docker_name, Duration::from_secs(15))?;
    if launch_only && !desktop {
        println!("{}", resp.alias);
        return Ok(0);
    }
    if let Some(editor) = open_editor {
        crate::shell::open(&resp.alias, editor)?;
        return Ok(0);
    }
    if desktop {
        if open_desktop {
            crate::desktop::open(
                &resp.docker_name,
                &workspace_name,
                &resp.mount_target,
                &config.logging.log_dir,
            )?;
        } else {
            crate::desktop::prepare(&resp.docker_name, &workspace_name, &config.logging.log_dir)?;
        }
        return Ok(0);
    }
    attach_and_report(&resp.docker_name, &args, Some(&resp.mount_target))
}

fn status(launch_only: bool, message: String) {
    if launch_only {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

/// Request the daemon's session-preserving backend refresh. This is not a
/// service/binary restart: running `docker run --rm -it` sessions remain live.
pub fn restart() -> Result<()> {
    // Match normal `hat` daemon attachment: the installed service owns the
    // global config even if the caller happens to be inside a project with a
    // local config. Fall back to ordinary discovery for standalone managers.
    let config_path = crate::manager::default_home_config_path()
        .ok()
        .filter(|path| path.exists())
        .or(crate::manager::resolve_or_prompt_config_path(None)?)
        .with_context(|| {
            format!(
                "no harness-hat config available; run `{} init` first",
                crate::cli::COMMAND_NAME
            )
        })?;
    let config = crate::config::load(&config_path)?;
    let token = read_manager_token(&config.logging.log_dir.join("token"))
        .with_context(|| "could not read manager token â€” is the daemon running?")?;
    let control_url = format!(
        "http://{}:{}",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    probe_manager(&control_url).with_context(|| {
        format!(
            "daemon is not reachable at {control_url}; start it with `{}`",
            crate::cli::COMMAND_NAME
        )
    })?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("building http client")?;
    let response = client
        .post(format!("{control_url}/daemon/restart"))
        .bearer_auth(token)
        .send()
        .context("posting /daemon/restart")?;
    let status = response.status();
    let body = response
        .bytes()
        .context("reading daemon refresh response")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<LaunchError>(&body) {
            bail!("daemon refresh failed ({status}): {}", error.reason);
        }
        bail!(
            "daemon refresh failed ({status}): {}",
            String::from_utf8_lossy(&body).trim()
        );
    }
    #[derive(Deserialize)]
    struct RestartResponse {
        message: String,
    }
    let response: RestartResponse =
        serde_json::from_slice(&body).context("parsing daemon refresh response")?;
    println!("{}", response.message);
    Ok(())
}

fn format_workspace_list(config: &Config) -> Result<String> {
    if config.workspaces.is_empty() {
        return Ok("No workspaces configured.".to_string());
    }

    let entries = config
        .workspaces
        .iter()
        .map(|workspace| -> Result<String> {
            let template = match workspace.template.as_deref() {
                Some(template) => template.to_string(),
                None => crate::rules::load(&workspace_rules_path(&workspace.canonical_path))?
                    .template
                    .unwrap_or_else(|| "<not selected>".to_string()),
            };
            Ok(format!(
                "- {}\n  path: {}\n  template: {}",
                workspace.name,
                workspace.canonical_path.display(),
                template
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    Ok(format!("Configured workspaces:\n{entries}"))
}

/// Poll `docker inspect` until the named container reports State.Running=true,
/// or until `timeout` elapses. The manager replies `launched` as soon as
/// `docker run` has *created* the container (the cidfile is written before
/// the container's main process is started), and `docker exec` against a
/// not-yet-running container fails with "container is not running" — so the
/// CLI would silently exit non-zero and the user would land back at their
/// host prompt. Polling closes that race.
fn wait_for_container_running(docker_name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut command = Command::new("docker");
        crate::process_util::hide_console_window(&mut command);
        let output = command
            .args(["inspect", "-f", "{{.State.Running}}", docker_name])
            .output()
            .context("running docker inspect to wait for container readiness")?;
        if output.status.success() {
            let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if state == "true" {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "container '{docker_name}' did not enter running state within {:?}: {}",
                timeout,
                stderr.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wrap `shell::exec_into_container` and emit a stderr line on non-zero
/// exits. Without this, a docker-exec failure (e.g. "container is not
/// running", or a missing command in the trailing args) silently inherits
/// the host shell, and the user can't tell whether the attach happened or
/// failed.
fn attach_and_report(docker_name: &str, args: &[OsString], workdir: Option<&str>) -> Result<i32> {
    let code = crate::shell::exec_into_container_at(docker_name, args, workdir)?;
    if code != 0 {
        let _ = writeln!(
            io::stderr(),
            "docker exec into '{docker_name}' exited with status {code}"
        );
    }
    Ok(code)
}

// ── PWD ↔ workspace matching ────────────────────────────────────────────────

/// Return the workspace whose `canonical_path` is the longest prefix of `pwd`.
/// Both sides are canonicalized at config-load time (and by the caller for
/// `pwd`), so a plain `starts_with` comparison is safe.
pub(crate) fn best_matching_workspace<'a>(
    config: &'a Config,
    pwd: &Path,
) -> Option<&'a WorkspaceConfig> {
    config
        .workspaces
        .iter()
        .filter(|w| pwd.starts_with(&w.canonical_path))
        .max_by_key(|w| w.canonical_path.components().count())
}

fn workspace_rules_path(workspace_path: &Path) -> PathBuf {
    workspace_path.join("harness-rules.toml")
}

fn save_workspace_template(rules_path: &Path, template: &str) -> Result<()> {
    if !rules_path.exists() {
        let mut rules = crate::rules::ProjectRules::default();
        rules.template = Some(template.to_string());
        return crate::rules::write_rules_file(rules_path, &rules);
    }

    let raw = std::fs::read_to_string(rules_path)?;
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    doc["template"] = toml_edit::value(template);
    crate::config::atomic_write_with_lock(rules_path, doc.to_string().as_bytes())?;
    Ok(())
}

/// Dedupe a candidate workspace name against `config.workspaces` by appending
/// `-2`, `-3`, … until a free slot is found. Mirrors what users would write by
/// hand when two repos share a directory basename.
pub(crate) fn dedupe_workspace_name(config: &Config, base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "workspace" } else { base };
    let used: std::collections::HashSet<&str> =
        config.workspaces.iter().map(|w| w.name.as_str()).collect();
    if !used.contains(base) {
        return base.to_string();
    }
    for n in 2..u32::MAX {
        let candidate = format!("{base}-{n}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
    }
    // Effectively unreachable, but don't return a clashing name.
    format!("{base}-{}", uuid::Uuid::new_v4().simple())
}

// ── Session discovery ────────────────────────────────────────────────────────

fn newest_session_for_workspace(workspace_name: &str) -> Result<Option<crate::shell::Session>> {
    let mut sessions = crate::shell::running_sessions()?;
    // `docker ps` returns newest-first; preserve that order, just filter.
    sessions.retain(|s| s.workspace == workspace_name);
    Ok(sessions.into_iter().next())
}

/// Resolve the mount target for a discovered session. New sessions carry an
/// authoritative label; older sessions get a best-effort lookup from the
/// current template configuration before falling back to the historic default.
fn mount_target_for_session(session: &crate::shell::Session, config: &Config) -> String {
    if let Some(target) = session.mount_target.as_deref() {
        return target.to_string();
    }
    let configured = config
        .workspaces
        .iter()
        .find(|workspace| workspace.name == session.workspace)
        .and_then(|workspace| {
            crate::config::resolve_workspace_container_templates(
                &workspace.canonical_path,
                &config.defaults.containers,
                &config.containers,
            )
            .ok()
            .and_then(|templates| {
                templates
                    .iter()
                    .find(|template| template.name == session.template)
                    .map(|template| crate::config::container_path_string(&template.mount_target))
            })
        })
        .or_else(|| {
            config
                .containers
                .iter()
                .find(|template| template.name == session.template)
                .map(|template| crate::config::container_path_string(&template.mount_target))
        });
    configured.unwrap_or_else(|| "/workspace".to_string())
}

/// Translate a canonical host cwd into an in-container directory beneath the
/// actual workspace mount. A named workspace selected from elsewhere safely
/// falls back to its mount root.
pub(crate) fn workspace_workdir(
    workspace: &WorkspaceConfig,
    cwd: &Path,
    mount_target: &str,
) -> String {
    let target = mount_target.trim_end_matches('/');
    let target = if target.is_empty() { "/" } else { target };
    let Ok(relative) = cwd.strip_prefix(&workspace.canonical_path) else {
        return target.to_string();
    };
    let components: Option<Vec<&str>> = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            std::path::Component::CurDir => Some(""),
            _ => None,
        })
        .collect();
    let Some(components) = components else {
        return target.to_string();
    };
    let suffix = components
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if suffix.is_empty() || target == "/" {
        if suffix.is_empty() {
            target.to_string()
        } else {
            format!("/{suffix}")
        }
    } else {
        format!("{target}/{suffix}")
    }
}

/// Look up `env_passthrough` for a chosen template so the caller's live
/// process env can be captured for those names (see
/// `shell::shell_exec_env_pairs_with_passthrough`). Falls back to
/// `defaults.containers.env_passthrough` for workspace-local templates (see
/// `resolve_workspace_container_templates`), which aren't in `config.containers`
/// but still inherit the default passthrough list. env_passthrough is
/// best-effort here regardless — the daemon's own `docker run -e NAME`
/// fallback (reading its own process env) still applies.
fn env_passthrough_for_template<'a>(config: &'a Config, template: &str) -> &'a [String] {
    config
        .containers
        .iter()
        .find(|c| c.name == template)
        .map(|c| c.env_passthrough.as_slice())
        .unwrap_or(&config.defaults.containers.env_passthrough)
}

fn desktop_image_needs_rebuild(
    config: &Config,
    workspace_path: &Path,
    template: &str,
) -> Result<bool> {
    let templates = crate::config::resolve_workspace_container_templates(
        workspace_path,
        &config.defaults.containers,
        &config.containers,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "failed to resolve Desktop image for {}: {error}",
            workspace_path.display()
        )
    })?;
    let image = templates
        .iter()
        .find(|container| container.name == template)
        .map(|container| container.image.as_str())
        .with_context(|| format!("no container template named {template:?}"))?;
    Ok(!crate::desktop::image_supports_desktop_ssh(image))
}

// ── Template picker ─────────────────────────────────────────────────────────

fn choose_template(
    config: &Config,
    workspace_path: &Path,
    override_name: Option<&str>,
    launch_only: bool,
) -> Result<String> {
    let templates = crate::config::resolve_workspace_container_templates(
        workspace_path,
        &config.defaults.containers,
        &config.containers,
    )
    .map_err(|e| anyhow::anyhow!("failed to scan {} templates: {e}", workspace_path.display()))?;

    if let Some(name) = override_name {
        let name = name.trim();
        if templates.iter().any(|c| c.name == name) {
            return Ok(name.to_string());
        }
        let available = templates
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no container template named '{name}' in this config (available: {available})");
    }
    if templates.is_empty() {
        bail!("config has no [container_profiles.*] entries — add at least one before launching");
    }
    if templates.len() == 1 {
        return Ok(templates[0].name.clone());
    }
    if !io::stdin().is_terminal() {
        bail!(
            "multiple container templates configured and stdin is not a TTY; \
             re-run with `--template NAME` (available: {})",
            templates
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    prompt_for_template(&templates, launch_only)
}

fn prompt_for_template(containers: &[ContainerDef], launch_only: bool) -> Result<String> {
    let mut prompt: Box<dyn Write> = if launch_only {
        Box::new(io::stderr())
    } else {
        Box::new(io::stdout())
    };
    writeln!(
        prompt,
        "There is no running session for this workspace.  Choose a container template to start one:"
    )?;
    for (i, ctr) in containers.iter().enumerate() {
        writeln!(prompt, "  {}. {}", i + 1, ctr.name)?;
    }
    loop {
        write!(
            prompt,
            "Select [1-{}] (or 'q' to cancel): ",
            containers.len()
        )?;
        prompt.flush().ok();
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("reading template selection")?;
        let trimmed = input.trim();
        if trimmed.eq_ignore_ascii_case("q") {
            bail!("cancelled");
        }
        if let Ok(n) = trimmed.parse::<usize>() {
            if (1..=containers.len()).contains(&n) {
                return Ok(containers[n - 1].name.clone());
            }
        }
        writeln!(
            prompt,
            "invalid selection; enter a number between 1 and {}",
            containers.len()
        )?;
    }
}

// ── Manager IPC ─────────────────────────────────────────────────────────────

fn read_manager_token(path: &Path) -> Result<String> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let token = contents.trim().to_string();
    if token.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(token)
}

fn probe_manager(control_url: &str) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("building http client")?;
    let resp = client
        .get(format!("{control_url}/healthz"))
        .send()
        .context("connecting to manager")?;
    if resp.status().is_success() {
        return Ok(());
    }

    let status = resp.status();
    let body = resp.bytes().context("reading manager health response")?;
    if let Ok(error) = serde_json::from_slice::<LaunchError>(&body)
        && error.error == "docker_unavailable"
    {
        bail!(
            "daemon is running, but Docker is unavailable: {}",
            error.reason
        );
    }
    bail!("manager returned {status} on /healthz");
}

#[derive(Debug, Deserialize)]
struct LaunchResponse {
    alias: String,
    docker_name: String,
    #[serde(rename = "session_token")]
    _session_token: String,
    mount_target: String,
}

#[derive(Debug, Deserialize)]
struct LaunchError {
    #[serde(rename = "error")]
    error: String,
    reason: String,
}

/// One line of the NDJSON event stream returned by `POST /workspace/launch`.
/// Mirrors the server-side `LaunchEvent` enum.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamedEvent {
    Status { message: String },
    BuildOutput { line: String, is_error: bool },
    Launched(LaunchResponse),
    Error { reason: String },
}

fn post_launch(
    control_url: &str,
    token: &str,
    workspace_name: &str,
    template: &str,
    force_rebuild: bool,
    cwd: Option<&str>,
    terminal_env: Vec<(String, String)>,
) -> Result<LaunchResponse> {
    // No total-request or body-read timeout: a long or temporarily quiet build
    // (cold image pull + multi-stage) can legitimately take many minutes, and
    // reqwest's default timeout would abort the NDJSON stream while the TUI
    // continues building. Connect timeout still defends against a fully
    // unreachable manager. If the manager hangs after the initial response,
    // the user can ctrl-C.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(None)
        .build()
        .context("building http client")?;
    let mut body = serde_json::json!({
        "workspace_name": workspace_name,
        "template": template,
        "force_rebuild": force_rebuild,
    });
    if !terminal_env.is_empty() {
        body["terminal_env"] = serde_json::json!(terminal_env);
    }
    if let Some(cwd) = cwd {
        body["cwd"] = serde_json::Value::String(cwd.to_string());
    }
    let resp = client
        .post(format!("{control_url}/workspace/launch"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("posting /workspace/launch")?;
    let status = resp.status();
    if !status.is_success() {
        // Errors before streaming begins are returned as a regular JSON body
        // (handler `into_response()` paths), not NDJSON. Read the whole body
        // and try to parse the standard error shape; fall back to the raw
        // text so the user always sees something useful.
        let bytes = resp.bytes().context("reading error body")?;
        if let Ok(err) = serde_json::from_slice::<LaunchError>(&bytes) {
            bail!("manager refused launch ({status}): {}", err.reason);
        }
        bail!(
            "manager refused launch ({status}): {}",
            String::from_utf8_lossy(&bytes).trim()
        );
    }
    consume_launch_stream(resp)
}

/// Stream parser: read NDJSON lines, mirror status/build-output to stderr,
/// return the terminal `Launched` payload or bail with the `Error` reason.
fn consume_launch_stream(resp: reqwest::blocking::Response) -> Result<LaunchResponse> {
    let reader = BufReader::new(resp);
    for line in reader.lines() {
        let line = line.context("reading streamed launch event")?;
        if line.trim().is_empty() {
            continue;
        }
        let event: StreamedEvent = serde_json::from_str(&line).with_context(|| {
            format!(
                "parsing streamed launch event: {}",
                truncate_for_error(&line)
            )
        })?;
        match event {
            StreamedEvent::Status { message } => {
                let _ = writeln!(io::stderr(), "[{}]", message);
            }
            StreamedEvent::BuildOutput { line, is_error } => {
                // Mirror docker build output to the user's terminal. Both
                // stdout and stderr from the build are written to *our*
                // stderr so the eventual docker-exec attach gets a clean
                // stdout for the user's shell session.
                let mut sink = io::stderr();
                if is_error {
                    let _ = writeln!(sink, "build! {}", line);
                } else {
                    let _ = writeln!(sink, "build  {}", line);
                }
            }
            StreamedEvent::Launched(resp) => return Ok(resp),
            StreamedEvent::Error { reason } => bail!("launch failed: {reason}"),
        }
    }
    bail!("manager closed the launch stream without sending a terminal event")
}

fn truncate_for_error(s: &str) -> String {
    if s.len() <= 200 {
        s.to_string()
    } else {
        format!("{}…", &s[..200])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn workspace(name: &str, canonical_path: &str) -> WorkspaceConfig {
        WorkspaceConfig {
            name: name.to_string(),
            canonical_path: PathBuf::from(canonical_path),
            sidebar_hotkey: None,
            template: None,
            mount_cwd: false,
        }
    }

    #[test]
    fn best_matching_picks_longest_prefix() {
        let mut config = Config::default();
        config.workspaces = vec![
            workspace("outer", "/repos/proj"),
            workspace("inner", "/repos/proj/web"),
            workspace("other", "/repos/other"),
        ];
        let pwd = PathBuf::from("/repos/proj/web/src/components");
        let matched = best_matching_workspace(&config, &pwd).expect("matched");
        assert_eq!(matched.name, "inner");
    }

    #[test]
    fn workspace_list_includes_name_path_and_template() {
        let mut config = Config::default();
        config.workspaces = vec![WorkspaceConfig {
            name: "api".to_string(),
            canonical_path: PathBuf::from("/src/api"),
            sidebar_hotkey: None,
            template: Some("rust".to_string()),
            mount_cwd: false,
        }];

        assert_eq!(
            format_workspace_list(&config).unwrap(),
            "Configured workspaces:\n- api\n  path: /src/api\n  template: rust"
        );
    }

    #[test]
    fn workspace_list_reports_when_no_workspaces_are_configured() {
        assert_eq!(
            format_workspace_list(&Config::default()).unwrap(),
            "No workspaces configured."
        );
    }

    fn container_def_named(name: &str, env_passthrough: Vec<String>) -> ContainerDef {
        ContainerDef {
            name: name.to_string(),
            image: String::new(),
            image_stem: String::new(),
            dockerfile_path: None,
            profile: None,
            mount_target: PathBuf::from("/workspace"),
            command: None,
            grayscale_palette: false,
            starter_network_allowlist: Vec::new(),
            allowed_hosts: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            mounts: Vec::new(),
            env: std::collections::HashMap::new(),
            env_passthrough,
            localhost_forwards: Vec::new(),
            memory: None,
            cpus: None,
            shm_size: None,
            attach_shell: None,
            claude_settings: None,
        }
    }

    #[test]
    fn env_passthrough_for_template_uses_matching_container_def() {
        let mut config = Config::default();
        config.containers = vec![
            container_def_named("rust", vec!["FOO".to_string()]),
            container_def_named("go", vec!["BAR".to_string()]),
        ];

        assert_eq!(
            env_passthrough_for_template(&config, "rust"),
            &["FOO".to_string()]
        );
        assert_eq!(
            env_passthrough_for_template(&config, "go"),
            &["BAR".to_string()]
        );
    }

    #[test]
    fn env_passthrough_for_template_falls_back_to_defaults_for_local_templates() {
        let mut config = Config::default();
        config.containers = vec![container_def_named("rust", vec!["FOO".to_string()])];
        config.defaults.containers.env_passthrough = vec!["DEFAULT_VAR".to_string()];

        // "local" templates (workspace .dockerfile files) aren't materialized
        // into config.containers, so lookups by that name must still surface
        // the default passthrough list rather than silently returning none.
        assert_eq!(
            env_passthrough_for_template(&config, "local-thing"),
            &["DEFAULT_VAR".to_string()]
        );
    }

    #[test]
    fn workspace_template_is_saved_to_workspace_rules() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = workspace_rules_path(dir.path());

        save_workspace_template(&rules_path, "rust").unwrap();

        assert_eq!(
            crate::rules::load(&rules_path).unwrap().template.as_deref(),
            Some("rust")
        );
    }

    #[test]
    fn workspace_list_reads_template_from_workspace_rules() {
        let dir = tempfile::tempdir().unwrap();
        save_workspace_template(&workspace_rules_path(dir.path()), "python").unwrap();
        let mut config = Config::default();
        config.workspaces = vec![WorkspaceConfig {
            name: "api".to_string(),
            canonical_path: dir.path().to_path_buf(),
            sidebar_hotkey: None,
            template: None,
            mount_cwd: false,
        }];

        assert!(
            format_workspace_list(&config)
                .unwrap()
                .contains("template: python")
        );
    }

    #[test]
    fn best_matching_returns_none_when_no_workspace_contains_pwd() {
        let mut config = Config::default();
        config.workspaces = vec![workspace("only", "/repos/proj")];
        let pwd = PathBuf::from("/elsewhere");
        assert!(best_matching_workspace(&config, &pwd).is_none());
    }

    #[test]
    fn best_matching_matches_workspace_root_exactly() {
        let mut config = Config::default();
        config.workspaces = vec![workspace("proj", "/repos/proj")];
        let pwd = PathBuf::from("/repos/proj");
        let matched = best_matching_workspace(&config, &pwd).expect("matched");
        assert_eq!(matched.name, "proj");
    }

    #[test]
    fn workspace_workdir_maps_root_nested_and_named_workspace_fallback() {
        let workspace = workspace("proj", "/repos/proj");
        assert_eq!(
            workspace_workdir(&workspace, Path::new("/repos/proj"), "/work"),
            "/work"
        );
        assert_eq!(
            workspace_workdir(&workspace, Path::new("/repos/proj/src/lib"), "/custom"),
            "/custom/src/lib"
        );
        assert_eq!(
            workspace_workdir(&workspace, Path::new("/elsewhere"), "/custom"),
            "/custom"
        );
    }

    #[test]
    fn legacy_mount_target_uses_template_then_workspace_default() {
        let mut config = Config::default();
        config.workspaces = vec![workspace("proj", "/repos/proj")];
        let template = ContainerDef {
            name: "dev".into(),
            image: String::new(),
            image_stem: String::new(),
            dockerfile_path: None,
            profile: None,
            mount_target: PathBuf::from("/custom"),
            command: None,
            grayscale_palette: false,
            starter_network_allowlist: vec![],
            allowed_hosts: vec![],
            mcp_log_paths: vec![],
            mcp_log_pattern: None,
            mounts: vec![],
            env: Default::default(),
            env_passthrough: vec![],
            localhost_forwards: vec![],
            memory: None,
            cpus: None,
            shm_size: None,
            attach_shell: None,
            claude_settings: None,
        };
        config.containers = vec![template];
        let legacy = crate::shell::Session {
            alias: "1".into(),
            container_id: String::new(),
            workspace: "proj".into(),
            template: "dev".into(),
            name: String::new(),
            mount_target: None,
        };
        assert_eq!(mount_target_for_session(&legacy, &config), "/custom");
        let unknown = crate::shell::Session {
            template: "none".into(),
            ..legacy
        };
        assert_eq!(mount_target_for_session(&unknown, &config), "/workspace");
    }

    #[test]
    fn dedupe_workspace_name_passes_through_free_name() {
        let config = Config::default();
        assert_eq!(dedupe_workspace_name(&config, "app"), "app");
    }

    #[test]
    fn dedupe_workspace_name_appends_suffix_on_collision() {
        let mut config = Config::default();
        config.workspaces = vec![workspace("app", "/a"), workspace("app-2", "/b")];
        assert_eq!(dedupe_workspace_name(&config, "app"), "app-3");
    }

    #[test]
    fn dedupe_workspace_name_falls_back_to_workspace_for_blank_basename() {
        let config = Config::default();
        assert_eq!(dedupe_workspace_name(&config, "  "), "workspace");
        assert_eq!(dedupe_workspace_name(&config, ""), "workspace");
    }
}

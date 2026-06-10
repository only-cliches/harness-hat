//! `hh shell [--template TEMPLATE] [ARGS...]`
//!
//! Boots a container in the current directory, mounts the workspace to
//! /workspace, runs the proxy, and execs the given command (default: bash).
//!
//! Template resolution order:
//!   1. --template CLI flag
//!   2. [container].template in harness-rules.toml
//!   3. Interactive picker (when more than one template is available)
//!
//! A lockfile at {state_dir}/harness-hat/locks/{sha256(cwd)}.lock prevents
//! two concurrent invocations in the same directory. The lock is released
//! automatically when the process exits (including on crash).

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

const BASE_IMAGE_TAG: &str = "harness-hat-base:local";

pub async fn run(template: Option<String>, args: Vec<String>) -> Result<i32> {
    crate::container::ensure_docker_installed_and_running()?;

    let cwd = std::env::current_dir()
        .context("reading current directory")?
        .canonicalize()
        .context("canonicalizing current directory")?;

    let _lock = acquire_lockfile(&cwd)?;

    let docker_dir = resolve_docker_dir(&cwd);
    crate::init::ensure_docker_assets(&docker_dir).context("setting up docker assets")?;

    let rules = crate::rules::load(&cwd.join("harness-rules.toml"))?;

    let template_name = resolve_template(
        template.as_deref(),
        rules.container.template.as_deref(),
        &docker_dir,
    )
    .await?;

    let image = format!("harness-hat-{template_name}:local");

    if !docker_image_exists(&image)? {
        let docker_dir_clone = docker_dir.clone();
        let image_clone = image.clone();
        let built = tokio::task::spawn_blocking(move || {
            prompt_and_build(&image_clone, &template_name, &docker_dir_clone)
        })
        .await??;
        if !built {
            return Ok(1);
        }
    }

    let state_dir = shell_state_dir()?;
    let ca = Arc::new(
        crate::ca::CaStore::load_or_create(&state_dir.join("ca"))
            .context("setting up CA store")?,
    );
    let ca_cert_path = state_dir.join("ca").join("ca.crt");

    let workspace_name = cwd
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let config = build_shell_config(&docker_dir, &cwd, &workspace_name, &state_dir);
    let shared_config = crate::shared_config::SharedConfig::new(Arc::new(config));

    let (net_pending_tx, mut net_pending_rx) =
        mpsc::channel::<crate::proxy::PendingNetworkItem>(64);
    let (activity_tx, _activity_rx) = mpsc::channel(4096);

    let proxy_state = crate::proxy::ProxyState::new(
        ca,
        shared_config,
        net_pending_tx,
        activity_tx,
    )?;

    let session_token = Uuid::new_v4().simple().to_string();
    let container_name = format!("harness-hat-{}", Uuid::new_v4().simple());
    let scoped_proxy = crate::proxy::spawn_scoped_listener(
        &proxy_state,
        "127.0.0.1",
        &workspace_name,
        &container_name,
        &session_token,
        crate::proxy::SourcePriority::Primary,
    )?;

    tokio::spawn(async move {
        while let Some(item) = net_pending_rx.recv().await {
            eprintln!(
                "\nharness-hat: blocked: {} {} — add to [network].allowlist in harness-rules.toml to permit",
                item.method, item.host
            );
            let _ = item.response_tx.send(crate::proxy::NetworkDecision::Deny);
            for tx in item.merged_response_txs {
                let _ = tx.send(crate::proxy::NetworkDecision::Deny);
            }
        }
    });

    let command = if args.is_empty() {
        vec!["/bin/bash".to_string()]
    } else {
        args
    };

    launch_container(&image, &container_name, &command, &cwd, &scoped_proxy, &ca_cert_path).await
}

// ── Lockfile ──────────────────────────────────────────────────────────────────

fn acquire_lockfile(cwd: &Path) -> Result<File> {
    let lock_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("could not determine state directory")?
        .join("harness-hat")
        .join("locks");
    fs::create_dir_all(&lock_dir)
        .with_context(|| format!("creating lock directory {}", lock_dir.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(cwd.as_os_str().as_encoded_bytes());
    let lock_path = lock_dir.join(format!("{}.lock", hex::encode(hasher.finalize())));

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;

    match file.try_lock_exclusive() {
        Ok(()) => {
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            Ok(file)
        }
        Err(_) => {
            let mut pid = String::new();
            file.read_to_string(&mut pid).ok();
            let pid = pid.trim();
            if pid.is_empty() {
                bail!(
                    "harness-hat is already running in this directory.\n\
                     Use a different directory or wait for the other session to exit."
                );
            }
            bail!(
                "harness-hat is already running in this directory (pid {pid}).\n\
                 Use a different directory, or kill pid {pid} to force."
            );
        }
    }
}

// ── Paths ─────────────────────────────────────────────────────────────────────

fn shell_state_dir() -> Result<PathBuf> {
    let dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("could not determine state directory")?
        .join("harness-hat");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn resolve_docker_dir(cwd: &Path) -> PathBuf {
    let local = cwd.join("docker");
    if local.is_dir() {
        return local;
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
        .join("harness-hat")
        .join("docker")
}

// ── Template resolution ───────────────────────────────────────────────────────

async fn resolve_template(
    cli: Option<&str>,
    toml: Option<&str>,
    docker_dir: &Path,
) -> Result<String> {
    if let Some(t) = cli {
        ensure_template_dockerfile_exists(t, docker_dir)?;
        return Ok(t.to_string());
    }
    if let Some(t) = toml {
        ensure_template_dockerfile_exists(t, docker_dir)?;
        return Ok(t.to_string());
    }

    let templates = available_templates(docker_dir);
    if templates.is_empty() {
        bail!(
            "no container templates found in {}.\n\
             Run `hh init` to generate the default templates.",
            docker_dir.display()
        );
    }
    if templates.len() == 1 {
        eprintln!("Using template: {}", templates[0]);
        return Ok(templates[0].clone());
    }

    let selected =
        tokio::task::spawn_blocking(move || interactive_template_picker(&templates)).await??;
    selected.ok_or_else(|| anyhow::anyhow!("no template selected"))
}

fn ensure_template_dockerfile_exists(name: &str, docker_dir: &Path) -> Result<()> {
    let dockerfile = docker_dir.join(format!("{name}.dockerfile"));
    if dockerfile.exists() {
        return Ok(());
    }
    let available = available_templates(docker_dir).join(", ");
    bail!(
        "template '{name}' not found: {} does not exist.\n\
         Available templates: {available}",
        dockerfile.display(),
    )
}

fn available_templates(docker_dir: &Path) -> Vec<String> {
    let mut templates = Vec::new();
    if let Ok(entries) = fs::read_dir(docker_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("dockerfile") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem == "harness-hat-base" {
                continue;
            }
            templates.push(stem.to_string());
        }
    }
    templates.sort();
    templates
}

fn interactive_template_picker(templates: &[String]) -> Result<Option<String>> {
    let mut selected = 0usize;
    let mut stdout = io::stdout();

    terminal::enable_raw_mode().context("enabling raw mode for template picker")?;

    let result: Result<Option<String>> = (|| {
        execute!(stdout, cursor::Hide)?;
        loop {
            render_picker(&mut stdout, templates, selected)?;
            match event::read()? {
                Event::Key(k) => match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected + 1 < templates.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Enter => return Ok(Some(templates[selected].clone())),
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None)
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    })();

    let _ = execute!(stdout, cursor::Show, cursor::MoveToColumn(0));
    let _ = terminal::disable_raw_mode();
    result
}

fn render_picker(stdout: &mut io::Stdout, templates: &[String], selected: usize) -> Result<()> {
    execute!(
        stdout,
        cursor::MoveToColumn(0),
        terminal::Clear(ClearType::FromCursorDown),
        Print("Select container template  (↑↓ / j k  •  Enter to confirm  •  q to cancel)\r\n"),
    )?;
    for (i, name) in templates.iter().enumerate() {
        if i == selected {
            execute!(
                stdout,
                SetForegroundColor(Color::Cyan),
                Print(format!("  ❯ {name}\r\n")),
                ResetColor,
            )?;
        } else {
            execute!(stdout, Print(format!("    {name}\r\n")))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

// ── Docker image check and build ──────────────────────────────────────────────

fn docker_image_exists(image: &str) -> Result<bool> {
    let status = std::process::Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running docker image inspect")?;
    Ok(status.success())
}

/// Shows the user the build command and asks for confirmation. Streams output
/// if they confirm. Returns `true` if the image is now built.
fn prompt_and_build(image: &str, template: &str, docker_dir: &Path) -> Result<bool> {
    let (build_args, base_build_args) = docker_build_commands(docker_dir, image);
    let build_cmd = format!("docker {}", build_args.join(" "));

    eprintln!("\nImage '{image}' not found locally.\n");
    eprintln!("  1. Build now:");
    eprintln!("       {build_cmd}");
    eprintln!("  2. Cancel");
    eprint!("\nChoice [1]: ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    match input.trim() {
        "" | "1" => {}
        _ => {
            eprintln!("Cancelled.");
            return Ok(false);
        }
    }

    if let Some(base_args) = base_build_args {
        if !docker_image_exists(BASE_IMAGE_TAG)? {
            eprintln!("\nBuilding base image first (this may take a few minutes)...");
            eprintln!("$ docker {}", base_args.join(" "));
            run_docker_build(&base_args)?;
        }
    }

    eprintln!("\nBuilding template '{template}'...");
    eprintln!("$ {build_cmd}");
    run_docker_build(&build_args)
        .with_context(|| format!("docker build failed for template '{template}'"))?;

    Ok(true)
}

fn docker_build_commands(docker_dir: &Path, image: &str) -> (Vec<String>, Option<Vec<String>>) {
    let stem = dockerfile_stem_for_image(image);
    let dockerfile = docker_dir.join(format!("{stem}.dockerfile"));
    let cmd = vec![
        "build".to_string(),
        "-t".to_string(),
        image.to_string(),
        "-f".to_string(),
        dockerfile.display().to_string(),
        docker_dir.display().to_string(),
    ];
    let base_cmd = if image == BASE_IMAGE_TAG {
        None
    } else {
        Some(vec![
            "build".to_string(),
            "-t".to_string(),
            BASE_IMAGE_TAG.to_string(),
            "-f".to_string(),
            docker_dir
                .join("harness-hat-base.dockerfile")
                .display()
                .to_string(),
            docker_dir.display().to_string(),
        ])
    };
    (cmd, base_cmd)
}

fn dockerfile_stem_for_image(image: &str) -> String {
    let raw = image
        .split(':')
        .next()
        .unwrap_or(image)
        .split('/')
        .next_back()
        .unwrap_or(image);
    if raw == "harness-hat-base" {
        return "harness-hat-base".to_string();
    }
    raw.strip_prefix("harness-hat-").unwrap_or(raw).to_string()
}

fn run_docker_build(args: &[String]) -> Result<()> {
    let status = std::process::Command::new("docker")
        .args(args)
        .status()
        .context("running docker build")?;
    if !status.success() {
        bail!("docker build exited {}", status.code().unwrap_or(1));
    }
    Ok(())
}

// ── Minimal Config for proxy rule loading ─────────────────────────────────────

fn build_shell_config(
    docker_dir: &Path,
    cwd: &Path,
    workspace_name: &str,
    state_dir: &Path,
) -> crate::config::Config {
    use crate::config::{Config, LoggingConfig, WorkspaceConfig};
    Config {
        docker_dir: docker_dir.to_path_buf(),
        workspaces: vec![WorkspaceConfig {
            name: workspace_name.to_string(),
            canonical_path: cwd.to_path_buf(),
            sidebar_hotkey: None,
        }],
        logging: LoggingConfig {
            log_dir: state_dir.to_path_buf(),
            ..Default::default()
        },
        ..Config::default()
    }
}

// ── Container launch ──────────────────────────────────────────────────────────

async fn launch_container(
    image: &str,
    container_name: &str,
    command: &[String],
    cwd: &Path,
    scoped_proxy: &crate::proxy::ScopedProxyListener,
    ca_cert_path: &Path,
) -> Result<i32> {
    // Replace loopback with host.docker.internal so the proxy URL is
    // reachable from inside the container.
    let proxy_url = scoped_proxy
        .proxy_url()
        .replace("127.0.0.1", "host.docker.internal")
        .replace("//localhost", "//host.docker.internal");

    const CA_BUNDLE: &str = "/tmp/harness-hat-ca-bundle.crt";
    const NO_PROXY: &str = "localhost,127.0.0.1,host.docker.internal";

    let mut docker_args: Vec<String> = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        // Makes host.docker.internal resolve on Linux Docker Engine.
        "--add-host=host.docker.internal:host-gateway".to_string(),
        "-v".to_string(),
        format!("{}:/workspace", cwd.display()),
        "-w".to_string(),
        "/workspace".to_string(),
        "-v".to_string(),
        format!(
            "{}:/usr/local/share/ca-certificates/harness-hat-ca.crt:ro",
            ca_cert_path.display()
        ),
    ];

    for (k, v) in [
        ("HTTP_PROXY", proxy_url.as_str()),
        ("HTTPS_PROXY", proxy_url.as_str()),
        ("http_proxy", proxy_url.as_str()),
        ("https_proxy", proxy_url.as_str()),
        ("NO_PROXY", NO_PROXY),
        ("no_proxy", NO_PROXY),
        ("SSL_CERT_FILE", CA_BUNDLE),
        ("CURL_CA_BUNDLE", CA_BUNDLE),
        ("REQUESTS_CA_BUNDLE", CA_BUNDLE),
        ("AWS_CA_BUNDLE", CA_BUNDLE),
        ("NODE_EXTRA_CA_CERTS", CA_BUNDLE),
        ("DENO_CERT", CA_BUNDLE),
        ("GIT_SSL_CAINFO", CA_BUNDLE),
        ("GRPC_DEFAULT_SSL_ROOTS_FILE_PATH", CA_BUNDLE),
        ("CODEX_CA_CERTIFICATE", CA_BUNDLE),
    ] {
        docker_args.push("-e".to_string());
        docker_args.push(format!("{k}={v}"));
    }

    docker_args.push(image.to_string());
    docker_args.extend_from_slice(command);

    let status = tokio::process::Command::new("docker")
        .args(&docker_args)
        .status()
        .await
        .context("running docker run")?;

    Ok(status.code().unwrap_or(1))
}

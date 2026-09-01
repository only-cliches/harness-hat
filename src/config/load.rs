use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{
    fs,
    io::{self, BufRead, BufReader},
};
use toml_edit::{DocumentMut, value};
use tracing::instrument;

use crate::config::{
    Config, ContainerDefaults, ContainerMount, ContainerProfile, LocalhostForward, MountMode,
    WorkspaceConfig, container_path_string, default_mount_target, is_absolute_container_path,
};

const WORKSPACE_SIDEBAR_HOTKEY_POOL: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

// ── Rule loading ─────────────────────────────────────────────────────────────

/// Load and compose rules for a specific project (global + that project's
/// harness-rules.toml). Called at request time so edits take effect without
/// restart.
#[instrument(skip(config))]
pub fn load_composed_rules_for_workspace(
    config: &Config,
    project_name: Option<&str>,
) -> Result<crate::rules::ComposedRules> {
    let mut errors = Vec::new();

    let global = match crate::rules::load(&config.manager.global_rules_file) {
        Ok(rules) => rules,
        Err(e) => {
            errors.push(format!(
                "global rules '{}': {e}",
                config.manager.global_rules_file.display()
            ));
            crate::rules::ProjectRules::default()
        }
    };

    let mut proj_rules = Vec::new();
    if let Some(project_name) = project_name {
        if let Some(project) = config.workspaces.iter().find(|p| p.name == project_name) {
            let path = project.canonical_path.join("harness-rules.toml");
            match crate::rules::load(&path) {
                Ok(rules) => proj_rules.push(rules),
                Err(e) => {
                    errors.push(format!(
                        "workspace '{}' rules '{}': {e}",
                        project.name,
                        path.display()
                    ));
                }
            }
        }
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "failed to load one or more rule files:\n{}",
            errors.join("\n")
        );
    }

    Ok(crate::rules::ComposedRules::compose(&global, &proj_rules))
}

// ── Loading ──────────────────────────────────────────────────────────────────

/// Hard cap on config file size; configs are normally a few KiB. Refuse to
/// load anything larger (including `/dev/zero` via symlink) so a corrupted or
/// hostile file cannot trigger an OOM.
const CONFIG_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[instrument(skip(path))]
pub fn load(path: &Path) -> Result<Config> {
    let raw = read_config_to_string(path)?;
    let mut config: Config =
        toml::from_str(&raw).with_context(|| format!("parsing config: {}", path.display()))?;
    validate_config_version(config.version, path)?;
    expand_config_paths(&mut config)?;
    validate_docker_dir(&config, path)?;
    resolve_container_profiles(&mut config)?;
    canonicalize_workspace_paths(&mut config)?;
    validate(&config)?;
    ensure_logging_instance_id(path, &raw, &mut config)?;
    Ok(config)
}

fn read_config_to_string(path: &Path) -> Result<String> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).with_context(|| format!("opening config: {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("statting config: {}", path.display()))?;
    anyhow::ensure!(
        meta.is_file(),
        "config {}: not a regular file",
        path.display()
    );
    let mut out = String::new();
    file.take(CONFIG_MAX_BYTES + 1)
        .read_to_string(&mut out)
        .with_context(|| format!("reading config: {}", path.display()))?;
    anyhow::ensure!(
        (out.len() as u64) <= CONFIG_MAX_BYTES,
        "config {}: file exceeds {} bytes",
        path.display(),
        CONFIG_MAX_BYTES
    );
    Ok(out)
}

pub fn workspace_sidebar_hotkey_pool() -> &'static [char] {
    WORKSPACE_SIDEBAR_HOTKEY_POOL
}

pub fn normalize_workspace_sidebar_hotkey(raw: &str) -> Option<char> {
    let mut chars = raw.trim().chars();
    let ch = chars.next()?.to_ascii_lowercase();
    if chars.next().is_some() || !workspace_sidebar_hotkey_pool().contains(&ch) {
        return None;
    }
    Some(ch)
}

pub fn resolve_workspace_sidebar_hotkeys(workspaces: &[WorkspaceConfig]) -> Vec<Option<char>> {
    let mut out = vec![None; workspaces.len()];
    let mut used = std::collections::HashSet::new();

    for (idx, workspace) in workspaces.iter().enumerate() {
        let Some(raw) = workspace.sidebar_hotkey.as_deref() else {
            continue;
        };
        let Some(ch) = normalize_workspace_sidebar_hotkey(raw) else {
            continue;
        };
        if used.insert(ch) {
            out[idx] = Some(ch);
        }
    }

    for (idx, workspace) in workspaces.iter().enumerate() {
        if out[idx].is_some() {
            continue;
        }

        let preferred = workspace
            .name
            .chars()
            .map(|ch| ch.to_ascii_lowercase())
            .filter(|ch| workspace_sidebar_hotkey_pool().contains(ch));

        let fallback = workspace_sidebar_hotkey_pool().iter().copied();
        let choice = preferred.chain(fallback).find(|ch| used.insert(*ch));
        out[idx] = choice;
    }

    out
}

pub fn select_workspace_sidebar_hotkey(
    existing_workspaces: &[WorkspaceConfig],
    workspace_name: &str,
) -> Option<char> {
    let mut workspaces = existing_workspaces.to_vec();
    workspaces.push(WorkspaceConfig {
        name: workspace_name.to_string(),
        canonical_path: PathBuf::new(),
        sidebar_hotkey: None,
        template: None,
        mount_cwd: false,
    });
    resolve_workspace_sidebar_hotkeys(&workspaces)
        .into_iter()
        .last()
        .flatten()
}

fn validate_config_version(version: u32, path: &Path) -> Result<()> {
    anyhow::ensure!(
        version > 0,
        "config {}: version must be greater than zero",
        path.display()
    );
    anyhow::ensure!(
        version <= crate::config::CURRENT_CONFIG_VERSION,
        "config {}: unsupported version {}; this build supports up to {}",
        path.display(),
        version,
        crate::config::CURRENT_CONFIG_VERSION
    );
    Ok(())
}

/// Expand `~` in all path fields so downstream code always sees absolute paths.
fn expand_config_paths(config: &mut Config) -> Result<()> {
    config.manager.global_rules_file = expand_path(&config.manager.global_rules_file)?;
    config.logging.log_dir = expand_path(&config.logging.log_dir)?;
    if !config.docker_dir.as_os_str().is_empty() {
        config.docker_dir = expand_path(&config.docker_dir)?;
    }
    for proj in &mut config.workspaces {
        proj.canonical_path = expand_path(&proj.canonical_path)?;
    }
    for ctr in &mut config.containers {
        for mount in &mut ctr.mounts {
            mount.host = expand_path(&mount.host)?;
        }
        if let Some(path) = &mut ctr.claude_settings {
            *path = expand_path(path)?;
        }
    }
    for mount in &mut config.defaults.containers.mounts {
        mount.host = expand_path(&mount.host)?;
    }
    if let Some(path) = &mut config.defaults.containers.claude_settings {
        *path = expand_path(path)?;
    }
    for profile in config.container_profiles.values_mut() {
        for mount in &mut profile.mounts {
            mount.host = expand_path(&mount.host)?;
        }
        if let Some(path) = &mut profile.claude_settings {
            *path = expand_path(path)?;
        }
    }
    Ok(())
}

fn resolve_container_profiles(config: &mut Config) -> Result<()> {
    anyhow::ensure!(
        config.containers.is_empty(),
        "legacy [[containers]] is no longer supported; define launchable entries under [container_profiles.<name>] only"
    );

    let defaults = config.defaults.containers.clone();
    let session_state_mounts = shared_session_state_mounts()?;
    let mut profile_names = config
        .container_profiles
        .keys()
        .cloned()
        .collect::<Vec<String>>();
    profile_names.sort();

    let mut resolved = Vec::with_capacity(profile_names.len());
    for profile_name in profile_names {
        let profile = config
            .container_profiles
            .get(&profile_name)
            .ok_or_else(|| anyhow::anyhow!("unknown container profile '{}'", profile_name))?;

        let image_stem_raw = profile.image.as_deref().unwrap_or("default").trim();
        anyhow::ensure!(
            !image_stem_raw.is_empty(),
            "container profile '{}': image must not be empty",
            profile_name
        );
        let image_stem = image_stem_raw.to_string();
        anyhow::ensure!(
            image_stem.chars().all(|c| c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, '-' | '_' | '.')),
            "container profile '{}': image must be a lowercase stem (allowed: a-z, 0-9, '-', '_', '.')",
            profile_name
        );
        let image_tag = image_tag_for_stem(&image_stem);
        resolved.push(materialize_container_def(
            &profile_name,
            image_tag,
            image_stem,
            Some(profile),
            &defaults,
            &session_state_mounts,
            None,
        ));
    }
    config.containers = resolved;

    Ok(())
}

pub(crate) fn materialize_container_def(
    profile_name: &str,
    image: String,
    image_stem: String,
    profile: Option<&ContainerProfile>,
    defaults: &ContainerDefaults,
    session_state_mounts: &[ContainerMount],
    dockerfile_path: Option<PathBuf>,
) -> crate::config::ContainerDef {
    let profile = match profile {
        Some(profile) => profile,
        None => &ContainerProfile::default(),
    };

    let explicit_mounts = merge_mounts(&defaults.mounts, session_state_mounts, &profile.mounts);

    let allowed_hosts = merge_unique_strings(&defaults.allowed_hosts, &profile.allowed_hosts, &[]);

    // Prefer the profile's value for an `Option` field, else the default's.
    macro_rules! prefer {
        ($field:ident) => {
            profile.$field.clone().or_else(|| defaults.$field.clone())
        };
    }

    crate::config::ContainerDef {
        name: profile_name.to_string(),
        profile: None,
        image,
        image_stem,
        dockerfile_path,
        mount_target: prefer!(mount_target).unwrap_or_else(default_mount_target),
        command: profile.command.clone(),
        grayscale_palette: prefer!(grayscale_palette).unwrap_or(false),
        starter_network_allowlist: profile.starter_network_allowlist.clone(),
        allowed_hosts,
        mcp_log_paths: merge_unique_paths(&defaults.mcp_log_paths, &profile.mcp_log_paths),
        mcp_log_pattern: prefer!(mcp_log_pattern),
        mounts: explicit_mounts,
        env: merge_env_vars(&defaults.env, &profile.env),
        env_passthrough: merge_unique_strings(
            &defaults.env_passthrough,
            &profile.env_passthrough,
            &[],
        ),
        localhost_forwards: merge_localhost_forwards(
            &defaults.localhost_forwards,
            &profile.localhost_forwards,
        ),
        memory: prefer!(memory),
        cpus: prefer!(cpus),
        shm_size: prefer!(shm_size),
        attach_shell: prefer!(attach_shell),
        claude_settings: prefer!(claude_settings),
    }
}

/// Merge preconfigured container profiles with workspace-local `.dockerfile`
/// templates found under the workspace root.
///
/// Local templates are always appended after configured templates so picker and
/// launch flows can treat workspace-local entries as optional extras.
pub(crate) fn resolve_workspace_container_templates(
    workspace_path: &Path,
    defaults: &crate::config::ContainerDefaults,
    configured: &[crate::config::ContainerDef],
) -> Result<Vec<crate::config::ContainerDef>> {
    let mut out = configured.to_vec();
    let mut seen_names = out
        .iter()
        .map(|container| container.name.clone())
        .collect::<HashSet<_>>();

    let local_dockerfiles = discover_workspace_local_dockerfiles(workspace_path)?;
    if local_dockerfiles.is_empty() {
        return Ok(out);
    }

    let session_state_mounts = shared_session_state_mounts()?;
    for dockerfile in local_dockerfiles {
        let raw_name = local_template_name(workspace_path, &dockerfile);
        let name = dedupe_template_name(raw_name, &mut seen_names);
        let image_stem = local_image_stem(workspace_path, &dockerfile);
        let image = image_tag_for_stem(&image_stem);
        out.push(materialize_container_def(
            &name,
            image,
            image_stem,
            None,
            defaults,
            &session_state_mounts,
            Some(dockerfile),
        ));
    }

    Ok(out)
}

fn dedupe_template_name(name: String, used: &mut HashSet<String>) -> String {
    if used.insert(name.clone()) {
        return name;
    }

    let mut suffix = 2;
    loop {
        let candidate = if suffix == 2 {
            format!("{name} (local)")
        } else {
            format!("{name} (local {suffix})")
        };
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn discover_workspace_local_dockerfiles(workspace_path: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !workspace_path.exists() {
        return Ok(out);
    }

    let mut stack = vec![workspace_path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if is_skippable_entry(&file_name) {
                continue;
            }

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("dockerfile"))
                && local_dockerfile_uses_local_base(&path)?
            {
                out.push(path);
            }
            // Symlinks are skipped to avoid recursion through unexpected cycles.
        }
    }

    out.sort();
    Ok(out)
}

fn is_skippable_entry(file_name: &str) -> bool {
    matches!(
        file_name,
        ".git" | ".idea" | ".gradle" | "target" | "node_modules" | "dist" | "build"
    )
}

fn local_dockerfile_uses_local_base(path: &Path) -> io::Result<bool> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let cmd = parts.next().unwrap_or_default();
        if !cmd.eq_ignore_ascii_case("FROM") {
            return Ok(false);
        }
        let base = parts.next().unwrap_or_default();
        return Ok(base.eq_ignore_ascii_case("harness-hat-base:local"));
    }

    Ok(false)
}

fn local_template_name(workspace_path: &Path, dockerfile_path: &Path) -> String {
    let relative = dockerfile_path
        .strip_prefix(workspace_path)
        .unwrap_or(dockerfile_path)
        .with_extension("");

    let raw = relative.to_string_lossy().to_string();
    if raw.trim().is_empty() {
        "dockerfile".to_string()
    } else {
        raw.replace(['\n', '\\'], "/")
            .trim_end_matches('/')
            .trim_start_matches('/')
            .to_string()
    }
}

fn local_image_stem(workspace_path: &Path, dockerfile_path: &Path) -> String {
    let workspace_name = workspace_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let rel_name = local_template_name(workspace_path, dockerfile_path).replace(['/', '\\'], "_");
    let mut raw = format!("local-{}-{}", workspace_name, rel_name);
    raw.make_ascii_lowercase();

    let mut stem = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            stem.push(ch);
        } else {
            stem.push('-');
        }
    }
    if stem.is_empty() {
        "local".to_string()
    } else {
        stem
    }
}

#[instrument(skip(config, config_path))]
fn validate_docker_dir(config: &Config, config_path: &Path) -> Result<()> {
    anyhow::ensure!(
        !config.docker_dir.as_os_str().is_empty(),
        "config {}: docker_dir is required",
        config_path.display()
    );
    anyhow::ensure!(
        !config.docker_dir.exists() || config.docker_dir.is_dir(),
        "config {}: docker_dir exists but is not a directory: {}",
        config_path.display(),
        config.docker_dir.display()
    );
    Ok(())
}

fn canonicalize_workspace_paths(config: &mut Config) -> Result<()> {
    for proj in &mut config.workspaces {
        anyhow::ensure!(
            !proj.canonical_path.as_os_str().is_empty(),
            "workspace '{}': canonical_path is required",
            proj.name
        );
        proj.canonical_path = proj.canonical_path.canonicalize().with_context(|| {
            format!(
                "workspace '{}': canonical_path is not accessible: {}",
                proj.name,
                proj.canonical_path.display()
            )
        })?;
        reject_sensitive_workspace_path(&proj.name, &proj.canonical_path)?;
    }
    Ok(())
}

/// Refuse workspaces whose canonical path equals or lives under user-secret or
/// system-config directories. Mounting these into a container as `/workspace`
/// rw would let any agent CLI exfiltrate or rewrite the host's credentials.
fn reject_sensitive_workspace_path(name: &str, canonical: &Path) -> Result<()> {
    if let Some(hit) = sensitive_path_hit(canonical) {
        anyhow::bail!(
            "workspace '{}': canonical_path {} resolves under a sensitive path ({}); refusing to mount",
            name,
            canonical.display(),
            hit.display()
        );
    }
    Ok(())
}

/// Refuse a configured bind-mount source that resolves under a sensitive
/// directory or is the Docker socket. The workspace check above only covers
/// `canonical_path`; without this, a `[[*.mounts]]` entry could mount `~/.ssh`,
/// `/etc`, or `/var/run/docker.sock` (the last being a full host-root escape)
/// straight into the container (H4).
fn reject_sensitive_mount_source(context: &str, host: &Path) -> Result<()> {
    let resolved = host.canonicalize();
    let candidate: &Path = resolved.as_deref().unwrap_or(host);

    if let Some(hit) = sensitive_path_hit(candidate) {
        anyhow::bail!(
            "{context}: mount.host {} resolves under a sensitive path ({}); refusing to mount",
            host.display(),
            hit.display()
        );
    }
    Ok(())
}

/// Return the sensitive root a path equals or lives under, if any. Covers
/// `/etc`, `~/.ssh`, `~/.gnupg`, and the Docker socket at its common locations.
fn sensitive_path_hit(candidate: &Path) -> Option<PathBuf> {
    // Resolve symlinks so `/etc` → `/private/etc` on macOS, etc.
    let candidate_resolved = candidate.canonicalize();
    let candidate = candidate_resolved.as_deref().unwrap_or(candidate);

    let mut exact_only: Vec<PathBuf> = vec![
        PathBuf::from("/"),
        PathBuf::from("/home"),
        PathBuf::from("/Users"),
    ];
    let mut sensitive: Vec<PathBuf> = vec![
        PathBuf::from("/etc"),
        PathBuf::from("/boot"),
        PathBuf::from("/dev"),
        PathBuf::from("/proc"),
        PathBuf::from("/root"),
        PathBuf::from("/run"),
        PathBuf::from("/sys"),
        PathBuf::from("/usr"),
        PathBuf::from("/var"),
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/docker.sock"),
    ];
    if let Some(home) = dirs::home_dir() {
        // Reject the home directory itself. Common broad parents are listed
        // explicitly above; deriving every ancestor from HOME is unsafe in
        // hermetic environments where HOME may live under a shared temp root.
        exact_only.push(home.clone());
        sensitive.push(home.join(".ssh"));
        sensitive.push(home.join(".gnupg"));
        sensitive.push(home.join(".aws"));

        sensitive.push(home.join(".config/gcloud"));
        sensitive.push(home.join(".config/gh"));
        sensitive.push(home.join(".docker"));
        sensitive.push(home.join(".kube"));
        sensitive.push(home.join(".netrc"));
    }
    for root in &exact_only {
        let canonical_root = root.canonicalize();
        let root = canonical_root.as_deref().unwrap_or(root.as_path());
        if candidate == root {
            return Some(root.to_path_buf());
        }
    }
    for s in &sensitive {
        let s_can = s.canonicalize();
        let s_ref: &Path = s_can.as_deref().unwrap_or(s.as_path());
        if candidate == s_ref || candidate.starts_with(s_ref) {
            return Some(s_ref.to_path_buf());
        }
    }
    // Catch the Docker socket regardless of directory (e.g. a rootless socket
    // under XDG_RUNTIME_DIR) by its filename.
    if candidate.file_name().is_some_and(|n| n == "docker.sock") {
        return Some(candidate.to_path_buf());
    }
    None
}

/// Concatenate `parts` in order, keeping the first item for which `eq` finds no
/// earlier duplicate (i.e. base takes precedence over profile over override).
/// Shared by the list-merge helpers below.
fn merge_dedup<T: Clone>(parts: &[&[T]], eq: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in parts.iter().flat_map(|part| part.iter()) {
        if !out.iter().any(|existing| eq(existing, item)) {
            out.push(item.clone());
        }
    }
    out
}

pub(crate) fn merge_unique_strings(
    base: &[String],
    profile: &[String],
    override_items: &[String],
) -> Vec<String> {
    merge_dedup(&[base, profile, override_items], |a, b| a == b)
}

pub(crate) fn merge_unique_paths(base: &[PathBuf], profile: &[PathBuf]) -> Vec<PathBuf> {
    merge_dedup(&[base, profile], |a, b| a == b)
}

fn merge_env_vars(
    base: &std::collections::HashMap<String, String>,
    profile: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut out = base.clone();
    out.extend(profile.clone());
    out
}

pub(crate) fn merge_localhost_forwards(
    base: &[LocalhostForward],
    profile: &[LocalhostForward],
) -> Vec<LocalhostForward> {
    let profile_ports = profile
        .iter()
        .map(|forward| forward.container_port)
        .collect::<std::collections::HashSet<_>>();
    let mut out = base
        .iter()
        .filter(|forward| !profile_ports.contains(&forward.container_port))
        .cloned()
        .collect::<Vec<_>>();
    for forward in profile {
        if !out
            .iter()
            .any(|existing| existing.container_port == forward.container_port)
        {
            out.push(forward.clone());
        }
    }
    out
}

/// Combine mount layers (defaults → session-state → per-profile overrides) into
/// the final mount set, keyed by **container destination**.
///
/// Docker rejects two `-v` mounts that share a container path ("Duplicate mount
/// point"), which aborts `docker run` before any container is created. So a
/// destination may appear at most once; when multiple layers target the same
/// container path the later layer wins (session-state mounts override config
/// defaults; per-profile overrides win over both). First-seen position is kept
/// for stable, readable output.
///
/// Deduping on the full `(host, container, mode)` tuple - as this once did - is
/// not enough: the same destination can arrive with different host paths (e.g.
/// the keyring mount, where a config entry uses `~/.local/share/...` but
/// `dirs::data_dir()` resolves to `~/Library/Application Support/...` on macOS),
/// and both would survive into the `docker run` invocation and break it.
pub(crate) fn merge_mounts(
    base: &[ContainerMount],
    profile: &[ContainerMount],
    override_items: &[ContainerMount],
) -> Vec<ContainerMount> {
    let mut out: Vec<ContainerMount> = Vec::new();
    for item in [base, profile, override_items].into_iter().flatten() {
        if let Some(existing) = out.iter_mut().find(|m| m.container == item.container) {
            *existing = item.clone();
        } else {
            out.push(item.clone());
        }
    }
    out
}

fn shared_session_state_mounts() -> Result<Vec<ContainerMount>> {
    let mut mounts = Vec::new();
    for (host, container) in [
        ("~/.claude.json", "/home/coder/.claude.json"),
        ("~/.claude/.claude.json", "/home/coder/.claude/.claude.json"),
        ("~/.claude", "/home/coder/.claude"),
        ("~/.codex", "/home/coder/.codex"),
        ("~/.config/codex", "/home/coder/.config/codex"),
        // OpenCode keeps global settings, agents, commands, and plugins under
        // ~/.config/opencode.
        ("~/.config/opencode", "/home/coder/.config/opencode"),
        // Antigravity CLI keeps session/config data in ~/.gemini/antigravity-cli.
        // Mount the root so migrated Gemini CLI state remains available too.
        ("~/.gemini", "/home/coder/.gemini"),
        ("~/.pi", "/home/coder/.pi"),
    ] {
        // Skip mounts whose host source does not exist rather than asking
        // Docker to bind a missing path (which silently creates a root-owned
        // empty dir on the host, or fails the run outright).
        if let Some(mount) = shared_session_mount(host, container, MountMode::Rw)? {
            mounts.push(mount);
        }
    }
    // Read-only: the container only ever reads this once (see .zshrc) to seed
    // its own workspace-local history file. It never writes back, so there's
    // no risk of the container corrupting the host's live shell history.
    if let Some(mount) = shared_session_mount(
        "~/.zsh_history",
        "/home/coder/.zsh_history.host",
        MountMode::Ro,
    )? {
        mounts.push(mount);
    }
    mounts.push(shared_container_keyring_mount()?);
    Ok(mounts)
}

fn shared_session_mount(
    host: &str,
    container: &str,
    mode: MountMode,
) -> Result<Option<ContainerMount>> {
    let host = expand_path(Path::new(host))?;
    if !host.exists() {
        return Ok(None);
    }
    Ok(Some(ContainerMount {
        host: host.clone(),
        container: PathBuf::from(container),
        mode,
        // Unset: the `.claude.json` mount picks up the seed-by-default heuristic
        // in container::spawn; the directory mounts (.claude, .codex, …) don't.
        seed: None,
        add_to_path: false,
    }))
}

fn shared_container_keyring_mount() -> Result<ContainerMount> {
    let host = dirs::data_dir()
        .context("cannot determine user data directory")?
        .join("harness-hat/container-keyrings");
    std::fs::create_dir_all(&host)
        .with_context(|| format!("creating container keyring state dir {}", host.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("restricting container keyring state dir {}", host.display()),
        )?;
    }
    Ok(ContainerMount {
        host,
        container: PathBuf::from("/home/coder/.local/share/keyrings"),
        mode: Default::default(),
        seed: None,
        add_to_path: false,
    })
}

pub fn image_tag_for_stem(stem: &str) -> String {
    let mut slug = String::new();
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            slug.push(ch.to_ascii_lowercase());
        } else {
            slug.push('-');
        }
    }
    if slug.is_empty() {
        slug.push_str("default");
    }
    format!("harness-hat-{slug}:local")
}

fn validate(config: &Config) -> Result<()> {
    for (profile_name, profile) in &config.env_profiles {
        for (key, value) in &profile.vars {
            anyhow::ensure!(
                crate::fs_util::is_valid_env_name(key),
                "env profile '{}': invalid environment variable name: {}",
                profile_name,
                key
            );
            anyhow::ensure!(
                !value.contains('\n') && !value.contains('\r'),
                "env profile '{}': value for {} must not contain newlines",
                profile_name,
                key
            );
        }
    }

    let mut seen = std::collections::HashSet::new();
    for proj in &config.workspaces {
        anyhow::ensure!(
            seen.insert(&proj.name),
            "duplicate workspace name: {}",
            proj.name
        );
        anyhow::ensure!(
            !proj.canonical_path.as_os_str().is_empty(),
            "workspace '{}': canonical_path is required",
            proj.name
        );
        anyhow::ensure!(
            proj.canonical_path.exists(),
            "workspace '{}': canonical_path does not exist: {}",
            proj.name,
            proj.canonical_path.display()
        );
        anyhow::ensure!(
            proj.canonical_path.is_dir(),
            "workspace '{}': canonical_path is not a directory: {}",
            proj.name,
            proj.canonical_path.display()
        );
    }
    let mut seen_containers = std::collections::HashSet::new();
    for ctr in &config.containers {
        anyhow::ensure!(
            seen_containers.insert(&ctr.name),
            "duplicate container name: {}",
            ctr.name
        );
        validate_command_argv(
            &format!("container profile '{}': command", ctr.name),
            ctr.command.as_deref(),
        )?;
        anyhow::ensure!(
            is_absolute_container_path(&ctr.mount_target),
            "container '{}': mount_target must be an absolute container path: {}",
            ctr.name,
            container_path_string(&ctr.mount_target)
        );
        for path in &ctr.mcp_log_paths {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "container '{}': mcp_log_paths contains an empty path",
                ctr.name
            );
            anyhow::ensure!(
                is_absolute_container_path(path),
                "container '{}': mcp_log_paths must be absolute container paths: {}",
                ctr.name,
                container_path_string(path)
            );
        }
        if let Some(pattern) = &ctr.mcp_log_pattern {
            anyhow::ensure!(
                !pattern.trim().is_empty(),
                "container '{}': mcp_log_pattern must not be empty",
                ctr.name
            );
            anyhow::ensure!(
                !pattern.contains('\n') && !pattern.contains('\r'),
                "container '{}': mcp_log_pattern must not contain newlines",
                ctr.name
            );
        }
        for mount in &ctr.mounts {
            anyhow::ensure!(
                !mount.host.as_os_str().is_empty(),
                "container '{}': mount.host must not be empty",
                ctr.name
            );
            anyhow::ensure!(
                !mount.container.as_os_str().is_empty(),
                "container '{}': mount.container must not be empty",
                ctr.name
            );
            anyhow::ensure!(
                is_absolute_container_path(&mount.container),
                "container '{}': mount.container must be an absolute path: {}",
                ctr.name,
                container_path_string(&mount.container)
            );
            reject_sensitive_mount_source(&format!("container '{}'", ctr.name), &mount.host)?;
        }
        if let Some(path) = &ctr.claude_settings {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "container '{}': claude_settings must not be empty",
                ctr.name
            );
            reject_sensitive_mount_source(
                &format!("container '{}': claude_settings", ctr.name),
                path,
            )?;
        }
        for (key, value) in &ctr.env {
            anyhow::ensure!(
                crate::fs_util::is_valid_env_name(key),
                "container '{}': invalid environment variable name: {}",
                ctr.name,
                key
            );
            anyhow::ensure!(
                !value.contains('\n') && !value.contains('\r'),
                "container '{}': env value for {} must not contain newlines",
                ctr.name,
                key
            );
        }
        for name in &ctr.env_passthrough {
            anyhow::ensure!(
                !name.trim().is_empty(),
                "container '{}': env_passthrough contains an empty name",
                ctr.name
            );
            anyhow::ensure!(
                !name.contains('='),
                "container '{}': env_passthrough must be env var names only (no '='): {}",
                ctr.name,
                name
            );
        }
        for forward in &ctr.localhost_forwards {
            anyhow::ensure!(
                forward.container_port > 0,
                "container '{}': localhost_forwards.container_port must be greater than zero",
                ctr.name
            );
            anyhow::ensure!(
                forward.effective_host_port() > 0,
                "container '{}': localhost_forwards.host_port must be greater than zero",
                ctr.name
            );
        }
        validate_optional_docker_value(&format!("container '{}': memory", ctr.name), &ctr.memory)?;
        validate_optional_docker_value(&format!("container '{}': cpus", ctr.name), &ctr.cpus)?;
        validate_optional_docker_value(
            &format!("container '{}': shm_size", ctr.name),
            &ctr.shm_size,
        )?;
    }
    Ok(())
}

fn validate_command_argv(field: &str, command: Option<&[String]>) -> Result<()> {
    let Some(command) = command else {
        return Ok(());
    };
    anyhow::ensure!(!command.is_empty(), "{field} must not be empty");
    for (idx, arg) in command.iter().enumerate() {
        anyhow::ensure!(!arg.trim().is_empty(), "{field}[{idx}] must not be empty");
    }
    Ok(())
}

fn validate_optional_docker_value(field: &str, value: &Option<String>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    anyhow::ensure!(!value.trim().is_empty(), "{field} must not be empty");
    anyhow::ensure!(
        !value.contains('\n') && !value.contains('\r'),
        "{field} must not contain newlines"
    );
    Ok(())
}

fn ensure_logging_instance_id(path: &Path, raw: &str, config: &mut Config) -> Result<()> {
    let current = config
        .logging
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(instance_id) = current {
        config.logging.instance_id = Some(instance_id);
        return Ok(());
    }

    let instance_id = uuid::Uuid::new_v4().to_string();
    let mut doc: DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing config document: {}", path.display()))?;
    doc["logging"]["instance_id"] = value(instance_id.clone());
    let rendered = doc.to_string();
    atomic_write_with_lock(path, rendered.as_bytes())
        .with_context(|| format!("writing config: {}", path.display()))?;
    config.logging.instance_id = Some(instance_id);
    Ok(())
}

/// Atomically write `contents` to `path` under an advisory exclusive file lock
/// taken on a sibling lockfile. The write goes to a tmp file in the same
/// directory (so `rename` is atomic on a single filesystem), is `fsync`'d, then
/// renamed over the destination.
pub(crate) fn atomic_write_with_lock(path: &Path, contents: &[u8]) -> Result<()> {
    use fs2::FileExt;
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("creating parent dir: {}", parent.display()))?;

    // Advisory lock co-located with the config so two harness processes serialize.
    let lock_path = path.with_extension({
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext.is_empty() {
            "lock".to_string()
        } else {
            format!("{ext}.lock")
        }
    });
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file: {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock_file)
        .with_context(|| format!("acquiring lock: {}", lock_path.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("creating tmp file in {}", parent.display()))?;
    tmp.write_all(contents)
        .with_context(|| format!("writing tmp file in {}", parent.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("fsyncing tmp file in {}", parent.display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("renaming tmp file over {}: {}", path.display(), e.error))?;

    // Best-effort: unlock on drop. Explicit unlock here is unnecessary because
    // `lock_file` dropping releases the lock.
    drop(lock_file);
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Expand `~` at the start of a path. `~user/foo` patterns are explicitly
/// rejected — silently treating them as a literal path would surprise users
/// who expect shell-style expansion.
pub fn expand_path(path: &Path) -> Result<PathBuf> {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(rest))
    } else if s == "~" {
        dirs::home_dir().context("cannot determine home directory")
    } else if s.starts_with('~') {
        anyhow::bail!(
            "path {:?} uses ~user-style expansion which is not supported; \
             use an explicit /home/<user>/... path or set $HOME",
            path
        )
    } else {
        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::config::ContainerDef;

    fn with_test_home<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = crate::TEST_ENV_LOCK.lock().expect("test env lock");
        let original_home = std::env::var_os("HOME");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match original_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        match original_xdg_data_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_DATA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_DATA_HOME");
            },
        }
        match result {
            Ok(result) => result,
            Err(err) => std::panic::resume_unwind(err),
        }
    }

    #[test]
    fn rejects_broad_and_credential_bearing_mount_roots() {
        // Production passes canonical workspace paths into this helper. Use the
        // current filesystem's canonical root so the assertion covers `/` on
        // Unix and the active drive root (for example `C:\`) on Windows.
        let cwd = std::env::current_dir().expect("current directory");
        let root = cwd
            .ancestors()
            .last()
            .expect("filesystem root")
            .canonicalize()
            .expect("canonical filesystem root");
        assert!(sensitive_path_hit(&root).is_some());

        #[cfg(unix)]
        {
            assert!(sensitive_path_hit(Path::new("/etc")).is_some());
            assert!(sensitive_path_hit(Path::new("/proc/self")).is_some());
        }
        if let Some(home) = dirs::home_dir() {
            let canonical_home = home.canonicalize().expect("canonical home directory");
            assert!(sensitive_path_hit(&canonical_home).is_some());
            assert!(sensitive_path_hit(&home.join(".aws/credentials")).is_some());
        }
    }

    #[test]
    fn rejects_sensitive_claude_settings_source() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let error = with_test_home(&home, || {
            let mut config = Config::default();
            let mut container: ContainerDef =
                serde_json::from_value(serde_json::json!({"name": "test"}))
                    .expect("minimal container definition");
            container.claude_settings = Some(home.join(".ssh/settings.json"));
            config.containers.push(container);

            validate(&config).expect_err("sensitive Claude settings must be rejected")
        });
        assert!(error.to_string().contains("claude_settings"));
    }

    #[test]
    fn rejects_invalid_tilde_expansion_in_claude_settings_source() {
        let mut config = Config::default();
        config.defaults.containers.claude_settings =
            Some(std::path::PathBuf::from("~other/settings.json"));

        assert!(expand_config_paths(&mut config).is_err());
    }
}

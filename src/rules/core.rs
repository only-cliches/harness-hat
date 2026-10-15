/// Parses `harness-rules.toml` files and composes global + per-workspace
/// network policy.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

use crate::config::LocalhostForward;

pub const CURRENT_RULES_VERSION: u32 = 1;

// ── Enums (re-used across config and rules) ──────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Auto,
    Prompt,
    Deny,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self::Prompt
    }
}

// ── harness-rules.toml schema ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProjectRules {
    /// Schema version for future harness-rules.toml migrations.
    #[serde(default = "current_rules_version")]
    pub version: u32,
    /// Optional project instructions. This field is preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_instructions: Option<String>,
    /// Preferred container template for this workspace. A primary-config
    /// workspace template takes precedence when explicitly configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Optional override for mounting the workspace at its mirrored host path
    /// instead of the configured container mount target. Windows drive paths
    /// use a best-effort `/C/Users/...` representation. When omitted,
    /// mirroring defaults to enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_cwd: Option<bool>,
    /// Additional host-local TCP services exposed as localhost inside the
    /// workspace container. Entries with the same container port override
    /// the configured container/profile forward.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub localhost_forwards: Vec<LocalhostForward>,
    #[serde(default)]
    pub hostdo: HostdoRules,
    #[serde(default)]
    pub network: NetworkRules,
}

fn current_rules_version() -> u32 {
    CURRENT_RULES_VERSION
}

impl Default for ProjectRules {
    fn default() -> Self {
        Self {
            version: CURRENT_RULES_VERSION,
            llm_instructions: None,
            template: None,
            mirror_cwd: Some(true),
            localhost_forwards: Vec::new(),
            hostdo: HostdoRules::default(),
            network: NetworkRules::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct HostdoRules {
    #[serde(default)]
    pub default_policy: NetworkPolicy,
    #[serde(default)]
    pub commands: Vec<HostdoCommand>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HostdoCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default = "default_hostdo_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub approval_mode: NetworkPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When set, the host command runs from a cleared environment containing
    /// only a small base set (PATH, HOME, ...) plus the variables named here,
    /// instead of inheriting the manager's full environment (M3). Only valid
    /// for direct (non-`image`) rules: Docker runners already start from a
    /// clean container environment, and forwarding host variables into them
    /// would widen exposure rather than narrow it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_allowlist: Option<Vec<String>>,
}

fn default_hostdo_timeout_secs() -> u64 {
    60
}

/// Hard ceiling for every hostdo command, regardless of its request or rule.
pub const MAX_HOSTDO_TIMEOUT_SECS: u64 = 5 * 60;

pub(crate) fn clamp_hostdo_timeout_secs(timeout_secs: u64) -> u64 {
    timeout_secs.clamp(1, MAX_HOSTDO_TIMEOUT_SECS)
}

impl Default for HostdoCommand {
    fn default() -> Self {
        Self {
            name: None,
            argv: Vec::new(),
            image: None,
            cwd: None,
            timeout_secs: default_hostdo_timeout_secs(),
            approval_mode: NetworkPolicy::Prompt,
            reason: None,
            env_allowlist: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NetworkRules {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denylist: Vec<String>,
}

impl Default for NetworkRules {
    fn default() -> Self {
        Self {
            allowlist: vec![],
            denylist: vec![],
        }
    }
}

/// One parsed network rule:
/// `method=GET,POST domain=api.example.com path=/api/*,/auth/* port=443,8443`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NetworkRule {
    pub methods: Vec<String>,
    pub domains: Vec<String>,
    pub paths: Vec<String>,
    pub ports: Vec<u16>,
}

// ── ComposedRules ────────────────────────────────────────────────────────────

/// Effective rule set for a given request context.
/// Global rules and all workspace-local rules are unioned together.
#[derive(Debug, Clone, Default)]
pub struct ComposedRules {
    /// Effective workspace-path mirroring setting after applying global and
    /// workspace rules. The default is enabled.
    pub mirror_cwd: bool,
    pub hostdo: HostdoRules,
    pub network_rules: Vec<NetworkRule>,
    pub network_deny_rules: Vec<NetworkRule>,
    pub network_default: NetworkPolicy,
    /// Effective global + workspace-local localhost forwards. Later layers
    /// replace earlier entries with the same container port.
    pub localhost_forwards: Vec<LocalhostForward>,
}

impl ComposedRules {
    /// Compose global rules + one or more project rule sets.
    /// Global rules come first (higher priority in declaration order).
    pub fn compose(global: &ProjectRules, projects: &[ProjectRules]) -> Self {
        let mut hostdo = global.hostdo.commands.clone();
        let mut network_allowlist = global.network.allowlist.clone();
        let mut network_denylist = global.network.denylist.clone();
        let mut hostdo_default = global.hostdo.default_policy;
        let mut mirror_cwd = global.mirror_cwd.unwrap_or(true);
        let mut localhost_forwards = global.localhost_forwards.clone();

        for proj in projects {
            if let Some(value) = proj.mirror_cwd {
                mirror_cwd = value;
            }
            localhost_forwards =
                merge_localhost_forwards(&localhost_forwards, &proj.localhost_forwards);
            hostdo_default = compose_hostdo_policies(hostdo_default, proj.hostdo.default_policy);
            for cmd in &proj.hostdo.commands {
                if !hostdo
                    .iter()
                    .any(|c| c.argv == cmd.argv && c.image == cmd.image)
                {
                    // Workspace-local hostdo rules are intentional executable
                    // grants. The approval UI persists remembered decisions
                    // here so they can be reviewed and shared with the repo.
                    hostdo.push(cmd.clone());
                }
            }
            network_allowlist.extend(proj.network.allowlist.clone());
            network_denylist.extend(proj.network.denylist.clone());
        }

        Self {
            mirror_cwd,
            hostdo: HostdoRules {
                default_policy: hostdo_default,
                commands: parse_dedup_hostdo_commands(hostdo),
            },
            network_rules: parse_dedup_network_rules(network_allowlist),
            network_deny_rules: parse_dedup_network_rules(network_denylist),
            // Coder-style rules engine is explicit allowlist with prompt-by-default.
            network_default: NetworkPolicy::Prompt,
            localhost_forwards,
        }
    }

    /// Find the effective network policy for a given request.
    pub fn match_network(&self, method: &str, host: &str, path: &str) -> NetworkPolicy {
        self.match_network_for_port(method, host, path, None)
    }

    /// Find the effective network policy for a request with a known destination port.
    pub fn match_network_for_port(
        &self,
        method: &str,
        host: &str,
        path: &str,
        port: Option<u16>,
    ) -> NetworkPolicy {
        if self
            .network_deny_rules
            .iter()
            .any(|r| network_rule_matches(r, method, host, path, port))
        {
            return NetworkPolicy::Deny;
        }
        if self
            .network_rules
            .iter()
            .any(|r| network_rule_matches(r, method, host, path, port))
        {
            NetworkPolicy::Auto
        } else {
            self.network_default
        }
    }

    /// Find the effective policy for CONNECT preflight.
    ///
    /// Domain-only allow rules approve HTTPS CONNECT on 443. Raw TCP CONNECT to
    /// other ports must be explicitly allowed with `port=...`. Domain-only deny
    /// rules remain broad and deny every port for that host.
    pub fn match_connect(&self, host: &str, port: u16) -> NetworkPolicy {
        if self
            .network_deny_rules
            .iter()
            .any(|r| connect_rule_matches(r, host, port, true))
        {
            return NetworkPolicy::Deny;
        }
        if self
            .network_rules
            .iter()
            .any(|r| connect_rule_matches(r, host, port, false))
        {
            NetworkPolicy::Auto
        } else {
            self.network_default
        }
    }

    /// Find effective host policy for an exact command match.
    pub fn match_hostdo(&self, command: &[String], image: Option<&str>) -> Option<NetworkPolicy> {
        self.find_hostdo(command, image)
            .map(|entry| entry.approval_mode)
    }

    pub fn find_hostdo(&self, command: &[String], image: Option<&str>) -> Option<&HostdoCommand> {
        self.hostdo
            .commands
            .iter()
            .find(|entry| entry.argv == command && entry.image.as_deref() == image)
    }
}

fn network_rule_matches(
    rule: &NetworkRule,
    method: &str,
    host: &str,
    path: &str,
    port: Option<u16>,
) -> bool {
    method_matches(&rule.methods, method)
        && domain_matches(&rule.domains, host)
        && path_matches(&rule.paths, path)
        && port_matches(&rule.ports, port)
}

fn connect_rule_matches(rule: &NetworkRule, host: &str, port: u16, deny_rule: bool) -> bool {
    method_matches(&rule.methods, "CONNECT")
        && domain_matches(&rule.domains, host)
        && path_matches(&rule.paths, "/")
        && connect_port_matches(&rule.ports, port, deny_rule)
}

fn method_matches(patterns: &[String], method: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|m| m.eq_ignore_ascii_case(method))
}

fn domain_matches(patterns: &[String], host: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    // Host strings are canonicalized by the caller (lowercase, trailing-dot
    // stripped, IDNA-normalized) — see `proxy::helpers::canonicalize_host`.
    // Defensive: lowercase + trim trailing `.` here in case some path forgot.
    let host_canon = normalize_host_for_match(host);
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern == "*" {
            return true;
        }
        // Coder semantics: "*.example.com" matches subdomains, not apex.
        if let Some(apex) = pattern.strip_prefix("*.") {
            let apex_lc = normalize_host_for_match(apex);
            return host_canon.ends_with(&format!(".{apex_lc}"));
        }
        normalize_host_for_match(pattern) == host_canon
    })
}

fn normalize_host_for_match(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    domain_matches(&[pattern.to_string()], host)
}

/// Normalize a request path before policy matching:
/// - strip the query string (`?...`) and fragment (`#...`)
/// - percent-decode (best-effort, invalid escapes left as-is)
/// - collapse `//` runs and resolve `..`/`.` segments
///
/// Path matching is byte-exact (case-sensitive) once normalized, mirroring how
/// real HTTP servers route. Authors who need case-insensitive rules can encode
/// both casings explicitly.
fn normalize_path_for_match(path: &str) -> String {
    let without_query = path
        .split_once('?')
        .map(|(p, _)| p)
        .unwrap_or(path)
        .split_once('#')
        .map(|(p, _)| p)
        .unwrap_or_else(|| path.split_once('?').map(|(p, _)| p).unwrap_or(path));

    let decoded = percent_decode_path(without_query);

    // Collapse `//` and resolve `.`/`..` segments.
    let leading_slash = decoded.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            segments.pop();
            continue;
        }
        segments.push(segment);
    }
    let mut out = String::with_capacity(decoded.len());
    if leading_slash {
        out.push('/');
    }
    out.push_str(&segments.join("/"));
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn percent_decode_path(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_nibble(bytes[i + 1]);
            let lo = hex_nibble(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn path_matches(patterns: &[String], path: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let normalized = normalize_path_for_match(path);
    patterns
        .iter()
        .any(|pattern| path_pattern_matches(pattern, &normalized))
}

/// Match a single path pattern against an already-normalized path.
/// A trailing `*` is segment-anchored: `path=/v1/*` matches `/v1/foo` and
/// `/v1/foo/bar` but not `/v1` or `/v1foo`. Embedded `*` retains
/// substring-style semantics for backward compatibility.
fn path_pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        // Segment-anchored trailing wildcard: must match the prefix then `/`.
        if prefix.is_empty() {
            // `/*` matches anything starting with `/`.
            return path.starts_with('/');
        }
        return path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/');
    }
    if pattern == "*" {
        return true;
    }
    wildcard_match(pattern, path)
}

fn port_matches(patterns: &[u16], port: Option<u16>) -> bool {
    patterns.is_empty() || port.is_some_and(|port| patterns.contains(&port))
}

fn connect_port_matches(patterns: &[u16], port: u16, deny_rule: bool) -> bool {
    if patterns.is_empty() {
        return deny_rule || port == 443;
    }
    patterns.contains(&port)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let mut parts = pattern.split('*').peekable();
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let mut idx = 0usize;

    if !starts_with_wildcard {
        let Some(first) = parts.next() else {
            return true;
        };
        if !text[idx..].starts_with(first) {
            return false;
        }
        idx += first.len();
    }

    let remaining: Vec<&str> = parts.collect();
    let last_idx = remaining.len().saturating_sub(1);
    for (i, part) in remaining.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == last_idx && !ends_with_wildcard {
            return text[idx..].ends_with(part);
        }
        if let Some(found) = text[idx..].find(part) {
            idx += found + part.len();
        } else {
            return false;
        }
    }
    true
}

pub fn parse_network_allowlist_rule(raw: &str) -> Result<NetworkRule> {
    let mut methods = Vec::new();
    let mut domains = Vec::new();
    let mut paths = Vec::new();
    let mut ports = Vec::new();

    let trimmed = raw.trim();
    anyhow::ensure!(!trimmed.is_empty(), "network rule entry must not be empty");
    for token in trimmed.split_whitespace() {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid token '{token}' in network entry '{raw}'"))?;
        let values: Vec<String> = value
            .split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .collect();
        anyhow::ensure!(
            !values.is_empty(),
            "network token '{key}' has no values in '{raw}'"
        );
        match key {
            "method" => {
                anyhow::ensure!(methods.is_empty(), "duplicate method key in '{raw}'");
                methods = values.into_iter().map(|v| v.to_ascii_uppercase()).collect();
            }
            "domain" => {
                anyhow::ensure!(domains.is_empty(), "duplicate domain key in '{raw}'");
                for d in &values {
                    anyhow::ensure!(
                        !d.starts_with('.'),
                        "domain pattern '{d}' must not start with '.' in '{raw}' (use '*.{}' for subdomains, or the apex for exact)",
                        d.trim_start_matches('.')
                    );
                    anyhow::ensure!(!d.is_empty(), "empty domain pattern in '{raw}'");
                    // Reject obviously bogus / non-ASCII patterns. Callers must
                    // pre-IDNA encode patterns to ASCII (`xn--…`) so matching
                    // is straightforward against canonicalized hosts.
                    anyhow::ensure!(
                        d == "*" || d.bytes().all(|b| b.is_ascii() && b != b' '),
                        "domain pattern '{d}' must be ASCII (use punycode/xn-- for IDN) in '{raw}'"
                    );
                }
                domains = values.into_iter().map(|v| v.to_ascii_lowercase()).collect();
            }
            "path" => {
                anyhow::ensure!(paths.is_empty(), "duplicate path key in '{raw}'");
                paths = values;
            }
            "port" => {
                anyhow::ensure!(ports.is_empty(), "duplicate port key in '{raw}'");
                ports = values
                    .into_iter()
                    .map(|value| {
                        value.parse::<u16>().with_context(|| {
                            format!("invalid port '{value}' in network entry '{raw}'")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            _ => anyhow::bail!("unknown key '{key}' in network entry '{raw}'"),
        }
    }
    anyhow::ensure!(
        !domains.is_empty(),
        "network entry '{raw}' is missing required 'domain=' key"
    );
    Ok(NetworkRule {
        methods,
        domains,
        paths,
        ports,
    })
}

/// Deduplicate `raws` (preserving first occurrence) and parse each into a
/// [`NetworkRule`], defensively skipping any that fail to parse — `load()`
/// already validates syntax, so invalid entries shouldn't reach here.
fn parse_dedup_network_rules(raws: Vec<String>) -> Vec<NetworkRule> {
    let mut seen = HashSet::new();
    raws.into_iter()
        .filter(|raw| seen.insert(raw.clone()))
        .filter_map(|raw| parse_network_allowlist_rule(&raw).ok())
        .collect()
}

fn parse_dedup_hostdo_commands(raws: Vec<HostdoCommand>) -> Vec<HostdoCommand> {
    let mut seen = HashSet::new();
    raws.into_iter()
        .filter(|raw| seen.insert((raw.argv.clone(), raw.image.clone())) && !raw.argv.is_empty())
        .collect()
}

fn merge_localhost_forwards(
    base: &[LocalhostForward],
    overrides: &[LocalhostForward],
) -> Vec<LocalhostForward> {
    let override_ports = overrides
        .iter()
        .map(|forward| forward.container_port)
        .collect::<HashSet<_>>();
    let mut out = base
        .iter()
        .filter(|forward| !override_ports.contains(&forward.container_port))
        .cloned()
        .collect::<Vec<_>>();
    out.extend(overrides.iter().cloned());
    out
}

fn compose_hostdo_policies(lhs: NetworkPolicy, rhs: NetworkPolicy) -> NetworkPolicy {
    match (lhs, rhs) {
        (NetworkPolicy::Deny, _) | (_, NetworkPolicy::Deny) => NetworkPolicy::Deny,
        (NetworkPolicy::Prompt, _) | (_, NetworkPolicy::Prompt) => NetworkPolicy::Prompt,
        _ => NetworkPolicy::Auto,
    }
}

// ── Loading / saving ─────────────────────────────────────────────────────────

/// Load a `harness-rules.toml` file.  Returns a default (empty) rule set if the
/// file does not exist, rather than an error.
pub fn load(path: &Path) -> Result<ProjectRules> {
    let Some(bytes) = crate::shared_config::read_rules_file_bytes(path)
        .with_context(|| format!("reading harness-rules.toml: {}", path.display()))?
    else {
        return Ok(ProjectRules::default());
    };
    let raw = std::str::from_utf8(&bytes)
        .with_context(|| format!("harness-rules.toml is not UTF-8: {}", path.display()))?;
    let mut parsed: ProjectRules = toml::from_str(raw)
        .with_context(|| format!("parsing harness-rules.toml: {}", path.display()))?;
    validate_rules_version(parsed.version, path)?;
    validate_template(parsed.template.as_deref(), path)?;
    validate_network_list("allowlist", &parsed.network.allowlist, path)?;
    validate_network_list("denylist", &parsed.network.denylist, path)?;
    validate_localhost_forwards(&parsed.localhost_forwards, path)?;
    validate_hostdo_rules(&parsed.hostdo.commands, path)?;
    for entry in &mut parsed.hostdo.commands {
        entry.timeout_secs = clamp_hostdo_timeout_secs(entry.timeout_secs);
    }
    Ok(parsed)
}

fn validate_localhost_forwards(forwards: &[LocalhostForward], path: &Path) -> Result<()> {
    for forward in forwards {
        anyhow::ensure!(
            forward.container_port > 0,
            "invalid [localhost_forwards] entry in {}: container_port must be greater than zero",
            path.display()
        );
        anyhow::ensure!(
            forward.effective_host_port() > 0,
            "invalid [localhost_forwards] entry in {}: host_port must be greater than zero",
            path.display()
        );
    }
    Ok(())
}

fn validate_template(template: Option<&str>, path: &Path) -> Result<()> {
    if let Some(template) = template {
        anyhow::ensure!(
            !template.trim().is_empty(),
            "invalid template in {}: template must not be empty",
            path.display()
        );
    }
    Ok(())
}

/// Validate every entry in a `[network]` list, attributing failures to the
/// named list (`allowlist`/`denylist`) and source file.
fn validate_network_list(list: &str, entries: &[String], path: &Path) -> Result<()> {
    for entry in entries {
        parse_network_allowlist_rule(entry).with_context(|| {
            format!(
                "invalid [network].{list} entry '{}' in {}",
                entry,
                path.display()
            )
        })?;
    }
    Ok(())
}

fn validate_hostdo_rules(entries: &[HostdoCommand], path: &Path) -> Result<()> {
    for entry in entries {
        anyhow::ensure!(
            !entry.argv.is_empty(),
            "invalid [hostdo].commands entry '{}' in {}: command argv must not be empty",
            entry.argv.join(" "),
            path.display()
        );
        anyhow::ensure!(
            !hostdo_invokes_hat(&entry.argv),
            "invalid [hostdo].commands entry '{}' in {}: invoking hat through hostdo is forbidden",
            entry.argv.join(" "),
            path.display()
        );
        if let Some(allowlist) = &entry.env_allowlist {
            anyhow::ensure!(
                entry.image.is_none(),
                "invalid [hostdo].commands entry '{}' in {}: env_allowlist is not supported on image rules (the runner container already starts with a clean environment)",
                entry.argv.join(" "),
                path.display()
            );
            for name in allowlist {
                anyhow::ensure!(
                    crate::fs_util::is_valid_env_name(name),
                    "invalid [hostdo].commands entry '{}' in {}: env_allowlist entry {name:?} is not a valid POSIX env variable name",
                    entry.argv.join(" "),
                    path.display()
                );
            }
        }
    }
    Ok(())
}

/// Return true when a hostdo argv directly targets the Harness Hat CLI.
/// Accept both container/Unix and Windows path separators because rules may be
/// shared across hosts, and compare case-insensitively for Windows parity.
/// Legacy `hht` names remain blocked so an older installed binary cannot bypass
/// the hostdo restriction during the CLI rename.
pub(crate) fn hostdo_invokes_hat(argv: &[String]) -> bool {
    let Some(executable) = argv.first() else {
        return false;
    };
    let basename = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .trim();
    matches!(
        basename.to_ascii_lowercase().as_str(),
        "hat" | "hat.exe" | "hht" | "hht.exe"
    )
}

/// Persist a host command approval for future requests.
///
/// The command is deduplicated by `argv + image` and forced to
/// `approval_mode = "auto"`.
/// Matching still depends on `argv + image` only; `timeout_secs` and `reason` are
/// persisted for review context and do not gate future auto-approvals.
pub fn append_hostdo_auto_approval(
    path: &Path,
    argv: &[String],
    image: Option<&str>,
    timeout_secs: u64,
    reason: Option<&str>,
) -> Result<bool> {
    if argv.is_empty() {
        return Ok(false);
    }
    anyhow::ensure!(
        !hostdo_invokes_hat(argv),
        "invoking hat through hostdo is forbidden"
    );
    let timeout_secs = clamp_hostdo_timeout_secs(timeout_secs);

    let mut rules = load(path)?;
    let mut changed = false;

    if let Some(existing) = rules
        .hostdo
        .commands
        .iter_mut()
        .find(|entry| entry.argv == argv && entry.image.as_deref() == image)
    {
        if existing.approval_mode != NetworkPolicy::Auto {
            existing.approval_mode = NetworkPolicy::Auto;
            changed = true;
        }
        if existing.timeout_secs != timeout_secs {
            existing.timeout_secs = timeout_secs;
            changed = true;
        }
        if reason.is_some_and(|reason| existing.reason.as_deref() != Some(reason)) {
            existing.reason = reason.map(str::to_string);
            changed = true;
        }
    } else {
        rules.hostdo.commands.push(HostdoCommand {
            name: None,
            argv: argv.to_vec(),
            image: image.map(str::to_string),
            cwd: Some("$WORKSPACE".to_string()),
            timeout_secs,
            approval_mode: NetworkPolicy::Auto,
            reason: reason.map(str::to_string),
            env_allowlist: None,
        });
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    write_rules_file(path, &rules)
        .with_context(|| format!("writing remembered hostdo approval to {}", path.display()))?;
    Ok(true)
}

fn validate_rules_version(version: u32, path: &Path) -> Result<()> {
    anyhow::ensure!(
        version > 0,
        "harness-rules.toml {}: version must be greater than zero",
        path.display()
    );
    anyhow::ensure!(
        version <= CURRENT_RULES_VERSION,
        "harness-rules.toml {}: unsupported version {}; this build supports up to {}",
        path.display(),
        version,
        CURRENT_RULES_VERSION
    );
    Ok(())
}

/// Write rules to a file with the standard comment header.
pub fn write_rules_file(path: &Path, rules: &ProjectRules) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory for {}", path.display()))?;
    }
    let content = render_rules_file(rules)?;
    crate::config::atomic_write_with_lock(path, content.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Render rules file contents exactly as `write_rules_file` would serialize it.
pub fn render_rules_file(rules: &ProjectRules) -> Result<String> {
    let toml_str = toml::to_string_pretty(rules).context("serializing rules to TOML")?;
    Ok(format!("{RULES_FILE_HEADER}{toml_str}"))
}

const RULES_FILE_HEADER: &str = "\
# harness-rules.toml - network policy for Harness Hat container sessions.
# Commit this file to your repository. Harness Hat reads it but never pushes
# changes back during workspace sync.
#
# Preferred place for human/LLM instructions:
# llm_instructions = \"\"\"\n\
# Environment:
# - You are operating inside a Linux Docker container managed by Harness Hat.
# - Your workspace is mounted into the container; edits usually persist to the host.
# - Do not assume host tools, credentials, or services are directly available in the container.
# - Use `killme` only when the user explicitly asks you to end this container.
# \"\"\"
#
# Workspace path mirroring (enabled by default):
# mirror_cwd = true
# Mounts an absolute POSIX workspace path at that same path in the Linux
# container instead of `/workspace`. Windows drive paths use a best-effort
# container path such as `/C/Users/example/project`.
#
# Preferred session template (saved by `hat ws` after the first choice):
# template = \"rust\"
# A `template` value in the matching `[[workspaces]]` entry in
# harness-hat.toml overrides this value.
#
# Localhost port forwards (workspace/session overrides):
# [[localhost_forwards]]
# container_port = 8081
# host_port = 11434
# A missing host_port uses the same port. Entries with the same
# container_port replace the configured default/profile forward.
#
# Hostdo (host-side command execution)
#
# default_policy: what happens when a command doesn't match any rule below.
#   auto   — run without prompting (use with caution)
#   prompt — ask the developer in the TUI (default)
#   deny   — reject silently
#
# Passthrough command (exact argv match, auto-approved):
#   [[hostdo.commands]]
#   argv = [\"cargo\", \"test\"] # run inside container with `hostdo run cargo test`
#   cwd = \"$WORKSPACE\"         # execution cwd only, not part of approval matching
#   timeout_secs = 60
#   # Values above 300 seconds are capped at 300 seconds.
#   # Optional context persisted for operator review; does not affect matching.
#   reason = \"run package index refresh after dependency update\"
#   approval_mode = \"auto\"
#
# Short-lived Docker runner (exact argv + image match, auto-approved):
#   [[hostdo.commands]]
#   argv = [\"npm\", \"test\"]   # run with `hostdo run --image node:20 npm test`
#   image = \"node:20\"
#   cwd = \"$WORKSPACE\"
#   timeout_secs = 60
#   # Optional context persisted for operator review; does not affect matching.
#   reason = \"run npm tests in container for repo parity\"
#   approval_mode = \"auto\"
#
# $WORKSPACE = workspace path on the host.
#
# Environment scrubbing (optional, passthrough commands only):
#   env_allowlist = [\"CARGO_TERM_COLOR\", \"CI\"]
# When env_allowlist is present the command runs from a cleared environment
# containing only a small base set (PATH, HOME, USER, LOGNAME, SHELL, TMPDIR,
# LANG, LC_ALL, LC_CTYPE, TERM) plus the variables listed, instead of
# inheriting the manager's full host environment. An empty list grants just
# the base set. Not supported on image rules — the runner container already
# starts with a clean environment.
#
# Network (HTTP/HTTPS proxy policy)
#
# Coder-style network rules. Deny matches win over allow matches; if no rule
# matches, request is prompted.
# Rule format:
#   method=GET,POST domain=api.example.com path=/v1/*,/health port=443,8443
#
# Use [network].allowlist for permanent allows and [network].denylist for
# permanent denies.
#
# Domain matching:
# - `domain=example.com` exact only
# - `domain=*.example.com` subdomains only (not the apex)
#
# Path matching:
# - exact (`/v1/users`)
# - wildcard (`/v1/*`)
#
# Port matching:
# - omitted port matches normal HTTP/HTTPS requests on any port
# - CONNECT allow rules without `port=` only auto-approve port 443
# - raw CONNECT to other ports requires an explicit `port=...` allow rule

";

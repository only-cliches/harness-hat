use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ── Top-level ────────────────────────────────────────────────────────────────

pub const CURRENT_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version for future harness-hat.toml migrations.
    #[serde(default = "current_config_version")]
    pub version: u32,
    pub manager: ManagerConfig,
    /// Directory containing the repository root used for Docker builds.
    /// This is required at startup and is auto-populated by `harness-hat --init`.
    #[serde(default)]
    pub docker_dir: PathBuf,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub env_profiles: HashMap<String, EnvProfile>,
    #[serde(default, alias = "projects")]
    pub workspaces: Vec<WorkspaceConfig>,
    #[serde(default)]
    pub container_profiles: HashMap<String, ContainerProfile>,
    /// Internal resolved launch entries synthesized from `container_profiles`.
    /// Config parsing rejects legacy `[[containers]]` entries.
    #[serde(default)]
    pub containers: Vec<ContainerDef>,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            manager: ManagerConfig::default(),
            // `docker_dir` is expected to be populated during `harness-hat --init`.
            // An empty PathBuf here signifies an uninitialized state.
            docker_dir: PathBuf::new(),
            defaults: DefaultsConfig::default(),
            env_profiles: HashMap::new(),
            workspaces: Vec::new(),
            container_profiles: HashMap::new(),
            containers: Vec::new(),
            logging: LoggingConfig::default(),
        }
    }
}

pub(crate) fn current_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ManagerConfig {
    /// Path to the global harness-rules.toml where auto-approved commands are persisted.
    /// Created on first use if it does not exist.
    #[serde(alias = "rules_file")]
    pub global_rules_file: PathBuf,
}

// ── Defaults ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    #[serde(default)]
    pub ui: UiDefaults,
    #[serde(default)]
    pub control: ControlDefaults,
    #[serde(default)]
    pub proxy: ProxyDefaults,
    #[serde(default)]
    pub containers: ContainerDefaults,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct UiDefaults {
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    #[serde(default = "default_show_log_pane")]
    pub show_log_pane: bool,
}

/// Provides the default value for `UiDefaults.sidebar_width`.
fn default_sidebar_width() -> u16 {
    32
}

fn default_show_log_pane() -> bool {
    false
}

impl Default for UiDefaults {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            show_log_pane: default_show_log_pane(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ControlDefaults {
    #[serde(default = "default_control_port")]
    pub server_port: u16,
    #[serde(default = "default_host")]
    pub server_host: String,
    #[serde(default = "default_token_env")]
    pub token_env_var: String,
    /// Opt-in to binding the control server / proxy to non-loopback addresses.
    /// Required to expose the API over a network.
    #[serde(default)]
    pub allow_remote_control: bool,
}

/// Provides the default value for `ControlDefaults.server_port`.
fn default_control_port() -> u16 {
    7878
}
/// Provides the default value for `ControlDefaults.server_host`.
fn default_host() -> String {
    "127.0.0.1".to_string()
}
/// Provides the default value for `ControlDefaults.token_env_var`.
fn default_token_env() -> String {
    "HARNESS_HAT_TOKEN".to_string()
}

impl Default for ControlDefaults {
    fn default() -> Self {
        Self {
            server_port: default_control_port(),
            server_host: default_host(),
            token_env_var: default_token_env(),
            allow_remote_control: false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProxyDefaults {
    /// Legacy root-listener port retained only so existing configuration files
    /// continue to deserialize. Sessions now receive authenticated ephemeral
    /// proxy listeners and this value is not bound.
    #[serde(default = "default_proxy_port", rename = "proxy_port")]
    pub legacy_proxy_port: u16,
    #[serde(default = "default_host")]
    pub proxy_host: String,
    /// Close a proxy client or CONNECT tunnel after this many seconds with no
    /// traffic. This is global because listeners are created per session.
    #[serde(default = "default_proxy_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// When enabled, containers are launched with NET_ADMIN + root so they can:
    ///   1) transparently redirect outbound HTTP/HTTPS through the harness-hat proxy
    ///   2) install strict outbound egress rules (iptables) to block direct egress
    ///      outside the proxy and control server.
    ///
    /// This is the recommended "near-impossible to bypass" mode.
    ///
    #[serde(default)]
    pub strict_network: bool,
}

/// Provides the compatibility default for the retired root proxy port.
fn default_proxy_port() -> u16 {
    28781
}

fn default_proxy_idle_timeout_secs() -> u64 {
    30
}

impl Default for ProxyDefaults {
    fn default() -> Self {
        Self {
            legacy_proxy_port: default_proxy_port(),
            proxy_host: default_host(),
            idle_timeout_secs: default_proxy_idle_timeout_secs(),
            strict_network: false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct EnvProfile {
    #[serde(default)]
    pub vars: HashMap<String, String>,
}

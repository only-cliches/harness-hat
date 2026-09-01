use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// Thread-safe hot-reloadable handle to the current config.
///
/// This is used so the TUI can update the config at runtime and the control
/// server + proxy can see the new workspace list without restart.
#[derive(Clone)]
pub struct SharedConfig {
    inner: Arc<RwLock<Arc<Config>>>,
    rules_guard: Arc<RwLock<RulesGuard>>,
}

#[derive(Clone, PartialEq, Eq)]
enum RulesFileFingerprint {
    Missing,
    Contents([u8; 32]),
}

struct GuardedRulesFile {
    trusted: Option<RulesFileFingerprint>,
    blocked: bool,
}

/// A configured policy file and the daemon's current trust decision for it.
/// Paths are derived from the active configuration; callers never supply an
/// arbitrary filesystem path to the rules guard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RulesFileScope {
    Global,
    Workspace { workspace: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulesFileStatus {
    pub scope: RulesFileScope,
    pub path: String,
    pub blocked: bool,
}

#[derive(Default)]
struct RulesGuard {
    files: HashMap<PathBuf, GuardedRulesFile>,
}

impl SharedConfig {
    pub fn new(config: Arc<Config>) -> Self {
        let mut guard = RulesGuard::default();
        guard.track_config(&config);
        Self {
            inner: Arc::new(RwLock::new(config)),
            rules_guard: Arc::new(RwLock::new(guard)),
        }
    }

    pub fn get(&self) -> Arc<Config> {
        // Treat a poisoned lock as recoverable: the inner data is just an
        // `Arc<Config>` whose invariants do not depend on partial mutation,
        // so reading the prior value is safe and preferable to panicking.
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, config: Arc<Config>) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = config;
        let config = self.get();
        self.rules_guard
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .track_config(&config);
    }

    /// Reject policy decisions when a rules file changed outside the manager.
    /// A file is trusted only at startup, after a manager-owned write, or after
    /// the user explicitly trusts the current version in the system dialog.
    pub fn ensure_rules_trusted_for_workspace(&self, workspace_name: Option<&str>) -> Result<()> {
        let config = self.get();
        let paths = rules_paths_for_workspace(&config, workspace_name);
        let mut guard = self.rules_guard.write().unwrap_or_else(|e| e.into_inner());
        for path in paths {
            let current = match rules_file_fingerprint(&path) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    guard.block(&path);
                    bail!(
                        "rules file '{}' cannot be verified: {error}",
                        path.display()
                    );
                }
            };
            let entry = guard.files.entry(path.clone()).or_insert(GuardedRulesFile {
                trusted: None,
                blocked: true,
            });
            if entry.blocked || entry.trusted.as_ref() != Some(&current) {
                entry.blocked = true;
                bail!(
                    "rules file '{}' changed outside Harness Hat and must be reviewed in the system dialog",
                    path.display()
                );
            }
        }
        Ok(())
    }

    /// Mark an externally changed rules file as blocked immediately, before a
    /// user-facing notification is shown.
    pub fn block_rules_file(&self, path: &Path) {
        self.rules_guard
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .block(path);
    }

    /// Trust exactly the file contents currently on disk. Any later change
    /// invalidates this trust on the next policy decision.
    pub fn trust_rules_file(&self, path: &Path) -> Result<()> {
        let current = rules_file_fingerprint(path)?;
        self.set_trusted_rules_file(path, current);
        Ok(())
    }

    /// Report the effective trust state for the configured global file and,
    /// when selected, the matching workspace file. Inspection deliberately
    /// refreshes the guard: a change noticed between filesystem-watch ticks
    /// must still be reported as blocked rather than stale/trusted.
    pub fn rules_status(&self, workspace_name: Option<&str>) -> Result<Vec<RulesFileStatus>> {
        let config = self.get();
        let mut targets = vec![(
            RulesFileScope::Global,
            config.manager.global_rules_file.clone(),
        )];
        if let Some(workspace_name) = workspace_name {
            let workspace = config
                .workspaces
                .iter()
                .find(|workspace| workspace.name == workspace_name)
                .ok_or_else(|| anyhow::anyhow!("no workspace named {workspace_name:?}"))?;
            targets.push((
                RulesFileScope::Workspace {
                    workspace: workspace.name.clone(),
                },
                workspace.canonical_path.join("harness-rules.toml"),
            ));
        } else {
            targets.extend(config.workspaces.iter().map(|workspace| {
                (
                    RulesFileScope::Workspace {
                        workspace: workspace.name.clone(),
                    },
                    workspace.canonical_path.join("harness-rules.toml"),
                )
            }));
        }

        let mut guard = self.rules_guard.write().unwrap_or_else(|e| e.into_inner());
        let mut statuses = Vec::with_capacity(targets.len());
        for (scope, path) in targets {
            let current = rules_file_fingerprint(&path);
            let entry = guard.files.entry(path.clone()).or_insert(GuardedRulesFile {
                trusted: None,
                blocked: true,
            });
            let blocked = match current {
                Ok(current) => entry.blocked || entry.trusted.as_ref() != Some(&current),
                Err(_) => true,
            };
            entry.blocked = blocked;
            statuses.push(RulesFileStatus {
                scope,
                path: path.display().to_string(),
                blocked,
            });
        }
        Ok(statuses)
    }

    /// Trust an internal write only when the file still contains the exact
    /// bytes the manager wrote. This prevents an intervening external change
    /// from being mistaken for a manager-owned update.
    pub fn trust_rules_file_if_contents(&self, path: &Path, expected: &str) -> Result<()> {
        let current = std::fs::read_to_string(path)?;
        if current != expected {
            bail!("rules file contents differ from the manager-owned write");
        }
        // Use the same canonical fingerprint as the enforcement path. In
        // particular, `template` is intentionally excluded because it is
        // launch metadata rather than policy. Storing a raw hash here made a
        // manager-owned remembered-rule write look like an external change on
        // the next proxy request whenever the file had a template selected.
        self.set_trusted_rules_file(path, rules_file_fingerprint(path)?);
        Ok(())
    }

    /// Trust a reviewed version only if the file is still exactly the bytes
    /// that triggered its system dialog. `None` represents a reviewed missing
    /// file, which is a valid rules state.
    pub fn trust_rules_file_if_bytes(&self, path: &Path, expected: Option<&[u8]>) -> Result<()> {
        let current = read_rules_file_bytes(path)?;
        if current.as_deref() != expected {
            bail!("rules file changed while its review dialog was open");
        }
        self.set_trusted_rules_file(path, rules_file_fingerprint(path)?);
        Ok(())
    }

    fn set_trusted_rules_file(&self, path: &Path, current: RulesFileFingerprint) {
        let mut guard = self.rules_guard.write().unwrap_or_else(|e| e.into_inner());
        guard.files.insert(
            path.to_path_buf(),
            GuardedRulesFile {
                trusted: Some(current),
                blocked: false,
            },
        );
    }
}

impl RulesGuard {
    fn track_config(&mut self, config: &Config) {
        for path in rules_paths_for_workspace(config, None).into_iter().chain(
            config
                .workspaces
                .iter()
                .map(|workspace| workspace.canonical_path.join("harness-rules.toml")),
        ) {
            if self.files.contains_key(&path) {
                continue;
            }
            let entry = match rules_file_fingerprint(&path) {
                Ok(fingerprint) => GuardedRulesFile {
                    trusted: Some(fingerprint),
                    blocked: false,
                },
                Err(_) => GuardedRulesFile {
                    trusted: None,
                    blocked: true,
                },
            };
            self.files.insert(path, entry);
        }
    }

    fn block(&mut self, path: &Path) {
        let entry = self
            .files
            .entry(path.to_path_buf())
            .or_insert(GuardedRulesFile {
                trusted: None,
                blocked: true,
            });
        entry.blocked = true;
    }
}

fn rules_paths_for_workspace(config: &Config, workspace_name: Option<&str>) -> Vec<PathBuf> {
    let mut paths = vec![config.manager.global_rules_file.clone()];
    if let Some(workspace_name) = workspace_name
        && let Some(workspace) = config
            .workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name)
    {
        paths.push(workspace.canonical_path.join("harness-rules.toml"));
    }
    paths
}

fn rules_file_fingerprint(path: &Path) -> Result<RulesFileFingerprint> {
    match read_rules_file_bytes(path)? {
        Some(bytes) => {
            // The workspace template is launch metadata, not a policy decision.
            // The workspace command persists the first selected template itself,
            // so treating that write as a policy-file change would immediately
            // block the launch that made it. Keep every other byte (including
            // comments) in the fingerprint so policy edits still require
            // explicit review.
            let raw = std::str::from_utf8(&bytes)
                .with_context(|| format!("rules file '{}' is not valid UTF-8", path.display()))?;
            let mut document = raw
                .parse::<toml_edit::DocumentMut>()
                .with_context(|| format!("parsing rules file '{}'", path.display()))?;
            document.remove("template");
            Ok(RulesFileFingerprint::Contents(
                Sha256::digest(document.to_string().as_bytes()).into(),
            ))
        }
        None => Ok(RulesFileFingerprint::Missing),
    }
}

fn read_rules_file_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WorkspaceConfig};
    use std::{fs, sync::Arc};

    #[test]
    fn shared_config_hot_reloads() {
        let config1 = Arc::new(Config::default());
        let shared = SharedConfig::new(config1);

        let config2 = Arc::new(Config {
            docker_dir: std::path::PathBuf::from("/new/docker"),
            ..Config::default()
        });

        shared.set(config2);

        let current = shared.get();
        assert_eq!(current.docker_dir, std::path::PathBuf::from("/new/docker"));
    }

    #[test]
    fn shared_config_clones_independent_reference() {
        let config1 = Arc::new(Config::default());
        let shared1 = SharedConfig::new(config1);
        let shared2 = shared1.clone();

        let config2 = Arc::new(Config {
            docker_dir: std::path::PathBuf::from("/shared/docker"),
            ..Config::default()
        });

        shared1.set(config2);

        // Both clones should see the update
        assert_eq!(
            shared2.get().docker_dir,
            std::path::PathBuf::from("/shared/docker")
        );
    }

    #[test]
    fn external_rules_change_blocks_until_current_contents_are_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join("harness-rules.toml");
        fs::write(&rules_path, "[network]\ndefault_policy = 'prompt'\n").unwrap();
        let config = Arc::new(Config {
            manager: crate::config::ManagerConfig {
                global_rules_file: rules_path.clone(),
            },
            ..Config::default()
        });
        let shared = SharedConfig::new(config);

        shared.ensure_rules_trusted_for_workspace(None).unwrap();
        fs::write(&rules_path, "[network]\ndefault_policy = 'deny'\n").unwrap();
        assert!(shared.ensure_rules_trusted_for_workspace(None).is_err());

        shared.trust_rules_file(&rules_path).unwrap();
        shared.ensure_rules_trusted_for_workspace(None).unwrap();
    }

    #[test]
    fn status_and_trust_are_scoped_to_one_rules_file() {
        let dir = tempfile::tempdir().unwrap();
        let global_rules = dir.path().join("global-rules.toml");
        let workspace_dir = dir.path().join("workspace");
        fs::create_dir(&workspace_dir).unwrap();
        let workspace_rules = workspace_dir.join("harness-rules.toml");
        fs::write(&global_rules, "[network]\ndefault_policy = 'prompt'\n").unwrap();
        fs::write(&workspace_rules, "[network]\ndefault_policy = 'prompt'\n").unwrap();
        let config = Arc::new(Config {
            manager: crate::config::ManagerConfig {
                global_rules_file: global_rules.clone(),
            },
            workspaces: vec![WorkspaceConfig {
                name: "api".to_string(),
                canonical_path: workspace_dir,
                sidebar_hotkey: None,
                template: None,
                mount_cwd: false,
            }],
            ..Config::default()
        });
        let shared = SharedConfig::new(config);

        fs::write(&workspace_rules, "[network]\ndefault_policy = 'deny'\n").unwrap();
        let status = shared.rules_status(Some("api")).unwrap();
        assert!(!status[0].blocked, "global rules stay trusted");
        assert!(status[1].blocked, "changed workspace rules are blocked");

        shared.trust_rules_file(&workspace_rules).unwrap();
        let status = shared.rules_status(Some("api")).unwrap();
        assert!(status.iter().all(|rule| !rule.blocked));

        fs::write(&global_rules, "[network]\ndefault_policy = 'deny'\n").unwrap();
        let status = shared.rules_status(Some("api")).unwrap();
        assert!(status[0].blocked, "changed global rules are blocked");
        assert!(!status[1].blocked, "workspace trust remains intact");
    }

    #[test]
    fn workspace_template_change_does_not_block_rules_trust() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join("harness-rules.toml");
        fs::write(
            &rules_path,
            "version = 1\n[network]\nallowlist = ['domain=example.com']\n",
        )
        .unwrap();
        let config = Arc::new(Config {
            manager: crate::config::ManagerConfig {
                global_rules_file: rules_path.clone(),
            },
            ..Config::default()
        });
        let shared = SharedConfig::new(config);

        fs::write(
            &rules_path,
            "version = 1\ntemplate = 'typescript'\n[network]\nallowlist = ['domain=example.com']\n",
        )
        .unwrap();

        shared.ensure_rules_trusted_for_workspace(None).unwrap();
    }

    #[test]
    fn manager_owned_rules_write_with_template_remains_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join("harness-rules.toml");
        fs::write(
            &rules_path,
            "version = 1\ntemplate = 'rust'\n[network]\nallowlist = ['domain=before.example']\n",
        )
        .unwrap();
        let config = Arc::new(Config {
            manager: crate::config::ManagerConfig {
                global_rules_file: rules_path.clone(),
            },
            ..Config::default()
        });
        let shared = SharedConfig::new(config);

        let remembered_contents =
            "version = 1\ntemplate = 'rust'\n[network]\nallowlist = ['domain=after.example']\n";
        fs::write(&rules_path, remembered_contents).unwrap();
        shared
            .trust_rules_file_if_contents(&rules_path, remembered_contents)
            .unwrap();

        shared.ensure_rules_trusted_for_workspace(None).unwrap();
    }

    #[test]
    fn unreadable_or_missing_trust_baseline_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join("rules-directory");
        fs::create_dir(&rules_path).unwrap();
        let config = Arc::new(Config {
            manager: crate::config::ManagerConfig {
                global_rules_file: rules_path,
            },
            ..Config::default()
        });

        let shared = SharedConfig::new(config);
        assert!(shared.ensure_rules_trusted_for_workspace(None).is_err());
    }

    #[test]
    fn reviewed_rules_must_not_change_before_trust() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join("harness-rules.toml");
        fs::write(&rules_path, "first").unwrap();
        let config = Arc::new(Config {
            manager: crate::config::ManagerConfig {
                global_rules_file: rules_path.clone(),
            },
            ..Config::default()
        });
        let shared = SharedConfig::new(config);

        fs::write(&rules_path, "second").unwrap();
        shared.block_rules_file(&rules_path);
        assert!(
            shared
                .trust_rules_file_if_bytes(&rules_path, Some(b"first"))
                .is_err()
        );
        assert!(shared.ensure_rules_trusted_for_workspace(None).is_err());
    }
}

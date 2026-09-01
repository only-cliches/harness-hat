use super::{
    Config, ContainerDef, ContainerDefaults, DefaultsConfig, default_mount_target,
    image_tag_for_stem, load, resolve_workspace_container_templates,
    resolve_workspace_sidebar_hotkeys,
};
use std::path::Path;

fn temp_workspace() -> tempfile::TempDir {
    let root = if cfg!(unix) {
        let unix_tmp = Path::new("/tmp");
        if unix_tmp.exists() {
            unix_tmp.to_path_buf()
        } else {
            std::env::temp_dir()
        }
    } else {
        std::env::temp_dir()
    };
    tempfile::tempdir_in(root).expect("tempdir")
}

fn container_def_for_test(name: &str, image: &str, image_stem: &str) -> ContainerDef {
    ContainerDef {
        name: name.to_string(),
        profile: None,
        image: image.to_string(),
        image_stem: image_stem.to_string(),
        dockerfile_path: None,
        mount_target: default_mount_target(),
        command: None,
        grayscale_palette: false,
        starter_network_allowlist: Vec::new(),
        allowed_hosts: Vec::new(),
        mcp_log_paths: Vec::new(),
        mcp_log_pattern: None,
        mounts: Vec::new(),
        env: Default::default(),
        env_passthrough: Vec::new(),
        localhost_forwards: Vec::new(),
        memory: None,
        cpus: None,
        shm_size: None,
        attach_shell: None,
        claude_settings: None,
    }
}

fn with_temp_home<R>(home: &Path, f: impl FnOnce() -> R) -> R {
    let _guard = crate::TEST_ENV_LOCK.lock().expect("test env lock");
    let original_home = std::env::var_os("HOME");
    let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
    }
    let result = f();
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
    result
}

#[test]
fn defaults_include_control_server() {
    let defaults = DefaultsConfig::default();
    assert_eq!(defaults.control.server_port, 7878);
    assert_eq!(defaults.control.server_host, "127.0.0.1");
    assert_eq!(defaults.control.token_env_var, "HARNESS_HAT_TOKEN");
}

#[test]
fn image_tag_for_stem_is_stable() {
    assert_eq!(image_tag_for_stem("default"), "harness-hat-default:local");
    assert_eq!(image_tag_for_stem("rust.dev"), "harness-hat-rust.dev:local");
}

#[test]
fn load_resolves_template_resource_fields() {
    let root = temp_workspace();
    let workspace = root.path().join("repo");
    let docker_dir = root.path().join("docker");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&docker_dir).expect("docker");
    let config_path = root.path().join("harness-hat.toml");
    let toml_path =
        |path: &std::path::Path| toml::Value::String(path.display().to_string()).to_string();
    std::fs::write(
        &config_path,
        format!(
            r#"
version = 1
docker_dir = {}

[manager]
global_rules_file = {}

[[workspaces]]
name = "repo"
canonical_path = {}

[defaults.containers]
memory = "2g"
cpus = "1.5"
shm_size = "512m"

[container_profiles.dev]
image = "default"
"#,
            toml_path(&docker_dir),
            toml_path(&root.path().join("global-rules.toml")),
            toml_path(&workspace)
        ),
    )
    .expect("write config");
    std::fs::write(root.path().join(".claude.json"), "{}").expect("write claude json");
    std::fs::create_dir_all(root.path().join(".claude")).expect("create claude dir");
    std::fs::write(root.path().join(".claude/.claude.json"), "{}")
        .expect("write nested claude json");
    std::fs::create_dir_all(root.path().join(".codex")).expect("create codex dir");
    std::fs::create_dir_all(root.path().join(".config/codex")).expect("create config codex dir");
    std::fs::create_dir_all(root.path().join(".config/opencode"))
        .expect("create OpenCode config dir");
    std::fs::create_dir_all(root.path().join(".gemini")).expect("create gemini dir");
    std::fs::create_dir_all(root.path().join(".pi")).expect("create pi dir");

    let cfg = with_temp_home(root.path(), || load(&config_path)).expect("load config");
    let template = cfg.containers.iter().find(|ctr| ctr.name == "dev").unwrap();
    assert_eq!(template.image, "harness-hat-default:local");
    assert_eq!(template.memory.as_deref(), Some("2g"));
    assert_eq!(template.cpus.as_deref(), Some("1.5"));
    assert_eq!(template.shm_size.as_deref(), Some("512m"));
    // `dirs` uses the Windows Known Folder API rather than the Unix HOME
    // variable, so derive the expected profile exactly as production does.
    // On Unix the test's HOME override still resolves to the temp directory.
    let home = with_temp_home(root.path(), || {
        dirs::home_dir().expect("platform home directory")
    });
    // The keyring mount host is `dirs::data_dir()` joined under the (temp) home,
    // matching `shared_container_keyring_mount`. This resolves differently per
    // platform (`~/.local/share` on Linux via XDG, `~/Library/Application
    // Support` on macOS), so derive it the same way the code does rather than
    // hardcoding a Linux path.
    let keyring_host = with_temp_home(root.path(), || {
        dirs::data_dir()
            .expect("data dir")
            .join("harness-hat/container-keyrings")
    });
    let optional_session_mounts = [
        (home.join(".claude.json"), "/home/coder/.claude.json"),
        (
            home.join(".claude/.claude.json"),
            "/home/coder/.claude/.claude.json",
        ),
        (home.join(".claude"), "/home/coder/.claude"),
        (home.join(".codex"), "/home/coder/.codex"),
        (home.join(".config/codex"), "/home/coder/.config/codex"),
        (
            home.join(".config/opencode"),
            "/home/coder/.config/opencode",
        ),
        (home.join(".gemini"), "/home/coder/.gemini"),
        (home.join(".pi"), "/home/coder/.pi"),
    ];
    let mut expected_session_mounts: Vec<_> = optional_session_mounts
        .into_iter()
        .filter(|(host, _)| host.exists())
        .collect();
    expected_session_mounts.push((keyring_host.clone(), "/home/coder/.local/share/keyrings"));
    for (host, container) in &expected_session_mounts {
        assert!(
            template.mounts.iter().any(|mount| {
                mount.host == *host && mount.container == std::path::PathBuf::from(container)
            }),
            "missing shared session mount {:?} -> {container}",
            host
        );
    }
    for (host, container) in &expected_session_mounts {
        let expected_container = std::path::PathBuf::from(container);
        if *host == expected_container {
            continue;
        }
        assert!(
            !template
                .mounts
                .iter()
                .any(|mount| mount.host == *host && mount.container == *host),
            "unexpected host-absolute shared session mount {:?}",
            host
        );
    }
}

#[test]
fn workspace_hotkeys_are_assigned_without_duplicates() {
    let workspaces = vec![
        super::WorkspaceConfig {
            name: "alpha".to_string(),
            canonical_path: std::path::PathBuf::from("/tmp/a"),
            sidebar_hotkey: Some("z".to_string()),
            template: None,
            mount_cwd: false,
        },
        super::WorkspaceConfig {
            name: "beta".to_string(),
            canonical_path: std::path::PathBuf::from("/tmp/b"),
            sidebar_hotkey: Some("z".to_string()),
            template: None,
            mount_cwd: false,
        },
    ];
    let hotkeys = resolve_workspace_sidebar_hotkeys(&workspaces);
    assert_eq!(hotkeys[0], Some('z'));
    assert_ne!(hotkeys[0], hotkeys[1]);
}

#[test]
fn workspace_container_templates_include_local_dockerfiles_with_base_tag() {
    let root = temp_workspace();
    with_temp_home(root.path(), || {
        let workspace = root.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let nested = workspace.join("services");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let matching_root = workspace.join("custom.dockerfile");
        std::fs::write(
            &matching_root,
            "FROM harness-hat-base:local\nRUN echo hello",
        )
        .expect("write matching root dockerfile");
        let matching_nested = nested.join("nested.dockerfile");
        std::fs::write(
            &matching_nested,
            "  # ignored comment\n\nFROM harness-hat-base:local as nested\nRUN echo nested",
        )
        .expect("write matching nested dockerfile");
        let non_matching = workspace.join("ignored.dockerfile");
        std::fs::write(&non_matching, "FROM ubuntu:24.04\nRUN echo ignored")
            .expect("write non matching dockerfile");

        let configured = vec![container_def_for_test(
            "configured",
            "harness-hat-configured:local",
            "configured",
        )];
        let defaults = ContainerDefaults {
            memory: Some("2g".to_string()),
            cpus: Some("1.2".to_string()),
            mount_target: None,
            grayscale_palette: None,
            mounts: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            env: Default::default(),
            env_passthrough: Vec::new(),
            allowed_hosts: Vec::new(),
            localhost_forwards: Vec::new(),
            shm_size: Some("1g".to_string()),
            attach_shell: None,
            claude_settings: None,
        };

        let templates = resolve_workspace_container_templates(&workspace, &defaults, &configured)
            .expect("discover templates");

        assert_eq!(templates.len(), 3);
        let configured_idx = templates
            .iter()
            .position(|c| c.name == "configured")
            .expect("configured template");
        let local_root_idx = templates
            .iter()
            .position(|c| c.name == "custom")
            .expect("custom template");
        let local_nested_idx = templates
            .iter()
            .position(|c| c.name == "services/nested")
            .expect("nested template");

        assert!(configured_idx < local_root_idx);
        assert!(local_root_idx < local_nested_idx);

        let local_root = templates[local_root_idx].clone();
        assert_eq!(
            local_root.dockerfile_path,
            Some(matching_root.clone()),
            "local template should remember its dockerfile path"
        );
        assert_eq!(local_root.image, "harness-hat-local-repo-custom:local");
        assert_eq!(local_root.memory.as_deref(), Some("2g"));
        assert_eq!(local_root.cpus.as_deref(), Some("1.2"));
        assert_eq!(local_root.shm_size.as_deref(), Some("1g"));

        let local_nested = templates[local_nested_idx].clone();
        assert_eq!(
            local_nested.dockerfile_path,
            Some(matching_nested),
            "local template should point to nested dockerfile"
        );
        assert_eq!(
            local_nested.image,
            "harness-hat-local-repo-services_nested:local"
        );

        let ignored_present = templates.iter().any(|c| c.name == "ignored");
        assert!(
            !ignored_present,
            "non-matching dockerfile should be ignored"
        );
    });
}

#[test]
fn workspace_container_templates_merge_deduplicates_matching_names() {
    let root = temp_workspace();
    with_temp_home(root.path(), || {
        let workspace = root.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let first = workspace.join("same.dockerfile");
        let second = workspace.join("alt").join("same.dockerfile");
        std::fs::create_dir_all(workspace.join("alt")).expect("alt");
        std::fs::write(&first, "FROM harness-hat-base:local\n").expect("write first");
        std::fs::write(&second, "FROM harness-hat-base:local\n").expect("write second");

        let configured = vec![
            container_def_for_test("configured", "harness-hat-configured:local", "configured"),
            container_def_for_test("same", "harness-hat-same:local", "same"),
        ];
        let defaults = ContainerDefaults {
            memory: None,
            cpus: None,
            mount_target: None,
            grayscale_palette: None,
            mounts: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            env: Default::default(),
            env_passthrough: Vec::new(),
            allowed_hosts: Vec::new(),
            localhost_forwards: Vec::new(),
            shm_size: None,
            attach_shell: None,
            claude_settings: None,
        };

        let templates = resolve_workspace_container_templates(&workspace, &defaults, &configured)
            .expect("discover templates");

        let names: Vec<_> = templates.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"same"));
        assert!(names.contains(&"same (local)"));
        assert!(names.contains(&"alt/same"));
    });
}

#[test]
fn empty_config_default_is_valid_structurally() {
    let cfg = Config::default();
    assert!(cfg.containers.is_empty());
    assert!(cfg.workspaces.is_empty());
}

#[test]
fn merge_mounts_collapses_duplicate_container_destinations() {
    use crate::config::{ContainerMount, MountMode};
    use std::path::PathBuf;

    // Docker rejects two mounts that share a container destination with
    // "Duplicate mount point", which aborts the whole `docker run` before any
    // container is created. This is exactly what happens on macOS when a
    // config-defined keyring mount (host `~/.local/share/...`) collides with
    // the code-injected one (`dirs::data_dir()` = `~/Library/Application
    // Support/...`): same container path, different hosts. `merge_mounts` must
    // collapse them to a single mount, with the later (session-state/code)
    // layer winning.
    let dest = PathBuf::from("/home/coder/.local/share/keyrings");
    let config_mount = ContainerMount {
        host: PathBuf::from("/Users/me/.local/share/harness-hat/container-keyrings"),
        container: dest.clone(),
        mode: MountMode::Rw,
        seed: None,
        add_to_path: false,
    };
    let code_mount = ContainerMount {
        host: PathBuf::from("/Users/me/Library/Application Support/harness-hat/container-keyrings"),
        container: dest.clone(),
        mode: MountMode::Rw,
        seed: None,
        add_to_path: false,
    };

    let merged = super::merge_mounts(
        std::slice::from_ref(&config_mount),
        std::slice::from_ref(&code_mount),
        &[],
    );

    let at_dest: Vec<_> = merged.iter().filter(|m| m.container == dest).collect();
    assert_eq!(
        at_dest.len(),
        1,
        "duplicate container destination must collapse to a single mount, got {at_dest:?}"
    );
    assert_eq!(
        at_dest[0].host, code_mount.host,
        "later layer (session-state/code mount) must win the destination"
    );

    // General invariant: no two resulting mounts may share a container path.
    let mut dests: Vec<&PathBuf> = merged.iter().map(|m| &m.container).collect();
    dests.sort();
    let unique = dests.len();
    dests.dedup();
    assert_eq!(
        unique,
        dests.len(),
        "merge_mounts must not emit duplicate container destinations"
    );
}

#[test]
fn merge_mounts_lets_later_layers_override_same_destination() {
    use crate::config::{ContainerMount, MountMode};
    use std::path::PathBuf;

    // A per-profile override mount for a destination already provided by the
    // defaults layer should replace it, not stack a second mount on the same
    // container path.
    let dest = PathBuf::from("/home/coder/.config/tool");
    let base = ContainerMount {
        host: PathBuf::from("/host/base/tool"),
        container: dest.clone(),
        mode: MountMode::Ro,
        seed: None,
        add_to_path: false,
    };
    let override_mount = ContainerMount {
        host: PathBuf::from("/host/override/tool"),
        container: dest.clone(),
        mode: MountMode::Rw,
        seed: None,
        add_to_path: false,
    };

    let merged = super::merge_mounts(
        std::slice::from_ref(&base),
        &[],
        std::slice::from_ref(&override_mount),
    );

    let at_dest: Vec<_> = merged.iter().filter(|m| m.container == dest).collect();
    assert_eq!(at_dest.len(), 1, "override must replace, not duplicate");
    assert_eq!(at_dest[0].host, override_mount.host);
    assert_eq!(at_dest[0].mode, MountMode::Rw);
}

#[test]
fn container_path_helpers_use_posix_semantics() {
    use crate::config::{container_path_string, is_absolute_container_path, join_container_path};
    use std::path::Path;

    assert_eq!(
        container_path_string(Path::new("\\home\\coder\\project")),
        "/home/coder/project"
    );
    assert!(is_absolute_container_path(Path::new("\\home\\coder")));
    assert!(!is_absolute_container_path(Path::new("C:\\Users\\dev")));
    assert_eq!(
        join_container_path("/workspace", Path::new("nested\\crate")),
        "/workspace/nested/crate"
    );
    assert_eq!(join_container_path("/", Path::new("nested")), "/nested");
}

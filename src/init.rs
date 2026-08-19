use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::instrument;

const SAMPLE_CONFIG: &str = include_str!("../harness-hat.example.toml");
const DOCKER_DIR_PLACEHOLDER: &str = "__HARNESS_HAT_DOCKER_DIR__";
const BASE_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/harness-hat-base.dockerfile");
const DEFAULT_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/default.dockerfile");
const TYPESCRIPT_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/typescript.dockerfile");
const GO_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/go.dockerfile");
const RUST_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/rust.dockerfile");
const PYTHON_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/python.dockerfile");
const KOTLIN_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/kotlin.dockerfile");
const ANDROID_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/android.dockerfile");
const CSHARP_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/csharp.dockerfile");
const PHP_DOCKERFILE_TEMPLATE: &str = include_str!("../docker/php.dockerfile");

/// Built-in dockerfile templates compiled into the binary.
///
/// The `default.dockerfile` is intentionally omitted because it is meant to be
/// user-editable and is treated as a regenerable template (only created when
/// missing — see [`ensure_default_dockerfile`]).
const BUILTIN_DOCKERFILES: &[(&str, &str)] = &[
    ("harness-hat-base.dockerfile", BASE_DOCKERFILE_TEMPLATE),
    ("typescript.dockerfile", TYPESCRIPT_DOCKERFILE_TEMPLATE),
    ("go.dockerfile", GO_DOCKERFILE_TEMPLATE),
    ("rust.dockerfile", RUST_DOCKERFILE_TEMPLATE),
    ("python.dockerfile", PYTHON_DOCKERFILE_TEMPLATE),
    ("kotlin.dockerfile", KOTLIN_DOCKERFILE_TEMPLATE),
    ("android.dockerfile", ANDROID_DOCKERFILE_TEMPLATE),
    ("csharp.dockerfile", CSHARP_DOCKERFILE_TEMPLATE),
    ("php.dockerfile", PHP_DOCKERFILE_TEMPLATE),
];

const HOSTDO_SCRIPT: &str = include_str!("../docker/scripts/hostdo.py");
const KILLME_SCRIPT: &str = include_str!("../docker/scripts/killme.py");

#[instrument(skip(output))]
pub fn write_sample_config(output: &Path) -> Result<()> {
    if output.exists() {
        bail!(
            "file already exists: {}  (delete it first or choose a different path)",
            output.display()
        );
    }
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let cwd = std::env::current_dir()?;
    let home_config_root = dirs::home_dir()
        .context("could not determine home directory")?
        .join(".config/harness-hat");
    let docker_dir = resolve_init_docker_dir(&cwd, &home_config_root);
    fs::create_dir_all(&docker_dir)?;
    ensure_builtin_dockerfiles(&docker_dir)?;
    let docker_dir_literal = toml::Value::String(docker_dir.display().to_string()).to_string();
    let sample = SAMPLE_CONFIG.replace(DOCKER_DIR_PLACEHOLDER, &docker_dir_literal);
    std::fs::write(output, sample)?;
    Ok(())
}

fn resolve_init_docker_dir(cwd: &Path, home_config_root: &Path) -> PathBuf {
    let local_docker_dir = cwd.join("docker");
    if local_docker_dir.is_dir() {
        local_docker_dir
    } else {
        home_config_root.join("docker")
    }
}

#[instrument(skip(docker_dir))]
pub fn ensure_docker_assets(docker_dir: &Path) -> Result<()> {
    ensure_builtin_dockerfiles(docker_dir)?;
    ensure_helper_scripts(docker_dir)?;

    let missing_dockerfiles = missing_builtin_dockerfiles(docker_dir);
    let missing_helper_scripts = missing_helper_scripts(docker_dir);

    if missing_dockerfiles.is_empty() && missing_helper_scripts.is_empty() {
        return Ok(());
    }

    println!(
        "harness-hat: the docker assets in {} are incomplete",
        docker_dir.display()
    );
    if !missing_dockerfiles.is_empty() {
        println!("  Missing Dockerfiles:");
        for file in &missing_dockerfiles {
            println!("    - {}", file.display());
        }
        println!("  These will be written from the installed binary.");
    }
    if !missing_helper_scripts.is_empty() {
        println!("  Missing helper scripts:");
        for file in &missing_helper_scripts {
            println!("    - {}", file.display());
        }
        println!("  These will be written from the installed binary.");
    }

    if !prompt_yes_no("Create the missing docker assets now? [y/N]: ")? {
        return Ok(());
    }

    fs::create_dir_all(docker_dir)?;
    ensure_builtin_dockerfiles(docker_dir)?;
    ensure_helper_scripts(docker_dir)?;
    Ok(())
}

#[instrument(skip(docker_dir))]
pub(crate) fn ensure_default_dockerfile(docker_dir: &Path) -> Result<()> {
    ensure_template_dockerfile(
        docker_dir,
        "default.dockerfile",
        DEFAULT_DOCKERFILE_TEMPLATE,
    )
}

#[instrument(skip(docker_dir))]
pub(crate) fn ensure_base_dockerfile(docker_dir: &Path) -> Result<()> {
    ensure_template_dockerfile(
        docker_dir,
        "harness-hat-base.dockerfile",
        BASE_DOCKERFILE_TEMPLATE,
    )
}

#[instrument(skip(docker_dir))]
pub fn ensure_language_dockerfiles(docker_dir: &Path) -> Result<()> {
    ensure_template_dockerfile(
        docker_dir,
        "typescript.dockerfile",
        TYPESCRIPT_DOCKERFILE_TEMPLATE,
    )?;
    ensure_template_dockerfile(docker_dir, "go.dockerfile", GO_DOCKERFILE_TEMPLATE)?;
    ensure_template_dockerfile(docker_dir, "rust.dockerfile", RUST_DOCKERFILE_TEMPLATE)?;
    ensure_template_dockerfile(docker_dir, "python.dockerfile", PYTHON_DOCKERFILE_TEMPLATE)?;
    ensure_template_dockerfile(docker_dir, "kotlin.dockerfile", KOTLIN_DOCKERFILE_TEMPLATE)?;
    ensure_template_dockerfile(
        docker_dir,
        "android.dockerfile",
        ANDROID_DOCKERFILE_TEMPLATE,
    )?;
    ensure_template_dockerfile(docker_dir, "csharp.dockerfile", CSHARP_DOCKERFILE_TEMPLATE)?;
    ensure_template_dockerfile(docker_dir, "php.dockerfile", PHP_DOCKERFILE_TEMPLATE)?;
    Ok(())
}

/// Refresh assets owned by Harness Hat during a graphical app upgrade.
/// `default.dockerfile` is intentionally excluded because it is the user's
/// editable fallback template.
pub fn refresh_managed_docker_assets(docker_dir: &Path) -> Result<()> {
    fs::create_dir_all(docker_dir)?;
    for (name, contents) in BUILTIN_DOCKERFILES {
        write_text_file(&docker_dir.join(name), contents)?;
    }
    ensure_default_dockerfile(docker_dir)?;
    ensure_helper_scripts(docker_dir)
}

fn ensure_builtin_dockerfiles(docker_dir: &Path) -> Result<()> {
    ensure_base_dockerfile(docker_dir)?;
    ensure_default_dockerfile(docker_dir)?;
    ensure_language_dockerfiles(docker_dir)
}

fn ensure_template_dockerfile(docker_dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = docker_dir.join(name);
    if path.exists() {
        return Ok(());
    }
    write_text_file(&path, contents)
}

#[cfg(test)]
pub fn builtin_dockerfile_paths() -> Vec<&'static str> {
    BUILTIN_DOCKERFILES.iter().map(|(name, _)| *name).collect()
}

fn missing_builtin_dockerfiles(docker_dir: &Path) -> Vec<PathBuf> {
    BUILTIN_DOCKERFILES
        .iter()
        .map(|(rel, _)| docker_dir.join(rel))
        .filter(|path| !path.exists())
        .collect()
}

fn missing_helper_scripts(docker_dir: &Path) -> Vec<PathBuf> {
    helper_script_paths(docker_dir)
        .into_iter()
        .filter(|path| !path.exists())
        .collect()
}

fn helper_script_paths(docker_dir: &Path) -> Vec<PathBuf> {
    vec![
        docker_dir.join("scripts/hostdo.py"),
        docker_dir.join("scripts/killme.py"),
    ]
}

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[instrument(skip(docker_dir))]
pub fn ensure_helper_scripts(docker_dir: &Path) -> Result<()> {
    let scripts_dir = docker_dir.join("scripts");
    fs::create_dir_all(&scripts_dir)?;
    write_text_file(&scripts_dir.join("hostdo.py"), HOSTDO_SCRIPT)?;
    write_text_file(&scripts_dir.join("killme.py"), KILLME_SCRIPT)?;
    Ok(())
}

fn write_text_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // These files execute inside Linux containers even when `hat init` runs on
    // Windows. Keep their shebangs and Dockerfile heredocs free of CRLF.
    fs::write(path, contents.replace("\r\n", "\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        builtin_dockerfile_paths, ensure_base_dockerfile, ensure_default_dockerfile,
        ensure_docker_assets, ensure_language_dockerfiles, refresh_managed_docker_assets,
        resolve_init_docker_dir, write_sample_config,
    };
    use crate::config::Config;

    #[test]
    fn sample_config_writes_parseable_docker_dir() {
        let root = std::env::temp_dir().join(format!("harness-hat-init-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp dir");
        let output = root.join("harness-hat.toml");
        let cwd = std::env::current_dir().expect("current dir");
        let sample = write_sample_config(&output);
        sample.expect("write sample config");

        let contents = std::fs::read_to_string(&output).expect("read sample config");
        let parsed: Config = toml::from_str(&contents).expect("parse sample config");
        assert_eq!(parsed.docker_dir, cwd.join("docker"));

        assert_eq!(parsed.defaults.control.server_port, 7878);
        assert_eq!(parsed.defaults.control.server_host, "127.0.0.1");
        for (host, container) in [
            ("~/.claude.json", "/home/coder/.claude.json"),
            ("~/.claude/.claude.json", "/home/coder/.claude/.claude.json"),
            ("~/.claude", "/home/coder/.claude"),
            ("~/.codex", "/home/coder/.codex"),
            ("~/.config/codex", "/home/coder/.config/codex"),
            // Antigravity CLI stores auth, settings, and conversations under
            // ~/.gemini/antigravity-cli, so the broader Gemini state root is
            // still the correct passthrough mount after migrating from Gemini CLI.
            ("~/.gemini", "/home/coder/.gemini"),
            ("~/.pi", "/home/coder/.pi"),
        ] {
            assert!(
                parsed.defaults.containers.mounts.iter().any(|mount| {
                    mount.host == std::path::PathBuf::from(host)
                        && mount.container == std::path::PathBuf::from(container)
                }),
                "missing shared defaults mount {host} -> {container}"
            );
        }

        let base = parsed.container_profiles.get("base").expect("base profile");
        assert_eq!(base.image.as_deref(), Some("default"));
        assert!(base.command.is_none());

        let large = parsed
            .container_profiles
            .get("large")
            .expect("large profile");
        assert_eq!(large.image.as_deref(), Some("default"));
        assert_eq!(large.memory.as_deref(), Some("8g"));
        assert_eq!(large.cpus.as_deref(), Some("4"));
        assert_eq!(large.shm_size.as_deref(), Some("2g"));

        let android = parsed
            .container_profiles
            .get("android")
            .expect("android profile");
        assert_eq!(android.image.as_deref(), Some("android"));
        assert_eq!(android.memory.as_deref(), Some("8g"));
        assert!(
            android
                .starter_network_allowlist
                .iter()
                .any(|host| host == "domain=dl.google.com")
        );
    }

    #[test]
    fn resolve_init_docker_dir_prefers_local_docker_folder() {
        let root =
            std::env::temp_dir().join(format!("harness-hat-init-local-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("cwd");
        let home = root.join("home/.config/harness-hat");
        std::fs::create_dir_all(cwd.join("docker")).expect("create local docker dir");
        let selected = resolve_init_docker_dir(&cwd, &home);
        assert_eq!(selected, cwd.join("docker"));
    }

    #[test]
    fn resolve_init_docker_dir_falls_back_to_home_config_root() {
        let root =
            std::env::temp_dir().join(format!("harness-hat-init-home-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("cwd");
        let home = root.join("home/.config/harness-hat");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let selected = resolve_init_docker_dir(&cwd, &home);
        assert_eq!(selected, home.join("docker"));
    }

    #[test]
    fn builtin_dockerfile_paths_include_expected_templates() {
        let paths = builtin_dockerfile_paths();
        assert!(paths.contains(&"harness-hat-base.dockerfile"));
        assert!(paths.contains(&"typescript.dockerfile"));
        assert!(paths.contains(&"go.dockerfile"));
        assert!(paths.contains(&"rust.dockerfile"));
        assert!(paths.contains(&"python.dockerfile"));
        assert!(paths.contains(&"kotlin.dockerfile"));
        assert!(paths.contains(&"android.dockerfile"));
        assert!(paths.contains(&"csharp.dockerfile"));
        assert!(paths.contains(&"php.dockerfile"));
    }

    #[test]
    fn graphical_upgrade_refreshes_managed_assets_but_preserves_default() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("harness-hat-base.dockerfile"), "old base").unwrap();
        std::fs::write(root.path().join("default.dockerfile"), "my custom default").unwrap();

        refresh_managed_docker_assets(root.path()).unwrap();

        let base =
            std::fs::read_to_string(root.path().join("harness-hat-base.dockerfile")).unwrap();
        assert!(base.contains("dev.harness-hat.desktop-ssh"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("default.dockerfile")).unwrap(),
            "my custom default"
        );
        assert!(root.path().join("scripts/hostdo.py").is_file());
    }

    #[test]
    fn ensure_docker_assets_is_a_noop_when_complete() {
        let root =
            std::env::temp_dir().join(format!("harness-hat-docker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("scripts")).expect("create scripts dir");
        for path in builtin_dockerfile_paths() {
            let file = root.join(path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("create dockerfile dir");
            }
            std::fs::write(&file, "FROM scratch").expect("write template");
        }
        std::fs::write(root.join("scripts/killme.py"), "killme").expect("write killme");

        ensure_docker_assets(&root).expect("ensure assets");
    }

    #[test]
    fn ensure_docker_assets_refreshes_helper_scripts() {
        let root = std::env::temp_dir().join(format!(
            "harness-hat-docker-refresh-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("scripts")).expect("create scripts dir");
        for path in builtin_dockerfile_paths() {
            let file = root.join(path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("create dockerfile dir");
            }
            std::fs::write(&file, "FROM scratch").expect("write template");
        }
        std::fs::write(root.join("scripts/killme.py"), "old killme").expect("write old killme");

        ensure_docker_assets(&root).expect("ensure assets");

        let killme = std::fs::read_to_string(root.join("scripts/killme.py")).expect("read killme");
        assert!(killme.contains("killme"));
        assert_ne!(killme, "old killme");
    }

    #[test]
    fn ensure_default_dockerfile_creates_template_when_missing() {
        let root = std::env::temp_dir().join(format!(
            "harness-hat-default-dockerfile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create root");
        let path = root.join("default.dockerfile");
        assert!(!path.exists());

        ensure_default_dockerfile(&root).expect("write default dockerfile");
        assert!(path.exists());

        let content = std::fs::read_to_string(path).expect("read default dockerfile");
        assert!(content.contains("harness-hat default image"));
    }

    #[test]
    fn ensure_base_dockerfile_creates_template_when_missing() {
        let root = std::env::temp_dir().join(format!(
            "harness-hat-base-dockerfile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create root");
        let path = root.join("harness-hat-base.dockerfile");
        assert!(!path.exists());

        ensure_base_dockerfile(&root).expect("write base dockerfile");
        assert!(path.exists());

        let content = std::fs::read_to_string(path).expect("read base dockerfile");
        assert!(content.contains("harness-hat base"));
    }

    #[test]
    fn ensure_language_dockerfiles_creates_templates_when_missing() {
        let root = std::env::temp_dir().join(format!(
            "harness-hat-language-dockerfiles-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create root");

        ensure_language_dockerfiles(&root).expect("write language dockerfiles");

        for (path, marker) in [
            ("typescript.dockerfile", "TypeScript / Node / Bun"),
            ("go.dockerfile", "Go image"),
            ("rust.dockerfile", "Rust image"),
            ("python.dockerfile", "harness-hat Python / uv image"),
            ("kotlin.dockerfile", "harness-hat Kotlin / JVM image"),
            ("android.dockerfile", "harness-hat Kotlin / Android image"),
            ("csharp.dockerfile", "harness-hat C# / .NET image"),
            ("php.dockerfile", "PHP image"),
        ] {
            let content = std::fs::read_to_string(root.join(path)).expect("read dockerfile");
            assert!(content.contains(marker), "{path} missing marker {marker}");
        }
    }
}

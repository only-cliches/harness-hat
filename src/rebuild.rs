//! `hat rebuild` — rebuild the base image and configured Dockerfile templates.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const BASE_IMAGE: &str = "harness-hat-base:local";
const BASE_DOCKERFILE: &str = "harness-hat-base.dockerfile";

/// Rebuild the base image, followed by selected templates (or templates whose
/// images already exist locally when `requested_templates` is empty). Pass
/// `all` to rebuild every template found in the configured Docker directory.
pub fn run(
    requested_templates: Vec<String>,
    all: bool,
    no_cache: bool,
    explicit_config: Option<PathBuf>,
) -> Result<()> {
    crate::container::ensure_docker_installed_and_running()?;

    let Some(config_path) = crate::manager::resolve_or_prompt_config_path(explicit_config)? else {
        return Ok(());
    };
    let config = crate::config::load(&config_path)?;
    crate::init::ensure_docker_assets(&config.docker_dir)?;

    let templates = discover_templates(&config.docker_dir)?;
    let built = if requested_templates.is_empty() && !all {
        locally_built_templates(&templates)?
    } else {
        BTreeSet::new()
    };
    let selected = select_templates(&templates, &requested_templates, all, &built)?;

    let base_dockerfile = config.docker_dir.join(BASE_DOCKERFILE);
    if !base_dockerfile.is_file() {
        bail!("base Dockerfile not found: {}", base_dockerfile.display());
    }

    println!("==> Building {BASE_IMAGE}");
    run_docker_build(&base_dockerfile, &config.docker_dir, BASE_IMAGE, no_cache)?;

    if selected.is_empty() {
        if requested_templates.is_empty() && !all {
            println!("==> No previously built template images found");
        } else {
            println!(
                "==> No template Dockerfiles found in {}",
                config.docker_dir.display()
            );
        }
        return Ok(());
    }

    println!(
        "==> Building templates in sequence: {}",
        selected.join(", ")
    );
    let docker_dir = config.docker_dir.clone();
    let mut failures = Vec::new();
    for stem in &selected {
        let dockerfile = templates
            .get(stem)
            .expect("selected templates originate from discovered templates")
            .clone();
        let image = crate::config::image_tag_for_stem(stem);
        println!("==> Building {image}");
        if let Err(error) = run_docker_build(&dockerfile, &docker_dir, &image, no_cache)
            .with_context(|| format!("building template '{stem}'"))
        {
            failures.push(error.to_string());
        }
    }

    if !failures.is_empty() {
        bail!(
            "one or more template builds failed:\n{}",
            failures.join("\n")
        );
    }

    println!("==> All images built successfully");
    Ok(())
}

fn discover_templates(docker_dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let entries = std::fs::read_dir(docker_dir)
        .with_context(|| format!("reading Docker directory {}", docker_dir.display()))?;
    let mut templates = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".dockerfile") else {
            continue;
        };
        if stem.is_empty() || stem == "harness-hat-base" {
            continue;
        }
        templates.insert(stem.to_string(), path);
    }
    Ok(templates)
}

fn select_templates(
    available: &BTreeMap<String, PathBuf>,
    requested: &[String],
    all: bool,
    built: &BTreeSet<String>,
) -> Result<Vec<String>> {
    if all {
        return Ok(available.keys().cloned().collect());
    }

    if requested.is_empty() {
        return Ok(available
            .keys()
            .filter(|name| built.contains(*name))
            .cloned()
            .collect());
    }

    let mut selected = Vec::new();
    for name in requested {
        if !available.contains_key(name) {
            let available = available.keys().cloned().collect::<Vec<_>>().join(", ");
            bail!("unknown template '{name}'; available templates: {available}");
        }
        if !selected.contains(name) {
            selected.push(name.clone());
        }
    }
    Ok(selected)
}

fn locally_built_templates(available: &BTreeMap<String, PathBuf>) -> Result<BTreeSet<String>> {
    let mut built = BTreeSet::new();
    for stem in available.keys() {
        let image = crate::config::image_tag_for_stem(stem);
        if docker_image_exists(&image)? {
            built.insert(stem.clone());
        }
    }
    Ok(built)
}

fn docker_image_exists(image: &str) -> Result<bool> {
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let status = command
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("checking whether docker image {image} exists"))?;
    Ok(status.success())
}

fn run_docker_build(dockerfile: &Path, context: &Path, image: &str, no_cache: bool) -> Result<()> {
    let mut command = Command::new("docker");
    command.arg("build");
    command.arg("--progress").arg("plain");
    command.arg("--network").arg("host");
    if no_cache {
        command.arg("--no-cache");
    }
    let status = command
        .arg("-t")
        .arg(image)
        .arg("-f")
        .arg(dockerfile)
        .arg(context)
        .status()
        .with_context(|| format!("running docker build for {image}"))?;
    if !status.success() {
        bail!("docker build for {image} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{discover_templates, select_templates};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    #[test]
    fn discover_templates_ignores_base_and_non_dockerfiles() {
        let dir = tempfile::tempdir().expect("temp Docker directory");
        std::fs::write(dir.path().join("go.dockerfile"), "FROM scratch").expect("write go");
        std::fs::write(
            dir.path().join("harness-hat-base.dockerfile"),
            "FROM scratch",
        )
        .expect("write base");
        std::fs::write(dir.path().join("notes.txt"), "ignored").expect("write notes");

        let templates = discover_templates(dir.path()).expect("discover templates");
        assert_eq!(templates.keys().collect::<Vec<_>>(), vec!["go"]);
    }

    #[test]
    fn select_templates_deduplicates_requested_names() {
        let available = BTreeMap::from([
            ("go".to_string(), PathBuf::from("go.dockerfile")),
            ("rust".to_string(), PathBuf::from("rust.dockerfile")),
        ]);

        let selected = select_templates(
            &available,
            &["rust".to_string(), "go".to_string(), "rust".to_string()],
            false,
            &BTreeSet::new(),
        )
        .expect("select templates");
        assert_eq!(selected, vec!["rust", "go"]);
    }

    #[test]
    fn select_templates_defaults_to_locally_built_templates() {
        let available = BTreeMap::from([
            ("go".to_string(), PathBuf::from("go.dockerfile")),
            ("rust".to_string(), PathBuf::from("rust.dockerfile")),
        ]);
        let built = BTreeSet::from(["rust".to_string()]);

        let selected =
            select_templates(&available, &[], false, &built).expect("select built templates");
        assert_eq!(selected, vec!["rust"]);
    }

    #[test]
    fn select_templates_all_includes_unbuilt_templates() {
        let available = BTreeMap::from([
            ("go".to_string(), PathBuf::from("go.dockerfile")),
            ("rust".to_string(), PathBuf::from("rust.dockerfile")),
        ]);

        let selected = select_templates(&available, &[], true, &BTreeSet::new())
            .expect("select all templates");
        assert_eq!(selected, vec!["go", "rust"]);
    }

    #[test]
    fn select_templates_rejects_unknown_name() {
        let available = BTreeMap::from([("go".to_string(), PathBuf::from("go.dockerfile"))]);
        let error = select_templates(&available, &["python".to_string()], false, &BTreeSet::new())
            .expect_err("unknown template must fail");
        assert!(error.to_string().contains("unknown template 'python'"));
    }
}

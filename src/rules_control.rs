//! Host-side client for inspecting and explicitly re-trusting managed rules.

use anyhow::{Context, Result, bail};
use std::time::Duration;

use crate::cli::RulesCommand;
use crate::server::{
    ErrorResponse, RulesStatusResponse, RulesTrustRequest, RulesTrustResponse, RulesTrustTarget,
};
use crate::shared_config::RulesFileScope;

pub fn run(command: RulesCommand) -> Result<()> {
    let config_path = crate::manager::default_home_config_path()?;
    if !config_path.exists() {
        bail!(
            "global config does not exist at {}; run `hat install` first",
            config_path.display()
        );
    }
    let config = crate::config::load(&config_path)?;
    let token_path = config.logging.log_dir.join("token");
    let token = std::fs::read_to_string(&token_path).with_context(|| {
        format!(
            "reading daemon token at {}; is the daemon running?",
            token_path.display()
        )
    })?;
    let token = token.trim();
    if token.is_empty() {
        bail!("daemon token at {} is empty", token_path.display());
    }
    let control_url = format!(
        "http://{}:{}",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("building rules client")?;

    match command {
        RulesCommand::Status { workspace, json } => {
            let mut url = format!("{control_url}/rules");
            if let Some(workspace) = workspace {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(workspace.as_bytes()).collect();
                url.push_str("?workspace=");
                url.push_str(&encoded);
            }
            let response: RulesStatusResponse = decode_response(
                client
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .context("requesting rules status; is the daemon running?")?,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_status(&response);
            }
        }
        RulesCommand::Trust { global, workspace } => {
            let target = match (global, workspace) {
                (true, None) => RulesTrustTarget::Global,
                (false, Some(workspace)) => RulesTrustTarget::Workspace { workspace },
                _ => bail!("choose exactly one of --global or --workspace NAME"),
            };
            let response: RulesTrustResponse = decode_response(
                client
                    .post(format!("{control_url}/rules/trust"))
                    .bearer_auth(token)
                    .json(&RulesTrustRequest { target })
                    .send()
                    .context("trusting rules file; is the daemon running?")?,
            )?;
            println!("{}", response.message);
        }
    }
    Ok(())
}

fn print_status(response: &RulesStatusResponse) {
    println!("SCOPE       STATE     PATH");
    for rule in &response.rules {
        let scope = match &rule.scope {
            RulesFileScope::Global => "global".to_string(),
            RulesFileScope::Workspace { workspace } => format!("workspace:{workspace}"),
        };
        let state = if rule.blocked { "blocked" } else { "trusted" };
        println!("{scope:<11} {state:<9} {}", rule.path);
    }
}

fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T> {
    let status = response.status();
    let body = response.bytes().context("reading daemon response")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<ErrorResponse>(&body) {
            bail!("{}: {}", error.error, error.reason);
        }
        bail!(
            "daemon request failed ({status}): {}",
            String::from_utf8_lossy(&body).trim()
        );
    }
    serde_json::from_slice(&body).context("decoding daemon response")
}

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
#[command(name = "hh", version, about = "Harness Hat — Docker-backed dev sessions")]
struct CliOptions {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Generate a sample config file (defaults to ./harness-hat.toml).
    Init {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Boot a container in the current directory and run a command.
    ///
    /// Template resolution order: --template flag, then [container].template
    /// in harness-rules.toml, then an interactive picker.
    /// Defaults to /bin/bash when no command is given.
    Shell {
        /// Container template to use (dockerfile stem, e.g. "rust", "default").
        #[arg(short, long, value_name = "TEMPLATE")]
        template: Option<String>,
        /// Command and arguments to run inside the container.
        #[arg(value_name = "ARGS", trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Option<Command>,
}

pub fn parse() -> Result<Cli> {
    let raw: Vec<OsString> = std::env::args_os().collect();
    parse_from(raw)
}

pub fn parse_from(raw: Vec<OsString>) -> Result<Cli> {
    const USAGE: &str = "Usage: hh [init [PATH] | shell [--template TEMPLATE] [ARGS...]]";
    if raw.is_empty() {
        bail!("missing argv[0]. {USAGE}");
    }
    let options = match CliOptions::try_parse_from(raw) {
        Ok(options) => options,
        Err(err) => err.exit(),
    };
    Ok(Cli {
        command: options.command,
    })
}

pub fn print_help() {
    use clap::CommandFactory;
    CliOptions::command().print_help().unwrap();
    println!();
}

#[cfg(test)]
mod tests {
    use super::{Command, parse_from};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = parse_from(argv(&["hh"])).expect("parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn init_subcommand_takes_optional_path() {
        let cli = parse_from(argv(&["hh", "init"])).expect("parse");
        assert!(matches!(cli.command, Some(Command::Init { path: None })));

        let cli = parse_from(argv(&["hh", "init", "custom.toml"])).expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Init { path: Some(p) }) if p == PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn shell_subcommand_no_args_defaults_to_bash() {
        let cli = parse_from(argv(&["hh", "shell"])).expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Shell { template: None, args }) if args.is_empty())
        );
    }

    #[test]
    fn shell_subcommand_with_template_flag() {
        let cli = parse_from(argv(&["hh", "shell", "--template", "rust"])).expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Shell { template: Some(t), args }) if t == "rust" && args.is_empty())
        );
    }

    #[test]
    fn shell_subcommand_passes_through_argv() {
        let cli = parse_from(argv(&["hh", "shell", "claude", "--dangerously-skip-permissions"]))
            .expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Shell { template: None, ref args }) if args == &["claude", "--dangerously-skip-permissions"])
        );
    }
}

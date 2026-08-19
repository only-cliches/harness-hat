use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub const COMMAND_NAME: &str = "hat";
pub const HOSTDO_CHILD_ENV: &str = "HARNESS_HAT_HOSTDO_CHILD";

/// Refuse to run the Harness Hat control CLI from a hostdo descendant. The
/// exec endpoint also blocks direct hat argv, while this inherited marker
/// catches ordinary shell/script wrappers before CLI parsing or side effects.
pub fn ensure_not_hostdo_child() -> Result<()> {
    if std::env::var_os(HOSTDO_CHILD_ENV).is_some_and(|value| value == "1") {
        bail!("hat cannot be invoked through hostdo");
    }
    Ok(())
}

#[derive(Debug, Clone, Parser)]
#[command(name = COMMAND_NAME, version, about = "Harness Hat — manager UI and daemon client")]
struct CliOptions {
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands. When none is given, `hat` launches the interactive manager or
/// attaches to an installed background daemon.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Generate a sample config file (defaults to ./harness-hat.toml).
    Init {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Operate on a running session, or launch one with `new --path DIRECTORY`.
    /// With no id, lists sessions. Any args after an id are passed verbatim to
    /// `docker exec`; use `ID open EDITOR` to launch an attached IDE. EDITOR
    /// must be one executable name available on PATH.
    #[command(name = "sh", visible_alias = "shell")]
    Shell {
        #[arg(value_name = "ID")]
        id: Option<String>,
        /// Launch a fresh session for the reserved `new` action.
        #[arg(long, value_name = "DIRECTORY", requires = "id")]
        path: Option<PathBuf>,
        /// Terminate and remove the named session instead of attaching to it.
        #[arg(long, conflicts_with = "args")]
        kill: bool,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<OsString>,
        /// Parsed from the trailing `open EDITOR` action.
        #[arg(skip)]
        open: Option<OpenEditor>,
    },
    /// Attach to (or start) a session for the current working directory.
    ///
    /// If the cwd is inside a configured workspace and a session is already
    /// running for it, attach to the most recent. Otherwise launch a new
    /// session against the running manager. If the cwd does not match any
    /// configured workspace, a new `[[workspaces]]` entry is appended to the
    /// config file using the directory's basename as the workspace name.
    ///
    /// Any args after the subcommand are passed verbatim to `docker exec`
    /// (same passthrough behavior as `hat sh ID …`).
    #[command(name = "ws", visible_alias = "workspace", alias = "wp")]
    Workspace {
        /// List configured workspaces without starting or attaching to a session.
        #[arg(
            long,
            conflicts_with_all = ["template", "name", "rebuild", "new", "args"]
        )]
        list: bool,
        /// Use a specific container template instead of prompting.
        #[arg(long, value_name = "NAME")]
        template: Option<String>,
        /// Jump directly to a named workspace instead of matching by cwd.
        #[arg(long, value_name = "WORKSPACE")]
        name: Option<String>,
        /// Rebuild the container image (and its base) before launching,
        /// bypassing the Docker layer cache. Useful after updating Dockerfiles
        /// or to pick up a newer version of an installed tool (e.g. claude-code).
        #[arg(long)]
        rebuild: bool,
        /// Always launch a fresh session instead of reusing a running one.
        #[arg(long)]
        new: bool,
        /// Open the workspace through Claude Desktop over a loopback-only SSH connection.
        #[arg(long, conflicts_with = "list")]
        desktop: bool,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<OsString>,
        /// Parsed from the trailing `open EDITOR` action.
        #[arg(skip)]
        open: Option<OpenEditor>,
    },
    /// Rebuild the base image followed by selected templates, or every
    /// Dockerfile template in the configured docker_dir when none are named.
    Rebuild {
        /// Disable Docker's layer cache for the base and template builds.
        #[arg(long)]
        no_cache: bool,
        /// Dockerfile template stems to rebuild, for example `go` or `python`.
        #[arg(value_name = "TEMPLATE")]
        templates: Vec<String>,
    },
    /// Reload the daemon configuration and refresh its caches without stopping sessions.
    Restart,
    /// Install Harness Hat as a per-user background agent.
    Install {
        /// On Linux, install without graphical-session dependencies and keep
        /// the user service running across logout via systemd lingering.
        #[arg(long)]
        headless: bool,
    },
    /// List and decide approvals queued by the background daemon.
    Approvals {
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    /// Remove the per-user Harness Hat background agent.
    Uninstall,
    /// Internal: pop a native system dialog and print the result to stdout.
    /// Invoked by the manager as a subprocess so the dialog has its own
    /// main thread / event loop; not intended for direct end-user use.
    #[command(name = "__dialog", hide = true, subcommand)]
    Dialog(DialogCommand),
}

/// An executable name accepted by `sh ID open EDITOR` and `ws open EDITOR`.
///
/// The executable is deliberately kept as one argv token. Harness Hat does
/// not parse shell syntax or accept editor flags here; callers that need
/// custom arguments can put a small wrapper executable on PATH instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenEditor(OsString);

impl OpenEditor {
    pub fn new(binary: OsString) -> Result<Self> {
        anyhow::ensure!(!binary.is_empty(), "editor executable name cannot be empty");
        let display = binary.to_string_lossy();
        anyhow::ensure!(
            !display.contains('/') && !display.contains('\\'),
            "editor must be a single executable name on PATH, not a path"
        );
        Ok(Self(binary))
    }

    /// The executable name to resolve and invoke.
    pub fn binary(&self) -> &OsStr {
        &self.0
    }

    /// A lossy display form suitable for diagnostics.
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum ApprovalCommand {
    /// List pending approvals.
    List {
        /// Emit stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Allow a pending network request or host command.
    Allow {
        #[arg(value_name = "ID")]
        id: String,
        /// Persist the decision in the matching workspace rules.
        #[arg(long)]
        remember: bool,
    },
    /// Deny a pending network request or host command.
    Deny {
        #[arg(value_name = "ID")]
        id: String,
        /// Persist the decision in the matching workspace rules.
        #[arg(long)]
        remember: bool,
    },
    /// Trust the unchanged contents of a blocked rules file.
    Trust {
        #[arg(value_name = "ID")]
        id: String,
    },
}

/// Dialog kinds the `__dialog` subcommand can render. Each variant maps to
/// one concrete native dialog; output is a single machine-readable line on
/// stdout (see `native_approval::Outcome::encode`).
#[derive(Debug, Clone, Subcommand)]
pub enum DialogCommand {
    /// Network-approval prompt: Allow / Deny + a "remember" checkbox.
    NetworkApproval {
        #[arg(long, value_name = "HOST")]
        host: String,
        #[arg(long, value_name = "METHOD", default_value = "")]
        method: String,
        #[arg(long, value_name = "PATH", default_value = "")]
        path: String,
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
        #[arg(long, value_name = "WORKSPACE")]
        workspace: Option<String>,
    },
    /// Host command-approval prompt.
    HostdoApproval {
        #[arg(long, value_name = "COMMAND")]
        command: String,
        #[arg(long, value_name = "REASON")]
        reason: Option<String>,
        #[arg(long, value_name = "CWD")]
        cwd: Option<String>,
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
        #[arg(long = "timeout", value_name = "TIMEOUT_SECS")]
        timeout_secs: Option<u64>,
        #[arg(long, value_name = "WORKSPACE")]
        workspace: Option<String>,
    },
    /// Rules-file tampering prompt. Only explicit trust unblocks decisions.
    RulesChanged {
        #[arg(long, value_name = "PATH")]
        path: PathBuf,
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
    if raw.is_empty() {
        bail!("missing argv[0]");
    }

    // `Error::exit()` prints --help/--version to stdout and exits 0, and prints
    // genuine usage errors to stderr (exit 2) with clap's formatting, rather
    // than surfacing them as an anyhow "Error: ..." message.
    let options = match CliOptions::try_parse_from(raw.clone()) {
        Ok(options) => options,
        Err(err) => err.exit(),
    };
    if raw.get(1).is_some_and(|arg| arg == "wp") {
        eprintln!("warning: `hat wp` is deprecated; use `hat ws` instead");
    }
    let command = normalize_actions(options.command)?;
    Ok(Cli { command })
}

fn normalize_actions(command: Option<Command>) -> Result<Option<Command>> {
    let Some(command) = command else {
        return Ok(None);
    };
    match command {
        Command::Shell {
            id,
            path,
            kill,
            mut args,
            mut open,
        } => {
            if open.is_none() && args.first().is_some_and(|arg| arg == "open") {
                anyhow::ensure!(
                    args.len() == 2,
                    "`open` requires one editor executable name: use `sh ID open EDITOR`"
                );
                open = Some(parse_editor(&args[1])?);
                args.clear();
            }
            if id.as_deref() == Some("new") {
                anyhow::ensure!(path.is_some(), "`sh new` requires `--path DIRECTORY`");
                anyhow::ensure!(
                    !kill && args.is_empty() && open.is_none(),
                    "`sh new --path DIRECTORY` cannot be combined with a command, `--kill`, or `open`"
                );
            } else {
                anyhow::ensure!(
                    path.is_none(),
                    "`--path` is only valid with `sh new --path DIRECTORY`"
                );
            }
            Ok(Some(Command::Shell {
                id,
                path,
                kill,
                args,
                open,
            }))
        }
        Command::Workspace {
            list,
            template,
            name,
            rebuild,
            new,
            desktop,
            mut args,
            mut open,
        } => {
            if open.is_none() && args.first().is_some_and(|arg| arg == "open") {
                anyhow::ensure!(
                    args.len() == 2,
                    "`open` requires one editor executable name: use `ws open EDITOR`"
                );
                open = Some(parse_editor(&args[1])?);
                args.clear();
            }
            anyhow::ensure!(
                !list || (!new && args.is_empty() && open.is_none()),
                "`ws --list` cannot be combined with `--new`, a command, or `open`"
            );
            anyhow::ensure!(
                !desktop || (args.is_empty() && open.is_none()),
                "`ws --desktop` cannot be combined with a command or `open`"
            );
            anyhow::ensure!(
                args.first().is_none_or(|arg| arg != "--path"),
                "`ws` is cwd-based; use `sh new --path DIRECTORY` for an explicit directory"
            );
            Ok(Some(Command::Workspace {
                list,
                template,
                name,
                rebuild,
                new,
                desktop,
                args,
                open,
            }))
        }
        other => Ok(Some(other)),
    }
}

fn parse_editor(value: &OsString) -> Result<OpenEditor> {
    OpenEditor::new(value.clone())
}

#[cfg(test)]
mod tests {
    use super::{ApprovalCommand, Command, DialogCommand, parse_from};
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = parse_from(argv(&["hat"])).expect("parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn init_subcommand_takes_optional_path() {
        let cli = parse_from(argv(&["hat", "init"])).expect("parse");
        assert!(matches!(cli.command, Some(Command::Init { path: None })));

        let cli = parse_from(argv(&["hat", "init", "custom.toml"])).expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Init { path: Some(p) }) if p == PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn shell_subcommand_takes_optional_id() {
        let cli = parse_from(argv(&["hat", "shell"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: None, path: None, kill: false, ref args, open: None }) if args.is_empty()
        ));

        let cli = parse_from(argv(&["hat", "shell", "42"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: Some(id), path: None, kill: false, ref args, open: None }) if id == "42" && args.is_empty()
        ));
    }

    #[test]
    fn shell_open_action_parses_id_and_editor() {
        let cli = parse_from(argv(&["hat", "sh", "42", "open", "codium"])).expect("parse");
        let Some(Command::Shell {
            id: Some(id),
            open: Some(editor),
            args,
            ..
        }) = cli.command
        else {
            panic!("expected shell open action");
        };
        assert_eq!(id, "42");
        assert_eq!(editor.binary(), OsStr::new("codium"));
        assert!(args.is_empty());
    }

    #[test]
    fn nested_open_action_accepts_any_single_executable_name() {
        assert!(parse_from(argv(&["hat", "sh", "42", "open", "notepad"])).is_ok());
        assert!(parse_from(argv(&["hat", "sh", "42", "open", "my-editor"])).is_ok());
    }

    #[test]
    fn nested_open_action_rejects_paths_and_extra_arguments() {
        assert!(parse_from(argv(&["hat", "sh", "42", "open", "./editor"])).is_err());
        assert!(parse_from(argv(&["hat", "sh", "42", "open", "editor", "--wait"])).is_err());
    }

    #[test]
    fn workspace_subcommand_parses_template_and_trailing_args() {
        let cli = parse_from(argv(&["hat", "workspace"])).expect("parse");
        let Some(Command::Workspace {
            template,
            args,
            open: None,
            ..
        }) = cli.command
        else {
            panic!("expected Workspace");
        };
        assert!(template.is_none());
        assert!(args.is_empty());

        let cli = parse_from(argv(&[
            "hat",
            "workspace",
            "--template",
            "dev",
            "claude",
            "--resume",
        ]))
        .expect("parse");
        let Some(Command::Workspace {
            template,
            args,
            open: None,
            ..
        }) = cli.command
        else {
            panic!("expected Workspace");
        };
        assert_eq!(template.as_deref(), Some("dev"));
        assert_eq!(
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["claude".to_string(), "--resume".to_string()],
        );
    }

    #[test]
    fn ws_alias_parses_as_workspace_and_preserves_trailing_args() {
        let cli = parse_from(argv(&["hat", "ws", ".."])).expect("parse");
        let Some(Command::Workspace {
            args, open: None, ..
        }) = cli.command
        else {
            panic!("expected Workspace subcommand");
        };
        assert_eq!(args, vec![OsString::from("..")]);
    }

    #[test]
    fn wp_alias_remains_compatible_and_preserves_trailing_args() {
        let cli = parse_from(argv(&["hat", "wp", ".."])).expect("parse");
        let Some(Command::Workspace {
            args, open: None, ..
        }) = cli.command
        else {
            panic!("expected Workspace subcommand");
        };
        assert_eq!(args, vec![OsString::from("..")]);
    }

    #[test]
    fn workspace_subcommand_parses_list() {
        let cli = parse_from(argv(&["hat", "ws", "--list"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Workspace { list: true, .. })
        ));
    }

    #[test]
    fn rebuild_subcommand_parses_cache_and_template_options() {
        let cli =
            parse_from(argv(&["hat", "rebuild", "--no-cache", "go", "python"])).expect("parse");
        let Some(Command::Rebuild {
            no_cache,
            templates,
        }) = cli.command
        else {
            panic!("expected Rebuild subcommand");
        };
        assert!(no_cache);
        assert_eq!(templates, vec!["go", "python"]);
    }

    #[test]
    fn restart_parses_as_a_top_level_command() {
        assert!(matches!(
            parse_from(argv(&["hat", "restart"])).unwrap().command,
            Some(Command::Restart)
        ));
    }

    #[test]
    fn shell_subcommand_collects_trailing_args_verbatim() {
        let cli = parse_from(argv(&["hat", "shell", "42", "claude", "--resume"])).expect("parse");
        let Some(Command::Shell {
            id,
            path: None,
            kill: false,
            args,
            open: None,
        }) = cli.command
        else {
            panic!("expected Shell subcommand");
        };
        assert_eq!(id.as_deref(), Some("42"));
        assert_eq!(
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["claude".to_string(), "--resume".to_string()],
        );
    }

    #[test]
    fn shell_subcommand_parses_kill_flag() {
        let cli = parse_from(argv(&["hat", "shell", "42", "--kill"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: Some(id), path: None, kill: true, args, open: None }) if id == "42" && args.is_empty()
        ));
    }

    #[test]
    fn shell_new_requires_path_and_preserves_directory() {
        let cli = parse_from(argv(&["hat", "sh", "new", "--path", "."])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell {
                id: Some(id),
                path: Some(path),
                kill: false,
                open: None,
                ref args,
            }) if id == "new" && path == PathBuf::from(".") && args.is_empty()
        ));
        assert!(parse_from(argv(&["hat", "sh", "new"])).is_err());
        assert!(parse_from(argv(&["hat", "sh", "new", "."])).is_err());
        assert!(parse_from(argv(&["hat", "sh", "new", "--path", ".", "echo"])).is_err());
        assert!(parse_from(argv(&["hat", "sh", "new", "--path", ".", "--kill"])).is_err());
    }

    #[test]
    fn workspace_new_and_open_actions_parse() {
        let cli = parse_from(argv(&["hat", "ws", "--new", "open", "vscode"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Workspace {
                new: true,
                open: Some(ref editor),
                ref args,
                ..
            }) if args.is_empty() && editor.binary() == OsStr::new("vscode")
        ));
        assert!(parse_from(argv(&["hat", "ws", "--path", "."])).is_err());
    }

    #[test]
    fn workspace_desktop_action_parses_and_rejects_passthrough_actions() {
        assert!(matches!(
            parse_from(argv(&["hat", "ws", "--desktop"]))
                .unwrap()
                .command,
            Some(Command::Workspace {
                desktop: true,
                ref args,
                open: None,
                ..
            }) if args.is_empty()
        ));
        assert!(parse_from(argv(&["hat", "ws", "--desktop", "claude"])).is_err());
        assert!(parse_from(argv(&["hat", "ws", "--desktop", "open", "code"])).is_err());
    }

    #[test]
    fn rules_changed_dialog_parses_its_file_path() {
        let cli = parse_from(argv(&[
            "hat",
            "__dialog",
            "rules-changed",
            "--path",
            "/tmp/harness-rules.toml",
        ]))
        .expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Dialog(DialogCommand::RulesChanged { path }))
                if path == PathBuf::from("/tmp/harness-rules.toml")
        ));
    }

    #[test]
    fn install_and_uninstall_parse_as_top_level_commands() {
        assert!(matches!(
            parse_from(argv(&["hat", "install"])).unwrap().command,
            Some(Command::Install { headless: false })
        ));
        assert!(matches!(
            parse_from(argv(&["hat", "install", "--headless"]))
                .unwrap()
                .command,
            Some(Command::Install { headless: true })
        ));
        assert!(matches!(
            parse_from(argv(&["hat", "uninstall"])).unwrap().command,
            Some(Command::Uninstall)
        ));
    }

    #[test]
    fn approvals_subcommands_parse() {
        assert!(matches!(
            parse_from(argv(&["hat", "approvals", "list", "--json"]))
                .unwrap()
                .command,
            Some(Command::Approvals {
                command: ApprovalCommand::List { json: true }
            })
        ));
        assert!(matches!(
            parse_from(argv(&["hat", "approvals", "allow", "42", "--remember"]))
                .unwrap()
                .command,
            Some(Command::Approvals {
                command: ApprovalCommand::Allow { id, remember: true }
            }) if id == "42"
        ));
        assert!(matches!(
            parse_from(argv(&["hat", "approvals", "deny", "0042"]))
                .unwrap()
                .command,
            Some(Command::Approvals {
                command: ApprovalCommand::Deny { id, remember: false }
            }) if id == "0042"
        ));
        assert!(matches!(
            parse_from(argv(&["hat", "approvals", "trust", "7"]))
                .unwrap()
                .command,
            Some(Command::Approvals {
                command: ApprovalCommand::Trust { id }
            }) if id == "7"
        ));
    }
}

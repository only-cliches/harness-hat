# Set Up Claude Code

[Previous: Configuration](03-configuration.md) | [Guide index](README.md) | [Next: Hostdo](05-hostdo.md)

Each session starts with fresh container state. Authenticate Claude through an environment variable so new sessions can start without an interactive browser login.

## Install Claude Code On The Local Machine

Complete [Step 4: Install Claude Code Locally](01-setup.md#step-4-install-claude-code-locally) before choosing an authentication method. The local `claude` CLI is required to create a setup token; it is separate from the Claude Code already included in Harness Hat sessions.

## Choose An Authentication Method

Choose exactly one method for Harness Hat sessions:

- **Recommended: `CLAUDE_CODE_OAUTH_TOKEN`.** Create it with `claude setup-token`; it stays tied to the user's Claude account and is the standard path for this guide.
- **Alternative: `ANTHROPIC_API_KEY`.** Use this when the developer or organization provides an Anthropic API key instead.

The default configuration created by `hat install` passes either variable into new sessions. Set only one.

## Recommended: Claude Setup Token

Create an OAuth token in a **terminal**:

```sh
claude setup-token
export CLAUDE_CODE_OAUTH_TOKEN="<token printed by Claude>"
```

> **Expected result:** `claude setup-token` prints a token to export, and the `export` command is silent. Start a new session after setting it.


## Alternative: API Key

Create an Anthropic API key, then export it in the current **terminal** for a temporary session:

```sh
export ANTHROPIC_API_KEY="sk-ant-api03-..."
```

> **Expected result:** `export` is silent. Start a new session after setting it; do not paste the key into a project file or commit it.

## Keep Authentication After Restarting The Terminal

Save only the method you chose in the startup profile for the **terminal** you use to run `hat`. Use `CLAUDE_CODE_OAUTH_TOKEN` unless you deliberately chose the API-key alternative.

First, identify the shell used by the current **terminal**:

```sh
printf '%s\n' "$SHELL"
```

> **Expected result:** the command prints a shell path such as `/bin/zsh` or `/bin/bash`. Modify only one existing startup file for that shell. Do not add the export to several profile files.

On macOS, prefer the existing `~/.zshrc` when the shell is zsh; zsh is the macOS default. For Bash, use the existing `~/.bash_profile` on macOS login shells or `~/.bashrc` on most Linux interactive shells. Use `~/.zprofile` only when it is the existing zsh profile your terminal loads.

### macOS Or Linux: zsh

When `printf '%s\n' "$SHELL"` reports zsh, open the existing zsh startup file in a **terminal**:

```sh
nano ~/.zshrc
```

If `~/.zprofile` is the existing zsh startup file your terminal uses instead, run `nano ~/.zprofile` and make the change there instead. Do not modify both files.

Add one of these lines, replacing the placeholder with the value you created above:

```sh
export CLAUDE_CODE_OAUTH_TOKEN="<token printed by Claude>"
# Alternative:
export ANTHROPIC_API_KEY="sk-ant-api03-..."
```

> **Expected result:** nano opens the selected zsh startup file. Save the file, exit nano, then open a new terminal. In the new terminal, `printenv ANTHROPIC_API_KEY` or `printenv CLAUDE_CODE_OAUTH_TOKEN` prints the saved value.

### macOS Or Linux: Bash

When `printf '%s\n' "$SHELL"` reports Bash, open the existing Bash startup file used by your system in a **terminal**:

```sh
# macOS login shells:
nano ~/.bash_profile

# Most Linux interactive shells:
nano ~/.bashrc
```

Add one of the same `export` lines shown for zsh, save the file, and open a new terminal.

> **Expected result:** the selected profile opens in nano. In the new terminal, `printenv ANTHROPIC_API_KEY` or `printenv CLAUDE_CODE_OAUTH_TOKEN` prints the saved value.

### Windows: PowerShell

Run one of these commands in PowerShell, replacing the placeholder with the value you created above:

```powershell
[Environment]::SetEnvironmentVariable("CLAUDE_CODE_OAUTH_TOKEN", "<token printed by Claude>", "User")
# Alternative:
[Environment]::SetEnvironmentVariable("ANTHROPIC_API_KEY", "sk-ant-api03-...", "User")
```

> **Expected result:** PowerShell returns without output. Close and reopen the terminal, then run `Get-ChildItem Env:ANTHROPIC_API_KEY` or `Get-ChildItem Env:CLAUDE_CODE_OAUTH_TOKEN` to confirm the value is available.

## Start Claude in a session

Run this in a **terminal** to enter the session:

```sh
cd ~/my-awesome-project
hat ws claude
# Or resume a Claude conversation:
hat ws claude --resume
```

> **Expected result:** Harness Hat opens the session and starts Claude. In the session, `/status` should show an authenticated Claude session rather than requesting a browser login.

Inside the **session terminal**, use `/status` to confirm the active authentication method. Before asking Claude to use host-side tools, read [Use hostdo with an agent](05-hostdo.md).

## Use Claude Desktop With A Hat Container

Claude Desktop can use a Harness Hat container as an SSH environment while the
native application remains on the host. Install Claude Desktop and the OpenSSH
client, then rebuild the Hat images once to include the container SSH service:

```sh
hat rebuild --no-cache
cd ~/my-awesome-project
hat ws --desktop
```

Harness Hat creates or reuses a Desktop-enabled session for the current
workspace, publishes its SSH service on a random host-loopback port, registers
the pinned host key in Hat-owned state, and opens Claude Desktop on macOS or
Windows. Hat adds one stable `Include` line to `~/.ssh/config`; changing ports
and keys are replaced in Hat's private per-workspace SSH files, so repeated
launches do not grow the user configuration. Hat never edits
`~/.claude/settings.json`.

The first time a workspace is used, finish the connection in Claude Desktop:

1. Open **Code** and start a new Code session.
2. Click **Local**, open **SSH**, then choose **Add SSH host…**.
3. Use the friendly name shown by Harness Hat and enter its
   `hat-<workspace>-<id>` value in **SSH Host**. Leave **SSH Port** and
   **Identity File** blank; the Hat-owned SSH configuration supplies them.
4. Add and connect to the SSH host, then select the remote project directory
   shown by Harness Hat.

Claude remembers that selection; later launches update the same SSH alias. The
Harness Hat launcher keeps these instructions, the exact host name, and the
folder path visible after it opens Claude. Its session status changes from
**Waiting for SSH** to **SSH connected** when the connection succeeds.

On macOS or Windows, the release also includes a graphical launcher. Open
**Harness Hat.app** on macOS, or extract the Windows ZIP and double-click
**hat-launcher.exe**. No terminal setup is required. The launcher checks for
Docker Desktop, OpenSSH, and Claude Desktop; creates Hat's default configuration;
and installs or repairs its per-user background service. Missing prerequisites
are shown with a direct download or setup link. Choose the project folder and
Hat starts the protected session and opens Claude Desktop. A saved workspace environment is selected by default;
for a new project Hat suggests one from markers such as `go.mod`, `Cargo.toml`,
`package.json`, or `pyproject.toml`. The dropdown always allows a different
choice. This is the same backend operation as `hat ws --desktop`; the user does
not need to open a terminal, change directories, configure SSH, run `hat init`,
or install the daemon manually. Docker images are built on demand for the
selected environment; images created before Desktop SSH support are detected
and rebuilt automatically.

**Start protected session** prepares Docker and SSH; the first launch may take
several minutes while Docker builds the environment. **Open Claude Desktop**
above the running-session list only opens Claude and does not start another
container.

The launcher also lists every running Hat session, including its project
folder, environment, session number, and current SSH connection state.
Desktop-enabled sessions show their SSH alias and loopback endpoint and offer
**Help**, while every session offers **Stop**. Dismiss the connection guide
with its **×** when you are done. The background
manager checks
SSH state even when the launcher window is closed: after a session has been
used, it is stopped if Claude remains disconnected for 10 minutes. A newly
created session that never receives its first SSH connection is stopped after
30 minutes. Reconnecting during either grace period cancels cleanup.

The launcher follows the operating system's light or dark appearance and
updates automatically when the system appearance changes.

Desktop-enabled sessions receive a read-only managed policy that disables
external Browser-pane navigation, Claude in Chrome, Claude.ai connectors, and
computer-use tools. Localhost app previews remain available. SSH uses a
Hat-specific key, does not forward the host SSH agent, and is not exposed beyond
`127.0.0.1`.

> **Security boundary:** only the named Hat SSH session is container-backed.
> Claude Desktop still lets a person create separate Local, Chat, or Cowork
> sessions, which are outside Harness Hat. Anthropic does not currently expose
> a per-connection policy that removes those choices from the application.

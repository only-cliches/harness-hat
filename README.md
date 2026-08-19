# 🎩 Harness Hat

[![Crates.io](https://img.shields.io/crates/v/harness-hat.svg)](https://crates.io/crates/harness-hat)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#requirements)

**Freedom inside. Approval at the edges.**

Harness Hat is a local control plane for coding agents. It launches each project in a reusable Docker workspace, starts your agent inside it, and mediates the two ways that agent reaches beyond the container:

* **Network access** goes through project policy with allow, deny, and approval decisions.
* **Host execution** goes through `hostdo`, an exact-command gateway with workspace confinement, timeouts, approvals, and audit history.

Inside the workspace, the agent gets a real shell, a real toolchain, and read-write access to the project. Outside the workspace, the boundaries stay explicit and reviewable.

```text
┌────────────────────────────── your workstation ──────────────────────────────┐
│                                                                              │
│  project files (read-write)                                                  │
│           │                                                                  │
│           ▼                                                                  │
│  ┌────────────────────────────────────────────────────────────────────────┐  │
│  │ Harness Hat workspace                                                  │  │
│  │ Docker + project toolchain + Codex / Claude / Antigravity / Pi         │  │
│  └───────────────────────────┬───────────────────────┬────────────────────┘  │
│                              │                       │                       │
│                     network policy                 hostdo                    │
│                     allow / deny / ask             exact argv / ask          │
│                              │                       │                       │
│                              ▼                       ▼                       │
│                           Internet              host command                 │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Quick start

Prerequisites: [Docker](https://docs.docker.com/get-docker/) is running and Rust/Cargo is installed.

### 1. Install

```sh
cargo install harness-hat
hat install
```

### 2. CD to your project

```sh
cd ~/src/my-awesome-project
```

### 3. Run Codex

```sh
hat ws codex
```

### 4. Win.

Codex is now running inside a reusable, policy-controlled development workspace.

On the first launch, Harness Hat:

1. registers the current directory as a workspace
2. creates `harness-rules.toml`
3. asks you to choose a container template
4. builds the image if it is missing
5. remembers the template and launches Codex

After that, the workflow stays gloriously boring:

```sh
hat ws codex
```

If a session is already running, Harness Hat attaches to it. Otherwise, it starts one.

## Why Harness Hat exists

Coding agents are most useful when they can edit files, run commands, install dependencies, and iterate without stopping for permission every few seconds. Running the same agent directly on a workstation gives it whatever access happens to be available to your user account.

Harness Hat gives the agent a large area where it can work freely, then places narrow gateways around the places where that environment touches the rest of the machine.

The result is a consistent outer policy layer that works across agents:

| Inside the workspace                                    | Crossing the boundary                        |
| ------------------------------------------------------- | -------------------------------------------- |
| Edit project files                                      | Network requests follow project rules        |
| Run compilers, tests, and package managers              | Unknown destinations can require approval    |
| Install tools inside the container                      | Host commands must use `hostdo`              |
| Use Codex, Claude Code, Antigravity, Pi, or another CLI | Remembered decisions become reviewable rules |

Agent-specific permission prompts can still be used. Harness Hat also includes convenience wrappers such as `codex-yolo`, `claude-yolo`, `agy-yolo`, and `omp-yolo` when you want Harness Hat to serve as the outer approval layer.

## Project-scoped policy

Each workspace gets a `harness-rules.toml` file. Commit it with the project so the environment's expected capabilities can be reviewed like code.

A small Rust project policy might look like this:

```toml
version = 1
template = "rust"
mirror_cwd = true

[network]
allowlist = [
  "domain=github.com",
  "domain=api.github.com",
  "domain=crates.io",
  "domain=static.crates.io",
  "domain=index.crates.io",
]
denylist = []

[hostdo]
default_policy = "prompt"

[[hostdo.commands]]
argv = ["cargo", "test"]
timeout_secs = 120
approval_mode = "auto"
reason = "Run the project test suite on the host"
env_allowlist = ["CI", "CARGO_TERM_COLOR"]
```

Rules from the global policy and workspace policy are composed at request time. Deny rules take precedence over allows. Unknown network requests prompt by default, while unknown `hostdo` commands follow the configured `default_policy`.

When you choose to remember an approval or denial, Harness Hat writes a narrow rule to the correct workspace file. If a policy file changes outside that trusted write path, new network and `hostdo` decisions stay blocked until the current file is reviewed and trusted.

## Network control without TLS interception

Harness Hat uses an authenticated, per-session HTTP/CONNECT proxy.

* Plain HTTP can be evaluated by method, host, path, and port.
* HTTPS and other TCP connections are evaluated by destination host and port.
* TLS remains end-to-end. Harness Hat does not install a private CA or decrypt HTTPS traffic.
* Explicit deny rules win over allow rules.
* Restricted private and local destinations stay blocked unless exposed through a configured localhost forward.

### Standard mode

Harness Hat supplies the normal proxy environment variables to the container. This works with tools that honor `HTTP_PROXY`, `HTTPS_PROXY`, or `ALL_PROXY`.

### Strict mode

When bypass resistance matters, enable routing-layer enforcement:

```toml
[defaults.proxy]
strict_network = true
```

Strict mode uses a TUN interface and firewall rules to route outbound TCP through Harness Hat even when an application ignores proxy environment variables. Direct egress is blocked, while Docker DNS, the Harness Hat control plane, and explicitly configured localhost forwards remain available. UDP and QUIC are intentionally blocked.

On Docker Desktop, strict mode requires privileged container setup so `/dev/net/tun` is available. On native Linux, Harness Hat uses a smaller capability set for network setup and then drops to the unprivileged `coder` user.

## Controlled host execution with `hostdo`

The supported path from a workspace to host-side execution is `hostdo`.

Run these commands from inside a Harness Hat session:

```sh
# Run synchronously and return the command's output and exit status.
hostdo output cargo test

# Explain why the command is needed in the approval UI and saved rule context.
hostdo output --reason "verify the release build" cargo build --release

# Start a background job and inspect it later.
job_id=$(hostdo run cargo test)
hostdo status "$job_id"
hostdo tail "$job_id" --all
hostdo stop "$job_id"

# Run in a separate short-lived Docker runner instead of directly on the host.
hostdo output --image node:20 npm test
```

`hostdo` is deliberately narrower than a generic host shell:

* policy matching is exact on `argv + image`
* the requested working directory must resolve inside the workspace
* a request can lower its timeout, but it cannot exceed the matching rule's ceiling or the hard five-minute maximum
* Harness Hat control tokens are stripped from the child environment
* rules can clear the inherited environment and allow only named variables
* approved, denied, timed-out, and completed commands are written to local audit history
* background jobs support output capture, status, stdin, cancellation, and timeouts

Use normal shell commands inside the container whenever possible. `hostdo` is for the small set of build, test, package, compiler, or workstation operations that genuinely need the host.

## Agent-ready images

Every bundled language image extends the same Harness Hat base, so agent CLIs and boundary tooling are available across environments.

The base currently includes:

* OpenAI Codex
* Claude Code
* Google Antigravity CLI
* Pi coding agent
* oh-my-pi
* `hostdo` and `killme`
* Git, Node.js, Python, ripgrep, ast-grep, zsh, and common development utilities

Built-in Dockerfile templates cover:

| Template     | Environment                                                          |
| ------------ | -------------------------------------------------------------------- |
| `default`    | Node.js, npm, Bun, pnpm, and TypeScript                              |
| `typescript` | TypeScript, Node.js, Bun, Vite, ESLint, Prettier, and nodemon        |
| `python`     | Python and `uv`                                                      |
| `rust`       | Rust, Cargo, rust-analyzer, Clippy, nextest, audit, and deny tooling |
| `go`         | Go, gopls, Delve, staticcheck, and golangci-lint                     |
| `kotlin`     | JDK, Kotlin, and Gradle                                              |
| `android`    | JDK, Kotlin, Gradle, and Android command-line tooling                |
| `csharp`     | .NET SDKs                                                            |
| `php`        | PHP, Composer, PHPUnit, PHPStan, and related tooling                 |

Select one directly:

```sh
hat ws --template rust codex
```

Harness Hat remembers the selection in the workspace policy.

### Bring your own image

Add any file ending in `.dockerfile` beneath the workspace and start it from the Harness Hat base:

```dockerfile
# project-tools.dockerfile
FROM harness-hat-base:local

USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends postgresql-client \
    && rm -rf /var/lib/apt/lists/*

USER coder
```

Harness Hat discovers compatible workspace-local Dockerfiles and adds them to the template picker.

## Reusable sessions and persistent agent state

`hat install` creates a per-user background agent that starts with the graphical desktop session. On a headless Linux host, use `hat install --headless`; it installs the same `systemd --user` service without graphical dependencies and enables lingering so it can run across logout and at boot.

Headless approvals can be managed over SSH with four-digit IDs:

```sh
hat approvals list
hat approvals list --json
hat approvals allow 42
hat approvals deny 0042 --remember
hat approvals trust 17
```

An attached `hat` TUI can also decide queued approvals. Unknown requests remain fail-closed while waiting, and changed rules files require the explicit `trust` command rather than allow/deny.

The CLI becomes a lightweight client:

```sh
hat                    # open the manager TUI
hat ws                 # start or attach to the workspace for the current directory
hat ws codex           # run Codex in that workspace
hat ws claude --resume # resume Claude in that workspace
hat ws --desktop       # open a container-backed Claude Desktop environment
hat ws --new            # force a fresh session for the current directory
hat ws open codium      # open the current workspace session in a PATH editor
hat sh                 # list active sessions
hat sh 42              # attach to a session
hat sh 42 --kill       # stop and remove a session
hat sh 42 open codium  # open the session in a PATH editor
hat sh new --path .    # launch a fresh session and print its integer ID
hat rebuild rust       # rebuild the base and Rust images
hat restart            # reload config and policy without stopping sessions
```

macOS and Windows releases also include a graphical Harness Hat launcher for
people who do not use a terminal. On macOS, open **Harness Hat.app**. On
Windows, extract the release ZIP and double-click **hat-launcher.exe**. The
launcher checks Docker Desktop, OpenSSH, and Claude Desktop, creates the default
configuration, and installs or repairs its per-user background service. Choose
a project folder, confirm the saved or automatically suggested development
environment, then either start the protected session by itself or open Claude
Desktop too. It performs the same protected launch as `hat ws --desktop` and
builds only the selected image when it is missing or too old for Desktop SSH.
After opening Claude, the launcher displays the exact first-time steps: open
**Code**, click **Local**, open **SSH**, choose **Add SSH host…**, and enter the
shown `hat-<workspace>-<id>` alias. Port and identity stay blank because Hat's
SSH configuration supplies them. The launcher also shows the direct loopback
endpoint and remote project path. The session status turns green when SSH is
connected.
The launcher also shows running protected sessions, their SSH connection
state, and a Stop control. Disconnected Desktop sessions are automatically
cleaned up after a reconnect grace period. The terminal manager and TUI remain
available unchanged.

Harness Hat also reuses supported agent state where possible, while avoiding broad home-directory mounts. Some state is bind-mounted, some is seeded into a private session copy, and Codex state on Windows is copied into container-local storage to avoid unsafe SQLite sharing through Docker Desktop.

## Use your editor

Harness Hat owns the container. VS Code, Windsurf, and compatible VS Code-based editors can join it through **Dev Containers: Attach to Running Container**.

```sh
cd ~/src/my-awesome-project
hat ws
```

Then attach the editor to the running Harness Hat container. Integrated terminals, extensions, language servers, debuggers, and agents run inside the same policy-controlled environment.

See [Use VS Code-Based Editors](User%20Guide/07-vscode-editors.md) for the full workflow.

## Know the boundary

Harness Hat is intentionally explicit about what the agent can still affect:

* **The workspace is read-write.** The agent can modify or delete project files, including Git metadata. Commit important work and review changes.
* **Selected agent state may be shared.** Harness Hat reuses supported authentication, configuration, plugin, and conversation state when those paths exist.
* **Approved host commands run with your desktop-user permissions.** Review the command, working directory, image, reason, and timeout before approving it.
* **Standard network mode depends on proxy-aware software.** Enable strict mode when routing-layer enforcement is required.
* **HTTPS contents remain private from Harness Hat.** Policy can see the CONNECT destination and port, not the encrypted request path or body.
* **Strict mode has a privilege tradeoff on Docker Desktop.** TUN setup requires a privileged container there.

Harness Hat rejects obviously dangerous workspace and mount sources, including broad system paths, common credential directories, and Docker sockets. The control server and proxy bind to loopback by default, and policy decisions fail closed when their source files require review.

## Requirements

* macOS, Linux with systemd user services, or Windows
* Docker Engine or Docker Desktop
* Linux containers on Windows
* Rust 1.89 or newer and Cargo for installation from crates.io
* a signed-in graphical desktop user for the default `hat install`, or a Linux user with systemd user services and `loginctl` for `hat install --headless`

Run Harness Hat as your normal user. Do not use `sudo` for `hat install` or `hat uninstall`.

## Documentation

The full guide goes deeper into setup, authentication, policy, operations, and editor integration:

1. [Set up Harness Hat](User%20Guide/01-setup.md)
2. [Create and use workspaces](User%20Guide/02-workspaces.md)
3. [Configure the manager and policy](User%20Guide/03-configuration.md)
4. [Set up Claude Code](User%20Guide/04-claude.md)
5. [Use `hostdo` with an agent](User%20Guide/05-hostdo.md)
6. [Operate and troubleshoot sessions](User%20Guide/06-operations.md)
7. [Use VS Code-based editors](User%20Guide/07-vscode-editors.md)

## License

Harness Hat is available under the [MIT License](LICENSE).

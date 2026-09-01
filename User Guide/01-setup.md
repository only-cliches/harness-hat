# Set Up Harness Hat

[Guide index](README.md) | [Next: Workspaces](02-workspaces.md)

## Automated installation (recommended)

The unattended installers provision Docker when it is missing, install the
appropriate Harness Hat release for the host CPU, add the binary directory to
the signed-in user's PATH, and register the per-user background agent. They
target macOS ARM64/AMD64, Linux ARM64/AMD64, and Windows AMD64.

Run the macOS or Linux command from the one signed-in user who will use Harness
Hat. The script requests elevation only for Docker setup. It preserves an
existing Harness Hat installation unless `--force` is supplied; use
`--version vX.Y.Z` to select a release instead of the latest.

```sh
# macOS
curl -fsSL https://raw.githubusercontent.com/only-cliches/harness-hat/main/scripts/install-macos.sh | bash

# Linux desktop
curl -fsSL https://raw.githubusercontent.com/only-cliches/harness-hat/main/scripts/install-linux.sh | bash

# Linux server: install the systemd user service without desktop dialogs
curl -fsSL https://raw.githubusercontent.com/only-cliches/harness-hat/main/scripts/install-linux.sh | bash -s -- --headless
```

On Windows, save the installer to a file so it can relaunch itself with the
UAC elevation required for WSL 2 and Docker Desktop:

```powershell
$installer = Join-Path $env:TEMP 'install-harness-hat.ps1'
Invoke-WebRequest https://raw.githubusercontent.com/only-cliches/harness-hat/main/scripts/install-windows.ps1 -OutFile $installer
& $installer
```

Windows uses `-Version vX.Y.Z` and `-Force` instead of the shell flags. Docker
may require a reboot, logout, or extra startup time; the installer still
registers Harness Hat, which can be used once `docker version` succeeds.

The remainder of this page documents the manual Cargo-based installation path.

## Step 1: Install Docker

Run the following commands in a **terminal**.

Install a current Docker engine or Docker Desktop for your host:

- [macOS: Docker Desktop](https://docs.docker.com/desktop/setup/install/mac-install/)
- [Linux: Docker Engine](https://docs.docker.com/engine/install/)
- [Windows: Docker Desktop with WSL2](https://docs.docker.com/desktop/setup/install/windows-install/)

On Windows, switch Docker Desktop to **Linux containers**. Harness Hat does not support Windows containers.

Start Docker, then verify that the same user who will run `hat` can use it:

Run the following commands in a **terminal**.

```sh
docker version
docker info
```

> **Expected result:** both commands print Docker client/server version and daemon details. They must not print `command not found`. If either command fails, restart the terminal and try again. If Docker still cannot start or the command remains unavailable, restart Docker Desktop or the Docker daemon; restart the machine as the last local recovery step.

On Linux, follow Docker's [post-install steps](https://docs.docker.com/engine/install/linux-postinstall/) if `docker version` requires `sudo` or cannot reach the daemon. Do not run Harness Hat as root to work around Docker permissions.

## Step 2: Install Rust

Run the following commands in a **terminal**.

Install Rust with the official [rustup instructions](https://www.rust-lang.org/tools/install), then open a new shell so Cargo is on `PATH`:

```sh
rustc --version
cargo --version
```

> **Expected result:** each command prints a version number. If either is not found, open a new terminal after installing Rust so rustup's Cargo bin directory is added to `PATH`.

## Step 3: Install Harness Hat

In a **terminal**, install Harness Hat with Cargo:

```sh
cargo install harness-hat
hat --version
```

> **Expected result:** Cargo finishes without an error and `hat --version` prints a Harness Hat version. If `hat` is not found, open a new terminal and run `hat --version` again before retrying the install.


## Step 4: Install Claude Code Locally

Claude Code must be installed on the development machine before you set up its session authentication. Harness Hat includes Claude Code in its session image, but `claude setup-token` runs from the local CLI.

In a **terminal**, install Node.js 18 or newer using the [official Node.js download](https://nodejs.org/en/download), then install Claude Code using Anthropic's [official setup instructions](https://docs.anthropic.com/en/docs/claude-code/getting-started):

```sh
node --version
npm --version
npm install -g @anthropic-ai/claude-code
claude --version
```

> **Expected result:** `node --version`, `npm --version`, and `claude --version` each print a version number; npm installs Claude Code without an error. Do not use `sudo` with the npm command. If `npm` or `claude` is not found, open a new terminal and try the version command again before reinstalling.

## Step 5: Create The Default Config And Install The Agent

Run these installation commands in a **terminal**.

This guide uses one global configuration file for every developer:

```text
~/.config/harness-hat/harness-hat.toml
```

Run `hat install` as your normal signed-in desktop user. Do not prefix it with `sudo`: the agent is per-user so it can access your Docker Desktop session and display approval dialogs. On its first run, it creates that default global config and its Docker assets, then installs the required per-user graphical background agent:

```sh
hat install
```

> **Expected result:** the first run reports that it created the default global config at `~/.config/harness-hat/harness-hat.toml`, then reports that the background agent was installed for the current desktop user. Later runs keep the existing config and reinstall the agent. If installation fails, first confirm `docker version` and `hat --version` work in the same terminal; then restart the terminal and retry.

### Headless Linux hosts

For a Linux machine reached through SSH with no graphical session, install the per-user service with:

```sh
hat install --headless
```

Run this as the normal Docker-enabled user, not with `sudo`. Harness Hat uses `loginctl enable-linger` for that user, allowing the `systemd --user` service to start at boot and remain active after logout. If lingering cannot be enabled, installation stops with an error instead of installing a service that silently disappears after the SSH session ends. `hat uninstall` does not disable lingering because other user services may depend on it.

Headless installs never attempt to display native dialogs. Use `hat approvals` over SSH or attach the normal `hat` TUI to handle queued requests.


Continue with [Workspaces](02-workspaces.md).

# Operations And Troubleshooting

[Previous: Hostdo](05-hostdo.md) | [Guide index](README.md) | [Back to setup](01-setup.md)

## Everyday commands

Run these commands in a **terminal**:

```sh
hat                              # attach to the installed hat-daemon TUI
hat restart                      # refresh daemon config/caches without stopping sessions
hat ws                           # attach or launch a session for the current directory
hat ws --name my-project         # choose a configured workspace
hat ws --list                    # list configured workspaces
hat ws --new                     # force a fresh session for the current directory
hat ws open codium               # open the current workspace session in a PATH editor
hat sh                           # list active sessions
hat sh <ID>                      # attach to a session
hat sh <ID> --kill-connections   # drop the session's currently-open network connections
hat sh <ID> --kill               # stop and remove a session
hat sh <ID> open codium          # open the session in a PATH editor
hat sh new --path .              # launch a fresh session and print its ID
hat rebuild rust                  # rebuild the base image and Rust template
```

> **Expected result:** `hat` opens the Harness Hat TUI attached to the installed `hat-daemon`; `ws` attaches to a session or starts one, `ws --list` prints each configured workspace name, path, and saved template, `sh` lists or attaches to existing sessions with both its Harness Hat session ID and Docker container ID, and `rebuild` prints Docker build output followed by a successful build result.

The `open` commands require a compatible editor with its Dev Containers
integration installed and enabled. See [Use VS Code-Based Editors](07-vscode-editors.md)
for setup and attachment instructions.

### Global sessions: `hat sh` and `hat ws`

Sessions are global objects managed by Harness Hat, not shells tied to the directory where you started the command. Every running session appears in the TUI, where you can inspect its terminal, activity, requests, builds, and approvals. You can also list and operate on the same sessions with `hat sh`.

`hat sh` is the ID-based entry point: run it from anywhere on your disk to list sessions, attach by ID, run a command, open an IDE, or stop a session. It does not affect which workspace is mounted in a container.

`hat ws` is the directory-aware entry point. It uses the directory you run it in to select or create a workspace, then performs the same attach, command, IDE, and launch behavior against the matching session. From a subdirectory of an existing workspace, it enters that same relative directory inside the container; this applies to both an existing session and a newly launched session. `hat ws --name` from outside the named workspace starts at its mount root.

In short: use `hat sh <ID>` when you know the global session ID, and use `hat ws` when the current directory should choose the workspace. They operate on the same sessions.

The attached TUI is rendered by `hat-daemon`, so its workspace, session, terminal, build, settings, and approval behavior is the same as the standalone manager. When the service is not installed or running, `hat` starts the standalone manager instead.

Run `killme` in a **session terminal** to request that Harness Hat stops that session. From a **terminal**, `hat sh <ID> --kill` stops and removes a session listed by `hat sh`. Use `hat sh <ID> --kill-connections` to close all network connections currently passing through that session's authenticated proxy without stopping the container. This is a transient cut: later connections can open again under the normal network policy. In the TUI, open a session or its Network list and press `x`.

## Refresh the daemon without losing sessions

Run `hat restart` after changing the primary `harness-hat.toml` or policy-related configuration when you want the running daemon to pick it up. It validates the file first, then refreshes configuration, workspace/sidebar state, rules-watch state, and reusable proxy/DNS clients. Running containers, terminal PTYs, approvals, builds, control listeners, and the daemon token stay intact. If validation fails, the active configuration remains in use.

This is intentionally not a service or binary restart. Restarting the background task would terminate the PTY-owned `docker run --rm` sessions.

## Remembered Templates

When `hat ws` asks you to choose a container template, it saves that choice as `template = "..."` in the workspace root's `harness-rules.toml`. The next launch uses that workspace-local choice without showing the picker. `hat ws --list` reports the remembered template for every workspace.

An optional `template` field in the matching `[[workspaces]]` entry of the primary `harness-hat.toml` overrides the workspace-local choice. Use that only when a shared primary config needs to enforce a template for a workspace:

```toml
[[workspaces]]
name = "my-project"
canonical_path = "~/src/my-project"
template = "rust"
```

Existing `[[workspaces]].template` values continue to work as primary-config overrides. Remove one to return control to the workspace-local remembered choice.

The precedence order is `hat ws --template`, the primary-config override, then the workspace's `harness-rules.toml` value.

## Run A Command In A Session

Both `hat ws` and `hat sh` accept a command after their normal arguments. Harness Hat runs that command directly inside the selected container instead of opening an interactive shell.

Run these commands in a **terminal**:

```sh
cd ~/my-awesome-project
# Use the workspace for the current directory.
hat ws claude-yolo

# Use an already-running session by its ID.
hat sh <ID> claude-yolo
```

> **Expected result:** Harness Hat starts or attaches to the selected session, runs `claude-yolo` in the container, and returns Claude's exit status to the terminal.

`claude-yolo` starts Claude with Claude's own permission prompts disabled. Since the container is providing the security layer, you can safely run Claude in `--dangerously-skip-permissions` mode.

## Network approvals

Unknown outbound hosts are policy checked. A project rule can allow or deny a host, method, path, or port. Prefer exact rules over broad wildcards, and review every remembered permission in version control.

While an approval for a domain and port is pending, repeated requests from the same session to that domain and port are folded into the existing approval. Different paths or HTTP methods therefore do not create repeated modals; the eventual decision is delivered to every folded request. Requests from another session or to another port remain separate.

If a rules-file change alert appears, inspect the changed global or project `harness-rules.toml`. New network and `hostdo` decisions stay blocked until the version shown by the alert is trusted. Closing the dialog remains blocked.

If a reviewed file appears to remain blocked, inspect the daemon's effective
guard state and explicitly trust the current reviewed file. This does not
approve requests or bypass policy; a later edit blocks it again.

```sh
hat rules status
hat rules trust --workspace my-project
# or, for the global policy file:
hat rules trust --global
```

In the TUI, open a workspace's Settings pane and use **Inspect rules status**,
**Trust workspace rules**, or **Trust global rules**.

## Inspect Active Requests

Run `hat` in a **terminal** to open the Harness Hat TUI. The sidebar shows active work for each session, including network requests and `hostdo` commands. Select an activity to inspect its current status, command or destination, output, and any approval or failure details while it is still running.

Network requests and `hostdo` commands remain visible as activity items until they complete. Use this view to confirm what an agent is waiting on before approving a request or investigating a command that is taking longer than expected.

## Rebuild after upgrades or Dockerfile changes

Running containers keep their existing image. Rebuild before launching a new session:

Run these commands in a **terminal**:

```sh
hat rebuild
hat rebuild --no-cache python
hat rebuild --all
```

> **Expected result:** Docker prints build progress for the base image and previously built templates. Use `--all` to include every configured template, or name a template to rebuild it explicitly. A new session launched after a successful build uses the rebuilt image; existing sessions do not change.

Use `hat ws --rebuild` for a one-off cache-bypassing rebuild before launch.

Workspace `*.dockerfiles` are scanned for launchable images. Any Dockerfile that starts with `FROM harness-hat-base:local` is added to the launch list alongside the preconfigured templates.

## Common failures

- **Manager is not reachable:** verify the background agent is installed with `hat install` (or `hat install --headless` on a Linux server), then verify the configured control host and port are loopback values.
- **Docker is unavailable:** start Docker Desktop or the Docker daemon, then confirm `docker version` works in the same user session.
- **A project is not found:** run `hat ws` from its directory to add it, or create it from **New Workspace...** in the TUI.
- **A request remains blocked:** check global and project rules, then inspect any rules-file-change dialog. Fail-closed behavior is intentional.
- **Claude is not authenticated:** verify the relevant environment variable is listed in `env_passthrough`, then launch a new session. See [Set Up Claude Code](04-claude.md).

For configuration details, return to [Configuration And Policy](03-configuration.md). To attach VS Code, Codex, Windsurf, or another VS Code-based IDE, continue to [Use VS Code-Based Editors](07-vscode-editors.md).

# Workspaces

[Previous: Setup](01-setup.md) | [Guide index](README.md) | [Next: Configuration](03-configuration.md)

A Docker session cannot see files on the development machine until Harness Hat is told which directory to give it access to. A **workspace** is that directory. Harness Hat bind-mounts the workspace into the Docker session so the agent can work with the project files.

Choose one of these two paths. Both register the workspace automatically; no configuration file needs to be created or edited.

## Path 1: Create A Workspace In The TUI

Run this in a **terminal**:

```sh
hat
```

In the TUI, select **New Workspace...** in the sidebar. Confirm the workspace name and directory, choose the project type when it applies, then select **Create**. Choose a container template and launch the session.

> **Expected result:** the workspace appears in the sidebar and Harness Hat opens a session with the chosen directory available to the agent.

Use this path when you want to review the directory and project type before creating the workspace.

## Path 2: Launch From The Project Directory

Run these commands in a **terminal**:

```sh
cd ~/src/my-awesome-project
hat ws
```

`hat ws` is the canonical spelling (`hat workspace` remains available as a compatibility alias):

```sh
hat ws
```

`hat ws` uses the host directory you run it from to select a configured workspace. When that directory is a subdirectory of the workspace, Harness Hat opens the session at the same relative container path.

If the directory is not already registered, Harness Hat adds it as a workspace, asks you to choose a template, then opens the session.

> **Expected result:** Harness Hat identifies or creates the workspace, launches or attaches to a session, and opens a session shell. Run this command from the directory you want the agent to access.

## Global sessions and `hat sh` versus `hat ws`

All running sessions are global Harness Hat objects. They are visible in the TUI and can be listed or operated on with `hat sh`.

You can run `hat sh` from anywhere on your disk to list sessions or attach to one by ID; it never remounts a workspace. `hat ws` uses the current directory to select or create the matching workspace, then attaches to or launches a session in the same way.

Use `hat ws` from a workspace root or any of its subdirectories. The configured workspace remains the mount source by default, and a subdirectory invocation maps to that same relative path beneath the session's mount target (including a custom or mirrored target). `mount_cwd = true` remains the opt-in exception that mounts the invoking directory itself.

The two commands operate on the same global sessions: choose `hat sh <ID>` for an ID-based workflow from any directory, or choose `hat ws` when the current directory should determine the workspace.

Use this path when you are already in the project and want to start work immediately.

To run a command immediately inside docker instead of opening a shell, put it after `workspace`:

```sh
# launch claude with all permission asks disabled.
hat ws claude-yolo
# resume a previous claude session in this workspace.
hat ws claude --resume
```

> **Expected result:** Harness Hat starts or attaches to the workspace session, then runs `claude-yolo` in that container. The command's exit status is returned to the terminal.

## What The Agent Sees

By default, an absolute POSIX workspace path is mirrored at the same path inside the Linux session. Windows drive paths use a best-effort equivalent, so `C:\Users\you\project` appears at `/C/Users/you/project`. Files are read-write, so agents can change source code, configuration, and Git metadata in that directory. Review changes before running them on the host.

## Control The Workspace Path

Path mirroring is enabled by default. To make that choice explicit, edit the `harness-rules.toml` file in the workspace root on the **host filesystem** and add:

```toml
mirror_cwd = true
```

Start a new session after saving the change. An absolute POSIX workspace path such as `/home/user/my-project` appears at `/home/user/my-project` inside Linux sessions. On Windows, a drive path such as `C:\Users\you\my-project` appears at the best-effort container path `/C/Users/you/my-project`. Set `mirror_cwd = false` to use the configured container location (normally `/workspace`) instead.

## Choose A Template Or Rebuild

Templates determine which development tools are available in the session. Choose a Rust template for a Rust project, for example, or select a different template when trying another environment. Harness Hat remembers the selection in the workspace's `harness-rules.toml`. Rebuild when Harness Hat or its Dockerfiles change, or when you want the image-installed tools and packages refreshed.

Run these commands in a **terminal**:

```sh
# Run your workspace using the "rust" template.
hat ws --template rust
# Run your workspace with a specific name and the "python" template
hat ws --name my-project --template python
```

> **Expected result:** each command selects the requested workspace/template and attaches to its session.

`hat ws --rebuild` rebuilds the selected session image without Docker's layer cache before launch. Run a full image refresh about once a week in a **terminal**:

```sh
hat rebuild --no-cache
```

> **Expected result:** Docker rebuilds the base image and every template image that already exists locally. Add `--all` (`hat rebuild --no-cache --all`) to include templates that have not been built yet. New sessions use the refreshed image; existing sessions keep their current image until they are restarted.

This refresh updates tools and packages installed by the Dockerfiles. It does not update project dependencies pinned by files such as `Cargo.lock`, `package-lock.json`, or `poetry.lock`.

Continue with [Configuration](03-configuration.md).

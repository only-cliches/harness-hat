# Configuration And Policy

[Previous: Workspaces](02-workspaces.md) | [Guide index](README.md) | [Next: Claude Code](04-claude.md)

Harness Hat manages its configuration and policy files for normal developer workflows. `hat install` creates the global configuration at `~/.config/harness-hat/harness-hat.toml`; creating a workspace through the TUI or `hat ws` records the workspace automatically.

## Global Configuration

The global configuration controls the local manager, Docker templates, networking defaults, and the registered workspace list. The default created by `hat install` is the standard setup for this guide.

The default also passes `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` from the **host terminal** environment into new sessions, so the Claude authentication steps in the next page work without additional configuration.

`[defaults.proxy]` also has `idle_timeout_secs`, which defaults to `30`. It closes a client socket or CONNECT tunnel when neither direction has transferred traffic for that period. Set a larger positive value for long-idle protocols.

## Project Policy

Each workspace can have project policy for network access and `hostdo`. When you create a typed workspace in the TUI, Harness Hat creates an appropriate starter policy. When a network or host-command prompt is approved with a remembered decision, Harness Hat records the narrow rule for that project.

Project policy can also define host-local TCP forwards in the workspace's `harness-rules.toml`:

```toml
[[localhost_forwards]]
container_port = 8081
host_port = 11434
```

Inside a new session, `localhost:8081` reaches host port `11434`. Omitting `host_port` uses the same port. A rule with the same `container_port` overrides the selected template's forward; changes apply to newly launched sessions.

Review remembered project-policy changes through normal version control. A changed global or project policy file blocks new network and host-command decisions until its current version is reviewed and trusted in the system dialog, the attached headless TUI, or `hat approvals trust ID`. Dismissing the review keeps requests blocked.

## Team-Managed Settings

Organization-wide policy belongs in the global policy managed by the team, rather than in each developer's setup steps. This includes permanent network denials, host-command defaults, and the workspace-path mirroring policy.

Path mirroring is enabled by default, so a POSIX workspace such as `/home/user/my-project` is mounted at that same path in Linux sessions instead of `/workspace`. Windows drive paths use a best-effort equivalent such as `/C/Users/you/my-project`. Set `mirror_cwd = false` to retain the configured container location.

Continue with [Claude Code](04-claude.md) or [hostdo](05-hostdo.md).

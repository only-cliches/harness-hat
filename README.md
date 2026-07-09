# Harness Hat

> Docker-backed development sessions with proxy-mediated network policy — driven from a terminal UI.

Harness Hat (`hh`) is a session manager for running coding agents and dev workflows inside disposable, network-filtered Docker containers. You register a workspace, pick a language template, and get an interactive shell in a sandbox whose outbound traffic is steered through a policy-enforcing proxy governed by per-workspace allow/deny rules.

## Why

Modern coding agents (`claude`, `codex`, `antigravity`, `pi`, …) want to read your home directory, install random packages, hit unknown endpoints, and execute arbitrary shell. Giving them an unrestricted shell on your laptop is a bad time.

Harness Hat boxes each session in a container with:

- A real shell, real toolchains, your repo bind-mounted at `/workspace`.
- A scoped HTTP/CONNECT proxy that **prompts before allowing unknown hosts**, persists your decisions to `harness-rules.toml`, and refuses anything denied.
- **Strict-network mode**: `tun2proxy` + `iptables` capture *all* outbound TCP, so agents can't bypass the proxy by ignoring `HTTPS_PROXY`.
- Per-session seeded mounts for `~/.claude.json`-style files that agents rewrite in place, so two concurrent sessions can't corrupt each other.
- Container bootstrap for proxy routing, strict egress rules, localhost forwards, agent state mounts, and common coding-agent CLIs.

If an agent wants to `curl evil.example.com/install.sh`, it asks first. You see the request. You decide.

## How it compares

Harness Hat overlaps with several development-environment, agent-sandbox, and container-workflow tools. This table focuses on the features that matter when you want an AI coding agent to work in a real repo without getting an unrestricted shell on your laptop.

| Feature | Harness Hat | [Dev Containers](https://containers.dev/) | [Codespaces](https://docs.github.com/en/codespaces/about-codespaces/what-are-codespaces) | [Coder](https://coder.com/docs) | [Daytona](https://www.daytona.io/docs/) | [E2B](https://e2b.dev/docs) | [Devbox](https://www.jetify.com/docs/devbox) |
|---------|-------------|----------------------------------------|--------------|-------|---------|-----|--------|
| Local-first | Yes, local Docker | Yes, or remote | Cloud | Self-hosted remote | Cloud/API | Cloud/API | Yes |
| Interactive dev shell | Yes | Yes | Yes | Yes | CLI/SSH/web | CLI/SDK | Yes |
| Runtime egress approvals | Yes | No | No | Admin/platform controls | Platform controls | Platform controls | No |
| Strict all-TCP egress capture | Yes | No | Platform-managed | Infrastructure-dependent | Sandbox networking | VM sandbox networking | No |
| Repo-local policy file | `harness-rules.toml` | Config, not policy | Dev container config | Template/config | Sandbox/template config | Template config | Config, not policy |
| Host-command approval gate | Yes, via `hostdo` | No | No | Not core | API/SDK model | API/SDK model | No |

## Requirements

- A macOS or Linux host with Docker — Docker Desktop on macOS, Docker Engine or Docker Desktop on Linux — and the `docker` CLI on your `PATH`.
- A Rust toolchain, for `cargo install`.
- For strict-network mode: `/dev/net/tun` on the host. See [Container privileges](#container-privileges) for what this implies — it matters if your organization restricts privileged containers.

## Quick start

```sh
cargo install harness-hat              # binary is `hh`
hh  # will prompt you to create config, or launch the manager if config exists
```

Inside the TUI: pick a workspace, pick a template, get a shell. From inside the container, run `killme` to ask Harness Hat to stop the session.

With the manager running in another terminal, you can also attach to or start the session for your current directory:

```sh
hh workspace                  # match $PWD to a workspace, launch if needed, then attach
hh workspace --template rust  # skip the template picker
hh workspace claude --resume  # runs "claude --resume" inside the session
```

To attach to an already-running session from a separate terminal:

```sh
hh shell           # lists running sessions
hh shell <ID>      # attaches via `docker exec -it`
hh shell <ID> CMD  # runs CMD via docker exec
```

### VSCode-like editors (VS Code, Cursor, Windsurf, etc.)

You can attach these editors directly to a running Harness Hat container using the Dev Containers extension:

1. Install the **Dev Containers** extension (`ms-vscode-remote.remote-containers`).
2. Start your Harness Hat session so the target container is running.
3. In the editor command palette, run `Dev Containers: Attach to Running Container...`.
4. Select the active `harness-hat` container.
5. Open `/workspace` in the remote window.

## Model

```
 workspace  ─┐
             ├── session  ──>  one running container
 template   ─┘
```

- **Workspace** — a fixed host directory (your repo), mounted into the container at `/workspace`.
- **Template** — a `[container_profiles.<name>]` block referencing a Dockerfile stem under `docker_dir`, plus any compatible workspace-local `*.dockerfile` files. Sets memory, CPU, mounts, env, pre-approved hosts, and starter network allowlist.
- **Session** — one container, one shell, one network policy. Stop it from the TUI or by running `killme` inside it.

## Built-in templates

The base image is Ubuntu 24.04 with Node 22, bundled agent CLIs (`claude`, `codex`, `agy`, `pi`), and the shared proxy/control plumbing. Stacked on top:

| Stem         | Toolchain                                                          |
|--------------|--------------------------------------------------------------------|
| `default`    | Node, pnpm, TypeScript, `tsx`, Bun                                 |
| `typescript` | TypeScript, Bun, npm, Node, pnpm, Vite, ESLint, Prettier           |
| `go`         | Go, `gopls`, Delve, `staticcheck`, `golangci-lint`                 |
| `rust`       | Rust stable + rustfmt, clippy, rust-analyzer, nextest, audit, deny |
| `php`        | PHP CLI/dev, Composer, PHPUnit, PHP-CS-Fixer, PHPStan, Pint, Xdebug, PCOV |
| `dotnet`     | .NET SDK 8/10, EF Core CLI, dotnet-format, CSharpier                     |

Drop your own `something.dockerfile` under `docker_dir` and reference it as `image = "something"`. Workspace-local `*.dockerfile` files are also auto-discovered as launch templates when their first non-comment instruction is `FROM harness-hat-base:local`.

### YOLO wrappers

The base image also ships `claude-yolo`, `codex-yolo`, and `agy-yolo` — thin wrappers that launch each agent with its own permission prompts disabled (`claude --dangerously-skip-permissions`, `codex --yolo`, `agy --dangerously-skip-permissions`). Running an agent like that on a bare host is exactly what Harness Hat exists to avoid; inside a session, the container boundary and the network policy are the guardrails instead, so the agent can work uninterrupted while the proxy still gates every outbound connection. Use them when you trust the sandbox, not the agent.

## Network policy

Each workspace can commit a `harness-rules.toml` next to its source. Harness Hat composes it with the global rules file at request time, so edits can take effect without restarting the container, and persists any "Allow forever" / "Deny forever" approvals from the TUI back into it.

```toml
version = 1

[network]
allowlist = [
  "api.github.com",
  "registry.npmjs.org",
  "*.crates.io",
]
denylist = [
  "*.evil.example",
]
```

- **Deny wins** over allow.
- **Unknown** requests prompt in the TUI (Allow once / Deny once / Allow forever / Deny forever).
- Domain rules support exact (`example.com`) and subdomain-only wildcards (`*.example.com`).
- Hostnames are canonicalized (lowercase, trailing-dot strip, IDNA) before rule matching — case, trailing dots, and punycode can't bypass denies.
- HTTPS and other raw TCP destinations are policy-checked as `CONNECT` requests. Because TLS is not decrypted, HTTPS rules can only match the CONNECT host and port, not the inner HTTP method or path. Domain-only allow rules auto-allow HTTPS CONNECT on port 443; non-443 CONNECT needs an explicit `port=...` rule.

For hosts that should never prompt, use `[defaults.containers].allowed_hosts` or per-template `allowed_hosts` in `harness-hat.toml`. This pre-approves matching hosts without bypassing the proxy or strict-network routing. `allowed_hosts` supports exact hosts, `*`, and apex-plus-subdomain patterns such as `*.example.com`.

## Strict-network mode

Enabled by default in the example config (`strict_network = true`). When on:

1. `tun2proxy` runs inside the container and captures **all** TCP via a TUN device.
2. `iptables` rejects every outbound packet that isn't loopback, Docker DNS, the scoped proxy, the control server, or an explicit `localhost_forwards` target.
3. UDP/QUIC are blocked (except DNS to Docker's embedded resolver).
4. IPv6 is rejected wholesale to prevent AAAA/QUIC hangs.

The result: an application that "doesn't honor `HTTPS_PROXY`" still gets its packets steered through the proxy or dropped.

### Container privileges

Strict mode changes how the container is started:

- **Linux**: the container starts with `--cap-drop ALL` and re-adds only `NET_ADMIN` (iptables + TUN setup), `SETUID`, and `SETGID` (the init's downward `gosu` drop to uid 1000), plus a `--device /dev/net/tun` passthrough — not full `--privileged`. If `/dev/net/tun` is missing on the host, the launch fails with an error instead of silently escalating.
- **macOS (Docker Desktop)**: the container is started `--privileged`, because that is the only way Docker Desktop exposes `/dev/net/tun`.

## Localhost port passthrough

Some tools need to reach a service already running on your laptop: a local model server, a dev database, a callback server, or an app backend. `localhost_forwards` exposes selected host-local TCP ports inside the container as `localhost:<container_port>` without opening general host networking.

```toml
[[defaults.containers.localhost_forwards]]
container_port = 8081
host_port = 11434

[[container_profiles.typescript.localhost_forwards]]
container_port = 3000
```

With the first rule, a process in the container that connects to `http://localhost:8081` reaches port `11434` on the host. With the second, `host_port` is omitted, so `localhost:3000` in that template reaches host port `3000`.

Forwards can be set under `[defaults.containers]` for every template or under `[container_profiles.<name>]` for one template. A profile forward with the same `container_port` replaces the default forward for that port.

In strict-network mode, configured forwards are added to the egress allowlist during container bootstrap. Other direct host or network destinations still go through the proxy policy or are blocked. This is not Docker `-p` publishing: it lets the container reach selected host services; it does not expose container ports back to the host or LAN.

## Security posture

The proxy and control plane are hardened against the usual proxy-abuse classes:

- Bearer-token and proxy-auth comparisons use constant-time equality.
- Scoped proxy listeners enforce per-source and total connection limits.
- Plain HTTP rejects missing, duplicate, or invalid `Host` headers.
- Forwarded HTTP strips hop-by-hop headers and every token named by `Connection:`.
- Destinations are resolved before connecting and restricted/private/link-local addresses are denied.
- DNS lookups are bounded, cached, and case-normalized.
- IPv6 SSRF predicate covers NAT64, 6to4, IPv4-translated, and discard-only prefixes.
- Audit log and `log_dir` are created `0o600`/`0o700` atomically and refuse to follow symlinks.
- Control server and proxy default to loopback-only; non-loopback binds require explicit `allow_remote_control = true`.
- Workspace paths under `~/.ssh`, `~/.gnupg`, `/etc` are refused.
- Mount/container paths reject `:` and `,` to prevent `-v` argument injection.

### Threat model — what Harness Hat does not protect against

Harness Hat narrows what an agent can reach; it does not make a malicious agent safe. Know the boundaries:

- **TLS is not decrypted.** Policy sees only the CONNECT host and port. Allowing a host allows everything that host serves — an agent allowed to reach `github.com` can push data to any repository it can authenticate to.
- **Passed-through secrets are readable.** Anything in `env_passthrough` (e.g. `ANTHROPIC_API_KEY`) is visible to every process in the session, including the agent.
- **Your repo is writable.** `/workspace` is a read-write bind mount. Agents can edit source, configs, and git hooks — review diffs before running the result on the host.
- **Container isolation is Docker's.** A kernel or runtime escape is outside Harness Hat's control, and strict mode on macOS starts the container privileged (see [Container privileges](#container-privileges)).

## Configuration overview

```toml
version = 1
docker_dir = "~/.config/harness-hat/docker"

[manager]
global_rules_file = "~/.config/harness-hat/harness-rules.toml"

[defaults.control]                       # killme + session identity
server_port = 7878
server_host = "127.0.0.1"
token_env_var = "HARNESS_HAT_TOKEN"

[defaults.proxy]
proxy_port = 28781
proxy_host = "127.0.0.1"
strict_network = true

[defaults.containers]
env_passthrough = ["TERM", "COLORTERM", "COLORFGBG", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]
allowed_hosts = [
  "api.anthropic.com",
  "claude.ai",
  "*.openai.com",
  "github.com",
]

[[defaults.containers.mounts]]           # shared across all templates
host = "~/.claude.json"
container = "/home/coder/.claude.json"
mode = "rw"
seed = true                              # per-session copy, not a live bind

[[defaults.containers.mounts]]
host = "~/.claude/.claude.json"
container = "/home/coder/.claude/.claude.json"
mode = "rw"
seed = true

[[defaults.containers.localhost_forwards]]
container_port = 8081
host_port = 8081

[container_profiles.rust]
image = "rust"
memory = "6g"
cpus = "3"
shm_size = "1g"
starter_network_allowlist = [
  "domain=crates.io",
  "domain=*.crates.io",
  "domain=github.com",
]

[[workspaces]]
name = "my-project"
canonical_path = "~/src/my-project"
```

Full example: [`harness-hat.example.toml`](harness-hat.example.toml).

## Rolling out to a team

The pieces that matter once more than a handful of developers are involved:

- **Commit `harness-rules.toml` per repo.** Network approvals live next to the code and flow through normal review, so one developer's "Allow forever" becomes the team's rule instead of a private setting.
- **Manage the global rules file centrally.** Point `[manager].global_rules_file` at a path owned by your configuration management. Denies always win when rules compose, so a managed denylist — and `[hostdo] default_policy = "deny"`, if you want host execution off — cannot be overridden by a repo-local file.
- **Pre-approve the baseline.** Put your organization's package registries, VCS hosts, and agent API endpoints in `[defaults.containers].allowed_hosts` so day-one sessions don't drown developers in prompts.
- **Ship a shared `harness-hat.toml`.** Templates, mounts, and defaults live in one file; distribute it with your dotfiles or fleet tooling and developers only add their own `[[workspaces]]` entries.
- **Pin the version.** `cargo install harness-hat --version X.Y.Z` keeps the fleet on a known release and makes upgrades deliberate.

## Claude CLI authentication

Each container session runs Claude Code in a fresh environment. Harness Hat supports API-key auth, `claude setup-token` OAuth env auth, and on macOS can inject the local Claude Code Keychain access token into the seeded container session files.

**API key** (recommended for most setups):

1. Generate a key at [console.anthropic.com](https://console.anthropic.com) → API Keys.
2. Export it in your shell profile:
   ```bash
   export ANTHROPIC_API_KEY="sk-ant-api03-..."
   ```

**OAuth token** (alternative — stays tied to your Claude account):

1. Run once on the host to generate a long-lived token:
   ```bash
   claude setup-token
   ```
2. Export the printed value in your shell profile:
   ```bash
   export CLAUDE_CODE_OAUTH_TOKEN="<token>"
   ```

Either env var bypasses the interactive browser login flow, so new sessions start authenticated immediately. Run `/status` inside a session to confirm which method is active.

## Antigravity CLI authentication

Antigravity CLI (`agy`) stores settings and history under `~/.gemini/antigravity-cli`, but its login tokens live in the OS secure keyring. Harness Hat mounts `.gemini` for settings and starts a headless Linux Secret Service in each session, backed by `~/.local/share/harness-hat/container-keyrings` on the host.

The first `agy` login should be done inside a Harness Hat session. After that, new sessions reuse the persisted container keyring. A host desktop login is not copied by the `.gemini` mount alone.

## Host-side commands

Managed containers include `hostdo`, a small bridge for running approved host-side build, package, compiler, and test commands when the container is not the right execution environment.

```sh
hostdo run cargo test
hostdo run --image node:20 npm test
hostdo list
hostdo tail <job-id> --rows 100
hostdo stop <job-id>
```

`hostdo` is the one deliberate hole in the sandbox: approved commands execute **on the host** (or, with `--image`, in a separate host-side Docker container), outside the session's network policy. Treat every rule you add as a host-execution grant.

Commands are checked against `[hostdo]` rules in the global and workspace `harness-rules.toml` files. Unknown commands prompt for approval, and remembered approvals are persisted as exact command rules. The `default_policy` key accepts `auto`, `prompt` (the default), or `deny` — and when the global and workspace files disagree, deny wins, so an organization can turn host execution off fleet-wide with `default_policy = "deny"` under `[hostdo]` in the managed global rules file.

Hostdo children never see harness-hat's own control-plane variables (`HARNESS_HAT_*` is always stripped). A rule can additionally set `env_allowlist = ["NAME", ...]` to run its command from a cleared environment containing only a small base set (`PATH`, `HOME`, locale, etc.) plus the listed variables, instead of inheriting the manager's full host environment.

## CLI

```
hh                       # launch interactive workspace manager (default)
hh --config PATH         # use a specific config
hh init [PATH]           # write a starter config (default: ./harness-hat.toml)
hh workspace             # attach to or start a session for the current directory
hh workspace --template NAME [COMMAND...]
hh shell                 # list running sessions
hh shell <ID> [COMMAND...] # docker exec into a running session
```

## Upgrading

```sh
cargo install harness-hat --force
```

Container images are built locally from the Dockerfiles under `docker_dir` and tagged `harness-hat-base:local`. After upgrading `hh` or changing a Dockerfile, rebuild the image from the TUI so new sessions pick up the changes — running sessions keep their existing image until restarted.

## License

MIT — see [`LICENSE`](LICENSE).

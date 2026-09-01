# Changelog

## 0.8.8 Future

### Added

- The default TypeScript image now includes Playwright-provisioned headless Chromium, `agent-browser`, and its Codex skill for browser automation without a second browser download.
- Added OpenCode to the shared base image and mounted its `~/.config/opencode` settings, agents, commands, and plugins into sessions.
- Added `hat rules status [--workspace NAME] [--json]` plus explicit per-file recovery commands: `hat rules trust --workspace NAME` and `hat rules trust --global`. Workspace Settings now expose the equivalent inspect and trust actions.
- Added `hat sh ID --kill-connections` and the matching TUI `x` action to drop all currently open proxy connections for a running session. The container keeps running, and later connections remain subject to normal network policy.

### Changed

- Updated the bundled Codex (0.149.1), Pi (0.84.2), OpenCode (1.18.21), Antigravity (1.1.19), and oh-my-pi (18.0.4) CLIs to their current pinned releases.
- Changed `hat rebuild` so that, when no templates are named, it rebuilds only template images that already exist locally. Explicit template names still force those templates to rebuild, while `hat rebuild --all` rebuilds every configured template.
- Folded repeated pending network requests for the same session, domain, and port into one approval prompt, even when their HTTP methods or paths differ. Requests from other sessions or to other ports remain separate.

### Fixed

- Container launch now rolls back any Docker container that starts but cannot be adopted into a TUI session, and removes its provisional session authorization. Failed launches no longer leave an unmanaged container with no scoped proxy/network access.
- Explicit rules recovery clears a matching stale rules-change alert and synchronizes the rules watcher baseline, so a reviewed file trusted through the TUI or CLI is not immediately blocked again.

## 0.8.7 Aug 14, 2026

### Added

- Added an `android` Docker template with Kotlin, Gradle, Android SDK command-line tools, API 36, Build Tools 36.0.0, platform-tools, and `adb` for Android builds.
- Added `ContainerMount.add_to_path` for prepending mounted container directories to the image's existing `PATH`, including non-interactive `hat sh`/`hat ws` commands.
- Added `hat install --headless` for Linux servers without a graphical session. The installer creates a `systemd --user` service, enables lingering so it can run across logout and at boot, and keeps installation scoped to the normal Docker-enabled user.
- Added headless approval management through `hat approvals list [--json]`, `allow ID [--remember]`, `deny ID [--remember]`, and `trust ID`. Pending requests use short four-digit IDs, remain fail-closed while waiting, and can also be decided from an attached TUI.
- Added nested `open EDITOR` actions (`hat sh ID open EDITOR` and `hat ws open EDITOR`) for opening sessions through any PATH-resolved editor executable that supports the Dev Containers integration.
- Added canonical `hat sh`/`hat ws` session entry points, nested IDE actions, `ws --new` fresh-session launches, and launch-only `hat sh new --path DIRECTORY`, which prints the new integer session ID.

### Changed

- Workspace path mirroring is now enabled by default. Absolute POSIX paths are preserved, and Windows drive paths use a best-effort container equivalent such as `/C/Users/example/project`. Set `mirror_cwd = false` to retain the configured mount target.
- Changed the canonical session termination syntax to `hat sh ID --kill`.
- Standardized CLI help and documentation around Clap-generated command usage, `sh`/`ws` (with `shell`/`workspace` compatibility aliases), compact shell actions, and approval subcommands.
- `ws` is now current-directory-only; explicit directory launch belongs to `hat sh new --path DIRECTORY`.
- Shell session IDs are now monotonically increasing integers (with legacy zero-padded IDs still accepted when attaching).
- Expanded the TUI's selected-session Shell section to show attach, command, IDE, and stop forms for `hat sh`.
- Generalized editor launches to accept one executable name from `PATH` (including VS Code forks and user-provided wrappers), with clear errors for missing executables or non-zero exits. Harness Hat does not probe editor capabilities because no reliable universal `--folder-uri` check exists.
- Clarified the editor guide so Dev Containers setup comes before editor launch instructions, and command-line opening and manual attachment are presented as alternative workflows.
- Fixed the TUI workspace action flow so Launch moves into the container-template picker while Remove remains available without launching a session.
- Fixed `hat ws` launch streams aborting during long or quiet Docker builds because the CLI inherited reqwest's default request timeout.
- `hostdo` now rejects direct invocation of `hat`/`hat.exe` (and legacy `hht` names) before policy matching, marks hostdo process trees so wrapped `hat` calls also refuse to run, and rejects rules files containing direct Harness Hat control commands.
- The maximum hostdo command timeout is now five minutes; larger requested or configured values are capped at 300 seconds.
- Renamed the user-facing CLI from `hht` to `hat` and the background service binary from `hht-daemon` to `hat-daemon`; release archives now use the new executable names.

## 0.8.6 Aug 12, 2026

### Added

- Added `hht ws` as the preferred shortcut for `hht workspace`; the previous shortcut remains available for compatibility but is deprecated.
- Added the `omp` and `omp-yolo` coding-agent commands to the base image, with
  pinned, architecture-specific release checksums and optional `~/.omp` state
  mounting.
- Added `Ctrl+D` workspace deletion from the sidebar, including the existing
  confirmation flow.
- Added mouse-driven text selection in the local terminal and activity panes, with OSC 52 clipboard copy support through `Cmd+C` or `Ctrl+Shift+C`.
- Added complete Docker build logs for failed image builds. The build pane shows the saved log path, and the ten most recent build logs are retained.

### Changed

- Failed image builds now retain their workspace/template context so the build can be retried directly from the build pane.
- Updated the base image to Ubuntu 26.04 LTS with Node.js 24.
- Updated the PHP template to PHPUnit 13.3.0, which is supported by Ubuntu
  26.04's PHP 8.5 runtime.
- `env_passthrough` values are now captured from the invoking `hht ws` process,
  so variables exported after the daemon starts reach new sessions.
- New sessions can seed their workspace-local zsh history from a read-only copy
  of the host history file.

### Fixed

- Attached TUI clients now start correctly on Windows after mouse capture was disabled for terminal text selection.
- Attached daemon clients now receive a complete frame when a Docker build fails, so the final failure state and saved log location are displayed.
- The background daemon now stays available when Docker is offline, reports the Docker-specific condition to `hht ws`, and retries Docker readiness every ten seconds so workspace launches work after Docker starts.
- Docker image builds now retry transient download failures for pinned tool and
  package sources.
- Attached daemon TUI actions that depend on the current directory now use the
  attached client's directory instead of the background service's directory.


## 0.8.5 Aug 2, 2026

### Added

- Added `hht restart`, a session-preserving daemon refresh that reloads validated configuration and disposable caches without replacing the daemon process or stopping running sessions.
- Added `hostdo --reason <text>` support so approval prompts can carry operator context; when remembered, the reason is persisted in `harness-rules.toml` but is not used in matching.
- Added native Windows network and host-command approval dialogs with remembered allow/deny decisions and foreground activation.

### Changed

- Hostdo command persistence no longer treats timeout as part of approval matching semantics; exact matches still use `argv + image`, while timeout and reason are stored only for context.

### Fixed

- `hht workspace` now preserves the caller's relative workspace directory when attaching to an existing session or attaching immediately after a new launch, including custom container mount targets. Named workspaces selected from elsewhere start at their mount root.
- Attached `hht` clients now retry transient daemon TUI backpressure instead of exiting with a generic `/tui/frame` operation timeout. The daemon reports a bounded render queue as `tui_busy` rather than leaving the HTTP request waiting indefinitely.
- Entering the terminal view in an attached TUI now forces a complete screen repaint, preventing a stale or blank delta-rendered screen.
- A lost manager-side `docker run` terminal connection no longer marks a still-running container as stopped. The session remains visible as terminal-detached and shows the `hht shell` reconnection command.
- Windows background operations no longer flash transient console windows when invoking Docker, host commands, or process-management utilities.
- Uninstalling the Windows service now stops both the scheduled task and any remaining daemon process in the current session.
- Attached TUI clients now refresh while builds run before a session exists, repaint once when a build completes, and clear stale rows after a resize.
- The manager now periodically reconciles live TUI sessions with Docker, so a missed Windows PTY-exit event cannot leave a removed `docker run --rm` container displayed as running.

## 0.8.4 July 29, 2026

### Added

- Added a shortcut for the `hht workspace` command.
- Added `[[localhost_forwards]]` support to `harness-rules.toml`, including workspace-specific overrides for configured container forwards.

### Changed

- The TUI now retains and exposes multiple terminal sessions launched for the same workspace and template as selectable child rows.
- Docker templates now source .NET, Bun, uv, Go, Rust, Temurin, and Gradle from pinned official multi-architecture image manifests instead of maintaining per-CPU release URLs and checksums. `gofumpt` is installed through its checksum-verified Go module.
- CI now rebuilds every built-in language image and smoke-tests the installed toolchains, catching Docker template regressions before release.

### Fixed

- Remembering a network or host-command decision no longer marks a rules file with a selected workspace template as externally modified. The daemon keeps the canonical policy fingerprint trusted, so proxy traffic continues for existing containers after a remembered choice.
- Daemon-attached `hht` clients now receive a complete first terminal frame instead of applying a stale screen delta.


## 0.8.3 July 27, 2026

- Resolved CI/CD Failures.

## 0.8.2 July 27, 2026

### Changed
- Forwarded terminal environment variables (`TERM`, `COLORTERM`, `COLORFGBG`) into `hht shell` container exec sessions, with a fallback `TERM=xterm-256color` when `TERM` is unset.
- Also forwarded terminal environment passthrough (`TERM`, `COLORTERM`, `COLORFGBG`) through `hht workspace` launch requests so CLI-attached sessions inherit caller terminal env on attach.
- Reduced TUI refresh polling jitter by making background channel draining and terminal snapshot refreshes report state changes, so the event loop only redraws when something actually changed.
- Increased container usage staleness threshold from 2 seconds to 5 seconds before re-fetching usage metrics.

### Fixed
- Improved TUI rendering performance and stability by caching the last rendered frame, skipping duplicate frames, and forcing a full repaint only when stale/dropped-frame recovery requires it.
- Replaced per-frame full-screen clear behavior in the terminal renderer with incremental cursor-cell output, and avoid unnecessary redraw passes when no frame content changed in manager and remote relay paths.

## 0.8.1 July 24, 2026

### Changed
- Clarified workspace-mount behavior in user guide documentation: `hht workspace` mounts the directory where it is invoked, while `hht shell` can be run from any directory and does not affect workspace mount location.
- Updated Docker image/template updates in the `docker/` assets to reflect current release image packaging changes.


## 0.8.0 July 24, 2026

### Fixed
- Network, host-command, and rules-change native approval dialogs launched by the background service now invoke the sibling `hht` executable, rather than the daemon-only binary. Unmatched network requests therefore display their approval prompt instead of being immediately denied.
- Test helpers that create temporary workspaces and proxy configs now use stable non-sensitive temp roots in host-managed environments (for example `hostdo` sessions on macOS) so test fixtures no longer fail due to the sensitive-path refusal checks.

### Added
- Authenticated, sequenced `/tui/events` long-poll feed for attached clients. Workspace launch requests, build output, launch completion/failure, and active-session refreshes now wake an open `hht` client without fixed-rate frame polling; clients reload their snapshot if they fall behind the bounded event window.
- User Guide page for attaching VS Code, Windsurf, Codex, and compatible VS Code-based IDEs to existing Harness Hat sessions with Development Containers.
- MVP Windows 11 host support for Docker Desktop Linux containers. Runtime bind mounts now use Docker `--mount` syntax, strip the `\\?\` prefixes produced by Windows canonicalization, strict-network Docker Desktop launches use `--privileged`, Docker image builds no longer depend on `sh -lc`, embedded and mounted Linux scripts are normalized to LF, Windows cancellation paths use `taskkill /T /F`, TUI key-release events no longer trigger duplicate build/launch actions, and ConPTY escapes Docker arguments containing spaces. Failed launches also stop polling promptly and surface the captured Docker error. Codex uses container-local state on Windows while portable auth/config/plugin data is seeded from the host, avoiding unsupported SQLite locking across the Windows-to-Linux bind filesystem.
- `allowed_hosts` configuration field in `harness-hat.toml` (at defaults and per-container level) for hosts that bypass network approval prompts without needing `harness-rules.toml` entries. Supports wildcard patterns matching.
- New `hht shell [ID] [COMMAND...]` subcommand: open an interactive shell in a running session, or run a one-off command in it. With no ID it lists running sessions with both their Harness Hat and Docker container IDs; `hht shell --kill <ID>` terminates and removes a session. Any args after the ID are passed verbatim to `docker exec` (e.g. `hht shell 0042 claude --resume`). Works as a thin Docker attach, independent of the manager TUI; falls back to `docker exec -i` (no `-t`) when stdin is not a terminal so piped commands like `echo prompt | hht shell ID cat` work.
- Restored the in-container `hostdo` command and tracked hostdo jobs (`run`/`list`/`status`/`tail`/`send`/`stop`) on top of the new `/exec` and `/exec/jobs/*` control-server endpoints.
- Restored hostdo sidebar child rows in the manager TUI: hostdo requests now create navigable activity items that can be inspected and cancelled with `Ctrl+C`, just like earlier hostdo activity tracking.
- Hostdo approvals now use the same native modal subprocess flow as network approvals on macOS, with the in-TUI approval overlay retained as the non-macOS fallback.
- Native OS notifications (Linux D-Bus, macOS, Windows toast) when a network-approval modal becomes pending in the TUI, so the user is nudged toward the approval even when the TUI isn't focused. The body shows the host, source workspace, and remaining pending count. Best-effort — any failure (missing D-Bus, macOS bundle quirks, toast permission) is logged at debug and ignored.
- Default `starter_network_allowlist` in the example config now includes Antigravity CLI's runtime domains: `antigravity-unleash.goog`, `play.googleapis.com`, `oauth2.googleapis.com`, `www.googleapis.com`, `daily-cloudcode-pa.googleapis.com`, `lh3.googleusercontent.com`, and the Playwright CDN domains (`playwright.azureedge.net`, `playwright-akamai.azureedge.net`, `playwright-verizon.azureedge.net`).
- New `hht init [PATH]` subcommand to generate a sample config (replaces the old `--init` flag; defaults to `./harness-hat.toml`).
- Built-in Docker templates for TypeScript/Bun/Node/pnpm, Go, Rust, and PHP development environments (`docker/typescript.dockerfile`, `docker/go.dockerfile`, `docker/rust.dockerfile`, `docker/php.dockerfile`).
- Built-in Docker templates for Python (`uv` with Python 3.13), Kotlin/JVM (Temurin 21, Kotlin, and Gradle), and C#/.NET (SDKs 10 and 8), with profile-specific starter network allowlists.
- `hht workspace --name <WORKSPACE>` to start or attach to a named workspace without changing directories, and `hht workspace --rebuild` to rebuild its base and template images with Docker's layer cache disabled. The selected template is now saved on the workspace and used as its default on later launches.
- Per-container `attach_shell` configuration, recorded on launched containers and used by `hht shell` / `hht workspace` attaches; the base image now includes zsh with Oh My Zsh and workspace-local history.
- Per-container `claude_settings` configuration to seed a private `~/.claude/settings.json` from a host file for each session.
- `hostdo output [--image IMAGE] [--timeout SECONDS] COMMAND...` for synchronous host-side command execution with captured output and the underlying exit code.
- `hht rebuild [--no-cache] [TEMPLATE...]` to rebuild the base image and all, or selected, Dockerfile templates from the configured `docker_dir`; and a tag-triggered GitHub Actions workflow that publishes macOS, Windows, and Linux release artifacts.
- `hht install` / `hht uninstall` for a per-user graphical background agent: launchd on macOS, systemd user services on Linux, and a Task Scheduler logon task on Windows. The agent runs the control plane and workspace launch path without a terminal; approval dialogs remain native and fail closed.
- The installed background agent now runs as the dedicated `hht-daemon` executable, with release archives packaging both `hht` and `hht-daemon`.
- A plain `hht` command now detects the installed daemon and attaches to its existing terminal UI instead of failing because the daemon already owns the control port. The daemon remains the owner of the live session, build, approval, and terminal state.
- Per-template Docker resource controls: `memory`, `cpus`, and `shm_size`.
- Per-session seeded mounts (`ContainerMount.seed`): files like `~/.claude.json` and `~/.claude/.claude.json` are copied per session instead of bind-mounted, so concurrent sessions don't corrupt a file the agent rewrites in place. The TUI mounts view visually distinguishes seeded mounts.
- Default container mounts for agent session state (`~/.claude.json`, `~/.claude/.claude.json`, `~/.claude`, `~/.codex`, `~/.config/codex`, `~/.gemini`, `~/.local/share/harness-hat/container-keyrings`, `~/.pi`) under `[defaults.containers.mounts]`; mounts whose host source is absent are skipped. The `~/.gemini` passthrough covers Antigravity CLI's `~/.gemini/antigravity-cli` settings/history state, while the dedicated keyrings mount persists its OS-keyring auth tokens.
- The base image now starts a headless DBus Secret Service (`gnome-keyring`) for user sessions so Antigravity CLI can store and reuse login credentials inside Harness Hat containers.
- The base image now includes `ripgrep`, ast-grep (`sg`), and zsh; the Go image includes `gofumpt`.
- Automated macOS Keychain OAuth credentials injection: on macOS, reads the Claude Code OAuth credentials from the system Keychain and merges them into the seeded `.claude.json` and `~/.claude/.claude.json` container files on startup, allowing Claude Code to authenticate seamlessly without manual token config.
- `[defaults.control].allow_remote_control` opt-in required to bind the control server or proxy to non-loopback addresses.
- Persistent build pane: failed/cancelled builds keep their output and a banner with an `[r] Rebuild` shortcut.
- `api.github.com` (starter network allowlists) and `downloads.claude.ai` (default `allowed_hosts`) added for in-container GitHub API access and the apt-based Claude Code install.
- Workspace-local `*.dockerfile` files are auto-discovered as launch templates when their first non-comment instruction is `FROM harness-hat-base:local`, alongside the built-in templates under `docker_dir`.
- `jq` in the base image (the default Claude Code statusLine script parses its status JSON with jq, which previously rendered blank in-container).
- Claude Code setup-token env auth: when `CLAUDE_CODE_OAUTH_TOKEN` is supplied (config env, passthrough, or Keychain injection), containers automatically receive `CLAUDE_CODE_HOST_AUTH_ENV_VAR` and default `CLAUDE_CODE_OAUTH_SCOPES`, and a minimal onboarding-complete `.claude.json` is seeded, so interactive Claude works without a manual `/login`.
- Hostdo rules can now set `env_allowlist = ["NAME", ...]` to run their command from a cleared environment containing only a small base set (`PATH`, `HOME`, locale, etc.) plus the listed variables, instead of inheriting the manager's full host environment. Not supported on `image` rules, which already start from a clean container environment.
- `mirror_cwd = true` opt-in in global or project `harness-rules.toml`: sessions mount an absolute POSIX workspace source at the identical path in the Linux container, rather than `/workspace`. `mount_cwd` launch sources are confined to the configured workspace before mounting. Native Windows paths retain the configured mount target because they cannot be represented exactly in Linux containers. New workspaces receive a default `harness-rules.toml` in their root so this per-workspace option is available immediately.
- `hht workspace --list` prints configured workspace names, paths, and saved templates without requiring Docker or the background agent to be running.

### Changed
- **Breaking:** removed the public `hht --config PATH` option. The normal CLI uses its standard config discovery flow, while the installed `hht-daemon` keeps its private fixed config path.
- Workspace template selections are now stored in the workspace root's `harness-rules.toml`; an optional `[[workspaces]].template` in `harness-hat.toml` overrides the workspace-local choice.
- Updated Rust dependencies to current compatible releases, including Axum 0.8, Reqwest 0.13, Tokio-compatible OpenTelemetry 0.32, TOML 1.1, and the latest TUI, terminal, utility, and native-dialog crates. The minimum supported Rust version is now 1.89.
- **Breaking:** `bypass_proxy` configuration has been replaced by `allowed_hosts` for specifying hosts that are automatically allowed without network approval. The field is now supported at the defaults level and per-container profile in `harness-hat.toml`, and no longer requires setting `NO_PROXY` environment variables.
- **Breaking:** the manager binary is renamed `harness-hat-manager` → `hht` (a single binary). Running `hht` with no subcommand launches the interactive workspace manager. The `hh` name is avoided because it resolves to the built-in Microsoft HTML Help executable on Windows.
- **Breaking:** Harness Hat now uses a workspace + Docker-template session model; sessions are shell-first Docker containers. The previous command-passthrough / host-command-control model is gone.
- **Breaking:** config section `[defaults.hostdo]` is replaced by `[defaults.control]` (the authenticated lifecycle/control server used by `killme`). The hostdo execution-policy keys (`max_timeout_secs`, `denied_executables`, `hostdo_block_common`, `denied_argument_fragments`, `command_aliases`, and per-workspace `hostdo` overrides) are no longer recognized.
- `hostdo` is no longer exposed as an `hht hostdo` subcommand; the supported interface is again the standalone in-container `hostdo` command mounted into managed containers.
- Workspace `harness-rules.toml` hostdo entries now support exact `image` and `timeout_secs` fields in addition to `argv`, `cwd`, and `approval_mode`, so remembered allow/deny decisions can persist image-backed and custom-timeout hostdo requests exactly.
- **Breaking:** example container profiles are reorganized from agent-centric names (`claude`, `codex`, `gemini`, `pi`) to language/size templates (`typescript`, `go`, `rust`, `php`, `base`, `large`).
- **Breaking:** the example config now ships with `server_host = "127.0.0.1"` and `proxy_host = "127.0.0.1"`. The manager refuses non-loopback binds unless `allow_remote_control = true` is set explicitly.
- **Breaking:** the base Docker image no longer installs Claude Code from npm or executes the mutable native `latest` installer. It uses Anthropic's signed stable apt repository, exposes the binary as both `claude` and `claude-stable`, and disables in-session auto-updates; image rebuilds are the only upgrade path.
- The base image installs Antigravity CLI (`agy`) from the official GitHub release artifacts instead of installing Gemini CLI from npm, and includes an `agy-yolo` wrapper for `agy --dangerously-skip-permissions`.
- Containers now run as user `coder` (uid 1000) across all templates.
- Mount-seed detection consolidated onto `ContainerMount::is_seeded()`, shared by container launch and the TUI.
- Top-level config structs now reject unknown fields (`#[serde(deny_unknown_fields)]`), surfacing typos and removed options instead of silently defaulting.
- README rewritten around the new workspace/template/shell workflow.

### Removed
- `docker/build.sh` (replaced by `hht rebuild [--no-cache] [TEMPLATE...]`).
- **Breaking:** root certificate injection and MITM TLS support (`src/ca.rs`, CA field from `ProxyState`, transparent TLS handler). Containers can no longer intercept HTTPS traffic; CONNECT requests now tunnel directly without certificate signing. This simplifies the proxy architecture and removes the need for injecting self-signed CAs into containers.
- **Breaking:** `bypass_proxy` configuration field (migrated to `allowed_hosts`). Use `allowed_hosts` in `harness-hat.toml` instead for hosts that should be automatically allowed without network approval.
- HTTP request type filtering and request-smuggling protections (H4 method validation, CR3 line-ending and body-framing validation). The proxy now accepts all HTTP methods and simplified body reading.
- **Breaking:** the `agentctl` subagent spawn/control system (`agentctl spawn`/`status`/`tail`/`send`/`stop`) and its TUI integration (`docker/scripts/agentctl.py`, `src/agents.rs`, `src/tui/app/agents.rs`).
- The legacy substring-based container-ID match in the stop endpoint.
- Per-request `reqwest::Client` rebuild on the proxy hot path (now cached on `ProxyState`).
- The unauthenticated legacy root proxy listener; only authenticated per-session proxy listeners are now bound.
- The unused PWA scaffold, source-export helper, and tracked OS/Python generated artifacts.

### Security
- External changes to global or workspace `harness-rules.toml` now immediately block new proxy and hostdo policy decisions. Harness Hat opens a native system dialog; only an explicit trust of the reviewed current file version clears the block. Closing, cancelling, or failing to display the dialog remains blocked.
- `claude_settings` sources now use the same path expansion and sensitive-path refusal as ordinary mounts. Invalid `~user` paths now fail configuration loading instead of being silently ignored.
- Oh My Zsh is installed from a pinned, checksum-verified source archive rather than by executing its mutable remote installer during image builds.
- Bearer-token and proxy-authorization comparisons now use constant-time equality.
- Incoming hostnames are now canonicalized (lowercase, trailing-dot strip, IDNA) before rule matching on the plain-HTTP, CONNECT, and SNI paths; deny rules can no longer be bypassed via case, trailing dot, or punycode. CONNECT requests whose host fails canonicalization are rejected with 400.
- Path matching now strips the query string, percent-decodes, and collapses `..`/`//` before evaluation.
- `Connection:`-listed header tokens are stripped before forwarding.
- Plain-HTTP `Host:` validation rejects duplicate and missing Host headers.
- SNI host names that are non-ASCII or invalid UTF-8 are rejected instead of being lossily decoded into a bogus policy key.
- Shared session-state mounts (`~/.claude`, `~/.codex`, …) are skipped when the host source does not exist instead of asking Docker to bind a missing path.
- An explicit network denylist match now wins over `allowed_hosts` on both proxy paths; previously `allowed_hosts` silently outranked the denylist.
- The `/exec` endpoint is gated by a dedicated concurrency semaphore (cap 32, fast-fail 503); jobs are scoped to their originating session, stdin uses bounded backpressure, output uses fixed-size chunked capture, and finished jobs are constrained by count, byte budget, and TTL.
- Sensitive-path refusal now unconditionally covers broad roots, system trees, Docker sockets, and common credential directories for both workspaces and configured mounts.
- Container-supplied hostdo `--image` references are validated (non-empty, no leading `-`, Docker-reference charset) and a `--` separator precedes the image in the `docker run` argv, so crafted values can't be parsed as flags.
- Hostdo children never inherit harness-hat's control-plane secrets: all `HARNESS_HAT_*` variables (bearer token, session token, scoped-proxy auth) are stripped from the child environment.
- A container-supplied hostdo `timeout_secs` is now clamped to the matched rule's own timeout (the rule value is a ceiling, not just a fallback) and to an absolute 6-hour maximum.
- Supply-chain hardening across the Docker images: the Composer installer is verified against its published sha384 signature; Claude Code comes only from its signed apt repository; Bun, rustup-init, and the Go toolchain are pinned and checksum-verified; and all `npm -g`, `go install`, `cargo install`, and `composer global` tools are pinned to exact versions.
- Container capability lockdown: every launch sets `--security-opt no-new-privileges`; non-strict Linux launches run with `--cap-drop ALL`; strict-mode Linux launches now also run `--cap-drop ALL` and re-add only the verified minimal set (`NET_ADMIN` for iptables/tun setup, `SETUID`/`SETGID` for the init's downward `gosu` drop).
- Fixed the IPv6 6to4 SSRF check, which previously matched all addresses due to a tautological predicate; all `2002::/16` (6to4) destinations are now correctly restricted.
- IPv6 SSRF predicate extended to cover NAT64, 6to4, IPv4-translated, and discard-only prefixes.
- DNS cache bounded (LRU) and case-normalized.
- Config `instance_id` write-back and workspace-block append are now atomic (tmp + fsync + rename) under an advisory file lock.
- Audit log file and `log_dir` are now created with `0o600` / `0o700` permissions atomically (no chmod-after race) and refuse to follow symlinks.
- Workspace path canonicalization runs before validation, and canonical paths under `~/.ssh`, `~/.gnupg`, and `/etc` are refused.
- Built-in Dockerfile templates are now written from compiled-in content rather than fetched at runtime over plain HTTP.
- Mount paths and container paths reject embedded `:` and `,` characters to prevent ambiguous `-v` argument parsing.
- Approval "Allow forever" / "Deny forever" decisions now require an unambiguous source workspace; the silent sidebar-fallback was removed.
- Stop endpoint exact-matches the requesting session's container ID rather than accepting substring prefixes.
- Control endpoints now enforce body-size, request-timeout, and concurrency limits.
- The Claude Code OAuth refresh token is no longer injected into containers — neither as `CLAUDE_CODE_OAUTH_REFRESH_TOKEN` nor inside the seeded `~/.claude.json`'s `claudeAiOauth` block. Containers can't write a refreshed token back to the host's macOS Keychain, so allowing in-container refresh would rotate (and invalidate) the host's refresh token while the new token died with the container, breaking auth on the next launch. Sessions in the container are now bounded by the access token lifetime; re-run Claude locally to refresh the Keychain.

### Fixed
- Explicit Claude auth env vars now take precedence over macOS Keychain injection, which remains only a fallback when no setup-token/API-key env var is supplied. Seeded Claude Code config for env-token auth also strips stale `oauthAccount` metadata in addition to `claudeAiOauth`, so interactive Claude falls back to the env token instead of short-lived stored OAuth state.
- Remembered hostdo approvals are again written to the originating workspace's `harness-rules.toml` and honored on subsequent exact command requests. The approval is only reported as remembered when that project-local write succeeds, and the duplicate server-side write path has been removed.
- The TUI no longer hard-freezes when the terminal emulator stops draining output (e.g. stalled by Teams/Zoom screen sharing). Rendering now goes through a dedicated stdout writer thread that drops frames instead of blocking when the terminal stalls, then forces a full repaint on recovery; and the TUI runs on its own thread with a dedicated runtime, so the control server and network proxy keep serving containers even while the TUI is stalled or busy in a synchronous docker call.
- Generated starter `harness-rules.toml` files once again document the hostdo rule model, including exact command examples and image-backed hostdo examples.
- Build tasks can now be cancelled (cooperative flag + task abort) on TUI quit.
- Pressing Esc or `h` on the build pane while a build is running now returns to the sidebar without canceling the build; press `C` to cancel a running build.
- Waiting sessions in the sidebar no longer show a `?` indicator to the left of their title.
- Added `localhost_forwards` for port 8081 to the `[defaults.containers]` example. The 0.8.0 restructure removed the agent-centric container profiles (including the pi profile that documented this forward); Pi's persistent `~/.pi/agent/models.json` still points to `http://localhost:8081/v1` but strict-network containers had no socat forwarder on that port, refusing every model call.
- The control server and proxy listener tasks are now aborted when the TUI loop exits, before telemetry shutdown, instead of leaking until process exit.
- TUI activity queue is bounded (drop on overflow with a debug log); the in-memory activity list is capped to prevent unbounded growth from long-running or never-completing entries.
- Rules-scan worker thread now resets its in-flight flag even on panic.
- TUI build pane error-detection heuristic no longer over-matches benign output containing the substring `error`.
- Container alias allocation now bails on docker errors rather than silently risking collisions.
- `loopback_to_host_docker` no longer appends a trailing slash when rewriting `HARNESS_HAT_URL` to `host.docker.internal`. The `url::Url` round-trip introduced during the hardening normalized `http://host:7878` to `http://host:7878/`; the container's strict-network init parses the port with naive shell, so the stray slash produced port `7878/`, failed `iptables`, and killed the container at startup (exit 2).
- `hht shell` no longer leaves the host terminal in a broken state when the container exits out from under it. The CLI now stays as the parent of `docker exec` (instead of `exec()`-ing into it), ignores `SIGINT`/`SIGQUIT`/`SIGTSTP` so they forward to the container, and on exit emits resets for focus reporting, bracketed paste, all mouse-reporting modes (X10, button-event, any-event, SGR, urxvt), cursor visibility, alternate screen, line wrap, and SGR attributes that an inner program (bash readline, vim, fzf, custom prompt) may have enabled but never gotten to disable. Restoration is skipped when stdout is not a TTY so the resets don't pollute piped output.
- Hostdo strict-network startup path discovery now avoids virtual DNS relay IPs (`198.18.x.x`) and prefers direct route/gateway targets from `/proc/net/route` first, preventing connection failures before the first request is made.
- Hostdo exec-job URLs use Axum 0.8 route syntax, resolving `/exec/jobs/{id}` correctly again.
- Added regression tests that lock in both behaviors: Axum 0.8 route compatibility for `/exec/jobs/<uuid>` responses and hostdo candidate base URL ordering/filtering in strict-network mode.
- Removed a duplicate container-keyrings mount point from the default mounts that broke container launch on macOS.
- Workspace and template scanning no longer freezes the TUI.
- The container-template picker now caches its template list while open and wraps Up/Down navigation at the top and bottom, eliminating the laggy boundary key-repeat behavior.
- Marketplace plugins (e.g. pgsd and its slash commands) load in-container again: non-seeded mounts whose host path differs from the container path are re-exposed read-only at their own host-absolute path, so the absolute `installPath` strings agent CLIs record (e.g. `/Users/<you>/.claude/plugins/...`) resolve under the container's `/home/coder` home. Seeded mounts stay private and are not re-exposed.
- Interactive Claude Code no longer 401s ("Please run /login") when launch auth comes from `CLAUDE_CODE_OAUTH_TOKEN`: the host's stale `.credentials.json` is shadowed with an empty seed and stale `claudeAiOauth` blocks are stripped from the seeded `.claude.json`, so the TUI falls back to the env token like `claude -p` does.


## 0.7.0 Jun 1, 2026

### Added
- `hostdo run`, `hostdo list`, `hostdo status`, `hostdo tail`, and `hostdo stop` now provide a tracked host-side process workflow so agents can inspect output after launch instead of relying only on the initial terminal stream.
- `hostdo tail` now supports `--rows <lines>`, `--all`, `--stdout`, `--stderr`, and `--json`, and `hostdo send` can forward input to tracked jobs.
- `hostdo list` now includes a `CONTAINER` column.
- `hostdo list --running` now filters output to only active hostdo jobs.
- Activity detail panes now support scroll mode with `Ctrl+S` and terminal-style navigation keys.

### Changed
- Updated dockerfiles.
- **Breaking:** hostdo is now subcommand-only: use `hostdo run ...` for command execution; direct passthrough forms like `hostdo cargo test` and `hostdo --image ...` were removed.
- **Breaking:** hostdo orchestration commands now mirror `agentctl` verbs: `read` was renamed to `tail`, `kill` was renamed to `stop`, `hostdo tail` defaults to 24 rows, and `hostdo send`/`hostdo stop` now emit JSON responses.
- `hostdo run` now emits `Waiting for developer approval... (Xs)` notices every 10 seconds while approval is pending.
- Running activity status text now uses the same light blue tone as the sidebar instead of yellow.

### Fixed
- Cancelling a sidebar `hostdo` task now terminates the command's full process group so shell or Docker child processes do not remain running after cancellation.
- `hostdo` command timeouts now still apply while draining output from processes whose parent has exited but whose descendants kept stdout or stderr open.
- Sidebar scrolling now keeps the first workspace title visible near the top; selecting the second sidebar row resets the sidebar scroll offset to the top.


## 0.6.0 May 18th, 2026

### Added
- `[defaults.hostdo].hostdo_block_common` now lets config override a built-in blocklist of common shell/file utilities that should not be run through `hostdo`.
- `hostdo --help` now prints detailed usage, timeout and image forms, hostdo policy guidance, rule examples, blocked-command guidance, and approval-wait guidance, and it points agents at project `harness-rules.toml` files for current allowlists and aliases.
- Generated starter `harness-rules.toml` files now document `hostdo --timeout` usage for commands that need an explicit host-side timeout.
- Workspaces can now persist `sidebar_hotkey` assignments in `harness-hat.toml`, with deterministic hotkey assignment for newly created workspaces.
- Hostdo activity detail panes now show the effective command CWD.
- The default Docker image now installs `pnpm`, `typescript`, and `tsx` alongside the bundled agent CLIs.

### Changed
- Prompted `hostdo` requests now enter the exec job protocol immediately and emit `Waiting for developer approval... (20s)` while a developer approval modal is pending.
- Sidebar workspace hotkeys now use bare `a-z0-9` keys while the sidebar is focused, jump to the first selectable child row in that workspace section, hide their badges outside sidebar focus, and no longer compete with sidebar-only letter bindings. Sidebar navigation now uses arrow keys and `Enter`, and log fullscreen moved to `Alt+O`.
- Mouse-wheel viewport scrolling in terminal panes is now twice as fast.
- Approval and confirm modals now require `Ctrl+...` shortcuts instead of bare `y/n/r/d`, `Enter`, or `Esc` so typing into an agent cannot accidentally approve or deny a request.

### Fixed
- Approval modals are now global across workspaces and remain visible in sidebar previews, terminal fullscreen, and log fullscreen views instead of only appearing in the originating workspace.
- `hostdo` now hard-denies common shell/file utilities such as `ls`, `cat`, `grep`, `find`, and `rm`, steering agents toward host-side build, package, compiler, and test tooling.
- Proxy tests now use unique temporary CA directories to avoid cross-test contamination.


## 0.5.0 May 11th, 2026

### Added
- Container profiles can now set `mouse_scroll = "auto"`, `"harness"`, or `"agent"` to control whether mouse wheel events scroll Harness Hat history or pass through to the inner agent TUI.
- Container profiles can now define fixed environment variables with `env = { NAME = "value" }`.
- Container profiles can now define `localhost_forwards` entries that expose selected host TCP services as `localhost:<port>` inside the container.
- Terminal panes now show a `Ctrl+G` fullscreen hint in both normal and fullscreen terminal views.
- `agentctl list` now reports the configured subagent profiles that the current container can launch.

### Changed
- The bundled OpenCode profile and package have been replaced by a Pi profile using `@earendil-works/pi-coding-agent`, command `pi`, common provider allowlist entries, and a `~/.pi` state mount.
- The default manager proxy port changed from `8081` to `28781` to avoid common local development port conflicts.
- Passthrough launches now honor profile fixed environment variables, mouse scroll routing, and localhost forwards.

### Fixed
- `localhost_forwards` now work with `strict_network` by resolving the Docker host alias before `tun2proxy` starts and allowing only the configured forwarded host ports through the strict egress filter.


## 0.4.0 May 8th, 2026

### Added
- Scoped proxy listeners now require per-session proxy authentication before accepting HTTP or CONNECT traffic.
- Scoped proxy credentials are now propagated into launched containers and strict-network `tun2proxy` setup through env files instead of exposing authenticated proxy URLs as container addresses.
- Proxy DNS guardrails now resolve destinations before forwarding and reject loopback, private, link-local, CGNAT, benchmark, multicast, reserved, and IPv4-mapped IPv6 restricted addresses.
- Proxy forwarding pins the resolved public addresses used for each outbound HTTP(S) request to reduce DNS rebinding exposure.
- HTTPS MITM forwarding now validates the inner `Host` header against the CONNECT/SNI target and rejects duplicate or mismatched `Host` headers.
- Network activity rows are now collapsed into a per-session `Network [X]` group with request navigation, selected-request detail, and selected-request cancellation.
- Agent containers now include an `agentctl` helper for same-workspace subagent spawning and terminal control through `spawn`, `status`, `tail`, `send`, and `stop`.
- Host-side `hostdo` execution now canonicalizes and confines request and rule CWDs to the configured workspace before running commands.
- Proxy tests now cover restricted-address blocking, IPv4-mapped loopback rejection, scoped proxy authentication, Host header mismatch rejection, CONNECT port handling, and oversized request bodies.
- Hostdo/server tests now cover workspace CWD mapping, parent-directory escape rejection, symlink escape rejection, and persisted `port=...` network rules.
- Config/server tests now cover canonical workspace loading, symlinked Docker-runner CWD mapping, and shared Docker env-file validation.
- Strict network mode now configures IPv6 egress blocking when `ip6tables` is available.
- The base Docker image now resolves proxy and exec bridge hosts to IPv4 addresses before starting `tun2proxy`, keeping strict-network control traffic off virtual DNS addresses.
- `hostdo` Docker runners now pass environment profiles through Docker env files instead of process arguments.
- Hostdo Docker runners now validate env-file names and values before writing them.
- Cargo audit coverage is clean after dependency upgrades for the TUI, PTY, OpenTelemetry, and `time` dependency families.
- `[defaults.ui].show_log_pane` can show the bottom TUI log pane, which is hidden by default while fullscreen log view remains available.
- `agentctl tail` now supports `--all` to retrieve all terminal rows retained in the PTY scrollback buffer.
- `agentctl send` now supports `--enter` and paced chunked delivery for longer prompts.
- `agentctl spawn-many` now supports paced subagent launches using `[agentctl].spawn_delay_ms` from `harness-rules.toml`, with a 100ms minimum effective delay.
- `[agentctl].max_subagents` now limits live descendants under a single top-level agent; the default is 10.
- Agent launch argv is now configurable per container profile under `[container_profiles.<name>].command`, allowing overrides such as `["claude", "--dangerously-skip-permissions"]`.
- Container profiles can now define `starter_network_allowlist` entries that are copied into newly created workspace `harness-rules.toml` files.
- `harness-hat.toml` and `harness-rules.toml` now support top-level `version = 1` schema markers for future migrations while treating missing versions as version 1.
- Duplicate pending network approval requests are now merged per workspace, method, host, port, and path so one modal decision can approve or deny all matching simultaneous requests.

### Changed
- Direct-mode workspace handling now uses each workspace's `canonical_path` directly instead of routing through legacy effective sync/workspace helper APIs.
- Workspace `canonical_path` values are now canonicalized during config load so direct mounts, hostdo confinement, and Docker runner CWD mapping use the same real filesystem root.
- Manager-generated workspace config now writes only the direct workspace block and no longer emits ignored `[workspaces.sync]` settings.
- Rules file rendering now always includes the standard header without carrying a dead `is_new` parameter through call sites.
- Hostdo approval persistence now stores the resolved host CWD used for execution, keeping saved rules aligned with the workspace-confined path.
- Manager proxy startup now binds the root proxy to `127.0.0.1:<proxy_port>` instead of inheriting the configurable proxy host.
- CONNECT policy matching is now port-aware: domain-only allow rules auto-allow HTTPS CONNECT on 443, while raw TCP CONNECT on other ports requires an explicit `port=...` rule.
- CONNECT passthrough and raw tunnel paths now run policy and public-address preflight checks before bypassing MITM inspection.
- Plain HTTP and HTTPS forwarding now strip caller-supplied `Host` headers so reqwest derives `Host` from the policy-checked URL.
- Subagent tail responses now read from the terminal scrollback buffer instead of only the visible terminal rows.
- Scoped per-container proxy listeners now cap active connections, and root/scoped proxy paths share a per-session source cap without blocking `tun2proxy`'s own transport sockets.
- Closing or stopping an agent now terminates its descendant subagents immediately.
- Network "always allow" persistence now includes `port=...` for raw non-443 CONNECT decisions.
- Default credential/session mounts in the example config are now commented examples instead of active mounts.
- Container launch now writes scoped proxy, `hostdo`, and Claude token values through flushed, validated env files rather than Docker `-e` arguments where possible.
- Container launch env files and Docker runner env files now share one validator for environment variable names and newline-free values.
- Temporary helper script copies are now created in the system temp directory instead of under `docker/scripts`.
- The default Docker image now installs pinned npm CLI versions, including Claude Code, instead of floating latest packages or downloading Claude Code through a curl installer.
- The base Docker image now builds pinned `tun2proxy` from crates.io and installs NodeSource through a signed apt keyring.
- The PWA dependency set was refreshed, unused UI packages were removed, and PostCSS is pinned through package overrides.
- Cargo package metadata now uses the isolation-focused description and keyword set.
- The minimum supported Rust version is now 1.88.
- Ratatui, OpenTelemetry, PTY/terminal, and related dependency families were upgraded.
- Sidebar network group rows now render as `Network [X]` instead of `X Network`.
- Network group detail panes now use the same `Network [X]` title format.
- Subagent names are parent-local aliases, and the sidebar now renders nested subagent trees recursively.
- Large activity start events now box the activity payload to reduce enum size.
- README and the example config now document that Docker-reachable bind addresses should be narrowed or firewalled on shared networks.
- `agentctl spawn` and `agentctl spawn-many` now accept configured profile names instead of being limited to hardcoded agent names.
- Starter `harness-rules.toml` generation now derives agent API allowlist entries from the selected profile's `starter_network_allowlist` rather than from a separate agent-kind field.
- Codex subagent launches use a shorter MCP diagnostic poll/stability window, and the temporary MCP startup gate no longer blocks the spawn request path.
- Subagent-scoped proxy capacity is now capped more tightly per subagent to avoid one child agent exhausting proxy resources.
- Network approval overlays now show how many matching requests were merged into the current modal.

### Fixed
- `bypass_proxy` can no longer skip network policy decisions for CONNECT or transparent TLS traffic.
- Scoped transparent TLS traffic without matching proxy authentication is now rejected instead of being allowed through the scoped proxy path.
- Raw CONNECT rules no longer allow non-443 ports from a domain-only allow entry.
- HTTPS requests can no longer be approved for one host while forwarding a different inner `Host` header.
- IPv4-mapped IPv6 literals can no longer bypass restricted IPv4 destination checks.
- Strict-network launches on Linux no longer fall back to broad `--privileged` mode when `/dev/net/tun` is unavailable.
- Existing CA private keys now have private file permissions enforced when they are loaded, matching newly generated keys.
- Env profiles now reject invalid environment variable names or values that cannot be represented safely in Docker env files.
- Generated container env-file values can no longer inject additional Docker environment entries via embedded newlines.
- Docker-backed `hostdo --image` commands now preserve workspace-relative runner CWD mapping when the configured workspace path is a symlink.
- High-volume `hostdo` command output can no longer grow an unbounded manager-side queue; stdout/stderr streaming now applies bounded backpressure and stops forwarding lines after the capture cap is reached.
- Cargo clippy warnings introduced by dependency and type-size changes were resolved.
- Codex subagent config snapshots now skip dangling symlinks and live runtime state directories instead of failing the launch.
- `agentctl status`, `tail`, `send`, and `stop` now use shorter control-request timeouts so one stuck subagent request does not hang the caller for the full spawn timeout.
- Pending network approval queues are now bounded; overflow requests are denied instead of allowing modal storms to grow without limit.
- Closing or stopping a subagent now denies and removes its pending network approval requests so stale proxy waiters do not linger.
- Parallel subagents triggering the same network prompt no longer freeze the TUI with duplicate modals.

### Removed
- Removed inert sync/workspace config schema fields (`workspace_path`, per-workspace `sync`, per-workspace `disposable`, per-workspace `default_policy`, `[defaults.sync]`, and `[defaults.workspace]`) that were already ignored by direct-mode runtime behavior.
- Removed dead request fields from the hostdo exec and container stop HTTP payloads.
- Removed no-op rules workspace sync hooks from approval persistence.
- Removed unused direct Rust dependencies on `webpki-roots`, `httparse`, and `portable-pty`.
- Removed the stale generated `docker/scripts/harness-hat-hostdo-8VP3so` helper artifact from the tree.
- Runtime helper artifacts under `docker/scripts/harness-hat-hostdo-*` are now ignored for older launch behavior.
- Removed unused PWA dependencies, including Ark UI and Park UI packages.
- Removed the unused `rustls-pemfile` dependency.
- Removed legacy per-agent config fields from `harness-hat.toml`; profile `command`, mounts, runtime toggles, and `starter_network_allowlist` now carry that behavior.

## [0.3.0] - May 5, 2026

### Added
- Project/package rename from `void-claw` to `harness-hat` across the Rust crate, manager binary, Docker templates, helper scripts, example config, rules file, README, and PWA metadata.
- `hostdo --image <image> ...` support for short-lived Docker runners, with image-specific approval rules and validation for requested Docker image names.
- Automatic Docker image checks for image-backed `hostdo` commands, including pull progress reporting while an image is downloading.
- Long-running `hostdo` job tracking for image-backed commands, including job polling from the `hostdo` helper and cancellable execution.
- Optional `hostdo --timeout <seconds>` requests, persisted `timeout_secs` rule updates, and `[defaults.hostdo].max_timeout_secs` enforcement.
- Streaming terminal output for `hostdo` commands and Docker runners, using the same terminal emulation path as agent terminals.
- Active hostdo and network requests now appear as selectable child rows under their container in the sidebar.
- Hostdo activity detail panes show command, image, timeout, status, elapsed timing, and terminal history; network detail panes show method, domain, path, protocol, payload metadata, payload preview, status, and connection history.
- `Ctrl+C` cancellation for selected in-flight hostdo and network activities.
- Status coloring for activity detail panes and sidebar rows: yellow while running, green for success, and red for failure/cancellation.
- Temporary completion highlighting for finished activity rows, with fading delayed while the row remains selected.
- `[network].denylist` rules for permanent network denies, with deny matches taking precedence over allow matches.
- Persistence for "always deny" network decisions into `harness-rules.toml`.
- Rules-file internal write tracking for manager-generated approvals and starter rules, avoiding false tamper alerts for expected writes.

### Changed
- Hostdo activity titles now show the actual command only, omitting `hostdo` options such as `--image` and `--timeout`.
- Hostdo command timers now measure the command phase only; Docker image checking and pulling are reported separately from the command timeout.
- Hostdo activity elapsed timers stop when the command finishes.
- Docker build and hostdo/detail panes now use more consistent controls, spacing, and footer behavior.
- Sidebar selection now preserves the selected item when activity rows appear, disappear, or fade above it.
- Activity fade timers reset when a fading row is selected again.
- The completion bell indicator is only restored for terminal bell events emitted by an agent.
- Network rule counts in the UI now include both allowlist and denylist entries.

### Fixed
- Hostdo detail panes now show both stdout and stderr instead of only stderr.
- Selected completed activity rows remain visible until selection moves away.
- Image-backed `hostdo` commands no longer make image download time appear to breach the command timeout.
- Docker build panes no longer advertise inactive `[c]` or `[r]` footer shortcuts.
- Network "always deny" approvals now create explicit persisted rules instead of relying on implicit prompt/default behavior.

## [0.2.0] - April 14, 2026

### Added
- Host command alias `cwd` resolution supports `$WORKSPACE` with subdirectories (for example: `$WORKSPACE/some-dir`).
- Tests for alias/cwd resolution and direct-mode behavior were expanded (including workspace alias parsing and mount/cwd mapping behavior).
- New binary split:
  - `void-claw-manager` for the interactive TUI manager.
  - `void-claw` for command passthrough (`void-claw -- ...`).
- Passthrough image selection via Dockerfile stem (`--image <name>` -> `<docker_dir>/<name>.dockerfile`) with explicit missing-file error messaging.
- New Docker templates:
  - `docker/void-claw-base.dockerfile`
  - `docker/default.dockerfile`

### Changed
- Terminology across the product has been updated from **Projects** to **Workspaces** in the TUI, docs, and config model.
- Config now supports `[[workspaces]]` as the primary key, while retaining compatibility with legacy `[[projects]]`.
- Runtime behavior is now direct-only: effective mount/workspace paths resolve to the canonical path, and sync mode resolves to `direct`.
- `hostdo`/rules cwd placeholders were consolidated to `$WORKSPACE` only; `$CANONICAL` references were removed from templates, tests, and examples.
- **Breaking:** network policy schema now uses Coder-style `[network].allowlist` entries (`method=... domain=... path=...`) with prompt-by-default matching; legacy `[[network.rules]]` entries are rejected.
- **Breaking:** `exclude_patterns` and `global_exclude_patterns` are no longer parsed from config/rules TOML files.
- **Breaking:** launch model is now profile-only. `container_profiles` are direct launch targets and legacy `[[containers]]` entries are rejected.
- **Breaking:** `container_profiles.<name>.image` now uses Dockerfile stem resolution (`<docker_dir>/<stem>.dockerfile`) rather than pre-baked per-agent image tags.
- Manager build/launch behavior now resolves images from Dockerfile stems consistently with passthrough CLI behavior.
- Fullscreen terminal hint text for `Ctrl+G` was removed from the UI chrome.
- README and sample config were updated to document direct mode and workspace-first naming.
- Repository/product naming has been aligned to `void-claw`.

### Removed
- Workspace mirroring and file-sync workflow from the TUI and runtime loop.
- The legacy sync subsystem (`src/sync`) and watcher-driven sync codepaths.
- Unused `walkdir` dependency and stale sync-related code.
- Obsolete `src-files-dump.md` artifact.
- Legacy per-agent Dockerfile subdirectories under `docker/`.
- Legacy `docker/ubuntu-24.04.Dockerfile` base filename (replaced by `docker/void-claw-base.dockerfile`).

## [0.1.0]
- Initial release.

# Unattended Cross-Platform Installation

## Summary

Add native unattended installers that provision Docker, install Harness Hat
release binaries, and register its per-user background agent for:

- macOS: ARM64 and AMD64
- Linux: ARM64 and AMD64
- Windows: AMD64

Default to the latest GitHub release, with an explicit version override.
Existing Harness Hat installations are skipped unless `--force` is supplied.

## Implementation changes

- Add `scripts/install-macos.sh`, `scripts/install-linux.sh`, and
  `scripts/install-windows.ps1`.
  - Resolve `latest` from GitHub Releases over HTTPS; accept a version
    argument/environment override.
  - Detect the supported OS/CPU pair and download/extract the matching archive
    containing both `hat` and `hat-daemon`.
  - Install into the active console user's per-user PATH location:
    - macOS/Linux: `~/.local/bin`
    - Windows: `%LOCALAPPDATA%\\HarnessHat\\bin`
  - Add the directory to the user PATH when needed.
  - Default to skip when `hat` already exists; `--force` replaces both
    binaries and reruns `hat install`.
  - Fail clearly when there is no single active console user or the
    platform/architecture is unsupported.

- Provision Docker before Harness Hat when it is absent.
  - macOS: download the official architecture-specific Docker Desktop DMG; run
    its installer with `--accept-license --user=<active-user>`, then launch it.
  - Windows: enable/update WSL 2, install the official x64 Docker Desktop
    installer silently with Linux containers/WSL2 and accepted license, then
    start Docker Desktop.
  - Linux: use Docker's official unattended installer for Debian/Ubuntu,
    Fedora/RHEL, and Arch-family hosts; enable the service and add the active
    user to the `docker` group.
  - If Docker needs reboot, logout, or additional startup time, continue with
    Harness Hat installation and report that Docker must be ready before first
    workspace use.

- Add installer flags:
  - `--version <vX.Y.Z>` / environment equivalent
  - `--force`
  - Linux-only `--headless`, passed through to `hat install --headless`;
    desktop mode is the default.
  - Elevated operations target the single active console user rather than
    root/Administrator.

- Expand `.github/workflows/release.yml` to publish archives for:
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
  - `x86_64-unknown-linux-gnu`
  - `aarch64-unknown-linux-gnu`
  - `x86_64-pc-windows-msvc`
  - Preserve the archive naming convention consumed by installers and package
    both executables in every artifact.

- Update the README and setup guide with copy-paste commands for each
  installer, flags, Docker licensing/elevation expectations, supported
  architectures, and the distinction between initial install, skip-existing
  behavior, and `--force`.

## Test plan

- Add shell/PowerShell-focused tests or mocked command tests for
  platform/architecture selection, version resolution, archive URLs,
  user-path construction, existing-install skip behavior, `--force`, and
  Linux `--headless`.
- Validate release workflow matrix/archive names match every installer mapping.
- Manually smoke-test each installer path in clean macOS ARM64/AMD64, Linux
  ARM64/AMD64, and Windows AMD64 environments, including Docker-already-present,
  Harness-Hat-already-present, and Docker-not-yet-ready cases.

## Assumptions

- Release downloads rely on HTTPS/TLS only; no checksum or signature
  verification is added.
- Rust and Cargo are not installed because releases are prebuilt.
- Docker Desktop licensing is accepted by the desktop installers as requested.

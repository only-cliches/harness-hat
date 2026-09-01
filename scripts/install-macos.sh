#!/usr/bin/env bash
# Install Docker Desktop and Harness Hat for the active macOS console user.
set -euo pipefail

readonly REPOSITORY='only-cliches/harness-hat'
VERSION='latest'
FORCE=0

usage() {
  cat <<'EOF'
Usage: install-macos.sh [--version VERSION] [--force]

Installs Docker Desktop when needed, then installs Harness Hat for the active
console user. Existing Harness Hat binaries are left unchanged unless --force
is supplied.
EOF
}

die() {
  printf 'harness-hat installer: %s\n' "$*" >&2
  exit 1
}

privileged() {
  if [[ $(id -u) -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || die '--version requires a value'
      VERSION=$2
      shift 2
      ;;
    --force)
      FORCE=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ $(uname -s) == 'Darwin' ]] || die 'this installer is for macOS only'
command -v curl >/dev/null || die 'curl is required to download installers'

TARGET_USER=$(stat -f '%Su' /dev/console)
[[ -n "$TARGET_USER" && "$TARGET_USER" != 'root' && "$TARGET_USER" != 'loginwindow' ]] \
  || die 'no active console user was found'
TARGET_GROUP=$(id -gn "$TARGET_USER")
TARGET_HOME=$(dscl . -read "/Users/$TARGET_USER" NFSHomeDirectory | awk '{print $2}')
[[ -n "$TARGET_HOME" ]] || die "cannot determine the home directory for $TARGET_USER"
TARGET_BIN="$TARGET_HOME/.local/bin"

run_as_target() {
  if [[ $(id -u) -eq 0 ]]; then
    sudo -H -u "$TARGET_USER" "$@"
  else
    "$@"
  fi
}

resolve_version() {
  if [[ "$VERSION" != 'latest' ]]; then
    case "$VERSION" in
      v*) printf '%s\n' "$VERSION" ;;
      *) printf 'v%s\n' "$VERSION" ;;
    esac
    return
  fi
  local latest_url
  latest_url=$(curl --fail --silent --show-error --location --output /dev/null \
    --write-out '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest")
  [[ "$latest_url" == */releases/tag/v* ]] || die 'could not resolve the latest Harness Hat release'
  printf '%s\n' "${latest_url##*/}"
}

ensure_user_path() {
  local profile line
  line="export PATH=\"\$HOME/.local/bin:\$PATH\""
  for profile in "$TARGET_HOME/.zprofile" "$TARGET_HOME/.profile"; do
    privileged touch "$profile"
    if ! privileged grep -Fqx "$line" "$profile"; then
      printf '%s\n' "$line" | privileged tee -a "$profile" >/dev/null
    fi
    privileged chown "$TARGET_USER:$TARGET_GROUP" "$profile"
  done
}

ensure_docker() {
  if run_as_target env PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin" docker version >/dev/null 2>&1; then
    printf 'Docker is already installed.\n'
    return
  fi

  local docker_arch docker_dmg mount_path
  case "$(uname -m)" in
    arm64) docker_arch='arm64' ;;
    x86_64) docker_arch='amd64' ;;
    *) die "unsupported macOS architecture: $(uname -m)" ;;
  esac
  docker_dmg="$WORK_DIR/Docker.dmg"
  mount_path='/Volumes/Docker'
  printf 'Installing Docker Desktop for macOS %s...\n' "$docker_arch"
  curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
    --output "$docker_dmg" "https://desktop.docker.com/mac/main/$docker_arch/Docker.dmg"
  privileged hdiutil attach -nobrowse -readonly "$docker_dmg" >/dev/null
  if [[ ! -x "$mount_path/Docker.app/Contents/MacOS/install" ]]; then
    privileged hdiutil detach "$mount_path" >/dev/null || true
    die 'Docker Desktop installer was not found in the downloaded disk image'
  fi
  privileged "$mount_path/Docker.app/Contents/MacOS/install" --accept-license --user="$TARGET_USER"
  privileged hdiutil detach "$mount_path" >/dev/null
  run_as_target open -a Docker || true

  local attempt=0
  while (( attempt < 45 )); do
    if run_as_target env PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin" docker version >/dev/null 2>&1; then
      printf 'Docker Desktop is ready.\n'
      return
    fi
    ((attempt += 1))
    sleep 2
  done
  printf 'Docker Desktop was installed but is not ready yet; continue after it finishes starting.\n' >&2
}

install_harness_hat() {
  if [[ -x "$TARGET_BIN/hat" && $FORCE -ne 1 ]]; then
    printf 'Harness Hat is already installed at %s; use --force to replace it.\n' "$TARGET_BIN/hat"
    return
  fi

  local release archive hat_arch extract_dir
  case "$(uname -m)" in
    arm64) hat_arch='aarch64-apple-darwin' ;;
    x86_64) hat_arch='x86_64-apple-darwin' ;;
    *) die "unsupported macOS architecture: $(uname -m)" ;;
  esac
  release=$(resolve_version)
  archive="hat-$hat_arch.tar.gz"
  extract_dir="$WORK_DIR/harness-hat"
  printf 'Installing Harness Hat %s...\n' "$release"
  curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
    --output "$WORK_DIR/$archive" \
    "https://github.com/$REPOSITORY/releases/download/$release/$archive"
  mkdir -p "$extract_dir"
  tar -xzf "$WORK_DIR/$archive" -C "$extract_dir"
  [[ -f "$extract_dir/hat" && -f "$extract_dir/hat-daemon" ]] \
    || die 'release archive does not contain both Harness Hat executables'
  privileged install -d -m 0755 -o "$TARGET_USER" -g "$TARGET_GROUP" "$TARGET_BIN"
  privileged install -m 0755 -o "$TARGET_USER" -g "$TARGET_GROUP" "$extract_dir/hat" "$TARGET_BIN/hat"
  privileged install -m 0755 -o "$TARGET_USER" -g "$TARGET_GROUP" "$extract_dir/hat-daemon" "$TARGET_BIN/hat-daemon"
  ensure_user_path
  run_as_target env PATH="$TARGET_BIN:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin" "$TARGET_BIN/hat" install
}

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
ensure_docker
install_harness_hat
printf 'Harness Hat installation complete for %s.\n' "$TARGET_USER"

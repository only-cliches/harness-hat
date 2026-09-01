#!/usr/bin/env bash
# Install Docker Engine and Harness Hat for one active Linux user.
set -euo pipefail

readonly REPOSITORY='only-cliches/harness-hat'
VERSION='latest'
FORCE=0
HEADLESS=0

usage() {
  cat <<'EOF'
Usage: install-linux.sh [--version VERSION] [--force] [--headless]

Installs Docker Engine when needed, then installs Harness Hat for the single
active login user. Desktop mode is the default; use --headless on servers.
Existing Harness Hat binaries are left unchanged unless --force is supplied.
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
    --headless)
      HEADLESS=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ $(uname -s) == 'Linux' ]] || die 'this installer is for Linux only'
command -v systemctl >/dev/null || die 'Harness Hat requires systemd user services'
if ! command -v curl >/dev/null && ! command -v wget >/dev/null; then
  die 'curl or wget is required to download installers'
fi

find_target_user() {
  local session user active candidate
  local -a users=()
  if command -v loginctl >/dev/null; then
    while read -r session; do
      [[ -n "$session" ]] || continue
      user=$(loginctl show-session "$session" -p Name --value 2>/dev/null || true)
      active=$(loginctl show-session "$session" -p Active --value 2>/dev/null || true)
      [[ "$active" == 'yes' && -n "$user" && "$user" != 'root' ]] || continue
      for candidate in "${users[@]:-}"; do
        [[ "$candidate" == "$user" ]] && continue 2
      done
      users+=("$user")
    done < <(loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}')
  fi
  if [[ ${#users[@]} -eq 1 ]]; then
    printf '%s\n' "${users[0]}"
    return
  fi
  if [[ ${#users[@]} -gt 1 ]]; then
    die 'more than one active login user was found; run the installer from the intended user session'
  fi
  if [[ $HEADLESS -eq 1 && -n ${SUDO_USER:-} && ${SUDO_USER:-} != 'root' ]]; then
    printf '%s\n' "$SUDO_USER"
    return
  fi
  if [[ $HEADLESS -eq 1 && $(id -u) -ne 0 ]]; then
    id -un
    return
  fi
  die 'no single active login user was found; use --headless from the target user SSH session'
}

TARGET_USER=$(find_target_user)
TARGET_UID=$(id -u "$TARGET_USER")
TARGET_GROUP=$(id -gn "$TARGET_USER")
TARGET_HOME=$(getent passwd "$TARGET_USER" | cut -d: -f6)
[[ -n "$TARGET_HOME" ]] || die "cannot determine the home directory for $TARGET_USER"
TARGET_BIN="$TARGET_HOME/.local/bin"

run_as_target() {
  if [[ $(id -u) -eq 0 ]]; then
    if command -v sudo >/dev/null; then
      sudo -H -u "$TARGET_USER" "$@"
    else
      runuser -u "$TARGET_USER" -- "$@"
    fi
  else
    "$@"
  fi
}

download() {
  local output=$1 url=$2
  if command -v curl >/dev/null; then
    curl --fail --silent --show-error --location --retry 5 --retry-all-errors --output "$output" "$url"
  else
    wget --quiet --output-document="$output" "$url"
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
  if command -v curl >/dev/null; then
    latest_url=$(curl --fail --silent --show-error --location --output /dev/null \
      --write-out '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest")
  else
    latest_url=$(wget --server-response --spider "https://github.com/$REPOSITORY/releases/latest" 2>&1 \
      | awk '/^  Location:/{location=$2} END {sub(/\r$/, "", location); print location}')
  fi
  [[ "$latest_url" == */releases/tag/v* ]] || die 'could not resolve the latest Harness Hat release'
  printf '%s\n' "${latest_url##*/}"
}

ensure_user_path() {
  local profile="$TARGET_HOME/.profile"
  local line="export PATH=\"\$HOME/.local/bin:\$PATH\""
  privileged touch "$profile"
  if ! privileged grep -Fqx "$line" "$profile"; then
    printf '%s\n' "$line" | privileged tee -a "$profile" >/dev/null
  fi
  privileged chown "$TARGET_USER:$TARGET_GROUP" "$profile"
}

ensure_docker() {
  if run_as_target docker version >/dev/null 2>&1; then
    printf 'Docker is already installed and ready.\n'
    return
  fi
  if command -v docker >/dev/null; then
    printf 'Docker is installed but not ready; leaving the existing installation unchanged.\n' >&2
    return
  fi
  printf 'Installing Docker Engine from Docker\x27s official unattended installer...\n'
  download "$WORK_DIR/get-docker.sh" 'https://get.docker.com'
  privileged sh "$WORK_DIR/get-docker.sh"
  privileged systemctl enable --now docker
  if ! getent group docker >/dev/null; then
    privileged groupadd docker
  fi
  privileged usermod -aG docker "$TARGET_USER"
  printf 'Docker Engine is installed. Log out and back in before Docker group membership takes effect.\n' >&2
}

install_harness_hat() {
  if [[ -x "$TARGET_BIN/hat" && $FORCE -ne 1 ]]; then
    printf 'Harness Hat is already installed at %s; use --force to replace it.\n' "$TARGET_BIN/hat"
    return
  fi
  local release archive hat_arch extract_dir headless_arg
  case "$(uname -m)" in
    aarch64|arm64) hat_arch='aarch64-unknown-linux-gnu' ;;
    x86_64|amd64) hat_arch='x86_64-unknown-linux-gnu' ;;
    *) die "unsupported Linux architecture: $(uname -m)" ;;
  esac
  release=$(resolve_version)
  archive="hat-$hat_arch.tar.gz"
  extract_dir="$WORK_DIR/harness-hat"
  printf 'Installing Harness Hat %s...\n' "$release"
  download "$WORK_DIR/$archive" "https://github.com/$REPOSITORY/releases/download/$release/$archive"
  mkdir -p "$extract_dir"
  tar -xzf "$WORK_DIR/$archive" -C "$extract_dir"
  [[ -f "$extract_dir/hat" && -f "$extract_dir/hat-daemon" ]] \
    || die 'release archive does not contain both Harness Hat executables'
  privileged install -d -m 0755 -o "$TARGET_USER" -g "$TARGET_GROUP" "$TARGET_BIN"
  privileged install -m 0755 -o "$TARGET_USER" -g "$TARGET_GROUP" "$extract_dir/hat" "$TARGET_BIN/hat"
  privileged install -m 0755 -o "$TARGET_USER" -g "$TARGET_GROUP" "$extract_dir/hat-daemon" "$TARGET_BIN/hat-daemon"
  ensure_user_path
  headless_arg=()
  if [[ $HEADLESS -eq 1 ]]; then
    headless_arg=(--headless)
  fi
  run_as_target env HOME="$TARGET_HOME" \
    XDG_RUNTIME_DIR="/run/user/$TARGET_UID" \
    DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$TARGET_UID/bus" \
    PATH="$TARGET_BIN:/usr/local/bin:/usr/bin:/bin" \
    "$TARGET_BIN/hat" install "${headless_arg[@]}"
}

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
ensure_docker
install_harness_hat
printf 'Harness Hat installation complete for %s.\n' "$TARGET_USER"

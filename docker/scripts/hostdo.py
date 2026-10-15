#!/usr/bin/env python3
"""
hostdo — harness-hat container-side command bridge (Python implementation).

Routes commands through the harness-hat host execution server for policy
enforcement and developer approval.  Requires only the Python 3 standard
library — no third-party packages.

Environment variables:
  HARNESS_HAT_URL      Base URL of the harness-hat manager (default: http://127.0.0.1:7878)
  HARNESS_HAT_SESSION_TOKEN  Per-session bearer token injected by harness-hat (required)

Usage:
  hostdo output [--image <docker-image>] [--timeout <seconds>] [--reason <text>] <command> [args...]
  hostdo run [--image <docker-image>] [--timeout <seconds>] [--reason <text>] <command> [args...]
  hostdo list [--running] [--json]
  hostdo status <job-id>
  hostdo tail <job-id> [--rows <lines>|--all] [--stdout|--stderr] [--json]
  hostdo send [--no-newline] <job-id> [text...]
  hostdo stop <job-id>
  hostdo --help

Exit code mirrors the executed command; exits 1 on infrastructure errors.

Requires Python 3 (stdlib only — no third-party packages).
"""

import json
import os
import socket
import sys
import time
import urllib.request
import urllib.error
import urllib.parse

# 6-minute timeout: 5-minute approval window + headroom for slow commands.
_TIMEOUT = 360
_MAX_COMMAND_TIMEOUT_SECS = 5 * 60
_APPROVAL_WAIT_NOTICE_SECS = 10

_HELP_TEXT = """hostdo — harness-hat container-side command bridge

Routes commands through the harness-hat host execution server for policy
enforcement and developer approval.

Usage:
  hostdo output [--image <docker-image>] [--timeout <seconds>] [--reason <text>] <command> [args...]
  hostdo run [--image <docker-image>] [--timeout <seconds>] [--reason <text>] <command> [args...]
  hostdo list [--running] [--json]
  hostdo status <job-id>
  hostdo tail <job-id> [--rows <lines>|--all] [--stdout|--stderr] [--json]
  hostdo send [--no-newline] <job-id> [text...]
  hostdo stop <job-id>
  hostdo --help

Options:
  run
      Start a command as a background job. Waits for developer approval if
      needed, then prints a job id to stdout once the command starts.
      The job id is a UUID — not the output. To see output use:
        hostdo tail <job-id> --all
      To capture output in a variable use hostdo output instead.

  output
      Run a command and wait for it to finish, then print its stdout and stderr
      inline. Exit code mirrors the command. Use this when you need the output
      directly, e.g.:
        TOKEN=$(hostdo output az account get-access-token ...)
        hostdo output cargo build 2>&1

  list
      List tracked hostdo jobs owned by this session.
      Add --running to show only running jobs.
      Add --json to print structured output.

  status <job-id>
      Print JSON status for a tracked hostdo job.

  tail <job-id> [--rows <lines>|--all] [--stdout|--stderr] [--json]
      Print captured output for a tracked hostdo job. Defaults to the last
      24 lines. Use --all for full output and --stdout or --stderr to select
      one stream. Add --json to print the full API response.

  send [--no-newline] <job-id> [text...]
      Send input to a tracked hostdo job. Text arguments are joined with spaces
      and a trailing newline is added unless --no-newline is set. If no text is
      supplied, stdin is forwarded as-is.
      Prints JSON with ok/job_id/message.

  stop <job-id>
      Request cancellation for a tracked hostdo job.
      Prints JSON with ok/job_id/message.

  --image <docker-image>
      Run the command in a short-lived Docker runner instead of directly on
      the host.

  --timeout <seconds>
      Request a command timeout in seconds. Values above 300 seconds are capped
      at 300; a matching hostdo rule may impose a lower ceiling.

  --reason <text>
      Provide a short explanation for why this host command is needed.
      The approval UI shows this to the developer.

  --help
      Show this help text and exit.

Host-side commands:
  Quick reference:
    hostdo output <cmd>         — run and get output inline (most common)
    hostdo run <cmd>            — run in background, returns job id
    hostdo tail <job-id> --all  — get full output of a background job
    hostdo list --running       — see what's currently running

  - Use `hostdo output ...` when you just want to run something and capture its
    output — it works like a normal shell command.
  - Use `hostdo run ...` for long-running commands, commands you want to interact
    with over stdin, or when you need to check status separately.
  - `hostdo run ...` prints a job id (UUID) to stdout — that is NOT the command
    output. Use `hostdo tail <job-id> --all` to get the output afterward.
  - Use `hostdo tail <job-id> --rows <lines>` to inspect recent output, and
    add `--stdout` or `--stderr` to select one stream.
  - Use `hostdo send <job-id> "text"` to send one input line, or pipe data into
    `hostdo send <job-id>` to forward stdin as-is.
  - Use `hostdo list` to see tracked jobs for this session.
  - Only use `hostdo run --image <docker-image> ...` or
    `hostdo output --image <docker-image> ...` when the user explicitly asks
    you to run against a Docker image or containerized runner.
  - `hostdo` requests are policy checked against the `[hostdo]` rules below
    and may prompt the developer.
    Matching is exact on argv + image only; timeout and reason are shown for
    context and stored for review, but do not affect matching.
  - Read this workspace's `harness-rules.toml` for the current allowlisted
    commands, aliases, and any project-specific rule updates.
  - Prefer existing auto-approved `hostdo` commands or
    `hostdo.command_aliases` before asking for a new host command approval.

Blocked common commands:
  - `hostdo` is not meant for basic shell/file utilities such as `ls`, `grep`,
    `rm`, `cat`, `find`, or similar commands.
  - Use it for host-side build, package, compiler, and test tooling instead.

Rule model:
  default_policy: what happens when a command doesn't match any rule below.
    auto   — run without prompting (use with caution)
    prompt — ask the developer in the TUI (default)
    deny   — reject silently

  Passthrough command (exact argv match, auto-approved):
    [[hostdo.commands]]
    argv = ["cargo", "test"]  # run inside container with `hostdo run cargo test`
    cwd = "$WORKSPACE"        # execution cwd only, not part of approval matching
    timeout_secs = 60
    # Optional context persisted for operator review; does not affect matching.
    reason = "run cargo test for dependency drift checks requested by agent"
    approval_mode = "auto"

  Short-lived Docker runner (exact argv + image match, auto-approved):
    [[hostdo.commands]]
    argv = ["npm", "test"]    # run with `hostdo run --image node:20 npm test`
    image = "node:20"
    cwd = "$WORKSPACE"
    timeout_secs = 60
    # Optional context persisted for operator review; does not affect matching.
    reason = "run npm tests in container for repo parity"
    approval_mode = "auto"

  Command alias (agent sends `hostdo run tests`, expands server-side):
    [hostdo.command_aliases]
    tests = "cargo test"  # run inside container with `hostdo run tests`
    build = { cmd = "cargo build --release", cwd = "$WORKSPACE" }

  $WORKSPACE = workspace path on the host.

Exit status:
  - Mirrors the executed command's exit code on success.
  - Exits 1 on infrastructure, parsing, policy, or approval errors.

Notes:
  - The user may be prompted to approve `hostdo` commands or agent-triggered
    network requests.
  - Allow extra time for the user to review those prompts before assuming the
    command is hung.
  - If something appears to be hanging, it may be waiting for developer
    approval in harness-hat.
"""


def _print_usage(stream) -> None:
    print(
        "usage: hostdo <output|run|list|status|tail|send|stop> ...",
        file=stream,
    )


def _print_help() -> None:
    print(_HELP_TEXT)


def _parse_positive_int(raw: str, label: str) -> int:
    try:
        value = int(raw, 10)
    except ValueError:
        print(f"hostdo: {label} must be a positive integer: {raw}", file=sys.stderr)
        sys.exit(1)
    if value <= 0:
        print(f"hostdo: {label} must be greater than zero: {raw}", file=sys.stderr)
        sys.exit(1)
    return value


def _parse_hostdo_args(argv: list[str]):
    image = None
    reason = None
    timeout_secs = None
    command_start = 0

    while command_start < len(argv):
        arg = argv[command_start]
        if arg == "--":
            command_start += 1
            break
        if arg == "--image":
            if command_start + 1 >= len(argv):
                print("hostdo: --image requires an image name", file=sys.stderr)
                sys.exit(1)
            image = argv[command_start + 1]
            command_start += 2
            continue
        if arg.startswith("--image="):
            image = arg.split("=", 1)[1]
            command_start += 1
            continue
        if arg == "--timeout":
            if command_start + 1 >= len(argv):
                print(
                    "hostdo: --timeout requires a positive integer number of seconds",
                    file=sys.stderr,
                )
                sys.exit(1)
            timeout_secs = _parse_positive_int(argv[command_start + 1], "--timeout")
            command_start += 2
            continue
        if arg.startswith("--timeout="):
            timeout_secs = _parse_positive_int(arg.split("=", 1)[1], "--timeout")
            command_start += 1
            continue
        if arg == "--reason":
            if command_start + 1 >= len(argv):
                print("hostdo: --reason requires text", file=sys.stderr)
                sys.exit(1)
            reason = argv[command_start + 1]
            command_start += 2
            continue
        if arg.startswith("--reason="):
            reason = arg.split("=", 1)[1]
            command_start += 1
            continue
        break

    command_argv = argv[command_start:]
    if image == "":
        print("hostdo: --image requires a non-empty image name", file=sys.stderr)
        sys.exit(1)
    if not command_argv:
        print("hostdo: no command specified", file=sys.stderr)
        _print_usage(sys.stderr)
        print("run `hostdo --help` for detailed usage and policy guidance", file=sys.stderr)
        sys.exit(1)

    if timeout_secs is not None:
        timeout_secs = min(timeout_secs, _MAX_COMMAND_TIMEOUT_SECS)

    return command_argv, image, timeout_secs, reason


def _no_proxy_opener() -> urllib.request.OpenerDirector:
    """
    Return a URL opener that bypasses HTTP_PROXY / HTTPS_PROXY env vars.

    The harness-hat control channel must never be routed through the policy proxy
    that harness-hat itself is managing — doing so would create a dependency loop
    and cause the approval request to be intercepted before it reaches the
    manager.
    """
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _default_gateway_ip() -> str:
    """
    Best-effort IPv4 default gateway lookup from /proc/net/route.
    """
    try:
        with open("/proc/net/route", "r", encoding="utf-8") as f:
            next(f, None)  # header
            for line in f:
                cols = line.strip().split()
                if len(cols) < 4:
                    continue
                destination_hex = cols[1]
                gateway_hex = cols[2]
                flags_hex = cols[3]
                if destination_hex != "00000000":
                    continue
                flags = int(flags_hex, 16)
                if (flags & 0x2) == 0:  # RTF_GATEWAY
                    continue
                g = int(gateway_hex, 16)
                octets = [
                    str(g & 0xFF),
                    str((g >> 8) & 0xFF),
                    str((g >> 16) & 0xFF),
                    str((g >> 24) & 0xFF),
                ]
                return ".".join(octets)
    except Exception:
        pass
    return ""


def _route_hex_ipv4(raw: str) -> str:
    value = int(raw, 16)
    return ".".join(
        [
            str(value & 0xFF),
            str((value >> 8) & 0xFF),
            str((value >> 16) & 0xFF),
            str((value >> 24) & 0xFF),
        ]
    )


def _is_virtual_dns_ip(ip: str) -> bool:
    parts = ip.split(".")
    if len(parts) != 4:
        return False
    try:
        first = int(parts[0], 10)
        second = int(parts[1], 10)
    except ValueError:
        return False
    return first == 198 and second in (18, 19)


def _docker_desktop_host_ips() -> list[str]:
    """
    Return route-derived Docker Desktop host IPs.

    In strict mode, tun2proxy virtual DNS maps host.docker.internal to
    198.18.0.0/15 placeholders. Docker Desktop still exposes the real host via
    a /32 route such as 192.168.65.254, which is the address hostdo needs.
    """
    seen: set[str] = set()
    out: list[str] = []
    try:
        with open("/proc/net/route", "r", encoding="utf-8") as f:
            next(f, None)
            for line in f:
                cols = line.strip().split()
                if len(cols) < 8:
                    continue
                destination_hex = cols[1]
                gateway_hex = cols[2]
                flags_hex = cols[3]
                mask_hex = cols[7]
                try:
                    flags = int(flags_hex, 16)
                except ValueError:
                    continue
                if (flags & 0x1) == 0:
                    continue
                for raw in (destination_hex, gateway_hex):
                    if raw in ("00000000",):
                        continue
                    if raw == destination_hex and mask_hex != "FFFFFFFF":
                        continue
                    ip = _route_hex_ipv4(raw)
                    if ip.startswith("127.") or _is_virtual_dns_ip(ip):
                        continue
                    if ip not in seen:
                        seen.add(ip)
                        out.append(ip)
    except Exception:
        pass
    return out


def _resolved_ipv4_host_entries(host: str) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    try:
        for entry in socket.getaddrinfo(host, None, socket.AF_INET, socket.SOCK_STREAM):
            ip = entry[4][0]
            if not _is_virtual_dns_ip(ip) and ip not in seen:
                seen.add(ip)
                out.append(ip)
    except Exception:
        return []
    return out


def _append_candidate_url(
    out: list[str],
    seen: set[str],
    scheme: str,
    host: str,
    port: int,
) -> None:
    candidate = f"{scheme}://{host}:{port}"
    if candidate not in seen:
        out.append(candidate)
        seen.add(candidate)


def _candidate_base_urls(
    base_url: str,
    route_host_ips: list[str] | None = None,
    gateway_ip: str | None = None,
    resolved_ipv4_host_entries=None,
) -> list[str]:
    """
    Build candidate manager URLs.
    If host.docker.internal is unreachable in this runtime, fallback to the
    container's default gateway IP (and common bridge gateway as last resort).
    """
    parsed = urllib.parse.urlparse(base_url)
    host = parsed.hostname or ""
    port = parsed.port or 80
    scheme = parsed.scheme or "http"
    strict_network = os.environ.get("HARNESS_HAT_STRICT_NETWORK", "").strip() == "1"
    seen = set[str]()
    out: list[str] = []
    if host == "host.docker.internal":
        if route_host_ips is None:
            route_host_ips = _docker_desktop_host_ips()
        if gateway_ip is None:
            gateway_ip = _default_gateway_ip()
        if resolved_ipv4_host_entries is None:
            resolved_ipv4_host_entries = _resolved_ipv4_host_entries

        for ip in route_host_ips:
            _append_candidate_url(out, seen, scheme, ip, port)

        gw = gateway_ip
        if gw and not _is_virtual_dns_ip(gw):
            _append_candidate_url(out, seen, scheme, gw, port)
        # Common Linux default bridge fallback.
        _append_candidate_url(out, seen, scheme, "172.17.0.1", port)
        # Common Docker Desktop host gateway fallback.
        _append_candidate_url(out, seen, scheme, "192.168.65.254", port)

        hosts = ["host.docker.internal", "host.containers.internal"]
        for alias in hosts:
            if alias != host and not strict_network:
                _append_candidate_url(out, seen, scheme, alias, port)
            for ip in resolved_ipv4_host_entries(alias):
                if not _is_virtual_dns_ip(ip):
                    _append_candidate_url(out, seen, scheme, ip, port)

        if not strict_network:
            _append_candidate_url(out, seen, scheme, host, port)
    else:
        _append_candidate_url(out, seen, scheme, host, port) if host else out.append(base_url)

    return out


def _read_http_error(exc: urllib.error.HTTPError) -> str:
    try:
        err = json.loads(exc.read())
        return err.get("reason", str(exc))
    except Exception:
        return str(exc)


def _exit_for_http_error(exc: urllib.error.HTTPError) -> None:
    reason = _read_http_error(exc)
    label = "denied" if exc.code == 403 else "failed"
    print(f"hostdo: {label} — {reason}", file=sys.stderr)
    sys.exit(1)


def _auth_headers(token: str, session_token: str) -> dict[str, str]:
    return {
        "Authorization": f"Bearer {token}",
        "x-harness-hat-session-token": session_token,
    }


def _json_request(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    method: str,
    path: str,
    token: str,
    session_token: str,
    body_data=None,
    timeout: int = 30,
) -> dict:
    data = None
    headers = _auth_headers(token, session_token)
    if body_data is not None:
        data = json.dumps(body_data).encode()
        headers["Content-Type"] = "application/json"
    if path == "/exec":
        headers["X-Hostdo-Pid"] = str(os.getpid())
        headers["X-Hostdo-Protocol"] = "jobs"
    req = urllib.request.Request(
        f"{base_url}{path}",
        data=data,
        headers=headers,
        method=method,
    )
    with opener.open(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def _json_request_with_fallback(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    method: str,
    path: str,
    token: str,
    session_token: str,
    body_data=None,
    timeout: int = 30,
) -> tuple[dict, str]:
    last_err = None
    attempted = []
    for candidate_base in _candidate_base_urls(base_url):
        attempted.append(candidate_base)
        try:
            return (
                _json_request(
                    opener,
                    candidate_base,
                    method,
                    path,
                    token,
                    session_token,
                    body_data,
                    timeout,
                ),
                candidate_base,
            )
        except urllib.error.HTTPError as exc:
            _exit_for_http_error(exc)
        except OSError as exc:
            # Connection can drop while the server is starting up, restarting,
            # or still initializing policy context. Retrying a fallback endpoint
            # gives a better failure mode than crashing with a traceback.
            last_err = exc
            continue
        except urllib.error.URLError as exc:
            last_err = exc
            continue
        except TimeoutError:
            print("hostdo: request timed out", file=sys.stderr)
            sys.exit(1)

    reason = getattr(last_err, "reason", last_err)
    print(f"hostdo: request failed: {reason}", file=sys.stderr)
    print(
        "  Is harness-hat running? Is HARNESS_HAT_URL correct? "
        f"({base_url})",
        file=sys.stderr,
    )
    if len(attempted) > 1:
        print("  Tried endpoints:", file=sys.stderr)
        for u in attempted:
            print(f"    - {u}", file=sys.stderr)
    sys.exit(1)


def _emit_job_message(data: dict, last_message):
    message = data.get("message", "")
    if message and message != last_message:
        print(f"hostdo: {message}", file=sys.stderr)
        return message
    return last_message


def _emit_approval_wait_message(
    data: dict,
    approval_wait_started_at,
    last_notice_elapsed_secs,
):
    if data.get("phase") != "pending_approval":
        return None, None

    now = time.monotonic()
    if approval_wait_started_at is None:
        approval_wait_started_at = now

    elapsed_secs = int(now - approval_wait_started_at)
    next_notice_secs = (
        ((elapsed_secs // _APPROVAL_WAIT_NOTICE_SECS) + 1) * _APPROVAL_WAIT_NOTICE_SECS
    )
    if elapsed_secs >= _APPROVAL_WAIT_NOTICE_SECS and elapsed_secs >= next_notice_secs - _APPROVAL_WAIT_NOTICE_SECS:
        notice_secs = (elapsed_secs // _APPROVAL_WAIT_NOTICE_SECS) * _APPROVAL_WAIT_NOTICE_SECS
        if notice_secs != last_notice_elapsed_secs:
            print(
                f"hostdo: Waiting for developer approval... ({notice_secs}s)",
                file=sys.stderr,
            )
            last_notice_elapsed_secs = notice_secs

    return approval_wait_started_at, last_notice_elapsed_secs


def _poll_exec_job(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    job_id: str,
    token: str,
    session_token: str,
    initial: dict,
) -> dict:
    data = initial
    last_message = None
    consecutive_poll_errors = 0
    approval_wait_started_at = None
    last_approval_notice_secs = None

    while True:
        state = data.get("state")
        if state == "complete":
            return data
        if state == "failed":
            reason = data.get("reason") or data.get("message") or "execution failed"
            print(f"hostdo: failed — {reason}", file=sys.stderr)
            sys.exit(1)
        if state != "running":
            print(f"hostdo: failed — unexpected job state: {state}", file=sys.stderr)
            sys.exit(1)

        approval_wait_started_at, last_approval_notice_secs = _emit_approval_wait_message(
            data,
            approval_wait_started_at,
            last_approval_notice_secs,
        )
        if data.get("phase") != "pending_approval":
            approval_wait_started_at = None
            last_approval_notice_secs = None
            last_message = _emit_job_message(data, last_message)
        poll_after_ms = int(data.get("poll_after_ms", 1000))
        time.sleep(max(poll_after_ms, 100) / 1000)

        req = urllib.request.Request(
            f"{base_url}/exec/jobs/{urllib.parse.quote(job_id, safe='')}",
            headers={
                "Authorization": f"Bearer {token}",
                "x-harness-hat-session-token": session_token,
            },
            method="GET",
        )
        try:
            with opener.open(req, timeout=30) as resp:
                data = json.loads(resp.read())
                consecutive_poll_errors = 0
        except urllib.error.HTTPError as exc:
            _exit_for_http_error(exc)
        except OSError as exc:
            # Connection resets during approval waits can occur transiently while
            # the manager is restarting or timing out. Retry briefly before
            # failing hard.
            consecutive_poll_errors += 1
            reason = getattr(exc, "strerror", str(exc))
            if consecutive_poll_errors <= 3:
                print(
                    f"hostdo: waiting for exec job status after polling error: {reason}",
                    file=sys.stderr,
                )
                continue
            print(f"hostdo: request failed while polling exec job: {reason}", file=sys.stderr)
            sys.exit(1)
        except urllib.error.URLError as exc:
            consecutive_poll_errors += 1
            reason = getattr(exc, "reason", exc)
            if consecutive_poll_errors <= 3:
                print(
                    f"hostdo: waiting for exec job status after polling error: {reason}",
                    file=sys.stderr,
                )
                continue
            print(f"hostdo: request failed while polling exec job: {reason}", file=sys.stderr)
            sys.exit(1)
        except TimeoutError:
            consecutive_poll_errors += 1
            if consecutive_poll_errors <= 3:
                print(
                    "hostdo: waiting for exec job status after polling timeout",
                    file=sys.stderr,
                )
                continue
            print("hostdo: request timed out while polling exec job", file=sys.stderr)
            sys.exit(1)


def _poll_exec_job_until_started(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    job_id: str,
    token: str,
    session_token: str,
    initial: dict,
) -> dict:
    data = initial
    last_message = None
    consecutive_poll_errors = 0
    approval_wait_started_at = None
    last_approval_notice_secs = None

    while True:
        state = data.get("state")
        phase = data.get("phase")
        if state == "complete" or (state == "running" and phase == "running_command"):
            return data
        if state == "failed":
            reason = data.get("reason") or data.get("message") or "execution failed"
            print(f"hostdo: failed — {reason}", file=sys.stderr)
            sys.exit(1)
        if state != "running":
            print(f"hostdo: failed — unexpected job state: {state}", file=sys.stderr)
            sys.exit(1)

        approval_wait_started_at, last_approval_notice_secs = _emit_approval_wait_message(
            data,
            approval_wait_started_at,
            last_approval_notice_secs,
        )
        if phase != "pending_approval":
            approval_wait_started_at = None
            last_approval_notice_secs = None
            last_message = _emit_job_message(data, last_message)
        poll_after_ms = int(data.get("poll_after_ms", 1000))
        time.sleep(max(poll_after_ms, 100) / 1000)

        try:
            data = _json_request(
                opener,
                base_url,
                "GET",
                f"/exec/jobs/{urllib.parse.quote(job_id, safe='')}",
                token,
                session_token,
                timeout=30,
            )
            consecutive_poll_errors = 0
        except urllib.error.HTTPError as exc:
            _exit_for_http_error(exc)
        except OSError as exc:
            # A short-lived socket reset while waiting for startup/approval is
            # usually transient; retry a few times before failing.
            consecutive_poll_errors += 1
            reason = getattr(exc, "strerror", str(exc))
            if consecutive_poll_errors <= 3:
                print(
                    f"hostdo: waiting for exec job status after polling error: {reason}",
                    file=sys.stderr,
                )
                continue
            print(f"hostdo: request failed while polling exec job: {reason}", file=sys.stderr)
            sys.exit(1)
        except urllib.error.URLError as exc:
            consecutive_poll_errors += 1
            reason = getattr(exc, "reason", exc)
            if consecutive_poll_errors <= 3:
                print(
                    f"hostdo: waiting for exec job status after polling error: {reason}",
                    file=sys.stderr,
                )
                continue
            print(f"hostdo: request failed while polling exec job: {reason}", file=sys.stderr)
            sys.exit(1)
        except TimeoutError:
            consecutive_poll_errors += 1
            if consecutive_poll_errors <= 3:
                print(
                    "hostdo: waiting for exec job status after polling timeout",
                    file=sys.stderr,
                )
                continue
            print("hostdo: request timed out while polling exec job", file=sys.stderr)
            sys.exit(1)


def _print_job_id(data: dict) -> None:
    job_id = data.get("job_id", "")
    if not job_id:
        print("hostdo: failed — response did not include a job id", file=sys.stderr)
        sys.exit(1)
    print(job_id)


def _print_jobs_table(data: dict) -> None:
    jobs = data.get("jobs", [])
    print("JOB_ID\tCONTAINER\tSTATE\tPHASE\tTIMEOUT\tEXIT\tCOMMAND")
    for job in jobs:
        job_id = job.get("job_id", "")
        container = job.get("container", "") or "-"
        state = job.get("state", "")
        phase = job.get("phase", "") or "-"
        timeout_secs = job.get("timeout_secs")
        timeout_text = "-" if timeout_secs is None else f"{timeout_secs}s"
        exit_code = job.get("exit_code")
        exit_text = "-" if exit_code is None else str(exit_code)
        argv = " ".join(job.get("argv", []))
        print(
            f"{job_id}\t{container}\t{state}\t{phase}\t{timeout_text}\t{exit_text}\t{argv}"
        )


def _filter_jobs_running(data: dict) -> dict:
    jobs = data.get("jobs", [])
    filtered = [job for job in jobs if job.get("state") == "running"]
    out = dict(data)
    out["jobs"] = filtered
    return out


def _print_job_output(data: dict, selected_stream=None) -> None:
    stdout = data.get("stdout", "")
    stderr = data.get("stderr", "")
    if selected_stream == "stdout":
        sys.stdout.write(stdout)
        sys.stdout.flush()
        return
    if selected_stream == "stderr":
        sys.stdout.write(stderr)
        sys.stdout.flush()
        return
    if stdout:
        sys.stdout.write(stdout)
        sys.stdout.flush()
    if stderr:
        sys.stderr.write(stderr)
        sys.stderr.flush()


def _parse_tail_args(argv: list[str]):
    stream = None
    rows = 24
    include_all = False
    json_mode = False
    job_id = None
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--json":
            json_mode = True
            i += 1
            continue
        if arg == "--stdout":
            if stream is not None:
                print("hostdo: choose only one of --stdout or --stderr", file=sys.stderr)
                sys.exit(1)
            stream = "stdout"
            i += 1
            continue
        if arg == "--stderr":
            if stream is not None:
                print("hostdo: choose only one of --stdout or --stderr", file=sys.stderr)
                sys.exit(1)
            stream = "stderr"
            i += 1
            continue
        if arg == "--all":
            include_all = True
            i += 1
            continue
        if arg == "--rows":
            if i + 1 >= len(argv):
                print("hostdo: --rows requires a line count", file=sys.stderr)
                sys.exit(1)
            rows = _parse_positive_int(argv[i + 1], "--rows")
            i += 2
            continue
        if arg.startswith("--rows="):
            rows = _parse_positive_int(arg.split("=", 1)[1], "--rows")
            i += 1
            continue
        if arg.startswith("-"):
            print(f"hostdo: unknown tail option: {arg}", file=sys.stderr)
            sys.exit(1)
        if job_id is not None:
            print("hostdo: tail requires exactly one job id", file=sys.stderr)
            sys.exit(1)
        job_id = arg
        i += 1

    if job_id is None:
        print("hostdo: tail requires exactly one job id", file=sys.stderr)
        sys.exit(1)
    tail = None if include_all else rows
    return job_id, stream, tail, json_mode


def _tail_job_output(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    session_token: str,
    argv: list[str],
) -> None:
    job_id, stream, tail, json_mode = _parse_tail_args(argv)
    query = {}
    if stream is not None:
        query["stream"] = stream
    if tail is not None:
        query["tail"] = str(tail)
    suffix = ""
    if query:
        suffix = "?" + urllib.parse.urlencode(query)
    quoted_job_id = urllib.parse.quote(job_id, safe="")
    data, _ = _json_request_with_fallback(
        opener,
        base_url,
        "GET",
        f"/exec/jobs/{quoted_job_id}/output{suffix}",
        token,
        session_token,
    )
    if json_mode:
        print(json.dumps(data, indent=2, sort_keys=True))
    else:
        _print_job_output(data, stream)


def _send_job_input(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    session_token: str,
    argv: list[str],
) -> None:
    add_newline = True
    i = 0
    while i < len(argv) and argv[i].startswith("-"):
        arg = argv[i]
        if arg == "--no-newline":
            add_newline = False
            i += 1
            continue
        print(f"hostdo: unknown send option: {arg}", file=sys.stderr)
        sys.exit(1)

    if i >= len(argv):
        print("hostdo: send requires a job id", file=sys.stderr)
        sys.exit(1)
    job_id = argv[i]
    text_args = argv[i + 1 :]
    if text_args:
        input_text = " ".join(text_args)
        if add_newline:
            input_text += "\n"
    else:
        input_text = sys.stdin.read()

    quoted_job_id = urllib.parse.quote(job_id, safe="")
    data, _ = _json_request_with_fallback(
        opener,
        base_url,
        "POST",
        f"/exec/jobs/{quoted_job_id}/input",
        token,
        session_token,
        body_data={"input": input_text},
    )
    print(json.dumps(data, indent=2, sort_keys=True))


def _run_detached(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    session_token: str,
    argv: list[str],
) -> None:
    command_argv, image, timeout_secs, reason = _parse_hostdo_args(argv)
    try:
        cwd = os.getcwd()
    except OSError as exc:
        print(f"hostdo: cannot determine working directory: {exc}", file=sys.stderr)
        sys.exit(1)

    body_data = {
        "argv": command_argv,
        "cwd": cwd,
        "detach": True,
    }
    if image is not None:
        body_data["image"] = image
    if timeout_secs is not None:
        body_data["timeout_secs"] = timeout_secs
    if reason is not None:
        body_data["reason"] = reason

    data, selected_base = _json_request_with_fallback(
        opener,
        base_url,
        "POST",
        "/exec",
        token,
        session_token,
        body_data=body_data,
        timeout=_TIMEOUT,
    )
    if data.get("state") != "running":
        _print_job_id(data)
        return

    job_id = data.get("job_id", "")
    if not job_id:
        print("hostdo: failed — running response did not include a job id", file=sys.stderr)
        sys.exit(1)
    data = _poll_exec_job_until_started(
        opener,
        selected_base,
        job_id,
        token,
        session_token,
        data,
    )
    _print_job_id(data)


def _run_inline(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    session_token: str,
    argv: list[str],
) -> None:
    command_argv, image, timeout_secs, reason = _parse_hostdo_args(argv)
    try:
        cwd = os.getcwd()
    except OSError as exc:
        print(f"hostdo: cannot determine working directory: {exc}", file=sys.stderr)
        sys.exit(1)

    body_data: dict = {
        "argv": command_argv,
        "cwd": cwd,
        "detach": True,
    }
    if image is not None:
        body_data["image"] = image
    if timeout_secs is not None:
        body_data["timeout_secs"] = timeout_secs
    if reason is not None:
        body_data["reason"] = reason

    data, selected_base = _json_request_with_fallback(
        opener,
        base_url,
        "POST",
        "/exec",
        token,
        session_token,
        body_data=body_data,
        timeout=_TIMEOUT,
    )

    job_id = data.get("job_id", "")
    if not job_id:
        print("hostdo: failed — response did not include a job id", file=sys.stderr)
        sys.exit(1)

    data = _poll_exec_job(opener, selected_base, job_id, token, session_token, data)

    # Fetch full output now that the job is complete.
    quoted_job_id = urllib.parse.quote(job_id, safe="")
    output_data, _ = _json_request_with_fallback(
        opener,
        selected_base,
        "GET",
        f"/exec/jobs/{quoted_job_id}/output",
        token,
        session_token,
    )
    _print_job_output(output_data)

    exit_code = data.get("exit_code")
    if exit_code is not None and exit_code != 0:
        sys.exit(exit_code)


def _run_job_control(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    session_token: str,
    argv: list[str],
) -> bool:
    if not argv:
        return False

    subcommand = argv[0]
    if subcommand == "output":
        if len(argv) == 1:
            print("hostdo: output requires a command", file=sys.stderr)
            sys.exit(1)
        _run_inline(opener, base_url, token, session_token, argv[1:])
        return True

    if subcommand == "run":
        if len(argv) == 1:
            print("hostdo: run requires a command", file=sys.stderr)
            sys.exit(1)
        _run_detached(opener, base_url, token, session_token, argv[1:])
        return True

    if subcommand == "list":
        json_mode = False
        running_only = False
        for arg in argv[1:]:
            if arg == "--json":
                json_mode = True
                continue
            if arg == "--running":
                running_only = True
                continue
            print("hostdo: list takes only --running and/or --json", file=sys.stderr)
            sys.exit(1)
        data, _ = _json_request_with_fallback(
            opener, base_url, "GET", "/exec/jobs", token, session_token
        )
        if running_only:
            data = _filter_jobs_running(data)
        if json_mode:
            print(json.dumps(data, indent=2, sort_keys=True))
        else:
            _print_jobs_table(data)
        return True

    if subcommand == "tail":
        if len(argv) == 1:
            print("hostdo: tail requires a job id", file=sys.stderr)
            sys.exit(1)
        _tail_job_output(opener, base_url, token, session_token, argv[1:])
        return True

    if subcommand == "send":
        if len(argv) == 1:
            print("hostdo: send requires a job id", file=sys.stderr)
            sys.exit(1)
        _send_job_input(opener, base_url, token, session_token, argv[1:])
        return True

    if subcommand in ("status", "stop"):
        if len(argv) != 2:
            print(f"hostdo: {subcommand} requires exactly one job id", file=sys.stderr)
            sys.exit(1)
        job_id = urllib.parse.quote(argv[1], safe="")
        if subcommand == "status":
            data, _ = _json_request_with_fallback(
                opener, base_url, "GET", f"/exec/jobs/{job_id}", token, session_token
            )
            print(json.dumps(data, indent=2, sort_keys=True))
            return True
        data, _ = _json_request_with_fallback(
            opener, base_url, "POST", f"/exec/jobs/{job_id}/kill", token, session_token
        )
        print(json.dumps(data, indent=2, sort_keys=True))
        return True

    return False


def main() -> None:
    argv = sys.argv[1:]
    if len(argv) == 1 and argv[0] == "--help":
        _print_help()
        sys.exit(0)
    if not argv:
        print("hostdo: no command specified", file=sys.stderr)
        _print_usage(sys.stderr)
        print("run `hostdo --help` for detailed usage and policy guidance", file=sys.stderr)
        sys.exit(1)
    if argv[0].startswith("-"):
        print(f"hostdo: unknown option: {argv[0]}", file=sys.stderr)
        _print_usage(sys.stderr)
        sys.exit(1)

    base_url = os.environ.get("HARNESS_HAT_URL", "http://127.0.0.1:7878").rstrip("/")

    session_token = os.environ.get("HARNESS_HAT_SESSION_TOKEN", "")
    if not session_token:
        print("hostdo: HARNESS_HAT_SESSION_TOKEN is not set", file=sys.stderr)
        print(
            "  This container was likely started with an older harness-hat image.",
            file=sys.stderr,
        )
        sys.exit(1)

    opener = _no_proxy_opener()

    if _run_job_control(opener, base_url, session_token, session_token, argv):
        return
    print(f"hostdo: unknown command: {argv[0]}", file=sys.stderr)
    _print_usage(sys.stderr)
    print("run `hostdo --help` for detailed usage and policy guidance", file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
    main()

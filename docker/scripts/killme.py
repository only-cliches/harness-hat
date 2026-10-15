#!/usr/bin/env python3
"""
killme — harness-hat container exit command.

Requests that the harness-hat manager stop the current container session.

Environment variables:
  HARNESS_HAT_URL      Base URL of the harness-hat manager (default: http://127.0.0.1:7878)
  HARNESS_HAT_SESSION_TOKEN  Per-session bearer token injected by harness-hat (required)
"""

import json
import os
import socket
import sys
import urllib.error
import urllib.parse
import urllib.request

_TIMEOUT = 30


def _no_proxy_opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


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
    seen = set()
    out = []
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
                    if raw == "00000000":
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
    seen = set()
    out = []
    try:
        for entry in socket.getaddrinfo(host, None, socket.AF_INET, socket.SOCK_STREAM):
            ip = entry[4][0]
            if not _is_virtual_dns_ip(ip) and ip not in seen:
                seen.add(ip)
                out.append(ip)
    except Exception:
        pass
    return out


def _candidate_base_urls(base_url: str) -> list[str]:
    parsed = urllib.parse.urlparse(base_url)
    host = parsed.hostname or ""
    port = parsed.port or 80
    scheme = parsed.scheme or "http"
    strict_network = os.environ.get("HARNESS_HAT_STRICT_NETWORK", "").strip() == "1"

    out = []
    if host == "host.docker.internal":
        for ip in _docker_desktop_host_ips():
            out.append(f"{scheme}://{ip}:{port}")
        out.append(f"{scheme}://172.17.0.1:{port}")
        out.append(f"{scheme}://192.168.65.254:{port}")
        for alias in ("host.docker.internal", "host.containers.internal"):
            if alias != host and not strict_network:
                out.append(f"{scheme}://{alias}:{port}")
            for ip in _resolved_ipv4_host_entries(alias):
                out.append(f"{scheme}://{ip}:{port}")
        if not strict_network:
            out.append(base_url)
    else:
        out.append(base_url)

    if not out:
        out = [base_url]

    seen = set()
    uniq = []
    for u in out:
        if u not in seen:
            seen.add(u)
            uniq.append(u)
    return uniq


def main() -> None:
    base_url = os.environ.get("HARNESS_HAT_URL", "http://127.0.0.1:7878").rstrip("/")

    session_token = os.environ.get("HARNESS_HAT_SESSION_TOKEN", "")
    if not session_token:
        print("killme: HARNESS_HAT_SESSION_TOKEN is not set", file=sys.stderr)
        sys.exit(1)

    body = json.dumps({}).encode()

    opener = _no_proxy_opener()
    last_err = None

    for candidate_base in _candidate_base_urls(base_url):
        req = urllib.request.Request(
            f"{candidate_base}/container/stop",
            data=body,
            headers={
                "Authorization": f"Bearer {session_token}",
                "Content-Type": "application/json",
                "x-harness-hat-session-token": session_token,
            },
            method="POST",
        )
        try:
            with opener.open(req, timeout=_TIMEOUT) as resp:
                data = json.loads(resp.read())
                if data.get("ok"):
                    sys.exit(0)
                print("killme: unexpected response from manager", file=sys.stderr)
                sys.exit(1)
        except urllib.error.HTTPError as exc:
            try:
                err = json.loads(exc.read())
                reason = err.get("reason", str(exc))
            except Exception:
                reason = str(exc)
            print(f"killme: denied — {reason}", file=sys.stderr)
            sys.exit(1)
        except urllib.error.URLError as exc:
            last_err = exc
            continue
        except TimeoutError:
            print("killme: request timed out", file=sys.stderr)
            sys.exit(1)

    reason = getattr(last_err, "reason", last_err)
    print(f"killme: request failed: {reason}", file=sys.stderr)
    print(
        f"  Is harness-hat running? Is HARNESS_HAT_URL correct? ({base_url})",
        file=sys.stderr,
    )
    sys.exit(1)


if __name__ == "__main__":
    main()

use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;
use globset::Glob;
use lru::LruCache;
use reqwest::StatusCode;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

use crate::activity::{Activity, ActivityState, wait_cancelled};
use crate::config::Config;
#[cfg(test)]
use crate::proxy::SourceIdentityStatus;
use crate::proxy::http::is_hop_by_hop;

const DNS_LOOKUP_LIMIT: usize = 16;
const DNS_CACHE_TTL: Duration = Duration::from_secs(300);
const DNS_STALE_TTL: Duration = Duration::from_secs(1800);
const DNS_CACHE_CAPACITY: usize = 1024;

static DNS_LOOKUP_LIMITER: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(DNS_LOOKUP_LIMIT)));
static DNS_CACHE: LazyLock<Mutex<LruCache<String, DnsCacheEntry>>> = LazyLock::new(|| {
    Mutex::new(LruCache::new(
        NonZeroUsize::new(DNS_CACHE_CAPACITY).expect("non-zero DNS cache cap"),
    ))
});

#[derive(Clone)]
struct DnsCacheEntry {
    addrs: Vec<SocketAddr>,
    stored_at: Instant,
}

pub(crate) fn format_byte_count(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes}b");
    }

    let mut value = bytes as f64;
    let mut unit = "b";
    for next_unit in ["kb", "mb", "gb", "tb"] {
        value /= 1024.0;
        unit = next_unit;
        if value < 1024.0 {
            break;
        }
    }

    if value >= 10.0 || value.fract().abs() < f64::EPSILON {
        format!("{value:.0}{unit}")
    } else {
        format!("{value:.1}{unit}")
    }
}

pub(crate) async fn tunnel_with_activity<D, U>(
    state: &crate::proxy::ProxyState,
    activity: &Activity,
    downstream: &mut D,
    upstream: &mut U,
) -> Result<()>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        result = copy_bidirectional_with_idle_timeout(downstream, upstream, state.proxy_idle_timeout()) => match result {
            Ok((from_client, from_server)) => {
                state.activity_line(
                    &activity.id,
                    format!(
                        "tunnel closed after {} upstream, {} downstream",
                        format_byte_count(from_client),
                        format_byte_count(from_server)
                    ),
                );
                state.activity_finished(
                    &activity.id,
                    ActivityState::Complete,
                    Some("tunnel closed".to_string()),
                );
                Ok(())
            }
            Err(e) => {
                state.activity_finished(&activity.id, ActivityState::Failed, Some(e.to_string()));
                Err(e)
            }
        },
        _ = wait_cancelled(activity.cancel_flag.clone()) => {
            state.activity_finished(
                &activity.id,
                ActivityState::Cancelled,
                Some("cancelled".to_string()),
            );
            Ok(())
        }
    }
}

/// Relay a tunnel while enforcing an inactivity deadline in either direction.
/// `copy_bidirectional` has no progress hook, so a plain timeout around it
/// would cap total tunnel lifetime rather than idle time.
pub(crate) async fn copy_bidirectional_with_idle_timeout<D, U>(
    downstream: &mut D,
    upstream: &mut U,
    idle_timeout: Duration,
) -> Result<(u64, u64)>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut downstream_read, mut downstream_write) = tokio::io::split(downstream);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let mut client_buffer = [0u8; 16 * 1024];
    let mut server_buffer = [0u8; 16 * 1024];
    let mut client_closed = false;
    let mut server_closed = false;
    let mut from_client = 0u64;
    let mut from_server = 0u64;
    let idle_deadline = tokio::time::Instant::now() + idle_timeout;
    let idle_timer = tokio::time::sleep_until(idle_deadline);
    tokio::pin!(idle_timer);

    while !client_closed || !server_closed {
        tokio::select! {
            result = downstream_read.read(&mut client_buffer), if !client_closed => {
                let count = result?;
                if count == 0 {
                    client_closed = true;
                    tokio::time::timeout(idle_timeout, upstream_write.shutdown())
                        .await
                        .map_err(|_| anyhow::anyhow!("proxy tunnel timed out shutting down upstream"))??;
                } else {
                    tokio::time::timeout(idle_timeout, upstream_write.write_all(&client_buffer[..count]))
                        .await
                        .map_err(|_| anyhow::anyhow!("proxy tunnel timed out writing upstream"))??;
                    from_client += count as u64;
                    idle_timer.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                }
            }
            result = upstream_read.read(&mut server_buffer), if !server_closed => {
                let count = result?;
                if count == 0 {
                    server_closed = true;
                    tokio::time::timeout(idle_timeout, downstream_write.shutdown())
                        .await
                        .map_err(|_| anyhow::anyhow!("proxy tunnel timed out shutting down downstream"))??;
                } else {
                    tokio::time::timeout(idle_timeout, downstream_write.write_all(&server_buffer[..count]))
                        .await
                        .map_err(|_| anyhow::anyhow!("proxy tunnel timed out writing downstream"))??;
                    from_server += count as u64;
                    idle_timer.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                }
            }
            _ = &mut idle_timer => {
                anyhow::bail!("proxy tunnel closed after {} seconds of inactivity", idle_timeout.as_secs());
            }
        }
    }

    Ok((from_client, from_server))
}

pub(crate) fn proxy_authorization_matches_token(
    headers: &[(String, String)],
    expected_token: &str,
) -> bool {
    let Some(auth) = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
        .map(|(_, v)| v.as_str())
    else {
        return false;
    };
    let Some((scheme, payload)) = auth.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return false;
    }
    let Ok(decoded) = STANDARD.decode(payload.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((username, password)) = creds.split_once(':') else {
        return false;
    };
    let username = percent_decode_basic_credential(username);
    let password = percent_decode_basic_credential(password);
    // Constant-time comparison so attackers can't recover the token byte-by-byte
    // over a timed channel. Both sides padded to equal length to avoid leaking
    // length via early-return.
    let username_ok = constant_time_eq_bytes(username.as_bytes(), b"harness-hat");
    let password_ok = constant_time_eq_bytes(password.as_bytes(), expected_token.as_bytes());
    username_ok & password_ok
}

/// Constant-time byte-slice equality that does not leak length differences.
/// Both inputs are zero-padded to the max length and then compared along with
/// an explicit length-equality bit so the timing reveals only `equal/not equal`.
pub(crate) fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    let max = a.len().max(b.len());
    let mut pa = vec![0u8; max];
    let mut pb = vec![0u8; max];
    pa[..a.len()].copy_from_slice(a);
    pb[..b.len()].copy_from_slice(b);
    let bytes_eq: bool = pa.ct_eq(&pb).into();
    bytes_eq && a.len() == b.len()
}

fn percent_decode_basic_credential(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn decode_source_from_proxy_authorization(
    value: &str,
) -> (Option<String>, Option<String>, SourceIdentityStatus) {
    let Some((scheme, payload)) = value.split_once(' ') else {
        return (None, None, SourceIdentityStatus::MalformedAuthHeader);
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return (None, None, SourceIdentityStatus::UnsupportedAuthScheme);
    }
    let decoded = match STANDARD.decode(payload.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return (None, None, SourceIdentityStatus::InvalidBase64),
    };
    let creds = match String::from_utf8(decoded) {
        Ok(s) => s,
        Err(_) => return (None, None, SourceIdentityStatus::InvalidUtf8),
    };
    let Some((username, password)) = creds.split_once(':') else {
        return (
            None,
            None,
            SourceIdentityStatus::MissingUsernamePasswordDelimiter,
        );
    };
    if username != "zcsrc" {
        return (None, None, SourceIdentityStatus::UnexpectedUsername);
    }
    let Some((project_enc, container_enc)) = password.split_once('.') else {
        return (
            None,
            None,
            SourceIdentityStatus::MissingProjectContainerDelimiter,
        );
    };
    let project = match URL_SAFE_NO_PAD.decode(project_enc.as_bytes()) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => return (None, None, SourceIdentityStatus::InvalidProjectEncoding),
        },
        Err(_) => return (None, None, SourceIdentityStatus::InvalidProjectEncoding),
    };
    let container = match URL_SAFE_NO_PAD.decode(container_enc.as_bytes()) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => return (None, None, SourceIdentityStatus::InvalidContainerEncoding),
        },
        Err(_) => return (None, None, SourceIdentityStatus::InvalidContainerEncoding),
    };
    (Some(project), Some(container), SourceIdentityStatus::Ok)
}

pub(crate) fn is_host_allowed(config: &Config, source_container: Option<&str>, host: &str) -> bool {
    let source_container = match source_container {
        Some(name) => name,
        None => return false,
    };
    let container = match config
        .containers
        .iter()
        .find(|c| c.name == source_container)
    {
        Some(c) => c,
        None => return false,
    };
    container
        .allowed_hosts
        .iter()
        .any(|pattern| host_matches_pattern(pattern, host))
}

pub(crate) fn host_matches_pattern(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }

    let host_lc = host.to_ascii_lowercase();
    let pattern_lc = pattern.to_ascii_lowercase();

    if let Some(apex) = pattern_lc.strip_prefix('.') {
        return host_lc == apex || host_lc.ends_with(&format!(".{apex}"));
    }

    if let Some(apex) = pattern_lc.strip_prefix("*.") {
        return host_lc.ends_with(&format!(".{apex}"));
    }

    if !pattern_lc.contains('*') {
        return host_lc == pattern_lc;
    }

    Glob::new(&pattern_lc)
        .ok()
        .map(|g| g.compile_matcher().is_match(&host_lc))
        .unwrap_or(false)
}

pub(crate) fn extract_host_port(
    headers: &[(String, String)],
    path: &str,
    default_port: u16,
) -> Option<(String, u16)> {
    // Strict Host validation: reject zero or >1 Host headers. The plain-HTTP
    // path used to silently pick the first Host header; that's a smuggling
    // surface (CR3/H3). Falling back to the request-target absolute-URI for
    // proxies that send `GET http://host/path HTTP/1.1`.
    let mut host_headers = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("host"));
    if let Some((_, v)) = host_headers.next() {
        if host_headers.next().is_some() {
            return None;
        }
        let (host_raw, port) = split_host_port(v.trim(), default_port);
        let canon = canonicalize_host(&host_raw).ok()?;
        return Some((canon, port));
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        if let Ok(url) = path.parse::<url::Url>() {
            let host = url.host_str()?.to_string();
            let port = url.port_or_known_default().unwrap_or(default_port);
            let canon = canonicalize_host(&host).ok()?;
            return Some((canon, port));
        }
    }
    None
}

/// Canonicalize an incoming hostname for both rule matching and outbound use:
/// - trim whitespace
/// - strip trailing `.`
/// - lowercase (ASCII; IDNA handles Unicode lowercase)
/// - run through `idna::domain_to_ascii` so `xn--…` and Unicode forms collapse
///   onto the same canonical key
///
/// IP literals pass through unchanged. Empty / control-character / mixed-script
/// inputs return `Err`, which callers must surface as a 400 (or equivalent
/// protocol error) so malformed inputs do not silently bypass policy.
pub(crate) fn canonicalize_host(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    anyhow::ensure!(!trimmed.is_empty(), "empty host");
    // Reject control chars / spaces in host. IDNA accepts some of these.
    anyhow::ensure!(
        !trimmed
            .chars()
            .any(|c| c.is_control() || c == ' ' || c == '\t'),
        "host contains control or whitespace characters"
    );
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        // Normalize the IP literal's textual form (e.g. `2001:0db8::1` → `2001:db8::1`).
        return Ok(ip.to_string());
    }
    // `idna::domain_to_ascii` lowercases + IDNA-encodes. It rejects mixed-script
    // attempts in default mode (uses strict UTS #46).
    let ascii = idna::domain_to_ascii(trimmed)
        .map_err(|e| anyhow::anyhow!("invalid host {trimmed:?}: {e}"))?;
    anyhow::ensure!(
        ascii.bytes().all(|b| b.is_ascii() && b > 0x20 && b != 0x7f),
        "host {trimmed:?} normalized to non-ASCII output"
    );
    Ok(ascii)
}

#[cfg(test)]
pub(crate) fn ensure_host_header_matches_target(
    headers: &[(String, String)],
    expected_host: &str,
    expected_port: u16,
) -> Result<()> {
    let mut host_headers = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim());
    let Some(host_header) = host_headers.next() else {
        return Ok(());
    };
    anyhow::ensure!(
        host_headers.next().is_none(),
        "multiple Host headers are not allowed"
    );

    let (actual_host, actual_port) = split_host_port(host_header, expected_port);
    anyhow::ensure!(
        actual_port == expected_port && host_names_match(&actual_host, expected_host),
        "Host header {host_header} does not match TLS target {expected_host}:{expected_port}"
    );
    Ok(())
}

#[cfg(test)]
fn host_names_match(actual: &str, expected: &str) -> bool {
    let actual = actual.trim().trim_end_matches('.');
    let expected = expected.trim().trim_end_matches('.');

    if let (Ok(actual_ip), Ok(expected_ip)) = (actual.parse::<IpAddr>(), expected.parse::<IpAddr>())
    {
        return actual_ip == expected_ip;
    }

    // Compare canonicalized (IDNA + lowercase) forms when both parse cleanly;
    // fall back to ASCII-insensitive compare so test inputs and edge cases
    // (like inputs that fail IDNA) still match the prior behavior.
    if let (Ok(a), Ok(e)) = (canonicalize_host(actual), canonicalize_host(expected)) {
        return a == e;
    }
    actual.eq_ignore_ascii_case(expected)
}

pub(crate) fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    let host = authority.trim();
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            let port = host[end + 1..]
                .strip_prefix(':')
                .and_then(|raw| raw.parse::<u16>().ok())
                .unwrap_or(default_port);
            return (host[1..end].to_string(), port);
        }
        return (host.to_string(), default_port);
    }
    if host.matches(':').count() == 1
        && let Some((name, raw_port)) = host.rsplit_once(':')
        && !name.is_empty()
        && let Ok(port) = raw_port.parse::<u16>()
    {
        return (name.to_string(), port);
    }
    (host.to_string(), default_port)
}

pub(crate) fn strip_scheme_and_host(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        if let Ok(url) = path.parse::<url::Url>() {
            let mut result = url.path().to_string();
            if let Some(q) = url.query() {
                result.push('?');
                result.push_str(q);
            }
            return result;
        }
    }
    path.to_string()
}

pub(crate) async fn write_response_any<W>(sink: &mut W, response: reqwest::Response) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    // Strip hop-by-hop headers as well as any header tokens listed in
    // upstream's `Connection:` header. RFC 7230 requires those to be removed
    // before forwarding.
    let extra_hop = connection_hop_tokens_from_reqwest_headers(response.headers());

    let status = response.status().as_u16();
    let reason = response.status().canonical_reason().unwrap_or("Unknown");

    let resp_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop_with_extra(name.as_str(), &extra_hop))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.to_string(), v.to_string()))
        })
        .collect();

    let content_length: Option<u64> = resp_headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok());

    let use_chunked = content_length.is_none();

    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in &resp_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if use_chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    }
    head.push_str("Connection: close\r\n");
    head.push_str("\r\n");
    sink.write_all(head.as_bytes()).await?;

    let mut body_stream = response.bytes_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk?;
        if chunk.is_empty() {
            continue;
        }
        if use_chunked {
            sink.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await?;
            sink.write_all(&chunk).await?;
            sink.write_all(b"\r\n").await?;
        } else {
            // Enforce upstream Content-Length: don't write more than was
            // promised (avoid response smuggling on a kept-alive socket; we
            // close the connection anyway, but be strict). Also bail on
            // short stream below.
            if let Some(cl) = content_length {
                let remaining = cl.saturating_sub(written);
                let len = (chunk.len() as u64).min(remaining);
                sink.write_all(&chunk[..len as usize]).await?;
                written += len;
                if remaining == 0 {
                    // Body finished; ignore any further bytes.
                    break;
                }
            } else {
                sink.write_all(&chunk).await?;
            }
        }
    }
    if use_chunked {
        sink.write_all(b"0\r\n\r\n").await?;
    } else if let Some(cl) = content_length
        && written < cl
    {
        anyhow::bail!(
            "upstream returned {written} bytes but Content-Length advertised {cl}; short body"
        );
    }
    Ok(())
}

/// Parse the `Connection:` header values out of a reqwest `HeaderMap` into a
/// lowercased token set. Used by `write_response_any` to honor RFC 7230 §6.1.
fn connection_hop_tokens_from_reqwest_headers(
    headers: &reqwest::header::HeaderMap,
) -> HashSet<String> {
    let mut out = HashSet::new();
    for value in headers.get_all(reqwest::header::CONNECTION) {
        let Ok(s) = value.to_str() else { continue };
        for token in s.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if !token.is_empty() {
                out.insert(token);
            }
        }
    }
    out
}

/// `is_hop_by_hop` plus per-message `Connection:` tokens.
pub(crate) fn is_hop_by_hop_with_extra(name: &str, extra: &HashSet<String>) -> bool {
    if is_hop_by_hop(name) {
        return true;
    }
    extra.contains(&name.to_ascii_lowercase())
}

/// Parse the `Connection:` header values out of a flat header list into a
/// lowercased token set. Used on the request path to mirror response-side
/// stripping.
pub(crate) fn connection_hop_tokens(headers: &[(String, String)]) -> HashSet<String> {
    let mut out = HashSet::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in value.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if !token.is_empty() {
                out.insert(token);
            }
        }
    }
    out
}

pub(crate) async fn write_error_any<W>(sink: &mut W, code: u16, msg: &str) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let body = msg.as_bytes();
    let reason = StatusCode::from_u16(code)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        .canonical_reason()
        .unwrap_or("Unknown");

    let out = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut raw = out.into_bytes();
    raw.extend_from_slice(body);
    sink.write_all(&raw).await?;
    Ok(())
}

pub(crate) fn is_expected_disconnect(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("close_notify")
        || msg.contains("unexpected eof")
        || msg.contains("connection reset by peer")
        || msg.contains("broken pipe")
}

#[cfg(test)]
pub(crate) async fn resolve_public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    resolve_public_addrs_with_priority(host, port, false).await
}

pub(crate) async fn resolve_public_addrs_with_priority(
    host: &str,
    port: u16,
    primary: bool,
) -> Result<Vec<SocketAddr>> {
    anyhow::ensure!(!host.trim().is_empty(), "destination host is empty");

    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure_public_ip(ip, host)?;
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let cache_key = dns_cache_key(host, port);
    if let Some(addrs) = cached_dns_addrs(&cache_key, DNS_CACHE_TTL) {
        return Ok(addrs);
    }

    let _lookup_permit = if primary {
        None
    } else {
        DNS_LOOKUP_LIMITER.clone().acquire_owned().await.ok()
    };

    let attempts = if primary { 4 } else { 2 };
    let mut last_error = None;
    for attempt in 0..attempts {
        match tokio::net::lookup_host((host, port)).await {
            Ok(resolved) => {
                let addrs = resolved.collect::<Vec<_>>();
                anyhow::ensure!(!addrs.is_empty(), "no addresses resolved for {host}:{port}");
                for addr in &addrs {
                    ensure_public_ip(addr.ip(), host)?;
                }
                store_dns_addrs(cache_key, addrs.clone());
                return Ok(addrs);
            }
            Err(e) => {
                last_error = Some(e);
                if attempt + 1 < attempts {
                    let delay_ms = if primary { 25 * (attempt + 1) } else { 75 };
                    tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
                }
            }
        }
    }

    if let Some(addrs) = cached_dns_addrs(&cache_key, DNS_STALE_TTL) {
        return Ok(addrs);
    }

    let error = last_error
        .map(|e| e.to_string())
        .unwrap_or_else(|| "unknown resolver error".to_string());
    Err(anyhow::anyhow!("resolving {host}:{port}: {error}"))
}

pub(crate) async fn connect_public_tcp_with_priority(
    host: &str,
    port: u16,
    primary: bool,
) -> Result<TcpStream> {
    let addrs = resolve_public_addrs_with_priority(host, port, primary).await?;
    TcpStream::connect(addrs.as_slice())
        .await
        .map_err(|e| anyhow::anyhow!("connect to {host}:{port} failed: {e}"))
}

fn dns_cache_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.trim_end_matches('.').to_ascii_lowercase())
}

fn cached_dns_addrs(key: &str, max_age: Duration) -> Option<Vec<SocketAddr>> {
    // LRU `get` requires &mut self because it touches recency order.
    let mut cache = DNS_CACHE.lock().ok()?;
    let entry = cache.get(key)?;
    if entry.stored_at.elapsed() <= max_age {
        Some(entry.addrs.clone())
    } else {
        None
    }
}

fn store_dns_addrs(key: String, addrs: Vec<SocketAddr>) {
    if let Ok(mut cache) = DNS_CACHE.lock() {
        cache.put(
            key,
            DnsCacheEntry {
                addrs,
                stored_at: Instant::now(),
            },
        );
    }
}

/// Clear process-wide DNS state as part of a soft daemon refresh.
pub(crate) fn clear_dns_cache() {
    if let Ok(mut cache) = DNS_CACHE.lock() {
        cache.clear();
    }
}

fn ensure_public_ip(ip: IpAddr, host: &str) -> Result<()> {
    anyhow::ensure!(
        !is_restricted_ip(ip),
        "destination {host} resolved to restricted address {ip}"
    );
    Ok(())
}

fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_restricted_ipv4(ip),
        IpAddr::V6(ip) => is_restricted_ipv6(ip),
    }
}

fn is_restricted_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 224
}

fn is_restricted_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_restricted_ipv4(mapped);
    }

    let segments = ip.segments();
    // ::ffff:0:0/96 — IPv4-translated (RFC 8215). Distinct from `::ffff:0:0/96`
    // IPv4-mapped which `to_ipv4_mapped` already handles; here we cover the
    // newer "translated" form some stacks emit.
    let is_ipv4_translated = segments[0..5].iter().all(|&s| s == 0)
        && segments[5] == 0xffff
        && segments[6] == 0
        && segments[7] == 0
        || (segments[0..4].iter().all(|&s| s == 0) && segments[4] == 0xffff && segments[5] == 0);
    // 64:ff9b::/96 — NAT64 well-known prefix. Can reach RFC1918 IPv4 via DNS64.
    let is_nat64 = segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0; 4];
    // 100::/64 — discard-only address block (RFC 6666).
    let is_discard = segments[0] == 0x0100 && segments[1..4] == [0; 3];
    // 2002::/16 — 6to4 (RFC 3056) encapsulates an IPv4 address in bytes 2..6.
    // Conservatively restrict all 6to4 to avoid an IPv4 SSRF bypass via this
    // tunnel (real public traffic should be native v6 or v4), which necessarily
    // also covers the case where the embedded IPv4 is itself restricted.
    let is_6to4 = segments[0] == 0x2002;

    ip == Ipv6Addr::LOCALHOST
        || ip == Ipv6Addr::UNSPECIFIED
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || is_ipv4_translated
        || is_nat64
        || is_discard
        || is_6to4
}

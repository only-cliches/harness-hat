use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::activity::{Activity, ActivityState};
use crate::config;
use crate::proxy::helpers::{
    proxy_authorization_matches_token, tunnel_with_activity, write_error_any,
};
use crate::proxy::http::{
    finish_blocked_network_activity, network_policy_allows, parse_connect_target,
    parse_request_line_and_headers, read_request_head_any,
};
use crate::proxy::{ProxyState, SourceIdentityStatus};
use crate::rules::NetworkPolicy;

#[cfg(test)]
pub(crate) fn parse_sni_from_tls_client_hello(record: &[u8]) -> Option<String> {
    if record.len() < 5 + 4 {
        return None;
    }
    if record[0] != 0x16 {
        return None;
    }
    let rec_len = u16::from_be_bytes([record[3], record[4]]) as usize;
    if record.len() < 5 + rec_len {
        return None;
    }
    let mut i = 5;
    if record.get(i)? != &0x01 {
        return None;
    }
    i += 1;
    let hs_len = ((record.get(i)? as &u8).to_owned() as usize) << 16
        | (((record.get(i + 1)? as &u8).to_owned() as usize) << 8)
        | (record.get(i + 2)? as &u8).to_owned() as usize;
    i += 3;
    if record.len() < i + hs_len {
        return None;
    }
    i += 2 + 32;
    let sid_len = *record.get(i)? as usize;
    i += 1 + sid_len;
    let cs_len = u16::from_be_bytes([*record.get(i)?, *record.get(i + 1)?]) as usize;
    i += 2 + cs_len;
    let comp_len = *record.get(i)? as usize;
    i += 1 + comp_len;
    let ext_len = u16::from_be_bytes([*record.get(i)?, *record.get(i + 1)?]) as usize;
    i += 2;
    let ext_end = i + ext_len;
    if record.len() < ext_end {
        return None;
    }
    while i + 4 <= ext_end {
        let et = u16::from_be_bytes([record[i], record[i + 1]]);
        let el = u16::from_be_bytes([record[i + 2], record[i + 3]]) as usize;
        i += 4;
        if i + el > ext_end {
            return None;
        }
        if et == 0x0000 && el >= 2 {
            let list_len = u16::from_be_bytes([record[i], record[i + 1]]) as usize;
            let mut j = i + 2;
            let list_end = j + list_len;
            if list_end > i + el {
                return None;
            }
            while j + 3 <= list_end {
                let name_type = record[j];
                let name_len = u16::from_be_bytes([record[j + 1], record[j + 2]]) as usize;
                j += 3;
                if j + name_len > list_end {
                    return None;
                }
                if name_type == 0 {
                    // RFC 6066: SNI host_name is an ASCII (A-label) string. Reject
                    // anything non-UTF-8 / non-ASCII rather than lossily mapping it
                    // to U+FFFD, which would otherwise become a bogus policy match
                    // key and certificate subject.
                    let sni = std::str::from_utf8(&record[j..j + name_len]).ok()?;
                    if !sni.is_empty() && sni.is_ascii() {
                        return Some(sni.to_string());
                    }
                }
                j += name_len;
            }
        }
        i += el;
    }
    None
}

// ── HTTPS CONNECT tunnel ──────────────────────────────────────────────────────

pub(crate) async fn handle_connect(mut stream: TcpStream, state: ProxyState) -> Result<()> {
    let (head, connect_remainder) =
        read_request_head_any(&mut stream, state.proxy_idle_timeout()).await?;
    let head_str = std::str::from_utf8(&head).unwrap_or("");

    let (raw_host, port) = parse_connect_target(head_str)
        .ok_or_else(|| anyhow::anyhow!("malformed CONNECT request"))?;
    // Canonicalize exactly like the plain-HTTP path (IDNA/punycode folding,
    // trailing-dot strip, control-char and mixed-script rejection). Without
    // this, an HTTPS CONNECT could evade an ASCII/punycode denylist entry via a
    // Unicode homoglyph or trailing-dot form (H1).
    let host = match crate::proxy::helpers::canonicalize_host(&raw_host) {
        Ok(h) => h,
        Err(_) => {
            write_error_any(&mut stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };
    let (_, _, connect_headers) = parse_request_line_and_headers(head_str)
        .ok_or_else(|| anyhow::anyhow!("malformed CONNECT request"))?;
    let (source_workspace, source_container, source_status, connect_has_proxy_authorization): (
        Option<String>,
        Option<String>,
        SourceIdentityStatus,
        bool,
    ) = if let Some(fixed) = &state.fixed_source {
        if !proxy_authorization_matches_token(&connect_headers, &fixed.auth_token) {
            write_error_any(&mut stream, 407, "Proxy Authentication Required").await?;
            return Ok(());
        }
        (
            Some(fixed.workspace_name.clone()),
            Some(fixed.container.clone()),
            SourceIdentityStatus::ListenerBoundSource,
            true,
        )
    } else {
        write_error_any(&mut stream, 407, "Proxy Authentication Required").await?;
        return Ok(());
    };
    let Some(_source_permit) = state
        .try_acquire_source_connection(source_workspace.as_deref(), source_container.as_deref())
    else {
        warn!(
            host = %host,
            port,
            source_workspace = ?source_workspace,
            source_container = ?source_container,
            source_status = source_status.as_str(),
            connect_has_proxy_authorization,
            "proxy source connection limit reached"
        );
        write_error_any(&mut stream, 503, "Proxy connection limit reached").await?;
        return Ok(());
    };

    let connect_protocol = if port == 443 {
        "connect"
    } else {
        "connect-tcp"
    };
    let connect_activity = state.start_network_activity(
        source_workspace.clone(),
        source_container.clone(),
        "CONNECT",
        &host,
        "/",
        connect_protocol,
        &[],
        &[],
        ActivityState::Forwarding,
    );
    state.activity_line(&connect_activity.id, format!("target {host}:{port}"));

    let cfg = state.config.get();
    if let Err(e) = state
        .config
        .ensure_rules_trusted_for_workspace(source_workspace.as_deref())
    {
        warn!("proxy rules are locked: {e}");
        state.activity_finished(
            &connect_activity.id,
            ActivityState::Denied,
            Some("rules file change requires review".to_string()),
        );
        write_error_any(&mut stream, 403, "Rules file change requires review").await?;
        return Ok(());
    }
    let rules = match config::load_composed_rules_for_workspace(&cfg, source_workspace.as_deref()) {
        Ok(rules) => rules,
        Err(e) => {
            warn!("proxy rules load error: {e}");
            state.activity_finished(
                &connect_activity.id,
                ActivityState::Failed,
                Some("invalid harness-rules.toml configuration".to_string()),
            );
            write_error_any(&mut stream, 500, "Invalid harness-rules.toml configuration").await?;
            return Ok(());
        }
    };
    if let Err(e) = state
        .config
        .ensure_rules_trusted_for_workspace(source_workspace.as_deref())
    {
        warn!("proxy rules changed while loading: {e}");
        state.activity_finished(
            &connect_activity.id,
            ActivityState::Denied,
            Some("rules file change requires review".to_string()),
        );
        write_error_any(&mut stream, 403, "Rules file change requires review").await?;
        return Ok(());
    }
    // Deny wins over allow: consult the denylist first so an explicit deny rule
    // cannot be overridden by an `allowed_hosts` entry (H2).
    let rule_policy = rules.match_connect(&host, port);
    let preflight_policy = if rule_policy == NetworkPolicy::Deny {
        NetworkPolicy::Deny
    } else if crate::proxy::helpers::is_host_allowed(&cfg, source_container.as_deref(), &host) {
        info!(
            host = %host,
            "CONNECT request for allowed host"
        );
        NetworkPolicy::Auto
    } else {
        rule_policy
    };
    if preflight_policy != NetworkPolicy::Deny {
        if let Err(e) = state.resolve_request_addrs(&host, port).await {
            state.activity_finished(
                &connect_activity.id,
                ActivityState::Denied,
                Some(e.to_string()),
            );
            write_error_any(&mut stream, 403, "Forbidden by harness-hat policy").await?;
            return Ok(());
        }
    }
    let preflight_allowed = network_policy_allows(
        &state,
        &connect_activity,
        preflight_policy,
        "waiting for CONNECT approval",
        "CONNECT",
        &host,
        Some(port),
        "/",
        source_workspace.clone(),
        source_container.clone(),
        source_status.as_str(),
        connect_has_proxy_authorization,
    )
    .await;
    if !preflight_allowed {
        finish_blocked_network_activity(&state, &connect_activity);
        write_error_any(&mut stream, 403, "Forbidden by harness-hat policy").await?;
        return Ok(());
    }

    info!(
        host = %host,
        port,
        source_workspace = ?source_workspace,
        source_container = ?source_container,
        source_status = source_status.as_str(),
        connect_has_proxy_authorization,
        "proxy CONNECT tunnel path"
    );
    tunnel_connect(
        &state,
        &connect_activity,
        &mut stream,
        &host,
        port,
        "CONNECT tunnel",
        &connect_remainder,
    )
    .await
}

async fn tunnel_connect(
    state: &ProxyState,
    activity: &Activity,
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    label: &str,
    connect_remainder: &[u8],
) -> Result<()> {
    state.activity_state(
        &activity.id,
        ActivityState::Forwarding,
        Some(format!("tunneling {host}:{port}")),
    );

    let mut upstream = match state.connect_public_tcp(host, port).await {
        Ok(upstream) => upstream,
        Err(e) => {
            let message = format!("{label} connect to {host}:{port} failed: {e}");
            state.activity_finished(&activity.id, ActivityState::Failed, Some(message.clone()));
            return Err(anyhow::anyhow!(message));
        }
    };

    if let Err(e) = stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
    {
        state.activity_finished(&activity.id, ActivityState::Failed, Some(e.to_string()));
        return Err(e.into());
    }

    if !connect_remainder.is_empty()
        && let Err(e) = upstream.write_all(connect_remainder).await
    {
        state.activity_finished(&activity.id, ActivityState::Failed, Some(e.to_string()));
        return Err(e.into());
    }

    tunnel_with_activity(state, activity, stream, &mut upstream).await
}

#[cfg(test)]
mod tests {
    use crate::proxy::connect::parse_sni_from_tls_client_hello;
    use crate::proxy::core::{NetworkDecision, ProxyState, SourceIdentityStatus, SourcePriority};
    use crate::proxy::helpers::{
        canonicalize_host, copy_bidirectional_with_idle_timeout,
        decode_source_from_proxy_authorization, ensure_host_header_matches_target,
        format_byte_count, host_matches_pattern, proxy_authorization_matches_token,
        resolve_public_addrs, split_host_port,
    };
    use crate::proxy::http::{prompt_network, read_body_any, read_request_head_any};
    use crate::shared_config::SharedConfig;
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    fn test_temp_root() -> std::path::PathBuf {
        if cfg!(unix) {
            let unix_tmp = Path::new("/tmp");
            if unix_tmp.exists() {
                return unix_tmp.to_path_buf();
            }
        }
        std::env::temp_dir()
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before unix epoch")
            .as_nanos();
        let dir = test_temp_root().join(format!("harness-hat-{prefix}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn with_temp_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _guard = crate::TEST_ENV_LOCK.lock().expect("test env lock");
        let original_home = std::env::var_os("HOME");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        let result = f();
        match original_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        match original_xdg_data_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_DATA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_DATA_HOME");
            },
        }
        result
    }

    #[test]
    fn decode_source_from_proxy_authorization_works() {
        let auth_payload = format!(
            "zcsrc:{}.{}",
            URL_SAFE_NO_PAD.encode("myproj"),
            URL_SAFE_NO_PAD.encode("mycont")
        );
        let header_value = format!("Basic {}", STANDARD.encode(auth_payload));
        let (project, container, status) = decode_source_from_proxy_authorization(&header_value);
        assert_eq!(status, SourceIdentityStatus::Ok);
        assert_eq!(project, Some("myproj".to_string()));
        assert_eq!(container, Some("mycont".to_string()));
    }

    #[test]
    fn proxy_authorization_matches_url_encoded_basic_credentials() {
        let header_value = format!("Basic {}", STANDARD.encode("harness%2Dhat:testtoken"));
        let headers = vec![("Proxy-Authorization".to_string(), header_value)];
        assert!(proxy_authorization_matches_token(&headers, "testtoken"));
        assert!(!proxy_authorization_matches_token(&headers, "wrong-token"));
    }

    #[test]
    fn source_connection_limit_is_per_container_identity() {
        let raw = r#"
docker_dir = "/tmp"
[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state =
            ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx).unwrap();

        let permits = (0..32)
            .map(|_| {
                state
                    .try_acquire_source_connection(Some("p"), Some("c"))
                    .expect("permit should be available")
            })
            .collect::<Vec<_>>();
        assert!(
            state
                .try_acquire_source_connection(Some("p"), Some("c"))
                .is_none()
        );
        assert!(
            state
                .try_acquire_source_connection(Some("p"), Some("other"))
                .is_some()
        );
        drop(permits);
        assert!(
            state
                .try_acquire_source_connection(Some("p"), Some("c"))
                .is_some()
        );
    }

    #[test]
    fn scoped_source_connection_limit_is_per_session_token() {
        let raw = r#"
docker_dir = "/tmp"
[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state =
            ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx).unwrap();
        let first = state.with_fixed_source("p", "tool", "session-a", SourcePriority::Limited);
        let second = state.with_fixed_source("p", "tool", "session-b", SourcePriority::Limited);

        let permits = (0..32)
            .map(|_| {
                first
                    .try_acquire_source_connection(Some("p"), Some("tool"))
                    .expect("permit should be available")
            })
            .collect::<Vec<_>>();
        assert!(
            first
                .try_acquire_source_connection(Some("p"), Some("tool"))
                .is_none()
        );
        assert!(
            second
                .try_acquire_source_connection(Some("p"), Some("tool"))
                .is_some(),
            "same profile name in another scoped listener must have its own source bucket"
        );
        drop(permits);
    }

    #[test]
    fn primary_source_connections_are_not_limited_by_proxy_admission() {
        let raw = r#"
docker_dir = "/tmp"
[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state =
            ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx).unwrap();
        let primary =
            state.with_fixed_source("p", "primary", "primary-session", SourcePriority::Primary);

        let primary_permits = (0..256)
            .map(|_| {
                primary
                    .try_acquire_source_connection(Some("p"), Some("primary"))
                    .expect("primary permit should be available")
            })
            .collect::<Vec<_>>();
        assert!(
            primary
                .try_acquire_source_connection(Some("p"), Some("primary"))
                .is_some(),
            "primary source connections should not be rejected by harness-hat admission limits"
        );
        drop(primary_permits);
    }

    #[test]
    fn limited_source_connection_limit_is_strict() {
        let raw = r#"
docker_dir = "/tmp"
[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state =
            ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx).unwrap();
        let limited =
            state.with_fixed_source("p", "tool", "limited-session", SourcePriority::Limited);

        let limited_permits = (0..32)
            .map(|_| {
                limited
                    .try_acquire_source_connection(Some("p"), Some("tool"))
                    .expect("limited permit should be available")
            })
            .collect::<Vec<_>>();
        assert!(
            limited
                .try_acquire_source_connection(Some("p"), Some("tool"))
                .is_none()
        );
        drop(limited_permits);
    }

    #[test]
    fn limiteds_cannot_exhaust_primary_scoped_proxy_capacity() {
        let raw = r#"
docker_dir = "/tmp"
[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state =
            ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx).unwrap();
        let primary =
            state.with_fixed_source("p", "primary", "primary-session", SourcePriority::Primary);
        let limited =
            state.with_fixed_source("p", "tool", "limited-session", SourcePriority::Limited);

        let mut limited_permits = Vec::new();
        while let Some(permit) = limited.try_acquire_connection() {
            limited_permits.push(permit);
            assert!(
                limited_permits.len() < 1000,
                "limited limiter should saturate well before this point"
            );
        }
        assert!(
            primary.try_acquire_connection().is_some(),
            "primary containers should retain reserved scoped proxy capacity when limiteds are saturated"
        );
        for _ in 0..1000 {
            assert!(
                primary.try_acquire_connection().is_some(),
                "primary proxy listener connections should not be rejected by harness-hat admission limits"
            );
        }
        drop(limited_permits);
    }

    #[test]
    fn format_byte_count_uses_compact_units() {
        assert_eq!(format_byte_count(0), "0b");
        assert_eq!(format_byte_count(512), "512b");
        assert_eq!(format_byte_count(1024), "1kb");
        assert_eq!(format_byte_count(1536), "1.5kb");
        assert_eq!(format_byte_count(10 * 1024), "10kb");
        assert_eq!(format_byte_count(1024 * 1024), "1mb");
    }

    #[tokio::test]
    async fn read_body_any_rejects_oversized_content_length() {
        let mut stream = tokio::io::empty();
        let headers = vec![("Content-Length".to_string(), "20000000".to_string())];

        let err = read_body_any(
            &mut stream,
            &headers,
            Vec::new(),
            std::time::Duration::from_secs(30),
        )
        .await
        .expect_err("oversized body should fail");

        assert!(
            err.to_string().contains("request body too large"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn tunnel_closes_after_the_configured_idle_timeout() {
        let (_client, mut downstream) = tokio::io::duplex(64);
        let (_server, mut upstream) = tokio::io::duplex(64);

        let err = copy_bidirectional_with_idle_timeout(
            &mut downstream,
            &mut upstream,
            std::time::Duration::from_millis(10),
        )
        .await
        .expect_err("an inactive tunnel should time out");

        assert!(err.to_string().contains("inactivity"));
    }

    #[tokio::test]
    async fn proxy_request_reads_time_out_when_client_stalls() {
        let (_client, mut stream) = tokio::io::duplex(64);
        let err = read_request_head_any(&mut stream, Duration::from_millis(20))
            .await
            .expect_err("incomplete request head should time out");
        assert!(err.to_string().contains("reading request head"));

        let (_client, mut stream) = tokio::io::duplex(64);
        let headers = vec![("Content-Length".to_string(), "4".to_string())];
        let err = read_body_any(
            &mut stream,
            &headers,
            b"ab".to_vec(),
            Duration::from_millis(20),
        )
        .await
        .expect_err("incomplete request body should time out");
        assert!(err.to_string().contains("reading request body"));
    }

    #[tokio::test]
    async fn tunnel_activity_extends_idle_deadline() {
        let (mut client, mut downstream) = tokio::io::duplex(64);
        let (mut server, mut upstream) = tokio::io::duplex(64);
        let relay = tokio::spawn(async move {
            copy_bidirectional_with_idle_timeout(
                &mut downstream,
                &mut upstream,
                Duration::from_millis(500),
            )
            .await
        });

        for _ in 0..4 {
            client.write_all(b"x").await.unwrap();
            let mut byte = [0];
            server.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, *b"x");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(
            !relay.is_finished(),
            "active tunnel closed before becoming idle"
        );

        drop(client);
        drop(server);
        assert_eq!(relay.await.unwrap().unwrap(), (4, 0));
    }

    #[tokio::test]
    async fn configured_localhost_forward_allows_scoped_proxy_loopback() {
        let raw = r#"
version = 1
docker_dir = "/tmp"

[manager]
global_rules_file = "/tmp/global.toml"

[container_profiles.pi]
image = "default"

[[container_profiles.pi.localhost_forwards]]
container_port = 8081
host_port = 18081
"#;
        let root = unique_temp_dir("proxy-test-config");
        let config_path = root.join("harness-hat.toml");
        std::fs::write(&config_path, raw).expect("write config");
        let cfg = with_temp_home(&root, || crate::config::load(&config_path)).expect("load config");
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state = ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx)
            .unwrap()
            .with_fixed_source("workspace", "pi", "session", SourcePriority::Primary);

        assert!(state.has_configured_localhost_forward("localhost", 8081));
        assert!(state.has_configured_localhost_forward("host.docker.internal", 8081));
        assert!(!state.has_configured_localhost_forward("localhost", 8082));

        let addrs = state
            .resolve_request_addrs("localhost", 8081)
            .await
            .expect("configured loopback forward should resolve");
        assert!(
            addrs
                .iter()
                .any(|addr| addr.ip().is_loopback() && addr.port() == 18081),
            "expected host loopback address on forwarded host port, got {addrs:?}"
        );

        let overridden = state.with_fixed_source_and_forwards(
            "workspace",
            "pi",
            "session",
            SourcePriority::Primary,
            vec![crate::config::LocalhostForward {
                container_port: 8081,
                host_port: Some(28081),
            }],
        );
        let overridden_addrs = overridden
            .resolve_request_addrs("localhost", 8081)
            .await
            .expect("rules-file forward override should resolve");
        assert!(
            overridden_addrs
                .iter()
                .any(|addr| addr.ip().is_loopback() && addr.port() == 28081),
            "expected rules-file override on forwarded host port, got {overridden_addrs:?}"
        );

        let err = state
            .resolve_request_addrs("localhost", 8082)
            .await
            .expect_err("unconfigured loopback should remain blocked");
        assert!(
            err.to_string().contains("restricted address"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn split_host_port_handles_authorities() {
        assert_eq!(
            split_host_port("example.com:8443", 443),
            ("example.com".to_string(), 8443)
        );
        assert_eq!(
            split_host_port("[2001:db8::1]:443", 80),
            ("2001:db8::1".to_string(), 443)
        );
        assert_eq!(
            split_host_port("2001:db8::1", 443),
            ("2001:db8::1".to_string(), 443)
        );
    }

    #[test]
    fn host_header_must_match_tls_target() {
        let headers = vec![("Host".to_string(), "Example.com:443".to_string())];
        ensure_host_header_matches_target(&headers, "example.com", 443)
            .expect("matching host header should be accepted");

        let headers = vec![("Host".to_string(), "example.com".to_string())];
        ensure_host_header_matches_target(&headers, "example.com", 443)
            .expect("missing default port should be accepted");

        let headers = vec![("Host".to_string(), "[2001:db8::1]:443".to_string())];
        ensure_host_header_matches_target(&headers, "2001:0db8::1", 443)
            .expect("equivalent IPv6 host header should be accepted");
    }

    #[test]
    fn host_header_rejects_mismatches_and_duplicates() {
        let headers = vec![("Host".to_string(), "blocked.example".to_string())];
        assert!(
            ensure_host_header_matches_target(&headers, "allowed.example", 443).is_err(),
            "mismatched host header should be rejected"
        );

        let headers = vec![("Host".to_string(), "example.com:444".to_string())];
        assert!(
            ensure_host_header_matches_target(&headers, "example.com", 443).is_err(),
            "mismatched host port should be rejected"
        );

        let headers = vec![
            ("Host".to_string(), "example.com".to_string()),
            ("host".to_string(), "example.com".to_string()),
        ];
        assert!(
            ensure_host_header_matches_target(&headers, "example.com", 443).is_err(),
            "duplicate host headers should be rejected"
        );
    }

    #[tokio::test]
    async fn resolve_public_addrs_rejects_restricted_ip_literals() {
        let err = resolve_public_addrs("127.0.0.1", 80)
            .await
            .expect_err("loopback should be rejected");
        assert!(
            err.to_string().contains("restricted address"),
            "unexpected error: {err}"
        );

        let err = resolve_public_addrs("::ffff:127.0.0.1", 80)
            .await
            .expect_err("IPv4-mapped loopback should be rejected");
        assert!(
            err.to_string().contains("restricted address"),
            "unexpected error: {err}"
        );

        resolve_public_addrs("8.8.8.8", 53)
            .await
            .expect("public IP literal should be accepted");
    }

    #[tokio::test]
    async fn prompt_network_sends_to_pending_tx() {
        let (_ca_tx, _ca_rx) = mpsc::channel::<()>(1); // dummy
        let (pending_tx, mut pending_rx) = mpsc::channel(1);
        // Wait, I can just use build_test_app logic if I want but let's just make a dummy config.
        let raw = r#"
docker_dir = "/tmp"
[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state =
            ProxyState::new(SharedConfig::new(Arc::new(cfg)), pending_tx, activity_tx).unwrap();

        let prompt_task = tokio::spawn(async move {
            prompt_network(
                &state,
                "GET",
                "example.com",
                Some(443),
                "/test",
                Some("p".into()),
                Some("c".into()),
                "ok",
                true,
                None,
            )
            .await
        });

        // TUI side: receive the item
        let item = pending_rx
            .recv()
            .await
            .expect("should receive pending item");
        assert_eq!(item.host, "example.com");
        assert_eq!(item.port, Some(443));

        // TUI side: allow it
        item.response_tx.send(NetworkDecision::Allow).unwrap();

        let result = prompt_task.await.unwrap();
        assert!(result, "prompt_network should return true for Allow");
    }

    #[test]
    fn canonicalize_host_normalizes_case_dot_and_idna() {
        assert_eq!(canonicalize_host("EXAMPLE.COM").unwrap(), "example.com");
        assert_eq!(canonicalize_host("example.com.").unwrap(), "example.com");
        // Unicode is IDNA-encoded to its punycode A-label.
        assert_eq!(
            canonicalize_host("bücher.example").unwrap(),
            "xn--bcher-kva.example"
        );
        assert!(canonicalize_host("").is_err());
        assert!(canonicalize_host("bad host").is_err());
    }

    #[test]
    fn allowed_host_wildcards_are_subdomain_only_like_network_rules() {
        assert!(host_matches_pattern("*.example.com", "api.example.com"));
        assert!(!host_matches_pattern("*.example.com", "example.com"));
        assert!(host_matches_pattern("example.com", "example.com"));
    }

    #[test]
    fn sni_parser_rejects_non_ascii_names() {
        // Minimal TLS 1.2 ClientHello carrying a single SNI host_name extension.
        fn client_hello_with_sni(name: &[u8]) -> Vec<u8> {
            let mut ext_body = Vec::new();
            let entry_len = 3 + name.len();
            ext_body.extend_from_slice(&(entry_len as u16).to_be_bytes()); // server_name_list len
            ext_body.push(0); // name_type = host_name
            ext_body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            ext_body.extend_from_slice(name);

            let mut ext = Vec::new();
            ext.extend_from_slice(&0x0000u16.to_be_bytes()); // ext type = server_name
            ext.extend_from_slice(&(ext_body.len() as u16).to_be_bytes());
            ext.extend_from_slice(&ext_body);

            let mut body = Vec::new();
            body.extend_from_slice(&[0x03, 0x03]); // client_version
            body.extend_from_slice(&[0u8; 32]); // random
            body.push(0); // session_id len
            body.extend_from_slice(&2u16.to_be_bytes()); // cipher_suites len
            body.extend_from_slice(&[0x00, 0x2f]);
            body.push(1); // compression methods len
            body.push(0);
            body.extend_from_slice(&(ext.len() as u16).to_be_bytes()); // extensions len
            body.extend_from_slice(&ext);

            let mut hs = Vec::new();
            hs.push(0x01); // handshake type = ClientHello
            let len = body.len();
            hs.push((len >> 16) as u8);
            hs.push((len >> 8) as u8);
            hs.push(len as u8);
            hs.extend_from_slice(&body);

            let mut record = Vec::new();
            record.push(0x16); // handshake record
            record.extend_from_slice(&[0x03, 0x01]); // record version
            record.extend_from_slice(&(hs.len() as u16).to_be_bytes());
            record.extend_from_slice(&hs);
            record
        }

        assert_eq!(
            parse_sni_from_tls_client_hello(&client_hello_with_sni(b"example.com")).as_deref(),
            Some("example.com")
        );
        // Invalid UTF-8 / non-ASCII SNI is rejected rather than lossily mapped.
        assert_eq!(
            parse_sni_from_tls_client_hello(&client_hello_with_sni(&[0xff, 0xfe, b'.', b'x'])),
            None
        );
        assert_eq!(
            parse_sni_from_tls_client_hello(&client_hello_with_sni("café.example".as_bytes())),
            None
        );
    }
}

//! End-to-end streaming stress coverage through the public chat-completions API.

use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::Body,
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use bytes::Bytes;
use futures::StreamExt;
use kinetix::{
    app::AppState,
    config::Config,
    crypto::{self, Crypto},
    db,
    logqueue::UsageLogQueue,
    paths::Paths,
    registry::Registry,
    types::{AuthScheme, WireFormat},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const CLIENT_KEY: &str = "sk-kinetix-streaming-stress-test";
const ROUTE: &str = "streaming-stress-route";
const FALLBACK_ROUTE: &str = "streaming-reset-route";
const BACKPRESSURE_EVENTS: usize = 1_024;
const BACKPRESSURE_EVENT_BYTES: usize = 32 * 1024;
const LARGE_FRAME_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct Probe {
    requests: Mutex<Vec<Value>>,
    streams_started: AtomicUsize,
    chunks_generated: AtomicUsize,
    streams_dropped: AtomicUsize,
    reset_after_commit_gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[derive(Clone, Default)]
struct MockUpstream(Arc<Probe>);

struct StreamDropCounter(Arc<Probe>);

impl Drop for StreamDropCounter {
    fn drop(&mut self) {
        self.0.streams_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn body_from_chunks(chunks: Vec<Bytes>, probe: Arc<Probe>, delay: Duration) -> Body {
    Body::from_stream(async_stream::stream! {
        let _drop_counter = StreamDropCounter(probe.clone());
        probe.streams_started.fetch_add(1, Ordering::Relaxed);
        for chunk in chunks {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
            yield Ok::<_, io::Error>(chunk);
        }
    })
}

fn body_ending_in_reset(chunk: Bytes, probe: Arc<Probe>) -> Body {
    Body::from_stream(async_stream::stream! {
        let _drop_counter = StreamDropCounter(probe.clone());
        probe.streams_started.fetch_add(1, Ordering::Relaxed);
        probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
        yield Ok::<_, io::Error>(chunk);
        yield Err(io::Error::new(io::ErrorKind::ConnectionReset, "synthetic upstream reset"));
    })
}

fn body_ending_in_reset_after_release(
    chunk: Bytes,
    probe: Arc<Probe>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> Body {
    Body::from_stream(async_stream::stream! {
        let _drop_counter = StreamDropCounter(probe.clone());
        probe.streams_started.fetch_add(1, Ordering::Relaxed);
        probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
        yield Ok::<_, io::Error>(chunk);
        let _ = release.await;
        yield Err(io::Error::new(io::ErrorKind::ConnectionReset, "synthetic upstream reset"));
    })
}

fn response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
}

fn event(text: &str) -> Bytes {
    Bytes::from(format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(text).unwrap()
    ))
}

async fn insert_provider_model(
    pool: &sqlx::SqlitePool,
    crypto: &Crypto,
    name: &str,
    base_url: &str,
    upstream_model: &str,
) -> (String, String, String) {
    let provider_id = db::insert_provider(
        pool,
        &db::NewProvider {
            name,
            base_url,
            wire_format: WireFormat::Openai,
            auth_scheme: AuthScheme::Bearer,
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: json!({}),
            timeout_ms: 500,
            capability_mode: "permissive",
            models_path: None,
            rate_limit_rules: json!({}),
            follow_redirects: false,
            credential_hosts: "",
            allow_insecure_tls: true,
            wire_plugin: "",
            credential_plugin: "",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();
    let account_name = format!("{name}-account");
    let secret = format!("{name}-secret");
    let encrypted_secret = crypto.encrypt(&secret).unwrap();
    let account_id = db::insert_account(
        pool,
        &provider_id,
        &account_name,
        &encrypted_secret,
        &secret,
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let model_id = db::insert_model(
        pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: upstream_model,
            display_name: "Streaming stress model",
            enabled: true,
            context_window: Some(32_000),
            max_output_tokens: Some(8_000),
            capabilities: json!({"text": true, "reasoning": true, "tools": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();
    (provider_id, account_id, model_id)
}

async fn upstream(
    State(mock): State<MockUpstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let case = body
        .pointer("/messages/0/content")
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("case:"))
        .unwrap_or("unknown")
        .to_owned();
    let reset_fallback = body["model"] == "reset-fallback-model";
    mock.0.requests.lock().await.push(body);
    assert!(headers.contains_key(AUTHORIZATION));

    match case.as_str() {
        "fragmented" => {
            let wire = concat!(
                "event: delta\r\n",
                "data: {\"id\":\"chatcmpl-fragmented\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking→🙂\"}}],\"vendor_extension\":{\"v\":1}}\r\n\r\n",
                "data: {\"id\":\"chatcmpl-fragmented\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
                "data: {\"id\":\"chatcmpl-fragmented\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Paris\\\"}\"}}]}}]}\r\n\r\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\r\n\r\n"
            );
            let bytes = wire.as_bytes();
            let sizes = [1, 2, 3, 5, 8, 13];
            let utf8_start = wire.find("→🙂").unwrap();
            let utf8_end = utf8_start + "→🙂".len();
            let mut chunks = Vec::new();
            let mut offset = 0;
            let mut index = 0;
            while offset < bytes.len() {
                let mut size = sizes[index % sizes.len()];
                if offset < utf8_start {
                    size = size.min(utf8_start - offset);
                } else if offset < utf8_end {
                    size = 1;
                }
                let end = (offset + size).min(bytes.len());
                chunks.push(Bytes::copy_from_slice(&bytes[offset..end]));
                offset = end;
                index += 1;
            }
            response(body_from_chunks(chunks, mock.0.clone(), Duration::ZERO))
        }
        "large_frame" => {
            let text = "x".repeat(LARGE_FRAME_BYTES);
            let mut chunks = vec![event(&text)];
            chunks.push(Bytes::from_static(b"data: [DONE]\n\n"));
            response(body_from_chunks(chunks, mock.0.clone(), Duration::ZERO))
        }
        "oversized_frame" => {
            let text = "x".repeat(kinetix::sse::DEFAULT_MAX_FRAME_BYTES + 64);
            response(body_from_chunks(
                vec![event(&text)],
                mock.0.clone(),
                Duration::ZERO,
            ))
        }
        "long_progress" => {
            let chunks = vec![
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"reason-0 \"}}]}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_long\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"reason-1 \"}}]}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Pa\"}}]}}]}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"reason-2 \"}}]}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ris\\\"}\"}}]}}]}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                ),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ];
            response(body_from_chunks(
                chunks,
                mock.0.clone(),
                Duration::from_millis(110),
            ))
        }
        "eof_before_terminal" => response(body_from_chunks(
            vec![event("visible-before-eof")],
            mock.0.clone(),
            Duration::ZERO,
        )),
        "truncated_frame" => response(body_from_chunks(
            vec![
                event("visible-before-truncation"),
                Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"truncated\"}}]"),
            ],
            mock.0.clone(),
            Duration::ZERO,
        )),
        "upstream_error" => response(body_from_chunks(
            vec![
                event("visible-before-error"),
                Bytes::from_static(b"data: {\"error\":{\"message\":\"synthetic failure\"}}\n\n"),
            ],
            mock.0.clone(),
            Duration::ZERO,
        )),
        "reset_before_commit" if reset_fallback => response(body_from_chunks(
            vec![
                event("fallback-after-reset"),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ],
            mock.0.clone(),
            Duration::ZERO,
        )),
        "reset_before_commit" => response(body_ending_in_reset(
            Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"partial-before-reset\"}}]",
            ),
            mock.0.clone(),
        )),
        "reset_after_commit" if reset_fallback => response(body_from_chunks(
            vec![
                event("fallback-after-reset"),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ],
            mock.0.clone(),
            Duration::ZERO,
        )),
        "reset_after_commit" => {
            let release = mock
                .0
                .reset_after_commit_gate
                .lock()
                .await
                .take()
                .expect("test must install the post-commit reset gate");
            response(body_ending_in_reset_after_release(
                event("visible-before-reset"),
                mock.0.clone(),
                release,
            ))
        }
        "idle_timeout" => {
            let chunks = [event("visible-before-timeout")];
            let probe = mock.0.clone();
            let body = Body::from_stream(async_stream::stream! {
                let _drop_counter = StreamDropCounter(probe.clone());
                probe.streams_started.fetch_add(1, Ordering::Relaxed);
                probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
                yield Ok::<_, io::Error>(chunks[0].clone());
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
            response(body)
        }
        "heartbeat_forever" | "disconnect_before_commit" => {
            let probe = mock.0.clone();
            let body = Body::from_stream(async_stream::stream! {
                let _drop_counter = StreamDropCounter(probe.clone());
                probe.streams_started.fetch_add(1, Ordering::Relaxed);
                loop {
                    probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
                    yield Ok::<_, io::Error>(Bytes::from_static(b": keepalive\r\n\r\n"));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
            response(body)
        }
        "disconnect_after_commit" => {
            let probe = mock.0.clone();
            let body = Body::from_stream(async_stream::stream! {
                let _drop_counter = StreamDropCounter(probe.clone());
                probe.streams_started.fetch_add(1, Ordering::Relaxed);
                probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
                yield Ok::<_, io::Error>(event("first-visible-event"));
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
                    yield Ok::<_, io::Error>(event("more"));
                }
            });
            response(body)
        }
        "backpressure" => {
            let probe = mock.0.clone();
            let body = Body::from_stream(async_stream::stream! {
                let _drop_counter = StreamDropCounter(probe.clone());
                probe.streams_started.fetch_add(1, Ordering::Relaxed);
                let frame = event(&"b".repeat(BACKPRESSURE_EVENT_BYTES));
                for _ in 0..BACKPRESSURE_EVENTS {
                    probe.chunks_generated.fetch_add(1, Ordering::Relaxed);
                    yield Ok::<_, io::Error>(frame.clone());
                }
                yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
            });
            response(body)
        }
        _ => (StatusCode::NOT_FOUND, "unknown streaming test case").into_response(),
    }
}

struct Harness {
    state: AppState,
    probe: Arc<Probe>,
    client: reqwest::Client,
    gateway_addr: std::net::SocketAddr,
    upstream_server: tokio::task::JoinHandle<()>,
    gateway_server: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
    account_id: String,
}

async fn setup() -> Harness {
    let mock = MockUpstream::default();
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_mock = mock.clone();
    let upstream_server = tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            Router::new()
                .fallback(any(upstream))
                .with_state(upstream_mock),
        )
        .await
        .unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-streaming-stress-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let paths = Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
    };
    paths.ensure_dirs().unwrap();
    let database_url = paths.database_url();
    let pool = db::connect(&database_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let crypto = Arc::new(Crypto::new(&[23_u8; 32]));
    let upstream_url = format!("http://{upstream_addr}");
    let (_, account_id, model_id) = insert_provider_model(
        &pool,
        &crypto,
        "streaming-stress-upstream",
        &upstream_url,
        "upstream-chat-model",
    )
    .await;
    let (_, _, reset_primary_model_id) = insert_provider_model(
        &pool,
        &crypto,
        "streaming-reset-primary",
        &upstream_url,
        "reset-primary-model",
    )
    .await;
    let (_, _, reset_fallback_model_id) = insert_provider_model(
        &pool,
        &crypto,
        "streaming-reset-fallback",
        &upstream_url,
        "reset-fallback-model",
    )
    .await;
    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: ROUTE,
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
            max_concurrent_requests: Some(1),
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();
    let fallback_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: FALLBACK_ROUTE,
            description: "",
            strategy: "priority",
            fallback_triggers: json!({"on5xx": true}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(2),
            max_concurrent_requests: Some(1),
        },
    )
    .await
    .unwrap();
    db::insert_route_target(
        &pool,
        &fallback_route_id,
        None,
        &reset_primary_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();
    db::insert_route_target(
        &pool,
        &fallback_route_id,
        None,
        &reset_fallback_model_id,
        2,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();
    db::insert_virtual_key(
        &pool,
        &db::VirtualKeyRow {
            id: "streaming-stress-key".into(),
            key_hash: crypto::hash_virtual_key(CLIENT_KEY),
            name: "Streaming stress test".into(),
            owner: "test".into(),
            tag: String::new(),
            allowed_models: json!(["*"]).to_string(),
            allowed_providers: json!([]).to_string(),
            rpm_limit: None,
            tpm_limit: None,
            max_concurrent_requests: Some(1),
            daily_budget: None,
            monthly_budget: None,
            expires_at: None,
            status: "active".into(),
            allowed_ips: json!([]).to_string(),
            body_logging: 0,
            created_at: db::now_iso(),
            revoked_at: None,
        },
    )
    .await
    .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();
    let state = AppState::new(
        Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [23_u8; 32],
            admin_token: "test-admin".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: true,
            max_inflight_inferences: 1,
            allow_insecure_tls: true,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
            alert_webhook_url: None,
            alert_fallback_rate: 1.0,
            alert_error_rate: 1.0,
            alert_min_requests: 1,
            alert_interval_secs: 60,
            alert_p95_latency_ms: 1_000,
            ip_rate_limit_per_min: 0,
            session_ttl_minutes: 60,
            export_retention_days: 1,
            paths,
            generated_admin_password: None,
        }),
        pool.clone(),
        registry,
        crypto,
        reqwest::Client::new(),
        UsageLogQueue::new(pool, 64),
        0,
    );

    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    let gateway = kinetix::router::build(state.clone());
    let gateway_server = tokio::spawn(async move {
        axum::serve(
            kinetix::server::DisconnectAwareListener::new(gateway_listener),
            gateway.into_make_service_with_connect_info::<kinetix::server::ClientConnectionInfo>(),
        )
        .await
        .unwrap();
    });

    Harness {
        state,
        probe: mock.0,
        client: reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap(),
        gateway_addr,
        upstream_server,
        gateway_server,
        root,
        account_id,
    }
}

impl Harness {
    async fn send(&self, case: &str, vendor_extension: Option<Value>) -> reqwest::Response {
        self.send_to_route(ROUTE, case, vendor_extension).await
    }

    async fn send_to_route(
        &self,
        route: &str,
        case: &str,
        vendor_extension: Option<Value>,
    ) -> reqwest::Response {
        let mut body = json!({
            "model": route,
            "messages": [{"role": "user", "content": format!("case:{case}")}],
            "stream": true
        });
        if let Some(extension) = vendor_extension {
            body["vendor_extension"] = extension;
        }
        self.client
            .post(format!("http://{}/v1/chat/completions", self.gateway_addr))
            .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn provider_failures(&self) -> i64 {
        sqlx::query_scalar("SELECT consecutive_failures FROM accounts WHERE id = ?")
            .bind(&self.account_id)
            .fetch_one(&self.state.pool)
            .await
            .unwrap()
    }

    async fn assert_admission_released(&self, expected_failures: i64) {
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            let response = self.send("fragmented", None).await;
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap()
        })
        .await
        .expect("a follow-up request must acquire all released admission permits");
        assert!(!response.is_empty());
        assert_eq!(self.provider_failures().await, expected_failures);
    }

    async fn trace(&self, request_id: &str) -> (Option<String>, String, Option<i64>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let trace = sqlx::query_as::<_, (Option<String>, String, Option<i64>)>(
                    "SELECT stream_outcome, commit_state, fallback_allowed FROM route_traces WHERE request_id = ?",
                )
                .bind(request_id)
                .fetch_optional(&self.state.pool)
                .await
                .unwrap();
                if let Some(trace) = trace {
                    return trace;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Route Trace was not persisted")
    }

    async fn wait_for_counter(&self, counter: &AtomicUsize, above: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while counter.load(Ordering::Relaxed) <= above {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("upstream stream did not reach the expected state");
    }

    async fn close(self) {
        self.gateway_server.abort();
        self.upstream_server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn chat_request(case: &str) -> Value {
    json!({
        "model": ROUTE,
        "messages": [{"role": "user", "content": format!("case:{case}")}],
        "stream": true
    })
}

#[tokio::test]
async fn streaming_transport_and_lifecycle_stress_regressions() {
    let harness = setup().await;

    let extension = json!({"opaque": [1, {"value": "preserve →🙂"}]});
    let response = harness.send("fragmented", Some(extension.clone())).await;
    let status = response.status();
    if status != StatusCode::OK {
        panic!(
            "unexpected status {status}: {}",
            response.text().await.unwrap()
        );
    }
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.bytes().await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("thinking→🙂"), "{body}");
    assert!(body.contains("call_1"), "{body}");
    assert!(body.contains("lookup"), "{body}");
    assert!(body.contains("{\\\"city\\\":"), "{body}");
    assert!(body.contains("\\\"Paris\\\"}"), "{body}");
    assert!(
        body.contains("vendor_extension"),
        "native event extensions survive: {body}"
    );
    assert!(
        !body.contains('\r'),
        "transport framing is normalized to LF"
    );
    let request = harness.probe.requests.lock().await[0].clone();
    assert_eq!(request["vendor_extension"], extension);
    let (outcome, commit, _) = harness.trace(&request_id).await;
    assert_eq!(outcome.as_deref(), Some("completed"));
    assert_eq!(commit, "committed");

    let response = harness.send("large_frame", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.bytes().await.unwrap();
    assert!(body.len() >= LARGE_FRAME_BYTES);
    assert!(body
        .windows(64)
        .any(|window| window.iter().all(|byte| *byte == b'x')));
    assert_eq!(
        harness.trace(&request_id).await.0.as_deref(),
        Some("completed")
    );

    let response = harness.send("long_progress", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let started_at = tokio::time::Instant::now();
    let body = response.bytes().await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(started_at.elapsed() > Duration::from_millis(500));
    assert!(body.contains("reason-0"), "{body}");
    assert!(body.contains("reason-2"), "{body}");
    assert!(body.contains("call_long"), "{body}");
    assert!(body.contains("lookup"), "{body}");
    assert_eq!(
        harness.trace(&request_id).await.0.as_deref(),
        Some("completed")
    );

    for (case, expected_outcome, expected_commit) in [
        ("eof_before_terminal", "upstream_clean_eof", "committed"),
        ("truncated_frame", "protocol_violation", "committed"),
        ("upstream_error", "upstream_error", "committed"),
        ("idle_timeout", "timeout", "committed"),
        ("oversized_frame", "protocol_violation", "not_committed"),
    ] {
        let request_count_before = harness.probe.requests.lock().await.len();
        let response = if case == "upstream_error" {
            harness.send_to_route(FALLBACK_ROUTE, case, None).await
        } else {
            harness.send(case, None).await
        };
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let _ = response.bytes().await.unwrap();
        let (outcome, commit, fallback_allowed) = harness.trace(&request_id).await;
        assert_eq!(outcome.as_deref(), Some(expected_outcome), "case {case}");
        assert_eq!(commit, expected_commit, "case {case}");
        if expected_commit == "committed" {
            assert_eq!(fallback_allowed, Some(0), "case {case}");
        }
        if case == "upstream_error" {
            assert_eq!(
                harness.probe.requests.lock().await.len(),
                request_count_before + 1,
                "post-commit SSE error must not replay on the eligible fallback target"
            );
        }
    }

    let response = harness.send("heartbeat_forever", None).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let _ = response.bytes().await.unwrap();
    let (outcome, commit, _) = harness.trace(&request_id).await;
    assert_eq!(outcome.as_deref(), Some("timeout"));
    assert_eq!(commit, "not_committed");

    let failures_before_pre_commit_cancel = harness.provider_failures().await;
    let baseline_started = harness.probe.streams_started.load(Ordering::Relaxed);
    let baseline_dropped = harness.probe.streams_dropped.load(Ordering::Relaxed);
    let before = tokio::spawn({
        let client = harness.client.clone();
        let addr = harness.gateway_addr;
        async move {
            client
                .post(format!("http://{addr}/v1/chat/completions"))
                .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
                .json(&chat_request("disconnect_before_commit"))
                .send()
                .await
        }
    });
    harness
        .wait_for_counter(&harness.probe.streams_started, baseline_started)
        .await;
    before.abort();
    let _ = before.await;
    harness
        .wait_for_counter(&harness.probe.streams_dropped, baseline_dropped)
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM route_traces WHERE route_name = ? AND stream_outcome = 'client_cancelled' AND commit_state = 'not_committed'",
            )
            .bind(ROUTE)
            .fetch_one(&harness.state.pool)
            .await
            .unwrap()
                > 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("pre-commit client cancellation was not recorded");
    assert_eq!(
        harness.provider_failures().await,
        failures_before_pre_commit_cancel,
        "pre-commit cancellation must not mutate provider failure state"
    );
    harness
        .assert_admission_released(failures_before_pre_commit_cancel)
        .await;

    let baseline_dropped = harness.probe.streams_dropped.load(Ordering::Relaxed);
    let failures_before_post_commit_cancel = harness.provider_failures().await;
    let response = harness.send("disconnect_after_commit", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut stream = response.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("first-visible-event"));
    drop(stream);
    harness
        .wait_for_counter(&harness.probe.streams_dropped, baseline_dropped)
        .await;
    let (outcome, commit, _) = harness.trace(&request_id).await;
    assert_eq!(outcome.as_deref(), Some("client_cancelled"));
    assert_eq!(commit, "committed");
    let failures_after: i64 =
        sqlx::query_scalar("SELECT consecutive_failures FROM accounts WHERE id = ?")
            .bind(&harness.account_id)
            .fetch_one(&harness.state.pool)
            .await
            .unwrap();
    assert_eq!(
        failures_after, failures_before_post_commit_cancel,
        "cancellation is not provider failure"
    );
    harness
        .assert_admission_released(failures_before_post_commit_cancel)
        .await;

    harness.probe.chunks_generated.store(0, Ordering::Relaxed);
    let response = harness.send("backpressure", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    harness
        .wait_for_counter(&harness.probe.chunks_generated, 0)
        .await;
    // TCP and HTTP buffering make upstream chunk counts nondeterministic. The
    // exact response-channel capacity and producer stall/resume are proven by
    // stream_response_channel_tests; this end-to-end case stresses a slow reader.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut stream = response.bytes_stream();
    let mut bytes_received = 0usize;
    while let Some(chunk) = stream.next().await {
        bytes_received += chunk.unwrap().len();
    }
    assert!(
        bytes_received >= BACKPRESSURE_EVENTS * BACKPRESSURE_EVENT_BYTES,
        "received {bytes_received} bytes; generated {} of {BACKPRESSURE_EVENTS} events",
        harness.probe.chunks_generated.load(Ordering::Relaxed)
    );
    assert_eq!(
        harness.probe.chunks_generated.load(Ordering::Relaxed),
        BACKPRESSURE_EVENTS
    );
    assert_eq!(
        harness.trace(&request_id).await.0.as_deref(),
        Some("completed")
    );

    let request_count_before = harness.probe.requests.lock().await.len();
    let response = harness
        .send_to_route(FALLBACK_ROUTE, "reset_before_commit", None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = String::from_utf8(response.bytes().await.unwrap().to_vec()).unwrap();
    assert_eq!(body.matches("fallback-after-reset").count(), 1, "{body}");
    assert!(!body.contains("partial-before-reset"), "{body}");
    assert_eq!(
        harness.probe.requests.lock().await.len(),
        request_count_before + 2,
        "pre-commit connection reset should replay on the fallback target"
    );
    let (outcome, commit, _) = harness.trace(&request_id).await;
    assert_eq!(outcome.as_deref(), Some("completed"));
    assert_eq!(commit, "committed");

    let request_count_before = harness.probe.requests.lock().await.len();
    let (release_reset, reset_gate) = tokio::sync::oneshot::channel();
    *harness.probe.reset_after_commit_gate.lock().await = Some(reset_gate);
    let response = harness
        .send_to_route(FALLBACK_ROUTE, "reset_after_commit", None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut body_stream = response.bytes_stream();
    let mut body = String::new();
    while !body.contains("visible-before-reset") {
        let chunk = body_stream
            .next()
            .await
            .expect("stream should emit the primary event before reset")
            .unwrap();
        body.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    release_reset
        .send(())
        .expect("upstream reset gate is active");
    while let Some(chunk) = body_stream.next().await {
        body.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
    }
    assert!(!body.contains("fallback-after-reset"), "{body}");
    assert_eq!(
        harness.probe.requests.lock().await.len(),
        request_count_before + 1,
        "post-commit connection reset must not replay on the fallback target"
    );
    let (outcome, commit, fallback_allowed) = harness.trace(&request_id).await;
    assert_eq!(outcome.as_deref(), Some("upstream_error"));
    assert_eq!(commit, "committed");
    assert_eq!(fallback_allowed, Some(0));

    harness.close().await;
}

//! Operator acceptance over real HTTP, including the CLI's Admin API client.
use axum::http::StatusCode;
use kinetix::{
    admin_contract, app::AppState, config::Config, crypto::Crypto, db, paths::Paths,
    registry::Registry,
};
use serde_json::{json, Value};
use std::sync::Arc;

struct Server {
    state: AppState,
    url: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Server {
    async fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("kinetix-admin-contract-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
        };
        paths.ensure_dirs().unwrap();
        let database_url = paths.database_url();
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let config = Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [42; 32],
            admin_token: "contract-admin".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: false,
            allow_insecure_tls: false,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
            max_inflight_inferences: kinetix::config::DEFAULT_MAX_INFLIGHT_INFERENCES,
            alert_webhook_url: None,
            alert_fallback_rate: 1.0,
            alert_error_rate: 1.0,
            alert_min_requests: 1,
            alert_interval_secs: 60,
            alert_p95_latency_ms: 1000,
            ip_rate_limit_per_min: 0,
            session_ttl_minutes: 60,
            export_retention_days: 1,
            paths,
            generated_admin_password: None,
        });
        let state = AppState::new(
            config,
            pool.clone(),
            Arc::new(Registry::new()),
            Arc::new(Crypto::new(&[42; 32])),
            reqwest::Client::new(),
            kinetix::logqueue::UsageLogQueue::new(pool, 16),
            0,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = kinetix::router::build(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            state,
            url,
            client: reqwest::Client::new(),
            task,
            root,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}/admin/api{path}", self.url))
            .header("x-kinetix-admin-token", "contract-admin")
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap()
    }

    async fn post(&self, path: &str, body: &Value) -> Value {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap()
    }
}

async fn error(
    response: reqwest::Response,
    status: StatusCode,
    code: &str,
    field: Option<&str>,
) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["content-type"], "application/json");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], code);
    assert!(body["error"]["message"].is_string());
    assert!(body["error"]["fields"].is_array());
    if let Some(field) = field {
        assert!(
            body["error"]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|detail| detail["field"] == field),
            "{body}"
        );
    }
    body
}

#[tokio::test]
async fn unknown_admin_endpoints_never_fall_through_to_dashboard_html() {
    let server = Server::new().await;
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        error(
            server
                .request(method, "/unknown/nested?query=value")
                .send()
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
            "not_found",
            None,
        )
        .await;
    }
    for suffix in ["/admin/api", "/admin/api/"] {
        error(
            server
                .client
                .get(format!("{}{suffix}", server.url))
                .send()
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
            "not_found",
            None,
        )
        .await;
    }
    let dashboard = server
        .client
        .get(format!("{}/admin/settings", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(dashboard.status(), StatusCode::OK);
    assert!(dashboard.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
}

#[tokio::test]
async fn admin_errors_and_request_bounds_are_one_contract() {
    let server = Server::new().await;
    error(
        server
            .client
            .get(format!("{}/admin/api/me", server.url))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        None,
    )
    .await;
    error(
        server
            .request(reqwest::Method::GET, "/unknown")
            .send()
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
        "not_found",
        None,
    )
    .await;
    let wrong_method = server
        .request(reqwest::Method::POST, "/me")
        .send()
        .await
        .unwrap();
    assert!(wrong_method.headers().contains_key("allow"));
    error(
        wrong_method,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        None,
    )
    .await;
    error(
        server
            .request(reqwest::Method::POST, "/keys")
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        None,
    )
    .await;
    error(
        server
            .request(reqwest::Method::POST, "/keys")
            .body("{}")
            .send()
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
        None,
    )
    .await;
    error(
        server
            .request(reqwest::Method::POST, "/keys")
            .json(&json!({"owner": "ops"}))
            .send()
            .await
            .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
        Some("name"),
    )
    .await;
    let body = error(server.request(reqwest::Method::POST, "/keys").json(&json!({"name": "ops", "owner": "ops", "rpm_limit": "arbitrary-secret-without-prefix"})).send().await.unwrap(), StatusCode::UNPROCESSABLE_ENTITY, "validation_failed", Some("rpm_limit")).await;
    assert!(!body.to_string().contains("arbitrary-secret-without-prefix"));
    error(
        server
            .request(reqwest::Method::POST, "/routes")
            .json(&json!({"name": "route", "targets": [{"model_id": "x", "weight": "bad"}]}))
            .send()
            .await
            .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
        Some("targets[0].weight"),
    )
    .await;
    error(
        server
            .request(reqwest::Method::POST, "/routes")
            .json(&json!({"name": "route", "strategy": "unknown"}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some("strategy"),
    )
    .await;
    for path in [
        "/usage?limit=-1",
        "/audit?limit=0",
        "/keys?limit=501",
        "/requests?limit=invalid",
    ] {
        error(
            server
                .request(reqwest::Method::GET, path)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("limit"),
        )
        .await;
    }
    error(
        server
            .request(reqwest::Method::GET, "/models?actor=ops")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some("actor"),
    )
    .await;
    for (path, field) in [
        (format!("/keys?q={}", "x".repeat(257)), "q"),
        (
            format!("/models?provider_id={}", "x".repeat(257)),
            "provider_id",
        ),
    ] {
        error(
            server
                .request(reqwest::Method::GET, &path)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some(field),
        )
        .await;
    }
    error(
        server
            .request(
                reqwest::Method::POST,
                "/plugins/missing/permissions/approve",
            )
            .body("{\"permissions\":[]}")
            .send()
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
        None,
    )
    .await;
    error(
        server
            .request(reqwest::Method::GET, "/keys?offset=1000001")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some("offset"),
    )
    .await;
    error(
        server
            .request(
                reqwest::Method::GET,
                &format!("/keys?q={}", "x".repeat(8192)),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::URI_TOO_LONG,
        "uri_too_long",
        None,
    )
    .await;
    error(
        server
            .request(reqwest::Method::POST, "/keys")
            .header("content-type", "application/json")
            .body(vec![b' '; admin_contract::BODY_LIMIT + 1])
            .send()
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        None,
    )
    .await;
    // No Content-Length: the streaming body is still bounded.
    let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(vec![
        b' ';
        admin_contract::BODY_LIMIT
            + 1
    ])]);
    error(
        server
            .request(reqwest::Method::POST, "/keys")
            .header("content-type", "application/json")
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        None,
    )
    .await;
    for (method, path) in [
        (reqwest::Method::POST, "/keys"),
        (reqwest::Method::PUT, "/keys/missing"),
    ] {
        error(
            server
                .request(method, path)
                .json(&json!({"name": "ops", "owner": "ops", "max_concurrent_requests": -1}))
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("max_concurrent_requests"),
        )
        .await;
    }
    let max_uri = format!(
        "/keys?q={}",
        "x".repeat(admin_contract::QUERY_LIMIT - "/admin/api/keys?q=".len())
    );
    error(
        server
            .request(reqwest::Method::GET, &max_uri)
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some("q"),
    )
    .await;
    error(
        server
            .request(reqwest::Method::GET, &format!("{max_uri}x"))
            .send()
            .await
            .unwrap(),
        StatusCode::URI_TOO_LONG,
        "uri_too_long",
        None,
    )
    .await;
    let mut bulk = b"{}".to_vec();
    bulk.resize(admin_contract::BODY_LIMIT + 1, b' ');
    error(
        server
            .request(reqwest::Method::POST, "/config/import")
            .header("content-type", "application/json")
            .body(bulk)
            .send()
            .await
            .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
        Some("config"),
    )
    .await;
    error(
        server
            .request(reqwest::Method::POST, "/config/import")
            .header("content-type", "application/json")
            .body(vec![b' '; admin_contract::BULK_BODY_LIMIT + 1])
            .send()
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        None,
    )
    .await;
    server.state.pool.close().await;
    error(
        server
            .request(reqwest::Method::POST, "/keys")
            .json(&json!({"name": "ops", "owner": "ops"}))
            .send()
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        None,
    )
    .await;
    let internal = error(
        server
            .request(reqwest::Method::GET, "/keys")
            .send()
            .await
            .unwrap(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        None,
    )
    .await;
    assert_eq!(internal["error"]["message"], "internal server error");
    assert!(!internal.to_string().contains(server.root.to_str().unwrap()));
}

#[tokio::test]
async fn collection_pages_filter_before_limit_and_keep_stable_ids() {
    let server = Server::new().await;
    let mut ids = Vec::new();
    for name in ["same", "same", "literal%name"] {
        let created = server
            .post("/keys", &json!({"name": name, "owner": "ops"}))
            .await;
        let id = created["key"]["id"].as_str().unwrap().to_string();
        assert_ne!(created["full_key"], created["key"]["key_mask"]);
        ids.push(id);
    }
    sqlx::query("UPDATE virtual_keys SET created_at='2026-01-01T00:00:00Z'")
        .execute(&server.state.pool)
        .await
        .unwrap();
    ids.sort_by(|a, b| b.cmp(a));
    let first = server.get("/keys?limit=1").await;
    assert_eq!(first["keys"][0]["id"], ids[0]);
    assert_eq!(
        first["page"],
        json!({"limit": 1, "offset": 0, "total": 3, "next_offset": 1})
    );
    assert_eq!(first, server.get("/keys?limit=1").await);
    let second = server.get("/keys?limit=1&offset=1").await;
    assert_eq!(second["keys"][0]["id"], ids[1]);
    let last = server.get("/keys?limit=1&offset=2").await;
    assert_eq!(last["keys"][0]["id"], ids[2]);
    assert_eq!(last["page"]["next_offset"], Value::Null);
    let filtered = server.get("/keys?q=%25&limit=1").await;
    assert_eq!(filtered["page"]["total"], 1);
    assert_eq!(filtered["keys"][0]["name"], "literal%name");
    assert!(server.get("/keys?q=%27%20OR%201%3D1--").await["keys"]
        .as_array()
        .unwrap()
        .is_empty());
    let empty = server.get("/keys?offset=20").await;
    assert!(empty["keys"].as_array().unwrap().is_empty());
    assert_eq!(empty["page"]["total"], 3);
    assert_eq!(empty["page"]["next_offset"], Value::Null);
    let rename = server
        .request(reqwest::Method::PUT, &format!("/keys/{}", ids[0]))
        .json(&json!({"name": "renamed"}))
        .send()
        .await
        .unwrap();
    assert!(rename.status().is_success());
    let renamed = server.get("/keys?q=renamed").await;
    assert_eq!(renamed["keys"][0]["id"], ids[0]);
    assert!(!renamed.to_string().contains("key_hash"));
    for (path, key) in [
        ("/providers", "providers"),
        ("/models", "models"),
        ("/accounts", "accounts"),
        ("/routes", "routes"),
        ("/aliases", "aliases"),
        ("/usage", "usage"),
        ("/requests", "usage"),
        ("/audit", "audit"),
    ] {
        let page = server.get(&format!("{path}?limit=1")).await;
        assert!(page[key].is_array(), "{path}: {page}");
        assert_eq!(page["page"]["limit"], 1);
        assert_eq!(page["page"]["offset"], 0);
    }
    let audit = server
        .get("/audit?action=key_created&actor=admin&limit=2")
        .await;
    assert_eq!(audit["page"]["total"], 3);
    assert_eq!(audit["audit"].as_array().unwrap().len(), 2);
    server
        .state
        .registry
        .reload(&server.state.pool)
        .await
        .unwrap();
    assert_eq!(server.get("/keys?q=renamed").await["keys"][0]["id"], ids[0]);
}

#[tokio::test]
async fn account_filters_and_hidden_rows_apply_before_pagination() {
    let server = Server::new().await;
    let provider = server.post("/providers", &json!({"name": "public", "base_url": "https://upstream.example", "wire_format": "openai"})).await;
    let provider_id = provider["id"].as_str().unwrap();
    sqlx::query("UPDATE providers SET credential_mode='none' WHERE id=?")
        .bind(provider_id)
        .execute(&server.state.pool)
        .await
        .unwrap();
    let encrypted = server
        .state
        .crypto
        .encrypt("fixture-account-value")
        .unwrap();
    db::insert_account(
        &server.state.pool,
        provider_id,
        "__kinetix_noauth__",
        &encrypted,
        "hidden",
        0,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let visible_id = db::insert_account(
        &server.state.pool,
        provider_id,
        "visible",
        &encrypted,
        "masked",
        100,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let filtered = server
        .get(&format!("/accounts?provider_id={provider_id}&limit=1"))
        .await;
    assert_eq!(filtered["accounts"][0]["id"], visible_id);
    assert_eq!(filtered["page"]["total"], 1);
    assert_eq!(filtered["page"]["next_offset"], Value::Null);
    assert_eq!(
        server.get("/accounts?q=__kinetix_noauth__").await["page"]["total"],
        0
    );
    assert_eq!(
        server.get("/accounts?provider_id=missing").await["page"]["total"],
        0
    );
    assert!(!filtered.to_string().contains("fixture-account-value"));
    assert!(!filtered.to_string().contains("secret_enc"));
}

#[tokio::test]
async fn provider_reads_redact_headers_and_roundtrip_placeholders_safely() {
    let server = Server::new().await;
    let secret = "opaque-credential-material-without-a-known-prefix";
    let mut provider = json!({"name": "provider", "base_url": "https://upstream.example", "wire_format": "openai", "auth_scheme": "bearer", "extra_headers": {"x-vendor-key": secret, "X-Vendor-Key": "second-opaque-credential"}});
    let created = server.post("/providers", &provider).await;
    let id = created["id"].as_str().unwrap();
    let listed = server.get("/providers").await;
    assert_eq!(
        listed["providers"][0]["extra_headers"]["x-vendor-key"],
        admin_contract::REDACTED
    );
    assert!(!listed.to_string().contains(secret));
    provider["extra_headers"]["x-vendor-key"] = json!(admin_contract::REDACTED);
    provider["extra_headers"]["X-Vendor-Key"] = json!(admin_contract::REDACTED);
    provider["name"] = json!("renamed");
    assert!(server
        .request(reqwest::Method::PUT, &format!("/providers/{id}"))
        .json(&provider)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let read = server.get(&format!("/providers/{id}")).await;
    assert_eq!(read["id"], id);
    assert_eq!(read["name"], "renamed");
    assert!(!read.to_string().contains(secret));
    let stored = db::get_provider(&server.state.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.extra_headers_map()["x-vendor-key"], secret);
    assert_eq!(
        stored.extra_headers_map()["X-Vendor-Key"],
        "second-opaque-credential"
    );
    let exported = server.get("/config/export").await;
    assert!(!exported.to_string().contains(secret));
    let mut invalid_headers = provider.clone();
    invalid_headers["extra_headers"]["x-vendor-key"] = json!(42);
    for (method, path) in [
        (reqwest::Method::POST, "/providers".to_string()),
        (reqwest::Method::PUT, format!("/providers/{id}")),
    ] {
        error(
            server
                .request(method, &path)
                .json(&invalid_headers)
                .send()
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            Some("extra_headers.x-vendor-key"),
        )
        .await;
    }
    let mut invalid_import = exported.clone();
    invalid_import["providers"][0]["extra_headers"]["x-vendor-key"] = json!(42);
    let rejected = server
        .post("/config/import", &json!({"config": invalid_import}))
        .await;
    assert_eq!(rejected["valid"], false);
    assert!(rejected["problems"]
        .to_string()
        .contains("extra_headers values must be strings"));
    assert!(server
        .get("/config/export?include_secrets=true")
        .await
        .to_string()
        .contains(secret));
    let imported = server
        .post(
            "/config/import",
            &json!({"config": exported, "apply": true}),
        )
        .await;
    assert_eq!(imported["ok"], true);
    assert_eq!(
        db::get_provider(&server.state.pool, id)
            .await
            .unwrap()
            .unwrap()
            .extra_headers_map()["x-vendor-key"],
        secret
    );
    error(
        server
            .request(reqwest::Method::POST, "/providers")
            .json(&provider)
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some("extra_headers"),
    )
    .await;
    provider["wire_format"] = json!("unknown");
    error(
        server
            .request(reqwest::Method::PUT, &format!("/providers/{id}"))
            .json(&provider)
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some("wire_format"),
    )
    .await;
}

#[tokio::test]
async fn collection_query_schemas_only_allow_runtime_supported_filters() {
    let server = Server::new().await;
    let reference = server.get("/reference").await;
    let operations = reference["operations"].as_array().unwrap();
    for (collection, supported) in [
        ("keys", vec!["status"]),
        ("providers", vec![]),
        ("models", vec!["provider_id"]),
        ("accounts", vec!["provider_id"]),
        ("routes", vec![]),
        ("aliases", vec![]),
        ("usage", vec!["key_id", "status"]),
        ("requests", vec!["key_id", "status"]),
        ("audit", vec!["actor", "action"]),
    ] {
        let path = format!("/admin/api/{collection}");
        let schema = &operations
            .iter()
            .find(|operation| operation["path"] == path && operation["method"] == "GET")
            .unwrap()["query_schema"];
        assert_eq!(schema["additionalProperties"], false, "{collection}");
        let properties = schema["properties"].as_object().unwrap();
        for parameter in ["limit", "offset", "q"] {
            assert!(
                properties.contains_key(parameter),
                "{collection}: {parameter}"
            );
        }
        for filter in ["provider_id", "key_id", "status", "actor", "action"] {
            let supported = supported.contains(&filter);
            let response = server
                .request(
                    reqwest::Method::GET,
                    &format!("/{collection}?{filter}=fixture"),
                )
                .send()
                .await
                .unwrap();
            if supported {
                assert_eq!(response.status(), StatusCode::OK, "{collection}: {filter}");
            } else {
                error(
                    response,
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    Some(filter),
                )
                .await;
            }
            assert_eq!(
                properties.contains_key(filter),
                supported,
                "{collection}: {filter}"
            );
        }
        assert_eq!(properties.len(), 3 + supported.len(), "{collection}");
    }
    // Specialized endpoints retain their independent query contracts.
    let observations = operations
        .iter()
        .find(|operation| operation["path"] == "/admin/api/models/{id}/observations")
        .unwrap();
    assert!(observations["query_schema"]["properties"]["cursor"].is_object());
}

#[tokio::test]
async fn cli_and_http_publish_and_use_the_same_reference_and_error_contract() {
    let server = Server::new().await;
    let binary = env!("CARGO_BIN_EXE_kinetix");
    let reference = server.get("/reference").await;
    let output = tokio::process::Command::new(binary)
        .arg("api-reference")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        reference
    );
    assert_eq!(reference["format"], "kinetix-admin-reference-v1");
    let operations = reference["operations"].as_array().unwrap();
    assert!(operations.len() > 90);
    let create = operations
        .iter()
        .find(|operation| operation["operation"] == "create_key")
        .unwrap();
    assert_eq!(
        create["request_schema"]["required"],
        json!(["name", "owner"])
    );
    assert_eq!(create["body_limit_bytes"], admin_contract::BODY_LIMIT);
    let dry_run = operations
        .iter()
        .find(|operation| operation["operation"] == "dry_run_route")
        .unwrap();
    assert!(dry_run["request_schema"]["properties"]["has_images"].is_object());
    let approval = operations
        .iter()
        .find(|operation| operation["operation"] == "approve_plugin_permissions")
        .unwrap();
    assert_eq!(approval["body_required"], false);
    for path in ["../me", "%2e%2e/me"] {
        let rejected = tokio::process::Command::new(binary)
            .args(["api", "--url", &server.url, path])
            .env("KINETIX_ADMIN_TOKEN", "contract-admin")
            .output()
            .await
            .unwrap();
        assert!(!rejected.status.success());
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("within /admin/api"));
    }
    let body_file = server.root.join("key.json");
    tokio::fs::write(&body_file, b"{\"name\":\"CLI\",\"owner\":\"ops\"}")
        .await
        .unwrap();
    let created = tokio::process::Command::new(binary)
        .args([
            "api",
            "--url",
            &server.url,
            "keys",
            "--method",
            "POST",
            "--body",
            body_file.to_str().unwrap(),
        ])
        .env("KINETIX_ADMIN_TOKEN", "contract-admin")
        .output()
        .await
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(
        server.get("/keys?q=CLI").await["keys"][0]["id"],
        created["key"]["id"]
    );
    let client = tokio::process::Command::new(binary)
        .args(["api", "--url", &server.url, "keys?limit=-1"])
        .env("KINETIX_ADMIN_TOKEN", "contract-admin")
        .output()
        .await
        .unwrap();
    assert!(!client.status.success());
    let cli_error: admin_contract::ErrorBody = serde_json::from_slice(&client.stdout).unwrap();
    assert_eq!(cli_error.error.code, "invalid_request");
    assert_eq!(cli_error.error.fields[0].field, "limit");
    let client = tokio::process::Command::new(binary)
        .args(["api", "--url", &server.url, "keys?limit=1"])
        .env("KINETIX_ADMIN_TOKEN", "contract-admin")
        .output()
        .await
        .unwrap();
    assert!(client.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&client.stdout).unwrap(),
        server.get("/keys?limit=1").await
    );
}

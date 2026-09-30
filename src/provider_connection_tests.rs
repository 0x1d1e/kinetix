async fn connection_http(
    state: &AppState,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = Box::pin(
        crate::router::build(state.clone()).oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("x-kinetix-admin-token", "test-admin")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn anonymous_provider_http_rejects_credentials_and_materializes_routing_account() {
    let (state, root) = test_state("anonymous-http").await;
    let body = json!({"name": "anonymous", "base_url": "https://api.example.com/v1", "wire_format": "openai", "auth_scheme": "none"});
    for field in [
        json!({"api_key": "must-not-be-stored"}),
        json!({"custom_header_name": "x-api-key"}),
        json!({"credential_plugin": "plugin:test/auth"}),
        json!({"extra_headers": {"Authorization": "must-not-be-sent"}}),
    ] {
        let mut invalid = body.clone();
        invalid
            .as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        assert_eq!(
            connection_http(&state, "POST", "/admin/api/providers", invalid.clone())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let validation = connection_http(&state, "POST", "/admin/api/validate/provider", invalid)
            .await
            .1;
        assert_eq!(validation["valid"], false);
    }
    assert!(db::list_providers(&state.pool).await.unwrap().is_empty());
    let (status, created) = connection_http(&state, "POST", "/admin/api/providers", body).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let provider_id = created["id"].as_str().unwrap();
    let provider = db::get_provider(&state.pool, provider_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.auth(), AuthScheme::None);
    assert_eq!(provider.credential_mode, "none");
    let accounts = db::accounts_for_provider(&state.pool, provider_id)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert!(accounts[0].secret_enc.is_empty());
    assert!(accounts[0].key_mask.is_empty());
    assert!(state
        .credential_for(&provider, &accounts[0])
        .await
        .unwrap()
        .secret
        .is_empty());
    assert_eq!(
        connection_http(
            &state,
            "POST",
            "/admin/api/accounts",
            json!({"provider_id": provider_id, "label": "bad", "api_key": "must-not-be-stored"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    state.pool.close().await;
    let _ = std::fs::remove_dir_all(root);
}

async fn install_public_parameters_plugin(state: &AppState, manifest: Value) {
    let manifest: crate::plugins::Manifest = serde_json::from_value(manifest).unwrap();
    let manifest = toml::to_string(&manifest).unwrap();
    let mut package = tar::Builder::new(Vec::new());
    for (path, data) in [
        ("plugin.toml", manifest.as_bytes()),
        ("plugin.wasm", &b"\0asm\x0d\0\x01\0"[..]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        package.append_data(&mut header, path, data).unwrap();
    }
    let manager = state.plugin_manager().unwrap();
    let installed = manager
        .install(&package.into_inner().unwrap(), None, &[], true)
        .await
        .unwrap();
    assert!(installed.provides.is_empty());
    manager.approve_permissions("plugin.test").await.unwrap();
    manager.enable("plugin.test").await.unwrap();
}

#[tokio::test]
async fn public_parameters_share_discovery_and_streaming_without_credentials() {
    public_parameter_http_case(200, true).await;
    public_parameter_http_case(429, true).await;
    public_parameter_http_case(503, true).await;
}

#[tokio::test]
async fn public_parameters_preserve_host_owned_bearer_credentials() {
    public_parameter_http_case(200, false).await;
}

async fn public_parameter_http_case(upstream_status: u16, anonymous: bool) {
    let auth_scheme = if anonymous { "none" } else { "bearer" };
    let credential_mode = if anonymous { "none" } else { "manual" };
    let (state, root) =
        test_state_with_plugins(&format!("connection-{upstream_status}-{auth_scheme}")).await;
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("localhost:{}", listener.local_addr().unwrap().port());
    let redirect_target = format!("http://{}/escaped", listener.local_addr().unwrap());
    let mock = axum::Router::new().fallback(axum::routing::any(move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
            let captured = captured.clone();
            let redirect_target = redirect_target.clone();
            async move {
                captured.lock().await.push((uri.to_string(), headers));
                if uri.path() == "/redirect" {
                    return axum::http::Response::builder().status(302).header("location", redirect_target).body(axum::body::Body::empty()).unwrap();
                }
                if uri.path().ends_with("/models") {
                    return axum::http::Response::builder().header("content-type", "application/json").body(axum::body::Body::from(r#"{"data":[{"id":"test-model"}]}"#)).unwrap();
                }
                if upstream_status != 200 {
                    return axum::http::Response::builder().status(upstream_status).header("content-type", "application/json").body(axum::body::Body::from(r#"{"error":{"message":"temporary upstream failure"}}"#)).unwrap();
                }
                axum::http::Response::builder().header("content-type", "text/event-stream").body(axum::body::Body::from(concat!(
                    "data: {\"id\":\"chat-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chat-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
                    "data: [DONE]\n\n"
                ))).unwrap()
            }
        }));
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let declarations =
        json!({"account_id": {"type": "identifier", "min_length": 1, "max_length": 32}});
    install_public_parameters_plugin(&state, json!({
            "manifest_version": crate::plugins::MANIFEST_VERSION,
            "id": "plugin.test", "name": "Test Plugin", "version": "0.1.0", "plugin_api": "1",
            "permissions": {"network_hosts": ["localhost"], "credential_read": false},
            "integrations": [{"id": "anonymous", "name": "Anonymous Provider", "credential_mode": credential_mode,
                "provider": {"base_url": format!("https://{address}/accounts/{{account_id}}/v1"), "wire_format": "openai", "auth_scheme": auth_scheme, "models_path": "/models", "parameters": declarations}
            }]
        })).await;
    let setup_path = "/admin/api/plugins/plugin.test/integrations/anonymous/provider";
    for values in [
        json!({}),
        json!({"account_id": "../escape"}),
        json!({"account_id": "ok", "undeclared": "no"}),
        json!({"account_id": "x".repeat(33)}),
    ] {
        assert_eq!(
            connection_http(
                &state,
                "POST",
                setup_path,
                json!({"connection_values": values})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(db::list_providers(&state.pool).await.unwrap().is_empty());
    assert!(requests.lock().await.is_empty());
    let (status, setup) = connection_http(
        &state,
        "POST",
        setup_path,
        json!({"connection_values": {"account_id": "tenant-123"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{setup}");
    let provider_id = setup["id"].as_str().unwrap();
    let mut edit = json!({"name": "Anonymous Provider", "base_url": format!("http://{address}/accounts/{{account_id}}/v1"), "models_path": "/models", "wire_format": "openai", "auth_scheme": auth_scheme, "allow_insecure_tls": true, "connection_values": {"account_id": "tenant-456"}});
    if !anonymous {
        edit["api_key"] = json!("upstream-test-token");
    }
    let (status, updated) = connection_http(
        &state,
        "PUT",
        &format!("/admin/api/providers/{provider_id}"),
        edit.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (status, repeated) = connection_http(&state, "POST", setup_path, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(repeated["id"], setup["id"]);
    assert_eq!(repeated["created"], false);
    assert_eq!(db::list_providers(&state.pool).await.unwrap().len(), 1);
    let mut validation = edit.clone();
    validation["provider_id"] = json!(provider_id);
    let (status, valid) = connection_http(
        &state,
        "POST",
        "/admin/api/validate/provider",
        validation.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{valid}");
    assert_eq!(valid["valid"], true, "{valid}");
    validation["connection_values"] = json!({});
    assert_eq!(
        connection_http(&state, "POST", "/admin/api/validate/provider", validation)
            .await
            .1["valid"],
        false
    );
    let mut invalid_edit = edit.clone();
    invalid_edit["connection_values"]["account_id"] = json!("%252f");
    assert_eq!(
        connection_http(
            &state,
            "PUT",
            &format!("/admin/api/providers/{provider_id}"),
            invalid_edit
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let provider = db::get_provider(&state.pool, provider_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        provider.connection().unwrap().unwrap().values["account_id"],
        "tenant-456"
    );
    let (status, discovered) = connection_http(
        &state,
        "POST",
        &format!("/admin/api/providers/{provider_id}/discover"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{discovered}");
    let model_id = insert_transport_test_model(&state, provider_id, "test-model", json!({})).await;
    let route_id = db::insert_route(
        &state.pool,
        &db::NewRoute {
            name: "connection-test",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&state.pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();
    state.registry.reload(&state.pool).await.unwrap();
    let (status, key) = connection_http(
        &state,
        "POST",
        "/admin/api/keys",
        json!({"name": "test", "owner": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{key}");
    let response = Box::pin(crate::router::build(state.clone()).oneshot(
            axum::http::Request::builder().method("POST").uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", key["full_key"].as_str().unwrap()))
                .body(axum::body::Body::from(json!({"model": "connection-test", "messages": [{"role": "user", "content": "hello"}], "stream": true}).to_string())).unwrap()
        )).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    if upstream_status == 200 {
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("hello") && body.contains("[DONE]"), "{body}");
    } else {
        assert!(
            status.is_client_error() || status.is_server_error(),
            "{status}: {body}"
        );
    }
    let accounts = db::accounts_for_provider(&state.pool, provider_id)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert_ne!(accounts[0].status, "disabled");
    if anonymous {
        assert!(accounts[0].secret_enc.is_empty());
    } else {
        assert_eq!(
            state.crypto.decrypt(&accounts[0].secret_enc).unwrap(),
            "upstream-test-token"
        );
    }
    let captured = requests.lock().await;
    assert!(captured
        .iter()
        .any(|(path, _)| path == "/accounts/tenant-456/v1/models"));
    assert!(captured
        .iter()
        .any(|(path, _)| path == "/accounts/tenant-456/v1/chat/completions"));
    for (path, headers) in captured.iter() {
        assert!(!path.contains('?'), "{path}");
        if anonymous {
            assert!(!headers.contains_key("authorization"));
        } else {
            assert_eq!(headers["authorization"], "Bearer upstream-test-token");
        }
        for name in ["proxy-authorization", "x-api-key", "x-goog-api-key"] {
            assert!(!headers.contains_key(name), "{name}");
        }
    }
    let captured_count = captured.len();
    drop(captured);
    if anonymous {
        let mut redirect_provider = provider.clone();
        redirect_provider.follow_redirects = 1;
        let snapshot = state.registry.snapshot();
        let model = snapshot.models.get(&model_id).unwrap();
        let ctx = UpstreamContext { provider: &redirect_provider, model, account_id: Some(&accounts[0].id), session_context: None, credential: String::new() };
        let adapter = state.adapters.for_provider(&redirect_provider);
        let error = crate::outbound::send_provider_request(&state.outbound_clients, true, true, &adapter, &ctx, crate::outbound::ProviderRequest {
            method: reqwest::Method::GET, url: url::Url::parse(&format!("http://{address}/redirect")).unwrap(), json_body: None, accept_event_stream: false, request_id: None, headers: Vec::new(), total_timeout: Some(std::time::Duration::from_secs(2)),
        }).await.unwrap_err();
        assert!(error.message.contains("network_hosts"), "{error:?}");
        assert_eq!(requests.lock().await.len(), captured_count + 1);
        for value in [json!({}), json!({"account_id": "../escape"})] {
            let mut parameters = redirect_provider.connection().unwrap().unwrap();
            parameters.values = serde_json::from_value(value).unwrap();
            let mut invalid_provider = redirect_provider.clone();
            invalid_provider.connection_parameters = Some(serde_json::to_string(&parameters).unwrap());
            let ctx = UpstreamContext { provider: &invalid_provider, model, account_id: Some(&accounts[0].id), session_context: None, credential: String::new() };
            assert!(adapter.build_url(&ctx).is_err());
        }
        assert_eq!(requests.lock().await.len(), captured_count + 1);
    }
    let export_path = if anonymous {
        "/admin/api/config/export"
    } else {
        "/admin/api/config/export?include_secrets=true"
    };
    let (status, exported) = connection_http(&state, "GET", export_path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{exported}");
    let saved = &exported["providers"][0]["connection_parameters"];
    assert_eq!(saved["declarations"], declarations);
    assert_eq!(saved["values"]["account_id"], "tenant-456");
    assert_eq!(exported["providers"][0]["auth_scheme"], auth_scheme);
    assert!(!exported.to_string().contains("upstream-test-token"));
    let (restored, restored_root) = test_state("connection-import").await;
    let (status, imported) = connection_http(
        &restored,
        "POST",
        "/admin/api/config/import",
        json!({"config": exported, "apply": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{imported}");
    let provider = db::list_providers(&restored.pool)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(provider.auth_scheme, auth_scheme);
    assert_eq!(
        provider.resolved_base_url().unwrap(),
        format!("http://{address}/accounts/tenant-456/v1")
    );
    let accounts = db::accounts_for_provider(&restored.pool, &provider.id)
        .await
        .unwrap();
    if anonymous {
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].secret_enc.is_empty());
    }
    if !anonymous {
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            restored.crypto.decrypt(&accounts[0].secret_enc).unwrap(),
            "upstream-test-token"
        );
    }
    server.abort();
    state.pool.close().await;
    restored.pool.close().await;
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(restored_root);
}

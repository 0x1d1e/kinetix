mod api_v1 {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "plugin",
        pub_export_macro: true,
    });
}

#[cfg(not(feature = "api-v2"))]
mod adapter_v1 {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "plugin-adapter",
        pub_export_macro: true,
    });
}

#[cfg(feature = "api-v2")]
mod adapter_v2 {
    wit_bindgen::generate!({
        path: "../../../wit/v2",
        world: "plugin-adapter-v2",
        pub_export_macro: true,
        generate_all,
    });
}

use api_v1::exports;
use api_v1::kinetix::plugin::types::PluginError;
#[cfg(not(feature = "api-v2"))]
use adapter_v1::exports::provider_adapter::Guest as AdapterGuest;
#[cfg(not(feature = "api-v2"))]
use adapter_v1::kinetix::plugin::types::PluginError as AdapterError;
#[cfg(feature = "api-v2")]
use adapter_v2::exports::kinetix::plugin2_0_0::provider_adapter::Guest as AdapterGuest;
#[cfg(feature = "api-v2")]
use adapter_v2::kinetix::plugin1_0_0::types::PluginError as AdapterError;
#[cfg(feature = "api-v2")]
use adapter_v2::kinetix::plugin2_0_0::types::SessionContext;

// Test-only guest: the API-v1 exports keep the required main world valid; the
// exercised behavior is the API-v2 adapter, which deliberately echoes only the
// session value it receives so the host-boundary test can detect raw leakage.
struct Component;

impl exports::credential_strategy::Guest for Component {
    fn resolve(
        provider_id: String,
        account_id: String,
        _account_label: String,
    ) -> Result<api_v1::kinetix::plugin::types::CredentialLease, PluginError> {
        Ok(api_v1::kinetix::plugin::types::CredentialLease {
            handle: format!("fixture:{provider_id}:{account_id}"),
            expires_at: None,
            refresh_after: None,
            health: "healthy".into(),
        })
    }

    fn health(_provider_id: String, _account_id: String) -> Result<String, PluginError> {
        Ok("healthy".into())
    }

    fn rotate(_provider_id: String, _account_id: String) -> Result<(), PluginError> {
        Ok(())
    }
}

impl exports::model_source::Guest for Component {
    fn discover(
        provider_id: String,
        _base_url: String,
        _models_path: String,
    ) -> Result<Vec<api_v1::kinetix::plugin::types::DiscoveredModel>, PluginError> {
        Ok(vec![api_v1::kinetix::plugin::types::DiscoveredModel {
            id: format!("{provider_id}/session-echo-fixture"),
            display_name: Some("Session echo fixture".into()),
            context_window: Some(4096),
            max_output_tokens: Some(1024),
            capabilities_json: None,
            raw_metadata: None,
        }])
    }
}

impl exports::health_probe::Guest for Component {
    fn probe(
        _provider_id: String,
        _account_id: String,
    ) -> Result<api_v1::kinetix::plugin::types::HealthObservation, PluginError> {
        Ok(api_v1::kinetix::plugin::types::HealthObservation {
            state: "healthy".into(),
            quota_state: None,
            reset_at: None,
            retry_after: None,
            detail_code: None,
        })
    }
}

impl exports::routing_facts::Guest for Component {
    fn facts(
        _request_json: String,
    ) -> Result<Vec<api_v1::kinetix::plugin::types::RoutingFact>, PluginError> {
        Ok(Vec::new())
    }
}

impl exports::hooks::Guest for Component {
    fn on_request_normalized(_request_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_target_candidate(_target_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_usage_finalized(_usage_json: String) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(not(feature = "api-v2"))]
impl AdapterGuest for Component {
    fn wire_format() -> String {
        "api1-session-echo-fixture".into()
    }

    fn build_url(_provider_json: String, _model_json: String) -> Result<String, AdapterError> {
        Ok("https://fixture.invalid/api1".into())
    }

    fn apply_auth(
        _provider_json: String,
        _credential: String,
    ) -> Result<String, AdapterError> {
        Ok(r#"[["x-plugin-api","1"]]"#.into())
    }

    fn build_body(
        _request_json: String,
        _provider_json: String,
        _model_json: String,
    ) -> Result<String, AdapterError> {
        Ok(r#"{"session":"api1-has-no-session-argument"}"#.into())
    }

    fn classify_error(
        _status: u16,
        _body: String,
        _headers_json: String,
    ) -> Result<String, AdapterError> {
        Ok("{}".into())
    }

    fn parse_stream_chunk(_data: String) -> Result<String, AdapterError> {
        Ok(r#"{"version":1,"events":[]}"#.into())
    }

    fn parse_full_response(_body_json: String) -> Result<String, AdapterError> {
        Ok(r#"{"version":1,"events":[]}"#.into())
    }
}

#[cfg(feature = "api-v2")]
impl AdapterGuest for Component {
    fn wire_format() -> String {
        "api2-session-echo-fixture".into()
    }

    fn build_url(_provider_json: String, _model_json: String) -> Result<String, AdapterError> {
        Ok("https://fixture.invalid/api2".into())
    }

    fn apply_auth(
        _provider_json: String,
        _credential: String,
        session: Option<SessionContext>,
    ) -> Result<String, AdapterError> {
        let Some(session) = session else {
            return Ok("[]".into());
        };
        Ok(serde_json::json!([["x-plugin-session", session.id]]).to_string())
    }

    fn build_body(
        _request_json: String,
        provider_json: String,
        _model_json: String,
        session: Option<SessionContext>,
    ) -> Result<String, AdapterError> {
        // Antigravity's legacy API-v2 flow reads project:<account-id> from
        // host storage, so consume the host-injected account context here.
        let provider: serde_json::Value =
            serde_json::from_str(&provider_json).unwrap_or_default();
        let account_id = provider
            .pointer("/_kinetix/account_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let project_key = format!("project:{account_id}");
        let project_id = adapter_v2::kinetix::plugin1_0_0::host_storage::get(&project_key)
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let login_hint =
            adapter_v2::kinetix::plugin1_0_0::host_storage::get("_config:login_hint")
                .and_then(|bytes| String::from_utf8(bytes).ok());
        Ok(serde_json::json!({
            "session": session.map(|session| session.id),
            "account_id": account_id,
            "project_id": project_id,
            "login_hint": login_hint,
        })
        .to_string())
    }

    fn classify_error(
        _status: u16,
        _body: String,
        _headers_json: String,
    ) -> Result<String, AdapterError> {
        Ok("{}".into())
    }

    fn parse_stream_chunk(_data: String) -> Result<String, AdapterError> {
        Ok(r#"{"version":1,"events":[]}"#.into())
    }

    fn parse_full_response(_body_json: String) -> Result<String, AdapterError> {
        Ok(r#"{"version":1,"events":[]}"#.into())
    }
}

api_v1::export!(Component with_types_in api_v1);
#[cfg(not(feature = "api-v2"))]
adapter_v1::export!(Component with_types_in adapter_v1);
#[cfg(feature = "api-v2")]
adapter_v2::export!(Component with_types_in adapter_v2);

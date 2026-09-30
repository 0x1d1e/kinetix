//! HTTP router: public API, admin API, embedded dashboard.

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::admin;
use crate::api;
use crate::app::AppState;
use crate::assets;
use crate::server::ClientConnectionInfo;

async fn attach_client_disconnect(mut request: Request, next: Next) -> Response {
    let disconnect = request
        .extensions()
        .get::<ConnectInfo<ClientConnectionInfo>>()
        .map(|info| info.0.disconnect.clone());
    if let Some(disconnect) = disconnect {
        request.extensions_mut().insert(disconnect);
    }
    next.run(request).await
}

pub fn build(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_credentials(true)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderName::from_static("x-api-key"),
            axum::http::HeaderName::from_static("anthropic-version"),
            axum::http::HeaderName::from_static("anthropic-beta"),
            axum::http::HeaderName::from_static("x-claude-code-session-id"),
            axum::http::HeaderName::from_static("x-kinetix-admin-token"),
        ]);

    let public = Router::new()
        .route("/callback", get(admin::plugin_auth_callback))
        .route("/healthz", get(api::healthz))
        .route("/v1/chat/completions", post(api::chat_completions))
        .route("/v1/responses", post(api::responses))
        .route("/v1/messages", post(api::messages))
        .route("/v1/messages/count_tokens", post(api::count_message_tokens))
        .route("/v1/models", get(api::list_models))
        .route("/v1/usage", get(api::client_usage))
        .layer(axum::middleware::from_fn(attach_client_disconnect));

    let admin_api = admin_routes()
        .finish()
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
        // Preserve fail-closed mutations while the control-plane store is degraded.
        // Reads remain available; only the Admin API error representation changes.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin::require_control_plane,
        ))
        .layer(axum::middleware::from_fn(crate::admin_contract::boundary));

    let unknown_admin_api = axum::routing::any(|| async {
        crate::admin_contract::error_response(
            axum::http::StatusCode::NOT_FOUND,
            "endpoint not found",
            vec![],
        )
    })
    .layer(axum::middleware::from_fn(crate::admin_contract::boundary));

    let dashboard = Router::new()
        .route("/", get(assets::serve))
        .route("/admin", get(assets::serve))
        .route("/admin/", get(assets::serve))
        .route("/admin/{*path}", get(assets::serve));

    Router::new()
        .merge(public)
        .nest("/admin/api", admin_api)
        // The dashboard wildcard must not turn unknown API paths into HTML 200s.
        .route("/admin/api", unknown_admin_api.clone())
        .route("/admin/api/", unknown_admin_api.clone())
        .route("/admin/api/{*path}", unknown_admin_api)
        .merge(dashboard)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(state)
}

pub(crate) fn admin_routes() -> crate::admin_contract::reference::AdminRouter {
    use crate::admin_contract::reference::{delete, get, post, put, AdminRouter};
    AdminRouter::new()
        .route("/login", post(admin::login))
        .route("/logout", post(admin::logout))
        .route("/me", get(admin::me))
        .route("/password", post(admin::change_password))
        .route(
            "/settings/public-base-url",
            get(admin::get_public_base_url).put(admin::update_public_base_url),
        )
        .route(
            "/settings/model-lifecycle",
            get(admin::get_model_lifecycle_settings).put(admin::update_model_lifecycle_settings),
        )
        .route("/overview", get(admin::overview))
        .route("/test-stream", post(admin::test_stream))
        .route("/keys", get(admin::list_keys).post(admin::create_key))
        .route(
            "/keys/{id}/client-profile-models",
            get(admin::client_profile_models),
        )
        .route(
            "/client-profiles/generate",
            post(admin::generate_client_profile),
        )
        .route(
            "/keys/{id}",
            put(admin::update_key).delete(admin::delete_key),
        )
        // providers
        .route(
            "/providers",
            get(admin::list_providers).post(admin::create_provider),
        )
        .route(
            "/providers/{id}",
            get(admin::get_provider)
                .put(admin::update_provider)
                .delete(admin::delete_provider),
        )
        .route(
            "/providers/{id}/discover",
            get(admin::cached_model_discovery).post(admin::discover_models),
        )
        .route("/providers/{id}/reconcile", post(admin::reconcile_models))
        .route(
            "/providers/{id}/pricing/sync",
            post(admin::sync_provider_pricing),
        )
        .route("/providers/{id}/test", post(admin::test_provider))
        .route(
            "/providers/{id}/credential-enrollment/start",
            post(admin::start_provider_credential_enrollment),
        )
        // models
        .route("/models", get(admin::list_models))
        .route("/providers/{id}/models", post(admin::create_model))
        .route(
            "/models/{id}/observations",
            get(admin::list_model_observations),
        )
        .route(
            "/models/{id}",
            put(admin::update_model).delete(admin::delete_model),
        )
        .route(
            "/models/{id}/reconciliation",
            put(admin::update_model_reconciliation),
        )
        .route("/models/{id}/probe", post(admin::probe_model_capability))
        // accounts
        .route(
            "/accounts",
            get(admin::list_accounts).post(admin::create_account),
        )
        .route(
            "/accounts/{id}",
            put(admin::update_account).delete(admin::delete_account),
        )
        .route("/accounts/{id}/reset", post(admin::reset_account))
        .route("/accounts/{id}/test", post(admin::test_account))
        // routes
        .route("/routes", get(admin::list_routes).post(admin::create_route))
        .route(
            "/routes/{id}",
            put(admin::update_route).delete(admin::delete_route),
        )
        .route("/routes/validate", post(admin::validate_route_config))
        .route("/routes/dry-run", post(admin::dry_run_route))
        .route("/validate", post(admin::validate_endpoint))
        .route("/validate/provider", post(admin::validate_provider))
        .route("/validate/model", post(admin::validate_model_edit))
        .route("/validate/account", post(admin::validate_account_edit))
        .route("/config/export", get(admin::export_config))
        .route("/config/import", post(admin::import_config))
        .route(
            "/credential-interchange/export",
            post(admin::export_credentials),
        )
        .route(
            "/credential-interchange/import",
            post(admin::import_credentials),
        )
        // aliases
        .route(
            "/aliases",
            get(admin::list_aliases).post(admin::create_alias),
        )
        .route("/aliases/{id}", delete(admin::delete_alias))
        // usage / requests / audit / metrics
        .route("/usage", get(admin::usage))
        .route("/requests", get(admin::usage))
        .route("/requests/live", get(admin::live_requests))
        .route(
            "/requests/{id}/route-trace",
            get(admin::request_route_trace),
        )
        .route(
            "/requests/{id}/diagnostics",
            get(admin::request_diagnostics),
        )
        // Resolve an opaque X-Kinetix-Route-Id to its Route Trace (FR-12.15).
        .route(
            "/route-traces/{opaque_id}",
            get(admin::route_trace_by_opaque),
        )
        .route("/audit", get(admin::audit))
        // usage/log export to disk (today/24h/7d/30d retention + cleanup)
        .route(
            "/exports",
            get(admin::list_exports).post(admin::export_usage_day),
        )
        .route("/exports/{name}", delete(admin::delete_export))
        .route("/metrics", get(admin::metrics))
        .route("/health/runtime", get(admin::runtime_health))
        // Plugins
        .route("/plugins", get(admin::list_plugins))
        .route("/plugins/catalog", get(admin::plugin_catalog))
        .route(
            "/plugins/catalog/refresh",
            post(admin::refresh_plugin_catalog),
        )
        .route(
            "/plugins/catalog/{id}/preview",
            get(admin::preview_catalog_plugin),
        )
        .route(
            "/plugins/catalog/{id}/install",
            post(admin::install_catalog_plugin),
        )
        .route("/plugins/install", post(admin::install_plugin))
        .route("/plugins/auth/start", post(admin::start_plugin_auth))
        .route(
            "/plugins/auth/complete",
            post(admin::complete_plugin_auth_manual),
        )
        .route("/plugins/auth/status", get(admin::plugin_auth_status))
        .route("/plugins/auth/callback", get(admin::plugin_auth_callback))
        .route(
            "/plugins/{id}/integrations/{integration}/provider",
            post(admin::setup_plugin_integration_provider),
        )
        .route(
            "/plugins/{id}",
            get(admin::get_plugin).delete(admin::remove_plugin),
        )
        .route("/plugins/{id}/enable", post(admin::enable_plugin))
        .route("/plugins/{id}/disable", post(admin::disable_plugin))
        .route("/plugins/{id}/validate", post(admin::validate_plugin))
        .route("/plugins/{id}/rollback", post(admin::rollback_plugin))
        .route(
            "/plugins/{id}/packages/{sha256}/preview",
            get(admin::preview_plugin_rollback),
        )
        .route(
            "/plugins/{id}/packages/{sha256}/reinstall",
            post(admin::reinstall_plugin_package),
        )
        .route(
            "/plugins/{id}/settings",
            get(admin::plugin_settings).put(admin::update_plugin_settings),
        )
        .route("/plugins/{id}/permissions", get(admin::plugin_permissions))
        .route(
            "/plugins/{id}/permissions/approve",
            post(admin::approve_plugin_permissions),
        )
        .route(
            "/plugins/{id}/permissions/revoke",
            post(admin::revoke_plugin_permissions),
        )
        .route("/plugins/{id}/audit", get(admin::plugin_audit))
        .route("/plugins/{id}/metrics", get(admin::plugin_metrics))
}

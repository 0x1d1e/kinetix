//! Hardened provider HTTP transport.
//!
//! Provider requests resolve and validate the destination immediately before
//! connection, pin that exact address set into reqwest, and handle redirects
//! explicitly so SSRF and credential-host policy is re-applied on every hop.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use url::Url;

use crate::adapters::{Adapter, UpstreamContext};
use crate::types::UpstreamFailure;

const MAX_REDIRECTS: usize = 5;
const MAX_PINNED_CLIENTS: usize = 256;
/// TCP/TLS establishment has its own bound. Provider `timeout_ms` is enforced
/// by the pipeline as first-event/idle phase budgets, never as total wall time.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TRANSIENT_GATEWAY_RETRIES: usize = 1;
const TRANSIENT_GATEWAY_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct ResolvedDestination {
    pub host: String,
    pub port: u16,
    pub addrs: Vec<SocketAddr>,
}

impl ResolvedDestination {
    fn cache_key(&self, insecure_tls: bool) -> String {
        let mut ips = self
            .addrs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        ips.sort_unstable();
        format!(
            "{}:{}|{}|{}",
            self.host,
            self.port,
            insecure_tls,
            ips.join(",")
        )
    }
}

#[derive(Debug)]
pub struct OutboundError {
    pub message: String,
    pub timeout: bool,
    pub adapter_failure: Option<UpstreamFailure>,
}

impl OutboundError {
    fn denied(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            timeout: false,
            adapter_failure: None,
        }
    }

    fn transport(error: &reqwest::Error) -> Self {
        let timeout = error.is_timeout();
        let message = if timeout {
            "upstream request timed out".to_string()
        } else if error.is_connect() {
            "connection to upstream failed".to_string()
        } else {
            format!(
                "upstream request failed: {}",
                crate::crypto::redact(&error.to_string())
            )
        };
        Self {
            message,
            timeout,
            adapter_failure: None,
        }
    }

    fn adapter(failure: UpstreamFailure) -> Self {
        Self {
            message: failure.message.clone(),
            timeout: matches!(failure.kind, crate::types::FailureKind::Timeout),
            adapter_failure: Some(failure),
        }
    }
}

#[derive(Clone)]
pub struct ProviderRequest {
    pub method: Method,
    pub url: Url,
    pub json_body: Option<Value>,
    pub accept_event_stream: bool,
    pub request_id: Option<String>,
    /// Safe client protocol headers explicitly selected by the API boundary.
    pub headers: Vec<(String, String)>,
    /// Optional whole-request timeout for bounded control-plane calls such as
    /// probes/discovery. Streaming proxy traffic must leave this as `None`.
    pub total_timeout: Option<Duration>,
}

/// Resolve a URL and validate the entire DNS answer set. The returned addresses
/// must be used for the actual connection; validating and then resolving again
/// would reintroduce the DNS-rebinding TOCTOU gap.
pub async fn resolve_destination(
    url: &Url,
    allow_private: bool,
    allow_insecure_tls: bool,
) -> Result<ResolvedDestination, OutboundError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(OutboundError::denied(
            "upstream URLs may not contain userinfo",
        ));
    }
    if url.scheme() != "https" && !allow_insecure_tls {
        return Err(OutboundError::denied(format!(
            "plain-HTTP upstream '{}' refused: TLS is mandatory",
            url
        )));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(OutboundError::denied(format!(
            "unsupported upstream URL scheme '{}'",
            url.scheme()
        )));
    }

    let host = url
        .host_str()
        .ok_or_else(|| OutboundError::denied("upstream URL has no host"))?
        .to_ascii_lowercase();
    if !allow_private && crate::net::is_blocked_host(&host) {
        return Err(OutboundError::denied(format!(
            "upstream host '{host}' is blocked by SSRF policy"
        )));
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| OutboundError::denied("upstream URL has no usable port"))?;
    let mut addrs = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| OutboundError::denied(format!("DNS resolution for '{host}' failed: {e}")))?
            .collect::<Vec<_>>()
    };

    addrs.sort_unstable();
    addrs.dedup();
    validate_destination_addrs(&host, addrs, allow_private).map(|addrs| ResolvedDestination {
        host,
        port,
        addrs,
    })
}

fn validate_destination_addrs(
    host: &str,
    addrs: Vec<SocketAddr>,
    allow_private: bool,
) -> Result<Vec<SocketAddr>, OutboundError> {
    if addrs.is_empty() {
        return Err(OutboundError::denied(format!(
            "DNS resolution for '{host}' returned no addresses"
        )));
    }

    if !allow_private {
        if let Some(blocked) = addrs
            .iter()
            .find(|addr| crate::net::is_blocked_ip(addr.ip()))
        {
            return Err(OutboundError::denied(format!(
                "host '{host}' resolves to a blocked private/reserved address ({})",
                blocked.ip()
            )));
        }
    }

    Ok(addrs)
}

fn pinned_client(
    cache: &DashMap<String, reqwest::Client>,
    destination: &ResolvedDestination,
    insecure_tls: bool,
) -> Result<reqwest::Client, OutboundError> {
    let key = destination.cache_key(insecure_tls);
    if let Some(client) = cache.get(&key) {
        return Ok(client.clone());
    }

    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(CONNECT_TIMEOUT)
        .http2_adaptive_window(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("kinetix/", env!("CARGO_PKG_VERSION")));

    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder = builder.resolve_to_addrs(&destination.host, &destination.addrs);

    let client = builder.build().map_err(|e| OutboundError {
        message: format!("building pinned upstream client failed: {e}"),
        timeout: false,
        adapter_failure: None,
    })?;

    if cache.len() >= MAX_PINNED_CLIENTS {
        cache.clear();
    }
    cache.insert(key, client.clone());
    Ok(client)
}

fn credentials_authorized(ctx: &UpstreamContext<'_>, url: &Url) -> bool {
    url.host_str()
        .map(|host| ctx.provider.host_authorized(host))
        .unwrap_or(false)
}

fn redirected_method(status: StatusCode, method: &Method) -> (Method, bool) {
    if status == StatusCode::SEE_OTHER
        || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND)
            && *method == Method::POST)
    {
        (Method::GET, false)
    } else {
        (method.clone(), true)
    }
}

/// Send a provider request with pinned DNS and explicit redirects.
///
/// Every redirect target is re-resolved and revalidated. Provider credentials
/// and provider-defined headers are stripped at each hop and only reapplied when
/// the redirect host is authorized by the provider credential binding.
fn should_retry_transient_gateway(
    accept_event_stream: bool,
    status: StatusCode,
    retries_done: usize,
) -> bool {
    accept_event_stream
        && retries_done < MAX_TRANSIENT_GATEWAY_RETRIES
        && matches!(
            status,
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
        )
}

pub async fn send_provider_request(
    cache: &DashMap<String, reqwest::Client>,
    allow_private: bool,
    allow_insecure_global: bool,
    adapter: &Arc<dyn Adapter>,
    ctx: &UpstreamContext<'_>,
    request: ProviderRequest,
) -> Result<reqwest::Response, OutboundError> {
    send_provider_request_inner(
        cache,
        allow_private,
        allow_insecure_global,
        adapter,
        ctx,
        request,
        None,
    )
    .await
}

/// Send an inference request and mark it dispatched before starting network I/O.
pub(crate) async fn send_provider_request_tracked(
    cache: &DashMap<String, reqwest::Client>,
    allow_private: bool,
    allow_insecure_global: bool,
    adapter: &Arc<dyn Adapter>,
    ctx: &UpstreamContext<'_>,
    request: ProviderRequest,
    dispatched: &AtomicBool,
) -> Result<reqwest::Response, OutboundError> {
    send_provider_request_inner(
        cache,
        allow_private,
        allow_insecure_global,
        adapter,
        ctx,
        request,
        Some(dispatched),
    )
    .await
}

async fn send_provider_request_inner(
    cache: &DashMap<String, reqwest::Client>,
    allow_private: bool,
    allow_insecure_global: bool,
    adapter: &Arc<dyn Adapter>,
    ctx: &UpstreamContext<'_>,
    request: ProviderRequest,
    dispatched: Option<&AtomicBool>,
) -> Result<reqwest::Response, OutboundError> {
    let mut retries_done = 0usize;
    loop {
        let response = send_provider_request_once(
            cache,
            allow_private,
            allow_insecure_global,
            adapter,
            ctx,
            &request,
            dispatched,
        )
        .await?;

        if should_retry_transient_gateway(
            request.accept_event_stream,
            response.status(),
            retries_done,
        ) {
            retries_done += 1;
            tracing::warn!(
                account_id = ctx.account_id.unwrap_or("<unknown>"),
                provider = %ctx.provider.name,
                status = %response.status(),
                retry = retries_done,
                "retrying transient upstream gateway failure before route fallback"
            );
            tokio::time::sleep(TRANSIENT_GATEWAY_BACKOFF).await;
            continue;
        }

        return Ok(response);
    }
}

async fn send_provider_request_once(
    cache: &DashMap<String, reqwest::Client>,
    allow_private: bool,
    allow_insecure_global: bool,
    adapter: &Arc<dyn Adapter>,
    ctx: &UpstreamContext<'_>,
    request: &ProviderRequest,
    dispatched: Option<&AtomicBool>,
) -> Result<reqwest::Response, OutboundError> {
    let insecure_tls = allow_insecure_global || ctx.provider.insecure_tls();
    let mut current = request.url.clone();
    let mut method = request.method.clone();
    let mut body = request.json_body.clone();

    if !credentials_authorized(ctx, &current) {
        let host = current.host_str().unwrap_or("<missing>");
        return Err(OutboundError::denied(format!(
            "credential host binding: '{host}' is not an authorized host for provider '{}'",
            ctx.provider.name
        )));
    }

    ctx.provider
        .resolved_endpoint()
        .map_err(OutboundError::denied)?;
    let connection = ctx.provider.connection().map_err(OutboundError::denied)?;
    for hop in 0..=MAX_REDIRECTS {
        if let Some(parameters) = &connection {
            parameters
                .authorize_url(&current)
                .map_err(OutboundError::denied)?;
        }
        let destination = resolve_destination(&current, allow_private, insecure_tls).await?;
        tracing::debug!(
            target = %destination.host,
            resolved_ips = ?destination.addrs,
            redirect_hop = hop,
            "outbound destination pinned"
        );
        let client = pinned_client(cache, &destination, insecure_tls)?;

        let authorized = credentials_authorized(ctx, &current);
        let mut builder = client.request(method.clone(), current.clone());
        if let Some(timeout) = request.total_timeout {
            builder = builder.timeout(timeout);
        }

        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        if request.accept_event_stream {
            builder = builder.header("accept", "text/event-stream");
        }
        if let Some(request_id) = request.request_id.as_deref() {
            builder = builder.header("x-request-id", request_id);
        }
        if let Some(json) = body.as_ref() {
            builder = builder.json(json);
        }

        if authorized {
            if ctx.provider.auth() != crate::types::AuthScheme::None {
                builder = adapter
                    .apply_auth(ctx, builder)
                    .map_err(OutboundError::adapter)?;
            }
            for (name, value) in ctx.provider.extra_headers_map() {
                builder = builder.header(name, value);
            }
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
        }

        if let Some(dispatched) = dispatched {
            dispatched.store(true, Ordering::Release);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| OutboundError::transport(&e))?;
        if !response.status().is_redirection() || !ctx.provider.follows_redirects() {
            return Ok(response);
        }

        if hop == MAX_REDIRECTS {
            return Err(OutboundError::denied(format!(
                "upstream redirect limit exceeded ({MAX_REDIRECTS})"
            )));
        }

        let location = match response.headers().get(reqwest::header::LOCATION) {
            Some(value) => value.to_str().map_err(|_| {
                OutboundError::denied("upstream redirect has invalid Location header")
            })?,
            None => return Ok(response),
        };
        let next = current
            .join(location)
            .map_err(|e| OutboundError::denied(format!("invalid upstream redirect target: {e}")))?;

        let (next_method, preserve_body) = redirected_method(response.status(), &method);
        method = next_method;
        if !preserve_body {
            body = None;
        }
        current = next;
    }

    unreachable!("redirect loop is bounded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ProviderRow;
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn provider() -> ProviderRow {
        ProviderRow {
            id: "p".into(),
            name: "test".into(),
            base_url: "https://api.example.com".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 1000,
            capability_mode: "permissive".into(),
            models_path: None,
            rate_limit_rules: "{}".into(),
            enabled: 1,
            follow_redirects: 1,
            credential_hosts: "auth.example.net".into(),
            allow_insecure_tls: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            wire_plugin: String::new(),
            credential_plugin: String::new(),
            model_source_plugin: String::new(),
            credential_mode: "manual".into(),
            source_plugin_id: None,
            source_integration_id: None,
            pricing_scope: "direct_api".into(),
            integration_features: None,
            integration_protocols: None,
            connection_parameters: None,
            connection_parameters_attested: None,
        }
    }

    #[test]
    fn rejects_mixed_public_private_dns_answers() {
        let public = SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 443);
        let private = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 443);
        assert!(
            validate_destination_addrs("rebind.example", vec![public, private], false).is_err()
        );
        assert!(validate_destination_addrs("public.example", vec![public], false).is_ok());
    }

    #[test]
    fn retries_only_transient_streaming_gateway_failures_once() {
        for status in [
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(should_retry_transient_gateway(true, status, 0));
            assert!(!should_retry_transient_gateway(true, status, 1));
        }

        assert!(!should_retry_transient_gateway(
            false,
            StatusCode::BAD_GATEWAY,
            0,
        ));
        assert!(!should_retry_transient_gateway(
            true,
            StatusCode::INTERNAL_SERVER_ERROR,
            0,
        ));
        assert!(!should_retry_transient_gateway(
            true,
            StatusCode::TOO_MANY_REQUESTS,
            0,
        ));
        assert!(!should_retry_transient_gateway(
            true,
            StatusCode::BAD_REQUEST,
            0,
        ));
    }

    async fn transient_gateway_retry_case(status: u16, explicit_empty_length: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let response = if attempt == 0 {
                    let length = if explicit_empty_length {
                        "Content-Length: 0\r\n"
                    } else {
                        ""
                    };
                    format!("HTTP/1.1 {status} transient\r\n{length}Connection: close\r\n\r\n")
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });

        let mut provider = provider();
        provider.base_url = format!("http://{addr}");
        provider.allow_insecure_tls = 1;
        provider.follow_redirects = 0;
        let model = crate::db::ModelRow {
            id: "m".into(),
            provider_id: provider.id.clone(),
            upstream_id: "model".into(),
            display_name: "model".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        };
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: Some("acc"),
            session_context: None,
            credential: "secret".into(),
        };
        let adapter: Arc<dyn Adapter> = Arc::new(crate::adapters::openai::OpenAiAdapter::new());
        let cache = DashMap::new();
        let response = send_provider_request(
            &cache,
            true,
            true,
            &adapter,
            &ctx,
            ProviderRequest {
                method: Method::POST,
                url: Url::parse(&format!("http://{addr}/v1/chat/completions")).unwrap(),
                json_body: Some(serde_json::json!({"model":"model","stream":true})),
                accept_event_stream: true,
                request_id: Some("req_retry".into()),
                headers: Vec::new(),
                total_timeout: Some(Duration::from_secs(2)),
            },
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retries_transient_gateways_with_or_without_content_length() {
        transient_gateway_retry_case(502, true).await;
        transient_gateway_retry_case(502, false).await;
        transient_gateway_retry_case(503, false).await;
        transient_gateway_retry_case(504, false).await;
    }

    #[test]
    fn redirect_credentials_are_host_bound() {
        let provider = provider();
        let model = crate::db::ModelRow {
            id: "m".into(),
            provider_id: provider.id.clone(),
            upstream_id: "model".into(),
            display_name: "model".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        };
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            session_context: None,
            credential: "secret".into(),
        };

        assert!(credentials_authorized(
            &ctx,
            &Url::parse("https://api.example.com/v1").unwrap()
        ));
        assert!(credentials_authorized(
            &ctx,
            &Url::parse("https://auth.example.net/v1").unwrap()
        ));
        assert!(!credentials_authorized(
            &ctx,
            &Url::parse("https://evil.example/v1").unwrap()
        ));
    }

    #[test]
    fn redirect_method_matches_http_semantics() {
        assert_eq!(
            redirected_method(StatusCode::TEMPORARY_REDIRECT, &Method::POST),
            (Method::POST, true)
        );
        assert_eq!(
            redirected_method(StatusCode::SEE_OTHER, &Method::POST),
            (Method::GET, false)
        );
    }
}

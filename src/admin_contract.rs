//! Shared Admin API transport contracts. Public inference protocols are separate.

pub mod reference;
pub mod storage;

use axum::body::HttpBody;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::{header, request::Parts, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

pub const BODY_LIMIT: usize = 1024 * 1024;
pub const BULK_BODY_LIMIT: usize = 16 * 1024 * 1024;
pub const QUERY_LIMIT: usize = 8192;
pub const MAX_PAGE_SIZE: i64 = 500;
pub const REDACTED: &str = "[REDACTED]";
pub const ERROR_CODES: &[(u16, &str)] = &[
    (400, "invalid_request"),
    (401, "unauthorized"),
    (403, "forbidden"),
    (404, "not_found"),
    (405, "method_not_allowed"),
    (409, "conflict"),
    (413, "body_too_large"),
    (414, "uri_too_long"),
    (415, "unsupported_media_type"),
    (422, "validation_failed"),
    (429, "rate_limited"),
    (500, "internal_error"),
    (503, "unavailable"),
];

#[derive(Debug, Clone, serde::Deserialize, Serialize, schemars::JsonSchema)]
pub struct FieldError {
    pub field: String,
    pub code: String,
    pub message: String,
}

#[derive(serde::Deserialize, Serialize, schemars::JsonSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(serde::Deserialize, Serialize, schemars::JsonSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    pub fields: Vec<FieldError>,
}

#[derive(Clone)]
struct ContractError;

pub fn error_response(
    status: StatusCode,
    message: impl Into<String>,
    mut fields: Vec<FieldError>,
) -> Response {
    for field in &mut fields {
        field.message = crate::crypto::redact(&field.message);
    }
    let code = ERROR_CODES
        .iter()
        .find_map(|(value, code)| (*value == status.as_u16()).then_some(*code))
        .unwrap_or("internal_error");
    let mut response = (
        status,
        axum::Json(ErrorBody {
            error: ErrorDetail {
                code: code.into(),
                message: message.into(),
                fields,
            },
        }),
    )
        .into_response();
    response.extensions_mut().insert(ContractError);
    response
}

pub fn field(
    field: impl Into<String>,
    code: impl Into<String>,
    message: impl Into<String>,
) -> FieldError {
    FieldError {
        field: field.into(),
        code: code.into(),
        message: message.into(),
    }
}

// Keep response JSON identical to Axum's Json; only input rejections differ.
#[derive(Debug)]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = Response;

    async fn from_request(request: Request, _: &S) -> Result<Self, Self::Rejection> {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if mime != "application/json"
            && !(mime.starts_with("application/") && mime.ends_with("+json"))
        {
            return Err(error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
                vec![],
            ));
        }
        let limit = body_limit(request.uri().path());
        let bytes = axum::body::to_bytes(request.into_body(), limit)
            .await
            .map_err(body_error)?;
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
        let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
            // Serde messages can contain submitted passwords/tokens. Never echo values.
            let syntax = error.inner().is_syntax() || error.inner().is_eof();
            let status = if syntax {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            let mut path = error.path().to_string();
            let inner = error.inner().to_string();
            if let Some(missing) = inner
                .strip_prefix("missing field `")
                .and_then(|s| s.split('`').next())
            {
                path = if path == "." {
                    missing.into()
                } else {
                    format!("{path}.{missing}")
                };
            }
            let message = if syntax {
                "malformed JSON"
            } else {
                "field is missing or has an invalid type or value"
            };
            error_response(
                status,
                message,
                if syntax {
                    vec![]
                } else {
                    vec![field(path, "invalid_field", message)]
                },
            )
        })?;
        deserializer.end().map_err(|_| {
            error_response(StatusCode::BAD_REQUEST, "trailing data after JSON", vec![])
        })?;
        Ok(Self(value))
    }
}

impl<S: Send + Sync, T: DeserializeOwned> axum::extract::OptionalFromRequest<S> for Json<T> {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        if !request.headers().contains_key(header::CONTENT_TYPE)
            && request.body().size_hint().exact() == Some(0)
        {
            return Ok(None);
        }
        <Self as FromRequest<S>>::from_request(request, state)
            .await
            .map(Some)
    }
}

pub struct Query<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().unwrap_or("");
        let deserializer =
            serde_urlencoded::Deserializer::new(url::form_urlencoded::parse(query.as_bytes()));
        serde_path_to_error::deserialize(deserializer)
            .map(Self)
            .map_err(|error| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid query parameters",
                    vec![field(
                        error.path().to_string(),
                        "invalid_field",
                        "invalid query parameter",
                    )],
                )
            })
    }
}

pub fn body_limit(path: &str) -> usize {
    match path.strip_prefix("/admin/api").unwrap_or(path) {
        "/plugins/install" | "/config/import" | "/credential-interchange/import" => BULK_BODY_LIMIT,
        _ => BODY_LIMIT,
    }
}

fn body_error(error: axum::Error) -> Response {
    if std::error::Error::source(&error)
        .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
    {
        error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds the endpoint limit",
            vec![],
        )
    } else {
        error_response(
            StatusCode::BAD_REQUEST,
            "failed to read request body",
            vec![],
        )
    }
}

/// Covers auth, routing, path and other framework rejections without affecting inference APIs.
pub async fn boundary(request: Request, next: Next) -> Response {
    let uri = request
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or(request.uri(), |original| &original.0);
    let mut response = if uri.to_string().len() > QUERY_LIMIT {
        error_response(
            StatusCode::URI_TOO_LONG,
            "request URI exceeds 8192 bytes",
            vec![],
        )
    } else {
        let limit = body_limit(request.uri().path());
        let (parts, body) = request.into_parts();
        match axum::body::to_bytes(body, limit).await {
            Ok(bytes) => {
                next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                    .await
            }
            Err(error) => body_error(error),
        }
    };
    if (response.status().is_client_error() || response.status().is_server_error())
        && response.extensions().get::<ContractError>().is_none()
    {
        let status = response.status();
        let headers = response.headers().clone();
        response = error_response(
            status,
            status.canonical_reason().unwrap_or("request failed"),
            vec![],
        );
        for name in [header::ALLOW, header::RETRY_AFTER] {
            if let Some(value) = headers.get(&name) {
                response.headers_mut().insert(name, value.clone());
            }
        }
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    #[schemars(range(min = 1, max = MAX_PAGE_SIZE))]
    pub limit: Option<i64>,
    #[schemars(range(min = 0, max = 1_000_000))]
    pub offset: Option<i64>,
    pub q: Option<String>,
    pub provider_id: Option<String>,
    pub key_id: Option<String>,
    pub status: Option<String>,
    pub actor: Option<String>,
    pub action: Option<String>,
}

impl ListQuery {
    pub fn bounds(&self) -> Result<(i64, i64), crate::admin::ApiError> {
        let limit = self.limit.unwrap_or(200);
        let offset = self.offset.unwrap_or(0);
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(crate::admin::ApiError::field(
                "limit",
                "must be between 1 and 500",
            ));
        }
        if !(0..=1_000_000).contains(&offset) {
            return Err(crate::admin::ApiError::field(
                "offset",
                "must be between 0 and 1000000",
            ));
        }
        for (name, value) in [
            ("q", &self.q),
            ("provider_id", &self.provider_id),
            ("key_id", &self.key_id),
            ("status", &self.status),
            ("actor", &self.actor),
            ("action", &self.action),
        ] {
            if value.as_ref().is_some_and(|value| value.len() > 256) {
                return Err(crate::admin::ApiError::field(
                    name,
                    "must not exceed 256 bytes",
                ));
            }
        }
        Ok((limit, offset))
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct Page {
    pub limit: i64,
    pub offset: i64,
    pub total: i64,
    pub next_offset: Option<i64>,
}

impl Page {
    pub fn new(limit: i64, offset: i64, total: i64, returned: usize) -> Self {
        let next = offset + returned as i64;
        Self {
            limit,
            offset,
            total,
            next_offset: (returned > 0 && next < total).then_some(next),
        }
    }
}

/// Header values are opaque secrets, regardless of spelling or token prefix.
pub fn redact_headers(headers: &std::collections::HashMap<String, String>) -> Value {
    json!(headers
        .keys()
        .map(|name| (name.clone(), REDACTED))
        .collect::<std::collections::BTreeMap<_, _>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn broken_body_is_not_reported_as_a_size_violation() {
        let app = axum::Router::new()
            .route("/", axum::routing::post(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn(boundary));
        let stream = futures::stream::iter([Err::<Vec<u8>, _>(std::io::Error::other(
            "private transport details",
        ))]);
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .body(axum::body::Body::from_stream(stream))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
            .await
            .unwrap();
        let body: ErrorBody = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.error.code, "invalid_request");
        assert_eq!(body.error.message, "failed to read request body");
    }
}

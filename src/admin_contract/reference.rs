//! Generate the machine-readable reference from the same typed registrations as routing.
use super::{Json, Query};
use crate::{app::AppState, auth::AdminAuth};
use axum::{handler::Handler, routing::MethodRouter, Router};
use schemars::JsonSchema;
use serde_json::{json, Value};

pub trait ExtractorContract {
    fn describe() -> Value {
        json!({})
    }
}
impl<T> ExtractorContract for axum::extract::State<T> {}
impl ExtractorContract for AdminAuth {}
impl ExtractorContract for axum_extra::extract::cookie::CookieJar {}
impl<T: JsonSchema> ExtractorContract for axum::extract::Path<T> {
    fn describe() -> Value {
        json!({"path_schema": schemars::schema_for!(T)})
    }
}
impl<T: JsonSchema> ExtractorContract for Json<T> {
    fn describe() -> Value {
        json!({"request_schema": schemars::schema_for!(T), "body_required": true})
    }
}
impl<T: JsonSchema> ExtractorContract for Option<Json<T>> {
    fn describe() -> Value {
        json!({"request_schema": schemars::schema_for!(T), "body_required": false})
    }
}
impl<T: JsonSchema> ExtractorContract for Query<T> {
    fn describe() -> Value {
        json!({"query_schema": schemars::schema_for!(T)})
    }
}

pub trait ExtractorsContract {
    fn describe() -> Value;
}
macro_rules! extractors {
    ($($extractor:ident),+) => {
        impl<M, $($extractor: ExtractorContract),+> ExtractorsContract for (M, $($extractor,)+) {
            fn describe() -> Value {
                let mut result = serde_json::Map::new();
                $(result.extend($extractor::describe().as_object().unwrap().clone());)+
                Value::Object(result)
            }
        }
    }
}
extractors!(A);
extractors!(A, B);
extractors!(A, B, C);
extractors!(A, B, C, D);
extractors!(A, B, C, D, E);

pub struct Methods {
    router: MethodRouter<AppState>,
    operations: Vec<Value>,
}

macro_rules! method {
    ($name:ident) => {
        pub fn $name<H, T>(handler: H) -> Methods
        where
            H: Handler<T, AppState>,
            T: ExtractorsContract + 'static,
        {
            Methods {
                router: axum::routing::$name(handler),
                operations: vec![operation::<H, T>(stringify!($name))],
            }
        }
        impl Methods {
            pub fn $name<H, T>(mut self, handler: H) -> Self
            where
                H: Handler<T, AppState>,
                T: ExtractorsContract + 'static,
            {
                self.router = self.router.$name(handler);
                self.operations.push(operation::<H, T>(stringify!($name)));
                self
            }
        }
    };
}
method!(get);
method!(post);
method!(put);
method!(delete);

fn operation<H, T: ExtractorsContract>(method: &str) -> Value {
    let mut operation = T::describe();
    operation["method"] = json!(method.to_ascii_uppercase());
    operation["operation"] = json!(std::any::type_name::<H>().rsplit("::").next().unwrap());
    operation
}

pub struct AdminRouter {
    router: Router<AppState>,
    operations: Vec<Value>,
}

impl Default for AdminRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminRouter {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            operations: vec![],
        }
    }

    pub fn route(mut self, path: &str, methods: Methods) -> Self {
        self.router = self.router.route(path, methods.router);
        for mut operation in methods.operations {
            operation["path"] = json!(format!("/admin/api{path}"));
            operation["body_limit_bytes"] = json!(super::body_limit(path));
            self.operations.push(operation);
        }
        self
    }

    pub fn reference(&self) -> Value {
        let mut operations = self.operations.clone();
        operations.push(
            json!({"path": "/admin/api/reference", "method": "GET", "operation": "reference"}),
        );
        json!({
            "format": "kinetix-admin-reference-v1",
            "version": env!("CARGO_PKG_VERSION"),
            "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
            "authentication": {"header": "x-kinetix-admin-token", "cookie": crate::auth::SESSION_COOKIE, "public_operations": ["login", "logout", "plugin_auth_callback"]},
            "operations": operations,
            "error_schema": schemars::schema_for!(super::ErrorBody),
            "error_codes": super::ERROR_CODES.iter().map(|(status, code)| (status.to_string(), json!(code))).collect::<serde_json::Map<_, _>>(),
            "page_schema": schemars::schema_for!(super::Page),
            "collections": super::storage::reference(),
            "pagination": {"default_limit": 200, "max_limit": super::MAX_PAGE_SIZE, "max_offset": 1000000, "max_filter_bytes": 256, "search": "ASCII case-insensitive literal substring", "snapshot": "per page, not across pages"},
            "uri_limit_bytes": super::QUERY_LIMIT,
            "resource_ids": "opaque persisted IDs; names are not IDs",
            "success_responses": "endpoint-specific JSON; paginated collections keep their named array and add page; metrics is Prometheus text; test-stream may be SSE; OAuth callbacks redirect",
            "secret_policy": "credentials are absent from ordinary reads; virtual keys are returned once at creation; explicit credential exports and generated client profiles may include secrets; provider extra_headers values are redacted"
        })
    }

    pub fn finish(self) -> Router<AppState> {
        let reference = self.reference();
        self.router.route(
            "/reference",
            axum::routing::get(move |_: AdminAuth| async move { axum::Json(reference) }),
        )
    }
}

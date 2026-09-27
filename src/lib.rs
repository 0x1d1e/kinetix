#![allow(
    clippy::large_enum_variant,
    clippy::result_large_err,
    clippy::too_many_arguments,
    clippy::type_complexity
)]

//! Kinetix: a multi-protocol LLM proxy.
//!
//! OpenAI Chat Completions and Anthropic Messages in (streaming first),
//! admin-configured upstreams out (Gemini, OpenAI-compatible, Anthropic), with
//! virtual keys, account pools with automatic fallback Routes, and cost
//! tracking.
//!
//! The crate is exposed as a library so that integration tests under `tests/`
//! can drive the wire encoders/decoders directly (NFR-5.5 golden wire-output
//! fixtures, FR-9.1 fixture suite) in addition to the unit and torture tests
//! embedded in the modules.

pub mod adapters;
pub mod admin;
pub mod admission;
pub mod alerts;
pub mod alloc;
pub mod api;
pub mod app;
pub mod assets;
pub mod auth;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod cost;
pub mod credential_refresh;
pub mod credentials;
pub mod crypto;
pub mod db;
pub mod export;
pub mod frontends;
pub mod limits;
pub mod live;
pub mod logqueue;
pub mod model_catalog;
pub mod net;
pub mod opaque_state;
pub mod outbound;
pub mod passthrough;
pub mod paths;
pub mod pipeline;
pub mod plugins;
pub mod pool;
pub mod predicate;
pub mod provider_circuit;
pub mod quota;
pub mod ratelimit;
pub mod registry;
pub mod router;
pub mod server;
pub mod sse;
pub mod target_telemetry;
#[cfg(test)]
mod torture;
pub mod trace;
pub mod types;
pub mod update;
pub mod upstream_traffic;
pub mod validate;

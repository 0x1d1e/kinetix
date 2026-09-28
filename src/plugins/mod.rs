//! Plugin subsystem.
//!
//! Plugins extend integration behavior through the versioned WIT contract;
//! routing policy and accounting remain owned by Kinetix core.

pub mod adapter;
pub mod catalog;
pub mod credential;
pub mod manager;
pub mod manifest;
pub mod package;
pub mod response_contract;
pub mod runtime;
pub mod store;
pub mod types;

pub use manager::PluginManager;

pub use manifest::{HostPolicy, ValidatedManifest};
pub use types::{
    Capability, CircuitState, CredentialMode, Integration, Limits, Manifest, Permissions,
    PluginRef, PluginStatus, PricingScope, Provided, MANIFEST_VERSION, PLUGIN_API_MAJOR,
};

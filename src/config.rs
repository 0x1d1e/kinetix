//! Application configuration, loaded from environment + optional TOML file.
//!
//! The master encryption key and admin token come from the environment (or a
//! file pointed to by an env var). The TOML file is only a *bootstrap* source:
//! it seeds providers/models/virtual keys on first run. After that the database
//! is authoritative (see `docs/` requirement FR-8.5 / open issue resolution).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// NFR-1.4 reference workload supports 200 concurrent streams.
pub const DEFAULT_MAX_INFLIGHT_INFERENCES: u64 = 200;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    /// Public base URL of the API hostname (used in docs/UI examples).
    pub public_base_url: String,
    pub database_url: String,
    /// 32-byte master key used to encrypt upstream credentials at rest.
    pub master_key: [u8; 32],
    /// Admin password/token for the admin API + dashboard session.
    pub admin_token: String,
    /// Optional Cloudflare Access audience. When set, admin requests must carry
    /// a valid `Cf-Access-Jwt-Assertion` (defense in depth, NFR-3.2).
    pub cf_access_aud: Option<String>,
    pub cf_access_team_domain: Option<String>,
    pub log_json: bool,
    /// Bootstrap config file (providers/models/keys), if present.
    pub bootstrap_file: Option<PathBuf>,
    /// Allow outbound requests to private/loopback ranges (NFR-3.9). Off by default.
    pub allow_private_upstreams: bool,
    /// Global dev override that permits plain-HTTP upstreams. This is a visibly
    /// marked development mode (NFR-3.12); TLS is mandatory otherwise.
    pub allow_insecure_tls: bool,
    /// Directory for the embedded dashboard override (dev mode) + db backups.
    pub data_dir: PathBuf,
    /// Graceful-shutdown drain window before in-flight streams are dropped
    /// (NFR-2.3). Default 30s.
    pub shutdown_grace_secs: u64,
    /// Maximum simultaneous inference requests across the instance. Excess
    /// requests fail fast with 429; the limit is never disabled.
    pub max_inflight_inferences: u64,
    /// Optional webhook URL for alerts (FR-6.6). When set, alert-worthy events
    /// are POSTed as JSON. Unset means alerting is disabled.
    pub alert_webhook_url: Option<String>,
    /// Fallback-rate alert threshold as a fraction of requests (FR-12.17).
    pub alert_fallback_rate: f64,
    /// 5xx/error-rate alert threshold as a fraction of requests.
    pub alert_error_rate: f64,
    /// Minimum requests in the window before a rate alert can fire, so a
    /// quiet instance does not alert on one request.
    pub alert_min_requests: i64,
    /// Seconds between alert evaluations.
    pub alert_interval_secs: u64,
    /// p95 added-proxy-latency alert threshold in milliseconds (Monitoring).
    pub alert_p95_latency_ms: u64,
    /// Per-IP requests/minute for the abuse limiter (NFR-3.6); 0 disables it.
    pub ip_rate_limit_per_min: u64,
    /// How long an admin dashboard session stays valid (minutes). Sessions are
    /// in-memory, so a server restart always requires a fresh login.
    pub session_ttl_minutes: u64,
    /// Usage/log export retention in days (files older than this may be pruned).
    pub export_retention_days: u64,
    /// Resolved filesystem layout (config/data/state directories).
    pub paths: crate::paths::Paths,
    /// Set only when a fresh install generated the admin password; the caller
    /// prints it exactly once after logging is initialized.
    pub generated_admin_password: Option<String>,
}

/// Explicit overrides supplied by the CLI (`kinetix serve --bind ...`). Any
/// field left `None` falls back to the env var or built-in default, so the
/// binary works both as a standalone CLI tool and via env/.env for development.
#[derive(Clone, Debug, Default)]
pub struct CliOverrides {
    pub bind: Option<String>,
    pub public_base_url: Option<String>,
    pub database_url: Option<String>,
    pub master_key: Option<String>,
    pub admin_token: Option<String>,
    pub log_json: Option<bool>,
    pub allow_private_upstreams: Option<bool>,
    pub allow_insecure_tls: Option<bool>,
    pub shutdown_grace_secs: Option<u64>,
    pub max_inflight_inferences: Option<u64>,
    pub ip_rate_limit_per_min: Option<u64>,
    pub session_ttl_minutes: Option<u64>,
    pub export_retention_days: Option<u64>,
    /// `--home <dir>`: put config/data/state under one root.
    pub home: Option<PathBuf>,
    /// Explicit config-file path (used by `kinetix serve --config`).
    pub config_file: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::build(CliOverrides::default())
    }

    /// Build the effective configuration from CLI overrides, the environment,
    /// an optional `.env`/config file, and built-in defaults — in that order of
    /// precedence. Also ensures the directory tree exists and, on a first run,
    /// generates a master key and an admin password, persisting both.
    pub fn build(ov: CliOverrides) -> Result<Self> {
        // `--home`/KINETIX_HOME means "an isolated instance": do not auto-load a
        // project `.env`, so the resolved directories and their derived database
        // URL are authoritative rather than being shadowed by a stray env file.
        if ov.home.is_none() {
            let _ = dotenvy::dotenv();
        }
        let paths = crate::paths::Paths::resolve(ov.home.as_deref());
        paths
            .ensure_dirs()
            .context("creating Kinetix directories")?;

        // Load the TOML config file (if present) as a *fallback* source for the
        // runtime settings. Env vars and CLI flags always win over the file.
        let file = load_file_config(ov.config_file.as_deref().unwrap_or(&paths.config_file()));

        let pick =
            |cli: Option<String>, env: &str, file: Option<String>, default: &str| -> String {
                cli.or_else(|| std::env::var(env).ok())
                    .or(file)
                    .unwrap_or_else(|| default.to_string())
            };
        let pick_bool = |cli: Option<bool>, env: &str, file: Option<bool>, default: bool| -> bool {
            cli.or_else(|| std::env::var(env).ok().map(|v| v == "true"))
                .or(file)
                .unwrap_or(default)
        };
        let pick_u64 = |cli: Option<u64>, env: &str, file: Option<u64>, default: u64| -> u64 {
            cli.or_else(|| std::env::var(env).ok().and_then(|v| v.parse().ok()))
                .or(file)
                .unwrap_or(default)
        };

        let bind = pick(
            ov.bind,
            "KINETIX_BIND",
            file.as_ref().and_then(|f| f.bind.clone()),
            "127.0.0.1:8080",
        );
        let public_base_url = pick(
            ov.public_base_url,
            "KINETIX_PUBLIC_BASE_URL",
            file.as_ref().and_then(|f| f.public_base_url.clone()),
            &format!("http://{bind}"),
        );
        let database_url = pick(
            ov.database_url,
            "KINETIX_DATABASE_URL",
            file.as_ref().and_then(|f| f.database_url.clone()),
            &paths.database_url(),
        );

        // Master key: CLI/env/file, else a persisted key, else generate + persist.
        let master_key = resolve_master_key(ov.master_key, &paths)?;

        // Admin password: CLI/env/file, else the persisted hash (checked at
        // login time), else a freshly generated one shown once.
        let (admin_token, generated_admin_password) = resolve_admin_token(
            ov.admin_token,
            file.as_ref().and_then(|f| f.admin_password.clone()),
            &paths,
        )?;

        let log_json = pick_bool(ov.log_json, "KINETIX_LOG_JSON", None, false);
        let allow_private_upstreams = pick_bool(
            ov.allow_private_upstreams,
            "KINETIX_ALLOW_PRIVATE_UPSTREAMS",
            None,
            false,
        );
        let allow_insecure_tls = pick_bool(
            ov.allow_insecure_tls,
            "KINETIX_ALLOW_INSECURE_TLS",
            None,
            false,
        );
        if allow_insecure_tls {
            tracing::warn!(
                "KINETIX_ALLOW_INSECURE_TLS=true: plain-HTTP upstreams are permitted. \
                 This is a development-only mode (NFR-3.12) and must not be used in production."
            );
        }

        let bootstrap_file = std::env::var("KINETIX_BOOTSTRAP_FILE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);

        let shutdown_grace_secs = pick_u64(
            ov.shutdown_grace_secs,
            "KINETIX_SHUTDOWN_GRACE_SECS",
            file.as_ref().and_then(|f| f.shutdown_grace_secs),
            30,
        );
        let max_inflight_inferences = pick_u64(
            ov.max_inflight_inferences,
            "KINETIX_MAX_INFLIGHT_INFERENCES",
            file.as_ref().and_then(|f| f.max_inflight_inferences),
            DEFAULT_MAX_INFLIGHT_INFERENCES,
        );
        if max_inflight_inferences == 0 {
            bail!("KINETIX_MAX_INFLIGHT_INFERENCES must be greater than zero");
        }
        let alert_webhook_url = std::env::var("KINETIX_ALERT_WEBHOOK_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| file.as_ref().and_then(|f| f.alert_webhook_url.clone()));
        let alert_fallback_rate = env_or("KINETIX_ALERT_FALLBACK_RATE", "0.25")
            .parse::<f64>()
            .unwrap_or(0.25);
        let alert_error_rate = env_or("KINETIX_ALERT_ERROR_RATE", "0.10")
            .parse::<f64>()
            .unwrap_or(0.10);
        let alert_min_requests = env_or("KINETIX_ALERT_MIN_REQUESTS", "20")
            .parse::<i64>()
            .unwrap_or(20);
        let alert_interval_secs = env_or("KINETIX_ALERT_INTERVAL_SECS", "60")
            .parse::<u64>()
            .unwrap_or(60);
        let alert_p95_latency_ms = env_or("KINETIX_ALERT_P95_LATENCY_MS", "100")
            .parse::<u64>()
            .unwrap_or(100);
        let ip_rate_limit_per_min = pick_u64(
            ov.ip_rate_limit_per_min,
            "KINETIX_IP_RATE_LIMIT_PER_MIN",
            file.as_ref().and_then(|f| f.ip_rate_limit_per_min),
            600,
        );
        let session_ttl_minutes = pick_u64(
            ov.session_ttl_minutes,
            "KINETIX_SESSION_TTL_MINUTES",
            file.as_ref().and_then(|f| f.session_ttl_minutes),
            720,
        );
        let export_retention_days = pick_u64(
            ov.export_retention_days,
            "KINETIX_EXPORT_RETENTION_DAYS",
            file.as_ref().and_then(|f| f.export_retention_days),
            30,
        );

        Ok(Config {
            bind,
            public_base_url,
            database_url,
            master_key,
            admin_token,
            cf_access_aud: std::env::var("KINETIX_CF_ACCESS_AUD")
                .ok()
                .filter(|s| !s.is_empty()),
            cf_access_team_domain: std::env::var("KINETIX_CF_ACCESS_TEAM_DOMAIN")
                .ok()
                .filter(|s| !s.is_empty()),
            log_json,
            bootstrap_file,
            allow_private_upstreams,
            allow_insecure_tls,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs,
            max_inflight_inferences,
            alert_webhook_url,
            alert_fallback_rate,
            alert_error_rate,
            alert_min_requests,
            alert_interval_secs,
            alert_p95_latency_ms,
            ip_rate_limit_per_min,
            session_ttl_minutes,
            export_retention_days,
            paths,
            generated_admin_password,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse a 32-byte key from hex, base64, or (last resort) a passphrase.
pub fn parse_master_key(raw: &str) -> Result<[u8; 32]> {
    let raw = raw.trim();
    if raw.len() == 64 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = hex::decode(raw).context("master key is not valid hex")?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        return Ok(out);
    }
    use base64::Engine;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(raw) {
        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Ok(out);
        }
    }
    if raw.len() >= 16 {
        use sha2::{Digest, Sha256};
        tracing::warn!("master key is not 32 bytes of hex/base64; deriving via SHA-256");
        let digest = Sha256::digest(raw.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        return Ok(out);
    }
    bail!("master key must be 64 hex chars, base64 of 32 bytes, or >=16 chars")
}

/// Resolve the master key: CLI override, then env (`KINETIX_MASTER_KEY` or
/// `KINETIX_MASTER_KEY_FILE`), then the persisted key file, else generate a new
/// one and persist it (0600) so the next start reuses it.
fn resolve_master_key(cli: Option<String>, paths: &crate::paths::Paths) -> Result<[u8; 32]> {
    if let Some(v) = cli {
        return parse_master_key(&v);
    }
    if let Ok(v) = std::env::var("KINETIX_MASTER_KEY") {
        return parse_master_key(&v);
    }
    if let Ok(path) = std::env::var("KINETIX_MASTER_KEY_FILE") {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading KINETIX_MASTER_KEY_FILE at {path}"))?;
        return parse_master_key(&raw);
    }
    let key_file = paths.master_key_file();
    if let Ok(raw) = std::fs::read_to_string(&key_file) {
        return parse_master_key(&raw);
    }
    // First run: generate and persist.
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    write_secret_file(&key_file, &hex::encode(bytes))?;
    tracing::info!(path = %key_file.display(), "generated a new master key");
    Ok(bytes)
}

/// Resolve the admin password. The *effective* password for a fresh install is
/// generated and printed once; thereafter the stored hash is authoritative
/// (validated at login), so this value is only used when no hash exists yet.
fn resolve_admin_token(
    cli: Option<String>,
    file: Option<String>,
    paths: &crate::paths::Paths,
) -> Result<(String, Option<String>)> {
    let provided = cli
        .or_else(|| std::env::var("KINETIX_ADMIN_TOKEN").ok())
        .or(file)
        .filter(|s| !s.trim().is_empty());

    let hash_file = paths.config_dir.join("admin_password.hash");
    let has_hash = hash_file.exists();

    if let Some(pw) = provided {
        if pw.trim().len() < 8 {
            bail!("admin password must be at least 8 characters");
        }
        // A supplied credential (CLI flag, env, or config file) is authoritative:
        // (re)seed the stored hash so login validates against it. Without this an
        // explicitly-provided password would be silently ignored once a hash file
        // existed, contradicting the documented CLI > env > file precedence.
        let want = crate::crypto::hash_virtual_key(pw.trim());
        let current = std::fs::read_to_string(&hash_file)
            .ok()
            .map(|s| s.trim().to_string());
        if current.as_deref() != Some(want.as_str()) {
            write_secret_file(&hash_file, &want)?;
        }
        return Ok((pw, None));
    }

    if has_hash {
        // No plaintext available; login validates against the stored hash. This
        // placeholder can never match a real password.
        return Ok(("\u{0}no-plaintext-password\u{0}".to_string(), None));
    }

    // Fresh install with no password supplied: generate, persist the hash, and
    // return the plaintext so the caller can print it exactly once.
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut bytes);
    let pw = hex::encode(bytes);
    write_secret_file(&hash_file, &crate::crypto::hash_virtual_key(&pw))?;
    Ok((pw.clone(), Some(pw)))
}

/// Write a file with owner-only (0600) permissions.
fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// File configuration (config.toml) — a fallback source for runtime settings
// ---------------------------------------------------------------------------

/// Runtime settings read from `config.toml` when the corresponding env var or
/// CLI flag is absent. This makes the binary usable with a plain file and no
/// environment at all, while env/CLI still take precedence.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub public_base_url: Option<String>,
    #[serde(default)]
    pub database_url: Option<String>,
    /// Plaintext admin password (stored here only if the operator chose to).
    #[serde(default)]
    pub admin_password: Option<String>,
    #[serde(default)]
    pub shutdown_grace_secs: Option<u64>,
    #[serde(default)]
    pub max_inflight_inferences: Option<u64>,
    #[serde(default)]
    pub ip_rate_limit_per_min: Option<u64>,
    #[serde(default)]
    pub session_ttl_minutes: Option<u64>,
    #[serde(default)]
    pub export_retention_days: Option<u64>,
    #[serde(default)]
    pub alert_webhook_url: Option<String>,
}

fn load_file_config(path: &Path) -> Option<FileConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<FileConfig>(&text) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "config file ignored (parse error)");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Bootstrap file schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapConfig {
    #[serde(default)]
    pub virtual_keys: Vec<BootstrapKey>,
    #[serde(default)]
    pub providers: Vec<BootstrapProvider>,
    #[serde(default)]
    pub aliases: Vec<BootstrapAlias>,
    #[serde(default)]
    pub routes: Vec<BootstrapRoute>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapKey {
    pub name: String,
    /// Full virtual key value. If omitted, one is generated and logged.
    pub key: Option<String>,
    pub owner: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default = "default_wildcard")]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub body_logging: bool,
    #[serde(default)]
    pub rpm_limit: Option<u32>,
    #[serde(default)]
    pub tpm_limit: Option<u32>,
    #[serde(default)]
    pub daily_budget: Option<f64>,
    #[serde(default)]
    pub monthly_budget: Option<f64>,
    #[serde(default)]
    pub max_concurrent_requests: Option<u32>,
}

fn default_wildcard() -> Vec<String> {
    vec!["*".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapProvider {
    pub name: String,
    pub base_url: String,
    /// `openai` | `anthropic` | `gemini` | `plugin`
    pub wire_format: String,
    /// `bearer` | `custom_header` | `query_param`
    #[serde(default = "default_bearer")]
    pub auth_scheme: String,
    #[serde(default)]
    pub custom_header_name: Option<String>,
    #[serde(default)]
    pub custom_param_name: Option<String>,
    #[serde(default)]
    pub extra_headers: std::collections::HashMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_permissive")]
    pub capability_mode: String,
    /// §6.0 plugin capability bindings (`plugin:<id>/<cap>`).
    #[serde(default)]
    pub wire_plugin: Option<String>,
    #[serde(default)]
    pub credential_plugin: Option<String>,
    #[serde(default)]
    pub model_source_plugin: Option<String>,
    /// Upstream credentials (API keys) to seed into the pool.
    #[serde(default)]
    pub accounts: Vec<BootstrapAccount>,
    #[serde(default)]
    pub models: Vec<BootstrapModel>,
    /// Model-list endpoint path override for discovery.
    #[serde(default)]
    pub models_path: Option<String>,
    /// Provider-specific error classification rules.
    #[serde(default)]
    pub rate_limit_rules: Option<toml::Value>,
    #[serde(default)]
    pub follow_redirects: bool,
    #[serde(default)]
    pub credential_hosts: String,
    #[serde(default)]
    pub allow_insecure_tls: bool,
}

fn default_bearer() -> String {
    "bearer".into()
}
fn default_timeout() -> u64 {
    120_000
}
fn default_permissive() -> String {
    "permissive".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapAccount {
    pub label: String,
    pub api_key: String,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default = "default_priority")]
    pub weight: i64,
    #[serde(default)]
    pub soft_quota_usd: Option<f64>,
    #[serde(default = "default_quota_type")]
    pub quota_type: String,
    #[serde(default)]
    pub quota_window_s: Option<i64>,
}

fn default_priority() -> i64 {
    1
}
fn default_quota_type() -> String {
    "none".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapModel {
    pub upstream_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub max_output_tokens: Option<i64>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub prices: Option<BootstrapPrices>,
    #[serde(default)]
    pub parameters: Option<toml::Value>,
    #[serde(default)]
    pub thinking_map: Option<toml::Value>,
    #[serde(default)]
    pub extra_request: Option<toml::Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BootstrapPrices {
    #[serde(default)]
    pub input_per_1m: Option<f64>,
    #[serde(default)]
    pub output_per_1m: Option<f64>,
    #[serde(default)]
    pub cached_per_1m: Option<f64>,
    #[serde(default)]
    pub cache_write_per_1m: Option<f64>,
    #[serde(default)]
    pub thinking_per_1m: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapAlias {
    pub alias: String,
    /// `model` (provider/model-id) or `route` (route name)
    pub target_type: String,
    pub target: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapRoute {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_priority_strategy")]
    pub strategy: String,
    /// Deprecated compatibility input. "error" is migrated to
    /// portability_policy="reject"; all other values are ignored.
    #[serde(default)]
    pub continuity_policy: Option<String>,
    /// FR-2.11: reject | strip_with_warning.
    #[serde(default)]
    pub portability_policy: String,
    #[serde(default)]
    pub fallback_triggers: Option<toml::Value>,
    /// FR-7.3: cache-aware sticky routing.
    #[serde(default)]
    pub cache_affinity: bool,
    #[serde(default)]
    pub sticky_routing: bool,
    #[serde(default)]
    pub max_attempts: Option<i64>,
    #[serde(default)]
    pub max_concurrent_requests: Option<i64>,
    #[serde(default)]
    pub targets: Vec<BootstrapRouteTarget>,
}

impl BootstrapRoute {
    pub fn effective_portability_policy(&self) -> &str {
        if !self.portability_policy.is_empty() {
            &self.portability_policy
        } else if self.continuity_policy.as_deref() == Some("error") {
            "reject"
        } else {
            "strip_with_warning"
        }
    }
}

fn default_priority_strategy() -> String {
    "priority".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapRouteTarget {
    /// Optional account label. Omit to use the provider's executable account
    /// pool instead of pinning one credential.
    #[serde(default)]
    pub account: Option<String>,
    /// `provider/model-id`
    pub model: String,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default)]
    pub weight: Option<i64>,
    /// Typed eligibility predicate (FR-12.3), as an inline TOML/JSON value.
    #[serde(default)]
    pub predicate: Option<toml::Value>,
    /// Per-target canonical/provider parameter overrides.
    #[serde(default)]
    pub param_overrides: Option<toml::Value>,
}

pub fn load_bootstrap(path: &std::path::Path) -> Result<BootstrapConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading bootstrap config {}", path.display()))?;
    let cfg: BootstrapConfig = toml::from_str(&text)
        .with_context(|| format!("parsing bootstrap config {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod bootstrap_config_tests {
    use super::*;

    #[test]
    fn parses_executable_bootstrap_controls() {
        let cfg: BootstrapConfig = toml::from_str(
            r#"
[[virtual_keys]]
name = "dev"
owner = "admin"
allowed_models = ["*"]
allowed_providers = ["provider-1"]
allowed_ips = ["127.0.0.1/32"]
body_logging = true

[[providers]]
name = "Provider"
base_url = "https://api.example.com"
wire_format = "openai"
follow_redirects = true
credential_hosts = "auth.example.com"
rate_limit_rules = { target = { statuses = [403, 404] } }

  [[providers.accounts]]
  label = "primary"
  api_key = "secret"
  priority = 1
  weight = 4
  quota_type = "rolling"
  quota_window_s = 3600

  [[providers.models]]
  upstream_id = "model"
  parameters = { temperature = { supported = true } }
  thinking_map = { levels = { high = "high" } }
  extra_request = { service_tier = "auto" }

[[routes]]
name = "route"
fallback_triggers = { on429 = false, onQuota = true, on5xx = true, onTimeout = false }
portability_policy = "reject"

  [[routes.targets]]
  model = "Provider/model"
  weight = 2
  param_overrides = { temperature = 0.2 }
"#,
        )
        .unwrap();

        assert_eq!(cfg.virtual_keys[0].allowed_providers, vec!["provider-1"]);
        assert!(cfg.virtual_keys[0].body_logging);
        assert!(cfg.providers[0].follow_redirects);
        assert_eq!(cfg.providers[0].accounts[0].weight, 4);
        assert_eq!(cfg.providers[0].accounts[0].quota_window_s, Some(3600));
        assert_eq!(cfg.routes[0].effective_portability_policy(), "reject");
        assert!(cfg.routes[0].fallback_triggers.is_some());
        assert!(cfg.routes[0].targets[0].account.is_none());
        assert!(cfg.routes[0].targets[0].param_overrides.is_some());
    }

    #[test]
    fn legacy_continuity_error_maps_to_reject() {
        let cfg: BootstrapConfig = toml::from_str(
            r#"
[[routes]]
name = "legacy"
continuity_policy = "error"
"#,
        )
        .unwrap();

        assert_eq!(cfg.routes[0].effective_portability_policy(), "reject");
    }
}

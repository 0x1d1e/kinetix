//! Database layer: connection pool, migrations, and typed access to every
//! configuration and logging table. SQLite in WAL mode via `sqlx`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use crate::types::{AuthScheme, Capabilities, ParamSpec, Prices, ThinkingMap, WireFormat};

pub type Pool = SqlitePool;

pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

pub async fn connect(database_url: &str) -> Result<Pool> {
    let opts = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid database url {database_url}"))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(10))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(16)
        .connect_with(opts)
        .await
        .context("connecting to sqlite")?;
    Ok(pool)
}

pub async fn migrate(pool: &Pool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("running migrations")?;
    enforce_provider_pricing_scopes(pool)
        .await
        .context("enforcing provider pricing scopes after migrations")?;
    Ok(())
}

/// Write a consistent pre-migration snapshot (NFR-2.4). Existing databases
/// must be backed up successfully before migrations are allowed to run.
pub async fn backup_before_migration(
    pool: &Pool,
    database_url: &str,
    data_dir: &std::path::Path,
) -> Result<Option<PathBuf>> {
    let Some(src) = database_file_path(database_url) else {
        return Ok(None);
    };
    if !src.exists() {
        return Ok(None);
    }
    let schema_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await
    .context("checking database schema before migration backup")?;
    if schema_tables == 0 {
        return Ok(None); // fresh database: no existing schema to preserve
    }

    let backup_dir = data_dir.join("backups");
    std::fs::create_dir_all(&backup_dir).context("creating pre-migration backup directory")?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let dst = backup_dir.join(format!(
        "kinetix-pre-migration-{stamp}-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let sql = vacuum_into_sql(&dst);
    sqlx::query(&sql)
        .execute(pool)
        .await
        .with_context(|| format!("writing pre-migration backup {}", dst.display()))?;
    tracing::info!(backup = %dst.display(), "wrote pre-migration backup");
    Ok(Some(dst))
}

fn database_file_path(database_url: &str) -> Option<PathBuf> {
    let path = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))?
        .split('?')
        .next()
        .unwrap_or("");
    if path.is_empty() || path == ":memory:" {
        return None;
    }
    Some(PathBuf::from(path))
}

fn vacuum_into_sql(path: &std::path::Path) -> String {
    format!(
        "VACUUM INTO '{}'",
        path.display().to_string().replace('\'', "''")
    )
}

fn is_scheduled_backup_filename(name: &str) -> bool {
    let Some(stamp) = name
        .strip_prefix("kinetix-")
        .and_then(|name| name.strip_suffix(".db"))
    else {
        return false;
    };
    let bytes = stamp.as_bytes();
    bytes.len() == 16
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z'
}

/// A consistent, WAL-safe scheduled backup using `VACUUM INTO` (NFR-2.4).
///
/// Unlike a raw file copy, `VACUUM INTO` produces a transactionally consistent
/// snapshot even while the database is live. Returns `Ok(None)` for in-memory
/// databases and `Err` when the backup fails.
pub async fn scheduled_backup(
    pool: &Pool,
    database_url: &str,
    data_dir: &std::path::Path,
    retain: usize,
) -> Result<Option<PathBuf>, String> {
    if database_file_path(database_url).is_none() {
        return Ok(None);
    }
    let backup_dir = data_dir.join("backups");
    if let Err(e) = std::fs::create_dir_all(&backup_dir) {
        return Err(format!("cannot create backup dir: {e}"));
    }
    // NFR-2.4: document the restore path next to the backups so recovery does
    // not depend on tribal knowledge (overwritten on each run).
    let readme = "Kinetix database backups\n\
=======================\n\n\
These files are transactionally-consistent snapshots written by `VACUUM INTO`,\n\
including pre-migration snapshots taken before migrations run.\n\n\
To restore:\n\n\
  1. Stop Kinetix (systemctl stop kinetix).\n\
  2. Remove the live database and its WAL sidecars:\n\
       rm -f /var/lib/kinetix/kinetix.db /var/lib/kinetix/kinetix.db-wal /var/lib/kinetix/kinetix.db-shm\n\
  3. Copy the chosen snapshot into place:\n\
       cp <snapshot>.db /var/lib/kinetix/kinetix.db\n\
  4. Ensure ownership matches the service user (chown kinetix:kinetix).\n\
  5. Start Kinetix (systemctl start kinetix); migrations re-run automatically.\n\n\
Retention: the newest 14 scheduled snapshots are kept; older ones are pruned.\n";
    let _ = std::fs::write(backup_dir.join("RESTORE.txt"), readme);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let dst = backup_dir.join(format!("kinetix-{stamp}.db"));
    let sql = vacuum_into_sql(&dst);
    if let Err(e) = sqlx::query(&sql).execute(pool).await {
        tracing::warn!(error = %e, "scheduled backup failed");
        return Err(format!("VACUUM INTO failed: {e}"));
    }
    tracing::info!(backup = %dst.display(), "wrote scheduled backup");
    // Retain scheduled snapshots only; pre-migration restore points are separate.
    if let Ok(entries) = std::fs::read_dir(&backup_dir) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_scheduled_backup_filename)
            })
            .collect();
        files.sort();
        while files.len() > retain.max(1) {
            let old = files.remove(0);
            let _ = std::fs::remove_file(&old);
        }
    }
    Ok(Some(dst))
}

// ===========================================================================
// Virtual keys
// ===========================================================================

#[derive(Debug, Clone, FromRow)]
pub struct VirtualKeyRow {
    pub id: String,
    pub key_hash: String,
    pub name: String,
    pub owner: String,
    pub tag: String,
    pub allowed_models: String,
    pub allowed_providers: String,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub daily_budget: Option<f64>,
    pub monthly_budget: Option<f64>,
    pub expires_at: Option<String>,
    pub status: String,
    pub allowed_ips: String,
    pub body_logging: i64,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

impl VirtualKeyRow {
    pub fn allowed_models(&self) -> Vec<String> {
        serde_json::from_str(&self.allowed_models).unwrap_or_else(|_| vec!["*".into()])
    }
    pub fn allowed_providers(&self) -> Vec<String> {
        serde_json::from_str(&self.allowed_providers).unwrap_or_default()
    }
    pub fn allowed_ips(&self) -> Vec<String> {
        serde_json::from_str(&self.allowed_ips).unwrap_or_default()
    }
    /// Whether a requested model name (alias/route/model) is permitted.
    pub fn permits_model(&self, model: &str) -> bool {
        Self::model_is_allowed(&self.allowed_models(), model)
    }

    /// Apply the same model-grant policy to a parsed allowlist.
    pub(crate) fn model_is_allowed(allowed: &[String], model: &str) -> bool {
        allowed.iter().any(|grant| {
            if grant == "*" {
                true
            } else if let Some(prefix) = grant.strip_suffix('*') {
                model.starts_with(prefix)
            } else {
                grant == model
            }
        })
    }
}

pub async fn insert_virtual_key(pool: &Pool, k: &VirtualKeyRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO virtual_keys
         (id, key_hash, name, owner, tag, allowed_models, allowed_providers, rpm_limit, tpm_limit,
          daily_budget, monthly_budget, expires_at, status, allowed_ips, body_logging, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&k.id)
    .bind(&k.key_hash)
    .bind(&k.name)
    .bind(&k.owner)
    .bind(&k.tag)
    .bind(&k.allowed_models)
    .bind(&k.allowed_providers)
    .bind(k.rpm_limit)
    .bind(k.tpm_limit)
    .bind(k.daily_budget)
    .bind(k.monthly_budget)
    .bind(&k.expires_at)
    .bind(&k.status)
    .bind(&k.allowed_ips)
    .bind(k.body_logging)
    .bind(&k.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_virtual_keys(pool: &Pool) -> Result<Vec<VirtualKeyRow>> {
    Ok(
        sqlx::query_as::<_, VirtualKeyRow>("SELECT * FROM virtual_keys ORDER BY created_at DESC")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_virtual_key_by_hash(pool: &Pool, hash: &str) -> Result<Option<VirtualKeyRow>> {
    Ok(
        sqlx::query_as::<_, VirtualKeyRow>("SELECT * FROM virtual_keys WHERE key_hash = ?")
            .bind(hash)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_virtual_key_by_id(pool: &Pool, id: &str) -> Result<Option<VirtualKeyRow>> {
    Ok(
        sqlx::query_as::<_, VirtualKeyRow>("SELECT * FROM virtual_keys WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_virtual_key_status(pool: &Pool, id: &str, status: &str) -> Result<()> {
    let revoked_at = if status == "revoked" {
        Some(now_iso())
    } else {
        None
    };
    sqlx::query(
        "UPDATE virtual_keys SET status = ?, revoked_at = COALESCE(?, revoked_at) WHERE id = ?",
    )
    .bind(status)
    .bind(revoked_at)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_virtual_key(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM virtual_keys WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ===========================================================================
// Providers
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub wire_format: String,
    pub auth_scheme: String,
    pub custom_header_name: Option<String>,
    pub custom_param_name: Option<String>,
    pub extra_headers: String,
    pub timeout_ms: i64,
    pub capability_mode: String,
    pub models_path: Option<String>,
    pub rate_limit_rules: String,
    pub enabled: i64,
    pub follow_redirects: i64,
    pub credential_hosts: String,
    pub allow_insecure_tls: i64,
    pub created_at: String,
    /// §6.0 plugin binding: `plugin:<id>/<capability>` or empty for native.
    #[serde(default)]
    pub wire_plugin: String,
    #[serde(default)]
    pub credential_plugin: String,
    #[serde(default)]
    pub model_source_plugin: String,
    pub credential_mode: String,
    #[serde(default)]
    pub source_plugin_id: Option<String>,
    #[serde(default)]
    pub source_integration_id: Option<String>,
    pub pricing_scope: String,
}

impl ProviderRow {
    pub fn wire(&self) -> WireFormat {
        WireFormat::parse(&self.wire_format).unwrap_or(WireFormat::Openai)
    }
    pub fn auth(&self) -> AuthScheme {
        AuthScheme::parse(&self.auth_scheme).unwrap_or(AuthScheme::Bearer)
    }
    pub fn extra_headers_map(&self) -> HashMap<String, String> {
        serde_json::from_str(&self.extra_headers).unwrap_or_default()
    }
    pub fn strict(&self) -> bool {
        self.capability_mode == "strict"
    }
    /// NFR-3.10: redirects are followed only when explicitly enabled.
    pub fn follows_redirects(&self) -> bool {
        self.follow_redirects != 0
    }
    /// NFR-3.12: TLS verification may only be disabled in an explicit dev mode.
    pub fn insecure_tls(&self) -> bool {
        self.allow_insecure_tls != 0
    }
    /// NFR-3.11: the host(s) a credential is authorized for. Empty means the
    /// provider's own base_url host only.
    pub fn credential_hosts(&self) -> Vec<String> {
        self.credential_hosts
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
    /// The host of the provider's configured base URL.
    pub fn base_host(&self) -> Option<String> {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    }
    /// The plugin capability this provider's outbound wire format is bound to
    /// (§6.0), if any.
    pub fn wire_plugin_ref(&self) -> Option<crate::plugins::PluginRef> {
        crate::plugins::PluginRef::parse(&self.wire_plugin)
    }
    /// The plugin capability supplying this provider's credentials (§6.0).
    pub fn credential_plugin_ref(&self) -> Option<crate::plugins::PluginRef> {
        crate::plugins::PluginRef::parse(&self.credential_plugin)
    }
    /// The plugin capability supplying this provider's model discovery (§6.0).
    pub fn model_source_plugin_ref(&self) -> Option<crate::plugins::PluginRef> {
        crate::plugins::PluginRef::parse(&self.model_source_plugin)
    }

    /// Whether a destination host is authorized to receive this provider's
    /// credential (NFR-3.11).
    pub fn host_authorized(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if let Some(base) = self.base_host() {
            if base.to_ascii_lowercase() == host {
                return true;
            }
        }
        self.credential_hosts()
            .iter()
            .any(|h| h.to_ascii_lowercase() == host)
    }
}

pub async fn list_providers(pool: &Pool) -> Result<Vec<ProviderRow>> {
    Ok(
        sqlx::query_as::<_, ProviderRow>("SELECT * FROM providers ORDER BY created_at")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_provider(pool: &Pool, id: &str) -> Result<Option<ProviderRow>> {
    Ok(
        sqlx::query_as::<_, ProviderRow>("SELECT * FROM providers WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub struct NewProvider<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub wire_format: WireFormat,
    pub auth_scheme: AuthScheme,
    pub custom_header_name: Option<&'a str>,
    pub custom_param_name: Option<&'a str>,
    pub extra_headers: Value,
    pub timeout_ms: i64,
    pub capability_mode: &'a str,
    pub models_path: Option<&'a str>,
    pub rate_limit_rules: Value,
    pub follow_redirects: bool,
    pub credential_hosts: &'a str,
    pub allow_insecure_tls: bool,
    /// §6.0 plugin bindings (`plugin:<id>/<cap>` or empty).
    pub wire_plugin: &'a str,
    pub credential_plugin: &'a str,
    pub model_source_plugin: &'a str,
    pub credential_mode: &'a str,
    pub source_plugin_id: Option<&'a str>,
    pub source_integration_id: Option<&'a str>,
}

pub fn conservative_provider_pricing_scope(
    credential_mode: &str,
    source_plugin_id: Option<&str>,
    source_integration_id: Option<&str>,
    wire_plugin: &str,
    credential_plugin: &str,
    model_source_plugin: &str,
) -> &'static str {
    if source_plugin_id.is_some()
        || source_integration_id.is_some()
        || credential_mode != "manual"
        || !wire_plugin.is_empty()
        || !credential_plugin.is_empty()
        || !model_source_plugin.is_empty()
    {
        "integration"
    } else {
        "direct_api"
    }
}

pub async fn insert_provider(pool: &Pool, p: &NewProvider<'_>) -> Result<String> {
    let id = format!("prov_{}", uuid::Uuid::new_v4().simple());
    let pricing_scope = conservative_provider_pricing_scope(
        p.credential_mode,
        p.source_plugin_id,
        p.source_integration_id,
        p.wire_plugin,
        p.credential_plugin,
        p.model_source_plugin,
    );
    sqlx::query(
        "INSERT INTO providers
         (id, name, base_url, wire_format, auth_scheme, custom_header_name, custom_param_name,
          extra_headers, timeout_ms, capability_mode, models_path, rate_limit_rules, enabled,
          follow_redirects, credential_hosts, allow_insecure_tls, created_at,
          wire_plugin, credential_plugin, model_source_plugin, credential_mode,
          source_plugin_id, source_integration_id, pricing_scope)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,1,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(p.name)
    .bind(p.base_url)
    .bind(p.wire_format.as_str())
    .bind(auth_scheme_str(p.auth_scheme))
    .bind(p.custom_header_name)
    .bind(p.custom_param_name)
    .bind(p.extra_headers.to_string())
    .bind(p.timeout_ms)
    .bind(p.capability_mode)
    .bind(p.models_path)
    .bind(p.rate_limit_rules.to_string())
    .bind(p.follow_redirects as i64)
    .bind(p.credential_hosts)
    .bind(p.allow_insecure_tls as i64)
    .bind(now_iso())
    .bind(p.wire_plugin)
    .bind(p.credential_plugin)
    .bind(p.model_source_plugin)
    .bind(p.credential_mode)
    .bind(p.source_plugin_id)
    .bind(p.source_integration_id)
    .bind(pricing_scope)
    .execute(pool)
    .await?;
    Ok(id)
}

pub fn auth_scheme_str(s: AuthScheme) -> &'static str {
    match s {
        AuthScheme::Bearer => "bearer",
        AuthScheme::CustomHeader => "custom_header",
        AuthScheme::QueryParam => "query_param",
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_provider(
    pool: &Pool,
    id: &str,
    p: &NewProvider<'_>,
    explicit_pricing_scope: Option<&str>,
) -> Result<()> {
    let existing = get_provider(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider '{id}' not found"))?;
    let drivers_changed = existing.credential_mode != p.credential_mode
        || existing.source_plugin_id.as_deref() != p.source_plugin_id
        || existing.source_integration_id.as_deref() != p.source_integration_id
        || existing.wire_plugin != p.wire_plugin
        || existing.credential_plugin != p.credential_plugin
        || existing.model_source_plugin != p.model_source_plugin;
    let catalog_identity_changed = existing.base_url != p.base_url || drivers_changed;
    let conservative = conservative_provider_pricing_scope(
        p.credential_mode,
        p.source_plugin_id,
        p.source_integration_id,
        p.wire_plugin,
        p.credential_plugin,
        p.model_source_plugin,
    );
    let pricing_scope = match explicit_pricing_scope {
        Some(scope @ ("direct_api" | "integration")) => scope.to_string(),
        Some(scope) => anyhow::bail!("invalid provider pricing scope '{scope}'"),
        None if !drivers_changed => existing.pricing_scope.clone(),
        None => conservative.to_string(),
    };

    let _guards = provider_price_guards(pool, id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE providers SET name=?, base_url=?, wire_format=?, auth_scheme=?, custom_header_name=?,
         custom_param_name=?, extra_headers=?, timeout_ms=?, capability_mode=?, models_path=?,
         rate_limit_rules=?, follow_redirects=?, credential_hosts=?, allow_insecure_tls=?,
         wire_plugin=?, credential_plugin=?, model_source_plugin=?, credential_mode=?,
         source_plugin_id=?, source_integration_id=?, pricing_scope=? WHERE id=?",
    )
    .bind(p.name)
    .bind(p.base_url)
    .bind(p.wire_format.as_str())
    .bind(auth_scheme_str(p.auth_scheme))
    .bind(p.custom_header_name)
    .bind(p.custom_param_name)
    .bind(p.extra_headers.to_string())
    .bind(p.timeout_ms)
    .bind(p.capability_mode)
    .bind(p.models_path)
    .bind(p.rate_limit_rules.to_string())
    .bind(p.follow_redirects as i64)
    .bind(p.credential_hosts)
    .bind(p.allow_insecure_tls as i64)
    .bind(p.wire_plugin)
    .bind(p.credential_plugin)
    .bind(p.model_source_plugin)
    .bind(p.credential_mode)
    .bind(p.source_plugin_id)
    .bind(p.source_integration_id)
    .bind(&pricing_scope)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    if pricing_scope == "integration" || catalog_identity_changed {
        revoke_external_catalog_effective_pricing_in_transaction(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn update_provider_credential_semantics(
    pool: &Pool,
    id: &str,
    credential_mode: &str,
    source_plugin_id: Option<&str>,
    source_integration_id: Option<&str>,
) -> Result<()> {
    update_provider_credential_semantics_with_scope(
        pool,
        id,
        credential_mode,
        source_plugin_id,
        source_integration_id,
        None,
    )
    .await
}

pub async fn update_provider_credential_semantics_with_scope(
    pool: &Pool,
    id: &str,
    credential_mode: &str,
    source_plugin_id: Option<&str>,
    source_integration_id: Option<&str>,
    explicit_pricing_scope: Option<&str>,
) -> Result<()> {
    let existing = get_provider(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider '{id}' not found"))?;
    let drivers_changed = existing.credential_mode != credential_mode
        || existing.source_plugin_id.as_deref() != source_plugin_id
        || existing.source_integration_id.as_deref() != source_integration_id;
    let conservative = conservative_provider_pricing_scope(
        credential_mode,
        source_plugin_id,
        source_integration_id,
        &existing.wire_plugin,
        &existing.credential_plugin,
        &existing.model_source_plugin,
    );
    let pricing_scope = match explicit_pricing_scope {
        Some(scope @ ("direct_api" | "integration")) => scope.to_string(),
        Some(scope) => anyhow::bail!("invalid provider pricing scope '{scope}'"),
        None if !drivers_changed => existing.pricing_scope.clone(),
        None => conservative.to_string(),
    };

    let _guards = provider_price_guards(pool, id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE providers
         SET credential_mode=?, source_plugin_id=?, source_integration_id=?, pricing_scope=?
         WHERE id=?",
    )
    .bind(credential_mode)
    .bind(source_plugin_id)
    .bind(source_integration_id)
    .bind(&pricing_scope)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if pricing_scope == "integration" || drivers_changed {
        revoke_external_catalog_effective_pricing_in_transaction(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn update_provider_pricing_scope(
    pool: &Pool,
    id: &str,
    pricing_scope: &str,
) -> Result<()> {
    if !matches!(pricing_scope, "integration" | "direct_api") {
        anyhow::bail!("invalid provider pricing scope '{pricing_scope}'");
    }
    let _guards = provider_price_guards(pool, id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE providers SET pricing_scope=? WHERE id=?")
        .bind(pricing_scope)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if pricing_scope == "integration" {
        revoke_external_catalog_effective_pricing_in_transaction(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn provider_pricing_scope(pool: &Pool, id: &str) -> Result<String> {
    Ok(
        sqlx::query_scalar::<_, String>("SELECT pricing_scope FROM providers WHERE id=?")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

async fn provider_price_guards(
    pool: &Pool,
    provider_id: &str,
) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>> {
    let model_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM models WHERE provider_id=? ORDER BY id")
            .bind(provider_id)
            .fetch_all(pool)
            .await?;
    let mut guards = Vec::with_capacity(model_ids.len());
    for model_id in model_ids {
        guards.push(price_version_lock(&model_id).lock_owned().await);
    }
    Ok(guards)
}

fn clear_price_field(prices: &mut Prices, field: &str) {
    match field {
        "input_per_1m" => prices.input_per_1m = None,
        "output_per_1m" => prices.output_per_1m = None,
        "cached_per_1m" => prices.cached_per_1m = None,
        "cache_write_per_1m" => prices.cache_write_per_1m = None,
        "thinking_per_1m" => prices.thinking_per_1m = None,
        _ => {}
    }
}

fn effective_source_after_revocation(
    fields: &serde_json::Map<String, Value>,
    previous_source: &str,
    prices: &Prices,
) -> String {
    let sources: std::collections::BTreeSet<&str> = fields
        .values()
        .filter_map(|field| field.get("source").and_then(Value::as_str))
        .collect();
    match sources.len() {
        1 => sources
            .into_iter()
            .next()
            .unwrap_or("untracked")
            .to_string(),
        n if n > 1 => "mixed".to_string(),
        _ if prices.is_configured()
            && !crate::model_catalog::is_external_catalog_price_source(previous_source) =>
        {
            previous_source.to_string()
        }
        _ => "untracked".to_string(),
    }
}

async fn revoke_external_catalog_effective_pricing_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    provider_id: &str,
) -> Result<()> {
    let rows =
        sqlx::query("SELECT id, prices, discovery FROM models WHERE provider_id=? ORDER BY id")
            .bind(provider_id)
            .fetch_all(&mut **tx)
            .await?;

    for row in rows {
        let model_id: String = row.try_get("id")?;
        let mut prices: Prices =
            serde_json::from_str(&row.try_get::<String, _>("prices")?).unwrap_or_default();
        let discovery: Value = serde_json::from_str(&row.try_get::<String, _>("discovery")?)
            .unwrap_or_else(|_| serde_json::json!({}));
        let effective = discovery
            .get("effective_pricing")
            .and_then(Value::as_object);
        let previous_source = effective
            .and_then(|value| value.get("source"))
            .and_then(Value::as_str)
            .unwrap_or("untracked");
        let mut fields = effective
            .and_then(|value| value.get("fields"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let external_catalog_fields: Vec<String> = fields
            .iter()
            .filter_map(|(field, provenance)| {
                provenance
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(crate::model_catalog::is_external_catalog_price_source)
                    .then_some(field.clone())
            })
            .collect();
        let legacy_external_catalog_snapshot = external_catalog_fields.is_empty()
            && crate::model_catalog::is_external_catalog_price_source(previous_source);
        if external_catalog_fields.is_empty() && !legacy_external_catalog_snapshot {
            continue;
        }

        if legacy_external_catalog_snapshot {
            for field in [
                "input_per_1m",
                "output_per_1m",
                "cached_per_1m",
                "cache_write_per_1m",
                "thinking_per_1m",
            ] {
                clear_price_field(&mut prices, field);
            }
            fields.clear();
        } else {
            for field in external_catalog_fields {
                clear_price_field(&mut prices, &field);
                fields.remove(&field);
            }
        }

        let mut metadata = effective
            .and_then(|value| value.get("metadata"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if !metadata.is_object() {
            metadata = serde_json::json!({});
        }
        metadata["fields"] = Value::Object(fields.clone());
        if !fields.values().any(|field| {
            field
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(crate::model_catalog::is_external_catalog_price_source)
        }) {
            if let Some(object) = metadata.as_object_mut() {
                object.remove("catalog_source_state");
            }
        }
        let source = effective_source_after_revocation(&fields, previous_source, &prices);
        apply_effective_model_pricing_transaction(tx, &model_id, &prices, &source, &metadata)
            .await?;
    }
    Ok(())
}

pub async fn enforce_provider_pricing_scopes(pool: &Pool) -> Result<()> {
    let provider_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM providers WHERE pricing_scope='integration' ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    for provider_id in provider_ids {
        update_provider_pricing_scope(pool, &provider_id, "integration").await?;
    }
    Ok(())
}

pub async fn delete_provider(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM providers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ===========================================================================
// Accounts
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AccountRow {
    pub id: String,
    pub provider_id: String,
    pub label: String,
    #[serde(skip)]
    pub secret_enc: String,
    pub key_mask: String,
    pub status: String,
    pub cooldown_until: Option<String>,
    pub quota_reset_at: Option<String>,
    pub quota_type: String,
    pub quota_window_s: Option<i64>,
    pub soft_quota_usd: Option<f64>,
    pub priority: i64,
    pub weight: i64,
    pub last_error: Option<String>,
    pub last_probe_at: Option<String>,
    pub circuit_open_until: Option<String>,
    pub consecutive_failures: i64,
    pub created_at: String,
}

pub async fn list_accounts(pool: &Pool) -> Result<Vec<AccountRow>> {
    Ok(
        sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts ORDER BY priority, created_at")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn accounts_for_provider(pool: &Pool, provider_id: &str) -> Result<Vec<AccountRow>> {
    Ok(sqlx::query_as::<_, AccountRow>(
        "SELECT * FROM accounts WHERE provider_id = ? AND status != 'disabled' ORDER BY priority",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get_account(pool: &Pool, id: &str) -> Result<Option<AccountRow>> {
    Ok(
        sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn insert_account(
    pool: &Pool,
    provider_id: &str,
    label: &str,
    secret_enc: &str,
    key_mask: &str,
    priority: i64,
    weight: i64,
    soft_quota_usd: Option<f64>,
    quota_type: &str,
) -> Result<String> {
    let id = format!("acc_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO accounts
         (id, provider_id, label, secret_enc, key_mask, status, quota_type, soft_quota_usd, priority, weight, created_at)
         VALUES (?,?,?,?,?,'healthy',?,?,?,?,?)",
    )
    .bind(&id)
    .bind(provider_id)
    .bind(label)
    .bind(secret_enc)
    .bind(key_mask)
    .bind(quota_type)
    .bind(soft_quota_usd)
    .bind(priority)
    .bind(weight)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn update_account(
    pool: &Pool,
    id: &str,
    label: &str,
    status: &str,
    priority: i64,
    weight: i64,
    soft_quota_usd: Option<f64>,
    quota_type: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE accounts SET label=?, status=?, priority=?, weight=?, soft_quota_usd=?, quota_type=? WHERE id=?",
    )
    .bind(label)
    .bind(status)
    .bind(priority)
    .bind(weight)
    .bind(soft_quota_usd)
    .bind(quota_type)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_account_status(
    pool: &Pool,
    id: &str,
    status: &str,
    cooldown_until: Option<&str>,
    quota_reset_at: Option<&str>,
    last_error: Option<&str>,
) -> Result<()> {
    // A healthy status is a recovery: clear any lingering circuit window so
    // `effective_status` reports Healthy again (FR-4.7).
    if status == "healthy" {
        sqlx::query(
            "UPDATE accounts SET status=?, cooldown_until=?, quota_reset_at=?, last_error=?, \
             circuit_open_until=NULL, consecutive_failures=0 WHERE id=?",
        )
        .bind(status)
        .bind(cooldown_until)
        .bind(quota_reset_at)
        .bind(last_error)
        .bind(id)
        .execute(pool)
        .await?;
        return Ok(());
    }
    sqlx::query(
        "UPDATE accounts SET status=?, cooldown_until=?, quota_reset_at=?, last_error=? WHERE id=?",
    )
    .bind(status)
    .bind(cooldown_until)
    .bind(quota_reset_at)
    .bind(last_error)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_account(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bump the consecutive-failure counter and open the circuit when the
/// threshold is reached (FR-4.7). Returns the new failure count.
pub async fn record_account_failure(
    pool: &Pool,
    id: &str,
    circuit_threshold: i64,
    open_secs: i64,
) -> Result<i64> {
    sqlx::query("UPDATE accounts SET consecutive_failures = consecutive_failures + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    let row = sqlx::query("SELECT consecutive_failures FROM accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let n = row
        .map(|r| r.get::<i64, _>("consecutive_failures"))
        .unwrap_or(0);
    if n >= circuit_threshold {
        let until = (Utc::now() + chrono::Duration::seconds(open_secs)).to_rfc3339();
        sqlx::query("UPDATE accounts SET circuit_open_until = ? WHERE id = ?")
            .bind(until)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(n)
}

/// Clear the circuit breaker and failure counter after a successful probe.
pub async fn reset_account_failures(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE accounts SET consecutive_failures = 0, circuit_open_until = NULL WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the instant an account was last actively probed (half-open recovery,
/// FR-4.7). Kept separate from the status write so it is a pure timestamp touch.
pub async fn touch_probe_at(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("UPDATE accounts SET last_probe_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ===========================================================================
// Models
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ModelRow {
    pub id: String,
    pub provider_id: String,
    pub upstream_id: String,
    pub display_name: String,
    pub enabled: i64,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: String,
    pub prices: String,
    pub parameters: String,
    pub thinking_map: String,
    pub extra_request: String,
    pub discovery: String,
    pub created_at: String,
    /// Host-owned provenance tag for opaque provider-state produced by a plugin
    /// adapter (empty when produced natively). Compatibility is versioned by the
    /// validated discovery descriptor, not by the plugin package version.
    #[serde(default)]
    pub opaque_state_plugin: String,
}

impl ModelRow {
    pub fn caps(&self) -> Capabilities {
        serde_json::from_str(&self.capabilities).unwrap_or_default()
    }
    pub fn prices(&self) -> Prices {
        serde_json::from_str(&self.prices).unwrap_or_default()
    }
    pub fn price_provenance(&self) -> (String, Value) {
        let discovery = serde_json::from_str::<Value>(&self.discovery)
            .unwrap_or_else(|_| serde_json::json!({}));
        let effective = discovery.get("effective_pricing");
        let source = effective
            .and_then(|value| value.get("source"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("operator")
            .to_string();
        let metadata = effective
            .and_then(|value| value.get("metadata"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        (source, metadata)
    }
    pub fn params(&self) -> HashMap<String, ParamSpec> {
        serde_json::from_str(&self.parameters).unwrap_or_default()
    }
    pub fn thinking(&self) -> ThinkingMap {
        serde_json::from_str(&self.thinking_map).unwrap_or_default()
    }
    pub fn extra_request_value(&self) -> Value {
        serde_json::from_str(&self.extra_request).unwrap_or(Value::Null)
    }
}

pub async fn list_models(pool: &Pool) -> Result<Vec<ModelRow>> {
    Ok(
        sqlx::query_as::<_, ModelRow>("SELECT * FROM models ORDER BY created_at")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn models_for_provider(pool: &Pool, provider_id: &str) -> Result<Vec<ModelRow>> {
    Ok(
        sqlx::query_as::<_, ModelRow>("SELECT * FROM models WHERE provider_id = ?")
            .bind(provider_id)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_model(pool: &Pool, id: &str) -> Result<Option<ModelRow>> {
    Ok(
        sqlx::query_as::<_, ModelRow>("SELECT * FROM models WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn find_model_by_upstream(
    pool: &Pool,
    provider_id: &str,
    upstream_id: &str,
) -> Result<Option<ModelRow>> {
    Ok(sqlx::query_as::<_, ModelRow>(
        "SELECT * FROM models WHERE provider_id = ? AND upstream_id = ?",
    )
    .bind(provider_id)
    .bind(upstream_id)
    .fetch_optional(pool)
    .await?)
}

pub struct NewModel<'a> {
    pub provider_id: &'a str,
    pub upstream_id: &'a str,
    pub display_name: &'a str,
    pub enabled: bool,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: Value,
    pub prices: Value,
    pub parameters: Value,
    pub thinking_map: Value,
    pub extra_request: Value,
    pub discovery: Value,
}

pub struct ModelPricingMutation<'a> {
    pub prices: &'a Prices,
    pub source: &'a str,
    pub metadata: &'a Value,
}

pub struct ProviderPricingMutation<'a> {
    pub model_id: &'a str,
    pub latest_observation: &'a Value,
    pub pricing: Option<ModelPricingMutation<'a>>,
}

pub struct ModelOperatorMutation<'a> {
    pub id: &'a str,
    pub display_name: &'a str,
    pub enabled: bool,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: &'a Value,
    pub parameters: &'a Value,
    pub thinking_map: &'a Value,
    pub extra_request: &'a Value,
    pub update_transport: bool,
    pub transport: Option<&'a str>,
    pub discovery_patch: &'a Value,
    pub pricing: Option<ModelPricingMutation<'a>>,
}

pub struct ModelCreation<'a> {
    pub model: NewModel<'a>,
    pub transport: Option<&'a str>,
    pub discovery_patch: &'a Value,
    pub opaque_state_plugin: Option<&'a str>,
    pub pricing: Option<ModelPricingMutation<'a>>,
}

async fn insert_model_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    m: &NewModel<'_>,
) -> Result<String> {
    let id = format!("model_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO models
         (id, provider_id, upstream_id, display_name, enabled, context_window, max_output_tokens,
          capabilities, prices, parameters, thinking_map, extra_request, discovery, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(m.provider_id)
    .bind(m.upstream_id)
    .bind(m.display_name)
    .bind(m.enabled as i64)
    .bind(m.context_window)
    .bind(m.max_output_tokens)
    .bind(m.capabilities.to_string())
    .bind(m.prices.to_string())
    .bind(m.parameters.to_string())
    .bind(m.thinking_map.to_string())
    .bind(m.extra_request.to_string())
    .bind(m.discovery.to_string())
    .bind(now_iso())
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

pub async fn insert_model(pool: &Pool, m: &NewModel<'_>) -> Result<String> {
    let mut tx = pool.begin().await?;
    let id = insert_model_in_transaction(&mut tx, m).await?;
    tx.commit().await?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
/// Record the last discovery observation for a model (FR-10.5). Only the
/// `discovery` column is touched, so admin-edited fields are never overwritten.
pub async fn set_model_discovery(pool: &Pool, id: &str, discovery: &Value) -> Result<()> {
    sqlx::query("UPDATE models SET discovery=? WHERE id=?")
        .bind(discovery.to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically merge observed discovery fields into the model metadata object.
/// The operator-owned `configured_transport` key is never touched by this
/// operation, even if an admin update races with model rediscovery.
pub async fn merge_model_discovery(pool: &Pool, id: &str, fresh: &Value) -> Result<()> {
    let Some(fields) = fresh.as_object() else {
        return Ok(());
    };
    if fields.is_empty() {
        return Ok(());
    }

    let base = "CASE WHEN json_valid(discovery) THEN CASE \
        WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END \
        ELSE '{}' END";
    let mut expression = format!("json_set({base}");
    for _ in fields {
        expression.push_str(", '$.' || ?, json(?)");
    }
    expression.push(')');
    if fresh.get("disappeared").and_then(Value::as_bool) == Some(false) {
        expression = format!("json_remove({expression}, '$.flagged_at')");
    }
    let sql = format!("UPDATE models SET discovery = {expression} WHERE id = ?");
    let mut query = sqlx::query(&sql);
    for (key, value) in fields {
        query = query.bind(key).bind(value.to_string());
    }
    query.bind(id).execute(pool).await?;
    Ok(())
}

/// Set the operator-owned model transport override in the existing discovery
/// metadata envelope. Rediscovery merges observed fields and leaves this key
/// untouched, keeping operator configuration authoritative without duplicating
/// the model provenance schema.
pub async fn set_model_transport_override(
    pool: &Pool,
    id: &str,
    transport: Option<&str>,
) -> Result<()> {
    // Update only the operator-owned JSON key atomically. Rediscovery may merge
    // observed metadata concurrently, but can neither overwrite this override
    // nor lose its own unrelated fields through a read/modify/write race.
    match transport {
        Some(transport) => {
            sqlx::query(
                "UPDATE models SET discovery = json_set(\
                    CASE WHEN json_valid(discovery) THEN \
                        CASE WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END \
                    ELSE '{}' END, '$.configured_transport', ?) WHERE id = ?",
            )
            .bind(transport)
            .bind(id)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE models SET discovery = json_remove(\
                    CASE WHEN json_valid(discovery) THEN \
                        CASE WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END \
                    ELSE '{}' END, '$.configured_transport') WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

pub async fn set_model_opaque_state_plugin(pool: &Pool, id: &str, plugin_id: &str) -> Result<()> {
    sqlx::query("UPDATE models SET opaque_state_plugin=? WHERE id=?")
        .bind(plugin_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_model(
    pool: &Pool,
    id: &str,
    display_name: &str,
    enabled: bool,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities: Value,
    prices: Value,
    parameters: Value,
    thinking_map: Value,
    extra_request: Value,
) -> Result<()> {
    sqlx::query(
        "UPDATE models SET display_name=?, enabled=?, context_window=?, max_output_tokens=?,
         capabilities=?, prices=?, parameters=?, thinking_map=?, extra_request=? WHERE id=?",
    )
    .bind(display_name)
    .bind(enabled as i64)
    .bind(context_window)
    .bind(max_output_tokens)
    .bind(capabilities.to_string())
    .bind(prices.to_string())
    .bind(parameters.to_string())
    .bind(thinking_map.to_string())
    .bind(extra_request.to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn update_model_configuration_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    mutation: &ModelOperatorMutation<'_>,
) -> Result<()> {
    let result = sqlx::query(
        "UPDATE models SET display_name=?, enabled=?, context_window=?, max_output_tokens=?,
         capabilities=?, parameters=?, thinking_map=?, extra_request=? WHERE id=?",
    )
    .bind(mutation.display_name)
    .bind(mutation.enabled as i64)
    .bind(mutation.context_window)
    .bind(mutation.max_output_tokens)
    .bind(mutation.capabilities.to_string())
    .bind(mutation.parameters.to_string())
    .bind(mutation.thinking_map.to_string())
    .bind(mutation.extra_request.to_string())
    .bind(mutation.id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() != 1 {
        anyhow::bail!(
            "model '{}' disappeared while applying operator mutation",
            mutation.id
        );
    }
    Ok(())
}

async fn set_model_transport_override_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    transport: Option<&str>,
) -> Result<()> {
    match transport {
        Some(transport) => {
            sqlx::query(
                "UPDATE models SET discovery = json_set(
                    CASE WHEN json_valid(discovery) THEN
                        CASE WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END
                    ELSE '{}' END, '$.configured_transport', ?) WHERE id = ?",
            )
            .bind(transport)
            .bind(id)
            .execute(&mut **tx)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE models SET discovery = json_remove(
                    CASE WHEN json_valid(discovery) THEN
                        CASE WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END
                    ELSE '{}' END, '$.configured_transport') WHERE id = ?",
            )
            .bind(id)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

async fn set_model_opaque_state_plugin_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    plugin_id: &str,
) -> Result<()> {
    let result = sqlx::query("UPDATE models SET opaque_state_plugin=? WHERE id=?")
        .bind(plugin_id)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() != 1 {
        anyhow::bail!("model '{id}' disappeared while binding opaque-state plugin");
    }
    Ok(())
}

async fn merge_model_discovery_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    fresh: &Value,
) -> Result<()> {
    let Some(fields) = fresh.as_object() else {
        return Ok(());
    };
    if fields.is_empty() {
        return Ok(());
    }

    let base = "CASE WHEN json_valid(discovery) THEN CASE \
        WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END \
        ELSE '{}' END";
    let mut expression = format!("json_set({base}");
    for _ in fields {
        expression.push_str(", '$.' || ?, json(?)");
    }
    expression.push(')');
    if fresh.get("disappeared").and_then(Value::as_bool) == Some(false) {
        expression = format!("json_remove({expression}, '$.flagged_at')");
    }
    let sql = format!("UPDATE models SET discovery = {expression} WHERE id = ?");
    let mut query = sqlx::query(&sql);
    for (key, value) in fields {
        query = query.bind(key).bind(value.to_string());
    }
    let result = query.bind(id).execute(&mut **tx).await?;
    if result.rows_affected() != 1 {
        anyhow::bail!("model '{id}' disappeared while merging discovery metadata");
    }
    Ok(())
}

/// Create one complete runtime-visible model state as a single database
/// transaction. A registry reload can only observe the model after transport,
/// ownership metadata, opaque-state binding, and immutable pricing are complete.
pub async fn commit_model_creation(
    pool: &Pool,
    creation: &ModelCreation<'_>,
) -> Result<(String, Option<String>)> {
    let mut tx = pool.begin().await?;
    let id = insert_model_in_transaction(&mut tx, &creation.model).await?;

    set_model_transport_override_in_transaction(&mut tx, &id, creation.transport).await?;
    merge_model_discovery_in_transaction(&mut tx, &id, creation.discovery_patch).await?;
    if let Some(plugin_id) = creation.opaque_state_plugin {
        set_model_opaque_state_plugin_in_transaction(&mut tx, &id, plugin_id).await?;
    }

    let version_id = if let Some(pricing) = creation.pricing.as_ref() {
        apply_effective_model_pricing_transaction(
            &mut tx,
            &id,
            pricing.prices,
            pricing.source,
            pricing.metadata,
        )
        .await?
    } else {
        None
    };

    tx.commit().await?;
    Ok((id, version_id))
}

/// Apply one operator-owned model mutation as a single database transaction.
/// Runtime-visible fields, ownership metadata, transport, reconciliation state,
/// and immutable/effective pricing either all commit or all roll back.
pub async fn commit_model_operator_mutation(
    pool: &Pool,
    mutation: &ModelOperatorMutation<'_>,
) -> Result<Option<String>> {
    let lock = price_version_lock(mutation.id);
    let _guard = lock.lock().await;
    let mut tx = pool.begin().await?;

    update_model_configuration_in_transaction(&mut tx, mutation).await?;
    if mutation.update_transport {
        set_model_transport_override_in_transaction(&mut tx, mutation.id, mutation.transport)
            .await?;
    }
    merge_model_discovery_in_transaction(&mut tx, mutation.id, mutation.discovery_patch).await?;

    let version_id = if let Some(pricing) = mutation.pricing.as_ref() {
        apply_effective_model_pricing_transaction(
            &mut tx,
            mutation.id,
            pricing.prices,
            pricing.source,
            pricing.metadata,
        )
        .await?
    } else {
        None
    };

    tx.commit().await?;
    Ok(version_id)
}

pub async fn update_model_prices(pool: &Pool, id: &str, prices: &Prices) -> Result<()> {
    sqlx::query("UPDATE models SET prices = ? WHERE id = ?")
        .bind(serde_json::to_string(prices)?)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_model(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM models WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record an immutable price snapshot with provenance (FR-6.3).
pub async fn insert_price_version_with_source(
    pool: &Pool,
    model_id: &str,
    p: &Prices,
    source: &str,
    source_metadata: &Value,
) -> Result<String> {
    let id = format!("price_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO price_versions
         (id, model_id, input_per_1m, output_per_1m, cached_per_1m, cache_write_per_1m,
          thinking_per_1m, created_at, source, source_metadata)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(model_id)
    .bind(p.input_per_1m)
    .bind(p.output_per_1m)
    .bind(p.cached_per_1m)
    .bind(p.cache_write_per_1m)
    .bind(p.thinking_per_1m)
    .bind(now_iso())
    .bind(source)
    .bind(source_metadata.to_string())
    .execute(pool)
    .await?;
    Ok(id)
}

/// Backwards-compatible operator-owned price version insertion.
pub async fn insert_price_version(pool: &Pool, model_id: &str, p: &Prices) -> Result<String> {
    insert_price_version_with_source(pool, model_id, p, "operator", &serde_json::json!({})).await
}

fn stable_price_provenance_metadata(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut stable = serde_json::Map::new();
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "observed_at" | "retrieved_at" | "etag" | "last_modified" | "freshness"
                ) {
                    continue;
                }
                stable.insert(key.clone(), stable_price_provenance_metadata(value));
            }
            Value::Object(stable)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(stable_price_provenance_metadata)
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn price_provenance_matches(
    stored_source: &str,
    stored_metadata: &str,
    source: &str,
    source_metadata: &Value,
) -> bool {
    if stored_source != source {
        return false;
    }
    serde_json::from_str::<Value>(stored_metadata).is_ok_and(|stored| {
        stable_price_provenance_metadata(&stored)
            == stable_price_provenance_metadata(source_metadata)
    })
}

fn price_snapshot_matches(
    row: &sqlx::sqlite::SqliteRow,
    p: &Prices,
    source: &str,
    source_metadata: &Value,
) -> Result<bool> {
    Ok(
        row.try_get::<Option<f64>, _>("input_per_1m")? == p.input_per_1m
            && row.try_get::<Option<f64>, _>("output_per_1m")? == p.output_per_1m
            && row.try_get::<Option<f64>, _>("cached_per_1m")? == p.cached_per_1m
            && row.try_get::<Option<f64>, _>("cache_write_per_1m")? == p.cache_write_per_1m
            && row.try_get::<Option<f64>, _>("thinking_per_1m")? == p.thinking_per_1m
            && price_provenance_matches(
                &row.try_get::<String, _>("source")?,
                &row.try_get::<String, _>("source_metadata")?,
                source,
                source_metadata,
            ),
    )
}

fn price_version_lock(model_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        dashmap::DashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
    > = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(dashmap::DashMap::new)
        .entry(model_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Resolve the immutable snapshot backing an effective price.
///
/// Snapshot identity is global within a model's price history, not merely
/// consecutive. A request that finishes after pricing changed can therefore
/// reuse the immutable version it started with instead of re-inserting it.
pub async fn ensure_price_version(
    pool: &Pool,
    model_id: &str,
    p: &Prices,
    source: &str,
    source_metadata: &Value,
) -> Result<Option<String>> {
    if !p.is_configured() {
        return Ok(None);
    }

    // Request finalization, pricing sync, and admin edits can all resolve the
    // same immutable snapshot concurrently. Serialize identity resolution per
    // model so the SELECT -> INSERT sequence cannot create duplicate versions.
    let lock = price_version_lock(model_id);
    let _guard = lock.lock().await;

    let candidates = sqlx::query(
        "SELECT id, input_per_1m, output_per_1m, cached_per_1m, cache_write_per_1m,
                thinking_per_1m, source, source_metadata
         FROM price_versions
         WHERE model_id = ?
           AND input_per_1m IS ?
           AND output_per_1m IS ?
           AND cached_per_1m IS ?
           AND cache_write_per_1m IS ?
           AND thinking_per_1m IS ?
           AND source = ?
         ORDER BY created_at DESC, rowid DESC",
    )
    .bind(model_id)
    .bind(p.input_per_1m)
    .bind(p.output_per_1m)
    .bind(p.cached_per_1m)
    .bind(p.cache_write_per_1m)
    .bind(p.thinking_per_1m)
    .bind(source)
    .fetch_all(pool)
    .await?;

    for row in candidates {
        if price_snapshot_matches(&row, p, source, source_metadata)? {
            return Ok(Some(row.try_get::<String, _>("id")?));
        }
    }

    Ok(Some(
        insert_price_version_with_source(pool, model_id, p, source, source_metadata).await?,
    ))
}

async fn ensure_price_version_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    model_id: &str,
    p: &Prices,
    source: &str,
    source_metadata: &Value,
) -> Result<Option<String>> {
    if !p.is_configured() {
        return Ok(None);
    }

    let candidates = sqlx::query(
        "SELECT id, input_per_1m, output_per_1m, cached_per_1m, cache_write_per_1m,
                thinking_per_1m, source, source_metadata
         FROM price_versions
         WHERE model_id = ?
           AND input_per_1m IS ?
           AND output_per_1m IS ?
           AND cached_per_1m IS ?
           AND cache_write_per_1m IS ?
           AND thinking_per_1m IS ?
           AND source = ?
         ORDER BY created_at DESC, rowid DESC",
    )
    .bind(model_id)
    .bind(p.input_per_1m)
    .bind(p.output_per_1m)
    .bind(p.cached_per_1m)
    .bind(p.cache_write_per_1m)
    .bind(p.thinking_per_1m)
    .bind(source)
    .fetch_all(&mut **tx)
    .await?;

    for row in candidates {
        if price_snapshot_matches(&row, p, source, source_metadata)? {
            return Ok(Some(row.try_get::<String, _>("id")?));
        }
    }

    let id = format!("price_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO price_versions
         (id, model_id, input_per_1m, output_per_1m, cached_per_1m, cache_write_per_1m,
          thinking_per_1m, created_at, source, source_metadata)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(model_id)
    .bind(p.input_per_1m)
    .bind(p.output_per_1m)
    .bind(p.cached_per_1m)
    .bind(p.cache_write_per_1m)
    .bind(p.thinking_per_1m)
    .bind(now_iso())
    .bind(source)
    .bind(source_metadata.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(Some(id))
}

async fn apply_effective_model_pricing_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    model_id: &str,
    prices: &Prices,
    source: &str,
    source_metadata: &Value,
) -> Result<Option<String>> {
    let version_id =
        ensure_price_version_in_transaction(tx, model_id, prices, source, source_metadata).await?;
    let fields = source_metadata
        .get("fields")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let has_owned_fields = fields.as_object().is_some_and(|fields| !fields.is_empty());
    let effective_pricing = if prices.is_configured() || has_owned_fields {
        serde_json::json!({
            "source": source,
            "fields": fields,
            "metadata": source_metadata,
            "price_version_id": version_id,
            "updated_at": now_iso(),
        })
    } else {
        Value::Null
    };
    let result = sqlx::query(
        "UPDATE models
         SET prices = ?,
             discovery = json_set(
                 CASE WHEN json_valid(discovery) THEN
                     CASE WHEN json_type(discovery) = 'object' THEN discovery ELSE '{}' END
                 ELSE '{}' END,
                 '$.effective_pricing',
                 json(?)
             )
         WHERE id = ?",
    )
    .bind(serde_json::to_string(prices)?)
    .bind(effective_pricing.to_string())
    .bind(model_id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() != 1 {
        anyhow::bail!("model '{model_id}' disappeared while committing effective pricing");
    }
    Ok(version_id)
}

/// Commit one provider-wide pricing synchronization atomically. New discovery
/// observations and effective immutable pricing become visible together, or all
/// staged model changes are rolled back.
pub async fn commit_provider_pricing_batch(
    pool: &Pool,
    mutations: &[ProviderPricingMutation<'_>],
) -> Result<Vec<Option<String>>> {
    if mutations.is_empty() {
        return Ok(Vec::new());
    }

    let mut model_ids: Vec<&str> = mutations.iter().map(|mutation| mutation.model_id).collect();
    model_ids.sort_unstable();
    model_ids.dedup();
    let mut price_guards = Vec::with_capacity(model_ids.len());
    for model_id in model_ids {
        price_guards.push(price_version_lock(model_id).lock_owned().await);
    }

    let mut tx = pool.begin().await?;
    let mut version_ids = Vec::with_capacity(mutations.len());
    for mutation in mutations {
        let discovery_patch = serde_json::json!({
            "latest_observation": mutation.latest_observation,
        });
        merge_model_discovery_in_transaction(&mut tx, mutation.model_id, &discovery_patch).await?;

        let version_id = if let Some(pricing) = mutation.pricing.as_ref() {
            apply_effective_model_pricing_transaction(
                &mut tx,
                mutation.model_id,
                pricing.prices,
                pricing.source,
                pricing.metadata,
            )
            .await?
        } else {
            None
        };
        version_ids.push(version_id);
    }

    tx.commit().await?;
    drop(price_guards);
    Ok(version_ids)
}

/// Atomically bind the effective model prices to the immutable price version and
/// the provenance used by request accounting.
pub async fn commit_effective_model_pricing(
    pool: &Pool,
    model_id: &str,
    prices: &Prices,
    source: &str,
    source_metadata: &Value,
) -> Result<Option<String>> {
    let lock = price_version_lock(model_id);
    let _guard = lock.lock().await;
    let mut tx = pool.begin().await?;
    let version_id = apply_effective_model_pricing_transaction(
        &mut tx,
        model_id,
        prices,
        source,
        source_metadata,
    )
    .await?;
    tx.commit().await?;
    Ok(version_id)
}

// ===========================================================================
// Aliases
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AliasRow {
    pub id: String,
    pub alias: String,
    pub target_type: String,
    pub target_id: String,
    pub description: String,
    pub created_at: String,
}

pub async fn list_aliases(pool: &Pool) -> Result<Vec<AliasRow>> {
    Ok(
        sqlx::query_as::<_, AliasRow>("SELECT * FROM aliases ORDER BY alias")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_alias(pool: &Pool, alias: &str) -> Result<Option<AliasRow>> {
    Ok(
        sqlx::query_as::<_, AliasRow>("SELECT * FROM aliases WHERE alias = ?")
            .bind(alias)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn upsert_alias(
    pool: &Pool,
    alias: &str,
    target_type: &str,
    target_id: &str,
    description: &str,
) -> Result<String> {
    if let Some(existing) = get_alias(pool, alias).await? {
        sqlx::query("UPDATE aliases SET target_type=?, target_id=?, description=? WHERE id=?")
            .bind(target_type)
            .bind(target_id)
            .bind(description)
            .bind(&existing.id)
            .execute(pool)
            .await?;
        return Ok(existing.id);
    }
    let id = format!("alias_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO aliases (id, alias, target_type, target_id, description, created_at) VALUES (?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(alias)
    .bind(target_type)
    .bind(target_id)
    .bind(description)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn delete_alias(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM aliases WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ===========================================================================
// Routes
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct RouteRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub strategy: String,
    pub fallback_triggers: String,
    /// Deprecated storage retained for database compatibility only.
    pub continuity_policy: String,
    pub portability_policy: String,
    pub sticky_routing: i64,
    pub cache_affinity: i64,
    pub max_attempts: Option<i64>,
    pub enabled: i64,
    pub created_at: String,
}

impl RouteRow {
    /// The configured policy for non-portable opaque state (FR-2.11).
    /// Accepts `reject` or `strip_with_warning`.
    pub fn portability(&self) -> &str {
        match self.portability_policy.as_str() {
            "reject" => "reject",
            _ => "strip_with_warning",
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct RouteTargetRow {
    pub id: String,
    pub route_id: String,
    pub account_id: Option<String>,
    pub model_id: String,
    pub priority: i64,
    pub weight: i64,
    pub param_overrides: String,
    pub predicate: String,
}

pub async fn list_routes(pool: &Pool) -> Result<Vec<RouteRow>> {
    Ok(
        sqlx::query_as::<_, RouteRow>("SELECT * FROM routes ORDER BY created_at")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_route(pool: &Pool, id: &str) -> Result<Option<RouteRow>> {
    Ok(
        sqlx::query_as::<_, RouteRow>("SELECT * FROM routes WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_route_by_name(pool: &Pool, name: &str) -> Result<Option<RouteRow>> {
    Ok(
        sqlx::query_as::<_, RouteRow>("SELECT * FROM routes WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn route_targets(pool: &Pool, route_id: &str) -> Result<Vec<RouteTargetRow>> {
    Ok(sqlx::query_as::<_, RouteTargetRow>(
        "SELECT * FROM route_targets WHERE route_id = ? ORDER BY priority, weight DESC",
    )
    .bind(route_id)
    .fetch_all(pool)
    .await?)
}

pub struct NewRoute<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub strategy: &'a str,
    pub fallback_triggers: Value,
    pub portability_policy: &'a str,
    pub sticky_routing: bool,
    pub cache_affinity: bool,
    pub max_attempts: Option<i64>,
}

pub async fn insert_route(pool: &Pool, c: &NewRoute<'_>) -> Result<String> {
    let id = format!("route_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO routes (id, name, description, strategy, fallback_triggers, continuity_policy, portability_policy, sticky_routing, cache_affinity, max_attempts, enabled, created_at)
         VALUES (?,?,?,?,?,'strip',?,?,?,?,1,?)",
    )
    .bind(&id)
    .bind(c.name)
    .bind(c.description)
    .bind(c.strategy)
    .bind(c.fallback_triggers.to_string())
    .bind(c.portability_policy)
    .bind(c.sticky_routing as i64)
    .bind(c.cache_affinity as i64)
    .bind(c.max_attempts)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn update_route(
    pool: &Pool,
    id: &str,
    description: &str,
    strategy: &str,
    fallback_triggers: Value,
    portability_policy: &str,
    sticky_routing: bool,
    cache_affinity: bool,
    max_attempts: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE routes SET description=?, strategy=?, fallback_triggers=?, continuity_policy='strip', portability_policy=?, sticky_routing=?, cache_affinity=?, max_attempts=? WHERE id=?",
    )
    .bind(description)
    .bind(strategy)
    .bind(fallback_triggers.to_string())
    .bind(portability_policy)
    .bind(sticky_routing as i64)
    .bind(cache_affinity as i64)
    .bind(max_attempts)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_route_targets(pool: &Pool, route_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM route_targets WHERE route_id = ?")
        .bind(route_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_route_target(
    pool: &Pool,
    route_id: &str,
    account_id: Option<&str>,
    model_id: &str,
    priority: i64,
    weight: i64,
    predicate: &str,
    param_overrides: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO route_targets (id, route_id, account_id, model_id, priority, weight, param_overrides, predicate)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(format!("tgt_{}", uuid::Uuid::new_v4().simple()))
    .bind(route_id)
    .bind(account_id)
    .bind(model_id)
    .bind(priority)
    .bind(weight)
    .bind(param_overrides)
    .bind(predicate)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_route(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM routes WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ===========================================================================
// Usage logs
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UsageLogRow {
    pub id: String,
    pub request_id: String,
    pub ts: String,
    pub key_id: Option<String>,
    pub key_name: Option<String>,
    pub client_format: String,
    pub requested_model: String,
    pub effective_model: Option<String>,
    pub route_id: Option<String>,
    pub route_name: Option<String>,
    pub fallback_hops: i64,
    pub fallback_path: String,
    pub status: String,
    pub status_code: i64,
    pub latency_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_known: i64,
    pub price_version_id: Option<String>,
    pub cache_status: String,
    pub serving_account_id: Option<String>,
    pub serving_account: Option<String>,
    pub serving_provider: Option<String>,
    pub upstream_request_id: Option<String>,
    pub flagged: i64,
    pub error_message: Option<String>,
    pub usage_confidence: String,
    pub commit_state: String,
    pub retry_count: i64,
    pub route_trace_id: Option<String>,
    pub opaque_route_id: Option<String>,
}

pub async fn insert_usage_log(pool: &Pool, u: &UsageLogRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO usage_logs
        (id, request_id, ts, key_id, key_name, client_format, requested_model, effective_model, route_id,
         route_name, fallback_hops, fallback_path, status, status_code, latency_ms, ttft_ms, input_tokens,
         output_tokens, cached_tokens, cache_write_tokens, thinking_tokens, cost_usd, cost_known, price_version_id, cache_status,
         serving_account_id, serving_account, serving_provider, upstream_request_id, flagged, error_message,
         usage_confidence, commit_state, retry_count, route_trace_id, opaque_route_id)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&u.id)
    .bind(&u.request_id)
    .bind(&u.ts)
    .bind(&u.key_id)
    .bind(&u.key_name)
    .bind(&u.client_format)
    .bind(&u.requested_model)
    .bind(&u.effective_model)
    .bind(&u.route_id)
    .bind(&u.route_name)
    .bind(u.fallback_hops)
    .bind(&u.fallback_path)
    .bind(&u.status)
    .bind(u.status_code)
    .bind(u.latency_ms)
    .bind(u.ttft_ms)
    .bind(u.input_tokens)
    .bind(u.output_tokens)
    .bind(u.cached_tokens)
    .bind(u.cache_write_tokens)
    .bind(u.thinking_tokens)
    .bind(u.cost_usd)
    .bind(u.cost_known)
    .bind(&u.price_version_id)
    .bind(&u.cache_status)
    .bind(&u.serving_account_id)
    .bind(&u.serving_account)
    .bind(&u.serving_provider)
    .bind(&u.upstream_request_id)
    .bind(u.flagged)
    .bind(&u.error_message)
    .bind(&u.usage_confidence)
    .bind(&u.commit_state)
    .bind(u.retry_count)
    .bind(&u.route_trace_id)
    .bind(&u.opaque_route_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn recent_usage(pool: &Pool, limit: i64) -> Result<Vec<UsageLogRow>> {
    Ok(
        sqlx::query_as::<_, UsageLogRow>("SELECT * FROM usage_logs ORDER BY ts DESC LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await?,
    )
}

/// All usage rows whose `ts` falls in the half-open interval `[from, to)`,
/// oldest first. Used by the per-day export job and the manual export endpoint.
pub async fn usage_between(pool: &Pool, from_iso: &str, to_iso: &str) -> Result<Vec<UsageLogRow>> {
    Ok(sqlx::query_as::<_, UsageLogRow>(
        "SELECT * FROM usage_logs WHERE ts >= ? AND ts < ? ORDER BY ts ASC",
    )
    .bind(from_iso)
    .bind(to_iso)
    .fetch_all(pool)
    .await?)
}

/// One row per UTC calendar day (`YYYY-MM-DD`) that has usage, with request and
/// token totals. Drives the usage retention/export view (today/24h/7d/30d).
pub async fn usage_days(pool: &Pool) -> Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query(
        "SELECT substr(ts,1,10) AS day,\n                COUNT(*) AS requests,\n                COALESCE(SUM(COALESCE(input_tokens,0) + COALESCE(output_tokens,0)),0) AS tokens\n         FROM usage_logs GROUP BY day ORDER BY day DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("day"),
                r.get::<i64, _>("requests"),
                r.get::<i64, _>("tokens"),
            )
        })
        .collect())
}

pub async fn usage_summary(pool: &Pool) -> Result<Value> {
    let row = sqlx::query(
        "SELECT
            COUNT(*) as requests,
            COALESCE(SUM(input_tokens),0) as input_tokens,
            COALESCE(SUM(output_tokens),0) as output_tokens,
            COALESCE(SUM(cached_tokens),0) as cached_tokens,
            COALESCE(SUM(cache_write_tokens),0) as cache_write_tokens,
            COALESCE(SUM(thinking_tokens),0) as thinking_tokens,
            COALESCE(SUM(CASE WHEN cost_known != 0 THEN cost_usd ELSE 0.0 END),0.0) as cost_usd,
            COALESCE(SUM(CASE WHEN cost_known = 0 THEN 1 ELSE 0 END),0) as unknown_cost_rows,
            COALESCE(SUM(CASE WHEN usage_confidence = 'unknown' THEN 1 ELSE 0 END),0) as unknown_usage_rows,
            COALESCE(SUM(CASE WHEN usage_confidence = 'estimated' THEN 1 ELSE 0 END),0) as estimated_usage_rows,
            COALESCE(SUM(CASE WHEN status IN ('upstream_error','stream_error','rate_limited','quota_exhausted','client_error') THEN 1 ELSE 0 END),0) as error_rows,
            COALESCE(SUM(fallback_hops),0) as fallback_hops,
            COALESCE(AVG(latency_ms),0.0) as avg_latency,
            COALESCE(AVG(ttft_ms),0.0) as avg_ttft
         FROM usage_logs",
    )
    .fetch_one(pool)
    .await?;
    Ok(serde_json::json!({
        "requests": row.get::<i64, _>("requests"),
        "input_tokens": row.get::<i64, _>("input_tokens"),
        "output_tokens": row.get::<i64, _>("output_tokens"),
        "cached_tokens": row.get::<i64, _>("cached_tokens"),
        "cache_write_tokens": row.get::<i64, _>("cache_write_tokens"),
        "thinking_tokens": row.get::<i64, _>("thinking_tokens"),
        "cost_usd": row.get::<f64, _>("cost_usd"),
        // USD totals are only meaningful for priced usage; unknown-cost rows
        // are counted separately so a total is never read as complete
        // (FR-6.3/6.9).
        "unknown_cost_requests": row.get::<i64, _>("unknown_cost_rows"),
        // Accounting confidence (FR-6.8): provider-reported vs estimated vs
        // unknown. Estimated/unknown rows must never be read as exact.
        "unknown_usage_requests": row.get::<i64, _>("unknown_usage_rows"),
        "estimated_usage_requests": row.get::<i64, _>("estimated_usage_rows"),
        "error_requests": row.get::<i64, _>("error_rows"),
        "fallback_hops": row.get::<i64, _>("fallback_hops"),
        "avg_latency_ms": row.get::<f64, _>("avg_latency"),
        "avg_ttft_ms": row.get::<f64, _>("avg_ttft"),
    }))
}

/// Per-key spend over a window plus the key's configured budget, for budget
/// threshold alerts. Only keys with a budget configured are returned.
pub async fn key_budget_status(
    pool: &Pool,
    since_iso: &str,
) -> Result<Vec<(String, String, f64, Option<f64>)>> {
    let rows = sqlx::query(
        "SELECT k.id as id, k.name as name,
                COALESCE(SUM(u.cost_usd),0.0) as spend,
                k.monthly_budget as monthly_budget
         FROM virtual_keys k
         LEFT JOIN usage_logs u ON u.key_id = k.id AND u.ts >= ?
         GROUP BY k.id
         HAVING k.monthly_budget IS NOT NULL",
    )
    .bind(since_iso)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("id"),
                r.get::<String, _>("name"),
                r.get::<f64, _>("spend"),
                r.get::<Option<f64>, _>("monthly_budget"),
            )
        })
        .collect())
}

/// Sum of cost for a key within a time window (ISO timestamp lower bound).
pub async fn key_spend_since(pool: &Pool, key_id: &str, since_iso: &str) -> Result<f64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(cost_usd),0.0) as total FROM usage_logs WHERE key_id = ? AND ts >= ?",
    )
    .bind(key_id)
    .bind(since_iso)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<f64, _>("total"))
}

/// Sum of cost for an account within a time window (for soft quotas).
pub async fn account_spend_since(pool: &Pool, account_id: &str, since_iso: &str) -> Result<f64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(cost_usd),0.0) as total FROM usage_logs WHERE serving_account_id = ? AND ts >= ?",
    )
    .bind(account_id)
    .bind(since_iso)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<f64, _>("total"))
}

/// Count of requests for a key since a timestamp (for RPM/TPM windows).
pub async fn key_usage_entries_since(
    pool: &Pool,
    key_id: &str,
    since_iso: &str,
) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query(
        "SELECT ts,
                COALESCE(input_tokens,0) + COALESCE(output_tokens,0) AS tokens
         FROM usage_logs
         WHERE key_id = ? AND ts >= ?
         ORDER BY ts ASC",
    )
    .bind(key_id)
    .bind(since_iso)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| (row.get::<String, _>("ts"), row.get::<i64, _>("tokens")))
        .collect())
}

pub async fn key_usage_since(pool: &Pool, key_id: &str, since_iso: &str) -> Result<(i64, i64)> {
    let row = sqlx::query(
        "SELECT COUNT(*) as n, COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)),0) as t
         FROM usage_logs WHERE key_id = ? AND ts >= ?",
    )
    .bind(key_id)
    .bind(since_iso)
    .fetch_one(pool)
    .await?;
    Ok((row.get::<i64, _>("n"), row.get::<i64, _>("t")))
}

// ===========================================================================
// Lifetime totals (for the dashboard)
// ===========================================================================

/// (requests, total tokens) grouped by key_id and by serving_account_id.
pub async fn lifetime_totals(
    pool: &Pool,
) -> Result<(HashMap<String, (i64, i64)>, HashMap<String, (i64, i64)>)> {
    let mut by_key: HashMap<String, (i64, i64)> = HashMap::new();
    let rows = sqlx::query(
        "SELECT key_id, COUNT(*) as n, COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)),0) as t
         FROM usage_logs WHERE key_id IS NOT NULL GROUP BY key_id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        by_key.insert(
            r.get::<String, _>("key_id"),
            (r.get::<i64, _>("n"), r.get::<i64, _>("t")),
        );
    }

    let mut by_account: HashMap<String, (i64, i64)> = HashMap::new();
    let rows = sqlx::query(
        "SELECT serving_account_id, COUNT(*) as n, COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)),0) as t
         FROM usage_logs WHERE serving_account_id IS NOT NULL GROUP BY serving_account_id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        by_account.insert(
            r.get::<String, _>("serving_account_id"),
            (r.get::<i64, _>("n"), r.get::<i64, _>("t")),
        );
    }

    Ok((by_key, by_account))
}

/// (requests, total tokens) grouped by serving_account_id and by key_id.
pub async fn request_counts_by_key(pool: &Pool) -> Result<HashMap<String, i64>> {
    let mut out: HashMap<String, i64> = HashMap::new();
    let rows = sqlx::query(
        "SELECT key_id, COUNT(*) as n FROM usage_logs WHERE key_id IS NOT NULL GROUP BY key_id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        out.insert(r.get::<String, _>("key_id"), r.get::<i64, _>("n"));
    }
    Ok(out)
}

// ===========================================================================
// Audit logs
// ===========================================================================

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AuditLogRow {
    pub id: String,
    pub ts: String,
    pub actor: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub target_name: String,
    pub details: String,
}

pub async fn insert_audit(
    pool: &Pool,
    actor: &str,
    action: &str,
    target_type: &str,
    target_id: &str,
    target_name: &str,
    details: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_logs (id, ts, actor, action, target_type, target_id, target_name, details)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(format!("audit_{}", uuid::Uuid::new_v4().simple()))
    .bind(now_iso())
    .bind(actor)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(target_name)
    .bind(details)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn recent_audit(pool: &Pool, limit: i64) -> Result<Vec<AuditLogRow>> {
    Ok(
        sqlx::query_as::<_, AuditLogRow>("SELECT * FROM audit_logs ORDER BY ts DESC LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await?,
    )
}

// ===========================================================================
// Body logs
// ===========================================================================

pub async fn insert_body_log(
    pool: &Pool,
    request_id: &str,
    key_id: &str,
    direction: &str,
    body: &str,
    retention_days: i64,
) -> Result<()> {
    let expires = Utc::now() + chrono::Duration::days(retention_days);
    sqlx::query(
        "INSERT INTO body_logs (id, request_id, ts, key_id, direction, body, expires_at) VALUES (?,?,?,?,?,?,?)",
    )
    .bind(format!("body_{}", uuid::Uuid::new_v4().simple()))
    .bind(request_id)
    .bind(now_iso())
    .bind(key_id)
    .bind(direction)
    .bind(body)
    .bind(expires.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn purge_expired_body_logs(pool: &Pool) -> Result<u64> {
    let res = sqlx::query("DELETE FROM body_logs WHERE expires_at < ?")
        .bind(now_iso())
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// ===========================================================================
// Settings
// ===========================================================================

pub async fn get_setting(pool: &Pool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

/// Delete a virtual key and everything that points at it, in one transaction,
/// so a revoked/removed key leaves no dangling references (usage rows are
/// retained for accounting but their `key_id` is nulled).
pub async fn delete_virtual_key_cascade(pool: &Pool, id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE usage_logs SET key_id = NULL WHERE key_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM virtual_keys WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_setting(pool: &Pool, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

pub fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

// ===========================================================================
// Route traces (FR-12.14) and flight events (FR-13)
// ===========================================================================

pub async fn insert_route_trace(pool: &Pool, t: &crate::trace::RouteTrace) -> Result<()> {
    sqlx::query(
        "INSERT INTO route_traces
         (id, request_id, opaque_route_id, ts, requested_model, route_id, route_name, final_target,
          commit_state, outcome, steps, warnings)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&t.opaque_route_id)
    .bind(&t.request_id)
    .bind(&t.opaque_route_id)
    .bind(now_iso())
    .bind(&t.requested_model)
    .bind(&t.route_id)
    .bind(&t.route_name)
    .bind(&t.final_target)
    .bind(&t.commit_state)
    .bind(&t.outcome)
    .bind(t.steps_json())
    .bind(t.warnings_json())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct RouteTraceRow {
    pub id: String,
    pub request_id: String,
    pub opaque_route_id: String,
    pub ts: String,
    pub requested_model: String,
    pub route_id: Option<String>,
    pub route_name: Option<String>,
    pub final_target: Option<String>,
    pub commit_state: String,
    pub outcome: String,
    pub steps: String,
    pub warnings: String,
}

pub async fn get_route_trace_by_request(
    pool: &Pool,
    request_id: &str,
) -> Result<Option<RouteTraceRow>> {
    Ok(sqlx::query_as::<_, RouteTraceRow>(
        "SELECT * FROM route_traces WHERE request_id = ? ORDER BY ts DESC LIMIT 1",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_route_trace_by_opaque(
    pool: &Pool,
    opaque_route_id: &str,
) -> Result<Option<RouteTraceRow>> {
    Ok(sqlx::query_as::<_, RouteTraceRow>(
        "SELECT * FROM route_traces WHERE opaque_route_id = ? LIMIT 1",
    )
    .bind(opaque_route_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn insert_flight_event(
    pool: &Pool,
    request_id: &str,
    seq: i64,
    event: &str,
    detail: &str,
    elapsed_ms: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO flight_events (id, request_id, ts, seq, event, detail, elapsed_ms)
         VALUES (?,?,?,?,?,?,?)",
    )
    .bind(format!("flight_{}", uuid::Uuid::new_v4().simple()))
    .bind(request_id)
    .bind(now_iso())
    .bind(seq)
    .bind(event)
    .bind(detail)
    .bind(elapsed_ms)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn purge_old_route_traces(pool: &Pool, retain_days: i64) -> Result<u64> {
    let cutoff = (Utc::now() - chrono::Duration::days(retain_days)).to_rfc3339();
    let res = sqlx::query("DELETE FROM route_traces WHERE ts < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod price_version_identity_tests {
    use super::*;
    use serde_json::json;

    async fn pricing_test_model(tag: &str) -> (Pool, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-price-version-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database_url = format!("sqlite://{}", root.join("kinetix.db").display());
        let pool = connect(&database_url).await.unwrap();
        migrate(&pool).await.unwrap();
        let provider_id = insert_provider(
            &pool,
            &NewProvider {
                name: "pricing-test",
                base_url: "https://example.invalid/v1",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();
        let model_id = insert_model(
            &pool,
            &NewModel {
                provider_id: &provider_id,
                upstream_id: "priced-model",
                display_name: "Priced Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({
                    "input_per_1m": 1.0,
                    "output_per_1m": 2.0
                }),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap();
        (pool, model_id, root)
    }

    #[tokio::test]
    async fn concurrent_identical_price_resolution_reuses_one_version() {
        let (pool, model_id, root) = pricing_test_model("concurrent").await;
        let prices = Prices {
            input_per_1m: Some(1.25),
            output_per_1m: Some(2.5),
            ..Prices::default()
        };
        let metadata = json!({"configured_by": "test"});
        let (left, right) = tokio::join!(
            ensure_price_version(&pool, &model_id, &prices, "operator", &metadata),
            ensure_price_version(&pool, &model_id, &prices, "operator", &metadata),
        );
        let left = left.unwrap().unwrap();
        let right = right.unwrap().unwrap();
        assert_eq!(left, right);

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                .bind(&model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn effective_price_rolls_back_when_version_persistence_fails() {
        let (pool, model_id, root) = pricing_test_model("rollback").await;
        sqlx::query(
            "CREATE TRIGGER reject_price_version_insert
             BEFORE INSERT ON price_versions
             BEGIN
                 SELECT RAISE(ABORT, 'injected price version failure');
             END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let next = Prices {
            input_per_1m: Some(9.0),
            output_per_1m: Some(18.0),
            ..Prices::default()
        };
        let result = commit_effective_model_pricing(
            &pool,
            &model_id,
            &next,
            "operator",
            &json!({"configured_by": "test"}),
        )
        .await;
        assert!(result.is_err());

        let row = get_model(&pool, &model_id).await.unwrap().unwrap();
        assert_eq!(row.prices().input_per_1m, Some(1.0));
        assert_eq!(row.prices().output_per_1m, Some(2.0));
        let discovery: Value = serde_json::from_str(&row.discovery).unwrap();
        assert!(discovery.get("effective_pricing").is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn operator_mutation_rolls_back_model_reconciliation_and_pricing_together() {
        let (pool, model_id, root) = pricing_test_model("operator-mutation-rollback").await;
        let old_prices = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            ..Prices::default()
        };
        let old_version = commit_effective_model_pricing(
            &pool,
            &model_id,
            &old_prices,
            "operator",
            &json!({
                "fields": {
                    "input_per_1m": {"source": "operator", "metadata": {}},
                    "output_per_1m": {"source": "operator", "metadata": {}}
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let original_reconciliation = json!({
            "status": "changed",
            "diff": [
                {"field": "context_window", "configured": null, "observed": 200000},
                {"field": "prices.output_per_1m", "configured": 2.0, "observed": 18.0}
            ]
        });
        merge_model_discovery(
            &pool,
            &model_id,
            &json!({"reconciliation": original_reconciliation.clone()}),
        )
        .await
        .unwrap();

        sqlx::query(
            "CREATE TRIGGER reject_operator_price_version_insert
             BEFORE INSERT ON price_versions
             BEGIN
                 SELECT RAISE(ABORT, 'injected operator price version failure');
             END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let capabilities = json!({"tool_calling": true});
        let parameters = json!({});
        let thinking_map = json!({});
        let extra_request = json!({});
        let discovery_patch = json!({
            "reconciliation": {
                "status": "accepted",
                "diff": []
            },
            "operator_capability_overrides": {
                "tool_calling": true
            }
        });
        let next_prices = Prices {
            input_per_1m: Some(9.0),
            output_per_1m: Some(18.0),
            ..Prices::default()
        };
        let price_metadata = json!({
            "fields": {
                "input_per_1m": {"source": "operator_accept", "metadata": {}},
                "output_per_1m": {"source": "operator_accept", "metadata": {}}
            }
        });
        let result = commit_model_operator_mutation(
            &pool,
            &ModelOperatorMutation {
                id: &model_id,
                display_name: "Changed Name",
                enabled: true,
                context_window: Some(200000),
                max_output_tokens: Some(8192),
                capabilities: &capabilities,
                parameters: &parameters,
                thinking_map: &thinking_map,
                extra_request: &extra_request,
                update_transport: false,
                transport: None,
                discovery_patch: &discovery_patch,
                pricing: Some(ModelPricingMutation {
                    prices: &next_prices,
                    source: "operator_accept",
                    metadata: &price_metadata,
                }),
            },
        )
        .await;
        assert!(result.is_err());

        let row = get_model(&pool, &model_id).await.unwrap().unwrap();
        assert_eq!(row.display_name, "Priced Model");
        assert_eq!(row.context_window, None);
        assert_eq!(row.max_output_tokens, None);
        assert_eq!(row.prices().input_per_1m, Some(1.0));
        assert_eq!(row.prices().output_per_1m, Some(2.0));
        assert_eq!(
            serde_json::from_str::<Value>(&row.capabilities).unwrap(),
            json!({})
        );
        let discovery: Value = serde_json::from_str(&row.discovery).unwrap();
        assert_eq!(
            discovery.get("reconciliation"),
            Some(&original_reconciliation)
        );
        assert_eq!(
            discovery
                .pointer("/effective_pricing/price_version_id")
                .and_then(Value::as_str),
            Some(old_version.as_str())
        );
        assert!(discovery.get("operator_capability_overrides").is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn volatile_catalog_metadata_does_not_change_price_identity() {
        let stored = json!({
            "fields": {
                "input_per_1m": {
                    "source": "models.dev:provider",
                    "metadata": {
                        "observed_at": "2026-09-27T00:00:00Z",
                        "catalog_source_state": {
                            "source": "models.dev",
                            "retrieved_at": "2026-09-27T00:00:00Z",
                            "freshness": "fresh",
                            "etag": "etag-1",
                            "last_modified": "Sun, 27 Sep 2026 00:00:00 GMT"
                        }
                    }
                }
            },
            "catalog_source_state": {
                "source": "models.dev",
                "retrieved_at": "2026-09-27T00:00:00Z",
                "freshness": "fresh",
                "etag": "etag-1"
            }
        });
        let refreshed = json!({
            "fields": {
                "input_per_1m": {
                    "source": "models.dev:provider",
                    "metadata": {
                        "observed_at": "2026-09-28T00:00:00Z",
                        "catalog_source_state": {
                            "source": "models.dev",
                            "retrieved_at": "2026-09-28T00:00:00Z",
                            "freshness": "stale",
                            "etag": "etag-2",
                            "last_modified": "Mon, 28 Sep 2026 00:00:00 GMT"
                        }
                    }
                }
            },
            "catalog_source_state": {
                "source": "models.dev",
                "retrieved_at": "2026-09-28T00:00:00Z",
                "freshness": "stale",
                "etag": "etag-2"
            }
        });

        assert!(price_provenance_matches(
            "models.dev",
            &stored.to_string(),
            "models.dev",
            &refreshed,
        ));
    }

    #[test]
    fn stable_price_ownership_change_changes_identity() {
        assert!(!price_provenance_matches(
            "mixed",
            r#"{"fields":{"input_per_1m":{"source":"models.dev:provider","metadata":{}}}}"#,
            "mixed",
            &json!({
                "fields": {
                    "input_per_1m": {
                        "source": "operator",
                        "metadata": {"configured_by": "admin"}
                    }
                }
            }),
        ));
        assert!(!price_provenance_matches(
            "models.dev",
            r#"{"reference":"models.dev:provider/openai/gpt"}"#,
            "operator_accept",
            &json!({"accepted_from":"models.dev"}),
        ));
        assert!(!price_provenance_matches(
            "models.dev",
            r#"{"reference":"old"}"#,
            "models.dev",
            &json!({"reference":"new"}),
        ));
    }

    #[tokio::test]
    async fn unchanged_prices_reuse_version_across_catalog_refresh_metadata() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-price-version-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = connect(&url).await.unwrap();
        migrate(&pool).await.unwrap();

        let provider_id = insert_provider(
            &pool,
            &NewProvider {
                name: "Provider",
                base_url: "https://example.test",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 120_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "example.test",
                allow_insecure_tls: false,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();
        let prices = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let model_id = insert_model(
            &pool,
            &NewModel {
                provider_id: &provider_id,
                upstream_id: "model",
                display_name: "Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: serde_json::to_value(&prices).unwrap(),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap();

        let first_metadata = json!({
            "fields": {
                "input_per_1m": {
                    "source": "models.dev:provider",
                    "metadata": {"observed_at": "2026-09-27T00:00:00Z"}
                }
            },
            "catalog_source_state": {
                "source": "models.dev",
                "retrieved_at": "2026-09-27T00:00:00Z",
                "etag": "etag-1",
                "freshness": "fresh"
            }
        });
        let second_metadata = json!({
            "fields": {
                "input_per_1m": {
                    "source": "models.dev:provider",
                    "metadata": {"observed_at": "2026-09-28T00:00:00Z"}
                }
            },
            "catalog_source_state": {
                "source": "models.dev",
                "retrieved_at": "2026-09-28T00:00:00Z",
                "etag": "etag-2",
                "freshness": "fresh"
            }
        });

        let first = ensure_price_version(&pool, &model_id, &prices, "models.dev", &first_metadata)
            .await
            .unwrap()
            .unwrap();
        let second =
            ensure_price_version(&pool, &model_id, &prices, "models.dev", &second_metadata)
                .await
                .unwrap()
                .unwrap();

        assert_eq!(first, second);
        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn in_flight_price_versions_reuse_historical_snapshots() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-price-version-race-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = connect(&url).await.unwrap();
        migrate(&pool).await.unwrap();

        let provider_id = insert_provider(
            &pool,
            &NewProvider {
                name: "Provider",
                base_url: "https://example.test",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 120_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "example.test",
                allow_insecure_tls: false,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();
        let model_id = insert_model(
            &pool,
            &NewModel {
                provider_id: &provider_id,
                upstream_id: "model",
                display_name: "Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap();
        let v1 = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            ..Default::default()
        };
        let v2 = Prices {
            input_per_1m: Some(3.0),
            output_per_1m: Some(4.0),
            ..Default::default()
        };
        let metadata = json!({"reference": "operator"});

        let v1_id = ensure_price_version(&pool, &model_id, &v1, "operator", &metadata)
            .await
            .unwrap()
            .unwrap();
        let v2_id = ensure_price_version(&pool, &model_id, &v2, "operator", &metadata)
            .await
            .unwrap()
            .unwrap();
        let late_v1_id = ensure_price_version(&pool, &model_id, &v1, "operator", &metadata)
            .await
            .unwrap()
            .unwrap();
        let current_v2_id = ensure_price_version(&pool, &model_id, &v2, "operator", &metadata)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(late_v1_id, v1_id);
        assert_eq!(current_v2_id, v2_id);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                .bind(&model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 2);

        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn effective_pricing_transaction_rolls_back_price_version_and_model_state() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-effective-pricing-rollback-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = connect(&url).await.unwrap();
        migrate(&pool).await.unwrap();

        let provider_id = insert_provider(
            &pool,
            &NewProvider {
                name: "Provider",
                base_url: "https://example.test",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 120_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "example.test",
                allow_insecure_tls: false,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();
        let old_prices = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            ..Default::default()
        };
        let old_effective = json!({
            "source": "operator",
            "fields": {
                "input_per_1m": {"source": "operator"},
                "output_per_1m": {"source": "operator"}
            },
            "metadata": {},
            "price_version_id": null,
            "updated_at": "2026-09-27T00:00:00Z"
        });
        let model_id = insert_model(
            &pool,
            &NewModel {
                provider_id: &provider_id,
                upstream_id: "model",
                display_name: "Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: serde_json::to_value(&old_prices).unwrap(),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({"effective_pricing": old_effective}),
            },
        )
        .await
        .unwrap();

        let new_prices = Prices {
            input_per_1m: Some(3.0),
            output_per_1m: Some(4.0),
            ..Default::default()
        };
        let metadata = json!({
            "fields": {
                "input_per_1m": {"source": "models.dev:provider"},
                "output_per_1m": {"source": "models.dev:provider"}
            }
        });
        let mut tx = pool.begin().await.unwrap();
        apply_effective_model_pricing_transaction(
            &mut tx,
            &model_id,
            &new_prices,
            "models.dev",
            &metadata,
        )
        .await
        .unwrap();

        let forced_failure = sqlx::query("INSERT INTO definitely_missing_table(value) VALUES (?)")
            .bind("fail")
            .execute(&mut *tx)
            .await;
        assert!(forced_failure.is_err());
        tx.rollback().await.unwrap();

        let row = get_model(&pool, &model_id).await.unwrap().unwrap();
        assert_eq!(row.prices().input_per_1m, old_prices.input_per_1m);
        assert_eq!(row.prices().output_per_1m, old_prices.output_per_1m);
        let discovery: Value = serde_json::from_str(&row.discovery).unwrap();
        assert_eq!(discovery["effective_pricing"], old_effective);
        let versions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                .bind(&model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(versions, 0);

        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }
}

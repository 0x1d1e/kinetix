//! Host-managed plugin persistence (§10).
//!
//! Plugins never touch SQL directly; every access goes through these typed
//! helpers. Plugin KV is encrypted at rest using the host crypto with a
//! separate key label derived from the master key (§10, §8.3).

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row};

use crate::crypto::Crypto;
use crate::db::Pool;

use super::manifest::ValidatedManifest;
use super::types::{CircuitState, Manifest, PluginStatus};

/// A stored plugin row.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PluginRow {
    pub id: String,
    pub version: String,
    pub plugin_api_major: i64,
    pub package_sha256: String,
    pub enabled: i64,
    pub signature: String,
    pub manifest_json: String,
    #[serde(skip)]
    pub component: Vec<u8>,
    pub installed_at: String,
    pub updated_at: String,
    pub pinned_version: Option<String>,
}

impl PluginRow {
    pub fn status(&self) -> PluginStatus {
        if self.enabled != 0 {
            PluginStatus::Enabled
        } else {
            PluginStatus::Disabled
        }
    }

    pub fn manifest(&self) -> Option<Manifest> {
        serde_json::from_str(&self.manifest_json).ok()
    }
}

/// A stored permission grant.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PermissionRow {
    pub plugin_id: String,
    pub permission: String,
    pub value_json: String,
    pub approved_at: String,
}

/// Immutable package provenance retained separately from active plugin state.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PackageRow {
    pub plugin_id: String,
    pub version: String,
    pub package_sha256: String,
    pub package_path: String,
    pub signature: String,
    pub source: String,
    pub installed_at: String,
}

/// A stored circuit-breaker row.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct RuntimeStateRow {
    pub plugin_id: String,
    pub circuit_state: String,
    pub consecutive_failures: i64,
    pub circuit_open_until: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error_at: Option<String>,
}

impl RuntimeStateRow {
    pub fn circuit(&self) -> CircuitState {
        CircuitState::parse(&self.circuit_state)
    }
}

/// Insert or replace an installed plugin, preserving only explicitly validated
/// prior approval for non-expanding updates.
pub async fn upsert_plugin(
    pool: &Pool,
    validated: &ValidatedManifest,
    sha256: &str,
    component: &[u8],
    signature: &str,
    package_path: &str,
    source: &str,
    expected_current_sha256: Option<&str>,
    preserved_grants: Option<&[PermissionGrant]>,
) -> Result<bool> {
    let manifest_json = serde_json::to_string(&validated.manifest)?;
    let api_major = validated.manifest.api_major().unwrap_or(0) as i64;
    let now = crate::db::now_iso();

    let mut tx = pool.begin().await.context("begin plugin upsert")?;
    let current = sqlx::query_as::<_, (String, Option<String>, i64)>(
        "SELECT package_sha256, pinned_version, enabled FROM plugins WHERE id = ?",
    )
    .bind(&validated.manifest.id)
    .fetch_optional(&mut *tx)
    .await?;
    if current.as_ref().map(|row| row.0.as_str()) != expected_current_sha256 {
        bail!("plugin changed during update; review the update again");
    }
    if let Some((_, Some(pinned), _)) = &current {
        if pinned != &validated.manifest.version {
            bail!(
                "plugin '{}' is pinned to version {pinned}; unpin it before updating",
                validated.manifest.id
            );
        }
    }
    let enabled = if preserved_grants.is_some() {
        current.as_ref().map(|row| row.2).unwrap_or(0)
    } else {
        0
    };

    sqlx::query(
        "INSERT INTO plugins
         (id, version, plugin_api_major, package_sha256, enabled, signature,
          manifest_json, component, installed_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT(id) DO UPDATE SET
           version=excluded.version,
           plugin_api_major=excluded.plugin_api_major,
           package_sha256=excluded.package_sha256,
           enabled=excluded.enabled,
           signature=excluded.signature,
           manifest_json=excluded.manifest_json,
           component=excluded.component,
           updated_at=excluded.updated_at",
    )
    .bind(&validated.manifest.id)
    .bind(&validated.manifest.version)
    .bind(api_major)
    .bind(sha256)
    .bind(enabled)
    .bind(signature)
    .bind(&manifest_json)
    .bind(component)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM plugin_permissions WHERE plugin_id = ?")
        .bind(&validated.manifest.id)
        .execute(&mut *tx)
        .await?;
    if let Some(grants) = preserved_grants {
        for grant in grants {
            sqlx::query(
                "INSERT INTO plugin_permissions (plugin_id, permission, value_json, approved_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&validated.manifest.id)
            .bind(&grant.permission)
            .bind(&grant.value_json)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
    }

    // Retain immutable provenance independently from the active plugin row.
    // Reinstalling identical bytes is idempotent; older package versions remain.
    sqlx::query(
        "INSERT INTO plugin_packages
         (plugin_id, version, package_sha256, package_path, signature, source, installed_at)
         VALUES (?,?,?,?,?,?,?)
         ON CONFLICT(plugin_id, package_sha256) DO NOTHING",
    )
    .bind(&validated.manifest.id)
    .bind(&validated.manifest.version)
    .bind(sha256)
    .bind(package_path)
    .bind(signature)
    .bind(source)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    // Ensure a runtime-state row exists.
    sqlx::query(
        "INSERT OR IGNORE INTO plugin_runtime_state (plugin_id, circuit_state, consecutive_failures)
         VALUES (?, 'closed', 0)",
    )
    .bind(&validated.manifest.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(enabled != 0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub permission: String,
    pub value_json: String,
}

pub async fn list_plugins(pool: &Pool) -> Result<Vec<PluginRow>> {
    Ok(
        sqlx::query_as::<_, PluginRow>("SELECT * FROM plugins ORDER BY id")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_plugin(pool: &Pool, id: &str) -> Result<Option<PluginRow>> {
    Ok(
        sqlx::query_as::<_, PluginRow>("SELECT * FROM plugins WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_packages(pool: &Pool, id: &str) -> Result<Vec<PackageRow>> {
    Ok(sqlx::query_as::<_, PackageRow>(
        "SELECT * FROM plugin_packages
         WHERE plugin_id = ?
         ORDER BY installed_at DESC, version DESC",
    )
    .bind(id)
    .fetch_all(pool)
    .await?)
}

pub async fn get_package(pool: &Pool, id: &str, sha256: &str) -> Result<Option<PackageRow>> {
    Ok(sqlx::query_as::<_, PackageRow>(
        "SELECT * FROM plugin_packages WHERE plugin_id = ? AND package_sha256 = ?",
    )
    .bind(id)
    .bind(sha256)
    .fetch_optional(pool)
    .await?)
}

pub async fn set_enabled(pool: &Pool, id: &str, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE plugins SET enabled = ?, updated_at = ? WHERE id = ?")
        .bind(if enabled { 1 } else { 0 })
        .bind(crate::db::now_iso())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_version_pin(pool: &Pool, id: &str, pinned: bool) -> Result<String> {
    let mut tx = pool
        .begin()
        .await
        .context("begin plugin version pin update")?;
    let result = if pinned {
        sqlx::query("UPDATE plugins SET pinned_version = version, updated_at = ? WHERE id = ?")
            .bind(crate::db::now_iso())
            .bind(id)
            .execute(&mut *tx)
            .await?
    } else {
        sqlx::query("UPDATE plugins SET pinned_version = NULL, updated_at = ? WHERE id = ?")
            .bind(crate::db::now_iso())
            .bind(id)
            .execute(&mut *tx)
            .await?
    };
    if result.rows_affected() == 0 {
        bail!("plugin '{id}' is not installed");
    }
    let version: String = sqlx::query_scalar("SELECT version FROM plugins WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(version)
}

pub async fn delete_plugin(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM plugins WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn permissions(pool: &Pool, id: &str) -> Result<Vec<PermissionRow>> {
    Ok(sqlx::query_as::<_, PermissionRow>(
        "SELECT * FROM plugin_permissions WHERE plugin_id = ? ORDER BY permission",
    )
    .bind(id)
    .fetch_all(pool)
    .await?)
}

/// Replace the approved permission set atomically.
pub async fn replace_permissions(pool: &Pool, id: &str, grants: &[PermissionGrant]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("begin plugin permission update")?;
    sqlx::query("DELETE FROM plugin_permissions WHERE plugin_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let now = crate::db::now_iso();
    for grant in grants {
        sqlx::query(
            "INSERT INTO plugin_permissions (plugin_id, permission, value_json, approved_at)
             VALUES (?,?,?,?)",
        )
        .bind(id)
        .bind(&grant.permission)
        .bind(&grant.value_json)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Revoke one approved permission grant.
pub async fn revoke_permission(pool: &Pool, id: &str, permission: &str) -> Result<()> {
    sqlx::query("DELETE FROM plugin_permissions WHERE plugin_id = ? AND permission = ?")
        .bind(id)
        .bind(permission)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn runtime_state(pool: &Pool, id: &str) -> Result<Option<RuntimeStateRow>> {
    Ok(sqlx::query_as::<_, RuntimeStateRow>(
        "SELECT * FROM plugin_runtime_state WHERE plugin_id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Whether an invocation may attempt to enter the circuit. Closed circuits are
/// ready immediately. An open circuit becomes probe-eligible only after its
/// cooldown has elapsed. A half-open circuit already has a probe in flight.
pub async fn circuit_ready(pool: &Pool, id: &str) -> Result<bool> {
    let Some(state) = runtime_state(pool, id).await? else {
        return Ok(true);
    };
    match state.circuit() {
        CircuitState::Closed => Ok(true),
        CircuitState::HalfOpen => Ok(false),
        CircuitState::Open => {
            let Some(until) = state.circuit_open_until.as_deref() else {
                return Ok(false);
            };
            let until = chrono::DateTime::parse_from_rfc3339(until)
                .context("invalid plugin circuit_open_until timestamp")?;
            Ok(until <= chrono::Utc::now())
        }
    }
}

/// Atomically claim the one allowed half-open probe after an open circuit's
/// cooldown. Returns true for a closed circuit or for the caller that won the
/// open -> half_open transition. Concurrent callers observe half_open and fail.
pub async fn claim_circuit_probe(pool: &Pool, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let claimed = sqlx::query(
        "UPDATE plugin_runtime_state
         SET circuit_state = 'half_open', circuit_open_until = NULL
         WHERE plugin_id = ?
           AND circuit_state = 'open'
           AND circuit_open_until IS NOT NULL
           AND julianday(circuit_open_until) <= julianday(?)",
    )
    .bind(id)
    .bind(&now)
    .execute(pool)
    .await?
    .rows_affected();

    if claimed == 1 {
        return Ok(true);
    }

    Ok(match runtime_state(pool, id).await? {
        None => true,
        Some(state) => matches!(state.circuit(), CircuitState::Closed),
    })
}

/// Return a half-open probe to the open state without incrementing failures.
/// Used when the probe is cancelled: cancellation is not a plugin fault, but it
/// also is not evidence that the plugin recovered.
pub async fn reopen_plugin_circuit(pool: &Pool, id: &str, open_secs: i64) -> Result<()> {
    let until = (chrono::Utc::now() + chrono::Duration::seconds(open_secs)).to_rfc3339();
    sqlx::query(
        "UPDATE plugin_runtime_state
         SET circuit_state = 'open', circuit_open_until = ?
         WHERE plugin_id = ? AND circuit_state = 'half_open'",
    )
    .bind(until)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bump the plugin's consecutive-failure counter and open the circuit when the
/// threshold is reached (§15). Returns the new count.
pub async fn record_plugin_failure(
    pool: &Pool,
    id: &str,
    threshold: i64,
    open_secs: i64,
    code: &str,
) -> Result<i64> {
    let now = crate::db::now_iso();
    sqlx::query(
        "UPDATE plugin_runtime_state
         SET consecutive_failures = consecutive_failures + 1,
             last_error_code = ?, last_error_at = ?
         WHERE plugin_id = ?",
    )
    .bind(code)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    let n =
        sqlx::query("SELECT consecutive_failures FROM plugin_runtime_state WHERE plugin_id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .map(|r| r.get::<i64, _>("consecutive_failures"))
            .unwrap_or(0);
    if n >= threshold {
        let until = (chrono::Utc::now() + chrono::Duration::seconds(open_secs)).to_rfc3339();
        sqlx::query(
            "UPDATE plugin_runtime_state SET circuit_state = 'open', circuit_open_until = ? WHERE plugin_id = ?",
        )
        .bind(until)
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(n)
}

pub async fn clear_plugin_failures(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE plugin_runtime_state
         SET consecutive_failures = 0, circuit_state = 'closed', circuit_open_until = NULL
         WHERE plugin_id = ?",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_circuit(
    pool: &Pool,
    id: &str,
    state: CircuitState,
    open_until: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE plugin_runtime_state SET circuit_state = ?, circuit_open_until = ? WHERE plugin_id = ?")
        .bind(state.as_str())
        .bind(open_until)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin KV (encrypted at rest)
// ---------------------------------------------------------------------------

/// Encrypt and store a plugin KV entry.
pub async fn kv_put(
    pool: &Pool,
    crypto: &Crypto,
    plugin_id: &str,
    key: &str,
    value: &[u8],
) -> Result<()> {
    let plaintext = base64::engine::general_purpose::STANDARD.encode(value);
    let enc = crypto.encrypt_kv(&plaintext)?;
    sqlx::query(
        "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES (?,?,?,?)
         ON CONFLICT(plugin_id, key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
    )
    .bind(plugin_id)
    .bind(key)
    .bind(enc.as_bytes())
    .bind(crate::db::now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

/// Encrypt and store a plugin KV entry only if the plugin's plaintext-value
/// storage quota remains satisfied.
///
/// The check runs under `BEGIN IMMEDIATE`, serializing competing writers before
/// usage is measured. Replacing an existing key subtracts its old plaintext
/// value length before adding the new one.
pub async fn kv_put_limited(
    pool: &Pool,
    crypto: &Crypto,
    plugin_id: &str,
    key: &str,
    value: &[u8],
    quota: u64,
) -> Result<()> {
    let plaintext = base64::engine::general_purpose::STANDARD.encode(value);
    let enc = crypto.encrypt_kv(&plaintext)?;

    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

    let result: Result<()> = async {
        let rows = sqlx::query("SELECT key, value FROM plugin_kv WHERE plugin_id = ?")
            .bind(plugin_id)
            .fetch_all(&mut *conn)
            .await?;

        let mut used = 0_u64;
        for row in rows {
            let existing_key: String = row.get("key");
            if existing_key == key {
                continue;
            }
            let encrypted: Vec<u8> = row.get("value");
            let encrypted =
                String::from_utf8(encrypted).context("plugin KV ciphertext not utf-8")?;
            let encoded = crypto.decrypt_kv(&encrypted)?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .context("plugin KV plaintext not base64")?;
            used = used
                .checked_add(decoded.len() as u64)
                .context("plugin KV usage overflow")?;
        }

        let projected = used
            .checked_add(value.len() as u64)
            .context("plugin KV usage overflow")?;
        if projected > quota {
            bail!(
                "storage quota exceeded: projected {projected} bytes exceeds {quota} bytes"
            );
        }

        sqlx::query(
            "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES (?,?,?,?)
             ON CONFLICT(plugin_id, key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        )
        .bind(plugin_id)
        .bind(key)
        .bind(enc.as_bytes())
        .bind(crate::db::now_iso())
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

/// Atomically replace every KV entry under `prefix` while enforcing the
/// plugin's plaintext storage quota. Keys outside the prefix are preserved.
///
/// This is used for cached routing-fact snapshots so a successful refresh
/// cannot leave keys from an older snapshot behind, and a failed/quota-exceeded
/// refresh leaves the previous snapshot intact.
pub async fn kv_replace_prefix_limited(
    pool: &Pool,
    crypto: &Crypto,
    plugin_id: &str,
    prefix: &str,
    entries: &[(String, Vec<u8>)],
    quota: u64,
) -> Result<()> {
    if prefix.is_empty() {
        bail!("KV snapshot prefix must not be empty");
    }
    if entries.iter().any(|(key, _)| !key.starts_with(prefix)) {
        bail!("KV snapshot entry is outside the requested prefix");
    }
    let mut unique_keys = std::collections::HashSet::new();
    if entries
        .iter()
        .any(|(key, _)| !unique_keys.insert(key.as_str()))
    {
        bail!("KV snapshot contains duplicate keys");
    }

    let mut encoded_entries = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let plaintext = base64::engine::general_purpose::STANDARD.encode(value);
        let encrypted = crypto.encrypt_kv(&plaintext)?;
        encoded_entries.push((key.clone(), encrypted, value.len() as u64));
    }

    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

    let result: Result<()> = async {
        let rows = sqlx::query("SELECT key, value FROM plugin_kv WHERE plugin_id = ?")
            .bind(plugin_id)
            .fetch_all(&mut *conn)
            .await?;

        let mut used = 0_u64;
        for row in rows {
            let existing_key: String = row.get("key");
            if existing_key.starts_with(prefix) {
                continue;
            }
            let encrypted: Vec<u8> = row.get("value");
            let encrypted =
                String::from_utf8(encrypted).context("plugin KV ciphertext not utf-8")?;
            let encoded = crypto.decrypt_kv(&encrypted)?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .context("plugin KV plaintext not base64")?;
            used = used
                .checked_add(decoded.len() as u64)
                .context("plugin KV usage overflow")?;
        }

        for (_, _, len) in &encoded_entries {
            used = used
                .checked_add(*len)
                .context("plugin KV usage overflow")?;
        }
        if used > quota {
            bail!("storage quota exceeded: projected {used} bytes exceeds {quota} bytes");
        }

        sqlx::query(
            "DELETE FROM plugin_kv
             WHERE plugin_id = ? AND substr(key, 1, ?) = ?",
        )
        .bind(plugin_id)
        .bind(prefix.len() as i64)
        .bind(prefix)
        .execute(&mut *conn)
        .await?;

        let now = crate::db::now_iso();
        for (key, encrypted, _) in &encoded_entries {
            sqlx::query(
                "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES (?,?,?,?)
                 ON CONFLICT(plugin_id, key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            )
            .bind(plugin_id)
            .bind(key)
            .bind(encrypted.as_bytes())
            .bind(&now)
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

pub async fn kv_get(
    pool: &Pool,
    crypto: &Crypto,
    plugin_id: &str,
    key: &str,
) -> Result<Option<Vec<u8>>> {
    let row = sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id = ? AND key = ?")
        .bind(plugin_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let enc: Vec<u8> = row.get("value");
    let enc = String::from_utf8(enc).context("plugin KV ciphertext not utf-8")?;
    let plaintext = crypto.decrypt_kv(&enc)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(plaintext.trim())
        .context("plugin KV plaintext not base64")?;
    Ok(Some(bytes))
}

pub async fn kv_delete(pool: &Pool, plugin_id: &str, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM plugin_kv WHERE plugin_id = ? AND key = ?")
        .bind(plugin_id)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// List KV entries whose key begins with `prefix`, newest key order. Used by
/// the host to read cached routing facts on the request path (§6.4) without
/// invoking the guest.
pub async fn kv_list_prefix(
    pool: &Pool,
    crypto: &Crypto,
    plugin_id: &str,
    prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query(
        "SELECT key, value FROM plugin_kv
         WHERE plugin_id = ? AND substr(key, 1, ?) = ?
         ORDER BY key",
    )
    .bind(plugin_id)
    .bind(prefix.len() as i64)
    .bind(prefix)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for row in rows {
        let key: String = row.get("key");
        let enc: Vec<u8> = row.get("value");
        let enc = match String::from_utf8(enc) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let plaintext = match crypto.decrypt_kv(&enc) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(plaintext.trim()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        out.push((key, bytes));
    }
    Ok(out)
}

/// Encrypted bytes persisted for a plugin. This is diagnostic only; storage
/// quota enforcement uses plaintext value lengths in `kv_put_limited`.
pub async fn kv_bytes(pool: &Pool, plugin_id: &str) -> Result<u64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(LENGTH(value)), 0) AS n FROM plugin_kv WHERE plugin_id = ?",
    )
    .bind(plugin_id)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<i64, _>("n").max(0) as u64)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    async fn test_store() -> (Pool, Arc<Crypto>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("kinetix-plugin-kv-quota-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let now = crate::db::now_iso();
        sqlx::query(
            "INSERT INTO plugins
             (id, version, plugin_api_major, package_sha256, enabled, signature, manifest_json, component, installed_at, updated_at)
             VALUES ('p', '1.0.0', 1, 'test-sha', 0, 'unsigned', '{}', X'', ?, ?)",
        )
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        (pool, Arc::new(Crypto::new(&[23_u8; 32])), dir)
    }

    #[tokio::test]
    async fn circuit_cooldown_allows_exactly_one_half_open_probe() {
        let (pool, _crypto, dir) = test_store().await;
        let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        sqlx::query(
            "INSERT INTO plugin_runtime_state
             (plugin_id, circuit_state, consecutive_failures, circuit_open_until)
             VALUES ('p', 'open', 5, ?)",
        )
        .bind(&past)
        .execute(&pool)
        .await
        .unwrap();

        assert!(circuit_ready(&pool, "p").await.unwrap());
        assert!(claim_circuit_probe(&pool, "p").await.unwrap());
        assert_eq!(
            runtime_state(&pool, "p").await.unwrap().unwrap().circuit(),
            CircuitState::HalfOpen
        );
        assert!(!circuit_ready(&pool, "p").await.unwrap());
        assert!(!claim_circuit_probe(&pool, "p").await.unwrap());

        reopen_plugin_circuit(&pool, "p", 60).await.unwrap();
        let reopened = runtime_state(&pool, "p").await.unwrap().unwrap();
        assert_eq!(reopened.circuit(), CircuitState::Open);
        assert!(reopened.circuit_open_until.is_some());
        assert!(!circuit_ready(&pool, "p").await.unwrap());

        clear_plugin_failures(&pool, "p").await.unwrap();
        assert!(circuit_ready(&pool, "p").await.unwrap());
        assert!(claim_circuit_probe(&pool, "p").await.unwrap());

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn circuit_open_before_cooldown_remains_blocked() {
        let (pool, _crypto, dir) = test_store().await;
        let future = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
        sqlx::query(
            "INSERT INTO plugin_runtime_state
             (plugin_id, circuit_state, consecutive_failures, circuit_open_until)
             VALUES ('p', 'open', 5, ?)",
        )
        .bind(&future)
        .execute(&pool)
        .await
        .unwrap();

        assert!(!circuit_ready(&pool, "p").await.unwrap());
        assert!(!claim_circuit_probe(&pool, "p").await.unwrap());

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn kv_quota_is_plaintext_and_replacement_aware() {
        let (pool, crypto, dir) = test_store().await;

        // Ciphertext is much larger than three bytes; a three-byte plaintext
        // value must still fit a three-byte manifest storage quota.
        kv_put_limited(&pool, &crypto, "p", "a", b"abc", 3)
            .await
            .unwrap();
        assert_eq!(
            kv_get(&pool, &crypto, "p", "a").await.unwrap(),
            Some(b"abc".to_vec())
        );

        // Replacing the same key does not double-count its old value.
        kv_put_limited(&pool, &crypto, "p", "a", b"123456", 6)
            .await
            .unwrap();

        let err = kv_put_limited(&pool, &crypto, "p", "b", b"x", 6)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("storage quota exceeded"), "{err}");
        assert!(kv_get(&pool, &crypto, "p", "b").await.unwrap().is_none());

        // Shrinking a replacement releases quota for another key.
        kv_put_limited(&pool, &crypto, "p", "a", b"12", 6)
            .await
            .unwrap();
        kv_put_limited(&pool, &crypto, "p", "b", b"3456", 6)
            .await
            .unwrap();

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cached_snapshot_replace_is_atomic_and_removes_old_keys() {
        let (pool, crypto, dir) = test_store().await;

        kv_put_limited(&pool, &crypto, "p", "ordinary", b"xx", 64)
            .await
            .unwrap();
        // '_' is a SQL LIKE wildcard, so this key specifically proves prefix
        // operations use literal matching rather than LIKE semantics.
        kv_put_limited(&pool, &crypto, "p", "xcache:unrelated", b"keep", 64)
            .await
            .unwrap();
        kv_replace_prefix_limited(
            &pool,
            &crypto,
            "p",
            "_cache:",
            &[
                ("_cache:a".into(), b"one".to_vec()),
                ("_cache:b".into(), b"two".to_vec()),
            ],
            64,
        )
        .await
        .unwrap();

        kv_replace_prefix_limited(
            &pool,
            &crypto,
            "p",
            "_cache:",
            &[("_cache:b".into(), b"new".to_vec())],
            64,
        )
        .await
        .unwrap();

        let snapshot = kv_list_prefix(&pool, &crypto, "p", "_cache:")
            .await
            .unwrap();
        assert_eq!(snapshot, vec![("_cache:b".into(), b"new".to_vec())]);
        assert_eq!(
            kv_get(&pool, &crypto, "p", "ordinary").await.unwrap(),
            Some(b"xx".to_vec())
        );
        assert_eq!(
            kv_get(&pool, &crypto, "p", "xcache:unrelated")
                .await
                .unwrap(),
            Some(b"keep".to_vec())
        );

        let err = kv_replace_prefix_limited(
            &pool,
            &crypto,
            "p",
            "_cache:",
            &[("_cache:c".into(), vec![b'x'; 64])],
            64,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("storage quota exceeded"), "{err}");

        // Failed replacement leaves the previous complete snapshot untouched.
        let snapshot = kv_list_prefix(&pool, &crypto, "p", "_cache:")
            .await
            .unwrap();
        assert_eq!(snapshot, vec![("_cache:b".into(), b"new".to_vec())]);

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn concurrent_kv_writers_cannot_overcommit_quota() {
        let (pool, crypto, dir) = test_store().await;
        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let crypto_a = crypto.clone();
        let crypto_b = crypto.clone();

        let a =
            tokio::spawn(
                async move { kv_put_limited(&pool_a, &crypto_a, "p", "a", b"1234", 6).await },
            );
        let b =
            tokio::spawn(
                async move { kv_put_limited(&pool_b, &crypto_b, "p", "b", b"5678", 6).await },
            );

        let (a, b) = tokio::join!(a, b);
        let results = [a.unwrap(), b.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

        let stored = kv_list_prefix(&pool, &crypto, "p", "").await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1.len(), 4);

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}

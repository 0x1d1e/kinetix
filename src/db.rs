//! Database layer: connection pool, migrations, and typed access to every
//! configuration and logging table. SQLite in WAL mode via `sqlx`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;

use crate::types::{AuthScheme, Capabilities, ParamSpec, Prices, ThinkingMap, WireFormat};

pub type Pool = SqlitePool;

const PRE_MIGRATION_BACKUP_RETAIN: usize = 3;
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum OpenDatabaseError {
    #[error("database connection failed: {0}")]
    Connection(#[source] anyhow::Error),
    #[error("database startup failed: {0}")]
    Startup(#[source] anyhow::Error),
}

struct PendingBackupMarkers {
    preparing: PathBuf,
    ready: PathBuf,
    backup_dir: PathBuf,
}

struct PendingPreMigrationBackup {
    markers: PendingBackupMarkers,
    snapshot: PathBuf,
}

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

/// Write a consistent SQLite snapshot to a new database file.
pub async fn snapshot(pool: &Pool, destination: &std::path::Path) -> Result<()> {
    sqlx::query(&vacuum_into_sql(destination))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn open_and_migrate(
    database_url: &str,
    data_dir: &std::path::Path,
) -> std::result::Result<Pool, OpenDatabaseError> {
    let pool = connect(database_url)
        .await
        .map_err(OpenDatabaseError::Connection)?;
    let startup = async {
        let changes_pending = migration_or_repair_will_modify(&pool).await?;
        let markers = pending_backup_markers(database_url, data_dir).await;
        let has_pending_backup = if let Some(markers) = markers.as_ref() {
            tokio::fs::try_exists(&markers.preparing).await?
                || tokio::fs::try_exists(&markers.ready).await?
        } else {
            false
        };
        let pending_backup = if changes_pending || has_pending_backup {
            prepare_pending_pre_migration_backup(&pool, markers, changes_pending).await?
        } else {
            None
        };

        migrate(&pool).await?;
        if let Some(pending_backup) = pending_backup {
            finish_pending_pre_migration_backup(pending_backup).await;
        }
        Ok::<_, anyhow::Error>(pool)
    }
    .await;
    startup.map_err(OpenDatabaseError::Startup)
}

pub async fn migrate(pool: &Pool) -> Result<()> {
    MIGRATOR.run(pool).await.context("running migrations")?;
    enforce_provider_pricing_scopes(pool)
        .await
        .context("enforcing provider pricing scopes after migrations")?;
    Ok(())
}

async fn migration_or_repair_will_modify(pool: &Pool) -> Result<bool> {
    let migrations_table_exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'
        )",
    )
    .fetch_one(pool)
    .await?;
    if migrations_table_exists == 0 {
        return Ok(true);
    }

    let dirty_migration: i64 =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE success = 0)")
            .fetch_one(pool)
            .await?;
    if dirty_migration != 0 {
        return Ok(false);
    }

    let applied_rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version, checksum FROM _sqlx_migrations WHERE success = 1 ORDER BY version",
    )
    .fetch_all(pool)
    .await?;
    let applied: HashMap<i64, Vec<u8>> = applied_rows.into_iter().collect();
    if applied
        .keys()
        .any(|version| !MIGRATOR.version_exists(*version))
    {
        return Ok(false);
    }

    for migration in MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
    {
        match applied.get(&migration.version) {
            Some(checksum) if checksum.as_slice() != migration.checksum.as_ref() => {
                return Ok(false);
            }
            Some(_) => {}
            None => return Ok(true),
        }
    }

    provider_pricing_scope_repairs_pending(pool).await
}

/// Write a consistent pre-migration snapshot (NFR-2.4).
///
/// Production startup uses a pending marker to protect this snapshot until
/// migrations and post-migration repairs both succeed.
pub async fn backup_before_migration(
    pool: &Pool,
    database_url: &str,
    data_dir: &std::path::Path,
) -> Result<Option<PathBuf>> {
    let Some(src) = database_file_path(database_url) else {
        return Ok(None);
    };
    if !tokio::fs::try_exists(&src).await? || !database_has_schema(pool).await? {
        return Ok(None);
    }

    let backup_dir = data_dir.join("backups");
    tokio::fs::create_dir_all(&backup_dir)
        .await
        .context("creating pre-migration backup directory")?;
    let dst = new_pre_migration_backup_path(&backup_dir);
    write_pre_migration_snapshot(pool, &dst).await?;
    Ok(Some(dst))
}

async fn pending_backup_markers(
    database_url: &str,
    data_dir: &std::path::Path,
) -> Option<PendingBackupMarkers> {
    let source = database_file_path(database_url)?;
    let source = tokio::fs::canonicalize(&source).await.unwrap_or(source);
    let digest = Sha256::digest(source.to_string_lossy().as_bytes());
    let backup_dir = data_dir.join("backups");
    let marker_id = hex::encode(digest);
    Some(PendingBackupMarkers {
        preparing: backup_dir.join(format!(".kinetix-pre-migration-{marker_id}.preparing")),
        ready: backup_dir.join(format!(".kinetix-pre-migration-{marker_id}.ready")),
        backup_dir,
    })
}

async fn prepare_pending_pre_migration_backup(
    pool: &Pool,
    markers: Option<PendingBackupMarkers>,
    changes_pending: bool,
) -> Result<Option<PendingPreMigrationBackup>> {
    let Some(markers) = markers else {
        return Ok(None);
    };
    let preparing_exists = tokio::fs::try_exists(&markers.preparing).await?;
    if tokio::fs::try_exists(&markers.ready).await? {
        match read_ready_snapshot(&markers.ready, &markers.backup_dir).await {
            Ok(snapshot) => {
                if preparing_exists {
                    match read_pending_snapshot(&markers.preparing, &markers.backup_dir).await {
                        Ok(preparing_snapshot) if preparing_snapshot == snapshot => {}
                        Ok(_) => anyhow::bail!(
                            "pending pre-migration markers refer to different snapshots"
                        ),
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "discarding incomplete preparing pre-migration marker"
                            );
                            tokio::fs::remove_file(&markers.preparing)
                                .await
                                .context("removing incomplete preparing pre-migration marker")?;
                            sync_backup_directory(&markers.backup_dir).await?;
                        }
                    }
                }
                return Ok(Some(PendingPreMigrationBackup { markers, snapshot }));
            }
            Err(ready_error) if preparing_exists => {
                let preparing_is_valid =
                    read_pending_snapshot(&markers.preparing, &markers.backup_dir)
                        .await
                        .is_ok();
                if !preparing_is_valid {
                    return Err(ready_error).context("validating pending pre-migration markers");
                }
                tracing::warn!(
                    error = %ready_error,
                    "discarding incomplete ready pre-migration marker"
                );
                tokio::fs::remove_file(&markers.ready)
                    .await
                    .context("removing incomplete ready pre-migration marker")?;
                sync_backup_directory(&markers.backup_dir).await?;
            }
            Err(error) => return Err(error),
        }
    }

    if !preparing_exists && !changes_pending {
        return Ok(None);
    }
    if !preparing_exists && !database_has_schema(pool).await? {
        return Ok(None);
    }

    tokio::fs::create_dir_all(&markers.backup_dir)
        .await
        .context("creating pre-migration backup directory")?;
    let snapshot = if preparing_exists {
        match read_pending_snapshot(&markers.preparing, &markers.backup_dir).await {
            Ok(snapshot) => {
                if !pending_snapshot_exists(&snapshot).await? {
                    if !database_has_schema(pool).await? {
                        tokio::fs::remove_file(&markers.preparing).await?;
                        sync_backup_directory(&markers.backup_dir).await?;
                        return Ok(None);
                    }
                    write_pre_migration_snapshot(pool, &snapshot).await?;
                }
                snapshot
            }
            Err(error) => {
                tracing::warn!(error = %error, "discarding incomplete preparing pre-migration marker");
                tokio::fs::remove_file(&markers.preparing)
                    .await
                    .context("removing incomplete preparing pre-migration marker")?;
                sync_backup_directory(&markers.backup_dir).await?;
                if !changes_pending || !database_has_schema(pool).await? {
                    return Ok(None);
                }
                let snapshot = new_pre_migration_backup_path(&markers.backup_dir);
                create_pending_marker(&markers.preparing, &snapshot)
                    .await
                    .context("creating pre-migration backup marker")?;
                write_pre_migration_snapshot(pool, &snapshot).await?;
                snapshot
            }
        }
    } else {
        let snapshot = new_pre_migration_backup_path(&markers.backup_dir);
        create_pending_marker(&markers.preparing, &snapshot)
            .await
            .context("creating pre-migration backup marker")?;
        write_pre_migration_snapshot(pool, &snapshot).await?;
        snapshot
    };

    // A preparing marker can outlive a crash before its directory entry was
    // durable, so make both the snapshot and its directory entry durable again
    // before publishing the ready marker.
    sync_snapshot(&snapshot).await?;
    sync_backup_directory(&markers.backup_dir).await?;
    create_pending_marker(&markers.ready, &snapshot).await?;
    if let Err(error) = tokio::fs::remove_file(&markers.preparing).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %error, "could not remove preparing pre-migration marker");
        }
    } else if let Err(error) = sync_backup_directory(&markers.backup_dir).await {
        tracing::warn!(error = %error, "could not persist preparing marker cleanup");
    }
    Ok(Some(PendingPreMigrationBackup { markers, snapshot }))
}

async fn read_pending_snapshot(
    marker: &std::path::Path,
    backup_dir: &std::path::Path,
) -> Result<PathBuf> {
    let contents = tokio::fs::read_to_string(marker)
        .await
        .with_context(|| format!("reading pending backup marker {}", marker.display()))?;
    let name = contents.trim_end_matches(['\r', '\n']);
    if name.is_empty()
        || name.contains(['\r', '\n'])
        || std::path::Path::new(name)
            .file_name()
            .and_then(|file_name| file_name.to_str())
            != Some(name)
        || !is_pre_migration_backup_filename(name)
    {
        anyhow::bail!("invalid pending pre-migration marker {}", marker.display());
    }
    Ok(backup_dir.join(name))
}

async fn read_ready_snapshot(
    marker: &std::path::Path,
    backup_dir: &std::path::Path,
) -> Result<PathBuf> {
    let snapshot = read_pending_snapshot(marker, backup_dir).await?;
    if !pending_snapshot_exists(&snapshot).await? {
        anyhow::bail!(
            "pending pre-migration snapshot {} is missing; refusing database changes",
            snapshot.display()
        );
    }
    sync_snapshot(&snapshot).await?;
    sync_backup_directory(backup_dir).await?;
    Ok(snapshot)
}

async fn pending_snapshot_exists(snapshot: &std::path::Path) -> Result<bool> {
    match tokio::fs::metadata(snapshot).await {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("checking snapshot {}", snapshot.display()))
        }
    }
}

async fn create_pending_marker(marker: &std::path::Path, snapshot: &std::path::Path) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let backup_dir = marker.parent().context("pending marker has no parent")?;
    let name = snapshot
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid pre-migration snapshot filename")?;
    if tokio::fs::try_exists(marker).await? {
        ensure_pending_marker_matches(marker, backup_dir, name).await?;
        sync_backup_directory(backup_dir).await?;
        return Ok(());
    }

    let marker_name = marker
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid pending marker filename")?;
    let temporary = marker.with_file_name(format!(
        ".{marker_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .with_context(|| format!("creating temporary marker {}", temporary.display()))?;
    let publication = async {
        file.write_all(format!("{name}\n").as_bytes()).await?;
        file.sync_all().await?;
        drop(file);

        if atomic_rename_noreplace(&temporary, marker).await? {
            sync_backup_directory(backup_dir).await?;
        } else {
            tokio::fs::remove_file(&temporary).await?;
            ensure_pending_marker_matches(marker, backup_dir, name).await?;
            sync_backup_directory(backup_dir).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if publication.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    publication.with_context(|| format!("publishing marker {}", marker.display()))
}

async fn ensure_pending_marker_matches(
    marker: &std::path::Path,
    backup_dir: &std::path::Path,
    expected_name: &str,
) -> Result<()> {
    let existing = read_pending_snapshot(marker, backup_dir).await?;
    if existing.file_name().and_then(|name| name.to_str()) == Some(expected_name) {
        Ok(())
    } else {
        anyhow::bail!("pending pre-migration marker already refers to another snapshot")
    }
}

async fn atomic_rename_noreplace(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;

        let source_path = source.to_owned();
        let destination_path = destination.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            let source = std::ffi::CString::new(source_path.as_os_str().as_bytes())
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
            let destination = std::ffi::CString::new(destination_path.as_os_str().as_bytes())
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
            // SAFETY: both paths are live NUL-terminated C strings and the call does not retain pointers.
            let result = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    source.as_ptr(),
                    libc::AT_FDCWD,
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result == 0 {
                Ok(true)
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        })
        .await
        .context("joining marker publication task")?;
        publish_after_renameat2(result, source, destination).await
    }

    #[cfg(not(target_os = "linux"))]
    atomic_hard_link_noreplace(source, destination).await
}

#[cfg(target_os = "linux")]
async fn publish_after_renameat2(
    result: std::io::Result<bool>,
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<bool> {
    match result {
        Ok(published) => Ok(published),
        Err(error) if renameat2_unsupported(&error) => {
            atomic_hard_link_noreplace(source, destination).await
        }
        Err(error) => Err(error).context("atomically publishing pending marker"),
    }
}

#[cfg(target_os = "linux")]
fn renameat2_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
}

async fn atomic_hard_link_noreplace(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<bool> {
    match tokio::fs::hard_link(source, destination).await {
        Ok(()) => {
            tokio::fs::remove_file(source)
                .await
                .context("removing temporary marker after hard-link publication")?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).context("atomically publishing pending marker via hard link"),
    }
}

async fn sync_snapshot(snapshot: &std::path::Path) -> Result<()> {
    tokio::fs::File::open(snapshot)
        .await
        .with_context(|| {
            format!(
                "opening pre-migration snapshot {} for sync",
                snapshot.display()
            )
        })?
        .sync_all()
        .await
        .with_context(|| format!("syncing pre-migration snapshot {}", snapshot.display()))
}

async fn sync_backup_directory(directory: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        let directory_path = directory.to_owned();
        let display_path = directory.display().to_string();
        tokio::task::spawn_blocking(move || std::fs::File::open(&directory_path)?.sync_all())
            .await
            .context("joining backup directory sync task")?
            .with_context(|| format!("syncing backup directory {display_path}"))?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

async fn database_has_schema(pool: &Pool) -> Result<bool> {
    let schema_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await
    .context("checking database schema before migration backup")?;
    Ok(schema_tables > 0)
}

fn new_pre_migration_backup_path(backup_dir: &std::path::Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    backup_dir.join(format!(
        "kinetix-pre-migration-{stamp}-{}.db",
        uuid::Uuid::new_v4().simple()
    ))
}

fn pre_migration_snapshot_temporary_path(snapshot: &std::path::Path) -> Result<PathBuf> {
    let name = snapshot
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid pre-migration snapshot filename")?;
    Ok(snapshot.with_file_name(format!(".{name}.tmp")))
}

async fn write_pre_migration_snapshot(pool: &Pool, snapshot: &std::path::Path) -> Result<()> {
    let temporary = pre_migration_snapshot_temporary_path(snapshot)?;
    if tokio::fs::try_exists(&temporary).await? {
        tokio::fs::remove_file(&temporary)
            .await
            .with_context(|| format!("removing incomplete snapshot {}", temporary.display()))?;
    }
    let sql = vacuum_into_sql(&temporary);
    if let Err(error) = sqlx::query(&sql).execute(pool).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error)
            .with_context(|| format!("writing pre-migration backup {}", snapshot.display()));
    }
    sync_snapshot(&temporary).await?;
    tokio::fs::rename(&temporary, snapshot)
        .await
        .with_context(|| format!("publishing pre-migration backup {}", snapshot.display()))?;
    let backup_dir = snapshot.parent().context("snapshot path has no parent")?;
    sync_backup_directory(backup_dir).await?;
    tracing::info!(backup = %snapshot.display(), "wrote pre-migration backup");
    Ok(())
}

async fn finish_pending_pre_migration_backup(pending: PendingPreMigrationBackup) {
    if let Err(error) =
        retain_pre_migration_backups(&pending.markers.backup_dir, &pending.snapshot).await
    {
        tracing::warn!(error = %error, "could not rotate pre-migration backups");
        return;
    }
    let mut changed = false;
    for marker in [&pending.markers.preparing, &pending.markers.ready] {
        match tokio::fs::remove_file(marker).await {
            Ok(()) => changed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(error = %error, "could not clear pending pre-migration marker");
                return;
            }
        }
    }
    if changed {
        if let Err(error) = sync_backup_directory(&pending.markers.backup_dir).await {
            tracing::warn!(error = %error, "could not persist pending marker cleanup");
        }
    }
}

async fn retain_pre_migration_backups(
    backup_dir: &std::path::Path,
    protected: &std::path::Path,
) -> Result<()> {
    let pending = pending_pre_migration_snapshots(backup_dir).await?;
    let mut entries = tokio::fs::read_dir(backup_dir)
        .await
        .context("reading pre-migration backup directory")?;
    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("reading pre-migration backup entries")?
    {
        let path = entry.path();
        if path != protected
            && !pending.contains(&path)
            && entry
                .file_type()
                .await
                .context("reading pre-migration backup entry type")?
                .is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_pre_migration_backup_filename)
        {
            files.push(path);
        }
    }
    files.push(protected.to_path_buf());
    files.sort();
    while files.len() > PRE_MIGRATION_BACKUP_RETAIN {
        let Some(index) = files.iter().position(|path| path != protected) else {
            break;
        };
        let old = files.remove(index);
        tokio::fs::remove_file(&old)
            .await
            .with_context(|| format!("removing old pre-migration backup {}", old.display()))?;
    }
    Ok(())
}

async fn pending_pre_migration_snapshots(backup_dir: &std::path::Path) -> Result<HashSet<PathBuf>> {
    let mut entries = tokio::fs::read_dir(backup_dir)
        .await
        .context("reading pending pre-migration markers")?;
    let mut snapshots = HashSet::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("reading pending pre-migration marker entries")?
    {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_pending_pre_migration_marker_filename)
        {
            continue;
        }
        if !entry
            .file_type()
            .await
            .context("reading pending marker entry type")?
            .is_file()
        {
            anyhow::bail!(
                "pending pre-migration marker {} is not a regular file",
                path.display()
            );
        }
        let snapshot = read_pending_snapshot(&path, backup_dir).await?;
        if !pending_snapshot_exists(&snapshot).await? {
            anyhow::bail!(
                "pending pre-migration snapshot {} is missing; refusing backup rotation",
                snapshot.display()
            );
        }
        snapshots.insert(snapshot);
    }
    Ok(snapshots)
}

fn is_pending_pre_migration_marker_filename(name: &str) -> bool {
    let Some(marker) = name.strip_prefix(".kinetix-pre-migration-") else {
        return false;
    };
    let Some((id, stage)) = marker.rsplit_once('.') else {
        return false;
    };
    id.len() == 64
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && matches!(stage, "preparing" | "ready")
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
    name.strip_prefix("kinetix-")
        .and_then(|name| name.strip_suffix(".db"))
        .is_some_and(is_backup_timestamp)
}

fn is_pre_migration_backup_filename(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("kinetix-pre-migration-")
        .and_then(|name| name.strip_suffix(".db"))
    else {
        return false;
    };
    if is_backup_timestamp(stem) {
        return true; // accept snapshots created before UUIDs were added
    }
    let Some((stamp, uuid)) = stem.split_once('-') else {
        return false;
    };
    is_backup_timestamp(stamp)
        && uuid.len() == 32
        && uuid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_backup_timestamp(stamp: &str) -> bool {
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
    if let Err(e) = tokio::fs::create_dir_all(&backup_dir).await {
        return Err(format!("cannot create backup dir: {e}"));
    }
    // NFR-2.4: document the restore path next to the backups so recovery does
    // not depend on tribal knowledge (overwritten on each run).
    let readme = "Kinetix database backups\n\
=======================\n\n\
These files are transactionally-consistent snapshots written by `VACUUM INTO`,\n\
including pre-migration snapshots taken before migrations or pricing repairs.\n\n\
To restore:\n\n\
  1. Stop Kinetix (systemctl stop kinetix).\n\
  2. Remove the live database and its WAL sidecars:\n\
       rm -f /var/lib/kinetix/kinetix.db /var/lib/kinetix/kinetix.db-wal /var/lib/kinetix/kinetix.db-shm\n\
  3. Copy the chosen snapshot into place:\n\
       cp <snapshot>.db /var/lib/kinetix/kinetix.db\n\
  4. Ensure ownership matches the service user (chown kinetix:kinetix).\n\
  5. Start Kinetix (systemctl start kinetix); migrations re-run automatically.\n\n\
Retention: the newest 14 scheduled and 3 completed pre-migration snapshots are\n\
kept. A snapshot for an unfinished upgrade is protected and reused on retries.\n\
Pending snapshots for databases sharing this directory are protected and do\n\
not count toward the three-snapshot limit until migrations and repairs succeed.\n";
    let _ = tokio::fs::write(backup_dir.join("RESTORE.txt"), readme).await;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let dst = backup_dir.join(format!("kinetix-{stamp}.db"));
    let sql = vacuum_into_sql(&dst);
    if let Err(e) = sqlx::query(&sql).execute(pool).await {
        tracing::warn!(error = %e, "scheduled backup failed");
        return Err(format!("VACUUM INTO failed: {e}"));
    }
    tracing::info!(backup = %dst.display(), "wrote scheduled backup");
    // Retain scheduled snapshots only; pre-migration restore points are separate.
    if let Ok(mut entries) = tokio::fs::read_dir(&backup_dir).await {
        let mut files: Vec<PathBuf> = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let path = entry.path();
                    if entry.file_type().await.is_ok_and(|kind| kind.is_file())
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(is_scheduled_backup_filename)
                    {
                        files.push(path);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(error = %error, "could not read scheduled backup directory");
                    break;
                }
            }
        }
        files.sort();
        while files.len() > retain.max(1) {
            let old = files.remove(0);
            if let Err(error) = tokio::fs::remove_file(&old).await {
                tracing::warn!(backup = %old.display(), error = %error, "could not prune scheduled backup");
            }
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
    pub max_concurrent_requests: Option<i64>,
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
          max_concurrent_requests, daily_budget, monthly_budget, expires_at, status, allowed_ips, body_logging, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
    .bind(k.max_concurrent_requests)
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
    #[serde(default)]
    pub integration_features: Option<String>,
    #[serde(default)]
    pub integration_protocols: Option<String>,
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
    /// Validated integration feature ceiling, if this provider was created
    /// from an integration that declares one.
    pub fn integration_feature_ceiling(
        &self,
    ) -> Result<Option<crate::plugins::types::IntegrationFeaturesV1>, String> {
        let Some(raw) = self.integration_features.as_deref() else {
            return Ok(None);
        };
        let features: crate::plugins::types::IntegrationFeaturesV1 =
            serde_json::from_str(raw).map_err(|error| error.to_string())?;
        features.validate()?;
        Ok(Some(features))
    }

    /// Validated input/upstream protocol declarations for an integration-backed
    /// provider. Missing declarations preserve legacy behavior.
    pub fn integration_protocol_ceiling(
        &self,
    ) -> Result<Option<crate::plugins::types::IntegrationProtocolsV1>, String> {
        let Some(raw) = self.integration_protocols.as_deref() else {
            return Ok(None);
        };
        let protocols: crate::plugins::types::IntegrationProtocolsV1 =
            serde_json::from_str(raw).map_err(|error| error.to_string())?;
        protocols.validate()?;
        Ok(Some(protocols))
    }

    pub fn allows_input_protocol(&self, protocol: &str) -> Result<bool, String> {
        let Some(ceiling) = self.integration_protocol_ceiling()? else {
            return Ok(true);
        };
        Ok(ceiling.allows_input(protocol))
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

pub async fn set_provider_integration_features(
    pool: &Pool,
    id: &str,
    features: Option<&crate::plugins::types::IntegrationFeaturesV1>,
) -> Result<()> {
    if let Some(features) = features {
        features.validate().map_err(anyhow::Error::msg)?;
    }
    let serialized = features.map(serde_json::to_string).transpose()?;
    sqlx::query("UPDATE providers SET integration_features=? WHERE id=?")
        .bind(serialized)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_provider_integration_protocols(
    pool: &Pool,
    id: &str,
    protocols: Option<&crate::plugins::types::IntegrationProtocolsV1>,
) -> Result<()> {
    if let Some(protocols) = protocols {
        protocols.validate().map_err(anyhow::Error::msg)?;
    }
    let serialized = protocols.map(serde_json::to_string).transpose()?;
    sqlx::query("UPDATE providers SET integration_protocols=? WHERE id=?")
        .bind(serialized)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn set_provider_integration_features_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    features: Option<&crate::plugins::types::IntegrationFeaturesV1>,
) -> Result<()> {
    if let Some(features) = features {
        features.validate().map_err(anyhow::Error::msg)?;
    }
    let serialized = features.map(serde_json::to_string).transpose()?;
    sqlx::query("UPDATE providers SET integration_features=? WHERE id=?")
        .bind(serialized)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn set_provider_integration_protocols_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    protocols: Option<&crate::plugins::types::IntegrationProtocolsV1>,
) -> Result<()> {
    if let Some(protocols) = protocols {
        protocols.validate().map_err(anyhow::Error::msg)?;
    }
    let serialized = protocols.map(serde_json::to_string).transpose()?;
    sqlx::query("UPDATE providers SET integration_protocols=? WHERE id=?")
        .bind(serialized)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
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

pub(crate) async fn insert_provider_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    p: &NewProvider<'_>,
    pricing_scope: &str,
) -> Result<String> {
    if !matches!(pricing_scope, "direct_api" | "integration") {
        anyhow::bail!("invalid provider pricing scope '{pricing_scope}'");
    }
    let id = format!("prov_{}", uuid::Uuid::new_v4().simple());
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
    .execute(&mut **tx)
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

pub(crate) async fn update_provider_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    p: &NewProvider<'_>,
    pricing_scope: &str,
) -> Result<()> {
    let existing = sqlx::query_as::<_, ProviderRow>("SELECT * FROM providers WHERE id=?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider '{id}' not found"))?;
    let drivers_changed = existing.credential_mode != p.credential_mode
        || existing.source_plugin_id.as_deref() != p.source_plugin_id
        || existing.source_integration_id.as_deref() != p.source_integration_id
        || existing.wire_plugin != p.wire_plugin
        || existing.credential_plugin != p.credential_plugin
        || existing.model_source_plugin != p.model_source_plugin;
    let catalog_identity_changed = existing.base_url != p.base_url || drivers_changed;
    if !matches!(pricing_scope, "direct_api" | "integration") {
        anyhow::bail!("invalid provider pricing scope '{pricing_scope}'");
    }

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
    .bind(pricing_scope)
    .bind(id)
    .execute(&mut **tx)
    .await?;

    if pricing_scope == "integration" || catalog_identity_changed {
        revoke_external_catalog_effective_pricing_in_transaction(tx, id).await?;
    }
    Ok(())
}

pub async fn update_provider_integration_bindings(
    pool: &Pool,
    id: &str,
    wire_format: WireFormat,
    wire_plugin: &str,
    credential_plugin: &str,
    model_source_plugin: &str,
) -> Result<()> {
    let existing = get_provider(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider '{id}' not found"))?;
    let bindings_changed = existing.wire_format != wire_format.as_str()
        || existing.wire_plugin != wire_plugin
        || existing.credential_plugin != credential_plugin
        || existing.model_source_plugin != model_source_plugin;
    if !bindings_changed {
        return Ok(());
    }

    let _guards = provider_price_guards(pool, id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE providers
         SET wire_format=?, wire_plugin=?, credential_plugin=?, model_source_plugin=?
         WHERE id=?",
    )
    .bind(wire_format.as_str())
    .bind(wire_plugin)
    .bind(credential_plugin)
    .bind(model_source_plugin)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    revoke_external_catalog_effective_pricing_in_transaction(&mut tx, id).await?;
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

fn effective_pricing_needs_scope_repair(
    effective: Option<&serde_json::Map<String, Value>>,
) -> bool {
    let previous_source = effective
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
        .unwrap_or("untracked");
    let fields = effective
        .and_then(|value| value.get("fields"))
        .and_then(Value::as_object);
    let has_external_catalog_field = fields.is_some_and(|fields| {
        fields.values().any(|field| {
            field
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(crate::model_catalog::is_external_catalog_price_source)
        })
    });
    let legacy_external_catalog_snapshot = !has_external_catalog_field
        && crate::model_catalog::is_external_catalog_price_source(previous_source);
    has_external_catalog_field || legacy_external_catalog_snapshot
}

async fn providers_needing_pricing_scope_repair(pool: &Pool) -> Result<Vec<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT providers.id, models.discovery
         FROM providers JOIN models ON models.provider_id = providers.id
         WHERE providers.pricing_scope = 'integration'
         ORDER BY providers.id, models.id",
    )
    .fetch_all(pool)
    .await?;
    let mut provider_ids = std::collections::BTreeSet::new();
    for (provider_id, discovery) in rows {
        let discovery: Value =
            serde_json::from_str(&discovery).unwrap_or_else(|_| serde_json::json!({}));
        let effective = discovery
            .get("effective_pricing")
            .and_then(Value::as_object);
        if effective_pricing_needs_scope_repair(effective) {
            provider_ids.insert(provider_id);
        }
    }
    Ok(provider_ids.into_iter().collect())
}

async fn provider_pricing_scope_repairs_pending(pool: &Pool) -> Result<bool> {
    Ok(!providers_needing_pricing_scope_repair(pool)
        .await?
        .is_empty())
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
        if !effective_pricing_needs_scope_repair(effective) {
            continue;
        }

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
    for provider_id in providers_needing_pricing_scope_repair(pool).await? {
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
    pub status_reason: String,
    pub status_changed_at: Option<String>,
    /// Optimistic generation for lifecycle and account configuration updates.
    #[serde(skip)]
    pub account_state_version: i64,
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

/// List all accounts for an admin view, including disabled accounts.
pub async fn list_accounts_for_provider(pool: &Pool, provider_id: &str) -> Result<Vec<AccountRow>> {
    Ok(sqlx::query_as::<_, AccountRow>(
        "SELECT * FROM accounts WHERE provider_id = ? ORDER BY priority, created_at",
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
         (id, provider_id, label, secret_enc, key_mask, status, status_reason, status_changed_at, quota_type, soft_quota_usd, priority, weight, created_at)
         VALUES (?,?,?,?,?,'healthy','account_created',?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(provider_id)
    .bind(label)
    .bind(secret_enc)
    .bind(key_mask)
    .bind(now_iso())
    .bind(quota_type)
    .bind(soft_quota_usd)
    .bind(priority)
    .bind(weight)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

pub(crate) async fn insert_account_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    provider_id: &str,
    label: &str,
    secret_enc: &str,
    key_mask: &str,
    priority: i64,
    weight: i64,
    soft_quota_usd: Option<f64>,
    quota_type: &str,
    quota_window_s: Option<i64>,
    enabled: bool,
) -> Result<String> {
    let id = format!("acc_{}", uuid::Uuid::new_v4().simple());
    let status = if enabled { "healthy" } else { "disabled" };
    let status_reason = if enabled {
        "account_created"
    } else {
        "operator_disabled"
    };
    let now = now_iso();
    sqlx::query(
        "INSERT INTO accounts
         (id, provider_id, label, secret_enc, key_mask, status, status_reason, status_changed_at,
          quota_type, quota_window_s, soft_quota_usd, priority, weight, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(provider_id)
    .bind(label)
    .bind(secret_enc)
    .bind(key_mask)
    .bind(status)
    .bind(status_reason)
    .bind(&now)
    .bind(quota_type)
    .bind(quota_window_s)
    .bind(soft_quota_usd)
    .bind(priority)
    .bind(weight)
    .bind(&now)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// Apply imported operator-owned account policy without replacing credentials.
/// Runtime health remains intact unless policy changes the account's enabled state.
pub(crate) async fn update_account_policy_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    enabled: bool,
    priority: i64,
    weight: i64,
    soft_quota_usd: Option<f64>,
    quota_type: &str,
    quota_window_s: Option<i64>,
) -> Result<()> {
    let (current_status, current_reason) = sqlx::query_as::<_, (String, String)>(
        "SELECT status, status_reason FROM accounts WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| anyhow::anyhow!("account {id} disappeared during config import"))?;

    let (status, status_reason) = if !enabled {
        ("disabled", "operator_disabled")
    } else if current_status == "disabled"
        && matches!(
            current_reason.as_str(),
            "operator_disabled" | "existing_disabled" | "unknown"
        )
    {
        ("healthy", "operator_enabled")
    } else {
        (current_status.as_str(), current_reason.as_str())
    };
    let reset_runtime_state =
        !enabled || status != current_status || status_reason != current_reason;
    let now = now_iso();
    sqlx::query(
        "UPDATE accounts SET status=?, status_reason=?, \
         status_changed_at=CASE WHEN status=? AND status_reason=? THEN status_changed_at ELSE ? END, \
         cooldown_until=CASE WHEN ? THEN NULL ELSE cooldown_until END, \
         quota_reset_at=CASE WHEN ? THEN NULL ELSE quota_reset_at END, \
         last_error=CASE WHEN ? THEN NULL ELSE last_error END, \
         circuit_open_until=CASE WHEN ? THEN NULL ELSE circuit_open_until END, \
         consecutive_failures=CASE WHEN ? THEN 0 ELSE consecutive_failures END, \
         quota_type=?, quota_window_s=?, soft_quota_usd=?, priority=?, weight=?, \
         account_state_version=account_state_version + 1 WHERE id=?",
    )
    .bind(status)
    .bind(status_reason)
    .bind(status)
    .bind(status_reason)
    .bind(now)
    .bind(reset_runtime_state)
    .bind(reset_runtime_state)
    .bind(reset_runtime_state)
    .bind(reset_runtime_state)
    .bind(reset_runtime_state)
    .bind(quota_type)
    .bind(quota_window_s)
    .bind(soft_quota_usd)
    .bind(priority)
    .bind(weight)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn update_account(
    pool: &Pool,
    id: &str,
    label: &str,
    status: Option<&str>,
    priority: i64,
    weight: i64,
    soft_quota_usd: Option<f64>,
    quota_type: &str,
    credential: Option<(&str, &str)>,
) -> Result<()> {
    if let Some(status) = status {
        if !matches!(status, "healthy" | "disabled") {
            anyhow::bail!("account status must be either healthy or disabled");
        }
        let reason = if status == "disabled" {
            "operator_disabled"
        } else {
            "operator_enabled"
        };
        sqlx::query(
            "UPDATE accounts SET label=?, status=?, status_reason=?, \
             status_changed_at=CASE WHEN status=? AND status_reason=? THEN status_changed_at ELSE ? END, \
             priority=?, weight=?, soft_quota_usd=?, quota_type=?, cooldown_until=NULL, \
             quota_reset_at=NULL, last_error=NULL, circuit_open_until=NULL, \
             consecutive_failures=0, secret_enc=COALESCE(?, secret_enc), \
             key_mask=COALESCE(?, key_mask), account_state_version=account_state_version + 1 WHERE id=?",
        )
        .bind(label)
        .bind(status)
        .bind(reason)
        .bind(status)
        .bind(reason)
        .bind(now_iso())
        .bind(priority)
        .bind(weight)
        .bind(soft_quota_usd)
        .bind(quota_type)
        .bind(credential.map(|(secret_enc, _)| secret_enc))
        .bind(credential.map(|(_, key_mask)| key_mask))
        .bind(id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE accounts SET label=?, priority=?, weight=?, soft_quota_usd=?, quota_type=?, \
             secret_enc=COALESCE(?, secret_enc), key_mask=COALESCE(?, key_mask), \
             account_state_version=account_state_version + 1 WHERE id=?",
        )
        .bind(label)
        .bind(priority)
        .bind(weight)
        .bind(soft_quota_usd)
        .bind(quota_type)
        .bind(credential.map(|(secret_enc, _)| secret_enc))
        .bind(credential.map(|(_, key_mask)| key_mask))
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn set_account_status(
    pool: &Pool,
    id: &str,
    status: &str,
    reason_code: &str,
    cooldown_until: Option<&str>,
    quota_reset_at: Option<&str>,
    last_error: Option<&str>,
) -> Result<()> {
    // Runtime and probe updates cannot re-enable an operator-disabled account.
    let clears_circuit = status == "healthy";
    sqlx::query(
        "UPDATE accounts SET status=?, status_reason=?, \
         status_changed_at=CASE WHEN status=? AND status_reason=? THEN status_changed_at ELSE ? END, \
         cooldown_until=?, quota_reset_at=?, last_error=?, \
         circuit_open_until=CASE WHEN ? THEN NULL ELSE circuit_open_until END, \
         consecutive_failures=CASE WHEN ? THEN 0 ELSE consecutive_failures END, \
         account_state_version=account_state_version + 1 \
         WHERE id=? AND status != 'disabled'",
    )
    .bind(status)
    .bind(reason_code)
    .bind(status)
    .bind(reason_code)
    .bind(now_iso())
    .bind(cooldown_until)
    .bind(quota_reset_at)
    .bind(last_error)
    .bind(clears_circuit)
    .bind(clears_circuit)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Apply a runtime lifecycle update only if the request still observes the
/// account generation from which its credential was resolved.
pub async fn set_account_status_if_version(
    pool: &Pool,
    id: &str,
    observed_state_version: i64,
    status: &str,
    reason_code: &str,
    cooldown_until: Option<&str>,
    quota_reset_at: Option<&str>,
    last_error: Option<&str>,
) -> Result<bool> {
    let clears_circuit = status == "healthy";
    let result = sqlx::query(
        "UPDATE accounts SET status=?, status_reason=?, \
         status_changed_at=CASE WHEN status=? AND status_reason=? THEN status_changed_at ELSE ? END, \
         cooldown_until=?, quota_reset_at=?, last_error=?, \
         circuit_open_until=CASE WHEN ? THEN NULL ELSE circuit_open_until END, \
         consecutive_failures=CASE WHEN ? THEN 0 ELSE consecutive_failures END, \
         account_state_version=account_state_version + 1 \
         WHERE id=? AND status != 'disabled' AND account_state_version=?",
    )
    .bind(status)
    .bind(reason_code)
    .bind(status)
    .bind(reason_code)
    .bind(now_iso())
    .bind(cooldown_until)
    .bind(quota_reset_at)
    .bind(last_error)
    .bind(clears_circuit)
    .bind(clears_circuit)
    .bind(id)
    .bind(observed_state_version)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Clear circuit-breaker state after a valid upstream response. The observed
/// account-state version makes an older in-flight success a no-op after any
/// newer lifecycle or failure update. An open circuit can only be cleared by a
/// request selected as its bounded half-open probe.
pub async fn recover_account_after_success(
    pool: &Pool,
    id: &str,
    observed_state_version: i64,
    is_half_open_probe: bool,
) -> Result<bool> {
    let now = now_iso();
    let result = sqlx::query(
        "UPDATE accounts SET \
         status=CASE \
             WHEN status='cooldown' AND julianday(cooldown_until) <= julianday(?) THEN 'healthy' \
             WHEN status='exhausted' AND julianday(quota_reset_at) <= julianday(?) THEN 'healthy' \
             ELSE status END, \
         status_reason=CASE \
             WHEN status='cooldown' AND julianday(cooldown_until) <= julianday(?) THEN 'cooldown_elapsed' \
             WHEN status='exhausted' AND julianday(quota_reset_at) <= julianday(?) THEN 'quota_reset' \
             WHEN status='healthy' AND circuit_open_until IS NOT NULL AND status_reason='circuit_open' THEN 'circuit_recovered' \
             ELSE status_reason END, \
         status_changed_at=CASE \
             WHEN (status='cooldown' AND julianday(cooldown_until) <= julianday(?)) \
               OR (status='exhausted' AND julianday(quota_reset_at) <= julianday(?)) \
               OR (status='healthy' AND circuit_open_until IS NOT NULL AND status_reason='circuit_open') \
             THEN ? ELSE status_changed_at END, \
         cooldown_until=CASE WHEN status='cooldown' AND julianday(cooldown_until) <= julianday(?) THEN NULL ELSE cooldown_until END, \
         quota_reset_at=CASE WHEN status='exhausted' AND julianday(quota_reset_at) <= julianday(?) THEN NULL ELSE quota_reset_at END, \
         last_error=CASE \
             WHEN status='healthy' \
               OR (status='cooldown' AND julianday(cooldown_until) <= julianday(?)) \
               OR (status='exhausted' AND julianday(quota_reset_at) <= julianday(?)) \
             THEN NULL ELSE last_error END, \
         circuit_open_until=NULL, consecutive_failures=0, \
         account_state_version=account_state_version + 1 \
         WHERE id=? AND status != 'disabled' AND account_state_version=? \
         AND ((circuit_open_until IS NULL AND (consecutive_failures != 0 \
                  OR (status='cooldown' AND julianday(cooldown_until) <= julianday(?)) \
                  OR (status='exhausted' AND julianday(quota_reset_at) <= julianday(?)))) \
              OR (? AND circuit_open_until IS NOT NULL))",
    )
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .bind(observed_state_version)
    .bind(&now)
    .bind(&now)
    .bind(is_half_open_probe)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_account(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Apply an account-scoped upstream failure and its lifecycle transition in
/// one compare-and-update. Returning `None` means the attempt's account
/// generation is stale or the account is already disabled.
pub async fn apply_account_failure(
    pool: &Pool,
    id: &str,
    observed_state_version: i64,
    status: &str,
    reason_code: &str,
    cooldown_until: Option<&str>,
    quota_reset_at: Option<&str>,
    last_error: &str,
    circuit_threshold: i64,
    open_secs: i64,
) -> Result<Option<i64>> {
    if !matches!(status, "cooldown" | "exhausted" | "disabled") {
        anyhow::bail!("invalid account failure status: {status}");
    }
    let counts_toward_circuit = status != "disabled";
    let open_until = (Utc::now() + chrono::Duration::seconds(open_secs)).to_rfc3339();
    let redacted_error = crate::crypto::redact(last_error);
    let failure_count = sqlx::query_scalar(
        "UPDATE accounts SET status=?, status_reason=?, \
         status_changed_at=CASE WHEN status=? AND status_reason=? THEN status_changed_at ELSE ? END, \
         cooldown_until=?, quota_reset_at=?, last_error=?, \
         consecutive_failures=consecutive_failures + CASE WHEN ? THEN 1 ELSE 0 END, \
         circuit_open_until=CASE WHEN ? AND consecutive_failures + 1 >= ? THEN ? ELSE circuit_open_until END, \
         account_state_version=account_state_version + 1 \
         WHERE id=? AND status != 'disabled' AND account_state_version=? \
         RETURNING consecutive_failures",
    )
    .bind(status)
    .bind(reason_code)
    .bind(status)
    .bind(reason_code)
    .bind(now_iso())
    .bind(cooldown_until)
    .bind(quota_reset_at)
    .bind(redacted_error)
    .bind(counts_toward_circuit)
    .bind(counts_toward_circuit)
    .bind(circuit_threshold)
    .bind(open_until)
    .bind(id)
    .bind(observed_state_version)
    .fetch_optional(pool)
    .await?;
    Ok(failure_count)
}

/// Bump the consecutive-failure counter and open the circuit when the
/// threshold is reached (FR-4.7). Returns the new failure count.
pub async fn record_account_failure(
    pool: &Pool,
    id: &str,
    circuit_threshold: i64,
    open_secs: i64,
) -> Result<i64> {
    sqlx::query(
        "UPDATE accounts SET consecutive_failures = consecutive_failures + 1, \
         account_state_version = account_state_version + 1 \
         WHERE id = ? AND status != 'disabled'",
    )
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
        sqlx::query(
            "UPDATE accounts SET circuit_open_until = ?, \
             status_reason = CASE WHEN status = 'healthy' THEN 'circuit_open' ELSE status_reason END, \
             status_changed_at = CASE WHEN status = 'healthy' AND circuit_open_until IS NULL THEN ? ELSE status_changed_at END \
             WHERE id = ? AND status != 'disabled'",
        )
            .bind(until)
            .bind(now_iso())
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(n)
}

/// Clear the circuit breaker and failure counter after a successful probe.
pub async fn reset_account_failures(pool: &Pool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE accounts SET consecutive_failures = 0, circuit_open_until = NULL, \
         account_state_version = account_state_version + 1 WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Atomically reserve an eligible half-open account probe. The immutable
/// registry version prevents a stale candidate from claiming after lifecycle
/// state changes; the conditional timestamp update permits one winner per gap.
pub async fn claim_half_open_probe_at(
    pool: &Pool,
    id: &str,
    observed_state_version: i64,
    now: DateTime<Utc>,
    min_gap_secs: i64,
) -> Result<bool> {
    let probe_after = (now - chrono::Duration::seconds(min_gap_secs.max(0))).to_rfc3339();
    let now = now.to_rfc3339();
    let result = sqlx::query(
        "UPDATE accounts SET last_probe_at=? \
         WHERE id=? AND status != 'disabled' AND account_state_version=? \
           AND circuit_open_until IS NOT NULL \
           AND julianday(circuit_open_until) <= julianday(?) \
           AND (status != 'cooldown' OR (cooldown_until IS NOT NULL AND julianday(cooldown_until) <= julianday(?))) \
           AND (status != 'exhausted' OR (quota_reset_at IS NOT NULL AND julianday(quota_reset_at) <= julianday(?))) \
           AND (last_probe_at IS NULL OR julianday(last_probe_at) <= julianday(?))",
    )
    .bind(&now)
    .bind(id)
    .bind(observed_state_version)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&probe_after)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Record the timestamp of an explicit administrator-initiated account test.
/// Automatic half-open dispatch must use [`claim_half_open_probe_at`].
pub async fn record_manual_probe_at(pool: &Pool, id: &str) -> Result<()> {
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

#[derive(Debug, Clone, FromRow)]
pub struct ModelObservationRow {
    pub id: String,
    pub model_id: String,
    pub kind: String,
    pub source: String,
    pub observed_at: String,
    pub scope_json: String,
    pub value_json: String,
}

/// A source observation to append alongside a current discovery/evidence update.
pub struct ModelObservationDraft<'a> {
    pub kind: &'a str,
    pub source: &'a str,
    pub observed_at: &'a str,
    pub scope: &'a Value,
    pub value: &'a Value,
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

async fn insert_model_observation_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    model_id: &str,
    observation: &ModelObservationDraft<'_>,
) -> Result<()> {
    let id = format!("observation_{}", uuid::Uuid::new_v4().simple());
    let observed_at = DateTime::parse_from_rfc3339(observation.observed_at)
        .context("model observation timestamp must be RFC3339")?
        .with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query(
        "INSERT INTO model_observations
         (id, model_id, kind, source, observed_at, scope_json, value_json)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(model_id)
    .bind(observation.kind)
    .bind(observation.source)
    .bind(observed_at)
    .bind(observation.scope.to_string())
    .bind(observation.value.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn list_model_observations(
    pool: &Pool,
    model_id: &str,
    limit: i64,
    cursor: Option<(&str, &str)>,
) -> Result<Vec<ModelObservationRow>> {
    let rows = if let Some((observed_at, id)) = cursor {
        sqlx::query_as::<_, ModelObservationRow>(
            "SELECT id, model_id, kind, source, observed_at, scope_json, value_json
             FROM model_observations
             WHERE model_id = ? AND (observed_at, id) < (?, ?)
             ORDER BY observed_at DESC, id DESC
             LIMIT ?",
        )
        .bind(model_id)
        .bind(observed_at)
        .bind(id)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, ModelObservationRow>(
            "SELECT id, model_id, kind, source, observed_at, scope_json, value_json
             FROM model_observations
             WHERE model_id = ?
             ORDER BY observed_at DESC, id DESC
             LIMIT ?",
        )
        .bind(model_id)
        .bind(limit)
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
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
    pub initial_observation: Option<ModelObservationDraft<'a>>,
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

/// Update the latest discovery/evidence view and append its immutable source
/// observation atomically.
pub async fn merge_model_discovery_with_observation(
    pool: &Pool,
    id: &str,
    fresh: &Value,
    observation: &ModelObservationDraft<'_>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    merge_model_discovery_in_transaction(&mut tx, id, fresh).await?;
    insert_model_observation_in_transaction(&mut tx, id, observation).await?;
    tx.commit().await?;
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
    if let Some(observation) = creation.initial_observation.as_ref() {
        insert_model_observation_in_transaction(&mut tx, &id, observation).await?;
    }
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

pub(crate) async fn commit_model_creation_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    creation: &ModelCreation<'_>,
) -> Result<(String, Option<String>)> {
    let id = insert_model_in_transaction(tx, &creation.model).await?;
    set_model_transport_override_in_transaction(tx, &id, creation.transport).await?;
    merge_model_discovery_in_transaction(tx, &id, creation.discovery_patch).await?;
    if let Some(plugin_id) = creation.opaque_state_plugin {
        set_model_opaque_state_plugin_in_transaction(tx, &id, plugin_id).await?;
    }
    let version_id = if let Some(pricing) = creation.pricing.as_ref() {
        apply_effective_model_pricing_transaction(
            tx,
            &id,
            pricing.prices,
            pricing.source,
            pricing.metadata,
        )
        .await?
    } else {
        None
    };
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

pub(crate) async fn commit_model_operator_mutation_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    mutation: &ModelOperatorMutation<'_>,
) -> Result<Option<String>> {
    update_model_configuration_in_transaction(tx, mutation).await?;
    if mutation.update_transport {
        set_model_transport_override_in_transaction(tx, mutation.id, mutation.transport).await?;
    }
    merge_model_discovery_in_transaction(tx, mutation.id, mutation.discovery_patch).await?;
    if let Some(pricing) = mutation.pricing.as_ref() {
        apply_effective_model_pricing_transaction(
            tx,
            mutation.id,
            pricing.prices,
            pricing.source,
            pricing.metadata,
        )
        .await
    } else {
        Ok(None)
    }
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

pub(crate) async fn lock_model_price_versions(
    model_ids: &[String],
) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut ids = model_ids.to_vec();
    ids.sort();
    ids.dedup();
    let mut guards = Vec::with_capacity(ids.len());
    for id in ids {
        guards.push(price_version_lock(&id).lock_owned().await);
    }
    guards
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
    pub max_concurrent_requests: Option<i64>,
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

pub(crate) struct ConfigExportSnapshot {
    pub providers: Vec<ProviderRow>,
    pub accounts: Vec<AccountRow>,
    pub models: Vec<ModelRow>,
    pub routes: Vec<RouteRow>,
    pub aliases: Vec<AliasRow>,
    pub route_targets: HashMap<String, Vec<RouteTargetRow>>,
}

pub(crate) async fn config_export_snapshot_with_hook<F, Fut>(
    pool: &Pool,
    after_resources_read: F,
) -> Result<ConfigExportSnapshot>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut tx = pool.begin().await?;
    // The first SELECT establishes SQLite's WAL read snapshot for every export row.
    let providers = sqlx::query_as::<_, ProviderRow>("SELECT * FROM providers ORDER BY created_at")
        .fetch_all(&mut *tx)
        .await?;
    let mut accounts = Vec::new();
    let mut models = Vec::new();
    for provider in &providers {
        accounts.extend(
            sqlx::query_as::<_, AccountRow>(
                "SELECT * FROM accounts WHERE provider_id = ? ORDER BY priority, created_at",
            )
            .bind(&provider.id)
            .fetch_all(&mut *tx)
            .await?,
        );
        models.extend(
            sqlx::query_as::<_, ModelRow>("SELECT * FROM models WHERE provider_id = ?")
                .bind(&provider.id)
                .fetch_all(&mut *tx)
                .await?,
        );
    }

    after_resources_read().await;

    let routes = sqlx::query_as::<_, RouteRow>("SELECT * FROM routes ORDER BY created_at")
        .fetch_all(&mut *tx)
        .await?;
    let aliases = sqlx::query_as::<_, AliasRow>("SELECT * FROM aliases ORDER BY alias")
        .fetch_all(&mut *tx)
        .await?;
    let mut route_targets = HashMap::new();
    for route in &routes {
        let targets = sqlx::query_as::<_, RouteTargetRow>(
            "SELECT * FROM route_targets WHERE route_id = ? ORDER BY priority, weight DESC",
        )
        .bind(&route.id)
        .fetch_all(&mut *tx)
        .await?;
        route_targets.insert(route.id.clone(), targets);
    }
    tx.commit().await?;

    Ok(ConfigExportSnapshot {
        providers,
        accounts,
        models,
        routes,
        aliases,
        route_targets,
    })
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
    pub max_concurrent_requests: Option<i64>,
}

pub async fn insert_route(pool: &Pool, c: &NewRoute<'_>) -> Result<String> {
    let id = format!("route_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO routes (id, name, description, strategy, fallback_triggers, continuity_policy, portability_policy, sticky_routing, cache_affinity, max_attempts, max_concurrent_requests, enabled, created_at)
         VALUES (?,?,?,?,?,'strip',?,?,?,?,?,1,?)",
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
    .bind(c.max_concurrent_requests)
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
    max_concurrent_requests: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE routes SET description=?, strategy=?, fallback_triggers=?, continuity_policy='strip', portability_policy=?, sticky_routing=?, cache_affinity=?, max_attempts=?, max_concurrent_requests=? WHERE id=?",
    )
    .bind(description)
    .bind(strategy)
    .bind(fallback_triggers.to_string())
    .bind(portability_policy)
    .bind(sticky_routing as i64)
    .bind(cache_affinity as i64)
    .bind(max_attempts)
    .bind(max_concurrent_requests)
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
          commit_state, outcome, steps, warnings, stream_outcome, terminal_failure_kind, fallback_allowed)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
    .bind(&t.stream_outcome)
    .bind(&t.terminal_failure_kind)
    .bind(t.fallback_allowed.map(i64::from))
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
    pub stream_outcome: Option<String>,
    pub terminal_failure_kind: Option<String>,
    pub fallback_allowed: Option<i64>,
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

#[cfg(test)]
mod account_success_recovery_tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::oneshot;

    async fn recovery_account(tag: &str) -> (Pool, String, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-account-recovery-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database_url = format!("sqlite://{}", root.join("kinetix.db").display());
        let pool = connect(&database_url).await.unwrap();
        migrate(&pool).await.unwrap();
        let provider_id = insert_provider(
            &pool,
            &NewProvider {
                name: "recovery-test",
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
        let account_id = insert_account(
            &pool,
            &provider_id,
            "recovery-test",
            "encrypted",
            "key…",
            1,
            1,
            None,
            "",
        )
        .await
        .unwrap();
        (pool, account_id, root)
    }

    #[tokio::test]
    async fn account_failure_transition_is_atomic_and_generation_guarded() {
        let (pool, account_id, root) = recovery_account("failure-generation").await;
        let before = get_account(&pool, &account_id).await.unwrap().unwrap();
        let cooldown_until = (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();

        assert_eq!(
            apply_account_failure(
                &pool,
                &account_id,
                before.account_state_version,
                "cooldown",
                "rate_limited",
                Some(cooldown_until.as_str()),
                None,
                "rate limit",
                1,
                60,
            )
            .await
            .unwrap(),
            Some(1)
        );
        let after_failure = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert_eq!(after_failure.status, "cooldown");
        assert_eq!(after_failure.status_reason, "rate_limited");
        assert_eq!(
            after_failure.cooldown_until.as_deref(),
            Some(cooldown_until.as_str())
        );
        assert_eq!(after_failure.consecutive_failures, 1);
        assert!(after_failure.circuit_open_until.is_some());
        assert_eq!(
            after_failure.account_state_version,
            before.account_state_version + 1
        );

        assert_eq!(
            apply_account_failure(
                &pool,
                &account_id,
                before.account_state_version,
                "disabled",
                "auth_error",
                None,
                None,
                "stale unauthorized response",
                4,
                30,
            )
            .await
            .unwrap(),
            None
        );
        let after_stale_failure = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert_eq!(after_stale_failure.status, "cooldown");
        assert_eq!(after_stale_failure.status_reason, "rate_limited");
        assert_eq!(
            after_stale_failure.last_error.as_deref(),
            Some("rate limit")
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    async fn stale_success_after_failure_transition(
        tag: &str,
        status: &str,
        reason: &str,
        cooldown_until: Option<String>,
        quota_reset_at: Option<String>,
    ) {
        let (pool, account_id, root) = recovery_account(tag).await;
        let attempt_account = get_account(&pool, &account_id).await.unwrap().unwrap();
        let observed_state_version = attempt_account.account_state_version;
        let (started_tx, started_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();
        let success_pool = pool.clone();
        let success_account_id = account_id.clone();
        let success = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            continue_rx.await.unwrap();
            recover_account_after_success(
                &success_pool,
                &success_account_id,
                observed_state_version,
                false,
            )
            .await
            .unwrap()
        });

        // Model a request already in flight before another request records its
        // newer lifecycle transition; let its 2xx recovery run afterward.
        started_rx.await.unwrap();
        set_account_status(
            &pool,
            &account_id,
            status,
            reason,
            cooldown_until.as_deref(),
            quota_reset_at.as_deref(),
            Some("newer failure"),
        )
        .await
        .unwrap();
        continue_tx.send(()).unwrap();

        assert!(!success.await.unwrap());
        let account = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert_eq!(account.status, status);
        assert_eq!(account.status_reason, reason);
        assert_eq!(account.cooldown_until, cooldown_until);
        assert_eq!(account.quota_reset_at, quota_reset_at);
        assert_eq!(account.last_error.as_deref(), Some("newer failure"));

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn in_flight_success_does_not_clear_newer_rate_limit_cooldown() {
        stale_success_after_failure_transition(
            "rate-limit",
            "cooldown",
            "rate_limited",
            Some((Utc::now() + chrono::Duration::minutes(1)).to_rfc3339()),
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn in_flight_success_does_not_clear_newer_quota_reset() {
        stale_success_after_failure_transition(
            "quota",
            "exhausted",
            "account_quota_exhausted",
            None,
            Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
        )
        .await;
    }

    #[tokio::test]
    async fn in_flight_success_preserves_failure_recorded_by_pipeline() {
        let (pool, account_id, root) = recovery_account("stale-circuit-success").await;
        let attempt_account = get_account(&pool, &account_id).await.unwrap().unwrap();
        let observed_state_version = attempt_account.account_state_version;
        let (started_tx, started_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();
        let success_pool = pool.clone();
        let success_account_id = account_id.clone();
        let success = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            continue_rx.await.unwrap();
            recover_account_after_success(
                &success_pool,
                &success_account_id,
                observed_state_version,
                false,
            )
            .await
            .unwrap()
        });

        // This is the same failure-evidence write used by handle_key_failure.
        started_rx.await.unwrap();
        assert_eq!(
            record_account_failure(&pool, &account_id, 1, 60)
                .await
                .unwrap(),
            1
        );
        let failed = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert!(failed.circuit_open_until.is_some());
        assert_eq!(failed.consecutive_failures, 1);

        // A valid 2xx from the older in-flight attempt must not clear B's failure.
        continue_tx.send(()).unwrap();
        assert!(!success.await.unwrap());
        let account = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert!(account.circuit_open_until.is_some());
        assert_eq!(account.consecutive_failures, 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    async fn expired_lifecycle_with_open_circuit_is_probeable(
        tag: &str,
        status: &str,
        reason: &str,
        cooldown: bool,
    ) {
        let (pool, account_id, root) = recovery_account(tag).await;
        let started = Utc::now();
        let expires = (started + chrono::Duration::seconds(1)).to_rfc3339();
        set_account_status(
            &pool,
            &account_id,
            status,
            reason,
            cooldown.then_some(expires.as_str()),
            (!cooldown).then_some(expires.as_str()),
            None,
        )
        .await
        .unwrap();

        for _ in 0..3 {
            record_account_failure(&pool, &account_id, 3, 2)
                .await
                .unwrap();
        }
        let account = get_account(&pool, &account_id).await.unwrap().unwrap();
        let after_windows = started + chrono::Duration::seconds(4);
        assert_eq!(
            crate::pool::effective_status_at(&account, after_windows),
            crate::pool::AccountStatus::CircuitOpen
        );
        assert!(
            crate::pool::should_probe_at(&account, after_windows),
            "expired {status} lifecycle and circuit windows must allow a bounded probe"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn expired_rate_limit_with_expired_circuit_allows_half_open_probe() {
        expired_lifecycle_with_open_circuit_is_probeable(
            "expired-rate-limit-probe",
            "cooldown",
            "rate_limited",
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn expired_quota_with_expired_circuit_allows_half_open_probe() {
        expired_lifecycle_with_open_circuit_is_probeable(
            "expired-quota-probe",
            "exhausted",
            "account_quota_exhausted",
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn account_updates_reject_runtime_owned_statuses() {
        let (pool, account_id, root) = recovery_account("invalid-admin-status").await;
        let until = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        set_account_status(
            &pool,
            &account_id,
            "cooldown",
            "rate_limited",
            Some(&until),
            None,
            Some("rate limited"),
        )
        .await
        .unwrap();
        let before = get_account(&pool, &account_id).await.unwrap().unwrap();

        assert!(update_account(
            &pool,
            &account_id,
            "renamed",
            Some("cooldown"),
            2,
            1,
            None,
            "none",
            None,
        )
        .await
        .is_err());

        let after = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert_eq!(after.label, before.label);
        assert_eq!(after.status, before.status);
        assert_eq!(after.status_reason, before.status_reason);
        assert_eq!(after.cooldown_until, before.cooldown_until);
        assert_eq!(after.account_state_version, before.account_state_version);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn success_clears_circuit_state_without_rewriting_account_status() {
        let (pool, account_id, root) = recovery_account("circuit").await;
        record_account_failure(&pool, &account_id, 1, 30)
            .await
            .unwrap();

        let account = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert!(recover_account_after_success(
            &pool,
            &account_id,
            account.account_state_version,
            true,
        )
        .await
        .unwrap());
        let account = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert_eq!(account.status, "healthy");
        assert_eq!(account.status_reason, "circuit_recovered");
        assert!(account.circuit_open_until.is_none());
        assert_eq!(account.consecutive_failures, 0);
        assert!(account.cooldown_until.is_none());
        assert!(account.quota_reset_at.is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    async fn expired_lifecycle_success_recovers(
        tag: &str,
        status: &str,
        reason: &str,
        expected_reason: &str,
        is_cooldown: bool,
    ) {
        let (pool, account_id, root) = recovery_account(tag).await;
        let expired = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        set_account_status(
            &pool,
            &account_id,
            status,
            reason,
            is_cooldown.then_some(expired.as_str()),
            (!is_cooldown).then_some(expired.as_str()),
            Some("old upstream failure"),
        )
        .await
        .unwrap();
        record_account_failure(&pool, &account_id, 3, 60)
            .await
            .unwrap();
        sqlx::query("UPDATE accounts SET status_changed_at='2000-01-01T00:00:00Z' WHERE id=?")
            .bind(&account_id)
            .execute(&pool)
            .await
            .unwrap();
        let before = get_account(&pool, &account_id).await.unwrap().unwrap();

        assert!(recover_account_after_success(
            &pool,
            &account_id,
            before.account_state_version,
            false,
        )
        .await
        .unwrap());
        let after = get_account(&pool, &account_id).await.unwrap().unwrap();
        assert_eq!(after.status, "healthy");
        assert_eq!(after.status_reason, expected_reason);
        assert_ne!(after.status_changed_at, before.status_changed_at);
        assert!(after
            .status_changed_at
            .as_deref()
            .and_then(parse_dt)
            .is_some_and(|changed| changed >= Utc::now() - chrono::Duration::seconds(2)));
        assert!(after.cooldown_until.is_none());
        assert!(after.quota_reset_at.is_none());
        assert!(after.last_error.is_none());
        assert!(after.circuit_open_until.is_none());
        assert_eq!(after.consecutive_failures, 0);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn successful_traffic_persists_expired_cooldown_recovery() {
        expired_lifecycle_success_recovers(
            "expired-cooldown-success",
            "cooldown",
            "rate_limited",
            "cooldown_elapsed",
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn successful_traffic_persists_expired_quota_recovery() {
        expired_lifecycle_success_recovers(
            "expired-quota-success",
            "exhausted",
            "account_quota_exhausted",
            "quota_reset",
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn concurrent_half_open_claims_have_exactly_one_winner() {
        let (pool, account_id, root) = recovery_account("probe-claim-race").await;
        record_account_failure(&pool, &account_id, 1, -1)
            .await
            .unwrap();
        let account = get_account(&pool, &account_id).await.unwrap().unwrap();
        let now = Utc::now();

        let (first, second) = tokio::join!(
            claim_half_open_probe_at(
                &pool,
                &account_id,
                account.account_state_version,
                now,
                crate::pool::HALF_OPEN_PROBE_MIN_GAP_SECS,
            ),
            claim_half_open_probe_at(
                &pool,
                &account_id,
                account.account_state_version,
                now,
                crate::pool::HALF_OPEN_PROBE_MIN_GAP_SECS,
            ),
        );
        assert_eq!(
            usize::from(first.unwrap()) + usize::from(second.unwrap()),
            1
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod pending_marker_publication_tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_renameat2_errors_use_atomic_no_replace_fallback() {
        for code in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
            assert!(renameat2_unsupported(&std::io::Error::from_raw_os_error(
                code
            )));
        }
        assert!(!renameat2_unsupported(&std::io::Error::from_raw_os_error(
            libc::EACCES
        )));

        let root = std::env::temp_dir().join(format!(
            "kinetix-marker-fallback-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let temporary = root.join("marker.tmp");
        let marker = root.join("marker.ready");
        tokio::fs::write(&temporary, b"first").await.unwrap();
        assert!(publish_after_renameat2(
            Err(std::io::Error::from_raw_os_error(libc::ENOSYS)),
            &temporary,
            &marker,
        )
        .await
        .unwrap());
        assert!(!temporary.exists());

        tokio::fs::write(&temporary, b"replacement").await.unwrap();
        assert!(!publish_after_renameat2(
            Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
            &temporary,
            &marker,
        )
        .await
        .unwrap());
        assert_eq!(tokio::fs::read(&marker).await.unwrap(), b"first");
        assert_eq!(tokio::fs::read(&temporary).await.unwrap(), b"replacement");

        std::fs::remove_dir_all(root).unwrap();
    }
}

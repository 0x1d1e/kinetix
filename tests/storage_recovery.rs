use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use kinetix::{crypto::Crypto, db};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::process::Command;
use tokio::time::timeout;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kinetix-storage-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn database_url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

async fn migrate_to_prefix(pool: &SqlitePool, count: usize) {
    let migrator = Migrator {
        migrations: Cow::Owned(MIGRATOR.iter().take(count).cloned().collect()),
        ..Migrator::DEFAULT
    };
    migrator.run(pool).await.unwrap();
}

#[tokio::test]
async fn pre_migration_backup_includes_committed_wal_data() {
    let root = temp_root("pre-migration-backup");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();

    // Keep the committed row in the WAL so a main-file-only copy loses it.
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('backup-sentinel', 'present')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        db_path
            .with_file_name("kinetix.db-wal")
            .metadata()
            .unwrap()
            .len()
            > 0
    );

    let backup_path = db::backup_before_migration(&pool, &url, &root)
        .await
        .unwrap();
    let backup = db::connect(&database_url(&backup_path)).await.unwrap();
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'backup-sentinel'")
            .fetch_optional(&backup)
            .await
            .unwrap();

    assert_eq!(value.as_deref(), Some("present"));
    backup.close().await;
    pool.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn all_historical_migration_prefixes_upgrade_without_data_loss() {
    let total = MIGRATOR.iter().len();
    for prefix in 0..=total {
        let root = temp_root("migration-prefix");
        let db_path = root.join("kinetix.db");
        let url = database_url(&db_path);
        let pool = db::connect(&url).await.unwrap();
        migrate_to_prefix(&pool, prefix).await;

        if prefix > 0 {
            sqlx::query("INSERT INTO settings (key, value) VALUES ('upgrade-sentinel', ?)")
                .bind(format!("before-{prefix}"))
                .execute(&pool)
                .await
                .unwrap();
        }

        db::migrate(&pool).await.unwrap();
        let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(applied, total as i64, "migration prefix {prefix}");
        let plugin_kv: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='plugin_kv'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(plugin_kv, 1, "migration prefix {prefix}");
        if prefix > 0 {
            let value: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key='upgrade-sentinel'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(value, format!("before-{prefix}"));
        }

        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn scheduled_backup_is_consistent_and_restores_plugin_kv_after_sidecar_cleanup() {
    let root = temp_root("restore");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO plugins
         (id, version, plugin_api_major, package_sha256, manifest_json, component, installed_at, updated_at)
         VALUES ('storage.test', '1.0.0', 1, 'hash', '{}', X'00', 'now', 'now')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let crypto = Crypto::new(&[17u8; 32]);
    kinetix::plugins::store::kv_put(&pool, &crypto, "storage.test", "persistent", b"before")
        .await
        .unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('restore-sentinel', 'committed')")
        .execute(&pool)
        .await
        .unwrap();

    let mut uncommitted = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('uncommitted-sentinel', 'hidden')")
        .execute(&mut *uncommitted)
        .await
        .unwrap();
    let backup_path = db::scheduled_backup(&pool, &url, &root, 14)
        .await
        .unwrap()
        .unwrap();
    let restore_guide = std::fs::read_to_string(root.join("backups/RESTORE.txt")).unwrap();
    assert!(restore_guide.contains("Stop Kinetix"));
    assert!(restore_guide.contains("kinetix.db-wal"));
    assert!(restore_guide.contains("kinetix.db-shm"));
    uncommitted.rollback().await.unwrap();

    let snapshot = db::connect(&database_url(&backup_path)).await.unwrap();
    let snapshot_value: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE key='restore-sentinel'")
            .fetch_one(&snapshot)
            .await
            .unwrap();
    let uncommitted_value: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key='uncommitted-sentinel'")
            .fetch_optional(&snapshot)
            .await
            .unwrap();
    assert_eq!(snapshot_value, "committed");
    assert!(uncommitted_value.is_none());
    snapshot.close().await;

    sqlx::query("UPDATE settings SET value='after-backup' WHERE key='restore-sentinel'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let wal_path = db_path.with_file_name("kinetix.db-wal");
    let shm_path = db_path.with_file_name("kinetix.db-shm");
    for sidecar in [wal_path, shm_path] {
        if let Err(error) = std::fs::remove_file(sidecar) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
    }
    std::fs::copy(&backup_path, &db_path).unwrap();

    let restored = db::connect(&url).await.unwrap();
    db::migrate(&restored).await.unwrap();
    let restored_value: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE key='restore-sentinel'")
            .fetch_one(&restored)
            .await
            .unwrap();
    let restored_kv =
        kinetix::plugins::store::kv_get(&restored, &crypto, "storage.test", "persistent")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(restored_value, "committed");
    assert_eq!(restored_kv, b"before");

    restored.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn abrupt_process_exit_recovers_committed_wal_transactions() {
    let root = temp_root("abrupt-exit");
    let db_path = root.join("kinetix.db");
    let worker = Command::new(std::env::current_exe().unwrap())
        .kill_on_drop(true)
        .arg("--exact")
        .arg("abrupt_process_exit_worker")
        .arg("--nocapture")
        .env("KINETIX_STORAGE_CRASH_DB", &db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = timeout(Duration::from_secs(20), worker)
        .await
        .expect("crash worker timed out")
        .unwrap();
    assert_eq!(status.code(), Some(73));

    let pool = db::connect(&database_url(&db_path)).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='crash-sentinel'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(value, "committed");

    pool.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn abrupt_process_exit_worker() {
    let Ok(path) = std::env::var("KINETIX_STORAGE_CRASH_DB") else {
        return;
    };
    let url = database_url(Path::new(&path));
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('crash-sentinel', 'committed')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(Path::new(&format!("{path}-wal")).metadata().unwrap().len() > 0);
    std::process::exit(73);
}

#[tokio::test]
async fn read_only_database_rejects_required_migrations() {
    let root = temp_root("read-only");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    migrate_to_prefix(&pool, 1).await;
    pool.close().await;

    let read_only_url = format!("{url}?mode=ro");
    let result = async {
        let pool = db::connect(&read_only_url).await?;
        db::backup_before_migration(&pool, &read_only_url, &root).await;
        db::migrate(&pool).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    assert!(
        result.is_err(),
        "read-only database must not finish migration"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn sqlite_full_and_unavailable_backup_directory_fail_without_partial_data() {
    let root = temp_root("disk-full");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let options = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();

    let mut connection = pool.acquire().await.unwrap();
    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    sqlx::query(&format!("PRAGMA max_page_count = {page_count}"))
        .execute(&mut *connection)
        .await
        .unwrap();
    let full = sqlx::query("INSERT INTO settings (key, value) VALUES ('full-sentinel', ?)")
        .bind("x".repeat(2 * 1024 * 1024))
        .execute(&mut *connection)
        .await;
    let full_error = full.expect_err("bounded SQLite database should report SQLITE_FULL");
    assert!(
        full_error.to_string().contains("full"),
        "expected SQLITE_FULL, got {full_error}"
    );
    drop(connection);
    let missing: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key='full-sentinel'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(
        missing.is_none(),
        "failed statement must not commit partial data"
    );

    let invalid_data_dir = root.join("not-a-directory");
    std::fs::write(&invalid_data_dir, b"block backup directory creation").unwrap();
    let backup = db::scheduled_backup(&pool, &url, &invalid_data_dir, 14).await;
    assert!(
        backup.is_err(),
        "unavailable backup directory must be reported"
    );

    pool.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn migration_failure_does_not_start_server_or_commit_partial_migration() {
    let root = temp_root("failed-migration");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    migrate_to_prefix(&pool, 2).await;
    // Force migration 3 to fail partway through, after creating its plugin tables.
    sqlx::query("ALTER TABLE usage_logs ADD COLUMN plugin_id TEXT")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let stderr_path = root.join("server.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kinetix"))
        .arg("--home")
        .arg(root.join("home"))
        .arg("--database-url")
        .arg(&url)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("serve")
        .stderr(Stdio::from(stderr))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let status = match timeout(Duration::from_secs(20), child.wait()).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            panic!("server did not exit after migration failure");
        }
    };
    assert!(!status.success(), "server must exit on a failed migration");
    let stderr = std::fs::read_to_string(stderr_path).unwrap();
    assert!(stderr.contains("running migrations"), "{stderr}");

    let migrated = db::connect(&url).await.unwrap();
    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&migrated)
        .await
        .unwrap();
    assert_eq!(applied, 2, "failed migration must remain unapplied");
    let plugin_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='plugins'",
    )
    .fetch_one(&migrated)
    .await
    .unwrap();
    assert_eq!(plugin_tables, 0, "failed migration DDL must roll back");
    migrated.close().await;

    let backup_dir = root.join("home/data/backups");
    let pre_migration_backup = std::fs::read_dir(backup_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("kinetix-pre-migration-"))
        })
        .expect("startup should take a pre-migration backup");
    let backup = db::connect(&database_url(&pre_migration_backup))
        .await
        .unwrap();
    let backup_version: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&backup)
        .await
        .unwrap();
    assert_eq!(backup_version, 2);
    backup.close().await;

    std::fs::remove_dir_all(root).unwrap();
}

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use kinetix::{
    crypto::Crypto,
    db,
    types::{AuthScheme, Prices, WireFormat},
};
use serde_json::json;
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
    let memory_pool = db::connect("sqlite::memory:").await.unwrap();
    assert!(
        db::backup_before_migration(&memory_pool, "sqlite::memory:", &root)
            .await
            .unwrap()
            .is_none()
    );
    memory_pool.close().await;

    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    assert!(db::backup_before_migration(&pool, &url, &root)
        .await
        .unwrap()
        .is_none());
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
        .unwrap()
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
async fn non_serve_backup_command_snapshots_old_schema_before_migrating() {
    let root = temp_root("cli-migration-backup");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    migrate_to_prefix(&pool, 2).await;
    pool.close().await;

    let home = root.join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_kinetix"))
        .arg("--home")
        .arg(&home)
        .arg("--database-url")
        .arg(&url)
        .arg("backup")
        .arg("list")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "backup list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let backup_dir = home.join("data/backups");
    let backups: Vec<PathBuf> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("kinetix-pre-migration-"))
        })
        .collect();
    assert_eq!(backups.len(), 1, "CLI migration should take one snapshot");

    let filename = backups[0].file_name().unwrap().to_str().unwrap();
    let snapshot_name = filename
        .strip_prefix("kinetix-pre-migration-")
        .and_then(|name| name.strip_suffix(".db"))
        .unwrap();
    let (stamp, uuid) = snapshot_name.split_once('-').unwrap();
    assert_eq!(stamp.len(), 16);
    assert_eq!(uuid.len(), 32);

    let backup = db::connect(&database_url(&backups[0])).await.unwrap();
    let old_migration_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&backup)
        .await
        .unwrap();
    assert_eq!(old_migration_count, 2);
    let old_plugin_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='plugins'",
    )
    .fetch_one(&backup)
    .await
    .unwrap();
    assert_eq!(old_plugin_tables, 0, "snapshot must retain the old schema");
    backup.close().await;

    let migrated = db::connect(&url).await.unwrap();
    let migration_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&migrated)
        .await
        .unwrap();
    assert_eq!(migration_count, MIGRATOR.iter().len() as i64);
    migrated.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn current_database_is_backed_up_before_pricing_scope_repair() {
    let root = temp_root("pricing-repair-backup");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "integration provider",
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
    let model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "legacy-priced-model",
            display_name: "Legacy Priced Model",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!({}),
            prices: json!({"input_per_1m": 0.9}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();
    let prices = Prices {
        input_per_1m: Some(0.9),
        ..Prices::default()
    };
    let provenance = json!({
        "fields": {
            "input_per_1m": {
                "source": "models.dev:provider",
                "metadata": {}
            }
        },
        "catalog_source_state": {"source": "models.dev"}
    });
    db::commit_effective_model_pricing(
        &pool,
        &model_id,
        &prices,
        "models.dev:provider",
        &provenance,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE providers SET pricing_scope='integration' WHERE id=?")
        .bind(&provider_id)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let data_dir = root.join("data");
    let migrated = db::open_and_migrate(&url, &data_dir).await.unwrap();
    assert_eq!(
        db::get_model(&migrated, &model_id)
            .await
            .unwrap()
            .unwrap()
            .prices()
            .input_per_1m,
        None,
        "scope repair should still revoke external catalog prices"
    );
    migrated.close().await;

    let backups: Vec<PathBuf> = std::fs::read_dir(data_dir.join("backups"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("kinetix-pre-migration-"))
        })
        .collect();
    assert_eq!(backups.len(), 1, "pricing repair must be backed up");
    let backup = db::connect(&database_url(&backups[0])).await.unwrap();
    assert_eq!(
        db::get_model(&backup, &model_id)
            .await
            .unwrap()
            .unwrap()
            .prices()
            .input_per_1m,
        Some(0.9),
        "backup must preserve prices from before the repair"
    );
    backup.close().await;
    std::fs::remove_dir_all(root).unwrap();
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
    assert!(restore_guide.contains("VACUUM INTO"));
    assert!(restore_guide.contains("including pre-migration snapshots"));
    assert!(restore_guide.contains("3 pre-migration snapshots"));
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
    std::fs::remove_file(&db_path).unwrap();
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
async fn scheduled_retention_does_not_prune_pre_migration_restore_points() {
    let root = temp_root("retention");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let backup_dir = root.join("backups");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let mut restore_points = Vec::new();
    for index in 0..14 {
        let path = backup_dir.join(format!("kinetix-pre-migration-20200101T{index:06}Z.db"));
        std::fs::write(&path, b"pre-migration restore point").unwrap();
        restore_points.push(path);
    }

    let scheduled = db::scheduled_backup(&pool, &url, &root, 14)
        .await
        .unwrap()
        .unwrap();
    assert!(
        scheduled.exists(),
        "returned scheduled backup must be retained"
    );
    assert!(restore_points.iter().all(|path| path.exists()));
    let retained_restore_points = std::fs::read_dir(&backup_dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("kinetix-pre-migration-"))
        })
        .count();
    assert_eq!(retained_restore_points, 14);

    pool.close().await;
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
        db::backup_before_migration(&pool, &read_only_url, &root).await?;
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pre_migration_backup_failure_aborts_server_before_migrations() {
    let root = temp_root("failed-pre-migration-backup");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    migrate_to_prefix(&pool, 2).await;
    pool.close().await;

    let home = root.join("home");
    let backup_dir = home.join("data/backups");
    std::fs::create_dir_all(backup_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/proc", &backup_dir).unwrap();

    let stderr_path = root.join("server.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kinetix"))
        .arg("--home")
        .arg(&home)
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
            panic!("server did not exit after pre-migration backup failure");
        }
    };
    assert!(
        !status.success(),
        "server must fail closed without a backup"
    );
    let stderr = std::fs::read_to_string(stderr_path).unwrap();
    assert!(stderr.contains("pre-migration backup"), "{stderr}");
    assert!(!stderr.contains("running migrations"), "{stderr}");

    let unchanged = db::connect(&url).await.unwrap();
    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&unchanged)
        .await
        .unwrap();
    assert_eq!(
        applied, 2,
        "backup failure must leave migration history intact"
    );
    let plugin_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='plugins'",
    )
    .fetch_one(&unchanged)
    .await
    .unwrap();
    assert_eq!(
        plugin_tables, 0,
        "migrations must not start without a backup"
    );
    unchanged.close().await;

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn current_database_starts_when_backup_directory_is_unavailable() {
    let root = temp_root("current-db-no-backup");
    let db_path = root.join("kinetix.db");
    let url = database_url(&db_path);
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    pool.close().await;

    let home = root.join("home");
    let backup_dir = home.join("data/backups");
    std::fs::create_dir_all(backup_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/proc", &backup_dir).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let stderr_path = root.join("server.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kinetix"))
        .arg("--home")
        .arg(&home)
        .arg("--database-url")
        .arg(&url)
        .arg("--bind")
        .arg(address.to_string())
        .arg("serve")
        .stderr(Stdio::from(stderr))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();

    let mut server_ready = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            server_ready = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !server_ready {
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill().await;
        }
        let _ = child.wait().await;
        let stderr = std::fs::read_to_string(stderr_path).unwrap();
        panic!("server did not start with current schema: {stderr}");
    }
    child.kill().await.unwrap();
    let _ = child.wait().await.unwrap();

    let still_current = db::connect(&url).await.unwrap();
    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&still_current)
        .await
        .unwrap();
    assert_eq!(applied, MIGRATOR.iter().len() as i64);
    still_current.close().await;
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

    let home = root.join("home");
    let stderr_path = root.join("server.stderr");
    for attempt in 0..5 {
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kinetix"))
            .arg("--home")
            .arg(&home)
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
                panic!("server did not exit after migration failure on attempt {attempt}");
            }
        };
        assert!(
            !status.success(),
            "server must exit on a failed migration (attempt {attempt})"
        );
    }
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
    let pre_migration_backups: Vec<PathBuf> = std::fs::read_dir(backup_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("kinetix-pre-migration-"))
        })
        .collect();
    assert_eq!(
        pre_migration_backups.len(),
        3,
        "restart retries must retain a bounded set of pre-migration snapshots"
    );
    let total_backup_bytes: u64 = pre_migration_backups
        .iter()
        .map(|path| path.metadata().unwrap().len())
        .sum();
    let max_expected_bytes = (db_path.metadata().unwrap().len() + 1024 * 1024) * 3;
    assert!(
        total_backup_bytes <= max_expected_bytes,
        "pre-migration snapshots exceeded the three-database size bound"
    );
    let pre_migration_backup = &pre_migration_backups[0];
    let backup = db::connect(&database_url(pre_migration_backup))
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

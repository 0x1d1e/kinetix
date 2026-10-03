//! Route writes shared by the Admin API, CLI, bootstrap, and config import.
//!
//! Every entry point resolves its own input syntax (names, `provider/upstream`,
//! account refs) to a [`RouteWrite`] and persists it here. This module owns
//! Route validation before persistence, default normalization, and the atomic
//! replacement of a Route row together with its Targets.

use serde_json::{json, Value};
use sqlx::SqliteConnection;

use crate::app::AppState;
use crate::route_validation::{self, RouteConfig, RouteValidation};

#[derive(Debug, thiserror::Error)]
pub enum RouteWriteError {
    /// The Route failed validation; carries every issue found.
    #[error("Route validation failed: {}", error_summary(.0))]
    Invalid(RouteValidation),
    #[error("Route '{0}' not found")]
    NotFound(String),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
    #[error(transparent)]
    Validation(#[from] anyhow::Error),
}

fn error_summary(validation: &RouteValidation) -> String {
    validation
        .issues
        .iter()
        .filter(|issue| issue.severity == "error")
        .map(|issue| format!("{}: {}", issue.code, issue.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A requested Route write. `config.id` selects update (Some) or insert (None);
/// `config.targets` fully replaces the persisted Targets.
#[derive(Debug, Clone)]
pub struct RouteWrite {
    pub config: RouteConfig,
    pub description: String,
    pub sticky_routing: bool,
    pub cache_affinity: bool,
    /// `None` keeps the persisted state on update and enables on insert.
    pub enabled: Option<bool>,
}

/// Validate against the current control-plane snapshot, then persist the Route
/// and its Targets in one transaction. Returns the Route id.
pub async fn save(state: &AppState, write: &RouteWrite) -> Result<String, RouteWriteError> {
    let validation = route_validation::validate(state, &write.config).await?;
    if !validation.valid {
        return Err(RouteWriteError::Invalid(validation));
    }
    let mut tx = state.pool.begin().await?;
    let id = persist(&mut tx, write).await?;
    tx.commit().await?;
    Ok(id)
}

/// Persist inside the caller's transaction, for callers that write the
/// referenced Providers, Accounts, and Models in the same transaction and so
/// cannot validate against the committed snapshot. Applies the structural
/// checks only.
pub async fn write(
    conn: &mut SqliteConnection,
    write: &RouteWrite,
) -> Result<String, RouteWriteError> {
    let validation = route_validation::validate_structure(&write.config);
    if !validation.valid {
        return Err(RouteWriteError::Invalid(validation));
    }
    persist(conn, write).await
}

fn default_fallback_triggers() -> Value {
    json!({"on429": true, "onQuota": true, "on5xx": true, "onTimeout": true})
}

fn json_object_or_empty(value: &Value) -> String {
    if value.is_null() {
        "{}".into()
    } else {
        value.to_string()
    }
}

async fn persist(
    conn: &mut SqliteConnection,
    write: &RouteWrite,
) -> Result<String, RouteWriteError> {
    let config = &write.config;
    let fallback_triggers = if config.fallback_triggers.is_null() {
        default_fallback_triggers()
    } else {
        config.fallback_triggers.clone()
    };
    let max_concurrent_requests = config.max_concurrent_requests.filter(|limit| *limit > 0);
    let id = match &config.id {
        Some(id) => {
            let updated = sqlx::query(
                "UPDATE routes SET description=?, strategy=?, fallback_triggers=?, continuity_policy='strip', portability_policy=?, sticky_routing=?, cache_affinity=?, max_attempts=?, max_concurrent_requests=?, enabled=COALESCE(?, enabled) WHERE id=?",
            )
            .bind(&write.description)
            .bind(&config.strategy)
            .bind(fallback_triggers.to_string())
            .bind(&config.portability_policy)
            .bind(write.sticky_routing as i64)
            .bind(write.cache_affinity as i64)
            .bind(config.max_attempts)
            .bind(max_concurrent_requests)
            .bind(write.enabled.map(i64::from))
            .bind(id)
            .execute(&mut *conn)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(RouteWriteError::NotFound(id.clone()));
            }
            sqlx::query("DELETE FROM route_targets WHERE route_id = ?")
                .bind(id)
                .execute(&mut *conn)
                .await?;
            id.clone()
        }
        None => {
            let id = format!("route_{}", uuid::Uuid::new_v4().simple());
            sqlx::query(
                "INSERT INTO routes (id, name, description, strategy, fallback_triggers, continuity_policy, portability_policy, sticky_routing, cache_affinity, max_attempts, max_concurrent_requests, enabled, created_at) VALUES (?,?,?,?,?,'strip',?,?,?,?,?,?,?)",
            )
            .bind(&id)
            .bind(&config.name)
            .bind(&write.description)
            .bind(&config.strategy)
            .bind(fallback_triggers.to_string())
            .bind(&config.portability_policy)
            .bind(write.sticky_routing as i64)
            .bind(write.cache_affinity as i64)
            .bind(config.max_attempts)
            .bind(max_concurrent_requests)
            .bind(i64::from(write.enabled.unwrap_or(true)))
            .bind(crate::db::now_iso())
            .execute(&mut *conn)
            .await?;
            id
        }
    };
    for target in &config.targets {
        sqlx::query(
            "INSERT INTO route_targets (id, route_id, account_id, model_id, priority, weight, param_overrides, predicate) VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(format!("tgt_{}", uuid::Uuid::new_v4().simple()))
        .bind(&id)
        .bind(target.account_id.as_deref())
        .bind(&target.model_id)
        .bind(target.priority)
        .bind(target.weight)
        .bind(json_object_or_empty(&target.param_overrides))
        .bind(json_object_or_empty(&target.predicate))
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route_validation::RouteTargetConfig;

    async fn pool() -> crate::db::Pool {
        let dir = std::env::temp_dir().join(format!("kinetix-routes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    async fn model(pool: &crate::db::Pool) -> String {
        let provider_id = format!("prov_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO providers (id, name, wire_format, base_url, auth_scheme, created_at) VALUES (?, ?, 'openai', 'https://example.test', 'bearer', ?)",
        )
        .bind(&provider_id)
        .bind(&provider_id)
        .bind(crate::db::now_iso())
        .execute(pool)
        .await
        .unwrap();
        let model_id = format!("model_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO models (id, provider_id, upstream_id, display_name, created_at) VALUES (?, ?, 'm', 'm', ?)",
        )
        .bind(&model_id)
        .bind(&provider_id)
        .bind(crate::db::now_iso())
        .execute(pool)
        .await
        .unwrap();
        model_id
    }

    fn target(model_id: &str) -> RouteTargetConfig {
        RouteTargetConfig {
            model_id: model_id.into(),
            account_id: None,
            priority: 1,
            weight: 1,
            predicate: Value::Null,
            param_overrides: Value::Null,
        }
    }

    fn route(id: Option<&str>, targets: Vec<RouteTargetConfig>) -> RouteWrite {
        RouteWrite {
            config: RouteConfig {
                id: id.map(str::to_owned),
                name: "main".into(),
                strategy: "priority".into(),
                portability_policy: "reject".into(),
                fallback_triggers: Value::Null,
                max_attempts: None,
                max_concurrent_requests: Some(0),
                enabled: true,
                targets,
            },
            description: String::new(),
            sticky_routing: false,
            cache_affinity: false,
            enabled: None,
        }
    }

    async fn route_count(pool: &crate::db::Pool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM routes")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rejects_invalid_routes_before_persisting() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        let mut invalid = route(None, Vec::new());
        invalid.config.strategy = "random".into();
        let error = write(&mut conn, &invalid).await.unwrap_err();
        let RouteWriteError::Invalid(validation) = error else {
            panic!("expected validation error, got {error}");
        };
        let codes: Vec<_> = validation.issues.iter().map(|i| i.code.as_str()).collect();
        assert!(codes.contains(&"invalid_strategy"), "{codes:?}");
        assert!(codes.contains(&"empty_route"), "{codes:?}");
        assert_eq!(route_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn failed_target_write_rolls_back_the_route() {
        let pool = pool().await;
        let model_id = model(&pool).await;
        let mut tx = pool.begin().await.unwrap();
        let error = write(
            &mut tx,
            &route(None, vec![target(&model_id), target("model_missing")]),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RouteWriteError::Storage(_)), "{error}");
        drop(tx);
        assert_eq!(route_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn normalizes_defaults_and_replaces_targets_on_update() {
        let pool = pool().await;
        let first = model(&pool).await;
        let second = model(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let id = write(&mut conn, &route(None, vec![target(&first)]))
            .await
            .unwrap();
        let updated = write(&mut conn, &route(Some(&id), vec![target(&second)]))
            .await
            .unwrap();
        assert_eq!(updated, id);
        drop(conn);

        let stored = crate::db::list_routes(&pool)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored.fallback_triggers).unwrap(),
            default_fallback_triggers()
        );
        assert_eq!(stored.max_concurrent_requests, None);
        assert_eq!(stored.enabled, 1);
        let targets = crate::db::route_targets(&pool, &id).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].model_id, second);
        assert_eq!(targets[0].predicate, "{}");
        assert_eq!(targets[0].param_overrides, "{}");
    }

    #[tokio::test]
    async fn updating_a_missing_route_is_not_found() {
        let pool = pool().await;
        let model_id = model(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let error = write(
            &mut conn,
            &route(Some("route_missing"), vec![target(&model_id)]),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RouteWriteError::NotFound(_)), "{error}");
        let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_targets")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(orphans, 0);
    }
}

//! Alias writes shared by the Admin API, CLI, bootstrap, and config import.
//!
//! Every entry point resolves its own input syntax (names, ids,
//! `provider/upstream`) to a target id and then writes through [`upsert`], so
//! the persisted alias invariants are enforced in exactly one place.

use sqlx::SqliteConnection;

/// What an alias resolves to. Persisted as `model` or `route`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasTarget {
    Model,
    Route,
}

impl AliasTarget {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "model" => Some(Self::Model),
            "route" => Some(Self::Route),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Route => "route",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AliasWriteError {
    /// The requested alias violates an alias invariant.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// A requested alias write. `target_id` is the persisted model or Route id.
#[derive(Debug, Clone, Copy)]
pub struct AliasWrite<'a> {
    pub alias: &'a str,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub description: &'a str,
}

/// Validate and create or update an alias by name, returning its id.
///
/// Run inside the caller's transaction when the alias must commit together
/// with the Route or model it targets.
pub async fn upsert(
    conn: &mut SqliteConnection,
    write: AliasWrite<'_>,
) -> Result<String, AliasWriteError> {
    let alias = write.alias.trim();
    if alias.is_empty() {
        return Err(AliasWriteError::Invalid("alias name is required".into()));
    }
    if alias != write.alias {
        return Err(AliasWriteError::Invalid(format!(
            "alias '{}' must not have leading or trailing whitespace",
            write.alias
        )));
    }
    let target = AliasTarget::parse(write.target_type).ok_or_else(|| {
        AliasWriteError::Invalid(format!(
            "alias '{alias}' has invalid target_type '{}'; expected model or route",
            write.target_type
        ))
    })?;
    let table_query = match target {
        AliasTarget::Model => "SELECT 1 FROM models WHERE id = ?",
        AliasTarget::Route => "SELECT 1 FROM routes WHERE id = ?",
    };
    let exists = sqlx::query_scalar::<_, i64>(table_query)
        .bind(write.target_id)
        .fetch_optional(&mut *conn)
        .await?
        .is_some();
    if !exists {
        return Err(AliasWriteError::Invalid(format!(
            "alias '{alias}' references missing {} '{}'",
            target.as_str(),
            write.target_id
        )));
    }

    let existing = sqlx::query_scalar::<_, String>("SELECT id FROM aliases WHERE alias = ?")
        .bind(alias)
        .fetch_optional(&mut *conn)
        .await?;
    if let Some(id) = existing {
        sqlx::query("UPDATE aliases SET target_type=?, target_id=?, description=? WHERE id=?")
            .bind(target.as_str())
            .bind(write.target_id)
            .bind(write.description)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
        return Ok(id);
    }
    let id = format!("alias_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO aliases (id, alias, target_type, target_id, description, created_at) VALUES (?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(alias)
    .bind(target.as_str())
    .bind(write.target_id)
    .bind(write.description)
    .bind(crate::db::now_iso())
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> crate::db::Pool {
        let dir = std::env::temp_dir().join(format!("kinetix-aliases-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    async fn route(pool: &crate::db::Pool) -> String {
        crate::db::insert_route(
            pool,
            &crate::db::NewRoute {
                name: "alias-target",
                description: "",
                strategy: "priority",
                fallback_triggers: serde_json::json!({}),
                portability_policy: "reject",
                sticky_routing: false,
                cache_affinity: false,
                max_attempts: None,
                max_concurrent_requests: None,
            },
        )
        .await
        .unwrap()
    }

    fn write<'a>(alias: &'a str, target_type: &'a str, target_id: &'a str) -> AliasWrite<'a> {
        AliasWrite {
            alias,
            target_type,
            target_id,
            description: "",
        }
    }

    #[tokio::test]
    async fn rejects_invalid_target_type_missing_target_and_blank_name() {
        let pool = pool().await;
        let route_id = route(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        for (request, expected) in [
            (write("fast", "Route", &route_id), "invalid target_type"),
            (write("fast", "route", "route_missing"), "missing route"),
            (write("fast", "model", &route_id), "missing model"),
            (write("  ", "route", &route_id), "alias name is required"),
            (write(" fast", "route", &route_id), "whitespace"),
        ] {
            let error = upsert(&mut conn, request).await.unwrap_err();
            assert!(
                matches!(&error, AliasWriteError::Invalid(message) if message.contains(expected)),
                "{request:?}: {error}"
            );
        }
        assert!(crate::db::list_aliases(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upserts_by_alias_name() {
        let pool = pool().await;
        let route_id = route(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let first = upsert(&mut conn, write("fast", "route", &route_id))
            .await
            .unwrap();
        let second = upsert(
            &mut conn,
            AliasWrite {
                description: "updated",
                ..write("fast", "route", &route_id)
            },
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        let stored = crate::db::get_alias(&pool, "fast").await.unwrap().unwrap();
        assert_eq!(stored.description, "updated");
        assert_eq!(stored.target_type, "route");
    }
}

//! Bounded, deterministic collection reads for the operator API.
use super::{ListQuery, Page};
use crate::{admin::ApiError, db};
use sqlx::{FromRow, QueryBuilder, Sqlite};

#[derive(Clone, Copy)]
pub enum Collection {
    Keys,
    Providers,
    Models,
    Accounts,
    Routes,
    Aliases,
    Usage,
    Audit,
}

impl Collection {
    fn table(self) -> &'static str {
        match self {
            Self::Keys => "virtual_keys",
            Self::Providers => "providers",
            Self::Models => "models",
            Self::Accounts => "accounts",
            Self::Routes => "routes",
            Self::Aliases => "aliases",
            Self::Usage => "usage_request_logs",
            Self::Audit => "audit_logs",
        }
    }

    fn order(self) -> &'static str {
        match self {
            Self::Keys => "created_at DESC, id DESC",
            Self::Accounts => "priority ASC, created_at ASC, id ASC",
            Self::Aliases => "alias ASC, id ASC",
            Self::Usage | Self::Audit => "ts DESC, id DESC",
            _ => "created_at ASC, id ASC",
        }
    }

    fn search(self) -> &'static str {
        match self {
            Self::Keys => "name",
            Self::Providers | Self::Routes => "name",
            Self::Models => "upstream_id",
            Self::Accounts => "label",
            Self::Aliases => "alias",
            Self::Usage => "requested_model",
            Self::Audit => "target_name",
        }
    }

    fn filter_column(self, name: &str) -> Option<&'static str> {
        match (self, name) {
            (Self::Models | Self::Accounts, "provider_id") => Some("provider_id"),
            (Self::Usage, "key_id") => Some("key_id"),
            (Self::Keys | Self::Usage, "status") => Some("status"),
            (Self::Audit, "actor") => Some("actor"),
            (Self::Audit, "action") => Some("action"),
            _ => None,
        }
    }
}

pub fn reference() -> serde_json::Value {
    let mut collections = serde_json::Map::new();
    for (name, collection) in [
        ("keys", Collection::Keys),
        ("providers", Collection::Providers),
        ("models", Collection::Models),
        ("accounts", Collection::Accounts),
        ("routes", Collection::Routes),
        ("aliases", Collection::Aliases),
        ("usage", Collection::Usage),
        ("audit", Collection::Audit),
    ] {
        let mut filters = vec!["q"];
        filters.extend(
            ["provider_id", "key_id", "status", "actor", "action"]
                .into_iter()
                .filter(|name| collection.filter_column(name).is_some()),
        );
        collections.insert(name.into(), serde_json::json!({"order": collection.order(), "filters": filters, "search_field": collection.search()}));
    }
    collections.insert(
        "requests".into(),
        serde_json::json!({"collection": "usage"}),
    );
    serde_json::Value::Object(collections)
}

fn filters<'a>(
    builder: &mut QueryBuilder<'a, Sqlite>,
    collection: Collection,
    query: &'a ListQuery,
) -> Result<(), ApiError> {
    builder.push(" WHERE 1=1");
    if matches!(collection, Collection::Accounts) {
        builder.push(" AND NOT (label='__kinetix_noauth__' AND EXISTS (SELECT 1 FROM providers WHERE providers.id=accounts.provider_id AND providers.credential_mode='none'))");
    }
    if let Some(q) = &query.q {
        builder
            .push(" AND instr(lower(")
            .push(collection.search())
            .push("), lower(")
            .push_bind(q)
            .push(")) > 0");
    }
    for (name, value) in [
        ("provider_id", &query.provider_id),
        ("key_id", &query.key_id),
        ("status", &query.status),
        ("actor", &query.actor),
        ("action", &query.action),
    ] {
        if let Some(value) = value {
            let column = collection.filter_column(name).ok_or_else(|| {
                ApiError::field(name, "filter is not supported by this collection")
            })?;
            builder
                .push(" AND ")
                .push(column)
                .push(" = ")
                .push_bind(value);
        }
    }
    Ok(())
}

pub async fn page<T>(
    pool: &db::Pool,
    collection: Collection,
    query: &ListQuery,
) -> Result<(Vec<T>, Page), ApiError>
where
    T: for<'r> FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
{
    let (limit, offset) = query.bounds()?;
    // Count and rows share a SQLite snapshot, even during concurrent mutations.
    let mut tx = pool.begin().await.map_err(ApiError::internal)?;
    let mut count = QueryBuilder::new(format!("SELECT COUNT(*) FROM {}", collection.table()));
    filters(&mut count, collection, query)?;
    let total: i64 = count
        .build_query_scalar()
        .fetch_one(&mut *tx)
        .await
        .map_err(ApiError::internal)?;
    let mut select = QueryBuilder::new(format!("SELECT * FROM {}", collection.table()));
    filters(&mut select, collection, query)?;
    select
        .push(" ORDER BY ")
        .push(collection.order())
        .push(" LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows: Vec<T> = select
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(ApiError::internal)?;
    tx.commit().await.map_err(ApiError::internal)?;
    let page = Page::new(limit, offset, total, rows.len());
    Ok((rows, page))
}

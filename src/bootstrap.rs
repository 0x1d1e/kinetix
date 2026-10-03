//! Bootstrap seeding: on first run (empty database) seed providers, models,
//! credentials, virtual keys, aliases, and routes from a TOML file. The
//! database is authoritative afterwards (FR-8.5).

use anyhow::Result;
use serde_json::json;

use crate::config::BootstrapConfig;
use crate::crypto::{self, Crypto};
use crate::db::{self, Pool};
use crate::types::{AuthScheme, Capabilities, Prices, WireFormat};

fn toml_json(value: Option<&toml::Value>) -> serde_json::Value {
    value
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| json!({}))
}

/// Seed the database if it has no providers yet. Returns the number of
/// virtual keys created, and any generated key values to log once.
pub async fn seed_if_empty(
    pool: &Pool,
    crypto: &Crypto,
    cfg: &BootstrapConfig,
) -> Result<Vec<(String, String)>> {
    let existing = db::list_providers(pool).await?;
    if !existing.is_empty() {
        tracing::info!("database already has providers; skipping bootstrap seed");
        return Ok(Vec::new());
    }
    if cfg.providers.is_empty() && cfg.virtual_keys.is_empty() {
        tracing::warn!("bootstrap file has no providers or keys; nothing to seed");
        return Ok(Vec::new());
    }

    let mut provider_ids: std::collections::HashMap<String, String> = Default::default();
    let mut model_ids: std::collections::HashMap<String, String> = Default::default();
    let mut account_ids: std::collections::HashMap<String, String> = Default::default();

    for p in &cfg.providers {
        let wire = WireFormat::parse(&p.wire_format).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid wire_format '{}' for provider '{}'",
                p.wire_format,
                p.name
            )
        })?;
        if wire == WireFormat::Plugin && p.wire_plugin.as_deref().unwrap_or("").trim().is_empty() {
            anyhow::bail!(
                "provider '{}' uses wire_format 'plugin' but has no wire_plugin binding",
                p.name
            );
        }
        let auth = AuthScheme::parse(&p.auth_scheme)
            .ok_or_else(|| anyhow::anyhow!("invalid auth_scheme"))?;
        let anonymous = auth == AuthScheme::None;
        anyhow::ensure!(
            !anonymous || p.credential_mode == Some(crate::plugins::CredentialMode::None),
            "auth_scheme 'none' requires credential_mode 'none'"
        );
        anyhow::ensure!(
            !anonymous || p.accounts.is_empty(),
            "no-auth providers must not configure account credentials"
        );
        anyhow::ensure!(
            anonymous
                || p.credential_mode
                    .is_none_or(|mode| mode == crate::plugins::CredentialMode::Manual),
            "authenticated bootstrap providers use manual credential enrollment"
        );
        crate::provider_connection::resolve_endpoint(
            &p.base_url,
            p.models_path.as_deref(),
            p.connection_parameters.as_ref(),
        )
        .map_err(anyhow::Error::msg)?;

        let id = db::insert_provider(
            pool,
            &db::NewProvider {
                name: &p.name,
                base_url: &p.base_url,
                wire_format: wire,
                auth_scheme: auth,
                custom_header_name: p.custom_header_name.as_deref(),
                custom_param_name: p.custom_param_name.as_deref(),
                extra_headers: serde_json::to_value(&p.extra_headers).unwrap_or(json!({})),
                timeout_ms: p.timeout_ms as i64,
                capability_mode: &p.capability_mode,
                models_path: p.models_path.as_deref(),
                rate_limit_rules: toml_json(p.rate_limit_rules.as_ref()),
                follow_redirects: p.follow_redirects,
                credential_hosts: &p.credential_hosts,
                allow_insecure_tls: p.allow_insecure_tls,
                wire_plugin: p.wire_plugin.as_deref().unwrap_or(""),
                credential_plugin: p.credential_plugin.as_deref().unwrap_or(""),
                model_source_plugin: p.model_source_plugin.as_deref().unwrap_or(""),
                credential_mode: p
                    .credential_mode
                    .unwrap_or(crate::plugins::CredentialMode::Manual)
                    .as_str(),
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await?;
        db::set_provider_connection_parameters(pool, &id, p.connection_parameters.as_ref()).await?;
        if anonymous {
            db::insert_account(pool, &id, "__kinetix_noauth__", "", "", 1, 1, None, "none").await?;
        }
        provider_ids.insert(p.name.clone(), id.clone());

        // Accounts.
        for a in &p.accounts {
            let enc = crypto.encrypt(&a.api_key)?;
            let acc_id = db::insert_account(
                pool,
                &id,
                &a.label,
                &enc,
                &crypto::mask_secret(&a.api_key),
                a.priority,
                a.weight,
                a.soft_quota_usd,
                &a.quota_type,
            )
            .await?;
            if let Some(window) = a.quota_window_s {
                sqlx::query("UPDATE accounts SET quota_window_s=? WHERE id=?")
                    .bind(window)
                    .bind(&acc_id)
                    .execute(pool)
                    .await?;
            }
            account_ids.insert(a.label.clone(), acc_id);
        }

        // Models.
        for m in &p.models {
            let caps = Capabilities::from_tokens(&m.capabilities.clone().unwrap_or_default());
            let prices = Prices {
                input_per_1m: m.prices.as_ref().and_then(|x| x.input_per_1m),
                output_per_1m: m.prices.as_ref().and_then(|x| x.output_per_1m),
                cached_per_1m: m.prices.as_ref().and_then(|x| x.cached_per_1m),
                cache_write_per_1m: m.prices.as_ref().and_then(|x| x.cache_write_per_1m),
                thinking_per_1m: m.prices.as_ref().and_then(|x| x.thinking_per_1m),
            };
            let model_id = db::insert_model(
                pool,
                &db::NewModel {
                    provider_id: &id,
                    upstream_id: &m.upstream_id,
                    display_name: m.display_name.as_deref().unwrap_or(&m.upstream_id),
                    enabled: m.enabled,
                    context_window: m.context_window,
                    max_output_tokens: m.max_output_tokens,
                    capabilities: serde_json::to_value(&caps).unwrap(),
                    prices: serde_json::to_value(&prices).unwrap(),
                    parameters: toml_json(m.parameters.as_ref()),
                    thinking_map: toml_json(m.thinking_map.as_ref()),
                    extra_request: toml_json(m.extra_request.as_ref()),
                    discovery: json!({}),
                },
            )
            .await?;
            if prices.is_configured() {
                let _ = db::insert_price_version(pool, &model_id, &prices).await;
            }
            model_ids.insert(format!("{}/{}", p.name, m.upstream_id), model_id);
        }
    }

    // Routes.
    let mut route_ids: std::collections::HashMap<String, String> = Default::default();
    for c in &cfg.routes {
        if c.max_concurrent_requests.is_some_and(|limit| limit < 0) {
            anyhow::bail!(
                "route '{}' max_concurrent_requests must be positive or omitted",
                c.name
            );
        }
        let route_id = db::insert_route(
            pool,
            &db::NewRoute {
                name: &c.name,
                description: &c.description,
                strategy: if c.strategy.is_empty() { "priority" } else { &c.strategy },
                fallback_triggers: c
                    .fallback_triggers
                    .as_ref()
                    .map(|v| toml_json(Some(v)))
                    .unwrap_or_else(|| json!({"on429": true, "onQuota": true, "on5xx": true, "onTimeout": true})),
                portability_policy: if c.portability_policy.is_empty() {
                    if c.continuity_policy.as_deref() == Some("error") {
                        "reject"
                    } else {
                        "strip_with_warning"
                    }
                } else {
                    &c.portability_policy
                },
                sticky_routing: c.sticky_routing,
                cache_affinity: c.cache_affinity,
                max_attempts: c.max_attempts,
                max_concurrent_requests: c.max_concurrent_requests.filter(|value| *value > 0),
            },
        )
        .await?;
        for t in &c.targets {
            let account_id = match t.account.as_deref() {
                Some(label) => Some(account_ids.get(label).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "route '{}' target references unknown account '{}'",
                        c.name,
                        label
                    )
                })?),
                None => None,
            };
            let model_id = model_ids.get(&t.model).cloned();
            if let Some(model_id) = model_id {
                let predicate = toml_json(t.predicate.as_ref()).to_string();
                let overrides = toml_json(t.param_overrides.as_ref()).to_string();
                db::insert_route_target(
                    pool,
                    &route_id,
                    account_id.as_deref(),
                    &model_id,
                    t.priority,
                    t.weight.unwrap_or(1),
                    &predicate,
                    &overrides,
                )
                .await?;
            } else {
                tracing::warn!(
                    "route '{}' target references unknown model '{}'",
                    c.name,
                    t.model
                );
            }
        }
        route_ids.insert(c.name.clone(), route_id);
    }

    // Aliases.
    for a in &cfg.aliases {
        let target_id = match a.target_type.as_str() {
            "route" => route_ids.get(&a.target).cloned(),
            _ => model_ids.get(&a.target).cloned(),
        };
        if let Some(target_id) = target_id {
            match crate::aliases::upsert(
                &mut *pool.acquire().await?,
                crate::aliases::AliasWrite {
                    alias: &a.alias,
                    target_type: &a.target_type,
                    target_id: &target_id,
                    description: "Seeded from bootstrap config",
                },
            )
            .await
            {
                Ok(_) => {}
                Err(crate::aliases::AliasWriteError::Invalid(message)) => {
                    tracing::warn!("skipping bootstrap alias: {message}");
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            tracing::warn!(
                "alias '{}' references unknown target '{}'",
                a.alias,
                a.target
            );
        }
    }

    // Virtual keys.
    let mut generated = Vec::new();
    for k in &cfg.virtual_keys {
        let full = k.key.clone().unwrap_or_else(crypto::generate_virtual_key);
        let hash = crypto::hash_virtual_key(&full);
        let row = db::VirtualKeyRow {
            id: format!("key_{}", uuid::Uuid::new_v4().simple()),
            key_hash: hash,
            name: k.name.clone(),
            owner: k.owner.clone(),
            tag: k.tag.clone(),
            allowed_models: serde_json::to_string(&k.allowed_models).unwrap(),
            allowed_providers: serde_json::to_string(&k.allowed_providers).unwrap(),
            rpm_limit: k.rpm_limit.map(|v| v as i64),
            tpm_limit: k.tpm_limit.map(|v| v as i64),
            max_concurrent_requests: k
                .max_concurrent_requests
                .filter(|value| *value > 0)
                .map(|v| v as i64),
            daily_budget: k.daily_budget,
            monthly_budget: k.monthly_budget,
            expires_at: k.expires_at.clone(),
            status: "active".into(),
            allowed_ips: serde_json::to_string(&k.allowed_ips).unwrap(),
            body_logging: k.body_logging as i64,
            created_at: db::now_iso(),
            revoked_at: None,
        };
        db::insert_virtual_key(pool, &row).await?;
        if k.key.is_none() {
            generated.push((k.name.clone(), full));
        }
    }

    Ok(generated)
}

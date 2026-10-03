//! Official plugin catalog and publisher trust metadata.
//!
//! Kinetix keeps a vendored snapshot for offline/default discovery. The source
//! catalog and publisher metadata live in `PrightCord/kinetix-plugins`; these
//! snapshots are host assets, not plugin source. Publisher keys remain separate
//! so changing a catalog entry cannot introduce a new trusted signing key.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/PrightCord/kinetix-plugins/main/catalog.json";
pub const CATALOG_CACHE_FILE: &str = "catalog.cache.json";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Catalog {
    pub schema_version: u32,
    #[serde(default)]
    pub plugins: Vec<CatalogPlugin>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogPlugin {
    pub id: String,
    pub name: String,
    pub description: String,
    pub publisher: String,
    #[serde(default)]
    pub official: bool,
    pub homepage: String,
    pub latest_version: String,
    pub artifact_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub installable: bool,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub release_url: Option<String>,
    #[serde(default)]
    pub changelog_url: Option<String>,
    #[serde(default)]
    pub distribution: Option<CatalogDistribution>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogDistribution {
    pub url: String,
    pub sha256: String,
    pub publisher_key_id: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublisherTrustStore {
    pub schema_version: u32,
    #[serde(default)]
    pub publishers: Vec<TrustedPublisher>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrustedPublisher {
    pub id: String,
    pub publisher: String,
    pub algorithm: String,
    pub public_key_base64: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

pub fn embedded_catalog() -> Result<Catalog> {
    let catalog: Catalog = serde_json::from_str(include_str!("catalog.snapshot.json"))
        .context("parsing embedded plugin catalog")?;
    if catalog.schema_version != 1 {
        bail!(
            "unsupported plugin catalog schema_version {}",
            catalog.schema_version
        );
    }
    validate_catalog_links(&catalog)?;
    Ok(catalog)
}

pub fn embedded_trust_store() -> Result<PublisherTrustStore> {
    let store: PublisherTrustStore =
        serde_json::from_str(include_str!("trusted-publishers.snapshot.json"))
            .context("parsing embedded plugin publisher trust store")?;
    if store.schema_version != 1 {
        bail!(
            "unsupported plugin publisher trust schema_version {}",
            store.schema_version
        );
    }
    Ok(store)
}

pub async fn fetch_remote_catalog(client: &reqwest::Client, url: &str) -> Result<Catalog> {
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .context("fetching remote catalog")?;

    if !response.status().is_success() {
        bail!("remote catalog returned HTTP {}", response.status());
    }

    let text = response
        .text()
        .await
        .context("reading remote catalog body")?;
    let catalog: Catalog = serde_json::from_str(&text).context("parsing remote catalog JSON")?;
    if catalog.schema_version != 1 {
        bail!(
            "unsupported remote plugin catalog schema_version {}",
            catalog.schema_version
        );
    }
    validate_catalog_links(&catalog)?;
    Ok(catalog)
}

pub fn read_cached_catalog(cache_path: &Path) -> Result<Catalog> {
    let text = std::fs::read_to_string(cache_path).context("reading catalog cache")?;
    let catalog: Catalog = serde_json::from_str(&text).context("parsing cached catalog JSON")?;
    if catalog.schema_version != 1 {
        bail!(
            "unsupported cached plugin catalog schema_version {}",
            catalog.schema_version
        );
    }
    validate_catalog_links(&catalog)?;
    Ok(catalog)
}

pub fn require_preview_sha256(expected: Option<&str>, actual: &str) -> Result<()> {
    if expected.is_some_and(|expected| !actual.eq_ignore_ascii_case(expected)) {
        bail!("catalog package changed since preview; review the current package again");
    }
    Ok(())
}

fn validate_catalog_links(catalog: &Catalog) -> Result<()> {
    for plugin in &catalog.plugins {
        for (label, link) in [
            ("release_url", plugin.release_url.as_deref()),
            ("changelog_url", plugin.changelog_url.as_deref()),
        ] {
            let Some(link) = link else { continue };
            let parsed = url::Url::parse(link).map_err(|error| {
                anyhow!("catalog {} for '{}' is invalid: {error}", label, plugin.id)
            })?;
            if parsed.scheme() != "https" || parsed.host_str().is_none() {
                bail!("catalog {} for '{}' must be an HTTPS URL", label, plugin.id);
            }
        }
    }
    Ok(())
}

pub fn write_cached_catalog(cache_path: &Path, catalog: &Catalog) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).context("creating catalog cache directory")?;
    }
    let data = serde_json::to_string_pretty(catalog).context("serializing catalog for cache")?;
    let tmp = cache_path.with_extension("tmp");
    std::fs::write(&tmp, data).context("writing catalog cache tempfile")?;
    std::fs::rename(&tmp, cache_path).context("committing catalog cache")?;
    Ok(())
}

pub async fn load_catalog(
    client: Option<&reqwest::Client>,
    cache_path: Option<&Path>,
    force_refresh: bool,
) -> Result<Catalog> {
    if force_refresh {
        if let (Some(client), Some(cache_path)) = (client, cache_path) {
            match fetch_remote_catalog(client, DEFAULT_CATALOG_URL).await {
                Ok(remote) => {
                    let _ = write_cached_catalog(cache_path, &remote);
                    return Ok(remote);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "remote catalog refresh failed; falling back to cache/snapshot");
                }
            }
        }
    }

    if let Some(cache_path) = cache_path {
        if cache_path.exists() {
            if let Ok(cached) = read_cached_catalog(cache_path) {
                return Ok(cached);
            }
        }
    }

    if let (Some(client), Some(cache_path)) = (client, cache_path) {
        if let Ok(remote) = fetch_remote_catalog(client, DEFAULT_CATALOG_URL).await {
            let _ = write_cached_catalog(cache_path, &remote);
            return Ok(remote);
        }
    }

    embedded_catalog()
}

pub fn filter_catalog<'a>(
    plugins: &'a [CatalogPlugin],
    query: Option<&str>,
    capability: Option<&str>,
) -> Vec<&'a CatalogPlugin> {
    let q = query
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let cap = capability
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    plugins
        .iter()
        .filter(|p| {
            if let Some(ref cap_filter) = cap {
                if !p
                    .capabilities
                    .iter()
                    .any(|c| c.to_ascii_lowercase() == *cap_filter)
                {
                    return false;
                }
            }
            if let Some(ref text) = q {
                let id_match = p.id.to_ascii_lowercase().contains(text);
                let name_match = p.name.to_ascii_lowercase().contains(text);
                let desc_match = p.description.to_ascii_lowercase().contains(text);
                let pub_match = p.publisher.to_ascii_lowercase().contains(text);
                if !id_match && !name_match && !desc_match && !pub_match {
                    return false;
                }
            }
            true
        })
        .collect()
}

pub fn find_plugin_in_catalog<'a>(catalog: &'a Catalog, id: &str) -> Option<&'a CatalogPlugin> {
    catalog.plugins.iter().find(|plugin| plugin.id == id)
}

pub fn find_plugin(id: &str) -> Result<CatalogPlugin> {
    embedded_catalog()?
        .plugins
        .into_iter()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| anyhow!("catalog plugin '{id}' not found"))
}

pub fn trusted_keys(store: &PublisherTrustStore, plugin: &CatalogPlugin) -> Result<Vec<[u8; 32]>> {
    let Some(distribution) = &plugin.distribution else {
        return Ok(Vec::new());
    };
    let mut keys = Vec::new();
    for publisher in store
        .publishers
        .iter()
        .filter(|p| p.id == distribution.publisher_key_id && p.enabled)
    {
        if publisher.publisher != plugin.publisher {
            bail!(
                "publisher key '{}' belongs to '{}' but catalog plugin '{}' declares '{}'",
                publisher.id,
                publisher.publisher,
                plugin.id,
                plugin.publisher
            );
        }
        if publisher.algorithm != "ed25519" {
            bail!(
                "publisher key '{}' uses unsupported algorithm '{}'",
                publisher.id,
                publisher.algorithm
            );
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(publisher.public_key_base64.trim())
            .context("decoding trusted publisher key")?;
        let key: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow!("trusted publisher Ed25519 key must be exactly 32 bytes"))?;
        keys.push(key);
    }
    Ok(keys)
}

pub fn trusted_key(
    store: &PublisherTrustStore,
    plugin: &CatalogPlugin,
) -> Result<Option<[u8; 32]>> {
    Ok(trusted_keys(store, plugin)?.into_iter().next())
}

pub fn install_ready(plugin: &CatalogPlugin, store: &PublisherTrustStore) -> Result<bool> {
    if !plugin.installable {
        return Ok(false);
    }
    let Some(distribution) = &plugin.distribution else {
        return Ok(false);
    };
    if distribution.url.is_empty()
        || distribution.sha256.len() != 64
        || distribution.publisher_key_id.is_empty()
        || distribution.allowed_hosts.is_empty()
    {
        return Ok(false);
    }
    Ok(!trusted_keys(store, plugin)?.is_empty())
}

pub fn validate_download_url(distribution: &CatalogDistribution, url: &url::Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("catalog plugin artifacts must use https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("catalog plugin artifact URLs may not contain userinfo");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("catalog plugin artifact URL has no host"))?
        .to_ascii_lowercase();

    // If allowed_hosts contains github.com or *.githubusercontent.com or objects.githubusercontent.com,
    // also permit release-assets.githubusercontent.com since GitHub Releases redirect (302) there.
    let matches_allowed = distribution
        .allowed_hosts
        .iter()
        .any(|allowed| crate::plugins::manifest::host_matches(allowed, &host))
        || (host == "release-assets.githubusercontent.com"
            && distribution.allowed_hosts.iter().any(|allowed| {
                allowed == "github.com"
                    || allowed == "objects.githubusercontent.com"
                    || crate::plugins::manifest::host_matches(
                        allowed,
                        "release-assets.githubusercontent.com",
                    )
            }));

    if !matches_allowed {
        bail!("catalog artifact host '{host}' is not in allowed_hosts");
    }
    Ok(())
}

/// Validate a generic HTTP/HTTPS package download URL.
pub fn validate_generic_download_url(url: &url::Url) -> Result<()> {
    if url.scheme() != "https" && url.scheme() != "http" {
        bail!("package url must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("package url may not contain userinfo");
    }
    if url.host_str().is_none() {
        bail!("package url has no host");
    }
    Ok(())
}

/// Download a package from a generic HTTP/HTTPS URL with redirect handling and size bounds.
pub async fn download_package_from_url(
    client: &reqwest::Client,
    initial_url: &str,
) -> Result<Vec<u8>> {
    let mut url =
        url::Url::parse(initial_url.trim()).map_err(|e| anyhow!("invalid package url: {e}"))?;

    for redirect_count in 0..=5 {
        validate_generic_download_url(&url)?;

        let mut response = client
            .get(url.clone())
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| anyhow!("package download failed: {e}"))?;

        if response.status().is_redirection() {
            if redirect_count == 5 {
                bail!("package download exceeded redirect limit");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow!("package download redirect has no valid Location header"))?;
            url = url
                .join(location)
                .map_err(|e| anyhow!("invalid package redirect URL: {e}"))?;
            continue;
        }

        if !response.status().is_success() {
            bail!("downloading package returned HTTP {}", response.status());
        }

        if response
            .content_length()
            .is_some_and(|len| len > crate::plugins::package::MAX_PACKAGE_BYTES)
        {
            bail!("package exceeds size limit");
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| anyhow!("reading package stream failed: {e}"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > crate::plugins::package::MAX_PACKAGE_BYTES
            {
                bail!("package exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }

    bail!("package download failed")
}

/// Compare two version strings to determine if `latest` is strictly newer than `installed`.
/// Uses Semantic Versioning if both strings are valid semver, falling back to equality check.
pub fn is_update_available(installed: &str, latest: &str) -> bool {
    let inst_clean = installed.trim().trim_start_matches('v');
    let lat_clean = latest.trim().trim_start_matches('v');

    match (
        semver::Version::parse(inst_clean),
        semver::Version::parse(lat_clean),
    ) {
        (Ok(inst_ver), Ok(lat_ver)) => lat_ver > inst_ver,
        _ => inst_clean != lat_clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_metadata_is_valid() {
        let catalog = embedded_catalog().unwrap();
        let trust = embedded_trust_store().unwrap();
        assert_eq!(catalog.schema_version, 1);
        assert_eq!(trust.schema_version, 1);
        assert!(catalog
            .plugins
            .iter()
            .any(|plugin| plugin.id == "dev.kinetix.antigravity-oauth"));
    }

    #[test]
    fn official_catalog_entries_are_install_ready() {
        let catalog = embedded_catalog().unwrap();
        let trust = embedded_trust_store().unwrap();
        assert_eq!(catalog.schema_version, 1);
        for id in [
            "dev.kinetix.antigravity-oauth",
            "dev.kinetix.claude-code-oauth",
            "dev.kinetix.opencode-free",
        ] {
            let plugin = find_plugin(id).unwrap();
            assert!(
                install_ready(&plugin, &trust).unwrap(),
                "plugin {} should be install_ready",
                id
            );
        }
    }

    #[test]
    fn catalog_install_rejects_a_package_changed_since_preview() {
        let actual = "a1".repeat(32);
        assert!(require_preview_sha256(Some(&actual.to_uppercase()), &actual).is_ok());
        assert!(require_preview_sha256(Some(&"b2".repeat(32)), &actual).is_err());
        assert!(require_preview_sha256(None, &actual).is_ok());
    }

    #[test]
    fn download_url_requires_https_and_allowlisted_host() {
        let distribution = CatalogDistribution {
            url: "https://github.com/example/plugin.kxp".into(),
            sha256: "0".repeat(64),
            publisher_key_id: "test".into(),
            allowed_hosts: vec!["github.com".into(), "*.githubusercontent.com".into()],
        };
        validate_download_url(
            &distribution,
            &url::Url::parse("https://github.com/example/plugin.kxp").unwrap(),
        )
        .unwrap();
        validate_download_url(
            &distribution,
            &url::Url::parse("https://release-assets.githubusercontent.com/example/plugin.kxp")
                .unwrap(),
        )
        .unwrap();
        assert!(validate_download_url(
            &distribution,
            &url::Url::parse("http://github.com/example/plugin.kxp").unwrap(),
        )
        .is_err());
        assert!(validate_download_url(
            &distribution,
            &url::Url::parse("https://evil.example/plugin.kxp").unwrap(),
        )
        .is_err());
        assert!(validate_download_url(
            &distribution,
            &url::Url::parse("https://nested.sub.githubusercontent.com/plugin.kxp").unwrap(),
        )
        .is_err());
        assert!(validate_download_url(
            &distribution,
            &url::Url::parse("https://user:pass@github.com/plugin.kxp").unwrap(),
        )
        .is_err());
    }

    #[test]
    fn generic_url_validation_and_update_check() {
        assert!(validate_generic_download_url(
            &url::Url::parse("https://example.com/p.kxp").unwrap()
        )
        .is_ok());
        assert!(validate_generic_download_url(
            &url::Url::parse("http://example.com/p.kxp").unwrap()
        )
        .is_ok());
        assert!(validate_generic_download_url(
            &url::Url::parse("ftp://example.com/p.kxp").unwrap()
        )
        .is_err());
        assert!(validate_generic_download_url(
            &url::Url::parse("https://user:pass@example.com/p.kxp").unwrap()
        )
        .is_err());

        // Semver comparisons
        assert!(is_update_available("0.1.0", "0.1.1"));
        assert!(is_update_available("0.1.2", "0.2.0"));
        assert!(is_update_available("v0.1.2", "v0.1.3"));
        assert!(!is_update_available("0.1.2", "0.1.2"));
        // Installed is newer than catalog (should not be considered an update)
        assert!(!is_update_available("0.1.3", "0.1.2"));
        assert!(!is_update_available("1.0.0", "0.9.0"));
    }
}

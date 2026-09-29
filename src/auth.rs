//! Authentication for both surfaces.
//!
//! - Public API: virtual keys (`Authorization: Bearer` for OpenAI clients,
//!   `x-api-key` for Anthropic clients, FR-3.5).
//! - Admin API/dashboard: env admin token -> signed session cookie, plus an
//!   optional Cloudflare Access JWT check (defense in depth, NFR-3.2).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum_extra::extract::cookie::CookieJar;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::app::AppState;
use crate::crypto;
use crate::db::{self, VirtualKeyRow};
use crate::types::ProxyError;

pub const SESSION_COOKIE: &str = "kinetix_admin";

const PLUGIN_AUTH_TTL: Duration = Duration::from_secs(10 * 60);

/// One-time browser authorization session for a plugin-provided account flow.
/// Authorization codes, PKCE verifiers, and CSRF state are intentionally
/// in-memory only and disappear on restart.
#[derive(Clone, Debug)]
pub struct PluginAuthSession {
    pub plugin_id: String,
    pub flow_name: String,
    pub integration_id: String,
    pub provider_id: String,
    pub credential_binding: String,
    pub redirect_uri: String,
    pub pkce_verifier: String,
    /// Opaque admin session token that initiated the flow. The browser callback
    /// must present the same session, so a leaked `state` alone cannot complete
    /// account enrollment.
    pub(crate) initiator: String,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
pub struct PluginAuthStatus {
    pub result: String,
    pub provider_id: Option<String>,
}

#[derive(Clone, Debug)]
struct CompletedPluginAuth {
    initiator: String,
    status: PluginAuthStatus,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
pub struct PluginAuthStart {
    pub state: String,
    pub pkce_challenge: String,
}

pub struct PluginAuthSessions {
    inner: Mutex<HashMap<String, PluginAuthSession>>,
    completed: Mutex<HashMap<String, CompletedPluginAuth>>,
}

impl PluginAuthSessions {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            completed: Mutex::new(HashMap::new()),
        }
    }

    pub fn create(
        &self,
        plugin_id: &str,
        flow_name: &str,
        integration_id: &str,
        provider_id: &str,
        credential_binding: &str,
        redirect_uri: &str,
        initiator: &str,
    ) -> PluginAuthStart {
        use base64::Engine;
        use rand::RngCore;
        use sha2::{Digest, Sha256};

        let mut state_bytes = [0u8; 32];
        let mut verifier_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut state_bytes);
        rand::thread_rng().fill_bytes(&mut verifier_bytes);

        let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state_bytes);
        let pkce_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
        let pkce_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pkce_verifier.as_bytes()));

        let now = Instant::now();
        let mut map = self.inner.lock();
        map.retain(|_, session| session.expires_at > now);
        map.insert(
            state.clone(),
            PluginAuthSession {
                plugin_id: plugin_id.to_string(),
                flow_name: flow_name.to_string(),
                integration_id: integration_id.to_string(),
                provider_id: provider_id.to_string(),
                credential_binding: credential_binding.to_string(),
                redirect_uri: redirect_uri.to_string(),
                pkce_verifier,
                initiator: initiator.to_string(),
                expires_at: now + PLUGIN_AUTH_TTL,
            },
        );

        PluginAuthStart {
            state,
            pkce_challenge,
        }
    }

    pub fn peek(&self, state: &str, initiator: &str) -> Option<PluginAuthSession> {
        let now = Instant::now();
        let mut map = self.inner.lock();
        map.retain(|_, session| session.expires_at > now);
        let session = map.get(state)?.clone();
        if !crate::crypto::constant_time_eq(&session.initiator, initiator) {
            return None;
        }
        Some(session)
    }

    pub fn status(&self, state: &str, initiator: &str) -> Option<PluginAuthStatus> {
        if self.peek(state, initiator).is_some() {
            return Some(PluginAuthStatus {
                result: "pending".into(),
                provider_id: None,
            });
        }

        let now = Instant::now();
        let mut completed = self.completed.lock();
        completed.retain(|_, entry| entry.expires_at > now);
        let entry = completed.get(state)?;
        if !crate::crypto::constant_time_eq(&entry.initiator, initiator) {
            return None;
        }
        Some(entry.status.clone())
    }

    pub fn record_completion(
        &self,
        state: &str,
        initiator: &str,
        result: &str,
        provider_id: Option<String>,
    ) {
        let now = Instant::now();
        let mut completed = self.completed.lock();
        completed.retain(|_, entry| entry.expires_at > now);
        completed.insert(
            state.to_string(),
            CompletedPluginAuth {
                initiator: initiator.to_string(),
                status: PluginAuthStatus {
                    result: result.to_string(),
                    provider_id,
                },
                expires_at: now + PLUGIN_AUTH_TTL,
            },
        );
    }

    /// Consume a state token exactly once. The presented `initiator` must match
    /// the admin session that started the flow; a mismatch consumes the state
    /// (so it cannot be retried) and returns `None`.
    pub fn take(&self, state: &str, initiator: &str) -> Option<PluginAuthSession> {
        let now = Instant::now();
        let mut map = self.inner.lock();
        map.retain(|_, session| session.expires_at > now);
        let session = map.remove(state)?;
        if session.expires_at <= now {
            return None;
        }
        // Constant-time compare so a timing oracle cannot recover the session
        // token by probing the callback.
        if !crate::crypto::constant_time_eq(&session.initiator, initiator) {
            return None;
        }
        Some(session)
    }

    /// Consume one-time state for explicitly approved loopback OAuth callbacks
    /// when browser cookie isolation prevents the initiating admin cookie from
    /// crossing between equivalent loopback hosts.
    ///
    /// This deliberately does not generalize state-only callbacks. Each plugin
    /// allowed here must have a host-generated redirect with a narrowly checked
    /// loopback host and callback path.
    pub fn take_loopback_callback(&self, state: &str) -> Option<PluginAuthSession> {
        let now = Instant::now();
        let mut map = self.inner.lock();
        map.retain(|_, session| session.expires_at > now);
        let session = map.remove(state)?;
        if session.expires_at <= now {
            return None;
        }

        let redirect = url::Url::parse(&session.redirect_uri).ok()?;
        if redirect.scheme() != "http"
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return None;
        }

        let host = redirect.host_str();
        let allowed = match session.plugin_id.as_str() {
            "dev.kinetix.claude-code-oauth" => {
                host == Some("localhost") && redirect.path() == "/callback"
            }
            "dev.kinetix.antigravity-oauth" => {
                matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
                    && redirect.path() == "/callback"
            }
            _ => false,
        };

        allowed.then_some(session)
    }

    pub fn revoke(&self, state: &str) {
        self.inner.lock().remove(state);
    }
}

impl Default for PluginAuthSessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Setting key under which the admin password hash is stored in the database.
pub const ADMIN_PASSWORD_SETTING: &str = "admin_password_hash";

/// In-memory admin sessions. Sessions are deliberately **not** persisted: a
/// process restart drops them all, so a browser must log in again (a restart
/// must never silently preserve access). Each session is a random opaque token
/// with a TTL; `revoke_all` is used when the password changes.
pub struct Sessions {
    inner: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
}

impl Sessions {
    pub fn new(ttl_minutes: u64) -> Self {
        Sessions {
            inner: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_minutes.max(1) * 60),
        }
    }

    /// Create a new session token, sweeping expired ones first.
    pub fn create(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        let mut map = self.inner.lock();
        let now = Instant::now();
        map.retain(|_, exp| *exp > now);
        map.insert(token.clone(), now + self.ttl);
        token
    }

    /// Whether a token names a live, unexpired session.
    pub fn valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut map = self.inner.lock();
        match map.get(token) {
            Some(exp) if *exp > now => true,
            Some(_) => {
                map.remove(token);
                false
            }
            None => false,
        }
    }

    pub fn revoke(&self, token: &str) {
        self.inner.lock().remove(token);
    }

    /// Invalidate every session (used when the admin password changes).
    pub fn revoke_all(&self) {
        self.inner.lock().clear();
    }
}

/// The stored admin password hash, if any. When absent the configured token is
/// still accepted, which covers a pre-existing install that has not changed its
/// password yet.
pub async fn stored_admin_hash(state: &AppState) -> Option<String> {
    if let Ok(Some(h)) = db::get_setting(&state.pool, ADMIN_PASSWORD_SETTING).await {
        return Some(h);
    }
    // Fall back to the file written by `kinetix init`/`kinetix password set`
    // (the DB may be fresh or the setting not yet seeded).
    std::fs::read_to_string(state.config.paths.config_dir.join("admin_password.hash"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Verify a plaintext admin password against the stored hash (or, failing that,
/// the configured token). Constant-time.
pub async fn verify_admin_password(state: &AppState, password: &str) -> bool {
    let presented = crypto::hash_virtual_key(password);
    match stored_admin_hash(state).await {
        Some(stored) => crypto::constant_time_eq(&presented, &stored),
        None => crypto::constant_time_eq(
            &presented,
            &crypto::hash_virtual_key(&state.config.admin_token),
        ),
    }
}

/// Set (or change) the admin password, persist its hash, and invalidate every
/// existing session so the change takes effect immediately.
pub async fn set_admin_password(state: &AppState, password: &str) -> anyhow::Result<()> {
    if password.trim().len() < 8 {
        anyhow::bail!("admin password must be at least 8 characters");
    }
    db::set_setting(
        &state.pool,
        ADMIN_PASSWORD_SETTING,
        &crypto::hash_virtual_key(password.trim()),
    )
    .await?;
    state.sessions.revoke_all();
    Ok(())
}

/// Extract the presented virtual key from either auth style.
pub fn extract_virtual_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        if let Some(token) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            return Some(token.trim().to_string());
        }
    }
    if let Some(v) = headers.get("x-api-key").and_then(|h| h.to_str().ok()) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Look up the virtual key row for a presented key, checking the hash in
/// constant time (NFR-3.3).
pub async fn authenticate_virtual_key(
    state: &AppState,
    presented: &str,
) -> Result<VirtualKeyRow, ProxyError> {
    let hash = crypto::hash_virtual_key(presented);
    let row = db::get_virtual_key_by_hash(&state.pool, &hash)
        .await
        .map_err(|error| {
            tracing::error!(%error, "virtual key authentication query failed");
            ProxyError::unavailable("authentication temporarily unavailable")
        })?;
    match row {
        Some(row) if crypto::constant_time_eq(&row.key_hash, &hash) => Ok(row),
        _ => Err(ProxyError::unauthorized(
            "invalid or unknown API key. Provide a Kinetix virtual key (sk-kinetix-...).",
        )),
    }
}

/// The authenticated virtual key, if any, resolved from request headers.
pub struct AuthKey(pub Option<VirtualKeyRow>);

impl FromRequestParts<AppState> for AuthKey {
    type Rejection = ProxyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match extract_virtual_key(&parts.headers) {
            Some(presented) => {
                let row = authenticate_virtual_key(state, &presented).await?;
                Ok(AuthKey(Some(row)))
            }
            None => Ok(AuthKey(None)),
        }
    }
}

/// An authenticated admin session.
pub struct AdminAuth {
    pub actor: String,
    /// The raw credential presented by the caller (session cookie value, or the
    /// `x-kinetix-admin-token` header). Used to bind one-time browser flows to
    /// the admin session that started them.
    pub token: String,
}

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = ProxyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Cloudflare Access JWT (if configured).
        if let Some(aud) = &state.config.cf_access_aud {
            let jwt = parts
                .headers
                .get("cf-access-jwt-assertion")
                .and_then(|h| h.to_str().ok());
            match jwt {
                Some(token) => {
                    validate_cf_access(state, token, aud)
                        .map_err(|e| ProxyError::new(crate::types::ErrorKind::Forbidden, e))?;
                }
                None => {
                    return Err(ProxyError::new(
                        crate::types::ErrorKind::Forbidden,
                        "missing Cloudflare Access assertion",
                    ))
                }
            }
        }

        // Session cookie or direct admin token header.
        let jar = CookieJar::from_headers(&parts.headers);
        let cookie_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_string());
        let header_token = parts
            .headers
            .get("x-kinetix-admin-token")
            .and_then(|h| h.to_str().ok())
            .map(String::from);

        // The cookie must carry a live session token (sessions are in-memory,
        // so a restart invalidates them). The header may additionally carry the
        // raw admin password for CLI/curl use (NFR-3.14).
        if let Some(t) = cookie_token {
            if state.sessions.valid(&t) {
                return Ok(AdminAuth {
                    actor: "admin".to_string(),
                    token: t,
                });
            }
        }
        if let Some(t) = header_token {
            if state.sessions.valid(&t) || verify_admin_password(state, &t).await {
                return Ok(AdminAuth {
                    actor: "admin".to_string(),
                    token: t,
                });
            }
        }
        Err(ProxyError::unauthorized("admin authentication required"))
    }
}

fn validate_cf_access(state: &AppState, token: &str, aud: &str) -> Result<(), String> {
    use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

    let header = decode_header(token).map_err(|e| format!("invalid Access token: {e}"))?;
    let team = state
        .config
        .cf_access_team_domain
        .clone()
        .ok_or_else(|| "Cloudflare Access team domain not configured".to_string())?;

    // Fetch the JWKS (cached by reqwest's connection pool; the dashboard is
    // low-traffic so a per-request fetch is acceptable and always fresh).
    let jwks_url = format!("https://{team}/cdn-cgi/access/certs");
    let jwks: serde_json::Value = reqwest::blocking::get(&jwks_url)
        .map_err(|e| format!("failed to fetch Access certs: {e}"))?
        .json()
        .map_err(|e| format!("invalid Access certs: {e}"))?;

    let kid = header
        .kid
        .ok_or_else(|| "Access token missing kid".to_string())?;
    let keys = jwks
        .get("keys")
        .and_then(|k| k.as_array())
        .ok_or_else(|| "Access certs missing keys".to_string())?;
    let jwk = keys
        .iter()
        .find(|k| k.get("kid").and_then(|x| x.as_str()) == Some(kid.as_str()))
        .ok_or_else(|| "no matching Access key".to_string())?;

    let n = jwk.get("n").and_then(|x| x.as_str()).ok_or("missing n")?;
    let e = jwk.get("e").and_then(|x| x.as_str()).ok_or("missing e")?;
    let key = DecodingKey::from_rsa_components(n, e).map_err(|e| e.to_string())?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[aud]);
    decode::<serde_json::Value>(token, &key, &validation)
        .map(|_| ())
        .map_err(|e| format!("Access token rejected: {e}"))
}

#[cfg(test)]
mod plugin_auth_tests {
    use super::*;

    #[test]
    fn plugin_auth_state_is_random_and_one_time() {
        let sessions = PluginAuthSessions::new();
        let first = sessions.create(
            "dev.example.plugin",
            "login",
            "integration-login",
            "prov_1",
            "plugin:dev.example.plugin/login-credential",
            "https://example.test/admin/api/plugins/auth/callback",
            "session-token-1",
        );
        let second = sessions.create(
            "dev.example.plugin",
            "login",
            "integration-login",
            "prov_1",
            "plugin:dev.example.plugin/login-credential",
            "https://example.test/admin/api/plugins/auth/callback",
            "session-token-1",
        );

        assert_ne!(first.state, second.state);
        assert_ne!(first.pkce_challenge, second.pkce_challenge);
        // A different admin session cannot consume the state, and the failed
        // attempt burns it.
        assert!(sessions.take(&first.state, "other-session").is_none());
        assert!(sessions.take(&first.state, "session-token-1").is_none());
        let session = sessions
            .take(&second.state, "session-token-1")
            .expect("matching session should consume state");
        assert_eq!(session.plugin_id, "dev.example.plugin");
        assert_eq!(session.flow_name, "login");
        assert_eq!(session.integration_id, "integration-login");
        assert_eq!(session.provider_id, "prov_1");
        assert!(!session.pkce_verifier.is_empty());
        assert!(sessions.take(&second.state, "session-token-1").is_none());
    }

    #[test]
    fn revoked_plugin_auth_state_cannot_be_consumed() {
        let sessions = PluginAuthSessions::new();
        let pending = sessions.create(
            "dev.example.plugin",
            "login",
            "integration-login",
            "prov_1",
            "plugin:dev.example.plugin/login-credential",
            "https://example.test/callback",
            "session-token-1",
        );
        sessions.revoke(&pending.state);
        assert!(sessions.take(&pending.state, "session-token-1").is_none());
    }

    #[test]
    fn claude_loopback_state_can_cross_loopback_cookie_hosts_once() {
        let sessions = PluginAuthSessions::new();
        let pending = sessions.create(
            "dev.kinetix.claude-code-oauth",
            "claude-code",
            "claude-code",
            "prov_1",
            "plugin:dev.kinetix.claude-code-oauth/claude-code-oauth",
            "http://localhost:20128/callback",
            "session-token-from-127-0-0-1",
        );

        let session = sessions
            .take_loopback_callback(&pending.state)
            .expect("Claude localhost callback should consume its one-time state");
        assert_eq!(session.plugin_id, "dev.kinetix.claude-code-oauth");
        assert_eq!(session.redirect_uri, "http://localhost:20128/callback");
        assert!(sessions.take_loopback_callback(&pending.state).is_none());
    }

    #[test]
    fn antigravity_loopback_state_can_cross_loopback_cookie_hosts_once() {
        let sessions = PluginAuthSessions::new();
        let pending = sessions.create(
            "dev.kinetix.antigravity-oauth",
            "antigravity",
            "antigravity",
            "prov_1",
            "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth",
            "http://127.0.0.1:20128/callback",
            "session-token-from-localhost",
        );

        let session = sessions
            .take_loopback_callback(&pending.state)
            .expect("Antigravity loopback callback should consume its one-time state");
        assert_eq!(session.plugin_id, "dev.kinetix.antigravity-oauth");
        assert_eq!(session.redirect_uri, "http://127.0.0.1:20128/callback");
        assert!(sessions.take_loopback_callback(&pending.state).is_none());
    }

    #[test]
    fn state_only_callback_is_rejected_for_unapproved_plugin_or_wrong_redirect() {
        let sessions = PluginAuthSessions::new();
        let generic = sessions.create(
            "dev.example.plugin",
            "login",
            "integration-login",
            "prov_1",
            "plugin:dev.example.plugin/login-credential",
            "http://localhost:20128/callback",
            "session-token-1",
        );
        assert!(sessions.take_loopback_callback(&generic.state).is_none());

        let wrong_redirect = sessions.create(
            "dev.kinetix.claude-code-oauth",
            "claude-code",
            "claude-code",
            "prov_1",
            "plugin:dev.kinetix.claude-code-oauth/claude-code-oauth",
            "http://127.0.0.1:20128/callback",
            "session-token-1",
        );
        assert!(sessions
            .take_loopback_callback(&wrong_redirect.state)
            .is_none());

        let antigravity_remote_redirect = sessions.create(
            "dev.kinetix.antigravity-oauth",
            "antigravity",
            "antigravity",
            "prov_1",
            "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth",
            "http://example.test:20128/admin/api/plugins/auth/callback",
            "session-token-1",
        );
        assert!(sessions
            .take_loopback_callback(&antigravity_remote_redirect.state)
            .is_none());
    }
}

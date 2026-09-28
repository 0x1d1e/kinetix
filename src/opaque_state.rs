//! Host-owned opaque provider continuation-state subsystem.
//!
//! Some upstream providers hand back an opaque continuation token alongside a
//! tool/function call that must be replayed verbatim on the next turn (e.g.
//! Gemini's `thoughtSignature`). Kinetix's canonical `Part::ToolCall.signature`
//! and `StreamEvent::ToolCallStart.signature` slots already round-trip this
//! value *inside a single request/response pair*, but a translated client
//! protocol (OpenAI Chat Completions, OpenAI Responses, Anthropic Messages)
//! has no field guaranteed to carry it back on the client's next request.
//!
//! This module is the persistence/recovery layer that closes that gap. It:
//!
//!   * stores the exact opaque value under the **client-visible** tool-call id
//!     (after `ToolStreamState` normalization), scoped by client identity so
//!     one virtual key can never resolve another's continuation state;
//!   * encrypts the value at rest under a dedicated derived cipher
//!     (`Crypto::encrypt_opaque_state` / `decrypt_opaque_state`);
//!   * enforces provider/family/producer/session/tool-name compatibility
//!     before ever handing a stored value back to a pipeline caller;
//!   * never invents, merges, or copies a signature between unrelated tool
//!     calls or provider families.
//!
//! Routing/portability *policy* (reject vs strip-with-warning) is decided by
//! `src/pipeline.rs`, not here. This module only answers "is there compatible
//! stored state for this tool call?" — it never chooses whether to use it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::mpsc;

use crate::crypto::Crypto;
use crate::db::Pool;

// ---------------------------------------------------------------------------
// Kind / target / lookup / record types (internal data-plane state only; none
// of this is exposed through the public HTTP/admin API).
// ---------------------------------------------------------------------------

/// The provider protocol family an opaque continuation value belongs to.
/// Only one variant exists today; the enum exists so future opaque-state
/// kinds (should another provider need one) do not need a schema migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpaqueStateKind {
    GeminiThoughtSignature,
}

impl OpaqueStateKind {
    fn as_str(&self) -> &'static str {
        match self {
            OpaqueStateKind::GeminiThoughtSignature => "gemini_thought_signature",
        }
    }
}

/// What an adapter/target is prepared to produce and accept opaque state as.
/// Compatibility between a stored record and a candidate target requires an
/// exact match on `provider_id`, `family`, `producer`, and the originating
/// model id (§6, §41).
///
/// The model id is part of the key deliberately. Google's `generateContent`
/// contract only guarantees a thought signature is accepted by the model that
/// produced it; switching models is documented to require dummy signatures,
/// not reuse of the original one. Cross-model restoration is therefore not
/// assumed until it has real acceptance evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueStateTarget {
    pub kind: OpaqueStateKind,
    pub provider_id: String,
    /// The opaque-state protocol family (e.g. `"gemini"`); a coarse
    /// provenance tag retained for diagnostics and future protocol variants.
    /// It is not sufficient on its own to authorize restoration.
    pub family: String,
    /// Adapter encoding-version provenance, e.g. `"native:gemini:v1"` or
    /// `"plugin:<plugin-id>@<version>/<capability>"` (§41). A version bump
    /// intentionally invalidates old rows rather than risk misinterpreting
    /// them.
    pub producer: String,
    /// Exact upstream model id that produced the state (e.g.
    /// `"gemini-2.5-flash"`). State captured on one model is never restored
    /// onto a different model.
    pub model_id: String,
}

/// A resolved client scope: identifies *who* is allowed to read back opaque
/// state. Ordinary API traffic scopes by virtual-key id; admin/internal
/// testing without a virtual key uses the fixed `"internal"` scope (§9).
#[derive(Debug, Clone)]
pub struct OpaqueClientScope(String);

impl OpaqueClientScope {
    pub fn for_key(key_id: &str) -> Self {
        OpaqueClientScope(key_id.to_string())
    }

    pub fn internal() -> Self {
        OpaqueClientScope("internal".to_string())
    }

    fn hash(&self) -> String {
        sha256_hex(self.0.as_bytes())
    }
}

/// Outcome of a stored-state lookup. Never returns the underlying DB record —
/// only the information the pipeline needs to hydrate or reject (§38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpaqueLookupResult {
    /// No stored state exists for this tool-call id (or it expired).
    Missing,
    /// Compatible stored state exists; here is the exact opaque value.
    Compatible(String),
    /// Stored state exists but targets a different provider/family/producer.
    Incompatible,
    /// Stored state exists but was captured under a different, conflicting
    /// session identity (§10).
    SessionMismatch,
    /// Stored state exists but was captured for a different tool name (§20).
    ToolNameMismatch,
    /// Stored state exists but could not be decrypted (crypto/key mismatch,
    /// or corrupt row); treated as unusable, never surfaced to the client
    /// verbatim (§19 case D).
    Unavailable,
}

// ---------------------------------------------------------------------------
// Hashing helpers (§8, §9): raw client tool-call ids and raw session values
// are never persisted, only SHA-256 hashes scoped by client identity.
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn tool_call_hash(scope: &OpaqueClientScope, tool_call_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope.0.as_bytes());
    hasher.update(b"\0");
    hasher.update(tool_call_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn session_hash(session: &str) -> String {
    sha256_hex(session.as_bytes())
}

fn tool_name_hash(tool_name: &str) -> String {
    sha256_hex(tool_name.as_bytes())
}

// ---------------------------------------------------------------------------
// Retention constants (§12).
// ---------------------------------------------------------------------------

const MEMORY_TTL: Duration = Duration::from_secs(60 * 60);
const PERSISTENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_MEMORY_ROWS: usize = 2_048;
const MAX_DB_ROWS: i64 = 10_000;
const PRUNE_EVERY_N_WRITES: u64 = 100;
/// Bound on queued SQLite durability jobs. The RAM cache is the synchronous
/// hot path; when this queue saturates, writes are dropped and counted rather
/// than blocking the streaming response (§13, §63).
const DURABILITY_QUEUE_CAPACITY: usize = 4_096;

// ---------------------------------------------------------------------------
// In-memory hot-path cache (§13, §40).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpaqueCacheKey {
    kind: &'static str,
    scope_hash: String,
    tool_call_hash: String,
    provider_id: String,
    family: String,
    producer: String,
    model_id: String,
}

#[derive(Debug, Clone)]
struct CachedOpaqueState {
    signature: String,
    session_hash: Option<String>,
    tool_name_hash: String,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct CachedIdentityEvidence {
    any: bool,
    tool_name_match: bool,
    session_compatible: bool,
}

// ---------------------------------------------------------------------------
// Observability counters (§36). No secrets, ids, or tool names — coarse
// outcome counts only.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Counters {
    capture_stored: std::sync::atomic::AtomicU64,
    capture_replaced: std::sync::atomic::AtomicU64,
    capture_dropped: std::sync::atomic::AtomicU64,
    capture_storage_error: std::sync::atomic::AtomicU64,
    lookup_hit: std::sync::atomic::AtomicU64,
    lookup_miss: std::sync::atomic::AtomicU64,
    lookup_expired: std::sync::atomic::AtomicU64,
    lookup_incompatible: std::sync::atomic::AtomicU64,
    lookup_session_mismatch: std::sync::atomic::AtomicU64,
    lookup_tool_name_mismatch: std::sync::atomic::AtomicU64,
    lookup_decrypt_error: std::sync::atomic::AtomicU64,
}

/// A point-in-time snapshot of opaque-state counters, safe to render on the
/// admin metrics surface (no secrets, no ids).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct OpaqueStateMetrics {
    pub capture_stored: u64,
    pub capture_replaced: u64,
    pub capture_dropped: u64,
    pub capture_storage_error: u64,
    pub lookup_hit: u64,
    pub lookup_miss: u64,
    pub lookup_expired: u64,
    pub lookup_incompatible: u64,
    pub lookup_session_mismatch: u64,
    pub lookup_tool_name_mismatch: u64,
    pub lookup_decrypt_error: u64,
    pub entries: u64,
}

// ---------------------------------------------------------------------------
// The store.
// ---------------------------------------------------------------------------

pub struct OpaqueStateStore {
    pool: Pool,
    crypto: Arc<Crypto>,
    cache: DashMap<OpaqueCacheKey, CachedOpaqueState>,
    /// Insertion order for bounded LRU-ish eviction (oldest first). Kept
    /// separate from the DashMap so eviction never needs to scan.
    order: Mutex<VecDeque<OpaqueCacheKey>>,
    counters: Arc<Counters>,
    durability: mpsc::Sender<DurabilityJob>,
    /// Bound on RAM-cache rows. A field (not just the const) so the eviction
    /// regression can exercise capacity without filling thousands of entries.
    max_memory_rows: usize,
}

/// A queued SQLite durability job. Encryption and the SQLite UPSERT happen on
/// the worker task, never inline in a streaming response, so a slow or locked
/// database cannot stall a tool-call event before the client receives it.
enum DurabilityJob {
    Write(Box<DurabilityWrite>),
    /// Ordering barrier: mpsc preserves order, so once the worker receives
    /// this it has processed every previously queued write. Used by tests and
    /// graceful shutdown, never on the request path.
    Flush(tokio::sync::oneshot::Sender<()>),
}

struct DurabilityWrite {
    kind: &'static str,
    scope_hash: String,
    tool_call_hash: String,
    session_hash: String,
    provider_id: String,
    family: String,
    producer: String,
    origin_model: String,
    tool_name_hash: String,
    signature: String,
}

impl OpaqueStateStore {
    pub fn new(pool: Pool, crypto: Arc<Crypto>) -> Self {
        let (tx, rx) = mpsc::channel::<DurabilityJob>(DURABILITY_QUEUE_CAPACITY);
        let counters = Arc::new(Counters::default());
        spawn_durability_worker(pool.clone(), crypto.clone(), rx, counters.clone());
        Self {
            pool,
            crypto,
            cache: DashMap::new(),
            order: Mutex::new(VecDeque::new()),
            counters,
            durability: tx,
            max_memory_rows: MAX_MEMORY_ROWS,
        }
    }

    #[cfg(test)]
    fn set_max_memory_rows_for_test(&mut self, rows: usize) {
        self.max_memory_rows = rows;
    }

    /// Wait until every queued durability write has been processed. Used by
    /// tests and graceful shutdown; never called on the request path.
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.durability.send(DurabilityJob::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    pub fn metrics(&self) -> OpaqueStateMetrics {
        use std::sync::atomic::Ordering::Relaxed;
        OpaqueStateMetrics {
            capture_stored: self.counters.capture_stored.load(Relaxed),
            capture_replaced: self.counters.capture_replaced.load(Relaxed),
            capture_dropped: self.counters.capture_dropped.load(Relaxed),
            capture_storage_error: self.counters.capture_storage_error.load(Relaxed),
            lookup_hit: self.counters.lookup_hit.load(Relaxed),
            lookup_miss: self.counters.lookup_miss.load(Relaxed),
            lookup_expired: self.counters.lookup_expired.load(Relaxed),
            lookup_incompatible: self.counters.lookup_incompatible.load(Relaxed),
            lookup_session_mismatch: self.counters.lookup_session_mismatch.load(Relaxed),
            lookup_tool_name_mismatch: self.counters.lookup_tool_name_mismatch.load(Relaxed),
            lookup_decrypt_error: self.counters.lookup_decrypt_error.load(Relaxed),
            entries: self.cache.len() as u64,
        }
    }

    /// Record an opaque continuation value under the client-visible tool-call
    /// id. The RAM cache is written synchronously so an immediate next request
    /// never races ahead of it (§39); the SQLite UPSERT/pruning is handed to a
    /// bounded background worker so a slow or locked database cannot stall the
    /// streaming response that carries the tool call (§13, §63). A saturated
    /// queue drops the durability write (counted) rather than blocking the
    /// data plane; a storage failure is likewise swallowed and counted.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_tool_signature(
        &self,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        session: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
        signature: &str,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        if signature.is_empty() || tool_call_id.is_empty() {
            return;
        }
        let scope_hash = scope.hash();
        let call_hash = tool_call_hash(scope, tool_call_id);
        let sess_hash = session.map(session_hash);
        let name_hash = tool_name_hash(tool_name);

        let key = self.cache_key(target, &scope_hash, &call_hash);
        let now = Instant::now();
        let replaced = self
            .cache
            .get(&key)
            .map(|existing| existing.signature != signature)
            .unwrap_or(false);

        self.cache.insert(
            key.clone(),
            CachedOpaqueState {
                signature: signature.to_string(),
                session_hash: sess_hash.clone(),
                tool_name_hash: name_hash.clone(),
                expires_at: now + MEMORY_TTL,
            },
        );
        self.touch_order(key);

        if replaced {
            self.counters.capture_replaced.fetch_add(1, Relaxed);
            tracing::warn!(kind = target.kind.as_str(), "opaque_state_replaced");
        }

        let job = DurabilityWrite {
            kind: target.kind.as_str(),
            scope_hash,
            tool_call_hash: call_hash,
            session_hash: sess_hash.unwrap_or_default(),
            provider_id: target.provider_id.clone(),
            family: target.family.clone(),
            producer: target.producer.clone(),
            origin_model: target.model_id.clone(),
            tool_name_hash: name_hash,
            signature: signature.to_string(),
        };
        if self
            .durability
            .try_send(DurabilityJob::Write(Box::new(job)))
            .is_err()
        {
            self.counters.capture_dropped.fetch_add(1, Relaxed);
            tracing::warn!(
                kind = target.kind.as_str(),
                "opaque_state_durability_queue_full"
            );
        }
    }

    /// Resolve stored state for a tool call.
    ///
    /// `capability` is the candidate target's declared opaque-state capability
    /// (from `Adapter::opaque_state_target`), or `None` when the target cannot
    /// carry opaque continuation state at all. Passing `None` still performs a
    /// lookup so the pipeline can distinguish "no stored state" from "stored
    /// state exists but this target cannot carry it" (§19 case C, §22): a row
    /// found under a target with no capability is reported as `Incompatible`.
    ///
    /// RAM is checked first. Exact-target lookup is O(1); after an exact miss,
    /// RAM is also inspected for same-call state captured under another model
    /// so immediate cross-model continuation cannot race asynchronous SQLite
    /// durability. SQLite remains the durable fallback and exact persisted
    /// target rows remain authoritative (§13). Never returns a raw DB record —
    /// only a caller-safe result (§38).
    pub async fn resolve_tool_signature(
        &self,
        scope: &OpaqueClientScope,
        capability: Option<&OpaqueStateTarget>,
        session: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
    ) -> OpaqueLookupResult {
        let scope_hash = scope.hash();
        let call_hash = tool_call_hash(scope, tool_call_id);

        // The exact target stays O(1) on the common path. An expired RAM entry
        // is evicted but must not short-circuit to Missing: MEMORY_TTL (1h) is
        // only the hot-cache lifetime, while SQLite retains rows for 24h.
        if let Some(target) = capability {
            let key = self.cache_key(target, &scope_hash, &call_hash);
            if let Some(entry) = self.cache.get(&key) {
                if entry.expires_at < Instant::now() {
                    drop(entry);
                    self.cache.remove(&key);
                    self.counters
                        .lookup_expired
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    return self.evaluate_cached(&entry, session, tool_name);
                }
            }
        }

        // Capture writes RAM synchronously and SQLite asynchronously. An
        // immediate continuation onto another model therefore cannot rely on
        // SQLite to notice that state exists. Scan only after the exact-target
        // O(1) miss and retain identity evidence from every live RAM row for
        // this client-visible tool-call id. A late exact-target insertion wins.
        let (late_exact, cached_evidence) =
            self.cached_identity_evidence(capability, &scope_hash, &call_hash, session, tool_name);
        if let Some(entry) = late_exact {
            return self.evaluate_cached(&entry, session, tool_name);
        }

        let rows = self.fetch_rows(&scope_hash, &call_hash).await;

        // A persistent row for the exact target model remains authoritative,
        // even if RAM already contains state for another model with the same
        // client-visible tool-call id.
        if let Some(target) = capability {
            if let Some(row) = rows.iter().find(|row| row.matches(target)) {
                if let Some(mismatch) = self.identity_mismatch(row, session, tool_name) {
                    return mismatch;
                }
                return self.restore_row(row, target, &scope_hash, &call_hash);
            }
        }

        // Cross-model (or capability-less) lookup. Merge synchronous RAM
        // evidence with durable rows so correctness never depends on the
        // background SQLite worker winning a race with the next request.
        let requested_tool = tool_name_hash(tool_name);
        let incoming_session = session.map(session_hash);
        let mut saw_any = cached_evidence.any;
        let mut saw_tool_name = cached_evidence.tool_name_match;
        let mut saw_compatible_session = cached_evidence.session_compatible;

        for row in &rows {
            saw_any = true;
            if row.tool_name_hash != requested_tool {
                continue;
            }
            saw_tool_name = true;
            if match (&row.session_hash, &incoming_session) {
                (Some(stored), Some(incoming)) => stored == incoming,
                _ => true,
            } {
                saw_compatible_session = true;
            }
        }

        if !saw_any {
            self.counters
                .lookup_miss
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::Missing;
        }
        if !saw_tool_name {
            self.counters
                .lookup_tool_name_mismatch
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::ToolNameMismatch;
        }
        if !saw_compatible_session {
            self.counters
                .lookup_session_mismatch
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::SessionMismatch;
        }

        self.counters
            .lookup_incompatible
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpaqueLookupResult::Incompatible
    }

    /// Validate a stored row's conversation identity: the same tool name and
    /// (when both sides carry one) the same session. Returns the mismatch
    /// result when the row cannot belong to this conversation, or `None` when
    /// it may. The client scope is already implied by the lookup key.
    fn identity_mismatch(
        &self,
        row: &StoredRow,
        session: Option<&str>,
        tool_name: &str,
    ) -> Option<OpaqueLookupResult> {
        if row.tool_name_hash != tool_name_hash(tool_name) {
            self.counters
                .lookup_tool_name_mismatch
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Some(OpaqueLookupResult::ToolNameMismatch);
        }
        if let (Some(stored), Some(incoming)) = (&row.session_hash, session) {
            if *stored != session_hash(incoming) {
                self.counters
                    .lookup_session_mismatch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some(OpaqueLookupResult::SessionMismatch);
            }
        }
        None
    }

    /// Decrypt a row already matched to `target` and repopulate the RAM hot path
    /// with it.
    fn restore_row(
        &self,
        row: &StoredRow,
        target: &OpaqueStateTarget,
        scope_hash: &str,
        call_hash: &str,
    ) -> OpaqueLookupResult {
        let signature = match self.crypto.decrypt_opaque_state(&row.value_enc) {
            Ok(sig) => sig,
            Err(_) => {
                self.counters
                    .lookup_decrypt_error
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Never surface crypto detail to the client; treat as
                // unavailable local state (§19 case D, §63).
                tracing::error!("opaque_state_decrypt_failed");
                return OpaqueLookupResult::Unavailable;
            }
        };

        let key = self.cache_key(target, scope_hash, call_hash);
        self.cache.insert(
            key.clone(),
            CachedOpaqueState {
                signature: signature.clone(),
                session_hash: row.session_hash.clone(),
                tool_name_hash: row.tool_name_hash.clone(),
                expires_at: Instant::now() + MEMORY_TTL,
            },
        );
        self.touch_order(key);

        self.counters
            .lookup_hit
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpaqueLookupResult::Compatible(signature)
    }

    fn cache_key(
        &self,
        target: &OpaqueStateTarget,
        scope_hash: &str,
        call_hash: &str,
    ) -> OpaqueCacheKey {
        OpaqueCacheKey {
            kind: target.kind.as_str(),
            scope_hash: scope_hash.to_string(),
            tool_call_hash: call_hash.to_string(),
            provider_id: target.provider_id.clone(),
            family: target.family.clone(),
            producer: target.producer.clone(),
            model_id: target.model_id.clone(),
        }
    }

    fn touch_order(&self, key: OpaqueCacheKey) {
        let mut order = self.order.lock();
        // Move-to-back: a re-captured key must not leave a stale occurrence at
        // the front. Otherwise, once the deque is at capacity, evicting that
        // stale occurrence would `cache.remove()` the *newly written* value and
        // silently drop a live signature (an immediate continuation would then
        // miss RAM). `retain` keeps `order` free of duplicates so one cache
        // entry always has exactly one eviction slot.
        order.retain(|existing| existing != &key);
        order.push_back(key);
        while order.len() > self.max_memory_rows {
            if let Some(evicted) = order.pop_front() {
                self.cache.remove(&evicted);
            }
        }
    }

    /// Summarize live RAM rows for one client-visible tool-call id after the
    /// exact-target fast path misses. This is the synchronous cross-model
    /// counterpart to fetch_rows(): it prevents portability decisions from
    /// depending on asynchronous SQLite durability.
    fn cached_identity_evidence(
        &self,
        capability: Option<&OpaqueStateTarget>,
        scope_hash: &str,
        call_hash: &str,
        session: Option<&str>,
        tool_name: &str,
    ) -> (Option<CachedOpaqueState>, CachedIdentityEvidence) {
        let exact_key = capability.map(|target| self.cache_key(target, scope_hash, call_hash));
        let requested_tool = tool_name_hash(tool_name);
        let incoming_session = session.map(session_hash);
        let now = Instant::now();
        let mut exact = None;
        let mut evidence = CachedIdentityEvidence::default();
        let mut expired = Vec::new();

        for entry in &self.cache {
            let key = entry.key();
            if key.scope_hash != scope_hash || key.tool_call_hash != call_hash {
                continue;
            }
            if entry.expires_at < now {
                expired.push(key.clone());
                continue;
            }

            if exact_key.as_ref().is_some_and(|expected| expected == key) {
                exact = Some(entry.value().clone());
                continue;
            }

            evidence.any = true;
            if entry.tool_name_hash != requested_tool {
                continue;
            }
            evidence.tool_name_match = true;
            if match (&entry.session_hash, &incoming_session) {
                (Some(stored), Some(incoming)) => stored == incoming,
                _ => true,
            } {
                evidence.session_compatible = true;
            }
        }

        for key in expired {
            if self.cache.remove(&key).is_some() {
                self.counters
                    .lookup_expired
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        (exact, evidence)
    }

    fn evaluate_cached(
        &self,
        entry: &CachedOpaqueState,
        session: Option<&str>,
        tool_name: &str,
    ) -> OpaqueLookupResult {
        // Tool-name validation (§20): a reused tool-call id bound to a
        // different tool name is malformed/reused conversation history.
        if entry.tool_name_hash != tool_name_hash(tool_name) {
            self.counters
                .lookup_tool_name_mismatch
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::ToolNameMismatch;
        }

        // Session compatibility (§10): both present and different => refuse.
        if let (Some(stored), Some(incoming)) = (&entry.session_hash, session) {
            if *stored != session_hash(incoming) {
                self.counters
                    .lookup_session_mismatch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return OpaqueLookupResult::SessionMismatch;
            }
        }

        self.counters
            .lookup_hit
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpaqueLookupResult::Compatible(entry.signature.clone())
    }

    /// Fetch every non-expired row for a tool-call id, newest first. The query
    /// is scoped by `scope_hash` + `tool_call_hash` only (not by provider/
    /// family/producer/model) so a target with no opaque-state capability can
    /// still observe that *some* non-portable continuation state exists for
    /// this tool call; the caller selects the compatible row.
    async fn fetch_rows(&self, scope_hash: &str, call_hash: &str) -> Vec<StoredRow> {
        let now = chrono::Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT kind, provider_id, family, producer, session_hash, tool_name_hash,
                    origin_model, value_enc, expires_at
             FROM opaque_provider_state
             WHERE scope_hash = ? AND tool_call_hash = ?
             ORDER BY created_at DESC",
        )
        .bind(scope_hash)
        .bind(call_hash)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter(|row| {
                let expires_at: String = row.get("expires_at");
                expires_at.as_str() >= now.as_str()
            })
            .map(|row| {
                let session_hash: String = row.get("session_hash");
                StoredRow {
                    kind: row.get("kind"),
                    provider_id: row.get("provider_id"),
                    family: row.get("family"),
                    producer: row.get("producer"),
                    session_hash: if session_hash.is_empty() {
                        None
                    } else {
                        Some(session_hash)
                    },
                    tool_name_hash: row.get("tool_name_hash"),
                    origin_model: row.get("origin_model"),
                    value_enc: row.get("value_enc"),
                }
            })
            .collect()
    }

    /// Delete expired rows, and if still over the row cap, delete the oldest
    /// rows above the limit (§12). Called lazily by the durability worker every
    /// `PRUNE_EVERY_N_WRITES` writes; kept public so tests can invoke it.
    pub async fn prune(&self) {
        prune_pool(&self.pool).await;
    }

    /// Row count, exposed for tests/diagnostics only.
    pub async fn count_rows(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) as n FROM opaque_provider_state")
            .fetch_one(&self.pool)
            .await
            .map(|row| row.get::<i64, _>("n"))
            .unwrap_or(0)
    }

    /// Drop the RAM cache only, leaving SQLite intact. Used by restart-
    /// durability tests to prove the SQLite fallback path works without a
    /// full process restart (§52).
    #[cfg(test)]
    pub fn clear_memory_cache_for_test(&self) {
        self.cache.clear();
        self.order.lock().clear();
    }

    #[cfg(test)]
    pub fn memory_entry_count_for_test(&self) -> usize {
        self.cache.len()
    }

    /// Test-only: read a value from the RAM cache only, never SQLite. Used by
    /// the eviction regression to observe the hot path deterministically
    /// (a SQLite read could race the asynchronous durability worker).
    #[cfg(test)]
    fn cached_signature_for_test(
        &self,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        tool_call_id: &str,
    ) -> Option<String> {
        let key = self.cache_key(target, &scope.hash(), &tool_call_hash(scope, tool_call_id));
        self.cache.get(&key).map(|entry| entry.signature.clone())
    }

    /// Test-only: age a single RAM entry past `MEMORY_TTL` without touching
    /// SQLite, so the entry is still present but expired — the exact shape of
    /// a long-running process where the 1h RAM TTL has lapsed but the 24h
    /// SQLite TTL has not. Used to prove RAM expiry falls through to the
    /// SQLite fallback instead of short-circuiting to `Missing`.
    #[cfg(test)]
    pub fn expire_memory_entry_for_test(
        &self,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        tool_call_id: &str,
    ) {
        let key = self.cache_key(target, &scope.hash(), &tool_call_hash(scope, tool_call_id));
        if let Some(mut entry) = self.cache.get_mut(&key) {
            entry.expires_at = Instant::now() - Duration::from_secs(1);
        }
    }

    /// Test-only: mark every stored row expired and drop the RAM cache so the
    /// next lookup exercises the expiry path deterministically.
    #[cfg(test)]
    pub async fn expire_all_for_test(&self) {
        // Flush first so the expiry UPDATE lands on the rows that were just
        // queued; otherwise the worker would persist them with a future
        // expiry after this returns.
        self.flush().await;
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let _ = sqlx::query("UPDATE opaque_provider_state SET expires_at = ?")
            .bind(past)
            .execute(&self.pool)
            .await;
        self.clear_memory_cache_for_test();
    }
}

/// A decrypted-adjacent row as read from SQLite (value still encrypted).
struct StoredRow {
    kind: String,
    provider_id: String,
    family: String,
    producer: String,
    session_hash: Option<String>,
    tool_name_hash: String,
    origin_model: String,
    value_enc: String,
}

impl StoredRow {
    fn matches(&self, target: &OpaqueStateTarget) -> bool {
        self.kind == target.kind.as_str()
            && self.provider_id == target.provider_id
            && self.family == target.family
            && self.producer == target.producer
            && self.origin_model == target.model_id
    }
}

// ---------------------------------------------------------------------------
// Durability worker: encryption + SQLite UPSERT + periodic pruning, entirely
// off the data plane.
// ---------------------------------------------------------------------------

fn spawn_durability_worker(
    pool: Pool,
    crypto: Arc<Crypto>,
    mut rx: mpsc::Receiver<DurabilityJob>,
    counters: Arc<Counters>,
) {
    tokio::spawn(async move {
        let mut writes: u64 = 0;
        while let Some(job) = rx.recv().await {
            match job {
                DurabilityJob::Write(job) => {
                    persist_opaque_state(&pool, &crypto, &counters, &job).await;
                    writes += 1;
                    if writes.is_multiple_of(PRUNE_EVERY_N_WRITES) {
                        prune_pool(&pool).await;
                    }
                }
                DurabilityJob::Flush(reply) => {
                    let _ = reply.send(());
                }
            }
        }
    });
}

async fn persist_opaque_state(
    pool: &Pool,
    crypto: &Crypto,
    counters: &Counters,
    job: &DurabilityWrite,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let value_enc = match crypto.encrypt_opaque_state(&job.signature) {
        Ok(enc) => enc,
        Err(_) => {
            counters.capture_storage_error.fetch_add(1, Relaxed);
            tracing::error!(kind = job.kind, "opaque_state_encrypt_failed");
            return;
        }
    };

    let created_at = crate::db::now_iso();
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::from_std(PERSISTENT_TTL).unwrap()).to_rfc3339();

    let write = sqlx::query(
        "INSERT INTO opaque_provider_state
            (kind, scope_hash, tool_call_hash, session_hash, provider_id, family,
             producer, origin_model, tool_name_hash, value_enc, created_at, expires_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT (kind, scope_hash, tool_call_hash, provider_id, family, producer, origin_model)
         DO UPDATE SET
            session_hash = excluded.session_hash,
            tool_name_hash = excluded.tool_name_hash,
            value_enc = excluded.value_enc,
            created_at = excluded.created_at,
            expires_at = excluded.expires_at",
    )
    .bind(job.kind)
    .bind(&job.scope_hash)
    .bind(&job.tool_call_hash)
    .bind(&job.session_hash)
    .bind(&job.provider_id)
    .bind(&job.family)
    .bind(&job.producer)
    .bind(&job.origin_model)
    .bind(&job.tool_name_hash)
    .bind(&value_enc)
    .bind(&created_at)
    .bind(&expires_at)
    .execute(pool)
    .await;

    match write {
        Ok(_) => {
            counters.capture_stored.fetch_add(1, Relaxed);
        }
        Err(error) => {
            counters.capture_storage_error.fetch_add(1, Relaxed);
            // Never crash the client response on a storage failure; the next
            // provider turn may fail naturally instead (§13, §63).
            tracing::error!(error = %error, "opaque_state_capture_failed");
        }
    }
}

async fn prune_pool(pool: &Pool) {
    let now = chrono::Utc::now().to_rfc3339();
    let _ = sqlx::query("DELETE FROM opaque_provider_state WHERE expires_at < ?")
        .bind(&now)
        .execute(pool)
        .await;

    if let Ok(row) = sqlx::query("SELECT COUNT(*) as n FROM opaque_provider_state")
        .fetch_one(pool)
        .await
    {
        let count: i64 = row.get("n");
        if count > MAX_DB_ROWS {
            let overflow = count - MAX_DB_ROWS;
            let _ = sqlx::query(
                "DELETE FROM opaque_provider_state WHERE rowid IN (
                    SELECT rowid FROM opaque_provider_state
                    ORDER BY created_at ASC
                    LIMIT ?
                )",
            )
            .bind(overflow)
            .execute(pool)
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> OpaqueStateStore {
        let dir = std::env::temp_dir().join(format!(
            "kinetix-opaque-state-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let crypto = Arc::new(Crypto::new(&[11u8; 32]));
        OpaqueStateStore::new(pool, crypto)
    }

    fn gemini_target() -> OpaqueStateTarget {
        OpaqueStateTarget {
            kind: OpaqueStateKind::GeminiThoughtSignature,
            provider_id: "ai-studio".into(),
            family: "gemini".into(),
            producer: "native:gemini:v1".into(),
            model_id: "gemini-3".into(),
        }
    }

    fn capture(
        store: &OpaqueStateStore,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        session: Option<&str>,
        id: &str,
        name: &str,
        sig: &str,
    ) {
        store.capture_tool_signature(scope, target, session, id, name, sig);
    }

    #[tokio::test]
    async fn ram_hit_after_capture() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn sqlite_fallback_hit_after_ram_clear() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        store.clear_memory_cache_for_test();
        assert_eq!(store.memory_entry_count_for_test(), 0);
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
        // The fallback lookup must repopulate RAM.
        assert_eq!(store.memory_entry_count_for_test(), 1);
    }

    /// Regression: RAM's `MEMORY_TTL` (1h) is shorter than SQLite's
    /// `PERSISTENT_TTL` (24h) by design — SQLite intentionally outlives the
    /// hot cache. Before the fix, an expired RAM entry made
    /// `resolve_tool_signature` return `Missing` immediately instead of
    /// falling through to the still-live SQLite row, so a continuation after
    /// ~1h on a long-running process failed even though the signature was
    /// still durably stored (restarting the process, which drops RAM
    /// entirely, would perversely have made it work again).
    #[tokio::test]
    async fn expired_ram_entry_falls_through_to_a_still_valid_sqlite_row() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        assert_eq!(store.memory_entry_count_for_test(), 1);

        // Age the RAM entry past its TTL without touching SQLite: the
        // persisted row is untouched and still has 23 of its 24 hours left.
        store.expire_memory_entry_for_test(&scope, &target, "call_1");

        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(
            result,
            OpaqueLookupResult::Compatible("SIG_A".into()),
            "an expired RAM entry must fall through to the still-live SQLite row, not report Missing"
        );
        // The fallback lookup must have evicted the stale entry and
        // repopulated RAM with a fresh one.
        assert_eq!(store.memory_entry_count_for_test(), 1);
        assert_eq!(
            store.cached_signature_for_test(&scope, &target, "call_1"),
            Some("SIG_A".into())
        );
    }

    #[tokio::test]
    async fn durability_write_is_queued_and_flushable() {
        // `capture_tool_signature` is a synchronous `fn` (it cannot be awaited),
        // so the data plane never blocks on SQLite. The RAM hit below is
        // therefore immediate, and the durable copy appears after the worker
        // drains the queue (observed via the `flush()` barrier). This is the
        // invariant that keeps a locked DB off the streaming path.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_A".into())
        );
        // Drop RAM, wait for the durability barrier, then read back from SQLite.
        store.clear_memory_cache_for_test();
        store.flush().await;
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_A".into())
        );
    }

    /// Regression: re-capturing the oldest key while the RAM cache is at
    /// capacity must move that key to the back, not leave a stale duplicate at
    /// the front. Before the fix, the stale occurrence was evicted and its
    /// `cache.remove()` deleted the value that had just been written, so an
    /// immediate continuation missed RAM (and could read stale SQLite state).
    #[tokio::test]
    async fn recapturing_oldest_key_at_capacity_keeps_the_new_value_in_ram() {
        let mut store = test_store().await;
        store.set_max_memory_rows_for_test(3);
        let scope = OpaqueClientScope::for_key("key_lru");
        let target = gemini_target();

        // Fill exactly to capacity: call_1 is now the oldest entry.
        for index in 1..=3 {
            capture(
                &store,
                &scope,
                &target,
                None,
                &format!("call_{index}"),
                "bash",
                &format!("SIG_{index}"),
            );
        }
        assert_eq!(store.memory_entry_count_for_test(), 3);

        // Replace the oldest entry with a new value. Pre-fix this removed the
        // freshly inserted value instead of the stale ordering slot.
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_1_NEW");

        // Observe the RAM hot path directly (no SQLite read, so the async
        // durability worker cannot make the assertion race).
        assert_eq!(
            store.cached_signature_for_test(&scope, &target, "call_1"),
            Some("SIG_1_NEW".to_string()),
            "re-capturing the oldest key must keep the new value resident"
        );
        assert_eq!(
            store.memory_entry_count_for_test(),
            3,
            "re-capturing must not grow the cache beyond capacity"
        );

        // An immediate continuation resolves the new value without touching
        // SQLite or missing the cache.
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_1_NEW".into())
        );
    }

    #[tokio::test]
    async fn missing_returns_missing_not_synthesized() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_unknown", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Missing);
    }

    #[tokio::test]
    async fn scope_isolation_across_virtual_keys() {
        let store = test_store().await;
        let target = gemini_target();
        let scope_a = OpaqueClientScope::for_key("key_a");
        let scope_b = OpaqueClientScope::for_key("key_b");
        capture(
            &store,
            &scope_a,
            &target,
            None,
            "call_same",
            "bash",
            "SIG_A",
        );
        store.flush().await;
        let result = store
            .resolve_tool_signature(&scope_b, Some(&target), None, "call_same", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Missing);
    }

    #[tokio::test]
    async fn session_match_allows_restore() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            Some("sess_1"),
            "call_1",
            "bash",
            "SIG_A",
        );
        let result = store
            .resolve_tool_signature(&scope, Some(&target), Some("sess_1"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn session_mismatch_refuses_restore() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            Some("sess_1"),
            "call_1",
            "bash",
            "SIG_A",
        );
        let result = store
            .resolve_tool_signature(&scope, Some(&target), Some("sess_2"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::SessionMismatch);
    }

    #[tokio::test]
    async fn no_stored_session_allows_lookup_by_tool_id_alone() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        // Request now carries a session even though none was stored: allowed.
        let result = store
            .resolve_tool_signature(&scope, Some(&target), Some("sess_new"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn tool_name_mismatch_refuses_restore() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "read")
            .await;
        assert_eq!(result, OpaqueLookupResult::ToolNameMismatch);
    }

    #[tokio::test]
    async fn producer_mismatch_is_incompatible() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let mut other = gemini_target();
        other.producer = "native:gemini:v2".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn family_mismatch_is_incompatible() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let mut other = gemini_target();
        other.family = "antigravity-claude".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn different_model_in_same_family_is_incompatible() {
        // generateContent signatures are only guaranteed on the originating
        // model, so a different model must never be handed stored state.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let mut other_model = gemini_target();
        other_model.model_id = "gemini-3-pro".into();

        // Do not flush: the next turn must observe the synchronous RAM capture
        // even if the background durability worker has not persisted it yet.
        let (_, evidence) = store.cached_identity_evidence(
            Some(&other_model),
            &scope.hash(),
            &tool_call_hash(&scope, "call_1"),
            None,
            "bash",
        );
        assert!(evidence.any);
        assert!(evidence.tool_name_match);
        assert!(evidence.session_compatible);

        let result = store
            .resolve_tool_signature(&scope, Some(&other_model), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn different_model_lookup_still_rejects_a_different_tool_name() {
        // A cross-model switch must not mask a reused tool-call id: identity is
        // validated before the target is classified as incompatible, so the
        // pipeline returns a hard client error instead of a placeholder.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            None,
            "call_1",
            "get_weather",
            "SIG_A",
        );
        store.flush().await;
        let mut other_model = gemini_target();
        other_model.model_id = "gemini-3-pro".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other_model), None, "call_1", "read_file")
            .await;
        assert_eq!(result, OpaqueLookupResult::ToolNameMismatch);
    }

    #[tokio::test]
    async fn different_model_lookup_still_rejects_a_different_explicit_session() {
        // Same reused tool-call id on another model but a conflicting session
        // must be refused, not translated with a placeholder.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            Some("sess_1"),
            "call_1",
            "bash",
            "SIG_A",
        );
        store.flush().await;
        let mut other_model = gemini_target();
        other_model.model_id = "gemini-3-pro".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other_model), Some("sess_2"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::SessionMismatch);
    }

    #[tokio::test]
    async fn different_model_lookup_with_matching_identity_stays_incompatible() {
        // Matching tool name and session on another model is a genuine
        // cross-model transfer: incompatible, not a mismatch.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            Some("sess_1"),
            "call_1",
            "bash",
            "SIG_A",
        );
        store.flush().await;
        let mut other_model = gemini_target();
        other_model.model_id = "gemini-3-pro".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other_model), Some("sess_1"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn exact_model_row_identity_is_not_shadowed_by_another_models_row_tool_name() {
        // Two models can share a tool-call id (bytes_for_a_different_model_do_not_collide).
        // When the request targets the exact model that owns one of those rows,
        // that row's identity must be checked first: a Pro row for `call_1` under
        // a different tool name must never make a Flash row for `call_1` under the
        // requested tool name disappear behind it. Before the fix, filtering by
        // tool name across *all* rows for the id could drop the exact-model row
        // and leave only the foreign-model row, which then failed the `matches`
        // scan and returned `Incompatible` instead of `ToolNameMismatch`.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let flash = gemini_target();
        let mut pro = gemini_target();
        pro.model_id = "gemini-3-pro".into();
        capture(
            &store,
            &scope,
            &flash,
            None,
            "call_1",
            "get_weather",
            "SIG_FLASH",
        );
        capture(&store, &scope, &pro, None, "call_1", "read_file", "SIG_PRO");
        store.flush().await;
        // Force the SQLite fallback path: the RAM cache is keyed per exact
        // target/model, so a hot-cache lookup would never exhibit this bug (it
        // never sees the other model's row). The bug is specifically in the
        // multi-row SQLite fetch_rows() scan, reached after eviction or restart.
        store.clear_memory_cache_for_test();

        // Request targets Flash (the exact model that owns the get_weather row)
        // but supplies the Pro row's tool name: must be rejected as a mismatch,
        // never answered from the Pro row and never classified Incompatible.
        let result = store
            .resolve_tool_signature(&scope, Some(&flash), None, "call_1", "read_file")
            .await;
        assert_eq!(result, OpaqueLookupResult::ToolNameMismatch);

        // The exact-model row with the matching tool name still resolves.
        let result = store
            .resolve_tool_signature(&scope, Some(&flash), None, "call_1", "get_weather")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_FLASH".into()));
    }

    #[tokio::test]
    async fn exact_model_row_identity_is_not_shadowed_by_another_models_row_session() {
        // Same failure mode as the tool-name case, for session identity: a Pro
        // row for `call_1` under a different session must not make the exact
        // Flash row for `call_1` unreachable, and the request must be refused
        // (not silently answered from the foreign row, not `Incompatible`).
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let flash = gemini_target();
        let mut pro = gemini_target();
        pro.model_id = "gemini-3-pro".into();
        capture(
            &store,
            &scope,
            &flash,
            Some("sess_flash"),
            "call_1",
            "bash",
            "SIG_FLASH",
        );
        capture(
            &store,
            &scope,
            &pro,
            Some("sess_pro"),
            "call_1",
            "bash",
            "SIG_PRO",
        );
        store.flush().await;
        // Force the SQLite fallback path (see the tool-name test above for why).
        store.clear_memory_cache_for_test();

        // Request targets Flash (owns sess_flash) but supplies Pro's session:
        // must be a session mismatch against the exact-model row, not answered
        // from the Pro row and not Incompatible.
        let result = store
            .resolve_tool_signature(&scope, Some(&flash), Some("sess_pro"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::SessionMismatch);

        // The exact-model row with the matching session still resolves.
        let result = store
            .resolve_tool_signature(&scope, Some(&flash), Some("sess_flash"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_FLASH".into()));
    }

    #[tokio::test]
    async fn account_change_within_same_provider_stays_compatible() {
        // Compatibility is keyed on provider/model/family/producer only — not
        // account. Same-provider account failover must not destroy state.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn duplicate_upsert_same_signature_is_idempotent() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        assert_eq!(store.count_rows().await, 1);
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn duplicate_write_different_signature_newest_wins() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_B");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_B".into()));
        store.flush().await;
        assert_eq!(store.count_rows().await, 1);
    }

    #[tokio::test]
    async fn bytes_for_a_different_model_do_not_collide() {
        // Two models may share a tool-call id; each keeps its own row.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let flash = gemini_target();
        let mut pro = gemini_target();
        pro.model_id = "gemini-3-pro".into();
        capture(&store, &scope, &flash, None, "call_1", "bash", "SIG_FLASH");
        capture(&store, &scope, &pro, None, "call_1", "bash", "SIG_PRO");
        store.flush().await;
        assert_eq!(store.count_rows().await, 2);
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&flash), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_FLASH".into())
        );
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&pro), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_PRO".into())
        );
    }

    #[tokio::test]
    async fn pruning_removes_rows_over_db_cap() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        // Insert a handful of rows and force a prune; cheap smoke test that
        // prune() does not error and respects the expiry predicate.
        for i in 0..5 {
            capture(
                &store,
                &scope,
                &target,
                None,
                &format!("call_{i}"),
                "bash",
                "SIG",
            );
        }
        store.flush().await;
        store.prune().await;
        assert_eq!(store.count_rows().await, 5);
    }

    #[tokio::test]
    async fn empty_signature_or_id_is_never_persisted() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "");
        capture(&store, &scope, &target, None, "", "bash", "SIG");
        store.flush().await;
        assert_eq!(store.count_rows().await, 0);
    }

    #[tokio::test]
    async fn target_without_capability_reports_incompatible_not_missing() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        // Do not flush: capability-less portability decisions must observe the
        // synchronous RAM capture just like cross-model decisions do.
        let result = store
            .resolve_tool_signature(&scope, None, None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn expired_state_is_treated_as_missing() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.expire_all_for_test().await;
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Missing);
    }

    #[tokio::test]
    async fn metrics_track_capture_and_lookup_outcomes() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let _ = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        let _ = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_missing", "bash")
            .await;
        let metrics = store.metrics();
        assert_eq!(metrics.capture_stored, 1);
        assert_eq!(metrics.capture_dropped, 0);
        assert_eq!(metrics.lookup_hit, 1);
        assert_eq!(metrics.lookup_miss, 1);
        assert_eq!(metrics.entries, 1);
    }
}

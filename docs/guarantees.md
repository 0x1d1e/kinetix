# Operational guarantees

Normative guarantees Kinetix makes to operators. Each one names the code or
test that enforces it. A change that weakens a guarantee must update this file.

## Artifact provenance

Applies to plugin packages (`src/plugins/manager.rs`, `src/plugins/package.rs`).

- **Package hash.** Every accepted package is identified by the SHA-256 of its
  bytes. When an expected SHA-256 is supplied (for example by the marketplace
  catalog), a mismatch rejects the install before anything is published.
- **Version.** The manifest version is validated on install. A plugin pinned
  to a version is not updated until it is unpinned.
- **Signature.** An unsigned package is accepted for local installs. A present
  signature must be well-formed. A signature not verified by a trusted
  publisher key is rejected unless the operator passes an explicit override
  (`--allow-untrusted-signature`). Marketplace installs require the catalog
  SHA-256 and a trusted signature.
- **Invocation identity.** Each request served by a plugin adapter records that
  package's SHA-256 in `usage_logs.plugin_package_sha256` and
  `usage_attempts.plugin_package_sha256` (NULL for built-in adapters). It is
  shown in admin request logs, CSV export, and the dashboard request detail.
- **Immutable.** Accepted package bytes and their provenance row
  (`plugin_packages`: id, version, SHA-256, signature status) are never
  rewritten. A different package adds a new row, keyed by plugin id and
  SHA-256.
- **Retained after removal.** Removing a plugin deletes its active install and
  runtime state, not its retained packages. `install_retained(id, sha256)`
  reinstalls one: the bytes are re-read and re-hashed against the provenance
  row before use.

## Configuration migration

Applies to the SQLite control plane (`src/db.rs`, `migrations/`).

- **Versioned.** Schema changes ship as timestamped migrations. Applied
  migrations are immutable; a checksum mismatch fails startup.
- **Pre-migration backup.** Before pending migrations or post-migration repairs
  change an existing database, a `VACUUM INTO` snapshot
  (`backups/kinetix-pre-migration-<ts>-<uuid>.db`) is written and marked
  pending. It stays protected from retention until migration and repair
  succeed. A failed retry reuses the same snapshot.
- **Recoverable on failure.** Each migration runs in its own transaction, so a
  failing migration leaves no partial schema change. A failure stops startup:
  the database is never served half-migrated, and the pre-migration snapshot remains for
  restore (see [Deployment](wiki/Deployment.md#backups-and-restore)).
- **No silent downgrade.** A binary that does not know an applied migration
  version refuses to start against that database.

## Deterministic eligibility

Applies to target and account selection (`src/pipeline.rs`, `src/pool.rs`).

Given the same runtime snapshot, account state, request, and clock instant,
the set of eligible candidates and their priority order are deterministic.
Eligibility depends only on those inputs: an account is eligible when its
effective status at that instant is healthy (disabled, unexpired cooldown,
unreset quota exhaustion, and open circuits are excluded). If no account of a
target is eligible, all are kept so the attempt reports the real state.
Accounts are sorted by priority, then id. Weighted random choice happens only
afterwards, within one priority tier, and only reorders siblings; every
eligible sibling stays available for fallback. Route dry runs
(`POST /admin/api/routes/dry-run`) use a stable seed, so a simulation is
reproducible and consumes no live randomness.

## Catalog semantics

Observed != accepted != runtime. A catalog refresh never changes runtime
semantics by itself; changes reach runtime only after an explicit accept or
model edit. See [model-state.md](model-state.md).

## Process lifecycle

Applies to SIGTERM / Ctrl-C (`src/server.rs`, `src/process.rs`). Grace period:
`shutdown_grace_secs` / `KINETIX_SHUTDOWN_GRACE_SECS`, default 30.

1. Stop accepting connections. Background loops stop at their next tick; an
   iteration already running finishes. New plugin hook jobs are refused.
2. Drain in-flight requests, committed streams, background iterations, and
   tracked flights (hook jobs, coalesced provider work) until the grace
   period ends.
3. At the deadline, cancel what remains. Uncommitted requests cancel their
   upstream calls and record cancellation accounting. Committed streams end
   with an explicit error. Remaining tracked work is dropped, with 2s to
   unwind.
4. Flush usage accounting, target telemetry, and opaque state (each step
   bounded to 5s).
5. Close the database.

## Translation

Every represented semantic field is preserved, translated, consumed
internally, or explicitly rejected. Nothing is dropped silently. Enforced per
frontend and transport by `tests/field_contract.rs`; see
[field-contract.md](field-contract.md) and the generated
[compatibility matrix](generated/compatibility-matrix.md).

## Demand-driven work

Disabled or non-due integrations perform no network, provider, or plugin work
just because they are configured.

- Health probes and cached routing-fact refresh run only for integrations a
  request used in the last 15 minutes.
- Credential refresh sleeps until the earliest due lease instead of polling.
- Registry reload compares one revision integer and rebuilds only when a
  registry-affecting write bumped it.

Measured per integration (`provider:<id>`, `plugin:<id>`) on
`GET /admin/api/metrics`:

```text
kinetix_integration_poll_total
kinetix_integration_wake_total
kinetix_integration_discovery_runs_total
kinetix_integration_probe_runs_total
kinetix_integration_credential_refresh_runs_total
kinetix_integration_wasm_instantiations_total
```

`idle_integrations_do_no_integration_specific_work` (`src/admin.rs`) holds
these at zero for 100 idle integrations.

# ADR-0001: Reboot Kinetix and kinetix-plugins at 0.1.0 and delete prior releases

## Status

Accepted

## Context

Kinetix is at 0.6.4 (tags `v0.6.0`-`v0.6.4`). kinetix-plugins tags each plugin
separately (for example `opencode-free-v0.1.9`, `antigravity-oauth-v0.1.19`).
The reboot ([docs/reboot.md](../reboot.md)) changes public contracts without
compatibility shims. The current version numbers imply a maturity the system
does not have.

## Decision

- Preserve the pre-reboot source on a `legacy/v0.6` branch in both repos. It
  accepts no feature work.
- Delete all existing GitHub releases and tags in both repos.
- Release the reboot as Kinetix 0.1.0 and kinetix-plugins 0.1.0, versioned
  and released together until the plugin contract stabilizes.

## Alternatives considered

- **Keep history, ship 0.7.0.** Avoids breaking update paths. Rejected because
  it keeps the maturity signal the reboot is meant to reset.
- **0.1.0 under a new repo/package identity.** Avoids version-ordering
  conflicts. Rejected to keep the existing repo, issues, and links.

## Consequences

### Positive

- Version numbers restart at a level that matches the conformance evidence.
- One version identifies a compatible core + plugin set.

### Negative / trade-offs

- `kinetix update` on 0.6.x compares semver (`src/update.rs:44`), sees 0.1.0
  as older, and reports nothing to do. Existing users must reinstall manually.
- Released 0.6.x binaries and `install.sh` pinned to old tags fetch from
  `releases/download/...` and will get 404s.
- Plugin catalog entries (`kinetix-plugins/catalog.json`, the bundled
  `src/plugins/catalog.snapshot.json`) point at deleted release assets.
  Plugin install from 0.6.x fails.
- Plugin version strings such as `0.1.7` will be reused for different
  artifacts. Anything keyed on version alone (caches, the recorded
  `plugin_package_sha256` provenance next to a version) becomes ambiguous
  across the boundary.

## Migration

1. Cut `legacy/v0.6` in both repos.
2. Announce the reboot and the manual reinstall requirement in the README
   before deleting releases.
3. Delete releases/tags only after 0.1.0 is ready to publish, so `latest` is
   never empty.

> Historical pre-reboot research notes. Several citations point at forks of the reference projects, not upstream. Superseded by [docs/reboot.md](../../reboot.md) and [docs/adr/](../../adr/). Do not update.

# Kinetix v0.1.0 Reboot

**Product/contract reboot, not rewrite**.

- Freeze current Kinetix + plugins as legacy.
- Remove/delete all releases in github
- New **v0.1.0**, breaking changes allowed.
- Keep proven code; delete accidental complexity.
- Define canonical request/response + plugin contract first.
- Compatibility/conformance suite becomes release gate.
- Build for **correctness + interoperability first**, features later.
- Core + plugins versioned/rebuilt together initially.

Current version numbers imply maturity the system doesn’t have. Resetting is justified.

Before rebooting to **v0.1.0**, finish only the work that reduces uncertainty:

1. [Compatibility audit](compatibility-audit.md)

   - Pin exactly what breaks vs 9router / OmniRoute.
   - Inputs, outputs, tools, thinking, streaming, continuation state, schema handling.
2. [Write the contracts](contracts.md)

   - Canonical request/response model.
   - Thinking translation rules.
   - Tool-schema policy.
   - Plugin ABI/capability contract.
   - Error/fallback semantics.
   - Streaming/continuation ownership.
3. [Build conformance fixtures](conformance-fixtures.md)

   - Provider wire-shape golden tests.
   - Cross-format compatibility cases.
   - Plugin/native adapters must pass same suite.
4. [Decide architecture boundaries](architecture-boundaries.md)

   - Core owns routing, canonical semantics, retries, fallback, accounting.
   - Plugins own provider-specific translation/auth.
   - No duplicated policy between core/plugins.
5. [Inventory current code](code-inventory.md)

   - **Keep:**  proven auth, OAuth rotation, provider clients, parsers, DB/migrations where valid.
   - **Rewrite:**  pipeline/admin god-code, inconsistent adapter paths, duplicated stream/schema logic.
   - **Delete:**  compatibility hacks with no contract.
6. [Freeze current releases](freeze-releases.md)

   - Tag final pre-reboot versions.
   - Document known failures.
   - Stop adding features.

Then start **Kinetix 0.1.0 + kinetix-plugins 0.1.0 together**.

Do **not** finish the current backlog first. Finish the **spec + conformance suite + architecture decisions** first.



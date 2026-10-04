# Model state lifecycle

Every Model has three distinct states. Code lives in `src/model_state.rs`.

```text
ObservedModel  --reconciliation-->  AcceptedModel  --registry snapshot-->  RuntimeModel
```

| State | What it is | Stored in | Changed by |
| --- | --- | --- | --- |
| Observed | Untrusted external facts: provider `/models`, plugin model sources, models.dev, probes | `discovery.latest_observation`, `discovery.reconciliation`, `model_observations` history | Every discovery refresh |
| Accepted | Operator- or policy-approved semantics | Model columns, plus accepted discovery keys: import snapshot, `configured_transport`, `operator_*_overrides`, `effective_pricing`, `accepted_provenance` | Model create/update, reconciliation `accept`/`pin`, pricing sync |
| Runtime | Effective execution semantics of a request | Immutable registry snapshot the request started with | Registry reload of accepted state |

## Invariant

A catalog refresh is not a runtime semantic change.

A refresh only writes observation keys (`latest_observation`, `last_seen`,
`reconciliation`, `disappeared`, `flagged_at`). Changed context window,
output limit, capabilities, thinking metadata, modalities, prices, display
name, or transport claims appear in `reconciliation.diff` for review. They
reach runtime only after an explicit `accept` (or a model edit).

- A model missing from the upstream list is flagged `disappeared` and stays
  routable. Reappearance clears the flag.
- Omitted or `null` upstream metadata is unknown, not a retraction. It never
  erases prior evidence.
- Transport claims are excluded from the default `accept` set. Changing a
  model's transport needs an explicit `fields: ["transport"]` decision.
- models.dev facts are observations like any other source. A catalog that
  conflicts with accepted state shows up in the diff only.

Two exceptions are allowed by policy:

- **Plugin `opaque_state`**: host-owned protocol metadata stays live so
  continuations remain correct.
- **Fresh probe evidence**: scope-matched evidence may refine capability
  fields the operator does not own. Operator overrides always win.

## Provenance

Runtime facts carry provenance (`RuntimeModel::provenance`):

| Field | Values |
| --- | --- |
| `transport` | `plugin`, `operator` (configured transport), discovery source, `provider` (provider default) |
| `context_window`, `max_output_tokens` | `operator`, `upstream_discovery`, `models.dev:*`, `provider_metadata`, ... |
| `thinking` | `operator`, or the discovery capability source |

Accepted limit provenance is recorded in `discovery.accepted_provenance` when
a model is created, edited, or a reconciliation diff is accepted.

## Accepting changes

```http
PUT /admin/api/models/{id}/reconciliation
{"action": "accept", "fields": ["context_window", "capabilities.tool_calling"]}
```

`action` is `accept`, `pin` (keep the accepted value and stop reporting the
field), or `ignore` (dismiss the current diff). For `accept`, empty `fields`
means every diff field except `transport`.

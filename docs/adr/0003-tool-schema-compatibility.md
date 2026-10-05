# ADR-0003: Tool schemas use one core engine with strict and compatible modes

## Status

Accepted

## Context

Real coding-agent tool schemas (Chrome DevTools `maxLength`, k-jev
`patternProperties`, tuples, `$ref`, Zod/TypeBox output, Codex shell) are
often not expressible in a target's schema dialect, Gemini especially.
Current Kinetix behavior depends on the integration (see ADR-0002): core
Gemini rejects unverified keywords, while the SDK compatible mode normalizes
them. Rejecting makes whole agents unusable for one validation keyword.

Reference gateways that sanitize aggressively have shipped bugs where
sanitizing changed meaning: nullable schemas flattened into fabricated
values, and traversal corrupting a property literally named `properties`.

## Decision

One core schema engine translates a client JSON Schema into the Target's
schema profile in `strict` or `compatible` mode. `compatible` is the
default.

The mode is Target-owned. The Provider supplies the default and a Target may
narrow or override it. A Route never sets it: a Route selects Targets and does
not change wire semantics. This replaces Antigravity's current Provider-only
setting.

Every transform is classified, and each classification is recorded in the
Route Trace:

| Class | Example | strict | compatible |
| --- | --- | --- | --- |
| preserved | supported keyword | keep | keep |
| equivalent translation | draft-07 tuple `items` -> `prefixItems` | translate | translate |
| annotation removed | `$schema`, `title` where irrelevant | remove | remove |
| validation weakened | `maxLength`, unsupported tuple constraints | reject | weaken + trace |
| rejected | change to type, required, nullability, or enum meaning | reject | reject |

Unknown keywords: preserved on same-format passthrough; on translated targets
the schema profile decides, and unknown means rejected unless the profile
lists the keyword as validation-only.

## Alternatives considered

- **Strict only.** Current core behavior; breaks real agents.
- **Sanitize everything (reference-gateway style).** Maximizes acceptance but
  allows silent semantic changes.
- **Route-owned mode.** The same Target would behave differently depending
  on which Route selected it, so Target fallback could change wire semantics
  mid-request.

## Consequences

- A weakened constraint means the model may produce values the client's
  validator rejects. The client still validates, so this is detectable, and
  the trace records the weakening.
- Profiles become versioned data that the conformance corpus pins with exact
  upstream JSON.

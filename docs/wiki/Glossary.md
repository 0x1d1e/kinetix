<!-- GENERATED from docs/GLOSSARY.md by scripts/render-doc-pages.py.
     Edit the canonical docs file instead. -->

# Kinetix glossary

These definitions describe Kinetix domain concepts, not implementation details.

## Account

A usable credential-bearing or no-auth identity associated with a Provider and
eligible for pool selection.

## Adapter / Provider Adapter

An outbound integration boundary that translates canonical Kinetix semantics
to and from a target wire protocol.

## Canonical Model Identity

The normalized model identity Kinetix uses to associate metadata across
provider-specific observations.

## Capability

A known model or integration property used for validation, translation,
eligibility, or presentation. Unknown is distinct from false or unsupported.

## Commit Point

The moment a response becomes client-visible. After this point Kinetix may not
retry or fail over to another target.

## Credential

Secret or authorization material used to authenticate an Account to an
upstream.

## Credential Enrollment

The process used to establish an Account's credentials: `manual`, `auth_flow`,
or `none`.

## Fallback

Attempting another eligible Route target after a pre-commit target failure.
Fallback does not mean post-commit stream recovery.

## Frontend

An inbound protocol boundary translating a client wire format into and out of
Kinetix canonical semantics.

## Integration

A reusable integration definition and capability bundle from which provider
behavior or credential enrollment may be configured. Plugin-backed integrations
group capabilities declared by a plugin; an Integration is not itself a
Provider or Account.

## Model

A Kinetix model configuration representing an upstream model exposed directly
or used by Routes.

## Model ID

The canonical, provider-independent model identifier used for identity or
enrichment matching where applicable. It is not necessarily a `provider/model`
string.

## Model Metadata Provenance

The source associated with an enriched model metadata field, such as provider
metadata, plugin evidence, models.dev, or bundled catalog data.

## Observation

Metadata reported by a provider, plugin, or catalog source before precedence
and normalization are applied.

## Opaque State

Provider-specific continuation state that cannot safely be interpreted as
portable canonical state.

## Pool

The selectable set of Accounts available for a Provider or target.

## Provider

An upstream service configuration defining how Kinetix can communicate with an
LLM service. A Provider is not a wire format.

## Route

A client-visible executable selector containing one or more Targets plus
selection and fallback policy.

## Session Affinity

An explicit continuity hint used to prefer a previously selected target or
account for a client session. When cache affinity or sticky routing is enabled
on the Route, a client `prompt_cache_key` (scoped per virtual key) also serves
as an affinity key. Kinetix does not infer affinity when no supported identity
is available.

## Source Integration

The Integration or binding from which a Provider or credential-enrollment path
originated. It records provenance and enrollment context, not Provider
identity.

## Target

One concrete candidate inside a Route. A Target identifies the configured
model and Provider path that may be attempted.

## Wire Format

The request/response protocol spoken on a connection. Examples include OpenAI
Chat Completions, OpenAI Responses, Anthropic Messages, Gemini, and
plugin-defined formats.

> Wire format is not provider identity.

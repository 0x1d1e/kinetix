# Admin API

The dashboard and `kinetix api` use `/admin/api`. These contracts do not change
OpenAI or Anthropic inference responses. Audit-event completeness is separate
from the API transport contract.

## Authentication and reference

Authenticate with `x-kinetix-admin-token` using the administrator password or a
session token, or with the dashboard session cookie. Cloudflare Access checks,
when configured, still apply. Login, logout, and credential-enrollment callbacks
have their existing unauthenticated flows. Responses use `Cache-Control: no-store`.

`GET /admin/api/reference` returns `kinetix-admin-reference-v1`: operations,
methods, paths, JSON Schema for request/query/path inputs, error/page schemas,
authentication, collection filters/order, and bounds. The same reference is
available offline:

```bash
kinetix api-reference > admin-reference.json
```

The reference is generated from the typed route registrations in
`src/router.rs` and DTOs, not a second endpoint inventory. Success payloads remain
endpoint-specific: JSON, Prometheus text, probe SSE, or OAuth redirects as listed
in the reference. It is not an OpenAPI document or a complete success-response
schema catalog.

## Errors and validation

Admin API 4xx and 5xx responses have this shape:

```json
{
  "error": {
    "code": "invalid_request",
    "message": "validation failed",
    "fields": [
      {"field": "max_concurrent_requests", "code": "invalid_value", "message": "must be positive or zero for unlimited"}
    ]
  }
}
```

Use `error.code`, HTTP status, and field paths for automation. Messages are for
people; `fields` may be empty for authentication, resource, or request-level
errors. Nested paths use `targets[0].weight`. Shared Route semantic validation
supplies its existing issue codes; it is not a separate API validation policy.

Malformed JSON is 400, missing JSON content type is 415, and missing/incorrectly
typed JSON fields are 422. Semantic validation and invalid queries are 400.
Authentication is 401/403, missing resources/endpoints are 404, wrong methods
are 405, and conflicts are 409. The reference supplies all status/code mappings.
`Allow` and `Retry-After`, when present, survive normalization. Internal failures
return a generic 500 message, never the database or underlying exception text.
Degraded control-plane storage still rejects mutations with 503.

## Collection pagination and filtering

Keys, providers, models, accounts, Routes, aliases, usage/requests, and audit
collections retain their named array and add:

```json
{"page":{"limit":200,"offset":0,"total":325,"next_offset":200}}
```

Use `limit` (1-500, default 200) and `offset` (0-1000000, default 0). Follow
`next_offset` until null. `total` counts matching rows before pagination. Counts
and rows share one database transaction per page; separate pages are not a
frozen snapshot. Each collection's fixed ordering has an ID tie-breaker.

`q` searches the field listed in the generated reference using an ASCII
case-insensitive literal substring: `%` and `_` are not wildcard operators. Exact filters vary
by collection: `provider_id`, `key_id`, `status`, `actor`, or `action`. Unsupported
filters are rejected, not ignored. Each filter is limited to 256 UTF-8 bytes.
Hidden no-auth accounts are not part of the accounts collection.

Model observations retain their existing bounded cursor pagination. Other
specialized read payloads are not covered by the offset collection contract.
The dashboard follows all resource pages; usage and audit views request a
bounded recent page.

Resource IDs are opaque persisted identifiers, not names or array positions.
Use IDs returned by the API; renaming, filtering, pagination, and registry reloads
do not change them.

## Bounds and secrets

The URI limit is 8192 bytes. Bodies are limited to 1 MiB, including requests
without `Content-Length`. Plugin installation, configuration import, and
credential-interchange import allow 16 MiB. Excess bodies return 413 and excess
URIs return 414. These limits do not apply to inference streaming.

Ordinary reads omit credential values and virtual-key hashes. Virtual-key
creation returns its full key once. Every provider `extra_headers` value is
a string and returned as `[REDACTED]`. Sending an unchanged placeholder under
the same stored header name on provider update or configuration import preserves
its value; a placeholder without a stored value is rejected. Omit a header to remove it, or send its
replacement value to change it.

Configuration export redacts headers unless `include_secrets=true`. Explicit
credential exports and generated client profiles may also contain secrets.
Protect those responses and files. A redacted export is not a credential backup
and cannot provision its redacted headers on a different server.

## CLI client

Existing provider/account/model/Route CLI commands remain offline database
administration commands. `kinetix api` is the HTTP client for the same contract
used by the dashboard:

```bash
export KINETIX_ADMIN_URL=http://127.0.0.1:8080
read -rs KINETIX_ADMIN_TOKEN
export KINETIX_ADMIN_TOKEN
kinetix api 'keys?limit=50&offset=0'
kinetix api keys --method POST --body key-request.json
```

Supported methods: GET, POST, PUT, DELETE. Paths are relative to `/admin/api`;
`--body` reads a JSON file. The client does not follow redirects or allow paths
to escape the API namespace. Server JSON is printed to stdout unchanged in
meaning, including structured errors; non-success status exits nonzero. Prefer
the token environment variable to a command-line argument.

## Client migration

`error` changed from a string to an object. Read `error.message` for display and
retain `error.fields` for correction. Formerly unbounded resource lists now
return pages; limits above 500 and nonpositive limits are rejected rather than
clamped. Header values in ordinary provider reads are now placeholders. Consumers
must follow pagination and preserve those placeholders when editing providers.

Contract acceptance lives in `tests/admin_api_contract.rs` (real HTTP and CLI)
and `dashboard/src/lib/api.test.mjs`; both run through `scripts/run-ci.sh`.

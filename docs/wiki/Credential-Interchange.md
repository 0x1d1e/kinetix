# Credential interchange

Portable credential bundles are independent of Kinetix configuration backups. They move credential descriptors and, only when explicitly requested, encrypted credential values. Provider definitions, account runtime state, and plugin-private storage are not included.

## Bundle format

A bundle has schema `llm-credential-bundle/v1` and contains records with a separate descriptor and optional secret envelope:

```json
{
  "schema": "llm-credential-bundle/v1",
  "credentials": [
    {
      "descriptor": {
        "schema": "llm-credential/v1",
        "kind": "api_key",
        "provider": "anthropic",
        "label": "work",
        "metadata": {
          "credential_mode": "manual",
          "extensions": {
            "org.prightcord.kinetix": {
              "auth_scheme": "bearer",
              "base_url": "https://api.anthropic.com",
              "credential_plugin": "",
              "source_plugin_id": null,
              "source_integration_id": null,
              "wire_format": "openai",
              "wire_plugin": "",
              "allow_insecure_tls": false,
              "credential_hosts": [],
              "follow_redirects": false,
              "custom_header_name": null,
              "custom_param_name": null
            }
          }
        }
      }
    }
  ]
}
```

`kind` is `api_key`, `oauth`, `token`, `custom`, or `none`. `metadata.credential_mode` is required and explicitly identifies `manual`, `auth_flow`, or `none`; it is not inferred from missing fields. Namespaced `metadata.extensions` carry provider-specific compatibility data without imposing it on other producers. Kinetix accepts unknown optional fields and ignores them where unsupported. Its `org.prightcord.kinetix` extension binds the normalized `base_url` and credential-delivery settings: auth scheme and plugin bindings, wire format and adapter plugin, TLS verification, credential-host allowlist, redirect policy, and custom auth field names. `credential_hosts` is exported as a sorted, lowercase array.

A `none` descriptor has kind `none` and no envelope. Manual descriptors use `api_key`, `token`, or `custom`; auth-flow descriptors use `oauth` or `custom`. Kinetix stores manual values as generic strings, so imported manual `token` and `custom` kinds are exported as `api_key` later. Auth-flow `oauth` versus `custom` is preserved. Kinetix matches `provider` to a unique configured provider name and `label` to an account label. Kinetix rejects imports when the endpoint or any credential-delivery setting differs from the configured provider. Duplicate provider names are rejected. This identity does not create or reconfigure providers; bundles do not transfer account policy or health state.

## Encrypted secrets

Exports omit `secret_envelope` and `encryption` by default. To include secrets, Kinetix requires `include_secrets: true` and an operator passphrase of at least 12 characters. Each bundle uses a random 16-byte salt and PBKDF2-HMAC-SHA256 with 600,000 iterations to derive one 256-bit key. Each non-empty secret is separately encrypted with AES-256-GCM and a random 12-byte nonce. The envelope payload is base64-encoded ciphertext followed by the GCM authentication tag:

```json
{
  "encryption": {
    "kdf": "PBKDF2-HMAC-SHA256",
    "iterations": 600000,
    "salt": "<base64 16-byte salt>",
    "cipher": "AES-256-GCM"
  },
  "secret_envelope": {
    "format": "llm-secret-envelope/v1",
    "cipher": "AES-256-GCM",
    "nonce": "<base64 12-byte nonce>",
    "payload": "<base64 ciphertext and tag>"
  }
}
```

The descriptor is authenticated as AES-GCM additional data: the UTF-8 prefix `kinetix:credential-interchange:v1`, one NUL byte, then the descriptor serialized with RFC 8785 JSON Canonicalization Scheme (JCS). This standard defines key ordering, string escaping, and ECMAScript-compatible number rendering; array order is preserved. The golden vectors below are also covered by tests and can be used by other implementations. A changed descriptor, nonce, payload, salt, or passphrase fails decryption. Store secret-inclusive bundle files as sensitive material; Kinetix never exports their plaintext secrets.

### JCS interoperability vectors

RFC 8785's number/string example:

Input:

```json
{"numbers":[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001],"string":"€$\u000f\nA'B\"\\\"/","literals":[null,true,false]}
```

Canonical UTF-8 output:

```json
{"literals":[null,true,false],"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27],"string":"€$\u000f\nA'B\"\\\"/"}
```

Credential descriptor input:

```json
{"schema":"llm-credential/v1","kind":"api_key","provider":"anthropic","label":"café/仕事","metadata":{"credential_mode":"manual","extensions":{"org.prightcord.kinetix":{"auth_scheme":"bearer"}}},"x-z":{"array":[3,2,1],"object":{"z":1e30,"é":"雪"}}}
```

Canonical UTF-8 output (the bytes appended after the NUL-terminated AAD prefix):

```json
{"kind":"api_key","label":"café/仕事","metadata":{"credential_mode":"manual","extensions":{"org.prightcord.kinetix":{"auth_scheme":"bearer"}}},"provider":"anthropic","schema":"llm-credential/v1","x-z":{"array":[3,2,1],"object":{"z":1e+30,"é":"雪"}}}
```

## Admin API

`POST /admin/api/credential-interchange/export` accepts:

```json
{"include_secrets": false}
```

Secret-inclusive export requires both explicit intent and a passphrase:

```json
{"include_secrets": true, "passphrase": "use-a-unique-long-passphrase"}
```

`POST /admin/api/credential-interchange/import` accepts a bundle and defaults to validation only:

```json
{
  "bundle": {"schema": "llm-credential-bundle/v1", "credentials": []},
  "apply": false,
  "replace_existing": false
}
```

Supply `passphrase` when the bundle contains secret envelopes. Dry-run returns `valid`, `plan`, `problems`, `conflicts`, `warnings`, and `missing_resources`. It writes nothing. Manual secrets must contain a non-whitespace character; auth-flow secrets must be JSON objects no larger than 256 KiB. Applying reruns validation and stages the next runtime snapshot inside one SQLite write transaction; validation, write, or snapshot-build failures roll back the whole import. Imported secrets are decrypted only in memory and re-encrypted with the destination's local key.

Kinetix resolves descriptors by unique provider name and validates the explicit credential mode. It checks the endpoint and all credential-delivery settings before accepting an import. Kinetix bundles exported before delivery identity was added must be re-exported. Auth-flow imports also require matching Kinetix integration bindings; installed plugin manifests are authoritative. An unavailable plugin integration is reported in `missing_resources`; the credential can still be stored, but the plugin must be installed before it can be used. No plugin-private KV state is imported. Secret-inclusive export currently rejects bundles containing auth-flow accounts because `accounts.secret_enc` may be stale relative to a plugin's rotated canonical credential. Import can create a new auth-flow account from an envelope as initial account-row state, but auth-flow replacement is rejected because updating only that row cannot safely replace plugin state or cached leases. Portable export and replacement need a versioned plugin snapshot/restore contract before they can be supported safely.

A descriptor without an envelope is validation-only: it can be checked or used as a template, but does not create a credential. Existing provider/label matches with an envelope are reported as conflicts and left unchanged unless `replace_existing: true`; auth-flow replacement is rejected even when explicitly requested. Manual replacement changes only credential material. New accounts use local defaults. A `none` descriptor verifies a credential-free provider and makes no account changes.

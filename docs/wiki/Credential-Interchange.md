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
              "credential_plugin": "",
              "source_plugin_id": null,
              "source_integration_id": null
            }
          }
        }
      }
    }
  ]
}
```

`kind` is `api_key`, `oauth`, `token`, `custom`, or `none`. `metadata.credential_mode` is required and explicitly identifies `manual`, `auth_flow`, or `none`; it is not inferred from missing fields. Namespaced `metadata.extensions` carry provider-specific compatibility data without imposing it on other producers. Kinetix accepts unknown optional fields and ignores them where unsupported.

A `none` descriptor has kind `none` and no envelope. Manual descriptors use `api_key`, `token`, or `custom`; auth-flow descriptors use `oauth` or `custom`. Kinetix stores manual values as generic strings, so imported manual `token` and `custom` kinds are exported as `api_key` later. Auth-flow `oauth` versus `custom` is preserved. Kinetix matches `provider` to an already configured provider name and `label` to an account label. Bundles do not create providers or transfer provider URLs, account policy, or health state.

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

The descriptor is authenticated as AES-GCM additional data: the UTF-8 prefix `kinetix:credential-interchange:v1`, one NUL byte, then compact UTF-8 JSON for the descriptor. Object keys are recursively sorted lexicographically; array order is preserved. A changed descriptor, nonce, payload, salt, or passphrase fails decryption. Store secret-inclusive bundle files as sensitive material; Kinetix never exports their plaintext secrets.

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

Supply `passphrase` when the bundle contains secret envelopes. Dry-run returns `valid`, `plan`, `problems`, `conflicts`, `warnings`, and `missing_resources`. It writes nothing. Applying reruns the same validation inside one SQLite write transaction; any failed write rolls back the whole import. Imported secrets are decrypted only in memory and re-encrypted with the destination's local key.

Kinetix resolves descriptors against existing providers by name and validates the explicit credential mode. Auth-flow imports also require matching Kinetix integration bindings; installed plugin manifests are authoritative. An unavailable plugin integration is reported in `missing_resources`; the credential can still be stored, but the plugin must be installed before it can be used. No plugin-private KV state is imported.

A descriptor without an envelope is validation-only: it can be checked or used as a template, but does not create a credential. Existing provider/label matches with an envelope are reported as conflicts and left unchanged unless `replace_existing: true`; replacement changes only credential material. New accounts use local defaults. A `none` descriptor verifies a credential-free provider and makes no account changes.

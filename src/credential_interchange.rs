//! Versioned, provider-neutral credential descriptors and encrypted secret envelopes.
//!
//! Bundle secrets are encrypted independently of Kinetix's at-rest key so the
//! interchange file can move between installations. The bundle-level KDF
//! parameters derive one key shared by its individual AES-GCM envelopes.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;

pub const BUNDLE_SCHEMA: &str = "llm-credential-bundle/v1";
pub const DESCRIPTOR_SCHEMA: &str = "llm-credential/v1";
pub const SECRET_ENVELOPE_FORMAT: &str = "llm-secret-envelope/v1";
pub const KINETIX_EXTENSION: &str = "org.prightcord.kinetix";
pub const KDF_NAME: &str = "PBKDF2-HMAC-SHA256";
pub const CIPHER_NAME: &str = "AES-256-GCM";
pub const KDF_ITERATIONS: u32 = 600_000;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
pub const MIN_EXPORT_PASSPHRASE_CHARS: usize = 12;
const MAX_CREDENTIALS: usize = 10_000;
const MAX_ENVELOPE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
    Token,
    Custom,
    None,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuth => "oauth",
            Self::Token => "token",
            Self::Custom => "custom",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialDescriptor {
    pub schema: String,
    pub kind: CredentialKind,
    pub provider: String,
    pub label: String,
    #[serde(default = "empty_object")]
    pub metadata: Value,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretEnvelope {
    pub format: String,
    pub cipher: String,
    pub nonce: String,
    pub payload: String,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRecord {
    pub descriptor: CredentialDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_envelope: Option<SecretEnvelope>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleEncryption {
    pub kdf: String,
    pub iterations: u32,
    pub salt: String,
    pub cipher: String,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialBundle {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<BundleEncryption>,
    pub credentials: Vec<CredentialRecord>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

impl CredentialBundle {
    pub fn from_value(value: Value) -> Result<Self, String> {
        let bundle: Self = serde_json::from_value(value)
            .map_err(|_| "credential bundle has an invalid structure".to_string())?;
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != BUNDLE_SCHEMA {
            return Err(format!(
                "unsupported credential bundle schema '{}'; expected '{BUNDLE_SCHEMA}'",
                self.schema
            ));
        }
        if self.credentials.len() > MAX_CREDENTIALS {
            return Err(format!(
                "credential bundle exceeds the limit of {MAX_CREDENTIALS} descriptors"
            ));
        }

        if let Some(encryption) = &self.encryption {
            if encryption.kdf != KDF_NAME
                || encryption.iterations != KDF_ITERATIONS
                || encryption.cipher != CIPHER_NAME
            {
                return Err("credential bundle uses an unsupported encryption profile".into());
            }
            let salt = decode_base64(&encryption.salt, "credential bundle salt")?;
            if salt.len() != SALT_LEN {
                return Err("credential bundle salt must be 16 bytes".into());
            }
        }

        for record in &self.credentials {
            let descriptor = &record.descriptor;
            if descriptor.schema != DESCRIPTOR_SCHEMA {
                return Err(format!(
                    "unsupported credential descriptor schema '{}'; expected '{DESCRIPTOR_SCHEMA}'",
                    descriptor.schema
                ));
            }
            if descriptor.provider.trim().is_empty() || descriptor.provider.len() > 256 {
                return Err("credential descriptor provider must be 1-256 bytes".into());
            }
            if descriptor.label.trim().is_empty() || descriptor.label.len() > 256 {
                return Err("credential descriptor label must be 1-256 bytes".into());
            }
            if !descriptor.metadata.is_object() {
                return Err("credential descriptor metadata must be an object".into());
            }
            if descriptor
                .metadata
                .get("extensions")
                .is_some_and(|extensions| !extensions.is_object())
            {
                return Err("credential descriptor metadata.extensions must be an object".into());
            }
            let mode = descriptor
                .metadata
                .get("credential_mode")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "credential descriptor metadata.credential_mode is required".to_string()
                })?;
            if !matches!(mode, "manual" | "auth_flow" | "none") {
                return Err(format!(
                    "credential descriptor has unsupported credential_mode '{mode}'"
                ));
            }
            let kind_matches_mode = match mode {
                "none" => descriptor.kind == CredentialKind::None,
                "manual" => matches!(
                    descriptor.kind,
                    CredentialKind::ApiKey | CredentialKind::Token | CredentialKind::Custom
                ),
                "auth_flow" => matches!(
                    descriptor.kind,
                    CredentialKind::OAuth | CredentialKind::Custom
                ),
                _ => false,
            };
            if !kind_matches_mode {
                return Err(format!(
                    "credential kind '{}' is incompatible with credential_mode '{mode}'",
                    descriptor.kind.as_str()
                ));
            }

            if let Some(envelope) = &record.secret_envelope {
                if descriptor.kind == CredentialKind::None {
                    return Err("no-auth credentials must not contain a secret envelope".into());
                }
                if self.encryption.is_none() {
                    return Err("secret envelope requires bundle encryption parameters".into());
                }
                if envelope.format != SECRET_ENVELOPE_FORMAT || envelope.cipher != CIPHER_NAME {
                    return Err(
                        "credential secret envelope uses an unsupported format or cipher".into(),
                    );
                }
                let nonce = decode_base64(&envelope.nonce, "credential envelope nonce")?;
                if nonce.len() != NONCE_LEN {
                    return Err("credential envelope nonce must be 12 bytes".into());
                }
                let payload = decode_base64(&envelope.payload, "credential envelope payload")?;
                if payload.len() < 16 || payload.len() > MAX_ENVELOPE_BYTES {
                    return Err("credential envelope payload has an invalid size".into());
                }
            }
        }
        Ok(())
    }

    pub fn has_secret_envelopes(&self) -> bool {
        self.credentials
            .iter()
            .any(|record| record.secret_envelope.is_some())
    }
}

/// Encrypt per-record secret strings with one random salt and key per bundle.
pub fn encrypt_secrets(
    passphrase: &str,
    descriptors: &[CredentialDescriptor],
    secrets: &[Option<String>],
) -> Result<(BundleEncryption, Vec<Option<SecretEnvelope>>), String> {
    if passphrase.trim().chars().count() < MIN_EXPORT_PASSPHRASE_CHARS {
        return Err(format!(
            "secret-inclusive export requires a passphrase of at least {MIN_EXPORT_PASSPHRASE_CHARS} characters"
        ));
    }
    if descriptors.len() != secrets.len() {
        return Err("credential descriptors and secrets do not match".into());
    }

    let mut salt = [0_u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let key = derive_key(passphrase, &salt);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "credential encryption failed")?;
    let mut envelopes = Vec::with_capacity(secrets.len());

    for (descriptor, secret) in descriptors.iter().zip(secrets) {
        let Some(secret) = secret else {
            envelopes.push(None);
            continue;
        };
        let mut nonce = [0_u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let aad = descriptor_aad(descriptor)?;
        let payload = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: secret.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| "credential encryption failed")?;
        envelopes.push(Some(SecretEnvelope {
            format: SECRET_ENVELOPE_FORMAT.into(),
            cipher: CIPHER_NAME.into(),
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
            payload: base64::engine::general_purpose::STANDARD.encode(payload),
            extensions: Map::new(),
        }));
    }

    Ok((
        BundleEncryption {
            kdf: KDF_NAME.into(),
            iterations: KDF_ITERATIONS,
            salt: base64::engine::general_purpose::STANDARD.encode(salt),
            cipher: CIPHER_NAME.into(),
            extensions: Map::new(),
        },
        envelopes,
    ))
}

/// Decrypts bundle envelopes. A missing passphrase is permitted only for a
/// descriptor-only bundle.
pub fn decrypt_secrets(
    bundle: &CredentialBundle,
    passphrase: Option<&str>,
) -> Result<Vec<Option<String>>, String> {
    bundle.validate()?;
    if !bundle.has_secret_envelopes() {
        return Ok(vec![None; bundle.credentials.len()]);
    }
    let passphrase = passphrase
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "a passphrase is required to import encrypted credential secrets".to_string()
        })?;
    let encryption = bundle
        .encryption
        .as_ref()
        .ok_or_else(|| "secret envelope requires bundle encryption parameters".to_string())?;
    let salt = decode_base64(&encryption.salt, "credential bundle salt")?;
    let key = derive_key(passphrase, &salt);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "credential decryption failed")?;

    bundle
        .credentials
        .iter()
        .map(|record| {
            let Some(envelope) = &record.secret_envelope else {
                return Ok(None);
            };
            let nonce = decode_base64(&envelope.nonce, "credential envelope nonce")?;
            let payload = decode_base64(&envelope.payload, "credential envelope payload")?;
            let aad = descriptor_aad(&record.descriptor)?;
            let plaintext = cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &payload,
                        aad: &aad,
                    },
                )
                .map_err(|_| {
                    "credential secret could not be decrypted; check the passphrase or bundle integrity"
                        .to_string()
                })?;
            String::from_utf8(plaintext)
                .map(Some)
                .map_err(|_| "decrypted credential secret is not UTF-8".to_string())
        })
        .collect()
}

fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0_u8; KEY_LEN];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, KDF_ITERATIONS, &mut key);
    key
}

fn decode_base64(value: &str, field: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| format!("{field} must be valid base64"))
}

fn descriptor_aad(descriptor: &CredentialDescriptor) -> Result<Vec<u8>, String> {
    let value = serde_json::to_value(descriptor)
        .map_err(|_| "credential descriptor could not be serialized".to_string())?;
    let mut bytes = b"kinetix:credential-interchange:v1\0".to_vec();
    bytes.extend_from_slice(
        &serde_json::to_vec(&canonicalize_json(value))
            .map_err(|_| "credential descriptor could not be serialized".to_string())?,
    );
    Ok(bytes)
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize_json(value));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor() -> CredentialDescriptor {
        CredentialDescriptor {
            schema: DESCRIPTOR_SCHEMA.into(),
            kind: CredentialKind::ApiKey,
            provider: "anthropic".into(),
            label: "work".into(),
            metadata: json!({
                "credential_mode": "manual",
                "extensions": {
                    "org.prightcord.kinetix": {"auth_scheme": "bearer"}
                }
            }),
            extensions: Map::new(),
        }
    }

    fn encrypted_bundle() -> CredentialBundle {
        let descriptor = descriptor();
        let (encryption, mut envelopes) = encrypt_secrets(
            "a correct horse battery staple",
            std::slice::from_ref(&descriptor),
            &[Some("portable-secret".into())],
        )
        .unwrap();
        CredentialBundle {
            schema: BUNDLE_SCHEMA.into(),
            encryption: Some(encryption),
            credentials: vec![CredentialRecord {
                descriptor,
                secret_envelope: envelopes.pop().unwrap(),
                extensions: Map::new(),
            }],
            extensions: Map::new(),
        }
    }

    #[test]
    fn encrypted_secret_round_trips_and_rejects_wrong_passphrase() {
        let bundle = encrypted_bundle();
        assert_eq!(
            decrypt_secrets(&bundle, Some("a correct horse battery staple")).unwrap(),
            vec![Some("portable-secret".into())]
        );
        assert!(decrypt_secrets(&bundle, Some("a different passphrase")).is_err());
    }

    #[test]
    fn secret_envelope_authenticates_its_descriptor() {
        let mut bundle = encrypted_bundle();
        bundle.credentials[0].descriptor.provider = "openai".into();
        assert!(decrypt_secrets(&bundle, Some("a correct horse battery staple")).is_err());
    }

    #[test]
    fn secret_export_rejects_short_passphrases() {
        let error =
            encrypt_secrets("short", &[descriptor()], &[Some("secret".into())]).unwrap_err();
        assert!(error.contains("at least 12 characters"));
    }

    #[test]
    fn descriptor_only_bundle_needs_no_passphrase() {
        let bundle = CredentialBundle {
            schema: BUNDLE_SCHEMA.into(),
            encryption: None,
            credentials: vec![CredentialRecord {
                descriptor: descriptor(),
                secret_envelope: None,
                extensions: Map::new(),
            }],
            extensions: Map::new(),
        };
        assert_eq!(decrypt_secrets(&bundle, None).unwrap(), vec![None]);
    }

    #[test]
    fn rejects_malformed_and_unsupported_schemas() {
        let error = CredentialBundle::from_value(json!({
            "schema": "llm-credential-bundle/v99",
            "credentials": []
        }))
        .unwrap_err();
        assert!(error.contains("unsupported credential bundle schema"));

        let error = CredentialBundle::from_value(json!({
            "schema": BUNDLE_SCHEMA,
            "credentials": [{
                "descriptor": {
                    "schema": DESCRIPTOR_SCHEMA,
                    "kind": "api_key",
                    "provider": "anthropic",
                    "label": "work",
                    "metadata": {}
                }
            }]
        }))
        .unwrap_err();
        assert!(error.contains("credential_mode is required"));
    }
}

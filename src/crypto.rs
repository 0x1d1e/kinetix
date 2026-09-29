//! Secret handling: AES-256-GCM encryption for upstream credentials at rest,
//! SHA-256 hashing for virtual keys, and redaction helpers for logs.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub struct Crypto {
    cipher: Aes256Gcm,
    /// A separate cipher for host-managed plugin KV, derived from the same
    /// master key under a distinct label (§10). Kept separate so plugin KV is
    /// never encrypted under the provider-credential key.
    kv_cipher: Aes256Gcm,
    /// A separate cipher for opaque provider continuation state (e.g. Gemini
    /// `thoughtSignature`), derived from the same master key under a distinct
    /// label. Kept separate so a leaked provider-credential key (or vice
    /// versa) cannot decrypt opaque continuation tokens.
    opaque_state_cipher: Aes256Gcm,
    /// Dedicated, master-key-derived HMAC key for API-v2 plugin session IDs.
    plugin_session_hmac_key: [u8; 32],
}

impl Crypto {
    pub fn new(master_key: &[u8; 32]) -> Self {
        let key = Key::<Aes256Gcm>::from_slice(master_key);
        // Derive a distinct 32-byte key for plugin KV: SHA-256("kinetix-plugin-kv" || master).
        let mut hasher = Sha256::new();
        hasher.update(b"kinetix-plugin-kv");
        hasher.update(master_key);
        let derived = hasher.finalize();
        let kv_key = Key::<Aes256Gcm>::from_slice(&derived);
        // Derive a distinct 32-byte key for opaque provider continuation
        // state: SHA-256("kinetix-opaque-provider-state" || master).
        let mut opaque_hasher = Sha256::new();
        opaque_hasher.update(b"kinetix-opaque-provider-state");
        opaque_hasher.update(master_key);
        let opaque_derived = opaque_hasher.finalize();
        let opaque_key = Key::<Aes256Gcm>::from_slice(&opaque_derived);
        // Key-separate plugin session IDs from credentials, plugin KV, and
        // opaque provider state. The same master key yields stable IDs across
        // process restarts, while the client value itself stays host-internal.
        let mut session_key_mac = <Hmac<Sha256> as Mac>::new_from_slice(master_key)
            .expect("HMAC accepts keys of any length");
        session_key_mac.update(b"kinetix:plugin-session-identity:v1");
        let plugin_session_hmac_key = session_key_mac.finalize().into_bytes().into();
        Crypto {
            cipher: Aes256Gcm::new(key),
            kv_cipher: Aes256Gcm::new(kv_key),
            opaque_state_cipher: Aes256Gcm::new(opaque_key),
            plugin_session_hmac_key,
        }
    }

    /// Derive a stable opaque identity for a recognized client session before
    /// it crosses the API-v2 plugin boundary. Stability depends on retaining
    /// the Kinetix master key; raw client identities are never returned.
    pub fn opaque_plugin_session_identity(&self, raw_session: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.plugin_session_hmac_key)
            .expect("HMAC accepts keys of any length");
        mac.update(raw_session.as_bytes());
        format!("kxs_v1_{}", hex::encode(mac.finalize().into_bytes()))
    }

    /// Encrypt a secret, returning base64(nonce || ciphertext).
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        Self::encrypt_with(&self.cipher, plaintext)
    }

    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        Self::decrypt_with(&self.cipher, encoded)
    }

    /// Encrypt plugin KV under the separate derived key label (§10).
    pub fn encrypt_kv(&self, plaintext: &str) -> Result<String> {
        Self::encrypt_with(&self.kv_cipher, plaintext)
    }

    pub fn decrypt_kv(&self, encoded: &str) -> Result<String> {
        Self::decrypt_with(&self.kv_cipher, encoded)
    }

    /// Encrypt opaque provider continuation state (e.g. Gemini
    /// `thoughtSignature`) under the dedicated derived key label. Never reuse
    /// the provider-credential or plugin-KV ciphers for this: the signature is
    /// a provider secret-like token and must be isolated from both.
    pub fn encrypt_opaque_state(&self, plaintext: &str) -> Result<String> {
        Self::encrypt_with(&self.opaque_state_cipher, plaintext)
    }

    pub fn decrypt_opaque_state(&self, encoded: &str) -> Result<String> {
        Self::decrypt_with(&self.opaque_state_cipher, encoded)
    }

    fn encrypt_with(cipher: &Aes256Gcm, plaintext: &str) -> Result<String> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(nonce.len() + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(base64::engine::general_purpose::STANDARD.encode(out))
    }

    fn decrypt_with(cipher: &Aes256Gcm, encoded: &str) -> Result<String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| anyhow!("invalid ciphertext encoding: {e}"))?;
        if raw.len() < 12 {
            return Err(anyhow!("ciphertext too short"));
        }
        let (nonce_bytes, ct) = raw.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let pt = cipher
            .decrypt(nonce, ct)
            .map_err(|_| anyhow!("decryption failed (wrong master key?)"))?;
        String::from_utf8(pt).map_err(|e| anyhow!("decrypted secret not utf-8: {e}"))
    }
}

/// Generate a new virtual key with the `sk-kinetix-` prefix.
pub fn generate_virtual_key() -> String {
    let mut bytes = [0u8; 24];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("sk-kinetix-{}", hex::encode(bytes))
}

/// SHA-256 hash of a virtual key, hex encoded. Constant-time compared.
pub fn hash_virtual_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Constant-time comparison of two hex hashes.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Mask a secret for display: keep the first 4 and last 4 characters.
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 10 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

/// Redact anything that looks like a secret from a string destined for logs.
pub fn redact(text: &str) -> String {
    // Replace known API key prefixes and long token-like substrings.
    let mut out = String::with_capacity(text.len());
    for token in
        text.split_inclusive(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '=')
    {
        let trimmed = token.trim_matches(|c: char| {
            c.is_whitespace()
                || c == '"'
                || c == '\''
                || c == ','
                || c == '{'
                || c == '}'
                || c == '='
        });
        if looks_like_secret(trimmed) {
            out.push_str("[REDACTED]");
            // preserve trailing delimiter
            if let Some(last) = token.chars().last() {
                if last.is_whitespace() || last == '"' || last == '\'' || last == '=' {
                    out.push(last);
                }
            }
        } else {
            out.push_str(token);
        }
    }
    out
}

fn looks_like_secret(s: &str) -> bool {
    const PREFIXES: [&str; 6] = ["sk-", "AIza", "AQ.", "gsk_", "sk-ant", "ya29."];
    if PREFIXES.iter().any(|p| s.starts_with(p)) && s.len() > 12 {
        return true;
    }
    // Long high-entropy tokens (>=40 chars, mostly alphanumeric)
    s.len() >= 40 && s.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= 36
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encryption() {
        let c = Crypto::new(&[7u8; 32]);
        let ct = c.encrypt("AIzaSy-super-secret").unwrap();
        assert_ne!(ct, "AIzaSy-super-secret");
        assert_eq!(c.decrypt(&ct).unwrap(), "AIzaSy-super-secret");
    }

    #[test]
    fn redacts_prefixes() {
        let out = redact("key=AIzaSyABCDEFGHIJKLMNOP");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn kv_uses_a_separate_key_label() {
        let c = Crypto::new(&[7u8; 32]);
        // KV ciphertext must not decrypt under the credential key (or vice
        // versa) because the two ciphers use different derived keys.
        let kv = c.encrypt_kv("refresh-token").unwrap();
        assert!(c.decrypt(&kv).is_err());
        assert_eq!(c.decrypt_kv(&kv).unwrap(), "refresh-token");
        let cred = c.encrypt("sk-secret").unwrap();
        assert!(c.decrypt_kv(&cred).is_err());
    }

    #[test]
    fn plugin_session_identity_is_stable_opaque_and_session_specific() {
        let raw_session = "customer@example.com";
        let first_crypto = Crypto::new(&[7u8; 32]);
        let identity = first_crypto.opaque_plugin_session_identity(raw_session);

        assert_eq!(
            identity,
            Crypto::new(&[7u8; 32]).opaque_plugin_session_identity(raw_session),
            "a persistent master key must preserve session IDs across restarts"
        );
        assert_eq!(
            identity,
            first_crypto.opaque_plugin_session_identity(raw_session),
            "retries and fallback attempts must derive the same ID"
        );
        assert_ne!(
            identity,
            first_crypto.opaque_plugin_session_identity("internal-ticket-123")
        );
        assert!(!identity.contains(raw_session));
        assert_ne!(
            identity,
            Crypto::new(&[8u8; 32]).opaque_plugin_session_identity(raw_session),
            "session IDs are protected by the deployment master key"
        );
    }

    #[test]
    fn opaque_state_roundtrip_and_key_isolation() {
        let c = Crypto::new(&[7u8; 32]);
        let ct = c.encrypt_opaque_state("OPAQUE_THOUGHT_SIGNATURE").unwrap();
        assert_ne!(ct, "OPAQUE_THOUGHT_SIGNATURE");
        assert_eq!(
            c.decrypt_opaque_state(&ct).unwrap(),
            "OPAQUE_THOUGHT_SIGNATURE"
        );

        // The credential cipher must not decrypt opaque-state ciphertext.
        assert!(c.decrypt(&ct).is_err());
        // The plugin-KV cipher must not decrypt opaque-state ciphertext either.
        assert!(c.decrypt_kv(&ct).is_err());

        // Nor may opaque-state decrypt credential/KV ciphertext.
        let cred = c.encrypt("sk-secret").unwrap();
        assert!(c.decrypt_opaque_state(&cred).is_err());
        let kv = c.encrypt_kv("refresh-token").unwrap();
        assert!(c.decrypt_opaque_state(&kv).is_err());
    }
}

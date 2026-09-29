//! Package (`.kxp`) reading, validation, and signing (§5, §11, §12).
//!
//! A `.kxp` is a deterministic archive containing `plugin.toml`, `plugin.wasm`,
//! `README.md`, `LICENSE`, and an optional `signature.ed25519`. Reading is
//! untrusted-input handling: paths are validated, sizes are bounded, and the
//! archive is never extracted to the filesystem.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

use super::manifest::{parse_and_validate, HostPolicy, ValidatedManifest};

/// Maximum compressed package size accepted from a URL or local file.
pub const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum uncompressed component size.
pub const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum size of any single non-component archive entry (README/LICENSE).
const MAX_ENTRY_BYTES: u64 = 2 * 1024 * 1024;

/// Signature verification status stored on the plugin row (§12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    /// No `signature.ed25519` present.
    Unsigned,
    /// Signature present and verified against a trusted publisher key.
    Verified,
    /// Signature present but not trusted; install requires explicit override.
    Untrusted,
}

impl SignatureStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SignatureStatus::Unsigned => "unsigned",
            SignatureStatus::Verified => "verified",
            SignatureStatus::Untrusted => "untrusted",
        }
    }
}

/// The contents of a validated package.
pub struct Package {
    pub manifest_toml: String,
    pub component: Vec<u8>,
    pub readme: Option<String>,
    pub license: Option<String>,
    /// Raw signature bytes, when present.
    pub signature: Option<Vec<u8>>,
    /// SHA-256 (hex) of the whole package as read.
    pub package_sha256: String,
}

/// Read and structurally validate a package from a byte buffer. This performs
/// no signature verification; call [`verify_signature`] separately.
pub fn read_package(bytes: &[u8]) -> Result<Package> {
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        bail!(
            "package is {} bytes, exceeding the {} byte limit",
            bytes.len(),
            MAX_PACKAGE_BYTES
        );
    }
    let package_sha256 = hex::encode(Sha256::digest(bytes));

    let mut archive = tar::Archive::new(bytes);
    let mut manifest_toml: Option<String> = None;
    let mut component: Option<Vec<u8>> = None;
    let mut readme: Option<String> = None;
    let mut license: Option<String> = None;
    let mut signature: Option<Vec<u8>> = None;
    let mut seen_entries = std::collections::HashSet::new();

    for entry in archive.entries().context("reading .kxp entries")? {
        let mut entry = entry.context("reading .kxp entry")?;
        let path = entry.path().context("reading entry path")?.into_owned();
        // §23: reject traversal and absolute/odd paths outright.
        validate_entry_path(&path)?;
        if !entry.header().entry_type().is_file() {
            bail!("archive entry '{}' is not a regular file", path.display());
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !seen_entries.insert(name.clone()) {
            bail!("duplicate archive entry '{name}'");
        }
        let size = entry.header().size().unwrap_or(0);

        match name.as_str() {
            "plugin.toml" => {
                if size > MAX_ENTRY_BYTES {
                    bail!("plugin.toml too large");
                }
                manifest_toml = Some(read_to_string(&mut entry)?);
            }
            "plugin.wasm" => {
                if size > MAX_COMPONENT_BYTES {
                    bail!("plugin.wasm is {size} bytes, exceeding the component limit");
                }
                let mut buf = Vec::with_capacity(size as usize);
                entry.read_to_end(&mut buf)?;
                component = Some(buf);
            }
            "signature.ed25519" => {
                if size > 4096 {
                    bail!("signature.ed25519 too large");
                }
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                signature = Some(buf);
            }
            "README.md" => readme = Some(read_bounded(&mut entry, MAX_ENTRY_BYTES)?),
            "LICENSE" => license = Some(read_bounded(&mut entry, MAX_ENTRY_BYTES)?),
            _ => {
                // Unknown top-level entries are tolerated but not used.
            }
        }
    }

    let manifest_toml = manifest_toml.ok_or_else(|| anyhow!("package is missing plugin.toml"))?;
    let component = component.ok_or_else(|| anyhow!("package is missing plugin.wasm"))?;
    if component.len() < 8 || &component[..4] != b"\0asm" {
        bail!("plugin.wasm is not a WebAssembly module (bad magic)");
    }

    Ok(Package {
        manifest_toml,
        component,
        readme,
        license,
        signature,
        package_sha256,
    })
}

/// Parse and validate a package's manifest under host policy.
pub fn validate_manifest(pkg: &Package, policy: HostPolicy) -> Result<ValidatedManifest> {
    parse_and_validate(&pkg.manifest_toml, policy)
}

/// Verify a package signature against the operator's trusted publisher keys
/// (§12). Returns the status; the caller decides whether to accept `Untrusted`.
pub fn verify_signature(pkg: &Package, trusted_keys: &[[u8; 32]]) -> Result<SignatureStatus> {
    let Some(sig_bytes) = &pkg.signature else {
        return Ok(SignatureStatus::Unsigned);
    };
    // The signed payload is the SHA-256 digest of the package's component bytes
    // plus its manifest. The signature file is either 64 raw bytes or base64.
    let sig: [u8; 64] = if sig_bytes.len() == 64 {
        sig_bytes.as_slice().try_into().unwrap()
    } else {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(String::from_utf8_lossy(sig_bytes).trim())
            .context("signature.ed25519 is not 64 bytes or valid base64")?;
        decoded
            .try_into()
            .map_err(|_| anyhow!("signature must be 64 bytes"))?
    };

    let mut hasher = Sha256::new();
    hasher.update(&pkg.component);
    hasher.update(pkg.manifest_toml.as_bytes());
    let digest = hasher.finalize();

    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let sig = Signature::from_bytes(&sig);
    for key_bytes in trusted_keys {
        let Ok(vk) = VerifyingKey::from_bytes(key_bytes) else {
            continue;
        };
        if vk.verify(digest.as_slice(), &sig).is_ok() {
            return Ok(SignatureStatus::Verified);
        }
    }
    Ok(SignatureStatus::Untrusted)
}

/// Read a package from a local file path.
pub fn read_package_file(path: &Path) -> Result<Package> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("reading package {}", path.display()))?;
    if meta.len() > MAX_PACKAGE_BYTES {
        bail!(
            "package is {} bytes, exceeding the {} byte limit",
            meta.len(),
            MAX_PACKAGE_BYTES
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    read_package(&bytes)
}

/// Compute the SHA-256 of an arbitrary byte buffer, hex encoded.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn validate_entry_path(path: &Path) -> Result<()> {
    let s = path.to_string_lossy();
    if s.is_empty() {
        bail!("empty archive entry path");
    }
    if s.starts_with('/') || s.contains('\\') {
        bail!("archive entry '{s}' has an absolute or invalid path");
    }
    for comp in path.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir => bail!("archive entry '{s}' contains a '..' traversal"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry '{s}' has an absolute path")
            }
            _ => {}
        }
    }
    if s.contains('/') || path.components().count() != 1 {
        bail!("archive entry '{s}' must be a top-level file");
    }
    Ok(())
}

fn read_to_string<R: Read>(r: &mut R) -> Result<String> {
    let mut s = String::new();
    r.read_to_string(&mut s)?;
    Ok(s)
}

fn read_bounded<R: Read>(r: &mut R, max: u64) -> Result<String> {
    let mut buf = Vec::new();
    let mut limited = r.take(max + 1);
    limited.read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        bail!("archive entry exceeds {max} bytes");
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_kxp(manifest: &str, wasm: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        append(&mut builder, "plugin.toml", manifest.as_bytes());
        append(&mut builder, "plugin.wasm", wasm);
        append(&mut builder, "README.md", b"# Foo");
        builder.into_inner().unwrap()
    }

    fn append(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, data).unwrap();
    }

    #[test]
    fn reads_a_valid_package() {
        let kxp = build_kxp("id = \"x\"", b"\0asm\x01\0\0\0");
        let pkg = read_package(&kxp).unwrap();
        assert_eq!(pkg.component, b"\0asm\x01\0\0\0");
        assert_eq!(pkg.readme.as_deref(), Some("# Foo"));
        assert_eq!(pkg.package_sha256.len(), 64);
    }

    #[test]
    fn rejects_non_wasm_component() {
        let kxp = build_kxp("id = \"x\"", b"not wasm");
        assert!(read_package(&kxp).is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        // `tar::Builder` refuses to write `..` paths, so exercise the validator
        // directly (this is the same guard applied to every archive entry).
        assert!(validate_entry_path(Path::new("../evil")).is_err());
        assert!(validate_entry_path(Path::new("/etc/passwd")).is_err());
        assert!(validate_entry_path(Path::new("sub/../../evil")).is_err());
        assert!(validate_entry_path(Path::new("sub/plugin.wasm")).is_err());
        assert!(validate_entry_path(Path::new("plugin.wasm")).is_ok());
    }

    #[test]
    fn rejects_duplicate_archive_entries() {
        let mut builder = tar::Builder::new(Vec::new());
        append(&mut builder, "plugin.toml", b"first");
        append(&mut builder, "plugin.toml", b"second");
        append(&mut builder, "plugin.wasm", b"\\0asm\\x01\\0\\0\\0");
        let err = read_package(&builder.into_inner().unwrap()).err().unwrap();
        assert!(err.to_string().contains("duplicate archive entry"), "{err}");
    }

    #[test]
    fn rejects_non_file_archive_entries() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "directory", std::io::empty())
            .unwrap();
        let err = read_package(&builder.into_inner().unwrap()).err().unwrap();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn unsigned_package_reports_unsigned() {
        let kxp = build_kxp("id = \"x\"", b"\0asm\x01\0\0\0");
        let pkg = read_package(&kxp).unwrap();
        assert_eq!(
            verify_signature(&pkg, &[]).unwrap(),
            SignatureStatus::Unsigned
        );
    }
}

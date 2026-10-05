//! Self-update support for released Kinetix binaries.
//!
//! Releases are built locally by `scripts/release-local.sh` and publish one
//! `kinetix-<tag>-<target>.tar.gz` plus a canonical `SHA256SUMS`. This module
//! consumes exactly that format; it never builds from source or touches Kinetix
//! configuration/state.

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cli::UpdateArgs;

const REPO: &str = "https://github.com/0x1d1e/kinetix";
const MAX_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;

pub async fn run(args: UpdateArgs) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("compiled Kinetix version is not valid semver")?;
    let target = match args.tag.as_deref() {
        Some(tag) => validate_tag(tag)?,
        None => resolve_latest_tag().await?,
    };
    let target_version = Version::parse(target.trim_start_matches('v'))
        .with_context(|| format!("release tag {target} is not valid semver"))?;
    let triple = host_target()?;

    println!("Checking for updates...");
    println!("Current version: v{current}");
    println!(
        "{} version:  {target}",
        if args.tag.is_some() {
            "Target"
        } else {
            "Latest"
        }
    );

    if args.check {
        if target_version > current {
            println!("A new release is available. Run 'kinetix update' to upgrade.");
        } else if target_version == current {
            println!("Kinetix is up to date.");
        } else {
            println!("The selected release is older than the installed version.");
        }
        println!("Release notes: {REPO}/releases/tag/{target}");
        return Ok(());
    }

    if target_version == current && !args.force {
        println!("Kinetix {target} is already installed. Use --force to reinstall.");
        return Ok(());
    }
    if args.tag.is_none() && target_version < current && !args.force {
        println!(
            "Installed version v{current} is newer than latest release {target}; nothing to do."
        );
        return Ok(());
    }

    println!("Target: {target} ({triple})");
    println!("Release notes: {REPO}/releases/tag/{target}");

    if !args.yes && !args.dry_run {
        print!("Replace the current Kinetix binary? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Update cancelled.");
            return Ok(());
        }
    }

    let asset = format!("kinetix-{target}-{triple}.tar.gz");
    let asset_url = format!("{REPO}/releases/download/{target}/{asset}");
    let sums_url = format!("{REPO}/releases/download/{target}/SHA256SUMS");

    println!("Downloading {asset}...");
    let (archive, sums) = tokio::try_join!(download_bytes(&asset_url), download_bytes(&sums_url))?;
    let sums = std::str::from_utf8(&sums).context("SHA256SUMS is not UTF-8")?;
    let expected =
        checksum_for(sums, &asset).ok_or_else(|| anyhow!("SHA256SUMS does not contain {asset}"))?;
    let actual = hex::encode(Sha256::digest(&archive));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }
    println!("Verified SHA-256: {actual}");

    let nonce = format!("{}-{}", std::process::id(), target.trim_start_matches('v'));
    let archive_path = std::env::temp_dir().join(format!("kinetix-update-{nonce}.tar.gz"));
    fs::write(&archive_path, &archive).context("write downloaded release archive")?;

    if args.dry_run {
        verify_archive_contains_binary(&archive_path)?;
        let _ = fs::remove_file(&archive_path);
        println!("Dry run complete: release downloaded and verified; binary was not modified.");
        return Ok(());
    }

    let current_exe = std::env::current_exe().context("resolve current executable path")?;
    let parent = current_exe
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent directory"))?;
    let staged = parent.join(format!(".kinetix-update-{nonce}"));

    let result = (|| -> Result<()> {
        extract_binary(&archive_path, &staged).with_context(|| {
            format!(
                "stage replacement beside {} (if this is a system install, retry with sudo)",
                current_exe.display()
            )
        })?;
        set_executable(&staged)?;
        fs::rename(&staged, &current_exe).with_context(|| {
            format!(
                "atomically replace {} (if this is a system install, retry with sudo)",
                current_exe.display()
            )
        })?;
        Ok(())
    })();

    let _ = fs::remove_file(&archive_path);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;

    println!("Successfully updated Kinetix to {target}.");
    println!("Binary: {}", current_exe.display());
    println!("If running as a systemd user service: systemctl --user restart kinetix");
    println!("If running as a system service: sudo systemctl restart kinetix");
    Ok(())
}

fn validate_tag(tag: &str) -> Result<String> {
    let tag = tag.trim();
    if !tag.starts_with('v') {
        bail!("release tag must be vX.Y.Z (got {tag})");
    }
    let version = Version::parse(tag.trim_start_matches('v'))
        .with_context(|| format!("release tag must be vX.Y.Z (got {tag})"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("release tag must be vX.Y.Z (got {tag})");
    }
    Ok(tag.to_string())
}

fn host_target() -> Result<&'static str> {
    if std::env::consts::OS != "linux" {
        bail!("self-update currently supports Linux release binaries only");
    }
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        other => bail!("unsupported architecture {other}; need x86_64 or aarch64"),
    }
}

async fn resolve_latest_tag() -> Result<String> {
    let client = http_client()?;
    let response = client
        .get(format!("{REPO}/releases/latest"))
        .send()
        .await
        .context("query latest GitHub release")?
        .error_for_status()
        .context("query latest GitHub release")?;
    ensure_trusted_url(response.url())?;
    let tag = response
        .url()
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .ok_or_else(|| anyhow!("latest release redirect did not contain a tag"))?;
    validate_tag(tag)
}

fn http_client() -> Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if trusted_host(attempt.url().host_str()) {
            attempt.follow()
        } else {
            attempt.error("release download redirected to an untrusted host")
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .user_agent(concat!("kinetix/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build update HTTP client")
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = http_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    ensure_trusted_url(response.url())?;
    if response.content_length().unwrap_or(0) > MAX_DOWNLOAD_BYTES as u64 {
        bail!(
            "release asset exceeds {} MiB safety limit",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        );
    }
    let bytes = response.bytes().await.context("read release asset")?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        bail!(
            "release asset exceeds {} MiB safety limit",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        );
    }
    Ok(bytes.to_vec())
}

fn trusted_host(host: Option<&str>) -> bool {
    matches!(
        host,
        Some("github.com")
            | Some("objects.githubusercontent.com")
            | Some("release-assets.githubusercontent.com")
    )
}

fn ensure_trusted_url(url: &reqwest::Url) -> Result<()> {
    if url.scheme() != "https" || !trusted_host(url.host_str()) {
        bail!("refusing untrusted release URL: {url}");
    }
    Ok(())
}

fn checksum_for<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == asset && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(digest)
    })
}

fn verify_archive_contains_binary(archive: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .arg("kinetix")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .context("run tar to inspect release archive; install tar and retry")?;
    if !status.success() {
        bail!("release archive does not contain the expected kinetix binary");
    }
    Ok(())
}

fn extract_binary(archive: &Path, destination: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .with_context(|| format!("create {}", destination.display()))?;
    let output = Command::new("tar")
        .arg("-xOzf")
        .arg(archive)
        .arg("kinetix")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .output()
        .context("run tar to extract release archive; install tar and retry")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(destination);
        bail!(
            "failed to extract kinetix from release archive: {}",
            message.trim()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod 0755 {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command as CliCommand};
    use clap::Parser;

    #[test]
    fn parses_update_cli_flags() {
        let cli = Cli::try_parse_from([
            "kinetix",
            "update",
            "--check",
            "--tag",
            "v0.4.0",
            "--yes",
            "--force",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            CliCommand::Update(args) => {
                assert!(args.check);
                assert_eq!(args.tag.as_deref(), Some("v0.4.0"));
                assert!(args.yes);
                assert!(args.force);
                assert!(args.dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_release_checksum() {
        let sums = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  other.tar.gz\n\
                    bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  kinetix-v0.4.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "kinetix-v0.4.0-x86_64-unknown-linux-gnu.tar.gz"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn validates_release_tags() {
        assert_eq!(validate_tag("v1.2.3").unwrap(), "v1.2.3");
        assert!(validate_tag("1.2.3").is_err());
        assert!(validate_tag("main").is_err());
    }

    #[test]
    fn trusted_release_hosts_only() {
        assert!(trusted_host(Some("github.com")));
        assert!(trusted_host(Some("release-assets.githubusercontent.com")));
        assert!(!trusted_host(Some("example.com")));
        assert!(!trusted_host(Some("github.com.evil.example")));
    }
}

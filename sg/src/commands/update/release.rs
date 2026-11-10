//! What a release is and how to trust one: its version, its checksum
//! manifest, and the signature over that manifest. Network access goes
//! through `curl`, as `install.sh` does.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::process::{Command, Stdio};

pub(crate) const RELEASES_URL: &str = "https://github.com/abendrothj/simgit/releases";

/// The key `release.yml` signs every `SHA256SUMS` with. Changing it strands
/// every install that trusts only this one; see CONTRIBUTING.md, "Releasing".
pub(crate) const RELEASE_PUBLIC_KEY: &str = include_str!("../../../minisign.pub");

/// Read only by debug builds, so tests can sign their own releases; a
/// shipped binary trusts nothing but `RELEASE_PUBLIC_KEY`.
const TEST_PUBLIC_KEY_VARIABLE: &str = "SIMGIT_UPDATE_PUBLIC_KEY";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// `X.Y.Z` or `vX.Y.Z` and nothing else: a pre-release or build suffix is
    /// not a release `update` moves to.
    pub(crate) fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.').map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            part.parse::<u64>().ok()
        });
        let version = Version {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
        };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub(crate) fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is X.Y.Z")
}

/// The release a `releases/latest` redirect names, if it names one of this
/// repository's.
pub(crate) fn tag_from_redirect(url: &str) -> Option<Version> {
    let tag = url
        .trim()
        .strip_prefix(RELEASES_URL)?
        .strip_prefix("/tag/v")?;
    Version::parse(tag)
}

/// The release archive for the platform this binary was built for.
pub(crate) fn asset_name() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "sg-aarch64-apple-darwin.tar.gz",
        ("macos", "x86_64") => "sg-x86_64-apple-darwin.tar.gz",
        ("linux", "aarch64") => "sg-aarch64-unknown-linux-gnu.tar.gz",
        ("linux", "x86_64") => "sg-x86_64-unknown-linux-gnu.tar.gz",
        (os, arch) => bail!("no simgit release is built for {arch} {os}"),
    })
}

/// The digest `manifest` records for `asset`, held to the same rules as
/// `install.sh`: exactly one `<64 hex digits>  <asset>` line.
pub(crate) fn checksum_for(manifest: &str, asset: &str) -> Result<String> {
    let mut found = None;
    for line in manifest.lines() {
        if !line.contains(asset) {
            continue;
        }
        let digest = line
            .strip_suffix(asset)
            .and_then(|rest| rest.strip_suffix("  "))
            .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
        let Some(digest) = digest else {
            bail!("malformed checksum record for {asset}");
        };
        if found.is_some() {
            bail!("duplicate checksum records for {asset}");
        }
        found = Some(digest.to_ascii_lowercase());
    }
    found.with_context(|| format!("SHA256SUMS has no record for {asset}"))
}

/// Check `signature` (a `.minisig` file's text) over `manifest` with
/// `public_key` (a `.pub` file's text).
pub(crate) fn verify_manifest(manifest: &[u8], signature: &str, public_key: &str) -> Result<()> {
    let key = minisign_verify::PublicKey::decode(public_key)
        .map_err(|error| anyhow::anyhow!("unreadable release public key: {error}"))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|error| anyhow::anyhow!("unreadable release signature: {error}"))?;
    key.verify(manifest, &signature, false)
        .map_err(|error| anyhow::anyhow!("the release signature does not verify: {error}"))
}

/// The key releases are checked against.
pub(crate) fn public_key() -> String {
    test_public_key().unwrap_or_else(|| RELEASE_PUBLIC_KEY.to_owned())
}

/// Whether this binary is one `release.yml` built, the only kind `update`
/// may replace with a release.
pub(crate) fn release_build() -> bool {
    option_env!("SIMGIT_RELEASE_BUILD").is_some() || test_public_key().is_some()
}

fn test_public_key() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(TEST_PUBLIC_KEY_VARIABLE).ok()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `curl` restricted to HTTPS with TLS 1.2 or later, failing on HTTP errors.
pub(crate) fn curl() -> Command {
    let mut command = Command::new("curl");
    command.args(["--proto", "=https", "--tlsv1.2", "-fsS"]);
    command
}

/// The newest published release, from the redirect GitHub serves for
/// `releases/latest`.
pub(crate) fn latest_release() -> Result<Version> {
    let output = curl()
        .args(["-o", "/dev/null", "-w", "%{redirect_url}"])
        .arg(format!("{RELEASES_URL}/latest"))
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("simgit update needs curl, which is not on PATH")
        }
        result => result.context("run curl")?,
    };
    if !output.status.success() {
        bail!(
            "could not find the latest simgit release: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let redirect = String::from_utf8_lossy(&output.stdout);
    tag_from_redirect(&redirect).with_context(|| {
        format!("could not find the latest simgit release: unexpected answer {redirect:?}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> minisign::KeyPair {
        minisign::KeyPair::generate_unencrypted_keypair().unwrap()
    }

    fn public_text(keypair: &minisign::KeyPair) -> String {
        keypair.pk.to_box().unwrap().into_string()
    }

    fn sign(keypair: &minisign::KeyPair, data: &[u8]) -> String {
        minisign::sign(None, &keypair.sk, std::io::Cursor::new(data), None, None)
            .unwrap()
            .into_string()
    }

    #[test]
    fn versions_order_numerically() {
        assert!(Version::parse("v0.10.0").unwrap() > Version::parse("0.9.9").unwrap());
        assert_eq!(Version::parse("v1.2.3").unwrap().to_string(), "1.2.3");
        for text in ["0.5", "0.5.0-rc1", "v", "", "0.5.0.1", "v 0.5.0", "-1.0.0"] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn tag_from_redirect_accepts_only_this_repositorys_tags() {
        assert_eq!(
            tag_from_redirect("https://github.com/abendrothj/simgit/releases/tag/v0.6.0\n"),
            Version::parse("0.6.0")
        );
        for url in [
            "https://github.com/someone/simgit/releases/tag/v0.6.0",
            "https://github.com/abendrothj/simgit/releases/tag/v0.6.0-rc1",
            "https://github.com/abendrothj/simgit/releases/tag/",
            "<html>",
            "",
        ] {
            assert_eq!(tag_from_redirect(url), None, "{url:?}");
        }
    }

    #[test]
    fn checksum_for_is_strict() {
        let digest = "a".repeat(64);
        let manifest = format!("{digest}  sg-x.tar.gz\n{}  sg-y.tar.gz\n", "b".repeat(64));
        assert_eq!(checksum_for(&manifest, "sg-x.tar.gz").unwrap(), digest);
        assert!(checksum_for(&manifest, "sg-z.tar.gz").is_err());
        let duplicate = format!("{digest}  sg-x.tar.gz\n{digest}  sg-x.tar.gz\n");
        assert!(checksum_for(&duplicate, "sg-x.tar.gz").is_err());
        assert!(checksum_for("abc  sg-x.tar.gz\n", "sg-x.tar.gz").is_err());
        assert!(checksum_for(&format!("{digest} sg-x.tar.gz\n"), "sg-x.tar.gz").is_err());
        let not_hex = format!("{}  sg-x.tar.gz\n", "g".repeat(64));
        assert!(checksum_for(&not_hex, "sg-x.tar.gz").is_err());
    }

    #[test]
    fn verify_manifest_accepts_a_signed_manifest_and_rejects_a_changed_one() {
        let keys = keypair();
        let signature = sign(&keys, b"a  sg-x\n");
        assert!(verify_manifest(b"a  sg-x\n", &signature, &public_text(&keys)).is_ok());
        assert!(verify_manifest(b"b  sg-x\n", &signature, &public_text(&keys)).is_err());
        assert!(verify_manifest(b"a  sg-x\n", &signature, &public_text(&keypair())).is_err());
    }

    #[test]
    fn embedded_public_key_is_the_release_key() {
        assert!(RELEASE_PUBLIC_KEY.contains("AF15B6F86449E8D3"));
        let keys = keypair();
        let signature = sign(&keys, b"a  sg-x\n");
        assert!(verify_manifest(b"a  sg-x\n", &signature, RELEASE_PUBLIC_KEY).is_err());
    }

    #[test]
    fn sha256_hex_matches_a_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

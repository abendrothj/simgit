//! `simgit update` and the update notice.

pub(crate) mod notice;
pub(crate) mod release;

use crate::commands::worktree::emit;
use anyhow::{bail, Context, Result};
use clap::Args;
use release::{
    asset_name, checksum_for, curl, current_version, latest_release, public_key, release_build,
    sha256_hex, verify_manifest, Version, RELEASES_URL,
};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

#[derive(Args)]
pub struct Update {
    /// Only report whether an update is available.
    #[arg(long)]
    pub check: bool,
}

/// Who owns the installed binary, and so who may replace it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallMethod {
    /// A release binary installed by `install.sh` or by hand: `update` replaces it.
    SelfManaged,
    Homebrew,
    Cargo,
    /// Built from source, so there is no release binary to replace it with.
    Source,
}

impl InstallMethod {
    pub(crate) fn label(self) -> &'static str {
        match self {
            InstallMethod::SelfManaged => "self",
            InstallMethod::Homebrew => "homebrew",
            InstallMethod::Cargo => "cargo",
            InstallMethod::Source => "source",
        }
    }

    /// The command that updates an install of this kind.
    pub(crate) fn command(self) -> &'static str {
        match self {
            InstallMethod::SelfManaged => "simgit update",
            InstallMethod::Homebrew => "brew upgrade simgit",
            InstallMethod::Cargo | InstallMethod::Source => "cargo install simgit-cli --locked",
        }
    }
}

/// How the binary at `exe` (already canonical) was installed. Homebrew keeps
/// every formula under `Cellar/<name>/`, and cargo installs into
/// `<cargo_home>/bin`, cargo-binstall included.
pub(crate) fn install_method(exe: &Path, cargo_home: &Path, release_build: bool) -> InstallMethod {
    let components: Vec<_> = exe.components().map(|part| part.as_os_str()).collect();
    if components
        .windows(2)
        .any(|pair| pair[0] == "Cellar" && pair[1] == "simgit")
    {
        return InstallMethod::Homebrew;
    }
    if exe.starts_with(cargo_home.join("bin")) {
        return InstallMethod::Cargo;
    }
    if !release_build {
        return InstallMethod::Source;
    }
    InstallMethod::SelfManaged
}

/// `install_method` for the running binary.
pub(crate) fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .unwrap_or_default();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_default();
    let cargo_home = cargo_home.canonicalize().unwrap_or(cargo_home);
    install_method(&exe, &cargo_home, release_build())
}

pub fn update(args: Update, json: bool) -> Result<()> {
    let method = detect_install_method();
    if !args.check && method != InstallMethod::SelfManaged {
        let owner = match method {
            InstallMethod::Homebrew => "Homebrew",
            InstallMethod::Cargo => "cargo",
            _ => "a source build",
        };
        bail!(
            "simgit was installed by {owner}; update it with: {}",
            method.command()
        );
    }
    let current = current_version();
    let latest = latest_release()?;
    if args.check {
        let available = latest > current;
        if json {
            emit(&json!({
                "current": current.to_string(),
                "latest": latest.to_string(),
                "update_available": available,
                "install_method": method.label(),
                "command": method.command(),
            }));
        } else {
            println!("simgit {current}; latest {latest}");
            if available {
                println!("update with: {}", method.command());
            }
        }
        return Ok(());
    }
    if latest <= current {
        if json {
            emit(&json!({ "updated": false, "current": current.to_string() }));
        } else {
            println!("simgit is up to date ({current})");
        }
        return Ok(());
    }
    let dir = install(latest)?;
    if json {
        emit(&json!({
            "updated": true,
            "previous": current.to_string(),
            "current": latest.to_string(),
            "path": dir.display().to_string(),
        }));
    } else {
        println!("updated simgit {current} -> {latest} in {}", dir.display());
    }
    Ok(())
}

/// A directory removed when dropped, whatever happened inside it.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Download release `latest`, trust it only once its signature, its checksum
/// and its own `--version` all agree, then rename it over the binaries in the
/// running binary's directory. Nothing is replaced unless every check passed.
/// Returns that directory.
fn install(latest: Version) -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .context("find the running simgit")?;
    let dir = exe
        .parent()
        .context("the running simgit has no directory")?
        .to_path_buf();
    // Staged next to the binaries, so the final renames stay on one
    // filesystem and are atomic.
    let staging = Staging(dir.join(format!(".simgit-update-{}", Uuid::new_v4())));
    fs::create_dir(&staging.0).with_context(|| {
        format!(
            "cannot write to {}, where simgit is installed",
            dir.display()
        )
    })?;

    let asset = asset_name()?;
    let base = format!("{RELEASES_URL}/download/v{latest}");
    let manifest = download(&format!("{base}/SHA256SUMS"), &staging.0.join("SHA256SUMS"))?;
    let signature = download(
        &format!("{base}/SHA256SUMS.minisig"),
        &staging.0.join("SHA256SUMS.minisig"),
    )?;
    verify_manifest(
        &manifest,
        &String::from_utf8_lossy(&signature),
        &public_key(),
    )?;
    let expected = checksum_for(&String::from_utf8_lossy(&manifest), asset)?;
    let archive_path = staging.0.join(asset);
    let archive = download(&format!("{base}/{asset}"), &archive_path)?;
    if sha256_hex(&archive) != expected {
        bail!("checksum mismatch for {asset}; nothing was replaced");
    }

    let extracted = Command::new("tar")
        .arg("xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging.0)
        .status()
        .context("run tar to extract the release")?;
    if !extracted.success() {
        bail!("could not extract {asset}");
    }
    let payload = staging.0.join(asset.trim_end_matches(".tar.gz"));
    let binary = fs::read(payload.join("simgit")).context("the release has no simgit binary")?;
    if fs::read(payload.join("sg")).ok().as_deref() != Some(&binary[..]) {
        bail!("the release's simgit and sg binaries differ; nothing was replaced");
    }

    // Replace what is installed: `sg` and `simgit` where present, and the
    // running binary whatever its name.
    let mut names: Vec<std::ffi::OsString> = ["sg", "simgit"]
        .into_iter()
        .filter(|name| dir.join(name).exists())
        .map(Into::into)
        .collect();
    if let Some(own) = exe.file_name() {
        if !names.iter().any(|name| name == own) {
            names.push(own.to_owned());
        }
    }
    let mut staged = Vec::new();
    for name in &names {
        let path = staging.0.join(format!("new-{}", name.to_string_lossy()));
        fs::write(&path, &binary).with_context(|| format!("stage {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        let reported = Command::new(&path)
            .arg("--version")
            .output()
            .with_context(|| format!("run the downloaded simgit {latest}"))?;
        let reported = String::from_utf8_lossy(&reported.stdout).trim().to_owned();
        if reported != format!("simgit {latest}") {
            bail!(
                "the downloaded simgit reports {reported:?}, not simgit {latest}; nothing was replaced"
            );
        }
        staged.push((path, dir.join(name)));
    }
    for (from, to) in staged {
        fs::rename(&from, &to).with_context(|| format!("replace {}", to.display()))?;
    }
    Ok(dir)
}

/// Fetch `url` into `path` and return its bytes.
fn download(url: &str, path: &Path) -> Result<Vec<u8>> {
    let output = curl()
        .arg("-L")
        .arg("-o")
        .arg(path)
        .arg(url)
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
            "could not download {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn install_method_reads_the_binarys_location() {
        let cargo_home = Path::new("/home/u/.cargo");
        let brew = Path::new("/opt/homebrew/Cellar/simgit/0.5.0/bin/simgit");
        assert_eq!(
            install_method(brew, cargo_home, true),
            InstallMethod::Homebrew
        );
        let cargo = Path::new("/home/u/.cargo/bin/simgit");
        assert_eq!(
            install_method(cargo, cargo_home, true),
            InstallMethod::Cargo
        );
        let other = Path::new("/home/u/.local/bin/simgit");
        assert_eq!(
            install_method(other, cargo_home, false),
            InstallMethod::Source
        );
        assert_eq!(
            install_method(other, cargo_home, true),
            InstallMethod::SelfManaged
        );
        // Only a `Cellar/simgit` pair is Homebrew's, not any directory so named.
        let lookalike = Path::new("/home/u/Cellar/tools/simgit");
        assert_eq!(
            install_method(lookalike, cargo_home, true),
            InstallMethod::SelfManaged
        );
    }

    #[test]
    fn each_install_method_names_its_own_update_command() {
        assert_eq!(InstallMethod::SelfManaged.command(), "simgit update");
        assert_eq!(InstallMethod::Homebrew.command(), "brew upgrade simgit");
        assert_eq!(
            InstallMethod::Cargo.command(),
            "cargo install simgit-cli --locked"
        );
        assert_eq!(
            InstallMethod::Source.command(),
            "cargo install simgit-cli --locked"
        );
        assert_eq!(InstallMethod::SelfManaged.label(), "self");
        assert_eq!(InstallMethod::Homebrew.label(), "homebrew");
        assert_eq!(InstallMethod::Cargo.label(), "cargo");
        assert_eq!(InstallMethod::Source.label(), "source");
    }
}

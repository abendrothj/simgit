//! `simgit update` and the update notice.

pub(crate) mod release;

use crate::commands::worktree::emit;
use anyhow::{bail, Result};
use clap::Args;
use release::{current_version, latest_release, release_build};
use serde_json::json;
use std::path::{Path, PathBuf};

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
    bail!(
        "simgit update is not available yet; run: {}",
        method.command()
    )
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

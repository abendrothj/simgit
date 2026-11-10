//! `simgit update` and the update notice, against a release served from disk.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LATEST: &str = "9.9.9";

/// A signed release on disk, a copy of the simgit under test installed the
/// way `install.sh` installs it, and a `curl` first on `PATH` that answers
/// as GitHub does: the `releases/latest` redirect, and each release asset.
/// Every `curl` call's arguments are logged.
struct FakeRelease {
    root: PathBuf,
    installed: PathBuf,
    tools: PathBuf,
    release: PathBuf,
    keys: minisign::KeyPair,
}

impl Drop for FakeRelease {
    fn drop(&mut self) {
        // A test may have made a directory read-only.
        let _ = fs::set_permissions(&self.installed, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl FakeRelease {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("simgit-update-{}", uuid::Uuid::new_v4()));
        let root = {
            fs::create_dir_all(&root).unwrap();
            root.canonicalize().unwrap()
        };
        let fixture = Self {
            installed: root.join("installed"),
            tools: root.join("tools"),
            release: root.join("release"),
            keys: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
            root,
        };
        fs::create_dir_all(&fixture.installed).unwrap();
        fs::create_dir_all(&fixture.tools).unwrap();
        fs::create_dir_all(&fixture.release).unwrap();
        for name in ["simgit", "sg"] {
            fs::copy(env!("CARGO_BIN_EXE_simgit"), fixture.installed.join(name)).unwrap();
        }
        let curl = fixture.tools.join("curl");
        fs::write(
            &curl,
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >>\"$SIMGIT_TEST_ROOT/curl.log\"\n\
             out=\nformat=\nurl=\nwhile [ $# -gt 0 ]; do\n\
             \tcase \"$1\" in\n\t-o) out=$2; shift 2 ;;\n\t-w) format=$2; shift 2 ;;\n\
             \t--proto|--max-time) shift 2 ;;\n\t-*) shift ;;\n\t*) url=$1; shift ;;\n\tesac\n\
             done\n\
             sleep \"${SIMGIT_TEST_CURL_DELAY:-0}\"\n\
             case \"$url\" in\n\
             */releases/latest)\n\
             \t[ \"$format\" = '%{redirect_url}' ] || exit 2\n\
             \tprintf 'https://github.com/abendrothj/simgit/releases/tag/v%s' \"$SIMGIT_TEST_LATEST\" ;;\n\
             */releases/download/*)\n\
             \trest=${url#*/releases/download/}\n\
             \tsource=\"$SIMGIT_TEST_ROOT/release/$rest\"\n\
             \t[ -f \"$source\" ] || exit 22\n\
             \tcp \"$source\" \"$out\" ;;\n\
             *) exit 22 ;;\n\
             esac\n",
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    fn public_key(&self) -> String {
        self.keys.pk.to_box().unwrap().into_string()
    }

    fn command(&self, binary: &Path) -> Command {
        let path = format!("{}:/usr/bin:/bin", self.tools.display());
        let mut command = Command::new(binary);
        command
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SIMGIT_TEST_ROOT", &self.root)
            .env("SIMGIT_TEST_LATEST", LATEST)
            .env("SIMGIT_UPDATE_PUBLIC_KEY", self.public_key())
            .env_remove("CARGO_HOME");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(&self.installed.join("simgit"))
            .args(args)
            .output()
            .unwrap()
    }

    fn curl_log(&self) -> String {
        fs::read_to_string(self.root.join("curl.log")).unwrap_or_default()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn update_check_reports_the_latest_release_as_json() {
    let fixture = FakeRelease::new();
    let output = fixture.run(&["--json", "update", "--check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report,
        serde_json::json!({
            "current": env!("CARGO_PKG_VERSION"),
            "latest": LATEST,
            "update_available": true,
            "install_method": "self",
            "command": "simgit update",
        })
    );
}

#[test]
fn update_check_passes_only_https_flags_to_curl() {
    let fixture = FakeRelease::new();
    let output = fixture.run(&["update", "--check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let log = fixture.curl_log();
    assert!(log.contains("--proto =https"), "{log}");
    assert!(log.contains("--tlsv1.2"), "{log}");
    assert!(
        log.contains("https://github.com/abendrothj/simgit/releases/latest"),
        "{log}"
    );
}

#[test]
fn update_check_names_a_missing_curl() {
    let fixture = FakeRelease::new();
    let output = fixture
        .command(&fixture.installed.join("simgit"))
        .env("PATH", fixture.root.join("no-tools"))
        .args(["--json", "update", "--check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let message = stderr(&output);
    assert!(message.contains("curl"), "{message}");
    assert_eq!(message.trim_end().lines().count(), 1, "{message}");
}

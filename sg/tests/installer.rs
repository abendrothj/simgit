use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A release served from a local directory to the real `install.sh`, which
/// still extracts, checksums, version-checks and stages the binaries on disk.
/// Only the network fetch is replaced, by a `curl` that records its arguments.
struct Release {
    root: PathBuf,
    release: PathBuf,
    bin: PathBuf,
    install: PathBuf,
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Release {
    fn new() -> Self {
        // Tests run in parallel threads, and the clock alone can repeat.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "simgit-install-test-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let fixture = Release {
            release: root.join("release"),
            bin: root.join("bin"),
            install: root.join("installed"),
            root,
        };
        fs::create_dir_all(&fixture.release).expect("create release fixture");
        fs::create_dir_all(&fixture.bin).expect("create tool fixture");

        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "aarch64-apple-darwin",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            other => panic!("unsupported installer test target: {other:?}"),
        };
        let archive_dir = format!("sg-{target}");
        let asset = format!("{archive_dir}.tar.gz");
        let payload = fixture.release.join(&archive_dir);
        fs::create_dir(&payload).expect("create archive directory");
        let binary = payload.join("simgit");
        // `doctor` answers the installer's copy-on-write probe from the test.
        fs::write(
            &binary,
            "#!/bin/sh\ncase \"$*\" in\n*doctor*) printf '{\\n  \"cow_supported\": %s\\n}\\n' \"${SIMGIT_TEST_COW:-null}\" ;;\n\
             *) printf 'simgit 0.0.0\\n' ;;\nesac\n",
        )
        .expect("create binary fixture");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("make binary executable");
        fs::copy(&binary, payload.join("sg")).expect("create alias fixture");
        let packed = Command::new("tar")
            .current_dir(&fixture.release)
            .args(["czf", &asset, &archive_dir])
            .status()
            .expect("package release fixture");
        assert!(packed.success(), "could not package release fixture");

        let checksum = if cfg!(target_os = "macos") {
            Command::new("shasum")
                .args(["-a", "256"])
                .arg(fixture.release.join(&asset))
                .output()
        } else {
            Command::new("sha256sum")
                .arg(fixture.release.join(&asset))
                .output()
        }
        .expect("hash release fixture");
        assert!(checksum.status.success(), "could not hash release fixture");
        let checksum = String::from_utf8(checksum.stdout).expect("checksum is UTF-8");
        let digest = checksum.split_whitespace().next().expect("checksum digest");
        fs::write(
            fixture.release.join("SHA256SUMS"),
            format!("{digest}  {asset}\n"),
        )
        .expect("create checksum manifest");

        // The URL is the last argument and the destination follows `-o`,
        // wherever the options put them. Every call's arguments are logged.
        let curl = fixture.bin.join("curl");
        fs::write(
            &curl,
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >>\"$SIMGIT_TEST_RELEASE/../curl.log\"\n\
             out=\nurl=\nwhile [ $# -gt 0 ]; do\n\
             \tcase \"$1\" in\n\t-o) out=$2; shift 2 ;;\n\t*) url=$1; shift ;;\n\tesac\n\
             done\ncp \"$SIMGIT_TEST_RELEASE/${url##*/}\" \"$out\"\n",
        )
        .expect("create local release fetcher");
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755))
            .expect("make fetcher executable");
        fixture
    }

    fn install(&self, version: &str) -> Output {
        self.install_with(version, &[])
    }

    fn install_with(&self, version: &str, env: &[(&str, &str)]) -> Output {
        let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../install.sh");
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        Command::new("sh")
            .arg(&installer)
            .env("SIMGIT_VERSION", version)
            .env("SIMGIT_INSTALL_DIR", &self.install)
            .env("SIMGIT_TEST_RELEASE", &self.release)
            .env("PATH", &path)
            .envs(env.iter().copied())
            .output()
            .expect("run release installer")
    }

    fn fetches(&self) -> String {
        fs::read_to_string(self.root.join("curl.log")).unwrap_or_default()
    }
}

#[test]
fn pinned_install_refuses_a_different_binary_version() {
    let release = Release::new();

    let mismatched = release.install("v9.9.9");
    assert!(!mismatched.status.success(), "wrong version was installed");
    assert!(
        String::from_utf8_lossy(&mismatched.stderr).contains("does not match requested v9.9.9"),
        "stderr: {}",
        String::from_utf8_lossy(&mismatched.stderr)
    );
    assert!(!release.install.join("simgit").exists());
    assert!(!release.install.join("sg").exists());

    let matching = release.install("v0.0.0");
    assert!(
        matching.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&matching.stderr)
    );
    assert!(release.install.join("simgit").exists());
    assert!(release.install.join("sg").exists());
}

#[test]
fn curl_downloads_only_over_https_with_tls_1_2() {
    let release = Release::new();
    let installed = release.install("v0.0.0");
    assert!(
        installed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let fetches = release.fetches();
    assert_eq!(fetches.lines().count(), 2, "fetches: {fetches}");
    for fetch in fetches.lines() {
        assert!(fetch.contains("--proto =https --tlsv1.2"), "fetch: {fetch}");
    }
}

#[test]
fn a_version_without_its_v_installs_the_tag() {
    let release = Release::new();
    let installed = release.install("0.0.0");
    assert!(
        installed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        release.fetches().contains("/releases/download/v0.0.0/"),
        "fetches: {}",
        release.fetches()
    );
}

#[test]
fn a_malformed_version_is_refused_before_any_download() {
    let release = Release::new();
    for version in [
        "latest",
        "v1.2",
        "1.2.3.4",
        "v1..2",
        "1.2.3-rc1",
        "../v1.2.3",
        "1.2.3 ",
    ] {
        let refused = release.install(version);
        assert!(!refused.status.success(), "{version:?} was accepted");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("SIMGIT_VERSION must look like v1.2.3"),
            "{version:?}: {stderr}"
        );
        assert_eq!(stderr.lines().count(), 1, "{version:?}: {stderr}");
    }
    assert_eq!(release.fetches(), "", "a refused version was downloaded");
    assert!(!release.install.exists());
}

/// The fuse-overlayfs hint is for filesystems without reflink; where simgit's
/// own probe finds copy-on-write clones it would only be noise.
#[cfg(target_os = "linux")]
#[test]
fn the_fuse_overlayfs_hint_appears_only_without_reflink() {
    let has_fuse = Command::new("sh")
        .args(["-c", "command -v fuse-overlayfs"])
        .output()
        .expect("look for fuse-overlayfs")
        .status
        .success();
    if has_fuse {
        eprintln!("skipped: fuse-overlayfs is installed, so the installer never hints");
        return;
    }
    let release = Release::new();
    for (cow, hinted) in [("true", false), ("false", true), ("null", true)] {
        let installed = release.install_with("v0.0.0", &[("SIMGIT_TEST_COW", cow)]);
        assert!(installed.status.success(), "cow={cow}");
        let stdout = String::from_utf8_lossy(&installed.stdout);
        assert_eq!(
            stdout.contains("fuse-overlayfs not found"),
            hinted,
            "cow={cow}: {stdout}"
        );
    }
}

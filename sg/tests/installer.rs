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
    archive_dir: String,
    asset: String,
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
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "aarch64-apple-darwin",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            other => panic!("unsupported installer test target: {other:?}"),
        };
        let archive_dir = format!("sg-{target}");
        let fixture = Release {
            release: root.join("release"),
            asset: format!("{archive_dir}.tar.gz"),
            archive_dir,
            bin: root.join("bin"),
            install: root.join("installed"),
            root,
        };
        fs::create_dir_all(&fixture.release).expect("create release fixture");
        fs::create_dir_all(&fixture.bin).expect("create tool fixture");

        let payload = fixture.payload();
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
        fixture.package();

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

        // The signature's cryptography is tested in Rust; here `minisign`
        // only records how it was asked and answers as the test says.
        fs::write(fixture.release.join("SHA256SUMS.minisig"), "signature\n")
            .expect("create signature fixture");
        let minisign = fixture.bin.join("minisign");
        fs::write(
            &minisign,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$SIMGIT_TEST_RELEASE/../minisign.log\"\n\
             exit \"${SIMGIT_TEST_MINISIGN_EXIT:-0}\"\n",
        )
        .expect("create minisign fixture");
        fs::set_permissions(&minisign, fs::Permissions::from_mode(0o755))
            .expect("make minisign executable");
        fixture
    }

    fn payload(&self) -> PathBuf {
        self.release.join(&self.archive_dir)
    }

    /// Pack the payload directory into the release archive and record its
    /// digest in `SHA256SUMS`, as a release would. Call it again after
    /// changing the payload.
    fn package(&self) {
        let packed = Command::new("tar")
            .current_dir(&self.release)
            .args(["czf", &self.asset, &self.archive_dir])
            .status()
            .expect("package release fixture");
        assert!(packed.success(), "could not package release fixture");

        let archive = self.release.join(&self.asset);
        let checksum = if cfg!(target_os = "macos") {
            Command::new("shasum")
                .args(["-a", "256"])
                .arg(&archive)
                .output()
        } else {
            Command::new("sha256sum").arg(&archive).output()
        }
        .expect("hash release fixture");
        assert!(checksum.status.success(), "could not hash release fixture");
        let checksum = String::from_utf8(checksum.stdout).expect("checksum is UTF-8");
        let digest = checksum.split_whitespace().next().expect("checksum digest");
        self.manifest(&format!("{digest}  {}\n", self.asset));
    }

    fn manifest(&self, contents: &str) {
        fs::write(self.release.join("SHA256SUMS"), contents).expect("create checksum manifest");
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

    /// A directory of the system tools `install.sh` needs, linked from
    /// `PATH`, so a test can leave one out (curl, minisign) whatever the
    /// machine has installed.
    fn system_tools(&self) -> PathBuf {
        let tools = self.root.join("tools");
        fs::create_dir(&tools).unwrap();
        let search = std::env::var_os("PATH").unwrap();
        for tool in [
            "uname",
            "mktemp",
            "rm",
            "tar",
            "gzip",
            "cmp",
            "cp",
            "chmod",
            "mv",
            "mkdir",
            "shasum",
            "sha256sum",
            "sh",
        ] {
            if let Some(found) = std::env::split_paths(&search)
                .map(|directory| directory.join(tool))
                .find(|path| path.is_file())
            {
                std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
            }
        }
        tools
    }

    /// `PATH` with the fixture's `curl` but no `minisign` at all.
    fn path_without_minisign(&self) -> String {
        fs::remove_file(self.bin.join("minisign")).unwrap();
        format!("{}:{}", self.bin.display(), self.system_tools().display())
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
    assert_eq!(fetches.lines().count(), 3, "fetches: {fetches}");
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

/// The installer must stop with `message` and leave nothing installed.
fn assert_refused(release: &Release, message: &str) {
    let refused = release.install("v0.0.0");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "installed despite: {message}");
    assert!(
        stderr.contains(message),
        "wanted {message:?}, got: {stderr}"
    );
    assert!(!release.install.join("simgit").exists());
    assert!(!release.install.join("sg").exists());
}

#[test]
fn a_checksum_mismatch_is_refused() {
    let release = Release::new();
    release.manifest(&format!("{}  {}\n", "0".repeat(64), release.asset));
    assert_refused(&release, "checksum mismatch");
}

#[test]
fn a_manifest_without_the_archive_is_refused() {
    let release = Release::new();
    release.manifest(&format!("{}  sg-other.tar.gz\n", "0".repeat(64)));
    assert_refused(&release, "has no record for");
    release.manifest("");
    assert_refused(&release, "has no record for");
}

#[test]
fn a_duplicate_checksum_record_is_refused() {
    let release = Release::new();
    let manifest = fs::read_to_string(release.release.join("SHA256SUMS")).unwrap();
    // Even two identical records: which one is trusted is not for us to guess.
    release.manifest(&format!("{manifest}{manifest}"));
    assert_refused(&release, "duplicate checksum records");
}

#[test]
fn a_malformed_checksum_record_is_refused() {
    let release = Release::new();
    let digest = "a".repeat(64);
    for (record, message) in [
        (
            format!("{}  {}", &digest[1..], release.asset),
            "malformed checksum for",
        ),
        (
            format!("{digest}0  {}", release.asset),
            "malformed checksum for",
        ),
        (
            format!("{}g  {}", &digest[1..], release.asset),
            "malformed checksum for",
        ),
        (
            format!("{digest} {}", release.asset),
            "malformed checksum record",
        ),
        (
            format!("{digest} extra  {}", release.asset),
            "malformed checksum record",
        ),
        (
            format!("{digest}  {} extra", release.asset),
            "malformed checksum record",
        ),
    ] {
        release.manifest(&format!("{record}\n"));
        assert_refused(&release, message);
    }
}

#[test]
fn an_sg_that_differs_from_simgit_is_refused() {
    let release = Release::new();
    fs::write(release.payload().join("sg"), "#!/bin/sh\nexit 0\n").unwrap();
    release.package();
    assert_refused(&release, "non-equivalent simgit and sg binaries");
}

#[test]
fn an_archive_without_sg_is_refused() {
    let release = Release::new();
    fs::remove_file(release.payload().join("sg")).unwrap();
    release.package();
    assert_refused(
        &release,
        &format!("did not contain {}/sg", release.archive_dir),
    );
}

/// Without curl the installer falls back to wget. The PATH here holds only the
/// tools the installer needs and a `wget` that records its arguments.
#[test]
fn wget_is_used_when_curl_is_missing() {
    let release = Release::new();
    let tools = release.system_tools();
    assert!(!tools.join("curl").exists());
    fs::write(
        release.bin.join("wget"),
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >>\"$SIMGIT_TEST_RELEASE/../wget.log\"\n\
         out=\nurl=\nwhile [ $# -gt 0 ]; do\n\
         \tcase \"$1\" in\n\t-qO|-O) out=$2; shift 2 ;;\n\t-*) shift ;;\n\t*) url=$1; shift ;;\n\tesac\n\
         done\ncp \"$SIMGIT_TEST_RELEASE/${url##*/}\" \"$out\"\n",
    )
    .unwrap();
    fs::set_permissions(release.bin.join("wget"), fs::Permissions::from_mode(0o755)).unwrap();
    // The stub dir goes first, curl's directory is not on PATH at all.
    fs::remove_file(release.bin.join("curl")).unwrap();
    let path = format!("{}:{}", release.bin.display(), tools.display());
    let installed = release.install_with("v0.0.0", &[("PATH", &path)]);
    assert!(
        installed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let log = fs::read_to_string(release.root.join("wget.log")).unwrap();
    assert_eq!(log.lines().count(), 3, "wget calls: {log}");
    assert!(
        log.lines().all(|call| call.contains("https://github.com/")),
        "wget calls: {log}"
    );
    assert_eq!(release.fetches(), "", "curl was used");
    assert!(release.install.join("simgit").exists());
    assert!(release.install.join("sg").exists());
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

#[test]
fn installer_key_matches_the_release_key() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let installer = fs::read_to_string(root.join("../install.sh")).unwrap();
    let release_key = fs::read_to_string(root.join("minisign.pub")).unwrap();
    let release_key = release_key.lines().nth(1).unwrap();
    assert!(
        installer.contains(&format!("PUBLIC_KEY='{release_key}'")),
        "install.sh does not carry sg/minisign.pub's key"
    );
}

#[test]
fn installer_checks_the_signature_with_the_release_key() {
    let release = Release::new();
    let installed = release.install("v0.0.0");
    assert!(
        installed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let calls = fs::read_to_string(release.root.join("minisign.log")).unwrap_or_default();
    let key =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("minisign.pub")).unwrap();
    let key = key.lines().nth(1).unwrap().to_owned();
    assert_eq!(calls.lines().count(), 1, "minisign calls: {calls}");
    assert!(
        calls.starts_with(&format!("-V -P {key} -m ")),
        "minisign calls: {calls}"
    );
    assert!(
        calls.trim_end().ends_with("/SHA256SUMS"),
        "minisign calls: {calls}"
    );
}

#[test]
fn installer_rejects_a_bad_signature_when_minisign_is_present() {
    let release = Release::new();
    let installed = release.install_with("v0.0.0", &[("SIMGIT_TEST_MINISIGN_EXIT", "1")]);
    assert!(!installed.status.success(), "a bad signature was installed");
    assert!(
        String::from_utf8_lossy(&installed.stderr).contains("signature"),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(!release.install.join("simgit").exists());
    assert!(!release.install.join("sg").exists());
}

#[test]
fn installer_requires_minisign_when_asked() {
    let release = Release::new();
    let path = release.path_without_minisign();
    let installed = release.install_with(
        "v0.0.0",
        &[("PATH", &path), ("SIMGIT_REQUIRE_SIGNATURE", "1")],
    );
    assert!(
        !installed.status.success(),
        "installed without a signature check"
    );
    assert!(
        String::from_utf8_lossy(&installed.stderr)
            .contains("SIMGIT_REQUIRE_SIGNATURE is set but minisign is not installed"),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(!release.install.join("simgit").exists());
}

#[test]
fn installer_says_when_it_checked_only_the_checksum() {
    let release = Release::new();
    let path = release.path_without_minisign();
    let installed = release.install_with("v0.0.0", &[("PATH", &path)]);
    assert!(
        installed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        said.contains("minisign not found; verified the checksum only"),
        "output: {said}"
    );
    assert!(release.install.join("simgit").exists());
}

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
             \t--proto|--max-time|--connect-timeout|--speed-limit|--speed-time) shift 2 ;;\n\t-*) shift ;;\n\t*) url=$1; shift ;;\n\tesac\n\
             done\n\
             sleep \"${SIMGIT_TEST_CURL_DELAY:-0}\"\n\
             case \"$url\" in\n\
             */releases/latest)\n\
             \t[ \"$format\" = '%{redirect_url}' ] || exit 2\n\
             \tprintf 'https://github.com/abendrothj/simgit/releases/tag/v%s' \"$SIMGIT_TEST_LATEST\" ;;\n\
             */Formula/simgit.rb)\n\
             \tprintf '  version \"%s\"\\n' \"${SIMGIT_TEST_FORMULA:-0.0.1}\" ;;\n\
             */si/mg/simgit-cli)\n\
             \tprintf '{\"name\":\"simgit-cli\",\"vers\":\"%s\",\"yanked\":false}\\n' \"${SIMGIT_TEST_CRATE:-0.0.1}\" ;;\n\
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

    /// Publish release `v{LATEST}`: an archive of two identical scripts that
    /// report `simgit {reports}`, its `SHA256SUMS`, and that manifest signed
    /// with `keys`.
    fn publish(&self, reports: &str, keys: &minisign::KeyPair) {
        let folder = self.release.join(format!("v{LATEST}"));
        let payload = folder.join(format!("sg-{}", target()));
        fs::create_dir_all(&payload).unwrap();
        for name in ["simgit", "sg"] {
            let script = payload.join(name);
            fs::write(&script, format!("#!/bin/sh\necho 'simgit {reports}'\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let asset = format!("sg-{}.tar.gz", target());
        let packed = Command::new("tar")
            .current_dir(&folder)
            .args(["czf", &asset, &format!("sg-{}", target())])
            .status()
            .unwrap();
        assert!(packed.success());
        let digest = sha256_hex(&fs::read(folder.join(&asset)).unwrap());
        self.sign_manifest(&format!("{digest}  {asset}\n"), keys);
    }

    /// Replace the published `SHA256SUMS` with `manifest`, signed with `keys`.
    fn sign_manifest(&self, manifest: &str, keys: &minisign::KeyPair) {
        let folder = self.release.join(format!("v{LATEST}"));
        fs::write(folder.join("SHA256SUMS"), manifest).unwrap();
        let signature = minisign::sign(
            None,
            &keys.sk,
            std::io::Cursor::new(manifest.as_bytes()),
            None,
            None,
        )
        .unwrap();
        fs::write(folder.join("SHA256SUMS.minisig"), signature.into_string()).unwrap();
    }

    /// The installed binaries' contents, to show a failed update changed nothing.
    fn snapshot(&self) -> Vec<(String, Option<Vec<u8>>)> {
        ["simgit", "sg"]
            .into_iter()
            .map(|name| (name.to_owned(), fs::read(self.installed.join(name)).ok()))
            .collect()
    }

    fn version_of(&self, binary: &Path) -> String {
        let output = Command::new(binary).arg("--version").output().unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

fn target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        other => panic!("no release target for {other:?}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn running_as_root() -> bool {
    let output = Command::new("id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim() == "0"
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

#[test]
fn update_replaces_both_binaries_with_a_verified_release() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let output = fixture.run(&["--json", "update"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["updated"], true, "{report}");
    assert_eq!(report["previous"], env!("CARGO_PKG_VERSION"), "{report}");
    assert_eq!(report["current"], LATEST, "{report}");
    for name in ["simgit", "sg"] {
        let binary = fixture.installed.join(name);
        assert_eq!(
            fixture.version_of(&binary),
            format!("simgit {LATEST}"),
            "{name}"
        );
    }
    let leftovers: Vec<_> = fs::read_dir(&fixture.installed).unwrap().collect();
    assert_eq!(leftovers.len(), 2, "{leftovers:?}");
}

#[test]
fn update_refuses_a_bad_signature_and_replaces_nothing() {
    let fixture = FakeRelease::new();
    let stranger = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    fixture.publish(LATEST, &stranger);
    let before = fixture.snapshot();
    let output = fixture.run(&["update"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("signature"), "{}", stderr(&output));
    assert_eq!(fixture.snapshot(), before);
    assert!(
        !fixture.curl_log().contains(".tar.gz"),
        "{}",
        fixture.curl_log()
    );
}

#[test]
fn update_refuses_a_checksum_mismatch_and_replaces_nothing() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let asset = format!("sg-{}.tar.gz", target());
    fixture.sign_manifest(&format!("{}  {asset}\n", "0".repeat(64)), &fixture.keys);
    let before = fixture.snapshot();
    let output = fixture.run(&["update"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("checksum"), "{}", stderr(&output));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn update_refuses_a_release_reporting_another_version() {
    let fixture = FakeRelease::new();
    fixture.publish("9.9.8", &fixture.keys);
    let before = fixture.snapshot();
    let output = fixture.run(&["update"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("9.9.8"), "{}", stderr(&output));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn update_is_a_no_op_when_already_current() {
    let fixture = FakeRelease::new();
    let output = fixture
        .command(&fixture.installed.join("simgit"))
        .env("SIMGIT_TEST_LATEST", "0.0.1")
        .args(["--json", "update"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report,
        serde_json::json!({"updated": false, "current": env!("CARGO_PKG_VERSION")})
    );
    assert!(
        !fixture.curl_log().contains("/download/"),
        "{}",
        fixture.curl_log()
    );
}

#[test]
fn update_refuses_homebrew_and_cargo_installs() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let brew = fixture.root.join("Cellar/simgit/0.5.0/bin");
    let cargo = fixture.root.join("cargo/bin");
    for (folder, command) in [
        (&brew, "brew upgrade simgit"),
        (&cargo, "cargo install simgit-cli --locked"),
    ] {
        fs::create_dir_all(folder).unwrap();
        let binary = folder.join("simgit");
        fs::copy(env!("CARGO_BIN_EXE_simgit"), &binary).unwrap();
        let before = fs::read(&binary).unwrap();
        let output = fixture
            .command(&binary)
            .env("CARGO_HOME", fixture.root.join("cargo"))
            .arg("update")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(stderr(&output).contains(command), "{}", stderr(&output));
        assert_eq!(fs::read(&binary).unwrap(), before);
    }
    assert_eq!(
        fixture.curl_log(),
        "",
        "nothing is fetched for a refused update"
    );
}

#[test]
fn update_replaces_the_target_of_a_symlinked_binary() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let links = fixture.root.join("links");
    fs::create_dir(&links).unwrap();
    let link = links.join("simgit");
    std::os::unix::fs::symlink(fixture.installed.join("simgit"), &link).unwrap();
    let output = fixture.command(&link).arg("update").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    let installed = fixture.installed.join("simgit");
    assert_eq!(fixture.version_of(&installed), format!("simgit {LATEST}"));
}

#[test]
fn update_reports_an_unwritable_directory_and_replaces_nothing() {
    if running_as_root() {
        eprintln!("skipped: root can write anywhere");
        return;
    }
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let before = fixture.snapshot();
    fs::set_permissions(&fixture.installed, fs::Permissions::from_mode(0o555)).unwrap();
    let output = fixture.run(&["--json", "update"]);
    fs::set_permissions(&fixture.installed, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains(&fixture.installed.display().to_string()),
        "{message}"
    );
    assert_eq!(message.trim_end().lines().count(), 1, "{message}");
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn update_replaces_only_the_binaries_present() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    fs::remove_file(fixture.installed.join("sg")).unwrap();
    let output = fixture.run(&["update"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!fixture.installed.join("sg").exists());
    let installed = fixture.installed.join("simgit");
    assert_eq!(fixture.version_of(&installed), format!("simgit {LATEST}"));
}

const LATEST_URL: &str = "https://github.com/abendrothj/simgit/releases/tag/v9.9.9";

impl FakeRelease {
    fn cache(&self) -> PathBuf {
        self.root.join("cache/simgit/latest")
    }

    /// The installed simgit with `args` under a pseudo-terminal, as a person
    /// runs it; stdout and stderr arrive together on stdout.
    fn on_terminal(&self, args: &[&str], path: &str) -> Command {
        let python = which("python3");
        let mut command = self.command(&python);
        command
            .env("PATH", path)
            .env_remove("CI")
            .env_remove("SIMGIT_NO_UPDATE_CHECK")
            .current_dir(&self.root)
            .args([
                "-c",
                "import os, pty, sys; sys.exit(os.waitstatus_to_exitcode(pty.spawn(sys.argv[1:])))",
            ])
            .arg(self.installed.join("simgit"))
            .args(args);
        command
    }

    fn tools_path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.tools.display())
    }
}

fn which(program: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{program} is not on PATH"))
}

#[test]
fn notice_shows_a_newer_cached_release_on_a_terminal() {
    let fixture = FakeRelease::new();
    fs::create_dir_all(fixture.cache().parent().unwrap()).unwrap();
    fs::write(fixture.cache(), LATEST_URL).unwrap();
    let output = fixture
        .on_terminal(&["doctor"], &fixture.tools_path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(
        shown.contains(&format!(
            "simgit {LATEST} is available (you have {}); update with: simgit update",
            env!("CARGO_PKG_VERSION")
        )),
        "{shown}"
    );
}

#[test]
fn notice_never_reaches_json_or_piped_output() {
    let fixture = FakeRelease::new();
    fs::create_dir_all(fixture.cache().parent().unwrap()).unwrap();
    fs::write(fixture.cache(), LATEST_URL).unwrap();
    let on_terminal = fixture
        .on_terminal(&["--json", "doctor"], &fixture.tools_path())
        .output()
        .unwrap();
    assert!(on_terminal.status.success());
    assert!(!String::from_utf8_lossy(&on_terminal.stdout).contains("is available"));
    let piped = fixture
        .command(&fixture.installed.join("simgit"))
        .env_remove("CI")
        .current_dir(&fixture.root)
        .arg("doctor")
        .output()
        .unwrap();
    assert!(piped.status.success());
    assert!(
        !stderr(&piped).contains("is available"),
        "{}",
        stderr(&piped)
    );
}

#[test]
fn stale_cache_is_refreshed_without_waiting() {
    let fixture = FakeRelease::new();
    let started = std::time::Instant::now();
    // The check takes six seconds; the command must not wait for it.
    let output = fixture
        .on_terminal(&["doctor"], &fixture.tools_path())
        .env("SIMGIT_TEST_CURL_DELAY", "6")
        .output()
        .unwrap();
    let took = started.elapsed();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(took < std::time::Duration::from_secs(3), "waited {took:?}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while fs::read_to_string(fixture.cache()).unwrap_or_default() != LATEST_URL {
        assert!(
            std::time::Instant::now() < deadline,
            "the cache was never refreshed"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn notice_is_silent_when_curl_is_missing() {
    let fixture = FakeRelease::new();
    let empty = fixture.root.join("no-tools");
    fs::create_dir(&empty).unwrap();
    let output = fixture
        .on_terminal(&["doctor"], &empty.display().to_string())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(!shown.contains("curl"), "{shown}");
    assert!(!shown.contains("is available"), "{shown}");
}

/// A user's `~/.curlrc` must not change what simgit asks curl: `location`
/// there would follow the `releases/latest` redirect and lose its answer.
#[test]
fn update_ignores_the_users_curlrc() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let output = fixture.run(&["update"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let log = fixture.curl_log();
    assert!(log.lines().count() >= 4, "{log}");
    for call in log.lines() {
        assert!(call.starts_with("-q "), "-q must come first: {call}");
    }
}

/// A stalled connection must not hang `update`, which agents run unattended.
#[test]
fn update_gives_up_on_a_stalled_connection() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let output = fixture.run(&["update"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let log = fixture.curl_log();
    for call in log.lines() {
        assert!(call.contains("--connect-timeout 10"), "{call}");
    }
    let lookup = log
        .lines()
        .find(|call| call.ends_with("/releases/latest"))
        .unwrap();
    assert!(lookup.contains("--max-time 30"), "{lookup}");
    for download in log
        .lines()
        .filter(|call| call.contains("/releases/download/"))
    {
        assert!(
            download.contains("--speed-limit 1024 --speed-time 30"),
            "{download}"
        );
    }
}

/// Another program's `sg` (ast-grep, shadow-utils) beside a hand-installed
/// simgit is not simgit's to replace.
#[test]
fn update_leaves_another_programs_sg_alone() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    let foreign = "#!/bin/sh\necho ast-grep\n";
    fs::write(fixture.installed.join("sg"), foreign).unwrap();
    let output = fixture.run(&["update"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(fixture.installed.join("sg")).unwrap(),
        foreign
    );
    let installed = fixture.installed.join("simgit");
    assert_eq!(fixture.version_of(&installed), format!("simgit {LATEST}"));
}

/// Run as `sg` with no `simgit` beside it, `update` replaces `sg` alone.
#[test]
fn update_run_as_sg_replaces_only_sg() {
    let fixture = FakeRelease::new();
    fixture.publish(LATEST, &fixture.keys);
    fs::remove_file(fixture.installed.join("simgit")).unwrap();
    let sg = fixture.installed.join("sg");
    let output = fixture.command(&sg).arg("update").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!fixture.installed.join("simgit").exists());
    assert_eq!(fixture.version_of(&sg), format!("simgit {LATEST}"));
}

/// A copy of the simgit under test at `folder`, where a package manager
/// would have put it.
fn installed_at(fixture: &FakeRelease, folder: &Path) -> PathBuf {
    fs::create_dir_all(folder).unwrap();
    let binary = folder.join("simgit");
    fs::copy(env!("CARGO_BIN_EXE_simgit"), &binary).unwrap();
    let _ = fixture;
    binary
}

/// `brew upgrade` offers a release only once the tap's formula names it, so
/// a Homebrew install asks the formula, not GitHub's latest release.
#[test]
fn update_check_reads_homebrews_formula_for_a_homebrew_install() {
    let fixture = FakeRelease::new();
    let binary = installed_at(&fixture, &fixture.root.join("Cellar/simgit/0.5.0/bin"));
    let output = fixture
        .command(&binary)
        .env("SIMGIT_TEST_FORMULA", "9.9.8")
        .args(["--json", "update", "--check"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["latest"], "9.9.8", "{report}");
    assert_eq!(report["install_method"], "homebrew", "{report}");
    assert_eq!(report["command"], "brew upgrade simgit", "{report}");
    assert!(
        fixture.curl_log().contains(
            "https://raw.githubusercontent.com/abendrothj/homebrew-tap/main/Formula/simgit.rb"
        ),
        "{}",
        fixture.curl_log()
    );
}

/// `cargo install` offers a release only once it is on crates.io.
#[test]
fn update_check_reads_crates_io_for_a_cargo_install() {
    let fixture = FakeRelease::new();
    let binary = installed_at(&fixture, &fixture.root.join("cargo/bin"));
    let output = fixture
        .command(&binary)
        .env("CARGO_HOME", fixture.root.join("cargo"))
        .env("SIMGIT_TEST_CRATE", "9.9.7")
        .args(["--json", "update", "--check"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["latest"], "9.9.7", "{report}");
    assert_eq!(report["install_method"], "cargo", "{report}");
    assert!(
        fixture
            .curl_log()
            .contains("https://index.crates.io/si/mg/simgit-cli"),
        "{}",
        fixture.curl_log()
    );
}

/// A Homebrew user hears of a release only once `brew upgrade` can install
/// it, whatever GitHub already lists.
#[test]
fn notice_for_a_homebrew_install_waits_for_the_formula() {
    let fixture = FakeRelease::new();
    let binary = installed_at(&fixture, &fixture.root.join("Cellar/simgit/0.5.0/bin"));
    let cache = fixture.cache().parent().unwrap().to_path_buf();
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("latest"), LATEST_URL).unwrap();
    let python = which("python3");
    let run = |formula: &str| {
        fs::write(
            cache.join("latest-homebrew"),
            format!("  version \"{formula}\"\n"),
        )
        .unwrap();
        let output = fixture
            .command(&python)
            .env_remove("CI")
            .env_remove("SIMGIT_NO_UPDATE_CHECK")
            .current_dir(&fixture.root)
            .args([
                "-c",
                "import os, pty, sys; sys.exit(os.waitstatus_to_exitcode(pty.spawn(sys.argv[1:])))",
            ])
            .arg(&binary)
            .arg("doctor")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let before = run(env!("CARGO_PKG_VERSION"));
    assert!(!before.contains("is available"), "{before}");
    let after = run(LATEST);
    assert!(
        after.contains(&format!(
            "simgit {LATEST} is available (you have {}); update with: brew upgrade simgit",
            env!("CARGO_PKG_VERSION")
        )),
        "{after}"
    );
}

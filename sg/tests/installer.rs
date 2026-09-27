use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn pinned_install_refuses_a_different_binary_version() {
    let root = Scratch(std::env::temp_dir().join(format!(
        "simgit-install-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )));
    let release = root.0.join("release");
    let bin = root.0.join("bin");
    let install = root.0.join("installed");
    fs::create_dir_all(&release).expect("create release fixture");
    fs::create_dir_all(&bin).expect("create tool fixture");

    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        other => panic!("unsupported installer test target: {other:?}"),
    };
    let archive_dir = format!("sg-{target}");
    let asset = format!("{archive_dir}.tar.gz");
    let payload = release.join(&archive_dir);
    fs::create_dir(&payload).expect("create archive directory");
    let binary = payload.join("simgit");
    fs::write(&binary, "#!/bin/sh\nprintf 'simgit 0.0.0\\n'\n").expect("create binary fixture");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
        .expect("make binary executable");
    fs::copy(&binary, payload.join("sg")).expect("create alias fixture");
    let packed = Command::new("tar")
        .current_dir(&release)
        .args(["czf", &asset, &archive_dir])
        .status()
        .expect("package release fixture");
    assert!(packed.success(), "could not package release fixture");

    let checksum = if cfg!(target_os = "macos") {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(release.join(&asset))
            .output()
    } else {
        Command::new("sha256sum").arg(release.join(&asset)).output()
    }
    .expect("hash release fixture");
    assert!(checksum.status.success(), "could not hash release fixture");
    let checksum = String::from_utf8(checksum.stdout).expect("checksum is UTF-8");
    let digest = checksum.split_whitespace().next().expect("checksum digest");
    fs::write(release.join("SHA256SUMS"), format!("{digest}  {asset}\n"))
        .expect("create checksum manifest");

    // Replace only the network fetch. The real installer extracts the archive,
    // checks its checksum and version, and stages the binaries on disk.
    let curl = bin.join("curl");
    fs::write(
        &curl,
        "#!/bin/sh\nset -eu\ncp \"$SIMGIT_TEST_RELEASE/${2##*/}\" \"$4\"\n",
    )
    .expect("create local release fetcher");
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).expect("make fetcher executable");

    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../install.sh");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |version: &str| {
        Command::new("sh")
            .arg(&installer)
            .env("SIMGIT_VERSION", version)
            .env("SIMGIT_INSTALL_DIR", &install)
            .env("SIMGIT_TEST_RELEASE", &release)
            .env("PATH", &path)
            .output()
            .expect("run release installer")
    };

    let mismatched = run("v9.9.9");
    assert!(!mismatched.status.success(), "wrong version was installed");
    assert!(
        String::from_utf8_lossy(&mismatched.stderr).contains("does not match requested v9.9.9"),
        "stderr: {}",
        String::from_utf8_lossy(&mismatched.stderr)
    );
    assert!(!install.join("simgit").exists());
    assert!(!install.join("sg").exists());

    let matching = run("v0.0.0");
    assert!(
        matching.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&matching.stderr)
    );
    assert!(install.join("simgit").exists());
    assert!(install.join("sg").exists());
}

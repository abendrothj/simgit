//! The once-a-day notice that a newer simgit is out. The network check never
//! delays a command: a detached `curl` writes GitHub's answer into a cache
//! file, and a later command reads it.

use super::detect_install_method;
use super::release::{curl, current_version, tag_from_redirect, Version, RELEASES_URL};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

/// How long an answer, or an attempt to get one, counts as fresh.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// After a command that succeeded: print the notice if the cache names a
/// newer release, and start a fresh check if the cache is a day old. `quiet`
/// is for commands whose output a program reads (`run`, `hook`) and for
/// `update` itself.
pub(crate) fn after_command(json: bool, quiet: bool) {
    let set = |name: &str| std::env::var_os(name).is_some();
    if !eligible(json, quiet, std::io::stderr().is_terminal(), &set) {
        return;
    }
    if let Some(message) = check() {
        eprintln!("{message}");
    }
}

/// The notice for Claude Code to show when a session starts: someone who uses
/// simgit only through the plugin never types a command that could show it.
pub(crate) fn session_start_message() -> Option<String> {
    let set = |name: &str| std::env::var_os(name).is_some();
    if switched_off(&set) {
        return None;
    }
    check()
}

/// The notice, if the cache names a newer release; and a fresh check started
/// if the cache is a day old.
fn check() -> Option<String> {
    let path = cache_path()?;
    let message = cached_latest(&path).and_then(|latest| {
        let current = current_version();
        (latest > current).then(|| {
            format!(
                "simgit {latest} is available (you have {current}); update with: {}",
                detect_install_method().command()
            )
        })
    });
    if is_stale(&path, SystemTime::now()) {
        refresh(&path);
    }
    message
}

/// Only a person at a terminal sees the notice, and only outside CI and
/// outside `--json`, `run`, `hook` and `update`.
fn eligible(json: bool, quiet: bool, stderr_terminal: bool, set: &dyn Fn(&str) -> bool) -> bool {
    !json && !quiet && stderr_terminal && !switched_off(set)
}

/// `CI` and `SIMGIT_NO_UPDATE_CHECK` turn off the notice and its check.
fn switched_off(set: &dyn Fn(&str) -> bool) -> bool {
    set("CI") || set("SIMGIT_NO_UPDATE_CHECK")
}

/// `$XDG_CACHE_HOME/simgit/latest`, else `$HOME/.cache/simgit/latest`.
fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("simgit").join("latest"))
}

/// The release the cache names. Anything else, such as the empty file of a
/// check still running or an error page, names none.
fn cached_latest(path: &Path) -> Option<Version> {
    tag_from_redirect(&fs::read_to_string(path).ok()?)
}

/// Missing, or last written (by an answer or by the start of a check) a day
/// or more before `now`.
fn is_stale(path: &Path, now: SystemTime) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return true;
    };
    now.duration_since(modified)
        .is_ok_and(|age| age >= CHECK_INTERVAL)
}

/// Start `curl` with its output redirected into the cache, and do not wait.
/// Truncating the file now marks the attempt, so a failing network is tried
/// once a day, not on every command. Every error is ignored: the notice is
/// a courtesy.
fn refresh(path: &Path) {
    let Some(dir) = path.parent() else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(cache) = fs::File::create(path) else {
        return;
    };
    let _ = curl()
        .args([
            "--max-time",
            "10",
            "-o",
            "/dev/null",
            "-w",
            "%{redirect_url}",
        ])
        .arg(format!("{RELEASES_URL}/latest"))
        .stdin(Stdio::null())
        .stdout(cache)
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("simgit-notice-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn eligible_only_for_a_terminal_outside_json_hook_run_and_ci() {
        let none = |_: &str| false;
        assert!(eligible(false, false, true, &none));
        assert!(!eligible(true, false, true, &none), "--json");
        assert!(!eligible(false, true, true, &none), "run, hook or update");
        assert!(
            !eligible(false, false, false, &none),
            "stderr is not a terminal"
        );
        assert!(!eligible(false, false, true, &|name| name == "CI"));
        assert!(!eligible(false, false, true, &|name| name == "SIMGIT_NO_UPDATE_CHECK"));
    }

    #[test]
    fn cached_latest_ignores_anything_but_a_release_tag() {
        let path = temp_file("latest");
        assert_eq!(cached_latest(&path), None, "missing");
        fs::write(
            &path,
            "https://github.com/abendrothj/simgit/releases/tag/v0.6.0",
        )
        .unwrap();
        assert_eq!(cached_latest(&path), Version::parse("0.6.0"));
        for junk in [
            "",
            "https://github.com/abendrothj/simgit/releases/ta",
            "<html>rate limited</html>",
            "https://github.com/someone/simgit/releases/tag/v0.6.0",
            "https://github.com/abendrothj/simgit/releases/tag/v0.6.0-rc1",
        ] {
            fs::write(&path, junk).unwrap();
            assert_eq!(cached_latest(&path), None, "{junk:?}");
        }
    }

    #[test]
    fn stale_after_a_day_or_when_missing() {
        let path = temp_file("latest");
        let now = SystemTime::now();
        assert!(is_stale(&path, now), "missing");
        fs::write(&path, "").unwrap();
        assert!(!is_stale(&path, now), "just written");
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(now - Duration::from_secs(25 * 60 * 60))
            .unwrap();
        assert!(is_stale(&path, now), "a day old");
    }
}

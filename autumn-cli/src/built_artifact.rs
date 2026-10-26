//! The executable path Cargo reports for a build.
//!
//! With `build.target`, Cargo writes to `<target>/<triple>/<profile>/<bin>`.
//! `cargo metadata` does not show the triple. So the CLI reads the
//! `executable` field of each `compiler-artifact` record instead.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;

/// The triple this CLI runs on. Only a binary for it can start here.
const HOST_TRIPLE: &str = env!("AUTUMN_CLI_HOST_TRIPLE");

/// Executables from the builds this process ran, newest last.
static BUILT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Executables of `bin` targets named in `cargo build --message-format=json` output.
pub fn parse_executables(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["reason"] == "compiler-artifact")
        .filter(|v| {
            v["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k == "bin"))
        })
        .filter_map(|v| v["executable"].as_str().map(PathBuf::from))
        .collect()
}

/// The profile directory of an artifact path: `debug` in `.../debug/app`.
fn profile_dir(path: &Path) -> Option<&OsStr> {
    path.parent()?.file_name()
}

/// The triple directory of an artifact path: `t` in `.../t/debug/app`.
fn triple_dir(path: &Path) -> Option<&OsStr> {
    path.parent()?.parent()?.file_name()
}

/// Add `executables` to `built`, newest last. A path is kept once.
fn record_into(built: &mut Vec<PathBuf>, executables: &[PathBuf]) {
    for exe in executables {
        built.retain(|old| old != exe);
        built.push(exe.clone());
    }
}

/// The path in `built` that matches `guess` by file name and profile directory.
///
/// `build.target` can name more than one triple. Then the host triple wins,
/// because only its binary can start here. Otherwise the newest match wins.
fn lookup_in(built: &[PathBuf], guess: &Path) -> Option<PathBuf> {
    let mut matches = built.iter().rev().filter(|path| {
        path.file_name() == guess.file_name() && profile_dir(path) == profile_dir(guess)
    });
    let newest = matches.clone().next()?;
    let host = matches.find(|path| triple_dir(path) == Some(OsStr::new(HOST_TRIPLE)));
    Some(host.unwrap_or(newest).clone())
}

/// Remember the executables of a finished build.
pub fn record(executables: &[PathBuf]) {
    if let Ok(mut built) = BUILT.lock() {
        record_into(&mut built, executables);
    }
}

/// Prefer the path Cargo reported over the rebuilt `guess`.
///
/// A recorded path must match the guess in file name and profile directory.
pub fn prefer_built(guess: PathBuf) -> PathBuf {
    BUILT
        .lock()
        .ok()
        .and_then(|built| lookup_in(&built, &guess))
        .unwrap_or(guess)
}

/// Run a `cargo build` command, record its executables, and return its status.
///
/// Adds `--message-format=json-render-diagnostics`. Cargo then prints
/// diagnostics on stderr, and stdout holds the artifact records.
pub fn run_build(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    let mut child = cmd
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped())
        .spawn()?;
    let mut bytes = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        // Drain to the end, or Cargo can block on a full pipe.
        let _ = pipe.read_to_end(&mut bytes);
    }
    let status = child.wait()?;
    if status.success() {
        record(&parse_executables(&String::from_utf8_lossy(&bytes)));
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIPLE_ARTIFACT: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///w#app@0.1.0","target":{"kind":["bin"],"name":"app"},"executable":"/w/target/x86_64-unknown-linux-gnu/debug/app"}"#;
    const LIB_ARTIFACT: &str = r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"dep"},"executable":null}"#;
    const BUILD_SCRIPT: &str = r#"{"reason":"build-script-executed","package_id":"x"}"#;

    #[test]
    fn parse_returns_triple_path_from_artifact() {
        let out = format!("{LIB_ARTIFACT}\n{BUILD_SCRIPT}\n{TRIPLE_ARTIFACT}\n");
        assert_eq!(
            parse_executables(&out),
            vec![PathBuf::from(
                "/w/target/x86_64-unknown-linux-gnu/debug/app"
            )]
        );
    }

    #[test]
    fn parse_skips_noise_and_test_executables() {
        let test_bin = r#"{"reason":"compiler-artifact","target":{"kind":["test"],"name":"t"},"executable":"/w/target/debug/deps/t-abc"}"#;
        let out = format!("not json\n\n{test_bin}\n{{\"reason\":\"build-finished\"}}\n");
        assert!(parse_executables(&out).is_empty());
    }

    #[test]
    fn newest_path_wins_for_same_file_name() {
        let mut built = Vec::new();
        record_into(
            &mut built,
            &[PathBuf::from("/a/debug/bin"), PathBuf::from("/a/other")],
        );
        record_into(&mut built, &[PathBuf::from("/b/debug/bin")]);
        record_into(&mut built, &[PathBuf::from("/b/debug/bin")]);
        assert_eq!(built.len(), 3, "a path is kept once: {built:?}");
        assert_eq!(
            lookup_in(&built, Path::new("/t/debug/bin")),
            Some(PathBuf::from("/b/debug/bin"))
        );
        assert_eq!(lookup_in(&built, Path::new("/t/debug/missing")), None);
    }

    #[test]
    fn prefer_built_swaps_a_stale_guess() {
        record(&[PathBuf::from("/t/triple/debug/yy_app")]);
        assert_eq!(
            prefer_built(PathBuf::from("/t/debug/yy_app")),
            PathBuf::from("/t/triple/debug/yy_app")
        );
    }

    #[test]
    fn other_profile_is_not_swapped_in() {
        let mut built = Vec::new();
        record_into(&mut built, &[PathBuf::from("/t/release/app")]);
        assert_eq!(lookup_in(&built, Path::new("/t/debug/app")), None);
        record_into(&mut built, &[PathBuf::from("/t/triple/debug/app")]);
        assert_eq!(built.len(), 2);
    }

    #[test]
    fn host_triple_wins_over_a_foreign_triple() {
        // `build.target = [host, foreign]`: Cargo reports both, in any order.
        let host = PathBuf::from(format!("/t/{HOST_TRIPLE}/debug/app"));
        let foreign = PathBuf::from("/t/foreign-triple/debug/app");
        for order in [[&host, &foreign], [&foreign, &host]] {
            let mut built = Vec::new();
            for exe in order {
                record_into(&mut built, std::slice::from_ref(exe));
            }
            assert_eq!(
                lookup_in(&built, Path::new("/t/debug/app")),
                Some(host.clone())
            );
        }
    }

    #[test]
    fn prefer_built_keeps_guess_when_nothing_recorded() {
        let guess = PathBuf::from("/t/debug/never_built");
        assert_eq!(prefer_built(guess.clone()), guess);
    }

    #[cfg(unix)]
    #[test]
    fn failed_build_records_nothing() {
        let line = TRIPLE_ARTIFACT.replace("/app", "/ww_failed");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf '%s\\n' \"$0\"; exit 3", &line]);
        assert!(!run_build(&mut cmd).expect("run").success());
        let guess = PathBuf::from("/w/target/x86_64-unknown-linux-gnu/debug/ww_failed");
        assert_eq!(prefer_built(guess.clone()), guess);
    }

    #[cfg(unix)]
    #[test]
    fn large_and_non_utf8_output_does_not_hang_or_lose_the_artifact() {
        let line = TRIPLE_ARTIFACT.replace("/app", "/ww_big");
        let script = "head -c 200000 /dev/zero | tr '\\0' 'x'; printf '\\377\\n%s\\n' \"$0\"";
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script, &line]);
        assert!(run_build(&mut cmd).expect("run").success());
        let guess = PathBuf::from("/w/target/debug/ww_big");
        assert_ne!(prefer_built(guess.clone()), guess);
    }

    #[cfg(unix)]
    #[test]
    fn run_build_records_executables_from_stdout() {
        let line = TRIPLE_ARTIFACT.replace("/app", "/ww_run");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf '%s\\n' \"$0\"", &line]);
        assert!(run_build(&mut cmd).expect("run").success());
        let guess = PathBuf::from("/w/target/debug/ww_run");
        assert_eq!(
            prefer_built(guess),
            PathBuf::from("/w/target/x86_64-unknown-linux-gnu/debug/ww_run")
        );
    }
}

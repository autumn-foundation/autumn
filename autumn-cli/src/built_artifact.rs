//! The executable path Cargo reports for a build.
//!
//! Cargo writes a binary to `<target>/<triple>/<profile>/<bin>` when
//! `build.target` is set. `cargo metadata` does not report the triple, so
//! rebuilding the path by hand finds the wrong file. Instead, read the
//! `executable` field of each `compiler-artifact` record and use it.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;

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

/// Remember the executables of a finished build.
pub fn record(executables: &[PathBuf]) {
    if let Ok(mut built) = BUILT.lock() {
        built.extend(executables.iter().cloned());
    }
}

/// The newest recorded executable whose file name is `file_name`, if any.
pub fn lookup(file_name: &OsStr) -> Option<PathBuf> {
    let built = BUILT.lock().ok()?;
    built
        .iter()
        .rev()
        .find(|path| path.file_name() == Some(file_name))
        .cloned()
}

/// Prefer the path Cargo reported over the rebuilt `guess`.
pub fn prefer_built(guess: PathBuf) -> PathBuf {
    guess.file_name().and_then(lookup).unwrap_or(guess)
}

/// Run a `cargo build` command, record its executables, and return its status.
///
/// Cargo renders diagnostics on stderr itself; stdout carries artifact records.
pub fn run_build(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    let mut child = cmd.stdout(Stdio::piped()).spawn()?;
    let mut stdout = String::new();
    if let Some(pipe) = child.stdout.take() {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            stdout.push_str(&line);
            stdout.push('\n');
        }
    }
    let status = child.wait()?;
    if status.success() {
        record(&parse_executables(&stdout));
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
    fn lookup_matches_by_file_name_and_newest_wins() {
        record(&[PathBuf::from("/a/old/zz_bin")]);
        record(&[PathBuf::from("/b/new/zz_bin")]);
        assert_eq!(
            lookup(OsStr::new("zz_bin")),
            Some(PathBuf::from("/b/new/zz_bin"))
        );
        assert_eq!(lookup(OsStr::new("zz_missing")), None);
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
    fn prefer_built_keeps_guess_when_nothing_recorded() {
        let guess = PathBuf::from("/t/debug/never_built");
        assert_eq!(prefer_built(guess.clone()), guess);
    }

    #[cfg(unix)]
    #[test]
    fn run_build_records_executables_from_stdout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf '%s\\n' \"$0\"", TRIPLE_ARTIFACT]);
        let status = run_build(&mut cmd).expect("run");
        assert!(status.success());
        assert_eq!(
            lookup(OsStr::new("app")),
            Some(PathBuf::from(
                "/w/target/x86_64-unknown-linux-gnu/debug/app"
            ))
        );
    }
}

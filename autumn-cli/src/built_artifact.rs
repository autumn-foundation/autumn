//! The executable path Cargo reports for a build.
//!
//! With `build.target`, Cargo writes to `<target>/<triple>/<profile>/<bin>`.
//! `cargo metadata` does not show the triple. So the CLI reads the
//! `executable` field of each `compiler-artifact` record instead.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};

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
fn triple_dir(path: &Path) -> Option<&str> {
    path.parent()?.parent()?.file_name()?.to_str()
}

/// Whether two artifact paths have the same file name and profile directory.
fn same_slot(a: &Path, b: &Path) -> bool {
    a.file_name() == b.file_name() && profile_dir(a) == profile_dir(b)
}

/// Add the `executables` of one build to `built`, newest last.
///
/// A build replaces the older paths in the same slot. The paths of one build
/// stay together: with more than one triple, one build reports one path for
/// each triple.
fn record_into(built: &mut Vec<PathBuf>, executables: &[PathBuf]) {
    built.retain(|old| !executables.iter().any(|exe| same_slot(old, exe)));
    for exe in executables {
        if !built.contains(exe) {
            built.push(exe.clone());
        }
    }
}

/// The path in `built` that matches `guess` by file name and profile directory.
fn lookup_in(built: &[PathBuf], guess: &Path) -> Option<PathBuf> {
    lookup_with(built, guess, native_triple)
}

/// Like [`lookup_in`], with the native triple from `native`.
///
/// With more than one triple, one build reports one path for each triple.
/// Then the path for the native triple wins, because it can start here.
/// Otherwise the newest path wins. `native` runs only for more than one match.
fn lookup_with(
    built: &[PathBuf],
    guess: &Path,
    native: impl FnOnce() -> Option<String>,
) -> Option<PathBuf> {
    let matches: Vec<&PathBuf> = built.iter().filter(|path| same_slot(path, guess)).collect();
    let newest = *matches.last()?;
    let for_native = (matches.len() > 1)
        .then(native)
        .flatten()
        .and_then(|triple| {
            matches
                .iter()
                .rev()
                .find(|path| triple_dir(path) == Some(triple.as_str()))
                .copied()
        });
    Some(for_native.unwrap_or(newest).clone())
}

/// The host triple of the `rustc` that Cargo uses, from `rustc -vV`.
///
/// This is the triple whose binaries can start here. The arch name in
/// `std::env::consts` is not enough: it is `powerpc64` for the native
/// `powerpc64le-...` triple, and `riscv64` for `riscv64gc-...`.
fn native_triple() -> Option<String> {
    static NATIVE: OnceLock<Option<String>> = OnceLock::new();
    NATIVE
        .get_or_init(|| {
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let output = Command::new(rustc).arg("-vV").output().ok()?;
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .map(|host| host.trim().to_owned())
        })
        .clone()
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
    fn newer_build_replaces_older_path_in_same_slot() {
        let mut built = Vec::new();
        record_into(
            &mut built,
            &[PathBuf::from("/a/debug/bin"), PathBuf::from("/a/other")],
        );
        record_into(&mut built, &[PathBuf::from("/b/debug/bin")]);
        assert_eq!(built.len(), 2);
        assert_eq!(
            lookup_in(&built, Path::new("/t/debug/bin")),
            Some(PathBuf::from("/b/debug/bin"))
        );
        assert_eq!(lookup_in(&built, Path::new("/t/debug/missing")), None);
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
    fn native_triple_wins_over_a_foreign_triple() {
        // `build.target = [native, foreign]`: one build reports both, in any order.
        let native = PathBuf::from("/t/x86_64-unknown-linux-gnu/debug/app");
        let foreign = PathBuf::from("/t/aarch64-unknown-linux-gnu/debug/app");
        let host = || Some("x86_64-unknown-linux-gnu".to_owned());
        for order in [[&native, &foreign], [&foreign, &native]] {
            let mut built = Vec::new();
            record_into(&mut built, &order.map(PathBuf::clone));
            assert_eq!(built.len(), 2);
            assert_eq!(
                lookup_with(&built, Path::new("/t/debug/app"), host),
                Some(native.clone())
            );
        }
    }

    #[test]
    fn newest_wins_when_no_triple_is_native() {
        let first = PathBuf::from("/t/a-b-c/debug/app");
        let last = PathBuf::from("/t/d-e-f/debug/app");
        let mut built = Vec::new();
        record_into(&mut built, &[first, last.clone()]);
        let guess = Path::new("/t/debug/app");
        assert_eq!(lookup_with(&built, guess, || None), Some(last.clone()));
        assert_eq!(
            lookup_with(&built, guess, || Some("x-y-z".into())),
            Some(last)
        );
    }

    #[test]
    fn one_match_does_not_probe_rustc() {
        let built = vec![PathBuf::from("/t/a-b-c/debug/app")];
        let probe = || -> Option<String> { panic!("probed rustc for one match") };
        assert_eq!(
            lookup_with(&built, Path::new("/t/debug/app"), probe),
            Some(built[0].clone())
        );
    }

    #[test]
    fn newer_build_beats_an_older_triple_entry() {
        // `build.target` was removed between two builds in one process.
        let mut built = Vec::new();
        record_into(
            &mut built,
            &[PathBuf::from("/t/x86_64-unknown-linux-gnu/debug/app")],
        );
        record_into(&mut built, &[PathBuf::from("/t/debug/app")]);
        assert_eq!(
            lookup_in(&built, Path::new("/t/debug/app")),
            Some(PathBuf::from("/t/debug/app"))
        );
    }

    #[test]
    fn native_triple_is_the_rustc_host() {
        let host = native_triple().expect("rustc -vV reports a host");
        assert!(host.contains('-'), "{host}");
        if matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            assert!(host.starts_with(std::env::consts::ARCH), "{host}");
        }
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
    fn native_triple_wins_when_its_name_differs_from_consts_arch() {
        // Little-endian PowerPC: `consts::ARCH` is `powerpc64`, the native
        // triple is `powerpc64le-...`. One build reports both triples.
        let le = PathBuf::from("/t/powerpc64le-unknown-linux-gnu/debug/app");
        let be = PathBuf::from("/t/powerpc64-unknown-linux-gnu/debug/app");
        let host = || Some("powerpc64le-unknown-linux-gnu".to_owned());
        for order in [[&le, &be], [&be, &le]] {
            let mut built = Vec::new();
            record_into(&mut built, &order.map(PathBuf::clone));
            assert_eq!(
                lookup_with(&built, Path::new("/t/debug/app"), host),
                Some(le.clone())
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

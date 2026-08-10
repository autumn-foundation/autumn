//! Locating a usable Chromium/Chrome binary on the host.
//!
//! Shared by the `system_test` harness (which launches the browser it finds;
//! feature `system-tests`, so this always-compiled module cannot link to it)
//! and `autumn doctor` (which only reports on it).
//! Both used to carry their own copy of this logic and drifted — a fix in one
//! left the other reporting the opposite answer on the same machine — so the
//! resolution order, the version probe, and its platform quirks live here
//! once.
//!
//! Deliberately **not** behind the `system-tests` feature: `autumn doctor`
//! must be able to tell you whether system tests would work without pulling
//! `chromiumoxide` (and a headless browser stack) into the CLI.
//!
//! # Resolution order
//!
//! 1. `AUTUMN_CHROMIUM` environment variable (full binary path)
//! 2. `PLAYWRIGHT_BROWSERS_PATH` — scans `<path>/chromium-*/…`
//! 3. `PATH`-based lookup (`chrome`, `google-chrome`, `chromium`, …)
//! 4. Well-known per-platform install locations

use std::fmt;
use std::path::{Path, PathBuf};

/// Version string reported for a browser binary that exists and is usable but
/// cannot tell us its version — see [`probe_version`].
pub const UNKNOWN_VERSION: &str = "version unavailable";

// ── BrowserCheck ───────────────────────────────────────────────────────────

/// Result of probing the host for a usable Chromium binary.
///
/// Returned by [`BrowserCheck::run`] and shown by `autumn doctor`.
#[derive(Debug, Clone)]
pub enum BrowserCheck {
    /// A usable binary was found at `path` with the reported `version` string.
    Found {
        /// Absolute path to the Chromium binary.
        path: PathBuf,
        /// Version string reported by `--version` (e.g. `"Chromium 122.0.6261.111"`),
        /// or [`UNKNOWN_VERSION`] when the platform cannot report one.
        version: String,
    },
    /// No usable binary could be found; `searched_paths` lists every path that
    /// was probed so the user knows what to add.
    NotFound {
        /// Every path that was checked and did not yield a working binary.
        searched_paths: Vec<PathBuf>,
    },
}

impl BrowserCheck {
    /// Probe the host for a Chromium binary using the documented resolution
    /// order and return the result.
    #[must_use]
    pub fn run() -> Self {
        let candidates = browser_candidates();
        let mut searched = Vec::new();
        for path in &candidates {
            if let Some(version) = probe_version(path) {
                return Self::Found {
                    path: path.clone(),
                    version,
                };
            }
            searched.push(path.clone());
        }
        Self::NotFound {
            searched_paths: searched,
        }
    }

    /// `true` when a browser was found.
    #[must_use]
    pub const fn is_found(&self) -> bool {
        matches!(self, Self::Found { .. })
    }
}

impl fmt::Display for BrowserCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Found { path, version } => {
                write!(f, "Chromium found: {} ({})", path.display(), version)
            }
            Self::NotFound { searched_paths } => {
                write!(
                    f,
                    "Chromium not found. Searched:\n{}",
                    searched_paths
                        .iter()
                        .map(|p| format!("  {}", p.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                )?;
                write!(
                    f,
                    "\n\nTo install on Ubuntu/Debian: apt-get install chromium-browser\n\
                     Or set the AUTUMN_CHROMIUM environment variable to the full binary path."
                )
            }
        }
    }
}

// ── Candidate discovery ────────────────────────────────────────────────────

/// Binary names to look for on `PATH`, in preference order.
const PATH_BINARY_NAMES: &[&str] = &[
    "chrome",
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
];

/// All paths to probe for a Chromium binary, in resolution order.
#[must_use]
pub fn browser_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Explicit override.
    if let Ok(p) = std::env::var("AUTUMN_CHROMIUM") {
        candidates.push(PathBuf::from(p));
    }

    // 2. Playwright browsers directory.
    if let Ok(base) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
        let base = PathBuf::from(base);
        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut pw_paths: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("chromium-"))
                .map(|e| {
                    if cfg!(target_os = "macos") {
                        e.path()
                            .join("chrome-mac")
                            .join("Chromium.app")
                            .join("Contents")
                            .join("MacOS")
                            .join("Chromium")
                    } else if cfg!(target_os = "windows") {
                        e.path().join("chrome-win").join("chrome.exe")
                    } else {
                        e.path().join("chrome-linux").join("chrome")
                    }
                })
                .collect();
            pw_paths.sort();
            pw_paths.reverse(); // highest revision first
            candidates.extend(pw_paths);
        }
    }

    // 3. PATH-based lookup — covers CI setups like browser-actions/setup-chrome
    //    that install a `chrome` or `google-chrome` binary on PATH rather than
    //    at a well-known fixed location.
    for name in PATH_BINARY_NAMES {
        // `Path::is_file` stats the literal path and does *not* apply
        // Windows' PATHEXT resolution, so a bare `chrome` never matches
        // `chrome.exe`. Ask for the name Windows actually stores on disk.
        let file_name = if cfg!(target_os = "windows") {
            format!("{name}.exe")
        } else {
            (*name).to_owned()
        };
        if let Some(p) = std::env::var_os("PATH").and_then(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(&file_name))
                .find(|p| p.is_file())
        }) {
            candidates.push(p);
        }
    }

    // 4. Well-known system paths.
    candidates.extend(well_known_paths());

    candidates
}

/// Per-platform install locations, checked after `PATH`.
fn well_known_paths() -> Vec<PathBuf> {
    if cfg!(target_os = "windows") {
        // Chrome installs under `%ProgramFiles%` / `%ProgramFiles(x86)%` for a
        // machine-wide install and `%LOCALAPPDATA%` for the (very common)
        // per-user one. Read the variables rather than hard-coding `C:\` —
        // the system drive is not always `C:`, and the 32-bit variable name
        // contains parentheses, so it must be fetched by name.
        ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .flat_map(|base| {
                let base = PathBuf::from(base);
                ["Google\\Chrome", "Google\\Chrome Beta", "Chromium"]
                    .iter()
                    .map(move |vendor| base.join(vendor).join("Application").join("chrome.exe"))
            })
            .collect()
    } else {
        [
            "/usr/bin/chromium-browser",
            "/usr/bin/chromium",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/snap/bin/chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// Return the first candidate that exists and probes successfully.
#[must_use]
pub fn find_chromium() -> Option<PathBuf> {
    browser_candidates()
        .into_iter()
        .find(|path| probe_version(path).is_some())
}

// ── Version probe ──────────────────────────────────────────────────────────

/// Outcome of asking a candidate binary for its version.
#[derive(Debug, PartialEq, Eq)]
enum VersionProbe {
    /// The process ran and exited successfully; payload is its raw stdout
    /// (which may legitimately be empty for a launcher shim that prints
    /// elsewhere).
    Ran(String),
    /// The process could not be spawned, or exited non-zero. The candidate is
    /// not usable.
    Failed,
    /// The probe was deliberately not executed, but the file is a plausible
    /// browser binary. See [`run_version_probe`] for why Windows takes this
    /// path.
    Skipped,
}

/// Arguments for the `--version` probe.
///
/// The private `--user-data-dir` keeps the probe from touching the user's real
/// profile: launching Chrome against a profile another instance already holds
/// makes the new process rendezvous with the running one instead of answering
/// us (#1456). `--version` exits before profile setup on POSIX, so this is
/// belt-and-braces there rather than load-bearing — but it costs nothing and
/// covers wrapper scripts that do reach profile startup.
fn version_probe_args(user_data_dir: &Path) -> Vec<String> {
    vec![
        "--version".to_owned(),
        format!("--user-data-dir={}", user_data_dir.display()),
    ]
}

/// A throwaway profile directory for a single probe.
fn unique_probe_dir() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("autumn-browser-probe-{}-{n}", std::process::id()))
}

/// `true` when `path` carries a Windows executable extension.
///
/// The Windows path accepts a candidate on file evidence alone, so this is the
/// only filter standing between a mistyped `AUTUMN_CHROMIUM` (a README, a
/// `.lnk` shortcut) and it shadowing a browser that would actually have
/// worked.
fn has_windows_executable_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
}

/// Ask `path` for its version, or decide not to ask.
///
/// **Windows never executes the candidate.** `chrome.exe` is a GUI-subsystem
/// binary: it writes nothing to the parent console, and `--version` is not an
/// early-exit switch there, so running it proceeds into real browser startup —
/// which is how the original report saw exit code 21 (the running instance
/// being notified) rather than a version string. Handing it a private profile
/// would remove that abort only to replace it with something worse: a real
/// browser window, and a [`std::process::Command::output`] call that blocks
/// until every child process closes its stdout handle. File existence is the
/// strongest signal available on Windows, and it is the one the issue's own
/// suggested fix names.
fn run_version_probe(path: &Path) -> VersionProbe {
    if !path.is_file() {
        return VersionProbe::Failed;
    }

    if cfg!(target_os = "windows") {
        return if has_windows_executable_extension(path) {
            VersionProbe::Skipped
        } else {
            VersionProbe::Failed
        };
    }

    let probe_profile = unique_probe_dir();
    let output = std::process::Command::new(path)
        .args(version_probe_args(&probe_profile))
        .output();
    // `--version` should not create the directory, but Chrome variants differ;
    // clean up unconditionally and ignore failure.
    let _ = std::fs::remove_dir_all(&probe_profile);

    match output {
        Ok(o) if o.status.success() => {
            VersionProbe::Ran(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        _ => VersionProbe::Failed,
    }
}

/// Turn a probe outcome into the reported version.
///
/// Split from [`run_version_probe`] so the platform-dependent decisions are
/// unit-testable on every host — a `#[cfg(windows)]` branch would be dead,
/// unexercised code on the Linux CI runner.
fn version_from_probe(probe: &VersionProbe) -> Option<String> {
    match probe {
        VersionProbe::Ran(stdout) => {
            let trimmed = stdout.trim();
            // Exiting 0 is the usability signal; the text is a bonus. A
            // launcher shim that prints its version to stderr (or nothing at
            // all) is still a browser we can drive.
            Some(if trimmed.is_empty() {
                UNKNOWN_VERSION.to_owned()
            } else {
                trimmed.to_owned()
            })
        }
        VersionProbe::Skipped => Some(UNKNOWN_VERSION.to_owned()),
        VersionProbe::Failed => None,
    }
}

/// Probe `path` for a browser version.
///
/// Returns `None` when the candidate is not a usable browser binary, and
/// [`UNKNOWN_VERSION`] when it is usable but cannot report a version.
///
/// On Linux and macOS "usable" means `<path> --version` ran and exited 0
/// (against a throwaway `--user-data-dir`, so it can never rendezvous with a
/// Chrome holding the real profile); empty output is still accepted, since a
/// launcher shim may print elsewhere.
///
/// On **Windows** the candidate is never executed: `chrome.exe` is a
/// GUI-subsystem binary that writes nothing to the parent console, and
/// `--version` is not an early-exit switch there, so running it starts a real
/// browser instead of answering (#1456). An existing file with an `.exe`
/// extension is accepted on that evidence alone.
#[must_use]
pub fn probe_version(path: &Path) -> Option<String> {
    version_from_probe(&run_version_probe(path))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_check_not_found_message_has_hints() {
        let check = BrowserCheck::NotFound {
            searched_paths: vec![PathBuf::from("/no/such/path")],
        };
        let msg = check.to_string();
        assert!(msg.contains("apt-get") || msg.contains("AUTUMN_CHROMIUM"));
    }

    #[test]
    fn browser_candidates_includes_common_paths() {
        let candidates = browser_candidates();
        let as_strings: Vec<_> = candidates
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            as_strings
                .iter()
                .any(|s| s.contains("chromium") || s.contains("chrome")),
            "should have at least one chrome path; got {as_strings:?}"
        );
    }

    #[test]
    fn well_known_paths_target_the_host_platform() {
        // #1456: the well-known list was Linux + macOS only, so a Windows host
        // with a stock Chrome install reached no candidate at all and the
        // version-probe fix downstream of it could never help.
        let paths: Vec<String> = well_known_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(!paths.is_empty(), "every platform needs candidates");
        if cfg!(target_os = "windows") {
            assert!(
                paths.iter().all(|p| p.ends_with("chrome.exe")),
                "Windows candidates must name the real on-disk binary; got {paths:?}"
            );
            assert!(
                paths.iter().any(|p| p.contains("Application")),
                "Windows Chrome lives in <base>\\...\\Application\\chrome.exe; got {paths:?}"
            );
        } else {
            assert!(
                paths.iter().any(|p| p.starts_with('/')),
                "POSIX candidates must be absolute; got {paths:?}"
            );
        }
    }

    #[test]
    fn version_probe_isolates_the_profile_directory() {
        let dir = PathBuf::from("/tmp/probe-profile");
        let args = version_probe_args(&dir);

        assert!(
            args.iter().any(|a| a == "--version"),
            "the probe must still ask for the version; got {args:?}"
        );
        let user_data_dir: Vec<_> = args
            .iter()
            .filter(|a| a.starts_with("--user-data-dir="))
            .collect();
        assert_eq!(
            user_data_dir.len(),
            1,
            "the probe must pass exactly one --user-data-dir so it never \
             rendezvouses with a Chrome already holding the real profile; \
             got {args:?}"
        );
        assert!(
            user_data_dir[0].contains("probe-profile"),
            "--user-data-dir must point at the supplied private directory; got {args:?}"
        );
    }

    #[test]
    fn probe_dirs_are_never_reused() {
        assert_ne!(
            unique_probe_dir(),
            unique_probe_dir(),
            "two probes must not share a directory, or one's cleanup deletes \
             the other's profile mid-probe"
        );
    }

    #[test]
    fn version_from_probe_prefers_reported_version() {
        assert_eq!(
            version_from_probe(&VersionProbe::Ran("Chromium 122.0.6261.111\n".to_owned())),
            Some("Chromium 122.0.6261.111".to_owned()),
            "reported version must be used verbatim (trimmed)"
        );
    }

    #[test]
    fn version_from_probe_accepts_a_binary_that_exits_zero_without_output() {
        // A launcher shim may print its version to stderr or not at all.
        // Exiting 0 is the usability signal, and rejecting these would have
        // regressed hosts that worked before #1456.
        for stdout in ["", "   \r\n"] {
            assert_eq!(
                version_from_probe(&VersionProbe::Ran(stdout.to_owned())),
                Some(UNKNOWN_VERSION.to_owned()),
                "stdout {stdout:?} exited 0, so the binary is usable"
            );
        }
    }

    #[test]
    fn version_from_probe_skipped_resolves_without_running_anything() {
        // The Windows path: chrome.exe exists but must not be executed.
        assert_eq!(
            version_from_probe(&VersionProbe::Skipped),
            Some(UNKNOWN_VERSION.to_owned()),
            "an existing Windows chrome.exe must resolve as found, not as \
             BrowserNotFound"
        );
    }

    #[test]
    fn version_from_probe_rejects_a_failed_probe() {
        // Spawn failure or a non-zero exit means the candidate is unusable.
        // It must keep failing so the search falls through to the next
        // candidate and, if none work, the caller still gets the
        // searched-paths error with its remediation hint.
        assert_eq!(
            version_from_probe(&VersionProbe::Failed),
            None,
            "a broken binary must not be papered over"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_probe_actually_passes_its_arguments_to_the_process() {
        // The pure functions above cover the decisions; this covers the
        // wiring. `echo` prints back whatever argv it received, so a
        // successful probe whose "version" is our own flags proves both
        // `--version` and the isolating `--user-data-dir` reached the child.
        let echo = ["/bin/echo", "/usr/bin/echo"]
            .into_iter()
            .map(Path::new)
            .find(|p| p.is_file());
        let Some(echo) = echo else {
            return; // no `echo` on this host; nothing to assert against
        };

        let reported = probe_version(echo).expect("echo exits 0, so it is 'usable'");
        assert!(
            reported.contains("--version"),
            "the probe must ask for the version; echo saw {reported:?}"
        );
        assert!(
            reported.contains("--user-data-dir="),
            "the probe must isolate the profile so a running Chrome cannot \
             abort it; echo saw {reported:?}"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_non_zero_exit_is_not_a_browser() {
        // `false` exists on every POSIX host and exits 1.
        let Some(falsy) = ["/bin/false", "/usr/bin/false"]
            .into_iter()
            .map(Path::new)
            .find(|p| p.is_file())
        else {
            return;
        };
        assert_eq!(
            probe_version(falsy),
            None,
            "a binary that exits non-zero must not be accepted as a browser"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_accepts_an_existing_exe_without_running_it() {
        // The running test binary is a real, existing `.exe` that is
        // emphatically not Chrome — and must still resolve, because on
        // Windows file existence is the only signal available and executing
        // the candidate is exactly what #1456 says not to do.
        let me = std::env::current_exe().expect("test binary path");
        assert_eq!(probe_version(&me), Some(UNKNOWN_VERSION.to_owned()));

        assert_eq!(
            probe_version(Path::new(r"C:\definitely\not\here\chrome.exe")),
            None,
            "a path that does not exist is still not a browser"
        );
    }

    #[test]
    fn missing_files_never_probe() {
        assert_eq!(
            run_version_probe(Path::new("/definitely/not/a/browser/chrome")),
            VersionProbe::Failed
        );
        assert_eq!(
            probe_version(Path::new("/definitely/not/a/browser/chrome")),
            None
        );
    }

    #[test]
    fn windows_candidates_must_look_like_executables() {
        assert!(has_windows_executable_extension(Path::new(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe"
        )));
        assert!(
            has_windows_executable_extension(Path::new("chrome.EXE")),
            "the extension check must be case-insensitive"
        );
        for not_a_binary in ["chrome.lnk", "README.md", "chrome"] {
            assert!(
                !has_windows_executable_extension(Path::new(not_a_binary)),
                "{not_a_binary:?} must not be accepted as a browser binary, or a \
                 mistyped AUTUMN_CHROMIUM would shadow one that works"
            );
        }
    }
}

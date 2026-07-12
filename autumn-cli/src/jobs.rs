//! `autumn jobs manifest` -- emit the effective drained-queue manifest.
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_DUMP_JOBS=1`, and writes the TOML `queues = [...]` document from its
//! stdout to the requested output path. This is the ground-truth drained-queue
//! set — the configured `[jobs.queues]` unioned with every
//! `#[job(queue = "…")]`-declared queue — that `autumn doctor` consumes via
//! `[jobs.fleet] manifest = "<path>"`.
//!
//! Emitting from inside the running app is the only sound source: jobs are
//! registered at runtime into the `Vec<JobInfo>` the user passes to
//! `.jobs(jobs![...])`, so the standalone CLI (which links `autumn-web` but never
//! the user's job functions) cannot see the `#[job(queue = …)]` set on its own.

use std::path::Path;
use std::process::Command;

use crate::routes::{compile_binary, find_binary};

/// Options controlling `autumn jobs manifest`.
pub struct ManifestOptions<'a> {
    /// Package to inspect (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to inspect (for packages with multiple bin targets).
    pub bin: Option<&'a str>,
    /// Path the emitted manifest is written to.
    pub output: &'a str,
}

/// Run `autumn jobs manifest`.
pub fn run(opts: &ManifestOptions<'_>) {
    eprintln!("\u{1F342} autumn jobs manifest\n");
    compile_binary(opts.package, opts.bin);
    let binary = find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_JOBS", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    let manifest = match manifest_from_child(
        output.status.success(),
        output.status.code(),
        &output.stdout,
    ) {
        Ok(manifest) => manifest,
        Err(code) => {
            eprintln!(
                "\u{2717} Binary exited with status {} while dumping jobs manifest",
                output.status
            );
            std::process::exit(code);
        }
    };

    if let Err(message) = write_manifest(Path::new(opts.output), &manifest) {
        eprintln!("{message}");
        std::process::exit(1);
    }
    eprintln!("\u{2713} Wrote jobs manifest \u{2192} {}", opts.output);
}

/// Interpret a finished dump child: `Ok(manifest)` when it exited cleanly,
/// `Err(exit_code)` when it failed (the caller reports and propagates the code).
///
/// Extracted from [`run`] so the success/failure decision and stdout capture are
/// unit-testable without spawning a real process.
fn manifest_from_child(success: bool, code: Option<i32>, stdout: &[u8]) -> Result<String, i32> {
    if success {
        Ok(String::from_utf8_lossy(stdout).into_owned())
    } else {
        Err(code.unwrap_or(1))
    }
}

/// Write `contents` to `path`, creating any missing parent directories.
///
/// Returns a formatted, user-facing error message on failure instead of exiting,
/// so both the parent-directory-creation and the write branches are unit-testable
/// (the caller in [`run`] prints the message and exits non-zero).
fn write_manifest(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "\u{2717} Failed to create {}: {e}",
            parent.display()
        ));
    }
    if let Err(e) = std::fs::write(path, contents) {
        return Err(format!("\u{2717} Failed to write {}: {e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── manifest_from_child ─────────────────────────────────────────────────

    #[test]
    fn manifest_from_child_returns_stdout_on_success() {
        let manifest =
            manifest_from_child(true, Some(0), b"queues = [\"default\"]\n").expect("clean exit");
        assert_eq!(manifest, "queues = [\"default\"]\n");
    }

    #[test]
    fn manifest_from_child_lossily_decodes_non_utf8_stdout() {
        // A clean exit with invalid UTF-8 bytes must not panic — it is decoded
        // lossily, mirroring the previous `String::from_utf8_lossy` behaviour.
        let manifest = manifest_from_child(true, Some(0), &[0xff, 0xfe, b'x']).expect("clean exit");
        assert!(manifest.ends_with('x'));
        assert!(manifest.contains('\u{FFFD}'), "invalid bytes become U+FFFD");
    }

    #[test]
    fn manifest_from_child_propagates_child_exit_code() {
        assert_eq!(manifest_from_child(false, Some(2), b""), Err(2));
    }

    #[test]
    fn manifest_from_child_defaults_missing_code_to_one() {
        // A signal-terminated child has no exit code; default to 1 so callers
        // still exit non-zero.
        assert_eq!(manifest_from_child(false, None, b""), Err(1));
    }

    // ── write_manifest ──────────────────────────────────────────────────────

    #[test]
    fn write_manifest_writes_contents_to_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jobs-manifest.toml");
        write_manifest(&path, "queues = [\"a\"]\n").expect("write should succeed");
        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "queues = [\"a\"]\n");
    }

    #[test]
    fn write_manifest_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nested parents that do not yet exist must be created.
        let path = dir.path().join("nested/deeper/jobs-manifest.toml");
        write_manifest(&path, "queues = []\n").expect("nested write should succeed");
        assert!(path.exists(), "manifest file should exist");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "queues = []\n"
        );
    }

    #[test]
    fn write_manifest_errors_when_parent_creation_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Create a regular file, then try to treat it as a parent directory:
        // `create_dir_all` must fail because a path component is a file.
        let blocker = dir.path().join("iam-a-file");
        std::fs::write(&blocker, "x").expect("create blocker file");
        let path = blocker.join("sub/jobs-manifest.toml");
        let err = write_manifest(&path, "queues = []\n").expect_err("parent creation must fail");
        assert!(
            err.contains("Failed to create"),
            "expected parent-creation error, got: {err}"
        );
    }

    #[test]
    fn write_manifest_errors_when_write_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The target path is an existing directory, so the write itself fails
        // even though the parent exists.
        let path = dir.path().to_path_buf();
        let err = write_manifest(&path, "queues = []\n").expect_err("writing to a dir must fail");
        assert!(
            err.contains("Failed to write"),
            "expected write error, got: {err}"
        );
    }
}

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
        // `Command` inherits this process's environment, and both
        // `AUTUMN_DUMP_DATA_FLOW` and `AUTUMN_DUMP_AGENT_AUTHORITY` are
        // dispatched *earlier* than `AUTUMN_DUMP_JOBS` in `AppBuilder::run`:
        // one left over in the CLI's own environment would answer this command
        // with that manifest instead of the jobs one.
        .env_remove(crate::data_flow::DUMP_ENV)
        .env_remove(crate::agents::DUMP_ENV)
        .env_remove(crate::graph::DUMP_ENV)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    let stdout = match manifest_from_child(
        output.status.success(),
        output.status.code(),
        &output.stdout,
    ) {
        Ok(stdout) => stdout,
        Err(code) => {
            eprintln!(
                "\u{2717} Binary exited with status {} while dumping jobs manifest",
                output.status
            );
            std::process::exit(code);
        }
    };

    // A clean exit is not enough: the child's stdout must be the expected TOML
    // manifest. If the app prints anything else to stdout (from a custom config
    // or telemetry initializer, say), writing it verbatim would produce a
    // corrupt manifest that `autumn doctor` silently treats as an empty declared
    // set — false-passing the very topology check this manifest exists to guard.
    // Mirror `autumn routes`, which strict-parses the child's stdout and errors
    // on anything unexpected rather than emitting garbage.
    let manifest = match validate_manifest(&stdout) {
        Ok(manifest) => manifest,
        Err(message) => {
            eprintln!("\u{2717} {message}");
            std::process::exit(1);
        }
    };

    if let Err(message) = write_manifest(Path::new(opts.output), &manifest) {
        eprintln!("{message}");
        std::process::exit(1);
    }
    eprintln!("\u{2713} Wrote jobs manifest \u{2192} {}", opts.output);
}

/// Interpret a finished dump child: `Ok(stdout)` when it exited cleanly,
/// `Err(exit_code)` when it failed (the caller reports and propagates the code).
///
/// Returns the child's captured stdout (lossily UTF-8 decoded) without inspecting
/// its contents — validation that it is a well-formed manifest is [`validate_manifest`]'s
/// job. Extracted from [`run`] so the success/failure decision and stdout capture
/// are unit-testable without spawning a real process.
fn manifest_from_child(success: bool, code: Option<i32>, stdout: &[u8]) -> Result<String, i32> {
    if success {
        Ok(String::from_utf8_lossy(stdout).into_owned())
    } else {
        Err(code.unwrap_or(1))
    }
}

/// Validate that `stdout` is the expected jobs manifest before it is written.
///
/// A clean child exit does not guarantee the captured stdout is the manifest: any
/// bytes the app writes to stdout during boot (from a custom config loader or
/// telemetry initializer, or a stray `println!`) land here too, and writing them
/// verbatim produces a corrupt file. `autumn doctor` parses only a valid TOML
/// `queues` array and silently ignores anything else, so a corrupt manifest lets
/// the topology coverage check false-pass without the app-declared queues.
///
/// Mirrors `autumn routes`, which strict-parses the child's stdout (as JSON there,
/// TOML here) and errors rather than accepting unexpected output. Requires the
/// stdout to parse as TOML with a top-level `queues` array of strings; on success
/// returns the original `stdout` unchanged so the on-disk bytes are byte-identical
/// to what the app emitted (preserving the highest-priority-first ordering).
fn validate_manifest(stdout: &str) -> Result<String, String> {
    let hint = "app did not emit a valid jobs manifest on stdout; \
                ensure nothing else writes to stdout during `autumn jobs manifest`";

    let value: toml::Value = toml::from_str(stdout)
        .map_err(|e| format!("{hint} (stdout did not parse as TOML: {e})"))?;
    let queues = value
        .get("queues")
        .ok_or_else(|| format!("{hint} (no top-level `queues` array found)"))?
        .as_array()
        .ok_or_else(|| format!("{hint} (`queues` is not an array)"))?;
    if !queues.iter().all(toml::Value::is_str) {
        return Err(format!("{hint} (`queues` is not an array of strings)"));
    }

    // Return the stdout unchanged so the on-disk bytes are byte-identical to what
    // the app emitted (preserving the highest-priority-first ordering).
    Ok(stdout.to_owned())
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

    // ── validate_manifest ───────────────────────────────────────────────────

    #[test]
    fn validate_manifest_accepts_well_formed_manifest() {
        let stdout = "queues = [\"critical\", \"email\"]\n";
        let validated = validate_manifest(stdout).expect("valid manifest should pass");
        // The stdout is returned byte-for-byte so the on-disk manifest is exactly
        // what the app emitted (preserving highest-priority-first ordering).
        assert_eq!(validated, stdout);
    }

    #[test]
    fn validate_manifest_accepts_empty_queues_array() {
        // A configured-but-empty queue set is a legitimate manifest, distinct
        // from missing output.
        let stdout = "queues = []\n";
        assert_eq!(
            validate_manifest(stdout).expect("empty array is valid"),
            stdout
        );
    }

    #[test]
    fn validate_manifest_rejects_leading_stray_line() {
        // A stray log/telemetry line before the manifest makes the whole payload
        // fail to parse as TOML — we must error and refuse to write, not silently
        // persist a corrupt file (strict-parse, mirroring `autumn routes`).
        let stdout = "some log line\nqueues = [\"critical\"]\n";
        let err = validate_manifest(stdout).expect_err("stray line must be rejected");
        assert!(
            err.contains("did not emit a valid jobs manifest"),
            "expected manifest-validation error, got: {err}"
        );
    }

    #[test]
    fn validate_manifest_rejects_empty_stdout() {
        let err = validate_manifest("").expect_err("empty stdout must be rejected");
        assert!(
            err.contains("did not emit a valid jobs manifest"),
            "expected manifest-validation error, got: {err}"
        );
    }

    #[test]
    fn validate_manifest_rejects_toml_without_queues_key() {
        // Valid TOML that lacks the `queues` array must be rejected, and the
        // message should name the missing key.
        let stdout = "other = [\"critical\"]\n";
        let err = validate_manifest(stdout).expect_err("missing queues key must be rejected");
        assert!(
            err.contains("no top-level `queues` array"),
            "expected missing-key error, got: {err}"
        );
    }

    #[test]
    fn validate_manifest_rejects_non_string_queue_elements() {
        // `queues` present but not an array of strings must be rejected.
        let stdout = "queues = [1, 2]\n";
        let err = validate_manifest(stdout).expect_err("non-string elements must be rejected");
        assert!(
            err.contains("not an array of strings"),
            "expected element-type error, got: {err}"
        );
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

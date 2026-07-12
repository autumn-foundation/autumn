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

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping jobs manifest",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let manifest = String::from_utf8_lossy(&output.stdout);
    write_manifest(Path::new(opts.output), &manifest);
    eprintln!("\u{2713} Wrote jobs manifest \u{2192} {}", opts.output);
}

/// Write `contents` to `path`, creating any missing parent directories.
fn write_manifest(path: &Path, contents: &str) {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("\u{2717} Failed to create {}: {e}", parent.display());
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(path, contents) {
        eprintln!("\u{2717} Failed to write {}: {e}", path.display());
        std::process::exit(1);
    }
}

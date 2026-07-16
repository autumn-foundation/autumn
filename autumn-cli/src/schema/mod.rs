//! Declarative-schema tooling (wave-15, tracking issue #1975).
//!
//! Slice 2 lives here: [`parse`], a `syn`-backed reader that lifts an app's
//! `#[model]` structs into the shared [`autumn_schema_core`] IR — the
//! **desired state** later slices (a checked-in snapshot, then the diff engine
//! and the full `autumn schema` command group) build on. It is read-only:
//! nothing here writes a migration, `schema.rs`, or any other codegen output.
//!
//! The experimental [`run`] entrypoint backs `autumn schema parse <path>` (slice
//! 2) and `autumn schema snapshot` (slice 3). `parse` prints the parsed IR as
//! JSON; `snapshot` writes the canonical, checked-in [`snapshot`] baseline the
//! later diff engine consumes. These are the first actions of the eventual full
//! `autumn schema` group.

pub mod parse;
pub mod snapshot;

use std::path::{Path, PathBuf};

use autumn_schema_core::Backend;

use parse::parse_models_path;
use snapshot::{SNAPSHOT_DEFAULT_PATH, SchemaSnapshot};

/// A `--backend pg|sqlite` override for the `snapshot` action, mapping onto the
/// schema-core [`Backend`] dialect tag.
#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum BackendArg {
    /// `PostgreSQL` (accepted as `pg` or `postgres`).
    #[value(alias = "postgres")]
    Pg,
    /// `SQLite`.
    Sqlite,
}

impl From<BackendArg> for Backend {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Pg => Self::Postgres,
            BackendArg::Sqlite => Self::Sqlite,
        }
    }
}

/// The `autumn schema` subcommand actions (experimental). Slices 2–3 ship
/// `parse` and `snapshot`; `diff`/… arrive in later slices.
#[derive(clap::Subcommand, Debug)]
pub enum SchemaAction {
    /// Parse `#[model]` structs at PATH (a `.rs` file or a directory of them)
    /// and print the resulting schema IR as JSON. Experimental / read-only.
    Parse {
        /// A `.rs` file or a directory containing `*.rs` model files.
        path: std::path::PathBuf,
    },
    /// Write a canonical, versioned, dialect-tagged snapshot of the declared
    /// `#[model]` structs — the checked-in diff baseline a later slice's diff
    /// engine compares the desired state against. Read-only w.r.t. the models.
    Snapshot {
        /// A `.rs` model file or a directory of them to snapshot. Defaults to
        /// the app's `src/models` directory, else `src/models.rs`.
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
        /// Where to write the snapshot. Defaults to `.autumn/schema-snapshot.json`.
        /// Mutually exclusive with `--stdout` (writing a file and printing to
        /// stdout are two different output modes). The default is applied in
        /// code when omitted, so `conflicts_with` never misfires on the default.
        #[arg(long, value_name = "PATH", conflicts_with = "stdout")]
        out: Option<PathBuf>,
        /// The dialect to tag the snapshot with. Defaults to the project's
        /// configured database backend (`detect_backend`).
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Print the canonical JSON to stdout instead of writing a file (useful
        /// for diffing / tests).
        #[arg(long)]
        stdout: bool,
    },
}

/// Run an `autumn schema` action. Prints to stdout on success; on error, writes
/// a message to stderr and exits non-zero (matching the other CLI handlers,
/// which own their own error reporting).
pub fn run(action: SchemaAction) {
    let result = match action {
        SchemaAction::Parse { path } => run_parse(&path).map(|json| println!("{json}")),
        SchemaAction::Snapshot {
            from,
            out,
            backend,
            stdout,
        } => run_snapshot(from.as_deref(), out.as_deref(), backend, stdout),
    };
    if let Err(message) = result {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

/// Map the CLI's runtime [`autumn_web::config::DatabaseBackend`] onto the
/// schema-core [`Backend`] dialect tag.
const fn map_detected_backend(detected: autumn_web::config::DatabaseBackend) -> Backend {
    match detected {
        autumn_web::config::DatabaseBackend::Postgres => Backend::Postgres,
        autumn_web::config::DatabaseBackend::Sqlite => Backend::Sqlite,
    }
}

/// Resolve the default models source when `--from` is omitted: the app's
/// `src/models` directory if it exists, else the single-file `src/models.rs`
/// layout, else a clear error telling the user to pass `--from`. Autumn supports
/// both layouts, so the snapshot default must accept either.
///
/// Takes an explicit `project_root` (rather than reading the process CWD) so it
/// is testable without mutating the current directory.
fn resolve_default_models_path(project_root: &Path) -> Result<PathBuf, String> {
    let dir = project_root.join("src").join("models");
    if dir.is_dir() {
        return Ok(dir);
    }
    let file = project_root.join("src").join("models.rs");
    if file.is_file() {
        return Ok(file);
    }
    Err(format!(
        "no models found at {} or {} — pass --from to point at your models file or directory",
        dir.display(),
        file.display()
    ))
}

/// Whether the `snapshot` command requires the current directory to be the
/// project root. It does whenever any input falls back to a project-relative
/// default: the source (`--from` omitted → `src/models`), the default output
/// file (`--out` omitted while not writing to stdout → `.autumn/…`), or the
/// auto-detected backend (`--backend` omitted — detection reads the root's
/// `autumn.toml` / `.env`, so anywhere else it would silently mis-detect
/// Postgres).
///
/// A fully explicit invocation — `--from` plus `--out`/`--stdout` plus
/// `--backend` — needs no project root and can run from anywhere. Kept pure so
/// the decision matrix is unit-testable without touching the process CWD.
const fn needs_project_root(
    from: Option<&Path>,
    out: Option<&Path>,
    stdout: bool,
    backend: Option<BackendArg>,
) -> bool {
    from.is_none() || (out.is_none() && !stdout) || backend.is_none()
}

/// The error surfaced when the `snapshot` command needs the project root but the
/// current directory is not one. When auto-detecting the backend is one of the
/// reasons, the message names `--backend` so the user can either run from the
/// root or opt out of detection; otherwise it is the generic project-root
/// message (a default path, not the backend, forced the requirement).
fn project_root_required_error(backend_is_a_reason: bool) -> String {
    if backend_is_a_reason {
        "cannot auto-detect the database backend outside the project root — \
         run from the project root or pass --backend pg|sqlite"
            .to_string()
    } else {
        crate::generate::GenerateError::NotInProject.to_string()
    }
}

/// Assemble a snapshot from the declared models and either write it to `out`
/// (default [`SNAPSHOT_DEFAULT_PATH`]) or print it to stdout.
///
/// The backend is `--backend` when given, else the project's configured backend
/// (`generate::detect_backend`, the same pg-vs-sqlite resolution the generator
/// uses). The source is `--from` (a `.rs` file or a directory), else the
/// project's default models path (`src/models` directory, else `src/models.rs`;
/// see [`resolve_default_models_path`]). Parser diagnostics are surfaced on
/// stderr (non-fatal), mirroring `parse`.
///
/// Building the baseline from the declared models (not a live DB) is deliberate:
/// at adoption time the models and the database agree, so this establishes the
/// initial baseline with no database connection. A `--from-db` oracle that reads
/// a live Postgres schema is deferred to a later slice (see the module docs).
fn run_snapshot(
    from: Option<&Path>,
    out: Option<&Path>,
    backend: Option<BackendArg>,
    stdout: bool,
) -> Result<(), String> {
    let project_root = std::env::current_dir()
        .map_err(|e| format!("failed to resolve the current directory: {e}"))?;

    // When any input falls back to a project-relative default the command needs
    // the current directory to be the project root (see `needs_project_root`):
    // the source (`--from` omitted → `src/models`), the default output file
    // (`--out` omitted while writing a file), or — critically — the auto-detected
    // backend (`--backend` omitted → detection reads the root's `autumn.toml` /
    // `.env`). Run from a subdirectory without `--backend`, `detect_backend`
    // would find no config and silently tag the snapshot Postgres, producing a
    // wrong baseline for a SQLite app. Fail fast with a clear message instead of
    // a confusing IO error or a mis-tagged snapshot. A fully explicit invocation
    // (`--from` plus `--out`/`--stdout` plus `--backend`) needs no project root.
    let backend_given = backend.is_some();
    if needs_project_root(from, out, stdout, backend)
        && crate::generate::ensure_project_root(&project_root).is_err()
    {
        return Err(project_root_required_error(!backend_given));
    }

    let models_path = match from {
        Some(path) => path.to_path_buf(),
        None => resolve_default_models_path(&project_root)?,
    };

    let backend = backend.map_or_else(
        || map_detected_backend(crate::generate::detect_backend(&project_root)),
        Backend::from,
    );

    let parsed = parse_models_path(&models_path, backend).map_err(|e| e.to_string())?;
    for diag in &parsed.diagnostics {
        eprintln!("warning: {}", diag.message);
    }

    let snapshot = SchemaSnapshot::new(backend, parsed.tables);
    if stdout {
        print!("{}", snapshot::to_canonical_json(&snapshot));
        return Ok(());
    }

    let out_path = out.map_or_else(|| PathBuf::from(SNAPSHOT_DEFAULT_PATH), Path::to_path_buf);
    snapshot::write_snapshot(&out_path, &snapshot).map_err(|e| e.to_string())?;
    println!(
        "wrote schema snapshot for {} table(s) to {}",
        snapshot.tables.len(),
        out_path.display()
    );
    Ok(())
}

/// Parse a file or directory and render the tables as pretty JSON, surfacing any
/// per-field diagnostics on stderr. Split out (returning `Result`) so it is
/// testable without a process exit.
fn run_parse(path: &Path) -> Result<String, String> {
    // The parser IR is backend-tagged; `parse` defaults to Postgres (the fully
    // wired runtime backend). A backend flag is a later-slice concern.
    let backend = Backend::Postgres;
    let parsed = parse_models_path(path, backend).map_err(|e| e.to_string())?;

    for diag in &parsed.diagnostics {
        eprintln!("warning: {}", diag.message);
    }

    serde_json::to_string_pretty(&parsed.tables).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_resolver_prefers_src_models_dir() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("src").join("models");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // A `src/models.rs` file also present must not win over the directory.
        std::fs::write(root.path().join("src").join("models.rs"), "").expect("write file");

        let resolved = resolve_default_models_path(root.path()).expect("resolve");
        assert_eq!(resolved, dir);
    }

    #[test]
    fn default_resolver_falls_back_to_single_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        let file = src.join("models.rs");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_default_models_path(root.path()).expect("resolve");
        assert_eq!(resolved, file);
    }

    #[test]
    fn fully_explicit_invocation_needs_no_project_root() {
        let from = Path::new("models.rs");
        let out = Path::new("snap.json");
        // `--from` + `--out` + `--backend`: nothing is defaulted, so the command
        // can run from any directory.
        assert!(!needs_project_root(
            Some(from),
            Some(out),
            false,
            Some(BackendArg::Sqlite)
        ));
        // `--from` + `--stdout` + `--backend` likewise.
        assert!(!needs_project_root(
            Some(from),
            None,
            true,
            Some(BackendArg::Sqlite)
        ));
    }

    #[test]
    fn missing_backend_requires_project_root_even_with_explicit_paths() {
        let from = Path::new("models.rs");
        let out = Path::new("snap.json");
        // Explicit `--from` and `--out`/`--stdout`, but no `--backend`: detection
        // needs the root's config, so the root is still required.
        assert!(needs_project_root(Some(from), Some(out), false, None));
        assert!(needs_project_root(Some(from), None, true, None));
    }

    #[test]
    fn default_paths_require_project_root() {
        let from = Path::new("models.rs");
        let out = Path::new("snap.json");
        // Missing `--from` defaults the source to `src/models`.
        assert!(needs_project_root(
            None,
            Some(out),
            false,
            Some(BackendArg::Sqlite)
        ));
        // Missing `--out` while not writing to stdout defaults the output file.
        assert!(needs_project_root(
            Some(from),
            None,
            false,
            Some(BackendArg::Sqlite)
        ));
        // Bare `snapshot` (everything defaulted) certainly needs the root.
        assert!(needs_project_root(None, None, false, None));
    }

    #[test]
    fn root_required_error_names_backend_when_detection_is_a_reason() {
        let msg = project_root_required_error(true);
        assert!(
            msg.contains("--backend"),
            "guides the user to --backend: {msg}"
        );
        assert!(
            msg.contains("auto-detect"),
            "explains the auto-detection failure: {msg}"
        );
    }

    #[test]
    fn root_required_error_is_generic_when_backend_is_explicit() {
        // Backend was given, so only a default path forced the requirement — the
        // generic project-root message is used (no misleading --backend hint).
        let msg = project_root_required_error(false);
        assert!(!msg.contains("--backend"), "no --backend hint: {msg}");
        assert!(
            msg.contains("Autumn project"),
            "the generic project-root message: {msg}"
        );
    }

    /// A minimal `clap::Parser` wrapper so the `SchemaAction` subcommand args
    /// (a `Subcommand`, not a `Parser`) can be exercised at parse time.
    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(subcommand)]
        action: SchemaAction,
    }

    #[test]
    fn out_and_stdout_are_mutually_exclusive() {
        use clap::Parser;
        // clap rejects `--out` together with `--stdout` before `run_snapshot`
        // ever runs, so the misleading "printed but `--out` file unwritten" case
        // is impossible.
        let err = TestCli::try_parse_from([
            "autumn",
            "snapshot",
            "--from",
            "models.rs",
            "--out",
            "snap.json",
            "--stdout",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        // Each mode alone still parses.
        assert!(
            TestCli::try_parse_from(["autumn", "snapshot", "--from", "models.rs", "--stdout"])
                .is_ok()
        );
        assert!(
            TestCli::try_parse_from([
                "autumn",
                "snapshot",
                "--from",
                "models.rs",
                "--out",
                "snap.json"
            ])
            .is_ok()
        );
    }

    #[test]
    fn default_resolver_errors_when_neither_exists() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("src")).expect("mkdir");

        let err = resolve_default_models_path(root.path()).unwrap_err();
        assert!(
            err.contains("--from"),
            "error tells the user to pass --from"
        );
    }
}

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

use parse::{parse_model_source, parse_models_dir};
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
        /// The models directory to snapshot. Defaults to the app's `src/models`.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Where to write the snapshot. Defaults to `.autumn/schema-snapshot.json`.
        #[arg(long, value_name = "PATH")]
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

/// Assemble a snapshot from the declared models and either write it to `out`
/// (default [`SNAPSHOT_DEFAULT_PATH`]) or print it to stdout.
///
/// The backend is `--backend` when given, else the project's configured backend
/// (`generate::detect_backend`, the same pg-vs-sqlite resolution the generator
/// uses). The source is `--from`, else the project's `src/models` directory.
/// Parser diagnostics are surfaced on stderr (non-fatal), mirroring `parse`.
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

    // When either default is in play — `src/models` for the source (no `--from`)
    // or `.autumn/…` for the output (no `--out`, writing a file) — the command
    // assumes the current directory is the project root. Fail fast with a clear
    // message instead of a confusing IO error (`src/models` / `.autumn/` not
    // found) when it is run from a subdirectory. A fully explicit invocation
    // (`--from` plus either `--out` or `--stdout`) needs no project root and
    // skips the check.
    if from.is_none() || (out.is_none() && !stdout) {
        crate::generate::ensure_project_root(&project_root).map_err(|e| e.to_string())?;
    }

    let models_dir = from.map_or_else(
        || project_root.join("src").join("models"),
        Path::to_path_buf,
    );

    let backend = backend.map_or_else(
        || map_detected_backend(crate::generate::detect_backend(&project_root)),
        Backend::from,
    );

    let parsed = parse_models_dir(&models_dir, backend).map_err(|e| e.to_string())?;
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
    let parsed = if path.is_dir() {
        parse_models_dir(path, backend)
    } else {
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        parse_model_source(&src, backend)
    }
    .map_err(|e| e.to_string())?;

    for diag in &parsed.diagnostics {
        eprintln!("warning: {}", diag.message);
    }

    serde_json::to_string_pretty(&parsed.tables).map_err(|e| e.to_string())
}

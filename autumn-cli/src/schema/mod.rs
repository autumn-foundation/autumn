//! Declarative-schema tooling (wave-15, tracking issue #1975).
//!
//! Slice 2 lives here: [`parse`], a `syn`-backed reader that lifts an app's
//! `#[model]` structs into the shared [`autumn_schema_core`] IR — the
//! **desired state** later slices (a checked-in snapshot, then the diff engine
//! and the full `autumn schema` command group) build on. It is read-only:
//! nothing here writes a migration, `schema.rs`, or any other codegen output.
//!
//! The experimental [`run`] entrypoint backs `autumn schema parse <path>`, which
//! prints the parsed IR as JSON. It is the first (deliberately minimal) stub of
//! the eventual `autumn schema` group and gives the parser a real caller.

pub mod parse;

use std::path::Path;

use autumn_schema_core::Backend;

use parse::{parse_model_source, parse_models_dir};

/// The `autumn schema` subcommand actions (experimental; slice 2 ships only
/// `parse`). Kept here so the slice-6 command group can grow additional actions
/// (`diff`, `snapshot`, …) alongside it in later slices.
#[derive(clap::Subcommand, Debug)]
pub enum SchemaAction {
    /// Parse `#[model]` structs at PATH (a `.rs` file or a directory of them)
    /// and print the resulting schema IR as JSON. Experimental / read-only.
    Parse {
        /// A `.rs` file or a directory containing `*.rs` model files.
        path: std::path::PathBuf,
    },
}

/// Run an `autumn schema` action. Prints to stdout on success; on error, writes
/// a message to stderr and exits non-zero (matching the other CLI handlers,
/// which own their own error reporting).
pub fn run(action: SchemaAction) {
    match action {
        SchemaAction::Parse { path } => match run_parse(&path) {
            Ok(json) => println!("{json}"),
            Err(message) => {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        },
    }
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

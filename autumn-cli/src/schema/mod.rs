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

pub mod diff;
pub mod doctor;
pub mod introspect;
pub mod migrate;
pub mod parse;
pub mod snapshot;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use autumn_schema_core::{Backend, Column, Table};

use diff::{MigrationPlan, SchemaChange};
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
    /// Diff the declared `#[model]` structs against the checked-in snapshot and
    /// either print the pending migration (default) or write it as a diesel
    /// `up.sql`/`down.sql` pair (`--write-migration`). Destructive drops are
    /// refused unless `--allow-destructive` is passed.
    Diff {
        /// Models source (a `.rs` file or a directory). Defaults to `src/models`
        /// then `src/models.rs`.
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
        /// Baseline snapshot. Defaults to `.autumn/schema-snapshot.json`.
        #[arg(long, value_name = "PATH")]
        snapshot: Option<PathBuf>,
        /// The dialect. Defaults to the project's configured backend.
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Write `migrations/<timestamp>_<name>/{up,down}.sql` instead of printing.
        #[arg(long)]
        write_migration: bool,
        /// Migration directory suffix when writing. Defaults to `schema_update`.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Permit destructive drops / an independent drop+add (tier-2 guard).
        #[arg(long)]
        allow_destructive: bool,
    },
    /// Introspect the configured Postgres database and write a canonical,
    /// dialect-tagged snapshot of its live shape — the DB-derived diff baseline.
    ///
    /// Read-only w.r.t. the database (only catalog reads). Unlike `snapshot`
    /// (which derives the baseline from the declared `#[model]` structs), `pull`
    /// derives it from the live database, so a brownfield schema — or a schema
    /// that drifted from the models — is captured exactly as it exists. Postgres
    /// only in this slice; a `SQLite` URL is refused with a clear message.
    Pull {
        /// Config profile whose database URL to introspect (defaults to the
        /// ambient profile resolution the other CLI commands use).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Where to write the snapshot. Defaults to `.autumn/schema-snapshot.json`.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Override the dialect tag / apply-path selection. Defaults to the
        /// backend implied by the resolved database URL.
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Print what would change without writing the snapshot file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Apply pending migration files against the configured database.
    ///
    /// Does NOT modify the checked-in schema snapshot: the baseline advances at
    /// generation time via `schema diff --write-migration`, so this command only
    /// applies pending migrations. Provider-locked against the snapshot's dialect;
    /// the destructive-change guards ran at diff time, so migration files apply
    /// verbatim here.
    Migrate {
        /// Config profile whose database URL to apply against (defaults to the
        /// ambient profile resolution the other CLI commands use).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },
    /// Read-only diagnosis of the declarative-schema state: filesystem, snapshot,
    /// model drift, backend provider-lock, and pending migrations. Exits
    /// non-zero when any check is an actionable error.
    Doctor {
        /// Config profile whose database URL to probe (defaults to the ambient
        /// profile resolution the other CLI commands use).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Emit the checks as JSON instead of the aligned text report.
        #[arg(long)]
        json: bool,
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
        SchemaAction::Diff {
            from,
            snapshot,
            backend,
            write_migration,
            name,
            allow_destructive,
        } => run_diff(
            from.as_deref(),
            snapshot.as_deref(),
            backend,
            write_migration,
            name.as_deref(),
            allow_destructive,
        ),
        SchemaAction::Pull {
            profile,
            out,
            backend,
            dry_run,
        } => run_pull(profile.as_deref(), out.as_deref(), backend, dry_run),
        SchemaAction::Migrate { profile } => migrate::run_migrate(profile.as_deref()),
        SchemaAction::Doctor { profile, json } => doctor::run_doctor(profile.as_deref(), json),
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

/// Resolve the schema-command backend for an explicit `--profile`, from the
/// already profile-resolved primary database `url`.
///
/// When a URL is configured its scheme is authoritative
/// ([`DatabaseBackend::detect`](autumn_web::config::DatabaseBackend::detect)),
/// so `--profile <name>` selects the apply path / provider-lock of the database
/// it actually acts against; only when no URL is configured does it fall back to
/// the profile-aware project default
/// ([`crate::generate::detect_backend_for_profile`]). Shared by `schema migrate`
/// and `schema doctor` so the two commands can never derive the backend
/// inconsistently for the same profile. With `profile == None` the resolution is
/// unchanged from the ambient behavior.
fn backend_for_url(project_root: &Path, profile: Option<&str>, url: Option<&str>) -> Backend {
    url.and_then(autumn_web::config::DatabaseBackend::detect)
        .map_or_else(
            || {
                map_detected_backend(crate::generate::detect_backend_for_profile(
                    project_root,
                    profile,
                ))
            },
            map_detected_backend,
        )
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

/// Resolve the declarative models source only when it actually exists.
///
/// Mirrors [`resolve_default_models_path`]'s `src/models` dir → `src/models.rs`
/// file precedence, but returns `None` (not an error) when neither is present —
/// so callers that must degrade gracefully on absence (the `migrate` snapshot
/// refresh, `doctor`'s drift check) share one resolver instead of duplicating it.
fn existing_models_path(project_root: &Path) -> Option<PathBuf> {
    let dir = project_root.join("src").join("models");
    if dir.is_dir() {
        return Some(dir);
    }
    let file = project_root.join("src").join("models.rs");
    if file.is_file() {
        return Some(file);
    }
    None
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

/// Introspect the configured Postgres database and write (or, with `--dry-run`,
/// describe) a canonical, dialect-tagged snapshot of its live shape.
///
/// This is the DB-introspection counterpart to `snapshot` (which derives the
/// baseline from the declared models). The resolution order — project root, then
/// the profile-resolved database URL, then the URL-implied backend (honoring a
/// `--backend` override) — mirrors `schema migrate` / `schema doctor`, so the
/// three commands can never derive the backend inconsistently for one profile.
///
/// Postgres only in this slice: a resolved `SQLite` backend is refused loudly (no
/// partial support, no partial write). Before writing, a **provider-lock** guard
/// refuses to clobber an existing snapshot tagged for another backend.
fn run_pull(
    profile: Option<&str>,
    out: Option<&Path>,
    backend: Option<BackendArg>,
    dry_run: bool,
) -> Result<(), String> {
    let project_root = std::env::current_dir()
        .map_err(|e| format!("failed to resolve the current directory: {e}"))?;
    pull_at(&project_root, profile, out, backend, dry_run)
}

/// The body of [`run_pull`], taking an explicit `project_root` so the wiring is
/// testable without mutating the process CWD (mirrors `diff_at` / `migrate_at`).
fn pull_at(
    project_root: &Path,
    profile: Option<&str>,
    out: Option<&Path>,
    backend: Option<BackendArg>,
    dry_run: bool,
) -> Result<(), String> {
    crate::generate::ensure_project_root(project_root)
        .map_err(|_| crate::generate::GenerateError::NotInProject.to_string())?;

    // Resolve the write/primary database URL exactly as the other schema commands
    // do, honoring an explicit `--profile`.
    let url = crate::migrate::resolve_primary_url(profile).ok_or_else(|| {
        "no database URL configured — set AUTUMN_DATABASE__URL or DATABASE_URL, or add a \
         [database] primary_url to autumn.toml"
            .to_string()
    })?;

    // Backend from the SAME profile-resolved context as the URL (its scheme is
    // authoritative), honoring an explicit `--backend` override.
    let backend: Backend = backend.map_or_else(
        || backend_for_url(project_root, profile, Some(url.as_str())),
        Backend::from,
    );

    // SQLite introspection is a future slice of #1975 — fail loud, never partial.
    if backend == Backend::Sqlite {
        return Err(
            "`autumn schema pull` currently supports Postgres only — SQLite database \
             introspection is a future slice of the declarative-schema work (#1975). No \
             snapshot was written."
                .to_string(),
        );
    }

    let out_path = out.map_or_else(
        || project_root.join(SNAPSHOT_DEFAULT_PATH),
        Path::to_path_buf,
    );

    // PROVIDER-LOCK: refuse to overwrite an existing snapshot tagged for another
    // backend (mirrors `ensure_backend_matches` / the doctor provider-lock check).
    // A missing or unreadable-as-absent snapshot is fine — this is the first
    // baseline. The existing tables also serve as the `--dry-run` diff baseline.
    let existing = load_existing_snapshot(&out_path);
    if let Some(existing) = &existing
        && existing.backend != backend
    {
        return Err(format!(
            "refusing to overwrite the existing snapshot at {} (backend {:?}) with a {:?} pull \
             — the snapshot is provider-locked to {:?}. Point --out elsewhere or remove the \
             mismatched snapshot first.",
            out_path.display(),
            existing.backend,
            backend,
            existing.backend
        ));
    }

    // Introspect the live database into the IR.
    let tables = introspect::introspect_postgres(&url).map_err(|e| e.to_string())?;
    let snapshot = SchemaSnapshot::new(backend, tables);

    if dry_run {
        report_pull_dry_run(&snapshot, existing.as_ref(), &out_path);
        return Ok(());
    }

    // A file that is present on disk but did not load (`existing` is `None` while
    // the path exists) is corrupt/unreadable-as-absent — a non-dry-run pull is
    // about to replace it. Tell the user rather than silently clobber it. An
    // absent snapshot stays silent (it exists nowhere to overwrite); a readable
    // but backend-mismatched snapshot never reaches here (provider-lock refused
    // above).
    if existing.is_none() && out_path.exists() {
        eprintln!(
            "note: existing snapshot at {} was unreadable; replacing it.",
            out_path.display()
        );
    }

    snapshot::write_snapshot(&out_path, &snapshot).map_err(|e| e.to_string())?;
    println!(
        "pulled schema snapshot for {} table(s) from the database to {}",
        snapshot.tables.len(),
        out_path.display()
    );
    Ok(())
}

/// Load the snapshot at `path` for the pull's provider-lock / dry-run baseline.
/// A missing OR unreadable file is treated as **absent** (`Ok(None)`) — a pull
/// establishes a fresh baseline, so a corrupt prior file is not fatal — but a
/// backend-mismatched *readable* snapshot still surfaces to the provider-lock
/// caller (it loaded fine; only its backend disagrees).
fn load_existing_snapshot(path: &Path) -> Option<SchemaSnapshot> {
    snapshot::load_snapshot(path).ok()
}

/// Print what a `--dry-run` pull would change between the existing snapshot
/// (baseline) and the freshly-pulled tables, without writing anything.
///
/// Drift is computed BIDIRECTIONALLY (via [`doctor::compute_db_schema_drift`]) so
/// the dry-run never under-reports: the forward plan is what pulling would change
/// in the snapshot, but a manually-dropped default / FK / CHECK in the live DB is
/// invisible to the forward pass (a desired `None` is "retained") and only surfaces
/// in the reverse plan. When the forward plan has changes it is printed via
/// [`diff::describe_plan`]; when only the reverse plan does, a short note names the
/// dropped-facet differences so they are not silently omitted.
fn report_pull_dry_run(
    pulled: &SchemaSnapshot,
    existing: Option<&SchemaSnapshot>,
    out_path: &Path,
) {
    match existing {
        None => {
            println!(
                "--dry-run: no existing snapshot at {} — would create a new baseline with {} \
                 table(s) from the database.",
                out_path.display(),
                pulled.tables.len()
            );
        }
        Some(existing) => {
            let drift = doctor::compute_db_schema_drift(&existing.tables, &pulled.tables);
            if !drift.forward.is_empty() {
                println!(
                    "--dry-run: the database differs from the snapshot at {} — {} change(s) would \
                     be captured (nothing written):",
                    out_path.display(),
                    drift.forward.changes.len()
                );
                print!("{}", diff::describe_plan(&drift.forward));
            } else if !drift.reverse.is_empty() {
                // The forward pass found nothing to pull, but the reverse pass did:
                // the live DB dropped a default / foreign key / CHECK the snapshot
                // still carries. Name it rather than falsely report "up to date".
                println!(
                    "--dry-run: the database at {} has {} dropped-default/foreign-key/CHECK \
                     difference(s) the snapshot still carries — run `autumn schema diff \
                     --write-migration` to generate a migration (nothing written).",
                    out_path.display(),
                    drift.reverse.changes.len()
                );
            } else {
                println!(
                    "--dry-run: the snapshot at {} is up to date — the database matches it.",
                    out_path.display()
                );
            }
        }
    }
}

/// Diff the declared models against the checked-in snapshot baseline and either
/// print the pending migration or write it as a diesel `up.sql`/`down.sql` pair.
///
/// **Provider-lock ordering (caller-owned):** the snapshot is loaded and its
/// backend tag is validated against the desired backend *before* the models are
/// parsed or diffed, so a dialect-mismatched snapshot fails cleanly with
/// [`snapshot::SnapshotError::BackendMismatch`] and no diff work runs. This is
/// the first caller of [`SchemaSnapshot::ensure_backend_matches`].
fn run_diff(
    from: Option<&Path>,
    snapshot_path: Option<&Path>,
    backend: Option<BackendArg>,
    write_migration: bool,
    name: Option<&str>,
    allow_destructive: bool,
) -> Result<(), String> {
    let project_root = std::env::current_dir()
        .map_err(|e| format!("failed to resolve the current directory: {e}"))?;
    diff_at(
        &project_root,
        from,
        snapshot_path,
        backend,
        write_migration,
        name,
        allow_destructive,
    )
}

/// The body of [`run_diff`], taking an explicit `project_root` (rather than the
/// process CWD) so the command wiring is testable without mutating the current
/// directory.
fn diff_at(
    project_root: &Path,
    from: Option<&Path>,
    snapshot_path: Option<&Path>,
    backend: Option<BackendArg>,
    write_migration: bool,
    name: Option<&str>,
    allow_destructive: bool,
) -> Result<(), String> {
    // When any input falls back to a project-relative default (the models
    // source, the default snapshot path, or the auto-detected backend), the
    // command needs the current directory to be the project root — mirroring
    // `run_snapshot`.
    let backend_given = backend.is_some();
    if (from.is_none() || snapshot_path.is_none() || backend.is_none())
        && crate::generate::ensure_project_root(project_root).is_err()
    {
        return Err(project_root_required_error(!backend_given));
    }

    // (a) Resolve the desired backend: `--backend`, else the project's configured
    //     backend.
    let backend: Backend = backend.map_or_else(
        || map_detected_backend(crate::generate::detect_backend(project_root)),
        Backend::from,
    );

    // (b) Load the baseline snapshot (default `SNAPSHOT_DEFAULT_PATH`). A missing
    //     file is a friendly "run `autumn schema snapshot` first" error.
    let snapshot_path = snapshot_path.map_or_else(
        || project_root.join(SNAPSHOT_DEFAULT_PATH),
        Path::to_path_buf,
    );
    let baseline = snapshot::load_snapshot(&snapshot_path).map_err(|e| match e {
        snapshot::SnapshotError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            format!(
                "no schema snapshot at {} — run `autumn schema snapshot` first to create the diff baseline",
                snapshot_path.display()
            )
        }
        other => other.to_string(),
    })?;

    // (c) PROVIDER-LOCK GUARD — before parsing/diffing.
    baseline
        .ensure_backend_matches(backend)
        .map_err(|e| e.to_string())?;

    // (d) Parse the desired state with the SAME backend tag.
    let models_path = match from {
        Some(path) => path.to_path_buf(),
        None => resolve_default_models_path(project_root)?,
    };
    let desired = parse_models_path(&models_path, backend).map_err(|e| e.to_string())?;
    for d in &desired.diagnostics {
        eprintln!("warning: {}", d.message);
    }

    // (e) Diff (pure) then guard (policy).
    let opts = diff::DiffOptions { allow_destructive };
    let plan = diff::diff_schema(&baseline.tables, &desired, opts);
    if plan.is_empty() {
        println!("No schema changes — models match the snapshot baseline.");
        return Ok(());
    }
    diff::guard_plan(&plan, opts).map_err(|e| e.to_string())?;

    // The SQLite table-recreate path needs each affected table's full desired (up)
    // / baseline (down) shape, which the per-change deltas don't carry; thread them
    // in via the context. Postgres ignores it (identical output).
    let ctx = diff::SchemaContext::from_tables(&desired.tables, &baseline.tables);
    let up = diff::emit_up_sql_with_context(&plan, &ctx).map_err(|e| e.to_string())?;
    let down = diff::emit_down_sql_with_context(&plan, &ctx).map_err(|e| e.to_string())?;

    if !write_migration {
        print!("{}", diff::describe_plan(&plan));
        println!("\n-- up.sql\n{up}\n-- down.sql\n{down}");
        return Ok(());
    }

    // (f) Write the migration dir, matching the generator's naming convention.
    let ts = crate::generate::timestamp_now();
    let suffix = crate::generate::naming::snake(name.unwrap_or("schema_update"));
    let dir = write_migration_dir(project_root, &ts, &suffix, &up, &down)?;

    // (g) Advance the checked-in snapshot to the state this migration converges
    // the database on. The snapshot now moves at GENERATION time (here), not at
    // apply time — `schema migrate` only applies files. The target is the guarded
    // `plan` PROJECTED onto the baseline (not `desired.tables` wholesale), so a
    // change the guard reduced or refused is never baked into the baseline: the
    // snapshot tracks exactly what the emitted SQL produces. A plain `schema diff`
    // (no `--write-migration`) returned above (step e), so it never reaches here
    // and never touches the snapshot.
    //
    // The migration files and the checked-in snapshot must advance together: if
    // the snapshot write fails (read-only dir, full disk, ...) after the
    // migration is on disk, retrying `schema diff --write-migration` would
    // regenerate the same SQL as a second migration (baseline never advanced)
    // and applying both would fail on duplicate DDL. Roll the migration dir back
    // on a snapshot-write failure so the pre-command state is left intact.
    let target_tables = project_plan_target(&baseline.tables, &plan);
    if let Err(e) =
        snapshot::write_snapshot(&snapshot_path, &SchemaSnapshot::new(backend, target_tables))
    {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e.to_string());
    }

    println!(
        "wrote migration {} ({} change(s))",
        dir.display(),
        plan.changes.len()
    );
    println!("advanced schema snapshot at {}", snapshot_path.display());
    Ok(())
}

/// Project the guarded migration `plan` onto the `baseline` tables to compute the
/// state the database will be in once the migration applies — the new snapshot
/// baseline `schema diff --write-migration` advances to.
///
/// Each [`SchemaChange`] carries the full desired objects/values (they come from
/// the desired state), so applying them onto a clone of the baseline is verbatim
/// and convergent: the result equals what the emitted `up.sql` produces. This is
/// deliberately NOT a snapshot of `desired.tables` wholesale — the guard may have
/// reduced or refused parts of the delta, and the snapshot must track only what
/// the migration actually does. Table order is irrelevant here: `write_snapshot`
/// re-sorts tables by name canonically.
fn project_plan_target(baseline: &[Table], plan: &MigrationPlan) -> Vec<Table> {
    // Name-keyed working set cloned from the baseline.
    let mut tables: BTreeMap<String, Table> = baseline
        .iter()
        .map(|t| (t.name.clone(), t.clone()))
        .collect();

    // Exhaustive match with NO wildcard arm: a future `SchemaChange` variant
    // forces a compile error here rather than silently mis-projecting the target.
    for change in &plan.changes {
        match change {
            SchemaChange::CreateTable(table) => {
                tables.insert(table.name.clone(), table.clone());
            }
            SchemaChange::DropTable(table) => {
                tables.remove(&table.name);
            }
            SchemaChange::AddColumn { table, column } => {
                if let Some(t) = tables.get_mut(table) {
                    t.columns.push(column.clone());
                }
            }
            SchemaChange::DropColumn { table, column } => {
                if let Some(t) = tables.get_mut(table) {
                    t.columns.retain(|c| c.name != column.name);
                }
            }
            SchemaChange::AlterColumnType {
                table, column, to, ..
            } => {
                if let Some(c) = column_mut(&mut tables, table, column) {
                    c.ty = to.clone();
                }
            }
            SchemaChange::SetNotNull { table, column } => {
                if let Some(c) = column_mut(&mut tables, table, column) {
                    c.nullable = false;
                }
            }
            SchemaChange::DropNotNull { table, column } => {
                if let Some(c) = column_mut(&mut tables, table, column) {
                    c.nullable = true;
                }
            }
            SchemaChange::SetDefault {
                table, column, to, ..
            } => {
                if let Some(c) = column_mut(&mut tables, table, column) {
                    c.default = Some(to.clone());
                }
            }
            SchemaChange::AddForeignKey {
                table,
                column,
                foreign_key,
            } => {
                if let Some(c) = column_mut(&mut tables, table, column) {
                    c.references = Some(foreign_key.clone());
                }
            }
            SchemaChange::AddIndex { table, index } => {
                if let Some(t) = tables.get_mut(table) {
                    t.indexes.push(index.clone());
                }
            }
            SchemaChange::DropIndex { table, index } => {
                if let Some(t) = tables.get_mut(table) {
                    t.indexes.retain(|i| i.name != index.name);
                }
            }
            SchemaChange::AddCheck { table, check } => {
                if let Some(t) = tables.get_mut(table) {
                    t.checks.push(check.clone());
                }
            }
            // The non-emittable marker variants: `guard_plan` refuses these before
            // we get here (a guarded plan never carries one), so projecting them is
            // a no-op — never a panic.
            SchemaChange::PrimaryKeyChange { .. }
            | SchemaChange::ForeignKeyChange { .. }
            | SchemaChange::DropTableBlockedByInboundFk { .. }
            | SchemaChange::AlterColumnTypeBlockedByFk { .. }
            | SchemaChange::AddForeignKeyToExistingColumn { .. }
            | SchemaChange::CreateTableBlockedBySkippedField { .. } => {}
        }
    }

    tables.into_values().collect()
}

/// Find a mutable column by name within a named table in the working set — the
/// column-facet helper [`project_plan_target`] uses to apply the `Alter*` /
/// `Set*` / `Drop*Null` column changes. Returns `None` when the table or column
/// is absent (a guarded plan never carries such a change, so a miss is a no-op).
fn column_mut<'a>(
    tables: &'a mut BTreeMap<String, Table>,
    table: &str,
    column: &str,
) -> Option<&'a mut Column> {
    tables
        .get_mut(table)
        .and_then(|t| t.columns.iter_mut().find(|c| c.name == column))
}

/// Write a `migrations/<ts>_<suffix>/{up,down}.sql` pair, creating the leaf
/// directory **exclusively**.
///
/// `create_dir_all` on the leaf would silently succeed if the directory already
/// existed and the following `fs::write`s would then OVERWRITE any `up.sql` /
/// `down.sql` already there — and because `timestamp_now` has 1-second
/// resolution, two `--write-migration --name <same>` runs in the same second
/// resolve to the same directory. So the parent `migrations/` is created
/// idempotently but the leaf is created with `create_dir`, which fails with
/// `AlreadyExists` if it is already present; that case is mapped to a clear
/// refusal and **neither** SQL file is written, so an already-generated (and
/// possibly committed or applied) migration is never clobbered.
fn write_migration_dir(
    project_root: &Path,
    ts: &str,
    suffix: &str,
    up: &str,
    down: &str,
) -> Result<std::path::PathBuf, String> {
    write_migration_dir_with(project_root, ts, suffix, up, down, |path, contents| {
        std::fs::write(path, contents)
    })
}

/// Inner implementation of [`write_migration_dir`] that takes the file-write
/// operation as a closure. Extracted so a test can deterministically force a
/// mid-write failure (e.g. the `down.sql` write) and assert the
/// partial-migration cleanup below; production always passes `std::fs::write`.
///
/// Transactional-on-failure: the leaf directory is created **exclusively** (so
/// this call, and only this call, owns it — a pre-existing COMPLETE migration
/// still trips the `AlreadyExists` collision guard and is never touched). Once
/// this call has created the dir, if EITHER SQL write fails the just-created dir
/// is best-effort removed (`remove_dir_all`, its own error ignored) and the
/// ORIGINAL write error is returned, so a partial `migrations/<ts>_<suffix>/`
/// containing only `up.sql` is never left behind to block a same-timestamp
/// retry.
fn write_migration_dir_with<W>(
    project_root: &Path,
    ts: &str,
    suffix: &str,
    up: &str,
    down: &str,
    write: W,
) -> Result<std::path::PathBuf, String>
where
    W: Fn(&Path, &str) -> std::io::Result<()>,
{
    let migrations_dir = project_root.join("migrations");
    let dir = migrations_dir.join(format!("{ts}_{suffix}"));
    std::fs::create_dir_all(&migrations_dir).map_err(|e| {
        format!(
            "failed to create migrations directory {}: {e}",
            migrations_dir.display()
        )
    })?;
    std::fs::create_dir(&dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "refusing to overwrite existing migration directory {}; a migration with this \
                 timestamp and name already exists",
                dir.display()
            )
        } else {
            format!(
                "failed to create migration directory {}: {e}",
                dir.display()
            )
        }
    })?;
    // From here on THIS call owns `dir` (exclusive `create_dir` above). If either
    // write fails, remove the dir we just created so no partial migration is left
    // to trip the collision guard on retry, and surface the original write error.
    let write_both = || -> Result<(), String> {
        write(&dir.join("up.sql"), up).map_err(|e| format!("failed to write up.sql: {e}"))?;
        write(&dir.join("down.sql"), down).map_err(|e| format!("failed to write down.sql: {e}"))?;
        Ok(())
    };
    if let Err(e) = write_both() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(dir)
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
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;

    // -- `diff` command wiring (tempdir, explicit project root) --------------

    /// Write a project scaffold: a `Cargo.toml` (so `ensure_project_root`
    /// passes), a single-file `src/models.rs`, and a snapshot at the default
    /// path. Returns the tempdir (kept alive by the caller).
    fn scaffold_project(models: &str, snapshot_json: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("Cargo.toml");
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");
        std::fs::write(src.join("models.rs"), models).expect("models.rs");
        let snap = root.path().join(".autumn");
        std::fs::create_dir_all(&snap).expect("mkdir .autumn");
        std::fs::write(snap.join("schema-snapshot.json"), snapshot_json).expect("snapshot");
        root
    }

    const POST_MODEL: &str = r#"
        #[autumn_web::model(managed)]
        pub struct Post {
            #[id]
            pub id: i64,
            pub title: String,
        }
    "#;

    /// A version-1 snapshot for a `posts(id, title)` table, matching `POST_MODEL`.
    fn posts_snapshot(backend: &str) -> String {
        format!(
            r#"{{
  "snapshot_version": 1,
  "backend": "{backend}",
  "tables": [
    {{
      "name": "posts",
      "columns": [
        {{ "name": "id", "ty": "Int64", "nullable": false, "primary_key": true, "unique": false, "default": null, "references": null }},
        {{ "name": "title", "ty": "Text", "nullable": false, "primary_key": false, "unique": false, "default": null, "references": null }},
        {{ "name": "created_at", "ty": "Timestamp", "nullable": false, "primary_key": false, "unique": false, "default": "Now", "references": null }}
      ],
      "primary_key": ["id"],
      "indexes": [],
      "checks": [],
      "backend": "{backend}",
      "managed": true
    }}
  ]
}}
"#
        )
    }

    #[test]
    fn diff_backend_mismatch_errors_before_diff() {
        // A Sqlite-tagged snapshot diffed with `--backend pg` must fail with the
        // provider-lock guard BEFORE any parse/diff work.
        let root = scaffold_project(POST_MODEL, &posts_snapshot("Sqlite"));
        let err = diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            err.contains("does not match") && err.to_lowercase().contains("backend"),
            "backend mismatch surfaced: {err}"
        );
    }

    #[test]
    fn diff_missing_snapshot_is_friendly_error() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("Cargo.toml");
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(src.join("models.rs"), POST_MODEL).expect("models.rs");
        // No snapshot file written.
        let err = diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            err.contains("autumn schema snapshot"),
            "friendly 'run snapshot first' message: {err}"
        );
    }

    #[test]
    fn diff_no_op_writes_nothing() {
        // Models match the snapshot → no-op → no migration directory AND the
        // checked-in snapshot is left byte-for-byte unchanged (a no-op diff must
        // not advance the baseline).
        let root = scaffold_project(POST_MODEL, &posts_snapshot("Postgres"));
        let snapshot_path = root.path().join(".autumn/schema-snapshot.json");
        let before = std::fs::read_to_string(&snapshot_path).expect("snapshot before");
        diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            true,
            None,
            false,
        )
        .expect("no-op diff ok");
        assert!(
            !root.path().join("migrations").exists(),
            "a no-op must not create a migrations directory"
        );
        let after = std::fs::read_to_string(&snapshot_path).expect("snapshot after");
        assert_eq!(
            before, after,
            "a no-op diff must leave the snapshot byte-for-byte unchanged"
        );
    }

    #[test]
    fn write_migration_creates_up_and_down() {
        // Add a `body` column to the model that the snapshot lacks → one AddColumn.
        let models = r#"
            #[autumn_web::model(managed)]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub body: Option<String>,
            }
        "#;
        let root = scaffold_project(models, &posts_snapshot("Postgres"));
        diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            true,
            Some("add_body"),
            false,
        )
        .expect("write migration ok");

        // Exactly one migration directory named `<ts>_add_body` with the pair.
        let migrations = root.path().join("migrations");
        let entries: Vec<_> = std::fs::read_dir(&migrations)
            .expect("read migrations dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "one migration dir: {entries:?}");
        assert!(
            entries[0].ends_with("_add_body"),
            "named suffix: {entries:?}"
        );
        let dir = migrations.join(&entries[0]);
        let up = std::fs::read_to_string(dir.join("up.sql")).expect("up.sql");
        let down = std::fs::read_to_string(dir.join("down.sql")).expect("down.sql");
        assert!(
            up.contains("ALTER TABLE posts ADD COLUMN body TEXT NULL;"),
            "up: {up}"
        );
        assert!(
            down.contains("ALTER TABLE posts DROP COLUMN body;"),
            "down: {down}"
        );

        // The checked-in snapshot ADVANCED to the plan's target state: it now
        // carries the `body` column the migration adds. Load it back and check.
        let snapshot_path = root.path().join(".autumn/schema-snapshot.json");
        let advanced = snapshot::load_snapshot(&snapshot_path).expect("load advanced snapshot");
        let posts = advanced
            .tables
            .iter()
            .find(|t| t.name == "posts")
            .expect("posts table in advanced snapshot");
        assert!(
            posts.columns.iter().any(|c| c.name == "body"),
            "advanced snapshot must contain the new `body` column: {:?}",
            posts.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    // The migration files and the checked-in snapshot must advance together
    // (Codex P1): if the snapshot write fails after `write_migration_dir` has
    // already left a complete migration on disk, the baseline never advances, so
    // retrying `schema diff --write-migration` regenerates the same SQL as a
    // SECOND migration and applying both fails on duplicate DDL. `diff_at` rolls
    // the migration dir back on a snapshot-write failure. Reproduce that failure
    // portably by making the snapshot's parent dir (`.autumn`) read-only so the
    // earlier snapshot LOAD still succeeds but `write_snapshot`'s tempfile
    // create+rename cannot, then assert the command errors and left no migration.
    #[cfg(unix)]
    #[test]
    fn write_migration_rolls_back_on_snapshot_write_failure() {
        use std::os::unix::fs::PermissionsExt;

        // Add a `body` column the snapshot lacks → a non-empty plan that would
        // write a migration and then advance the snapshot.
        let models = r#"
            #[autumn_web::model(managed)]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub body: Option<String>,
            }
        "#;
        let root = scaffold_project(models, &posts_snapshot("Postgres"));
        let autumn_dir = root.path().join(".autumn");

        // Make the snapshot's parent directory read-only so a new tempfile cannot
        // be created in it (the earlier read of the existing snapshot still works).
        std::fs::set_permissions(&autumn_dir, std::fs::Permissions::from_mode(0o555))
            .expect("chmod .autumn read-only");

        // Skipped when the process can bypass the permission bits (e.g. running as
        // root, where a read-only dir is still writable), which would otherwise
        // make this non-deterministic — mirrors `classify_unreadable_dir_is_an_error`.
        let can_still_write = tempfile::NamedTempFile::new_in(&autumn_dir).is_ok();
        if can_still_write {
            std::fs::set_permissions(&autumn_dir, std::fs::Permissions::from_mode(0o755))
                .expect("restore chmod");
            return;
        }

        let result = diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            true,
            Some("add_body"),
            false,
        );

        // Restore permissions so the tempdir can be cleaned up (and the migrations
        // assertion below can read the dir regardless of outcome).
        std::fs::set_permissions(&autumn_dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore chmod");

        // (a) The command surfaced the snapshot-write failure as an error.
        assert!(
            result.is_err(),
            "a snapshot-write failure must surface as an error, got: {result:?}"
        );

        // (b) The just-created migration was rolled back: no migration directory
        // is left behind, so a retry regenerates a single migration (not a second).
        let migrations = root.path().join("migrations");
        let leftover: Vec<_> = std::fs::read_dir(&migrations)
            .map(|rd| {
                rd.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "the migration dir must be rolled back on a snapshot-write failure, found: {leftover:?}"
        );
    }

    /// The core drift scenario from #2041: `schema diff --write-migration`
    /// advances the snapshot to the GENERATED plan's target, so a later,
    /// still-ungenerated model edit diffs on top of it (only the *new* delta),
    /// never re-generating the already-generated change.
    #[test]
    #[allow(clippy::too_many_lines)] // linear scenario reads clearest end-to-end
    fn write_migration_advances_snapshot_so_later_edits_diff_on_top() {
        // Change A: add `body`. Baseline is the posts(id,title,created_at) snapshot.
        let models_a = r#"
            #[autumn_web::model(managed)]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub body: Option<String>,
            }
        "#;
        let root = scaffold_project(models_a, &posts_snapshot("Postgres"));
        let snapshot_path = root.path().join(".autumn/schema-snapshot.json");

        // Generate change A. The snapshot advances to include `body`.
        diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            true,
            Some("add_body"),
            false,
        )
        .expect("write migration A ok");

        let after_a = snapshot::load_snapshot(&snapshot_path).expect("load snapshot after A");
        let posts_a = after_a
            .tables
            .iter()
            .find(|t| t.name == "posts")
            .expect("posts after A");
        assert!(
            posts_a.columns.iter().any(|c| c.name == "body"),
            "snapshot advanced to include `body`"
        );
        // The not-yet-added `draft` column is NOT in the snapshot yet.
        assert!(
            !posts_a.columns.iter().any(|c| c.name == "draft"),
            "snapshot must not contain the not-yet-added `draft` column"
        );

        // Convergence: re-running with the SAME models is now a no-op — no second
        // migration directory is written (the snapshot already matches).
        diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            true,
            Some("add_body_again"),
            false,
        )
        .expect("second (converged) diff ok");
        let dir_count = std::fs::read_dir(root.path().join("migrations"))
            .expect("read migrations")
            .count();
        assert_eq!(
            dir_count, 1,
            "a converged re-run must not write a second migration"
        );

        // Now edit the models to add `draft` on top of the already-generated
        // `body`. Because the snapshot advanced to change A, the next diff
        // generates ONLY `draft` — proving the baseline moved to the plan target,
        // not that it stayed at the original baseline.
        std::fs::write(
            root.path().join("src").join("models.rs"),
            r#"
                #[autumn_web::model(managed)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    pub body: Option<String>,
                    pub draft: Option<String>,
                }
            "#,
        )
        .expect("edit models to add draft");

        diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            true,
            Some("add_draft"),
            false,
        )
        .expect("write migration B (draft) ok");

        // Exactly two migration dirs; the newest carries `draft` but NOT `body`.
        let mut names: Vec<String> = std::fs::read_dir(root.path().join("migrations"))
            .expect("read migrations")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "two migration dirs now: {names:?}");
        let draft_dir = names
            .iter()
            .find(|n| n.ends_with("_add_draft"))
            .expect("add_draft dir");
        let up = std::fs::read_to_string(
            root.path()
                .join("migrations")
                .join(draft_dir)
                .join("up.sql"),
        )
        .expect("draft up.sql");
        assert!(
            up.contains("draft"),
            "the second migration still generates `draft`: {up}"
        );
        assert!(
            !up.contains("body"),
            "the second migration must NOT re-generate the already-added `body`: {up}"
        );
    }

    /// Round 3 / Finding X: a second `--write-migration` resolving to an
    /// already-existing `migrations/<ts>_<suffix>/` directory (two runs in the
    /// same 1-second-resolution timestamp) is REFUSED rather than silently
    /// overwriting the first migration's SQL. Drives `write_migration_dir`
    /// directly with a fixed timestamp so the collision is deterministic.
    #[test]
    fn write_migration_refuses_to_overwrite_existing_dir() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = write_migration_dir(
            root.path(),
            "20260101120000",
            "add_body",
            "UP-ONE",
            "DOWN-ONE",
        )
        .expect("first write ok");
        assert_eq!(
            std::fs::read_to_string(dir.join("up.sql")).expect("up.sql"),
            "UP-ONE"
        );

        // A second write to the same <ts>_<suffix> is refused with a clear error.
        let err = write_migration_dir(
            root.path(),
            "20260101120000",
            "add_body",
            "UP-TWO",
            "DOWN-TWO",
        )
        .expect_err("second write to the same dir must be refused");
        assert!(
            err.contains("refusing to overwrite") && err.contains("add_body"),
            "clear refusal naming the colliding dir: {err}"
        );

        // The first migration's contents are left intact (not clobbered).
        assert_eq!(
            std::fs::read_to_string(dir.join("up.sql")).expect("up.sql"),
            "UP-ONE",
            "first up.sql untouched"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("down.sql")).expect("down.sql"),
            "DOWN-ONE",
            "first down.sql untouched"
        );
    }

    /// Round 9 / P2: if the `up.sql` write succeeds but the `down.sql` write
    /// fails (disk-full / I/O error), the just-created leaf directory must be
    /// removed so no partial migration is left behind — otherwise a retry with
    /// the same timestamp+name would hit the exclusive-create collision guard
    /// and be wrongly refused. Injects a `write` closure that fails only on
    /// `down.sql` to force the failure deterministically, then asserts the dir
    /// is gone and a clean retry succeeds.
    #[test]
    fn write_migration_cleans_up_partial_dir_on_write_failure() {
        let root = tempfile::tempdir().expect("tempdir");
        let leaf = root
            .path()
            .join("migrations")
            .join("20260101120000_add_body");

        let err = write_migration_dir_with(
            root.path(),
            "20260101120000",
            "add_body",
            "UP",
            "DOWN",
            |path, contents| {
                if path.file_name().and_then(|n| n.to_str()) == Some("down.sql") {
                    Err(std::io::Error::other("simulated disk full"))
                } else {
                    std::fs::write(path, contents)
                }
            },
        )
        .expect_err("down.sql write failure must propagate");
        assert!(
            err.contains("down.sql") && err.contains("simulated disk full"),
            "original write error is surfaced: {err}"
        );

        // The partially-written dir (which had only up.sql) was cleaned up.
        assert!(
            !leaf.exists(),
            "partial migration dir must be removed on write failure"
        );

        // A retry with the SAME timestamp+name now succeeds: the collision guard
        // sees no leftover dir, so the user is not stuck needing manual cleanup.
        let dir = write_migration_dir(root.path(), "20260101120000", "add_body", "UP", "DOWN")
            .expect("retry after cleanup succeeds");
        assert_eq!(
            std::fs::read_to_string(dir.join("up.sql")).expect("up.sql"),
            "UP"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("down.sql")).expect("down.sql"),
            "DOWN"
        );
    }

    #[test]
    fn diff_destructive_refused_without_flag_then_allowed() {
        // Snapshot has a `title` the model dropped → DropColumn → refused by default.
        let models = r#"
            #[autumn_web::model(managed)]
            pub struct Post {
                #[id]
                pub id: i64,
            }
        "#;
        let root = scaffold_project(models, &posts_snapshot("Postgres"));
        let err = diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            err.contains("--allow-destructive"),
            "refusal names the flag: {err}"
        );

        // With the flag it succeeds (prints the plan).
        diff_at(
            root.path(),
            None,
            None,
            Some(BackendArg::Pg),
            false,
            None,
            true,
        )
        .expect("allowed with --allow-destructive");
    }

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

    /// Findings 2 & 3 (#2036): the schema-command backend derives from the
    /// profile-resolved URL when one is configured (its scheme is authoritative),
    /// and only falls back to the project default when no URL is present — so
    /// `--profile <name>` picks the apply path / provider-lock of the database it
    /// acts against, not the ambient project default.
    #[test]
    fn backend_for_url_prefers_url_scheme_over_project_default() {
        // A tempdir with no config → the project default is Postgres.
        let root = tempfile::tempdir().expect("tempdir");
        // A configured SQLite URL is authoritative regardless of that default.
        assert_eq!(
            backend_for_url(root.path(), None, Some("sqlite://app.db")),
            Backend::Sqlite
        );
        // A configured Postgres URL likewise.
        assert_eq!(
            backend_for_url(root.path(), None, Some("postgres://u@h/db")),
            Backend::Postgres
        );
        // No URL → falls back to the profile-aware project default (Postgres here).
        assert_eq!(backend_for_url(root.path(), None, None), Backend::Postgres);
    }

    #[test]
    fn existing_models_path_prefers_dir_then_file_then_none() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(existing_models_path(root.path()).is_none());

        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");
        let file = src.join("models.rs");
        std::fs::write(&file, "").expect("write file");
        assert_eq!(existing_models_path(root.path()), Some(file));

        let dir = src.join("models");
        std::fs::create_dir_all(&dir).expect("mkdir models");
        assert_eq!(existing_models_path(root.path()), Some(dir));
    }

    #[test]
    fn load_existing_snapshot_absent_and_corrupt_are_none_valid_is_some() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("snap.json");
        // Missing file → treated as absent (fresh baseline).
        assert!(load_existing_snapshot(&path).is_none());
        // Corrupt/unreadable-as-absent → also None (a pull re-establishes it).
        std::fs::write(&path, "{ not json").expect("write corrupt");
        assert!(load_existing_snapshot(&path).is_none());
        // A valid snapshot loads and surfaces its backend to the provider-lock.
        let snap = SchemaSnapshot::new(Backend::Sqlite, Vec::new());
        snapshot::write_snapshot(&path, &snap).expect("write valid");
        let loaded = load_existing_snapshot(&path).expect("some");
        assert_eq!(loaded.backend, Backend::Sqlite);
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

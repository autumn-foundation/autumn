//! `autumn schema doctor` — read-only diagnosis of the declarative-schema state
//! (slice 6 of tracking issue #1975).
//!
//! Doctor answers "is my declarative-schema setup healthy?" without mutating
//! anything: it reports a table of named checks — filesystem, snapshot, model
//! drift, backend provider-lock, and pending migrations — each with an
//! `OK`/`WARN`/`ERROR` status and a one-line detail. It exits non-zero when any
//! check is `ERROR` (an actionable misconfiguration); a `WARN` alone (e.g.
//! uncommitted model changes, or an unreachable database) never fails the
//! command, so doctor stays runnable offline.
//!
//! # Testability
//!
//! The check computation is a pure function ([`compute_checks`]) over gathered
//! facts ([`LocalFacts`] + a [`PendingState`]); all filesystem/DB/env I/O lives
//! in the thin shell ([`run_doctor`] / [`gather_local_facts`]). Unit tests
//! exercise the pure layer directly with no database.
//!
//! # Backend note
//!
//! The pending-migrations check is Postgres-specific (it consumes the pg
//! `pending_migrations` status API). A non-Postgres backend is reported as a
//! skipped WARN rather than failing — doctor references no `SQLite` symbol and so
//! compiles identically with or without the `sqlite` feature.

use std::path::Path;

use autumn_schema_core::{Backend, Table};
use diesel_migrations::FileBasedMigrations;
use serde::Serialize;

use super::diff::{DiffOptions, diff_schema};
use super::parse::parse_models_path;
use super::snapshot::{SNAPSHOT_DEFAULT_PATH, SnapshotError, load_snapshot};

/// A single diagnostic check outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Stable check identifier (e.g. `project-root`, `snapshot-drift`).
    pub name: String,
    /// The severity of the outcome.
    pub status: Status,
    /// A one-line, human-readable detail (remediation when actionable).
    pub detail: String,
}

/// Check severity. Serializes as the upper-case token used in the report and the
/// `--json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    /// The check passed.
    Ok,
    /// A heads-up that does not fail the command (drift, unreachable DB, skips).
    Warn,
    /// An actionable problem — the command exits non-zero.
    Error,
}

impl Status {
    /// The bracketed label used in the plain-text report.
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "[OK]   ",
            Self::Warn => "[WARN] ",
            Self::Error => "[ERROR]",
        }
    }
}

/// Snapshot load outcome plus, when loaded, its dialect tag and the model-drift
/// result computed against it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotState {
    /// Loaded and parsed; carries its backend tag and the drift verdict.
    Loaded { backend: Backend, drift: DriftState },
    /// No snapshot file at the default path.
    Missing,
    /// The file exists but could not be read/parsed (IO, JSON, or bad version).
    Unreadable(String),
}

/// Model-vs-snapshot drift verdict (only meaningful when the snapshot loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
enum DriftState {
    /// The declared models match the snapshot baseline.
    Clean,
    /// The models diverge from the baseline by this many changes.
    Changed(usize),
    /// The declared models could not be parsed to compute drift.
    ModelsError(String),
}

/// Pending-migrations probe outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingState {
    /// No database URL is configured — the check is skipped.
    NotConfigured,
    /// The backend/build cannot run this check (e.g. non-Postgres backend).
    Skipped(String),
    /// The database was reached; carries the pending migration names.
    Reachable(Vec<String>),
    /// The database could not be reached (secret-free reason).
    Unreachable(String),
}

/// Filesystem/snapshot/backend facts gathered without touching the database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFacts {
    cargo_toml_present: bool,
    autumn_dir_present: bool,
    snapshot: SnapshotState,
    detected_backend: Backend,
    /// Backend implied by the resolved DB URL scheme (`None` if no URL / unknown).
    url_backend: Option<Backend>,
}

/// Run the doctor: gather facts, compute checks, print the report (table or
/// JSON), and return `Err(summary)` when any check is `ERROR` so the dispatch
/// tail exits non-zero.
///
/// # Errors
///
/// Returns a one-line summary when one or more checks are `ERROR`.
pub fn run_doctor(profile: Option<&str>, json: bool) -> Result<(), String> {
    let project_root = std::env::current_dir()
        .map_err(|e| format!("failed to resolve the current directory: {e}"))?;
    let local = gather_local_facts(&project_root, profile);
    let pending = gather_pending(&project_root, profile, &local);
    let checks = compute_checks(&local, &pending);
    render(&checks, json)?;
    finish(&checks)
}

/// Gather the non-DB facts (filesystem, snapshot, drift, backends).
fn gather_local_facts(project_root: &Path, profile: Option<&str>) -> LocalFacts {
    let cargo_toml_present = project_root.join("Cargo.toml").is_file();
    let autumn_dir_present = project_root.join(".autumn").is_dir();

    // Resolve the URL once for the requested `--profile`; both the URL-implied
    // backend (dialect-vs-DB check) and the project/selected backend (provider-
    // lock + pending-path selection) derive from this SAME profile-resolved
    // context, so `schema doctor --profile <name>` diagnoses the profile it
    // claims to (rather than mixing the requested profile's URL with the ambient
    // profile's detected backend).
    let url = crate::migrate::resolve_primary_url(profile);
    let url_backend = url
        .as_deref()
        .and_then(autumn_web::config::DatabaseBackend::detect)
        .map(super::map_detected_backend);
    let detected_backend = super::backend_for_url(project_root, profile, url.as_deref());
    let snapshot = probe_snapshot(project_root, detected_backend);

    LocalFacts {
        cargo_toml_present,
        autumn_dir_present,
        snapshot,
        detected_backend,
        url_backend,
    }
}

/// Load the snapshot and, if it loads, compute model drift against it using the
/// snapshot's own dialect tag.
fn probe_snapshot(project_root: &Path, _detected: Backend) -> SnapshotState {
    let snapshot_path = project_root.join(SNAPSHOT_DEFAULT_PATH);
    match load_snapshot(&snapshot_path) {
        Ok(snapshot) => {
            let drift = compute_drift(project_root, snapshot.backend, &snapshot.tables);
            SnapshotState::Loaded {
                backend: snapshot.backend,
                drift,
            }
        }
        Err(SnapshotError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            SnapshotState::Missing
        }
        Err(other) => SnapshotState::Unreadable(other.to_string()),
    }
}

/// Diff the declared models (parsed with the snapshot's backend) against the
/// baseline tables to decide the drift verdict.
fn compute_drift(project_root: &Path, backend: Backend, baseline: &[Table]) -> DriftState {
    let Some(models_path) = super::existing_models_path(project_root) else {
        return DriftState::ModelsError(
            "no declarative models found under src/ to compare against the snapshot".to_string(),
        );
    };
    match parse_models_path(&models_path, backend) {
        Ok(desired) => {
            let plan = diff_schema(baseline, &desired, DiffOptions::default());
            if plan.is_empty() {
                DriftState::Clean
            } else {
                DriftState::Changed(plan.changes.len())
            }
        }
        Err(e) => DriftState::ModelsError(e.to_string()),
    }
}

/// Probe pending migrations for the detected backend. Postgres consults the live
/// `pending_migrations` status API; any other backend (and a build without the
/// database URL) is skipped with a note. Never references a `SQLite` symbol.
fn gather_pending(project_root: &Path, profile: Option<&str>, local: &LocalFacts) -> PendingState {
    let Some(url) = crate::migrate::resolve_primary_url(profile) else {
        return PendingState::NotConfigured;
    };
    if local.detected_backend != Backend::Postgres {
        return PendingState::Skipped(format!(
            "pending-migration check is Postgres-only; detected {:?} backend",
            local.detected_backend
        ));
    }
    probe_pending(&url, &project_root.join("migrations"))
}

/// Probe pending migrations against `url` using the project's `migrations_dir`.
///
/// A present, readable dir is the migration source. When it is missing/unreadable
/// we must NOT short-circuit to "up to date" — that would let a missing migrations
/// directory mask an offline or misconfigured database (a false OK). Instead we
/// probe connectivity with a freshly-created, guaranteed-empty temp directory:
/// with zero available migrations `pending_migrations` still connects, returning
/// an empty set on a reachable DB (correctly "up to date") and an error on an
/// unreachable one (surfaced as [`PendingState::Unreachable`], i.e. a WARN).
fn probe_pending(url: &str, migrations_dir: &Path) -> PendingState {
    // Deferred-init tempdir guard: bound in this scope so, when used, it outlives
    // the `pending_migrations` call below.
    let fallback_dir;
    let migrations = if let Ok(source) = FileBasedMigrations::from_path(migrations_dir) {
        source
    } else {
        fallback_dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(e) => {
                return PendingState::Unreachable(format!(
                    "could not create a temp directory to probe the database: {e}"
                ));
            }
        };
        match FileBasedMigrations::from_path(fallback_dir.path()) {
            Ok(source) => source,
            Err(e) => {
                return PendingState::Unreachable(format!(
                    "could not build an empty migration source to probe the database: {e}"
                ));
            }
        }
    };
    match autumn_web::migrate::pending_migrations(url, migrations) {
        Ok(names) => PendingState::Reachable(names),
        // MigrationError Display is secret-free.
        Err(e) => PendingState::Unreachable(e.to_string()),
    }
}

/// Pure check computation over gathered facts. Ordered filesystem → snapshot →
/// drift → backend (provider-lock, then dialect-vs-DB) → database.
fn compute_checks(local: &LocalFacts, pending: &PendingState) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. project-root
    checks.push(if !local.cargo_toml_present {
        Check {
            name: "project-root".to_string(),
            status: Status::Error,
            detail: "not in an Autumn project (no Cargo.toml) — run from your project root"
                .to_string(),
        }
    } else if !local.autumn_dir_present {
        Check {
            name: "project-root".to_string(),
            status: Status::Error,
            detail: "missing .autumn/ state directory — run `autumn schema snapshot` first"
                .to_string(),
        }
    } else {
        Check {
            name: "project-root".to_string(),
            status: Status::Ok,
            detail: "Cargo.toml and .autumn/ present".to_string(),
        }
    });

    // 2. snapshot-present
    checks.push(match &local.snapshot {
        SnapshotState::Loaded { backend, .. } => Check {
            name: "snapshot-present".to_string(),
            status: Status::Ok,
            detail: format!("snapshot loaded (backend {backend:?})"),
        },
        SnapshotState::Missing => Check {
            name: "snapshot-present".to_string(),
            status: Status::Error,
            detail: format!(
                "no snapshot at {SNAPSHOT_DEFAULT_PATH} — run `autumn schema snapshot`"
            ),
        },
        SnapshotState::Unreadable(reason) => Check {
            name: "snapshot-present".to_string(),
            status: Status::Error,
            detail: format!("snapshot is unreadable: {reason}"),
        },
    });

    // 3. snapshot-drift
    checks.push(drift_check(&local.snapshot));

    // 4. provider-lock (snapshot backend vs detected backend)
    checks.push(provider_lock_check(&local.snapshot, local.detected_backend));

    // 5. snapshot-dialect vs DB (snapshot backend vs resolved-URL backend)
    checks.push(dialect_vs_db_check(&local.snapshot, local.url_backend));

    // 6. pending-migrations
    checks.push(pending_check(pending));

    checks
}

/// The `snapshot-drift` row.
fn drift_check(snapshot: &SnapshotState) -> Check {
    let name = "snapshot-drift".to_string();
    match snapshot {
        SnapshotState::Loaded {
            drift: DriftState::Clean,
            ..
        } => Check {
            name,
            status: Status::Ok,
            detail: "models match the snapshot baseline".to_string(),
        },
        SnapshotState::Loaded {
            drift: DriftState::Changed(n),
            ..
        } => Check {
            name,
            status: Status::Warn,
            detail: format!(
                "{n} pending model change(s) — run `autumn schema diff --write-migration`"
            ),
        },
        SnapshotState::Loaded {
            drift: DriftState::ModelsError(reason),
            ..
        } => Check {
            name,
            status: Status::Warn,
            detail: format!("could not evaluate drift: {reason}"),
        },
        SnapshotState::Missing | SnapshotState::Unreadable(_) => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot baseline".to_string(),
        },
    }
}

/// The `provider-lock` row (snapshot dialect vs the detected backend).
fn provider_lock_check(snapshot: &SnapshotState, detected: Backend) -> Check {
    let name = "provider-lock".to_string();
    match snapshot {
        SnapshotState::Loaded { backend, .. } if *backend == detected => Check {
            name,
            status: Status::Ok,
            detail: format!("snapshot and detected backend agree ({detected:?})"),
        },
        SnapshotState::Loaded { backend, .. } => Check {
            name,
            status: Status::Error,
            detail: format!(
                "snapshot backend {backend:?} does not match the detected backend {detected:?}"
            ),
        },
        SnapshotState::Missing | SnapshotState::Unreadable(_) => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot".to_string(),
        },
    }
}

/// The `snapshot-dialect-vs-db` row (snapshot dialect vs the resolved DB URL's
/// backend). Distinct from provider-lock: the configured backend and the URL
/// scheme can diverge.
fn dialect_vs_db_check(snapshot: &SnapshotState, url_backend: Option<Backend>) -> Check {
    let name = "snapshot-dialect-vs-db".to_string();
    match (snapshot, url_backend) {
        (SnapshotState::Missing | SnapshotState::Unreadable(_), _) => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot".to_string(),
        },
        (SnapshotState::Loaded { .. }, None) => Check {
            name,
            status: Status::Warn,
            detail: "no database URL configured — skipped".to_string(),
        },
        (SnapshotState::Loaded { backend, .. }, Some(url_backend)) if *backend == url_backend => {
            Check {
                name,
                status: Status::Ok,
                detail: format!("snapshot dialect matches the database URL ({url_backend:?})"),
            }
        }
        (SnapshotState::Loaded { backend, .. }, Some(url_backend)) => Check {
            name,
            status: Status::Error,
            detail: format!(
                "snapshot backend {backend:?} does not match the database URL backend {url_backend:?}"
            ),
        },
    }
}

/// The `pending-migrations` row.
fn pending_check(pending: &PendingState) -> Check {
    let name = "pending-migrations".to_string();
    match pending {
        PendingState::NotConfigured => Check {
            name,
            status: Status::Warn,
            detail: "no database configured — skipped".to_string(),
        },
        PendingState::Skipped(reason) => Check {
            name,
            status: Status::Warn,
            detail: reason.clone(),
        },
        PendingState::Unreachable(reason) => Check {
            name,
            status: Status::Warn,
            detail: format!("could not reach database: {reason}"),
        },
        PendingState::Reachable(names) if names.is_empty() => Check {
            name,
            status: Status::Ok,
            detail: "database is up to date".to_string(),
        },
        PendingState::Reachable(names) => Check {
            name,
            status: Status::Warn,
            detail: format!(
                "{} pending migration(s): {} — run `autumn schema migrate`",
                names.len(),
                names.join(", ")
            ),
        },
    }
}

/// Print the report as an aligned table, or as JSON when `json` is set.
fn render(checks: &[Check], json: bool) -> Result<(), String> {
    if json {
        let out = serde_json::to_string_pretty(checks)
            .map_err(|e| format!("failed to serialize doctor report: {e}"))?;
        println!("{out}");
        return Ok(());
    }
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    println!("Schema doctor:");
    for check in checks {
        println!(
            "  {} {:<width$}  {}",
            check.status.label(),
            check.name,
            check.detail,
            width = width
        );
    }
    Ok(())
}

/// Turn an `ERROR`-containing report into a non-zero exit via `Err`, else `Ok`.
fn finish(checks: &[Check]) -> Result<(), String> {
    let errors: Vec<&str> = checks
        .iter()
        .filter(|c| c.status == Status::Error)
        .map(|c| c.name.as_str())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "schema doctor found {} error check(s): {}",
            errors.len(),
            errors.join(", ")
        ))
    }
}

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;

    const POST_MODEL: &str = r#"
        #[autumn_web::model(managed)]
        pub struct Post {
            #[id]
            pub id: i64,
            pub title: String,
        }
    "#;

    /// A version-1 snapshot for a `posts(id, title, created_at)` table, matching
    /// `POST_MODEL` (the parser adds the implicit `created_at`).
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

    /// Write a project scaffold: `Cargo.toml`, `src/models.rs`, and a snapshot at
    /// the default path. Returns the tempdir (kept alive by the caller).
    fn scaffold(models: &str, snapshot_json: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("Cargo.toml");
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");
        std::fs::write(src.join("models.rs"), models).expect("models.rs");
        let autumn = root.path().join(".autumn");
        std::fs::create_dir_all(&autumn).expect("mkdir .autumn");
        if let Some(json) = snapshot_json {
            std::fs::write(autumn.join("schema-snapshot.json"), json).expect("snapshot");
        }
        root
    }

    #[test]
    fn missing_project_root_is_error() {
        let root = tempfile::tempdir().expect("tempdir"); // no Cargo.toml
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let pr = checks.iter().find(|c| c.name == "project-root").unwrap();
        assert_eq!(pr.status, Status::Error, "{pr:?}");
        // finish() turns it into a non-zero exit.
        assert!(finish(&checks).is_err());
    }

    #[test]
    fn matching_models_report_drift_ok() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let drift = checks.iter().find(|c| c.name == "snapshot-drift").unwrap();
        assert_eq!(drift.status, Status::Ok, "{drift:?}");
        // No ERROR checks (detected Postgres matches the Postgres snapshot).
        assert!(finish(&checks).is_ok(), "checks: {checks:?}");
    }

    #[test]
    fn diverging_models_report_drift_warn() {
        // Model adds a `body` column the snapshot lacks → drift WARN.
        let models = r#"
            #[autumn_web::model(managed)]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub body: Option<String>,
            }
        "#;
        let root = scaffold(models, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let drift = checks.iter().find(|c| c.name == "snapshot-drift").unwrap();
        assert_eq!(drift.status, Status::Warn, "{drift:?}");
        assert!(drift.detail.contains("pending model change"), "{drift:?}");
        // A WARN alone does not fail the command.
        assert!(finish(&checks).is_ok());
    }

    /// Finding 3 (#2036): the SELECTED profile's backend must flow into the
    /// checks. When `gather_local_facts` resolves the requested profile's backend
    /// to Sqlite (from its database URL), a Sqlite-tagged snapshot must NOT trip a
    /// false `provider-lock` ERROR — even though the ambient project default is
    /// Postgres (see the contrasting `snapshot_backend_mismatch_is_provider_lock_error`).
    /// Synthesizes the post-resolution facts directly so the pure check layer is
    /// exercised without a database.
    #[test]
    fn selected_profile_backend_flows_into_provider_lock() {
        let local = LocalFacts {
            cargo_toml_present: true,
            autumn_dir_present: true,
            snapshot: SnapshotState::Loaded {
                backend: Backend::Sqlite,
                drift: DriftState::Clean,
            },
            detected_backend: Backend::Sqlite,
            url_backend: Some(Backend::Sqlite),
        };
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let lock = checks.iter().find(|c| c.name == "provider-lock").unwrap();
        assert_eq!(lock.status, Status::Ok, "{lock:?}");
        // No ERROR overall: the selected Sqlite backend agrees with the snapshot.
        assert!(finish(&checks).is_ok(), "checks: {checks:?}");
    }

    #[test]
    fn snapshot_backend_mismatch_is_provider_lock_error() {
        // Sqlite-tagged snapshot but the tempdir project detects Postgres.
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Sqlite")));
        let local = gather_local_facts(root.path(), None);
        assert_eq!(local.detected_backend, Backend::Postgres);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let lock = checks.iter().find(|c| c.name == "provider-lock").unwrap();
        assert_eq!(lock.status, Status::Error, "{lock:?}");
        assert!(finish(&checks).is_err());
    }

    #[test]
    fn missing_snapshot_is_snapshot_present_error() {
        let root = scaffold(POST_MODEL, None); // .autumn present, no snapshot file
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let snap = checks
            .iter()
            .find(|c| c.name == "snapshot-present")
            .unwrap();
        assert_eq!(snap.status, Status::Error, "{snap:?}");
        // Drift/provider-lock degrade to WARN skips when there's no snapshot.
        let drift = checks.iter().find(|c| c.name == "snapshot-drift").unwrap();
        assert_eq!(drift.status, Status::Warn);
        assert!(finish(&checks).is_err());
    }

    #[test]
    fn unreadable_snapshot_is_snapshot_present_error() {
        let root = scaffold(POST_MODEL, Some("{ not valid json"));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let snap = checks
            .iter()
            .find(|c| c.name == "snapshot-present")
            .unwrap();
        assert_eq!(snap.status, Status::Error, "{snap:?}");
    }

    /// Finding 1 (connectivity false-positive): a MISSING migrations dir must not
    /// short-circuit `probe_pending` to `Reachable(empty)` — with a configured URL
    /// the probe must still attempt a connection, so an offline/misconfigured
    /// database surfaces as `Unreachable` (a WARN) rather than a false "database is
    /// up to date". Uses a loopback port with nothing listening (immediate
    /// connection-refused — fast and deterministic, no network egress). Gated to
    /// the default Postgres lane since the pending probe is Postgres-only.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn missing_migrations_dir_still_probes_the_database() {
        let root = tempfile::tempdir().expect("tempdir"); // no migrations/ dir
        let state = probe_pending(
            "postgres://user:pw@127.0.0.1:1/db",
            &root.path().join("migrations"),
        );
        assert!(
            matches!(state, PendingState::Unreachable(_)),
            "missing dir + unreachable DB must be Unreachable, got {state:?}"
        );
    }

    #[test]
    fn pending_reachable_empty_is_ok_and_nonempty_is_warn() {
        let clean = pending_check(&PendingState::Reachable(Vec::new()));
        assert_eq!(clean.status, Status::Ok);
        let dirty = pending_check(&PendingState::Reachable(vec!["20260101_init".to_string()]));
        assert_eq!(dirty.status, Status::Warn);
        assert!(dirty.detail.contains("autumn schema migrate"));
        // Unreachable is a WARN, never a hard fail (doctor runs offline).
        let down = pending_check(&PendingState::Unreachable("connection refused".to_string()));
        assert_eq!(down.status, Status::Warn);
    }

    #[test]
    fn dialect_vs_db_mismatch_is_error() {
        let snapshot = SnapshotState::Loaded {
            backend: Backend::Postgres,
            drift: DriftState::Clean,
        };
        let check = dialect_vs_db_check(&snapshot, Some(Backend::Sqlite));
        assert_eq!(check.status, Status::Error, "{check:?}");
        // Matching URL backend is OK.
        let ok = dialect_vs_db_check(&snapshot, Some(Backend::Postgres));
        assert_eq!(ok.status, Status::Ok);
        // No URL → skipped WARN.
        let skip = dialect_vs_db_check(&snapshot, None);
        assert_eq!(skip.status, Status::Warn);
    }

    #[test]
    fn status_serializes_uppercase_in_json() {
        let checks = vec![Check {
            name: "x".to_string(),
            status: Status::Warn,
            detail: "y".to_string(),
        }];
        let json = serde_json::to_string(&checks).unwrap();
        assert!(json.contains("\"status\":\"WARN\""), "{json}");
        assert!(json.contains("\"name\":\"x\""));
    }

    #[test]
    fn report_ordering_is_filesystem_then_snapshot_then_db() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(&local, &PendingState::NotConfigured);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "project-root",
                "snapshot-present",
                "snapshot-drift",
                "provider-lock",
                "snapshot-dialect-vs-db",
                "pending-migrations",
            ]
        );
    }
}

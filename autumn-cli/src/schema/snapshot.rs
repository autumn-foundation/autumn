//! The checked-in schema **snapshot** artifact and its canonical save/load — the
//! offline, VCS-reviewable *diff baseline* the declarative-schema wave builds on
//! (slice 3 of tracking issue #1975).
//!
//! Slice 2 gave us a reader of the **desired state** ([`crate::schema::parse`],
//! which lifts `#[model]` structs into the [`autumn_schema_core`] IR). This slice
//! adds the other half of a diff: a durable, versioned, dialect-tagged file that
//! records the schema's *last-known agreed shape*. At adoption time the declared
//! models and the live database agree, so writing a snapshot of the models
//! establishes the initial baseline; a later slice's diff engine compares the
//! freshly-parsed desired state against this committed file to compute the
//! pending migration.
//!
//! # Why a bespoke envelope (not just `Vec<Table>`)
//!
//! A raw `Vec<Table>` round-trips through serde, but a checked-in artifact needs
//! two extra guarantees the bare IR does not carry at the top level:
//!
//! - **A `snapshot_version`** ([`SNAPSHOT_VERSION`]) so a future format change is
//!   detected loudly ([`SnapshotError::UnsupportedVersion`]) instead of being
//!   silently mis-parsed by an older CLI.
//! - **A top-level `backend`** dialect tag (Decision 5 provider-lock): the same
//!   logical schema renders different DDL per backend, so a snapshot is locked to
//!   the provider it was generated against. [`SchemaSnapshot::ensure_backend_matches`]
//!   is the guard a later slice's diff engine calls before diffing.
//!
//! # Canonical serialization
//!
//! [`to_canonical_json`] is deterministic: tables are sorted by name and, within
//! a table, indexes and checks are sorted by name, so the same logical schema
//! always serializes to byte-identical JSON regardless of source-file order or
//! map iteration. Columns keep their declared struct order — that order is
//! meaningful (it is the `CREATE TABLE` column order) and readable in review.
//! Deterministic output is what makes the file a clean `git diff` target.

use std::path::Path;

use autumn_schema_core::{Backend, Schema, Table};
use serde::{Deserialize, Serialize};

/// The current on-disk snapshot format version. Bumped only by a
/// backwards-incompatible change to the envelope; [`load_snapshot`] rejects any
/// other value so an older CLI never silently mis-reads a newer file.
pub const SNAPSHOT_VERSION: u32 = 1;

/// The conventional, checked-in location of the schema snapshot, relative to the
/// project root. Lives under the `.autumn/` state directory (the same
/// dot-namespace the CLI already uses for generated state such as
/// `static/.autumn-assets.json` / `.autumn-manifest.json`) so the diff baseline
/// sits beside the project's other Autumn-owned artifacts and is easy to review
/// in a pull request. The `snapshot` command's `--out` flag overrides it.
pub const SNAPSHOT_DEFAULT_PATH: &str = ".autumn/schema-snapshot.json";

/// A durable, versioned, dialect-tagged snapshot of a schema's shape — the
/// checked-in diff baseline.
///
/// The field order (`snapshot_version`, `backend`, `tables`) is also the
/// serialized key order, chosen so the two most operationally important facts
/// (which format, which provider) lead the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// The on-disk format version (see [`SNAPSHOT_VERSION`]).
    pub snapshot_version: u32,
    /// The dialect this snapshot is locked to (its provider-lock; see [`Backend`]).
    pub backend: Backend,
    /// The tables, canonicalized on write (see [`to_canonical_json`]).
    pub tables: Vec<Table>,
}

impl SchemaSnapshot {
    /// Build a snapshot at the current [`SNAPSHOT_VERSION`] from `backend` and
    /// `tables`. The tables are stored as given; canonical ordering is applied at
    /// serialization time by [`to_canonical_json`], not here, so a caller's live
    /// in-memory ordering (e.g. declaration order) is preserved for any other use.
    #[must_use]
    pub const fn new(backend: Backend, tables: Vec<Table>) -> Self {
        Self {
            snapshot_version: SNAPSHOT_VERSION,
            backend,
            tables,
        }
    }

    /// Build a snapshot from an [`autumn_schema_core::Schema`], taking its
    /// backend tag and tables.
    // Part of the snapshot API surface; the model-derived `snapshot` command
    // builds from `Vec<Table>` directly, so this `Schema`-taking constructor is
    // exercised by tests / later slices rather than the current command path.
    #[allow(dead_code)]
    #[must_use]
    pub fn from_schema(schema: Schema) -> Self {
        Self::new(schema.backend, schema.tables)
    }

    /// Confirm this snapshot targets `expected` — the provider-lock guard a later
    /// slice's diff engine calls before diffing a desired state against the
    /// baseline (a snapshot generated for one dialect must not be diffed against
    /// another). Never enforced by the writer.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::BackendMismatch`] when the tags differ.
    // The provider-lock guard is consumed by the slice-4 diff engine (which
    // reads a baseline and diffs it against a desired state); the writer never
    // calls it, so it is not yet reachable from the command path.
    #[allow(dead_code)]
    pub fn ensure_backend_matches(&self, expected: Backend) -> Result<(), SnapshotError> {
        if self.backend == expected {
            Ok(())
        } else {
            Err(SnapshotError::BackendMismatch {
                expected,
                found: self.backend,
            })
        }
    }
}

/// Failure modes for reading, writing, or validating a snapshot file. `Display`
/// carries no secrets — a snapshot describes schema shape only.
// The read-side variants (`Json` / `UnsupportedVersion` / `BackendMismatch`) are
// produced by `load_snapshot` / `ensure_backend_matches`, which the slice-4 diff
// engine consumes; the current `snapshot` command only writes (using `Io`).
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The snapshot file could not be read or written.
    #[error("failed to access snapshot at {path}: {source}")]
    Io {
        /// The path that could not be read or written.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The snapshot file was not valid JSON (or did not match the envelope shape).
    #[error("failed to parse snapshot at {path}: {source}")]
    Json {
        /// The path whose contents failed to parse.
        path: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// The file declares a `snapshot_version` this CLI does not understand.
    #[error(
        "unsupported snapshot_version {found} (this build understands version {SNAPSHOT_VERSION}); \
         upgrade the Autumn CLI to read this snapshot"
    )]
    UnsupportedVersion {
        /// The version found in the file.
        found: u32,
    },
    /// The snapshot's dialect tag does not match the expected backend (the
    /// provider-lock guard; see [`SchemaSnapshot::ensure_backend_matches`]).
    #[error("snapshot backend {found:?} does not match the expected backend {expected:?}")]
    BackendMismatch {
        /// The backend the caller expected.
        expected: Backend,
        /// The backend recorded in the snapshot.
        found: Backend,
    },
}

/// Render `snapshot` to canonical, pretty-printed (2-space) JSON with a trailing
/// newline.
///
/// Canonical means deterministic: tables are sorted by name and, within each
/// table, indexes and checks are sorted (by name, then by a stable tiebreak), so
/// the same logical schema always yields byte-identical output. Columns keep
/// their declared order (the `CREATE TABLE` column order). Serializing the same
/// input twice is guaranteed to produce identical bytes.
#[must_use]
pub fn to_canonical_json(snapshot: &SchemaSnapshot) -> String {
    let canonical = SchemaSnapshot {
        snapshot_version: snapshot.snapshot_version,
        backend: snapshot.backend,
        tables: canonical_tables(&snapshot.tables),
    };
    // `serde_json::to_string_pretty` on a struct emits fields in declaration
    // order (not a sorted map), so ordering nondeterminism can only come from the
    // `Vec`s we sort above — the output is stable.
    let mut json = serde_json::to_string_pretty(&canonical)
        .expect("SchemaSnapshot is always serializable to JSON");
    json.push('\n');
    json
}

/// Return a name-sorted clone of `tables` with each table's indexes and checks
/// sorted too. Columns are intentionally left in declared order.
fn canonical_tables(tables: &[Table]) -> Vec<Table> {
    let mut tables = tables.to_vec();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    for table in &mut tables {
        table.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        // Checks carry an `Option<String>` name; fall back to the expression as a
        // stable tiebreak so unnamed checks still order deterministically.
        table.checks.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.expression.cmp(&b.expression))
        });
    }
    tables
}

/// Write `snapshot` canonically to `path`, creating any missing parent
/// directories (e.g. the conventional `.autumn/` dir).
///
/// The replacement is **atomic**: the canonical JSON is written to a
/// same-directory temporary file, flushed and `fsync`ed, then `rename`d over the
/// destination (an atomic swap on the same filesystem). A disk-full or
/// interrupted write therefore leaves the previously-committed baseline intact
/// instead of truncating or corrupting it, and readers never observe a
/// half-written file. On any failure the temp file is cleaned up (best-effort)
/// and the error is returned.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] if a parent directory cannot be created or the
/// snapshot cannot be written and swapped into place.
pub fn write_snapshot(path: &Path, snapshot: &SchemaSnapshot) -> Result<(), SnapshotError> {
    use std::io::Write;

    let io_error = |source: std::io::Error| SnapshotError::Io {
        path: path.display().to_string(),
        source,
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| SnapshotError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    // Stage into a temp file in the *same directory* as the destination so the
    // final `rename` is an atomic same-filesystem swap. `NamedTempFile` picks a
    // collision-resistant name and removes the temp file on drop, so any early
    // return below (or a failed `persist`) cleans it up automatically.
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(io_error)?;
    temp.write_all(to_canonical_json(snapshot).as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(io_error)?;
    temp.persist(path).map_err(|e| io_error(e.error))?;
    Ok(())
}

/// Read and validate a snapshot from `path`.
///
/// The `snapshot_version` is checked *before* the full envelope is deserialized,
/// so a future format the current CLI cannot read fails with a clear
/// [`SnapshotError::UnsupportedVersion`] rather than an opaque field-shape error.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] if the file cannot be read,
/// [`SnapshotError::UnsupportedVersion`] if it declares an unknown version, or
/// [`SnapshotError::Json`] if it is not a valid snapshot document.
// The baseline reader is consumed by the slice-4 diff engine; the current
// command only writes, so it is not yet reachable from the command path.
#[allow(dead_code)]
pub fn load_snapshot(path: &Path) -> Result<SchemaSnapshot, SnapshotError> {
    let text = std::fs::read_to_string(path).map_err(|source| SnapshotError::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse_snapshot_str(&text, &path.display().to_string())
}

/// Parse a snapshot from an in-memory string, applying the same version guard as
/// [`load_snapshot`]. Split out so it is testable without touching the
/// filesystem; `label` names the source in any error.
// Reachable only through `load_snapshot` (slice-4 diff engine) and the tests.
#[allow(dead_code)]
fn parse_snapshot_str(text: &str, label: &str) -> Result<SchemaSnapshot, SnapshotError> {
    // Read the version from a loose `Value` first so an unknown *future* format
    // (whose table shape this build may not understand) still fails with the
    // precise version error instead of a confusing deserialize error.
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|source| SnapshotError::Json {
            path: label.to_owned(),
            source,
        })?;
    if let Some(version) = value
        .get("snapshot_version")
        .and_then(serde_json::Value::as_u64)
        && version != u64::from(SNAPSHOT_VERSION)
    {
        return Err(SnapshotError::UnsupportedVersion {
            found: u32::try_from(version).unwrap_or(u32::MAX),
        });
    }
    serde_json::from_value(value).map_err(|source| SnapshotError::Json {
        path: label.to_owned(),
        source,
    })
}

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;
    use autumn_schema_core::{
        CheckConstraint, Column, ColumnDefault, ColumnType, ForeignKey, Index,
    };

    use crate::schema::parse::parse_model_source;

    /// A small, deterministic two-table fixture exercising columns, a foreign
    /// key, a default, indexes (deliberately out of name order), and a check.
    fn fixture_tables() -> Vec<Table> {
        // Declared second alphabetically but pushed first, to prove sorting.
        let mut posts = Table::new("posts", Backend::Postgres);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        posts.primary_key.push("id".to_owned());
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        let mut created = Column::new("created_at", ColumnType::Timestamp);
        created.default = Some(ColumnDefault::Now);
        posts.columns.push(id);
        posts.columns.push(author);
        posts.columns.push(Column::new("body", ColumnType::Text));
        posts.columns.push(created);
        // Two indexes pushed out of name order to prove index sorting.
        posts.indexes.push(Index {
            name: "idx_posts_created_at".to_owned(),
            columns: vec!["created_at".to_owned()],
            unique: false,
        });
        posts.indexes.push(Index {
            name: "idx_posts_author_id".to_owned(),
            columns: vec!["author_id".to_owned()],
            unique: false,
        });
        posts.checks.push(CheckConstraint {
            name: Some("posts_body_len".to_owned()),
            expression: "length(body) > 0".to_owned(),
        });

        let mut accounts = Table::new("accounts", Backend::Postgres);
        let mut aid = Column::new("id", ColumnType::Int64);
        aid.primary_key = true;
        accounts.primary_key.push("id".to_owned());
        let mut email = Column::new("email", ColumnType::Text);
        email.unique = true;
        accounts.columns.push(aid);
        accounts.columns.push(email);

        // `posts` pushed before `accounts` on purpose — canonical output sorts them.
        vec![posts, accounts]
    }

    #[test]
    fn round_trips_through_canonical_json() {
        let snapshot = SchemaSnapshot::new(Backend::Postgres, fixture_tables());
        let json = to_canonical_json(&snapshot);
        let back = parse_snapshot_str(&json, "<test>").expect("parse back");
        // Equality is order-insensitive at the table/index level only if both
        // sides are canonical; canonicalize the original for the comparison.
        let expected = SchemaSnapshot {
            snapshot_version: SNAPSHOT_VERSION,
            backend: Backend::Postgres,
            tables: canonical_tables(&snapshot.tables),
        };
        assert_eq!(back, expected);
    }

    #[test]
    fn write_then_load_round_trips_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A nested path proves parent-dir creation.
        let path = dir.path().join(".autumn").join("schema-snapshot.json");
        let snapshot = SchemaSnapshot::new(Backend::Sqlite, fixture_tables());
        write_snapshot(&path, &snapshot).expect("write");
        let loaded = load_snapshot(&path).expect("load");
        assert_eq!(loaded.backend, Backend::Sqlite);
        assert_eq!(loaded.snapshot_version, SNAPSHOT_VERSION);
        // Loaded tables are the canonical form.
        assert_eq!(loaded.tables, canonical_tables(&snapshot.tables));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // the pinned golden JSON fixture is inline
    fn canonical_json_matches_golden_and_is_byte_stable() {
        let snapshot = SchemaSnapshot::new(Backend::Postgres, fixture_tables());
        let json = to_canonical_json(&snapshot);

        // Golden fixture: tables sorted (accounts before posts), indexes sorted
        // (author_id before created_at), columns in declared order, envelope
        // fields leading. Re-generate deliberately if the IR shape changes.
        let expected = r#"{
  "snapshot_version": 1,
  "backend": "Postgres",
  "tables": [
    {
      "name": "accounts",
      "columns": [
        {
          "name": "id",
          "ty": "Int64",
          "nullable": false,
          "primary_key": true,
          "unique": false,
          "default": null,
          "references": null
        },
        {
          "name": "email",
          "ty": "Text",
          "nullable": false,
          "primary_key": false,
          "unique": true,
          "default": null,
          "references": null
        }
      ],
      "primary_key": [
        "id"
      ],
      "indexes": [],
      "checks": [],
      "backend": "Postgres",
      "managed": true
    },
    {
      "name": "posts",
      "columns": [
        {
          "name": "id",
          "ty": "Int64",
          "nullable": false,
          "primary_key": true,
          "unique": false,
          "default": null,
          "references": null
        },
        {
          "name": "author_id",
          "ty": "Int64",
          "nullable": false,
          "primary_key": false,
          "unique": false,
          "default": null,
          "references": {
            "table": "users",
            "column": "id"
          }
        },
        {
          "name": "body",
          "ty": "Text",
          "nullable": false,
          "primary_key": false,
          "unique": false,
          "default": null,
          "references": null
        },
        {
          "name": "created_at",
          "ty": "Timestamp",
          "nullable": false,
          "primary_key": false,
          "unique": false,
          "default": "Now",
          "references": null
        }
      ],
      "primary_key": [
        "id"
      ],
      "indexes": [
        {
          "name": "idx_posts_author_id",
          "columns": [
            "author_id"
          ],
          "unique": false
        },
        {
          "name": "idx_posts_created_at",
          "columns": [
            "created_at"
          ],
          "unique": false
        }
      ],
      "checks": [
        {
          "name": "posts_body_len",
          "expression": "length(body) > 0"
        }
      ],
      "backend": "Postgres",
      "managed": true
    }
  ]
}
"#;
        assert_eq!(
            json, expected,
            "canonical JSON drifted from the golden fixture"
        );
        // Byte-stability: re-serializing yields identical bytes.
        assert_eq!(to_canonical_json(&snapshot), json);
        // And canonicalization is idempotent: feeding the canonical form back in
        // produces the same output (input order does not matter).
        let reparsed = parse_snapshot_str(&json, "<test>").expect("parse");
        assert_eq!(to_canonical_json(&reparsed), json);
    }

    #[test]
    fn write_snapshot_atomically_replaces_existing_file_without_leftover_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schema-snapshot.json");

        // Seed a pre-existing (stale) baseline at the destination.
        std::fs::write(&path, "OLD STALE CONTENT").expect("seed old baseline");

        let snapshot = SchemaSnapshot::new(Backend::Postgres, fixture_tables());
        write_snapshot(&path, &snapshot).expect("write");

        // The destination now holds the fresh canonical JSON (replacement worked).
        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, to_canonical_json(&snapshot));
        assert!(!written.contains("OLD STALE CONTENT"));

        // No temp file leaked into the directory on success: the destination is
        // the only entry.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("schema-snapshot.json")],
            "the atomic write must leave no leftover temp file: {entries:?}"
        );
    }

    #[test]
    fn unknown_future_version_is_rejected() {
        let doc = r#"{ "snapshot_version": 999, "backend": "Postgres", "tables": [] }"#;
        let err = parse_snapshot_str(doc, "<test>").unwrap_err();
        match err {
            SnapshotError::UnsupportedVersion { found } => assert_eq!(found, 999),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn backend_tag_serializes_and_ensure_matches() {
        let pg = SchemaSnapshot::new(Backend::Postgres, Vec::new());
        assert!(to_canonical_json(&pg).contains("\"backend\": \"Postgres\""));
        assert!(pg.ensure_backend_matches(Backend::Postgres).is_ok());
        let mismatch = pg.ensure_backend_matches(Backend::Sqlite).unwrap_err();
        assert!(matches!(
            mismatch,
            SnapshotError::BackendMismatch {
                expected: Backend::Sqlite,
                found: Backend::Postgres
            }
        ));

        let sqlite = SchemaSnapshot::new(Backend::Sqlite, Vec::new());
        assert!(to_canonical_json(&sqlite).contains("\"backend\": \"Sqlite\""));
        assert!(sqlite.ensure_backend_matches(Backend::Sqlite).is_ok());
    }

    #[test]
    fn malformed_json_is_a_json_error() {
        let err = parse_snapshot_str("{ not json", "<test>").unwrap_err();
        assert!(matches!(err, SnapshotError::Json { .. }));
    }

    #[test]
    fn end_to_end_from_parsed_model_source() {
        // Slice-2 parser -> snapshot -> canonical JSON contains the expected shape.
        let src = r#"
            #[autumn_web::model]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let parsed = parse_model_source(src, Backend::Postgres).expect("parse");
        let snapshot = SchemaSnapshot::new(Backend::Postgres, parsed.tables);
        let json = to_canonical_json(&snapshot);
        assert!(json.contains("\"name\": \"posts\""));
        assert!(json.contains("\"name\": \"title\""));
        assert!(json.contains("\"snapshot_version\": 1"));
        assert!(json.contains("\"default\": \"Now\""));
        // Re-loading the produced JSON yields a valid, version-1 snapshot.
        let loaded = parse_snapshot_str(&json, "<test>").expect("reload");
        assert_eq!(loaded.snapshot_version, SNAPSHOT_VERSION);
        assert_eq!(loaded.tables.len(), 1);
        assert_eq!(loaded.tables[0].name, "posts");
    }
}

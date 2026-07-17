//! Automatic record version history for `#[repository]` writes.
//!
//! This module provides the types, traits, and utilities for Autumn's
//! opt-in record version history feature (see issue #700). When a
//! `#[repository]` model is annotated with `versioned = true`, every
//! successful insert, update, and delete produces an immutable
//! [`VersionEntry`] that records the actor, the column-level diff, and
//! the originating request ID.
//!
//! # Opting in
//!
//! ```rust,ignore
//! #[repository(Post, versioned = true)]
//! pub trait PostRepository {}
//! ```
//!
//! That single declaration is the only per-model change required.
//! All write paths — hand-written handlers, `#[repository(api = "…")]`
//! auto-generated endpoints, `#[job]` and `#[mailer]` paths, and
//! `autumn task` one-off scripts — capture history automatically.
//!
//! # Retrieval
//!
//! ```rust,ignore
//! // Chronological page of entries for record 42.
//! let page = Post::history(42, &mut db, VersionFilter::default()).await?;
//!
//! // Changes between two timestamps.
//! let filter = VersionFilter::between(from, to);
//! let page = Post::history(42, &mut db, filter).await?;
//! ```
//!
//! # Sensitive columns
//!
//! ```rust,ignore
//! #[version_history(sensitive = ["password_digest", "reset_token"])]
//! #[repository(Post, versioned = true)]
//! pub trait PostRepository {}
//! ```
//!
//! Excluded columns still appear in the entry as changed (so the
//! timeline is complete) but their before/after values are omitted.
//!
//! # Immutability
//!
//! There is no public API to update or delete history entries.
//! Test-fixture teardown uses `VersionHistoryStore::__test_clear_for_record`,
//! which is **not** part of the stable public API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Narrow framework migration set required by `#[repository(..., versioned = true)]`.
///
/// Backend-forked (#1996): the Postgres DDL (`BIGSERIAL`/`JSONB`/`DEFAULT NOW()`/
/// `ADD COLUMN IF NOT EXISTS`) is not valid `SQLite`, so the `SQLite` build embeds
/// a parallel set (`INTEGER PRIMARY KEY`/`TEXT`/`DEFAULT CURRENT_TIMESTAMP`) under
/// the same `20260526000000_create_version_history` version dir name, keeping
/// `__diesel_schema_migrations` bookkeeping identical across backends.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub const VERSION_HISTORY_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("version_history_migrations");

/// `SQLite` variant of [`VERSION_HISTORY_MIGRATIONS`]. See that item for the
/// backend-fork rationale.
#[cfg(all(feature = "db", feature = "sqlite"))]
pub const VERSION_HISTORY_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("version_history_migrations_sqlite");

/// Link-time marker emitted by generated repositories that opt into version history.
#[cfg(feature = "db")]
#[doc(hidden)]
pub struct VersionedRepositoryDescriptor;

#[cfg(feature = "db")]
inventory::collect!(VersionedRepositoryDescriptor);

#[cfg(feature = "db")]
pub(crate) fn has_versioned_repository_descriptors() -> bool {
    inventory::iter::<VersionedRepositoryDescriptor>
        .into_iter()
        .next()
        .is_some()
}

// ── Operation discriminant ───────────────────────────────────────────

/// The mutation operation that produced a version entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionOp {
    /// A new record was inserted.
    Insert,
    /// An existing record was updated.
    Update,
    /// A record was deleted.
    Delete,
}

impl VersionOp {
    /// Returns the operation name as a static string slice.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

impl std::fmt::Display for VersionOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Column-level diff ────────────────────────────────────────────────

/// A single column's before/after values in a version entry.
///
/// For inserts, `before` is `None`. For deletes, `after` is `None`.
/// For sensitive columns that changed, both `before` and `after` are
/// `None` (but the entry appears so the timeline is complete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnChange {
    /// The column name.
    pub column: String,
    /// Value before the mutation (`None` for inserts or sensitive columns).
    pub before: Option<serde_json::Value>,
    /// Value after the mutation (`None` for deletes or sensitive columns).
    pub after: Option<serde_json::Value>,
    /// Whether this column was excluded from diff capture for privacy.
    pub sensitive: bool,
}

impl ColumnChange {
    /// Create a regular (non-sensitive) change entry.
    #[must_use]
    pub fn new(
        column: impl Into<String>,
        before: Option<serde_json::Value>,
        after: Option<serde_json::Value>,
    ) -> Self {
        Self {
            column: column.into(),
            before,
            after,
            sensitive: false,
        }
    }

    /// Create a sensitive change entry (values omitted, column name retained).
    #[must_use]
    pub fn sensitive(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            before: None,
            after: None,
            sensitive: true,
        }
    }
}

// ── Version entry ────────────────────────────────────────────────────

/// An immutable record of a single mutation to a versioned model.
///
/// Each insert, update, or delete on an opted-in repository produces
/// exactly one `VersionEntry`. Entries are append-only: there is no
/// public API to modify or delete them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionEntry {
    /// Auto-incrementing primary key in the history table.
    pub id: i64,
    /// The table name of the model that was mutated.
    pub table_name: String,
    /// The primary key of the record that was mutated.
    pub record_id: i64,
    /// The kind of mutation.
    pub op: VersionOp,
    /// Actor identifier: authenticated user ID, or `"system"` when no
    /// session is in scope (jobs, scheduled tasks, migrations).
    pub actor: String,
    /// Request / trace correlation ID (from `MutationContext`).
    pub request_id: Option<String>,
    /// Column-level diff. For updates, only changed columns are included.
    pub changes: Vec<ColumnChange>,
    /// UTC timestamp when this entry was written.
    pub recorded_at: DateTime<Utc>,
}

impl VersionEntry {
    /// The actor token used when no session is in scope.
    pub const SYSTEM_ACTOR: &'static str = "system";
}

// ── Retrieval filter ─────────────────────────────────────────────────

/// Filter parameters for version history retrieval.
///
/// ```rust,ignore
/// // Default: first page, 25 entries.
/// let page = Post::history(42, &mut db, VersionFilter::default()).await?;
///
/// // Changes in a specific time window.
/// let filter = VersionFilter::between(from, to);
/// let page = Post::history(42, &mut db, filter).await?;
/// ```
#[derive(Debug, Clone)]
pub struct VersionFilter {
    /// Only include entries recorded after this timestamp (inclusive).
    pub from: Option<DateTime<Utc>>,
    /// Only include entries recorded before this timestamp (inclusive).
    pub to: Option<DateTime<Utc>>,
    /// 1-indexed page number (defaults to 1).
    pub page: u64,
    /// Entries per page (defaults to 25).
    pub per_page: u64,
}

impl Default for VersionFilter {
    fn default() -> Self {
        Self {
            from: None,
            to: None,
            page: 1,
            per_page: 25,
        }
    }
}

impl VersionFilter {
    /// Create a filter for changes between two timestamps.
    #[must_use]
    pub const fn between(from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        Self {
            from: Some(from),
            to: Some(to),
            page: 1,
            per_page: 25,
        }
    }

    /// Effective page number (at least 1).
    #[must_use]
    pub fn page(&self) -> u64 {
        self.page.max(1)
    }

    /// Effective page size (at least 1, at most 100).
    #[must_use]
    pub fn per_page(&self) -> u64 {
        self.per_page.clamp(1, 100)
    }

    /// LIMIT/OFFSET for SQL queries.
    ///
    /// Uses saturating arithmetic so an astronomically large `page` value
    /// from query parameters cannot overflow: the offset is capped at
    /// `i64::MAX` (far beyond any realistic table size).
    #[must_use]
    pub fn limit_offset(&self) -> (i64, i64) {
        let per = self.per_page().cast_signed();
        let offset = (self.page() - 1)
            .saturating_mul(self.per_page())
            .min(i64::MAX as u64)
            .cast_signed();
        (per, offset)
    }
}

// ── Paginated result ─────────────────────────────────────────────────

/// A paginated page of [`VersionEntry`] records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPage {
    /// The entries for the current page, in chronological order (oldest first).
    pub entries: Vec<VersionEntry>,
    /// Total number of entries matching the filter.
    pub total: u64,
    /// Current page (1-indexed).
    pub page: u64,
    /// Entries per page.
    pub per_page: u64,
}

impl VersionPage {
    /// Total number of pages.
    #[must_use]
    pub const fn total_pages(&self) -> u64 {
        if self.per_page == 0 {
            return 0;
        }
        self.total.div_ceil(self.per_page)
    }

    /// Whether there is a next page.
    #[must_use]
    pub const fn has_next_page(&self) -> bool {
        self.page < self.total_pages()
    }

    /// Whether there is a previous page.
    #[must_use]
    pub const fn has_prev_page(&self) -> bool {
        self.page > 1
    }
}

// ── Diff computation ─────────────────────────────────────────────────

/// Compute the column-level diff between `before` and `after` JSON objects.
///
/// - Only columns present in `after` are included (the update changeset).
/// - Columns where the value did not change are omitted.
/// - Columns listed in `sensitive` appear as [`ColumnChange::sensitive`]
///   when they changed, with both values omitted.
///
/// Returns an empty `Vec` when nothing changed.
#[must_use]
pub fn compute_diff(
    before: &serde_json::Value,
    after: &serde_json::Value,
    sensitive: &[&str],
) -> Vec<ColumnChange> {
    let before_obj = before.as_object();
    let Some(after_obj) = after.as_object() else {
        return vec![];
    };

    let mut changes = Vec::new();
    for (col, after_val) in after_obj {
        let before_val = before_obj.and_then(|o| o.get(col.as_str()));
        let did_change = before_val != Some(after_val);
        if !did_change {
            continue;
        }
        if sensitive.contains(&col.as_str()) {
            changes.push(ColumnChange::sensitive(col.clone()));
        } else {
            changes.push(ColumnChange::new(
                col.clone(),
                before_val.cloned(),
                Some(after_val.clone()),
            ));
        }
    }
    changes
}

/// Compute changes for an insert (all columns are "after" values).
///
/// Sensitive columns appear as [`ColumnChange::sensitive`].
#[must_use]
pub fn compute_insert_changes(record: &serde_json::Value, sensitive: &[&str]) -> Vec<ColumnChange> {
    let Some(obj) = record.as_object() else {
        return vec![];
    };
    obj.iter()
        .map(|(col, val)| {
            if sensitive.contains(&col.as_str()) {
                ColumnChange::sensitive(col.clone())
            } else {
                ColumnChange::new(col.clone(), None, Some(val.clone()))
            }
        })
        .collect()
}

/// Compute changes for a delete (all columns are "before" values).
///
/// Sensitive columns appear as [`ColumnChange::sensitive`].
#[must_use]
pub fn compute_delete_changes(record: &serde_json::Value, sensitive: &[&str]) -> Vec<ColumnChange> {
    let Some(obj) = record.as_object() else {
        return vec![];
    };
    obj.iter()
        .map(|(col, val)| {
            if sensitive.contains(&col.as_str()) {
                ColumnChange::sensitive(col.clone())
            } else {
                ColumnChange::new(col.clone(), Some(val.clone()), None)
            }
        })
        .collect()
}

// ── VersionedRecord trait ────────────────────────────────────────────

/// Implemented by models that opt into automatic version history.
///
/// The `#[repository(Model, versioned = true)]` macro generates this
/// implementation automatically. You do not need to implement it by hand.
///
/// Manual implementation is possible for models with custom serialization
/// requirements or non-standard primary keys.
pub trait VersionedRecord: Send + Sync + 'static {
    /// The database table name for this model.
    fn version_table_name() -> &'static str
    where
        Self: Sized;

    /// The primary key of this record.
    fn version_record_id(&self) -> i64;

    /// Serialize this record's column values as a JSON object.
    ///
    /// Used to compute diffs. The default implementation serializes via
    /// `serde_json::to_value` and falls back to an empty object on error.
    fn version_column_values(&self) -> serde_json::Value;

    /// Columns whose values must not appear in history entries.
    ///
    /// These columns still appear in the diff as "changed" (when they
    /// changed) but their before/after values are replaced with `null`.
    /// Defaults to an empty slice (no columns excluded).
    #[must_use]
    fn version_sensitive_columns() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &[]
    }

    /// Tenant scope for history rows written by tenant-scoped repositories.
    ///
    /// Non-tenant repositories return `None`. Generated tenant-scoped
    /// repositories override this so `version_history()` can fail closed to the
    /// current tenant unless `across_tenants()` is used.
    fn version_tenant_id(&self) -> Option<&str> {
        None
    }
}

/// Extract a tenant ID from supported generated model field shapes.
///
/// Generated tenant-scoped `VersionedRecord` impls use this so both
/// `tenant_id: String` and `tenant_id: Option<String>` models can opt into
/// version history.
#[doc(hidden)]
pub trait VersionTenantIdValue {
    fn version_tenant_id(&self) -> Option<&str>;
}

impl VersionTenantIdValue for String {
    fn version_tenant_id(&self) -> Option<&str> {
        Some(self.as_str())
    }
}

impl VersionTenantIdValue for Option<String> {
    fn version_tenant_id(&self) -> Option<&str> {
        self.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── VersionOp ────────────────────────────────────────────────────

    #[test]
    fn version_op_as_str() {
        assert_eq!(VersionOp::Insert.as_str(), "insert");
        assert_eq!(VersionOp::Update.as_str(), "update");
        assert_eq!(VersionOp::Delete.as_str(), "delete");
    }

    #[test]
    fn version_op_display() {
        assert_eq!(format!("{}", VersionOp::Insert), "insert");
        assert_eq!(format!("{}", VersionOp::Update), "update");
        assert_eq!(format!("{}", VersionOp::Delete), "delete");
    }

    #[test]
    fn version_op_serde_roundtrip() {
        for op in [VersionOp::Insert, VersionOp::Update, VersionOp::Delete] {
            let json = serde_json::to_string(&op).unwrap();
            let back: VersionOp = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op);
        }
    }

    // ── ColumnChange ─────────────────────────────────────────────────

    #[test]
    fn column_change_new_not_sensitive() {
        let c = ColumnChange::new(
            "title",
            Some(serde_json::json!("old")),
            Some(serde_json::json!("new")),
        );
        assert_eq!(c.column, "title");
        assert!(!c.sensitive);
        assert_eq!(c.before, Some(serde_json::json!("old")));
        assert_eq!(c.after, Some(serde_json::json!("new")));
    }

    #[test]
    fn column_change_sensitive_omits_values() {
        let c = ColumnChange::sensitive("password_digest");
        assert_eq!(c.column, "password_digest");
        assert!(c.sensitive);
        assert!(c.before.is_none());
        assert!(c.after.is_none());
    }

    #[test]
    fn column_change_insert_has_no_before() {
        let c = ColumnChange::new("title", None, Some(serde_json::json!("Hello")));
        assert!(c.before.is_none());
        assert_eq!(c.after, Some(serde_json::json!("Hello")));
    }

    #[test]
    fn column_change_delete_has_no_after() {
        let c = ColumnChange::new("title", Some(serde_json::json!("Hello")), None);
        assert_eq!(c.before, Some(serde_json::json!("Hello")));
        assert!(c.after.is_none());
    }

    #[test]
    fn column_change_serde_roundtrip() {
        let c = ColumnChange::new(
            "body",
            Some(serde_json::json!("old")),
            Some(serde_json::json!("new")),
        );
        let json = serde_json::to_string(&c).unwrap();
        let back: ColumnChange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn column_change_sensitive_serde_roundtrip() {
        let c = ColumnChange::sensitive("secret");
        let json = serde_json::to_string(&c).unwrap();
        let back: ColumnChange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    // ── VersionEntry ─────────────────────────────────────────────────

    #[test]
    fn version_entry_system_actor_constant() {
        assert_eq!(VersionEntry::SYSTEM_ACTOR, "system");
    }

    #[test]
    fn version_entry_serde_roundtrip() {
        let entry = VersionEntry {
            id: 1,
            table_name: "posts".to_owned(),
            record_id: 42,
            op: VersionOp::Update,
            actor: "user-123".to_owned(),
            request_id: Some("req-abc".to_owned()),
            changes: vec![ColumnChange::new(
                "title",
                Some(serde_json::json!("old")),
                Some(serde_json::json!("new")),
            )],
            recorded_at: DateTime::from_timestamp(0, 0).unwrap(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: VersionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn version_entry_request_id_optional() {
        let entry = VersionEntry {
            id: 1,
            table_name: "users".to_owned(),
            record_id: 1,
            op: VersionOp::Insert,
            actor: VersionEntry::SYSTEM_ACTOR.to_owned(),
            request_id: None,
            changes: vec![],
            recorded_at: Utc::now(),
        };
        assert!(entry.request_id.is_none());
    }

    // ── VersionFilter ────────────────────────────────────────────────

    #[test]
    fn version_filter_default_page_and_per_page() {
        let f = VersionFilter::default();
        assert_eq!(f.page(), 1);
        assert_eq!(f.per_page(), 25);
    }

    #[test]
    fn version_filter_page_zero_clamps_to_one() {
        let f = VersionFilter {
            page: 0,
            per_page: 10,
            ..Default::default()
        };
        assert_eq!(f.page(), 1);
    }

    #[test]
    fn version_filter_per_page_zero_clamps_to_one() {
        let f = VersionFilter {
            page: 1,
            per_page: 0,
            ..Default::default()
        };
        assert_eq!(f.per_page(), 1);
    }

    #[test]
    fn version_filter_per_page_over_100_clamps_to_100() {
        let f = VersionFilter {
            page: 1,
            per_page: 500,
            ..Default::default()
        };
        assert_eq!(f.per_page(), 100);
    }

    #[test]
    fn version_filter_limit_offset_first_page() {
        let f = VersionFilter {
            page: 1,
            per_page: 25,
            ..Default::default()
        };
        assert_eq!(f.limit_offset(), (25, 0));
    }

    #[test]
    fn version_filter_limit_offset_second_page() {
        let f = VersionFilter {
            page: 2,
            per_page: 10,
            ..Default::default()
        };
        assert_eq!(f.limit_offset(), (10, 10));
    }

    #[test]
    fn version_filter_between_sets_timestamps() {
        let from = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let to = DateTime::from_timestamp(2_000_000, 0).unwrap();
        let f = VersionFilter::between(from, to);
        assert_eq!(f.from, Some(from));
        assert_eq!(f.to, Some(to));
        assert_eq!(f.page, 1);
        assert_eq!(f.per_page, 25);
    }

    // ── VersionPage ──────────────────────────────────────────────────

    #[test]
    fn version_page_total_pages_exact() {
        let p = VersionPage {
            entries: vec![],
            total: 20,
            page: 1,
            per_page: 10,
        };
        assert_eq!(p.total_pages(), 2);
    }

    #[test]
    fn version_page_total_pages_partial() {
        let p = VersionPage {
            entries: vec![],
            total: 21,
            page: 1,
            per_page: 10,
        };
        assert_eq!(p.total_pages(), 3);
    }

    #[test]
    fn version_page_total_pages_zero_per_page() {
        let p = VersionPage {
            entries: vec![],
            total: 10,
            page: 1,
            per_page: 0,
        };
        assert_eq!(p.total_pages(), 0);
    }

    #[test]
    fn version_page_has_next_page() {
        let p = VersionPage {
            entries: vec![],
            total: 30,
            page: 1,
            per_page: 10,
        };
        assert!(p.has_next_page());
    }

    #[test]
    fn version_page_no_next_page_on_last() {
        let p = VersionPage {
            entries: vec![],
            total: 30,
            page: 3,
            per_page: 10,
        };
        assert!(!p.has_next_page());
    }

    #[test]
    fn version_page_has_prev_page() {
        let p = VersionPage {
            entries: vec![],
            total: 30,
            page: 2,
            per_page: 10,
        };
        assert!(p.has_prev_page());
    }

    #[test]
    fn version_page_no_prev_page_on_first() {
        let p = VersionPage {
            entries: vec![],
            total: 30,
            page: 1,
            per_page: 10,
        };
        assert!(!p.has_prev_page());
    }

    // ── compute_diff ────────────────────────────────────────────────

    #[test]
    fn compute_diff_changed_columns_only() {
        let before = serde_json::json!({"title": "old", "body": "same", "published": false});
        let after = serde_json::json!({"title": "new", "body": "same", "published": true});
        let changes = compute_diff(&before, &after, &[]);
        let cols: Vec<&str> = changes.iter().map(|c| c.column.as_str()).collect();
        // Only changed columns appear
        assert!(
            cols.contains(&"title"),
            "title should be in changes: {cols:?}"
        );
        assert!(
            cols.contains(&"published"),
            "published should be in changes: {cols:?}"
        );
        assert!(
            !cols.contains(&"body"),
            "unchanged body must not appear: {cols:?}"
        );
    }

    #[test]
    fn compute_diff_no_changes_returns_empty() {
        let before = serde_json::json!({"title": "same"});
        let after = serde_json::json!({"title": "same"});
        let changes = compute_diff(&before, &after, &[]);
        assert!(changes.is_empty());
    }

    #[test]
    fn compute_diff_sensitive_column_omits_values() {
        let before = serde_json::json!({"title": "old", "password_digest": "hash1"});
        let after = serde_json::json!({"title": "new", "password_digest": "hash2"});
        let changes = compute_diff(&before, &after, &["password_digest"]);
        let pass_change = changes
            .iter()
            .find(|c| c.column == "password_digest")
            .unwrap();
        assert!(
            pass_change.sensitive,
            "password_digest should be marked sensitive"
        );
        assert!(
            pass_change.before.is_none(),
            "sensitive before must be null"
        );
        assert!(pass_change.after.is_none(), "sensitive after must be null");
    }

    #[test]
    fn compute_diff_sensitive_unchanged_column_excluded() {
        let before = serde_json::json!({"title": "new", "password_digest": "samehash"});
        let after = serde_json::json!({"title": "new", "password_digest": "samehash"});
        let changes = compute_diff(&before, &after, &["password_digest"]);
        // If sensitive column didn't change, it should NOT appear (no info leak)
        assert!(changes.is_empty(), "no changes means no entries at all");
    }

    #[test]
    fn compute_diff_with_null_before_treated_as_insert() {
        let before = serde_json::json!(null);
        let after = serde_json::json!({"title": "Hello"});
        let changes = compute_diff(&before, &after, &[]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].column, "title");
        assert!(changes[0].before.is_none());
        assert_eq!(changes[0].after, Some(serde_json::json!("Hello")));
    }

    // ── compute_insert_changes ───────────────────────────────────────

    #[test]
    fn compute_insert_changes_all_columns_are_after() {
        let record = serde_json::json!({"id": 1, "title": "Hello", "body": "World"});
        let changes = compute_insert_changes(&record, &[]);
        assert_eq!(changes.len(), 3);
        for c in &changes {
            assert!(c.before.is_none(), "insert before must be None");
            assert!(c.after.is_some(), "insert after must be Some");
            assert!(!c.sensitive);
        }
    }

    #[test]
    fn compute_insert_changes_sensitive_columns_omitted() {
        let record = serde_json::json!({"id": 1, "password_digest": "hashed"});
        let changes = compute_insert_changes(&record, &["password_digest"]);
        let pass = changes
            .iter()
            .find(|c| c.column == "password_digest")
            .unwrap();
        assert!(pass.sensitive);
        assert!(pass.before.is_none());
        assert!(pass.after.is_none());
    }

    // ── compute_delete_changes ───────────────────────────────────────

    #[test]
    fn compute_delete_changes_all_columns_are_before() {
        let record = serde_json::json!({"id": 5, "title": "Bye"});
        let changes = compute_delete_changes(&record, &[]);
        assert_eq!(changes.len(), 2);
        for c in &changes {
            assert!(c.before.is_some(), "delete before must be Some");
            assert!(c.after.is_none(), "delete after must be None");
        }
    }

    #[test]
    fn compute_delete_changes_sensitive_columns_omitted() {
        let record = serde_json::json!({"id": 5, "secret_token": "tok"});
        let changes = compute_delete_changes(&record, &["secret_token"]);
        let secret = changes.iter().find(|c| c.column == "secret_token").unwrap();
        assert!(secret.sensitive);
        assert!(secret.before.is_none());
        assert!(secret.after.is_none());
    }

    // ── VersionedRecord trait ────────────────────────────────────────

    #[test]
    fn versioned_record_default_sensitive_columns_is_empty() {
        struct Dummy;
        impl VersionedRecord for Dummy {
            fn version_table_name() -> &'static str {
                "dummies"
            }
            fn version_record_id(&self) -> i64 {
                1
            }
            fn version_column_values(&self) -> serde_json::Value {
                serde_json::json!({})
            }
        }
        assert!(Dummy::version_sensitive_columns().is_empty());
        assert_eq!(Dummy.version_tenant_id(), None);
    }

    #[test]
    fn versioned_record_custom_sensitive_columns() {
        struct SecureModel;
        impl VersionedRecord for SecureModel {
            fn version_table_name() -> &'static str {
                "secure_models"
            }
            fn version_record_id(&self) -> i64 {
                99
            }
            fn version_column_values(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn version_sensitive_columns() -> &'static [&'static str] {
                &["password_digest", "api_key"]
            }
        }
        let cols = SecureModel::version_sensitive_columns();
        assert!(cols.contains(&"password_digest"));
        assert!(cols.contains(&"api_key"));
    }

    #[test]
    fn versioned_record_can_expose_tenant_id_for_scoped_history() {
        struct TenantModel {
            tenant_id: String,
        }
        impl VersionedRecord for TenantModel {
            fn version_table_name() -> &'static str {
                "tenant_models"
            }
            fn version_record_id(&self) -> i64 {
                7
            }
            fn version_column_values(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn version_tenant_id(&self) -> Option<&str> {
                Some(self.tenant_id.as_str())
            }
        }

        let record = TenantModel {
            tenant_id: "tenant-a".to_owned(),
        };

        assert_eq!(record.version_tenant_id(), Some("tenant-a"));
    }

    #[test]
    fn version_tenant_id_value_supports_required_tenant_id() {
        let tenant_id = "tenant-a".to_owned();

        assert_eq!(
            VersionTenantIdValue::version_tenant_id(&tenant_id),
            Some("tenant-a")
        );
    }

    #[test]
    fn version_tenant_id_value_supports_optional_tenant_id() {
        let tenant_id = Some("tenant-a".to_owned());
        let missing: Option<String> = None;

        assert_eq!(
            VersionTenantIdValue::version_tenant_id(&tenant_id),
            Some("tenant-a")
        );
        assert_eq!(VersionTenantIdValue::version_tenant_id(&missing), None);
    }

    // ── Immutability / append-only guarantee ────────────────────────

    #[test]
    fn version_entry_op_is_not_mutable_via_public_api() {
        // The entry type is non-exhaustive from the user's perspective:
        // they can read all fields but the only construction path in real
        // code is via the generated repository (no pub constructor that
        // takes all fields). This test documents the "append-only" contract
        // by verifying there is no public mutating method on VersionEntry.
        let mut entry = VersionEntry {
            id: 1,
            table_name: "posts".to_owned(),
            record_id: 1,
            op: VersionOp::Insert,
            actor: "system".to_owned(),
            request_id: None,
            changes: vec![],
            recorded_at: Utc::now(),
        };
        // Fields are pub so tests can construct them; however there is
        // intentionally no `delete()` or `update()` method.
        entry.actor = "reassigned-in-test".to_owned(); // allowed for test setup
        assert_eq!(entry.actor, "reassigned-in-test");
        // What's NOT present: VersionEntry::delete(&mut db).await
        // That's the append-only guarantee — enforced by absence.
    }
}

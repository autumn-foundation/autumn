//! Core traits that models implement to participate in the admin panel.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

// ── Field metadata ──────────────────────────────────────────────────

/// The kind of a model field, used to select the appropriate form widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminFieldKind {
    /// Single-line text input.
    Text,
    /// Multi-line textarea.
    TextArea,
    /// Integer input.
    Integer,
    /// Floating-point input.
    Float,
    /// Boolean checkbox.
    Boolean,
    /// Date picker (no time).
    Date,
    /// Date + time picker.
    DateTime,
    /// Select dropdown with fixed choices.
    Select(Vec<SelectOption>),
    /// Hidden field (shown in detail, not editable).
    Hidden,
    /// Password field (write-only, never displayed).
    Password,
    /// JSON editor.
    Json,
}

/// A single option in a [`AdminFieldKind::Select`] dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// Metadata for a single model field.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // orthogonal flags on a plain config record
#[non_exhaustive]
pub struct AdminField {
    /// Column name in the database / struct field name.
    pub name: &'static str,
    /// Human-readable label for the UI.
    pub label: String,
    /// Widget type.
    pub kind: AdminFieldKind,
    /// Whether this field appears in the list view table.
    pub list_display: bool,
    /// Whether this field is searchable (included in text search).
    pub searchable: bool,
    /// Whether this field can be used as a filter.
    pub filterable: bool,
    /// Whether this field is required on create/edit forms.
    pub required: bool,
    /// Whether this field is editable (false for IDs, timestamps, etc.).
    pub editable: bool,
    /// Editable only on create; shown as read-only on edit (e.g. `principal_id`, `expires_at`).
    /// `strip_meta_fields` drops these on update submissions so the model never sees them.
    pub create_only: bool,
    /// Sort priority in list view (None = not sortable).
    pub sortable: bool,
    /// Whether this column is encrypted at rest (#805). When set, the field is
    /// rendered as a disabled, redacted, unsubmitted control in forms (so its
    /// plaintext is never placed into the HTML and a save never overwrites the
    /// stored ciphertext), and — unless [`Self::encrypted_visible`] is also set —
    /// redacted (`••••••••`) in list and detail views.
    ///
    /// This is a per-field flag rather than a global column-name lookup so that an
    /// unrelated resource with a same-named plaintext field stays fully editable.
    pub encrypted: bool,
    /// For an [`Self::encrypted`] column, show its decrypted plaintext in list and
    /// detail (read) views — the `#[encrypted(admin_visible)]` opt-in. Edit forms
    /// still never pre-fill the plaintext. Has no effect unless `encrypted` is set.
    pub encrypted_visible: bool,
}

impl AdminField {
    /// Create a new field with sensible defaults.
    ///
    /// By default: displayed in list, not searchable, not filterable,
    /// required, and sortable. Editable defaults to `true` except for
    /// [`AdminFieldKind::Hidden`], which is read-only by contract
    /// (and is therefore excluded from `strip_meta_fields` acceptance
    /// even if a caller later flips `editable` back to `true`).
    #[must_use]
    pub fn new(name: &'static str, kind: AdminFieldKind) -> Self {
        let editable = !matches!(kind, AdminFieldKind::Hidden);
        Self {
            name,
            label: humanize_field_name(name),
            kind,
            list_display: true,
            searchable: false,
            filterable: false,
            required: true,
            editable,
            create_only: false,
            sortable: true,
            encrypted: false,
            encrypted_visible: false,
        }
    }

    /// Mark this column as encrypted at rest (#805): redacted in read views and
    /// rendered as a disabled, unsubmitted control in forms.
    #[must_use]
    pub const fn encrypted(mut self) -> Self {
        self.encrypted = true;
        self
    }

    /// Mark this column as encrypted at rest but show its decrypted plaintext in
    /// read views (the `#[encrypted(admin_visible)]` opt-in). Implies
    /// [`Self::encrypted`]; edit forms still never pre-fill the plaintext.
    #[must_use]
    pub const fn encrypted_visible(mut self) -> Self {
        self.encrypted = true;
        self.encrypted_visible = true;
        self
    }

    /// Set the human-readable label.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Mark this field as searchable.
    #[must_use]
    pub const fn searchable(mut self) -> Self {
        self.searchable = true;
        self
    }

    /// Mark this field as filterable.
    #[must_use]
    pub const fn filterable(mut self) -> Self {
        self.filterable = true;
        self
    }

    /// Mark this field as optional.
    #[must_use]
    pub const fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// Mark this field as read-only.
    #[must_use]
    pub const fn readonly(mut self) -> Self {
        self.editable = false;
        self
    }

    /// Editable on create, read-only on edit (e.g. `principal_id`, `expires_at`).
    /// The field renders normally in the create form but as a disabled display
    /// in the edit form; update submissions never receive its value.
    #[must_use]
    pub const fn create_only(mut self) -> Self {
        self.create_only = true;
        self
    }

    /// Hide this field from the list view.
    #[must_use]
    pub const fn hide_from_list(mut self) -> Self {
        self.list_display = false;
        self
    }
}

// ── Bulk actions ────────────────────────────────────────────────────

/// A named bulk action that can be performed on selected records.
pub struct AdminAction {
    /// Machine name (used in form values).
    pub name: &'static str,
    /// Human-readable label for the button.
    pub label: String,
    /// CSS class for styling (e.g., "danger" for destructive actions).
    pub style: ActionStyle,
    /// Whether a confirmation dialog is shown before executing.
    pub confirm: bool,
}

/// Visual style for an admin action button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStyle {
    /// Default/neutral style.
    Default,
    /// Primary/positive action.
    Primary,
    /// Destructive/dangerous action (red).
    Danger,
}

// ── Version history ──────────────────────────────────────────────────

/// A single entry in the admin History pane for an opted-in model.
///
/// Mirrors [`autumn_web::VersionEntry`] but is decoupled from the runtime
/// type so the admin plugin has no compile-time dependency on the DB feature.
#[derive(Debug, Clone)]
pub struct AdminHistoryEntry {
    /// Auto-incrementing PK in the history table.
    pub id: i64,
    /// Actor identifier (`user_id` or `"system"`).
    pub actor: String,
    /// Operation: `"insert"`, `"update"`, or `"delete"`.
    pub op: String,
    /// Request / trace correlation ID.
    pub request_id: Option<String>,
    /// Column-level changes, serialized as JSON for template rendering.
    pub changes: Vec<Value>,
    /// When this entry was recorded.
    pub recorded_at: DateTime<Utc>,
}

/// Paginated history result for the admin History pane.
#[derive(Debug, Clone)]
pub struct AdminHistoryPage {
    pub entries: Vec<AdminHistoryEntry>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
}

impl AdminHistoryPage {
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
}

// ── The core trait ──────────────────────────────────────────────────

/// Type alias for the boxed future returned by async `AdminModel` methods.
pub type AdminFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdminError>> + Send + 'a>>;

/// Error type for admin operations.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Record not found")]
    NotFound,

    #[error("Validation failed: {0}")]
    Validation(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("{0}")]
    Other(String),
}

// ── CSV import types ────────────────────────────────────────────────

/// Mode for an admin CSV import operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CsvImportMode {
    /// Insert every row as a new record.
    #[default]
    Insert,
    /// Dry-run: validate rows but do not write anything.
    DryRun,
}

impl CsvImportMode {
    /// Parse from a form field value.
    ///
    /// Returns `None` for unrecognised values so callers can reject them with a
    /// 400 instead of silently falling back to the destructive `Insert` path.
    /// An *absent* mode field should default to `Insert` without calling this.
    #[must_use]
    pub fn from_form_value(s: &str) -> Option<Self> {
        match s {
            "insert" | "Insert" => Some(Self::Insert),
            "dry_run" | "DryRun" | "dry-run" => Some(Self::DryRun),
            _ => None,
        }
    }
}

/// The outcome of processing a single imported CSV row.
#[derive(Debug)]
pub enum AdminImportRowResult {
    /// The row was inserted as a new record.
    Inserted,
    /// The row updated an existing record.
    Updated,
    /// The row was intentionally skipped.
    Skipped,
    /// A row-level error (no specific column).
    RowError(String),
    /// A field-level error with a column name.
    FieldError { column: String, message: String },
}

/// Summary of a completed (or dry-run) admin CSV import.
#[derive(Debug, Default, Clone)]
pub struct AdminImportReport {
    pub inserted: u64,
    pub updated: u64,
    pub skipped: u64,
    pub errors: Vec<AdminImportError>,
}

/// A single parse/validation error from an admin CSV import.
#[derive(Debug, Clone)]
pub struct AdminImportError {
    /// 1-based CSV line number (header = line 1).
    pub line: u64,
    /// Column name, if known.
    pub column: Option<String>,
    /// Human-readable description.
    pub message: String,
}

/// The core trait that enables a model to be managed in the admin panel.
///
/// Implementors provide field metadata, CRUD operations, and display
/// configuration. The admin plugin uses this trait to generate all views
/// dynamically at runtime.
///
/// # Design notes
///
/// All data flows through `serde_json::Value` to keep the trait object-safe.
/// The admin panel doesn't need to know concrete types — it renders fields
/// based on [`AdminField`] metadata and passes values as JSON.
pub trait AdminModel: Send + Sync + 'static {
    /// URL-safe slug for this model (e.g., "projects", "tickets").
    /// Used in admin URLs: `/admin/projects/`, `/admin/projects/42/`.
    fn slug(&self) -> &'static str;

    /// Human-readable singular name (e.g., "Project").
    fn display_name(&self) -> &'static str;

    /// Human-readable plural name (e.g., "Projects").
    fn display_name_plural(&self) -> &'static str;

    /// Field metadata for this model.
    fn fields(&self) -> Vec<AdminField>;

    /// Available bulk actions.
    ///
    /// Defaults to "Delete selected". When `supports_soft_delete()` returns
    /// `true`, also includes "Restore selected" and "Purge selected" so that
    /// the admin route validator can dispatch those action names.
    fn actions(&self) -> Vec<AdminAction> {
        let mut acts = vec![AdminAction {
            name: "delete",
            label: "Delete selected".to_owned(),
            style: ActionStyle::Danger,
            confirm: true,
        }];
        if self.supports_soft_delete() {
            acts.push(AdminAction {
                name: "restore",
                label: "Restore selected".to_owned(),
                style: ActionStyle::Default,
                confirm: false,
            });
            acts.push(AdminAction {
                name: "purge",
                label: "Purge selected".to_owned(),
                style: ActionStyle::Danger,
                confirm: true,
            });
        }
        acts
    }

    // ── CRUD operations ─────────────────────────────────────────

    /// List records with pagination, search, sort, and filters.
    fn list(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        params: ListParams,
    ) -> AdminFuture<'_, ListResult>;

    /// Get a single record by ID.
    fn get(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        id: i64,
    ) -> AdminFuture<'_, Option<Value>>;

    /// Create a new record from form data.
    fn create(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        data: Value,
    ) -> AdminFuture<'_, Value>;

    /// Update an existing record.
    fn update(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        id: i64,
        data: Value,
    ) -> AdminFuture<'_, Value>;

    /// Delete a record by ID.
    fn delete(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        id: i64,
    ) -> AdminFuture<'_, ()>;

    /// Whether this model supports soft-delete (defaults to `false`).
    ///
    /// When `true`, the admin panel shows a Trash tab with restore/purge.
    fn supports_soft_delete(&self) -> bool {
        false
    }

    /// Restore a soft-deleted record (set `deleted_at = NULL`).
    ///
    /// The default returns `AdminError::Other` when `supports_soft_delete()` is
    /// `false`, so models that opt in must override this method.
    fn restore<'a>(
        &'a self,
        _pool: &'a diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        _id: i64,
    ) -> AdminFuture<'a, ()> {
        Box::pin(async move {
            Err(AdminError::Other(
                "this model does not support soft delete; \
                 override supports_soft_delete() to return true and implement restore()"
                    .to_owned(),
            ))
        })
    }

    /// Permanently delete (purge) a soft-deleted record.
    ///
    /// The default returns `AdminError::Other` when `supports_soft_delete()` is
    /// `false`.
    fn purge<'a>(
        &'a self,
        _pool: &'a diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        _id: i64,
    ) -> AdminFuture<'a, ()> {
        Box::pin(async move {
            Err(AdminError::Other(
                "this model does not support soft delete; \
                 override supports_soft_delete() to return true and implement purge()"
                    .to_owned(),
            ))
        })
    }

    /// List soft-deleted records (where `deleted_at IS NOT NULL`).
    ///
    /// The default returns `AdminError::Other` when `supports_soft_delete()` is
    /// `false`.
    fn list_deleted<'a>(
        &'a self,
        _pool: &'a diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        _params: ListParams,
    ) -> AdminFuture<'a, ListResult> {
        Box::pin(async move {
            Err(AdminError::Other(
                "this model does not support soft delete; \
                 override supports_soft_delete() to return true and implement list_deleted()"
                    .to_owned(),
            ))
        })
    }

    /// Execute a bulk action on the given IDs.
    fn execute_action(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        action: &str,
        ids: Vec<i64>,
    ) -> AdminFuture<'_, u64> {
        // Default implementation: dispatch the built-in `"delete"`, `"restore"`,
        // and `"purge"` actions. Any other action name returns an error so it
        // doesn't silently no-op — overriders that declare custom actions must
        // implement them here.
        //
        // We clone the pool (deadpool::Pool is Arc-backed, cheap) so the
        // returned future only borrows from `&self` and avoids the
        // lifetime mismatch between `&self` and `&pool` that would
        // otherwise show up in the trait's elided `'_` return signature.
        let action = action.to_owned();
        let pool = pool.clone();
        Box::pin(async move {
            match action.as_str() {
                "delete" => {
                    let mut count: u64 = 0;
                    for id in ids {
                        self.delete(&pool, id).await?;
                        count += 1;
                    }
                    Ok(count)
                }
                "restore" => {
                    let mut count: u64 = 0;
                    for id in ids {
                        self.restore(&pool, id).await?;
                        count += 1;
                    }
                    Ok(count)
                }
                "purge" => {
                    let mut count: u64 = 0;
                    for id in ids {
                        self.purge(&pool, id).await?;
                        count += 1;
                    }
                    Ok(count)
                }
                other => Err(AdminError::Other(format!(
                    "unhandled bulk action '{other}'; \
                     override AdminModel::execute_action to support it"
                ))),
            }
        })
    }

    /// Return a display string for a record (used in breadcrumbs, titles).
    ///
    /// Defaults to `"ModelName #id"` (or `"ModelName <no id>"` when the
    /// record has no numeric `id`).
    fn record_display(&self, record: &Value) -> String {
        record_id(record).map_or_else(
            || format!("{} <no id>", self.display_name()),
            |id| format!("{} #{id}", self.display_name()),
        )
    }

    /// Records per page in the list view. Override to taste.
    fn per_page(&self) -> u64 {
        25
    }

    /// Count records matching a list query (defaults to `list(..., per_page: 0).total`).
    ///
    /// Override if the backend can count without materializing records.
    fn count(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    ) -> AdminFuture<'_, u64> {
        let params = ListParams {
            page: 1,
            per_page: 0,
            ..Default::default()
        };
        let fut = self.list(pool, params);
        Box::pin(async move { fut.await.map(|r| r.total) })
    }

    // ── CSV import / export ─────────────────────────────────────────

    /// Whether this model exposes a `GET /admin/{slug}/export.csv` link.
    ///
    /// Defaults to `false` — export must be explicitly opted into to avoid
    /// silently exposing model data on upgrade. Override to `true` to enable
    /// the CSV export button and route.
    fn supports_csv_export(&self) -> bool {
        false
    }

    /// Column names written to the CSV header row during export.
    ///
    /// Defaults to the ordered names of all non-password, non-hidden fields
    /// declared in [`fields`]. Override to add computed columns (e.g. a
    /// joined display value) or to omit sensitive columns (PII redaction).
    ///
    /// # PII redaction strategy
    ///
    /// To redact a column: remove it from this list. To include a placeholder
    /// instead of the real value, add the column here and override
    /// [`AdminModel::csv_export_row`] to return `"[REDACTED]"` for that key.
    ///
    /// [`fields`]: AdminModel::fields
    fn csv_export_columns(&self) -> Vec<&'static str> {
        self.fields()
            .into_iter()
            .filter(|f| {
                !matches!(f.kind, AdminFieldKind::Password | AdminFieldKind::Hidden) && !f.encrypted
            })
            .map(|f| f.name)
            .collect()
    }

    /// Serialize a single record (as returned by [`list`]) into an ordered
    /// list of string values for CSV export.
    ///
    /// The default implementation extracts values for each column in
    /// [`csv_export_columns`] from the JSON record. Override to add computed
    /// columns (joined values, formatted timestamps, etc.).
    ///
    /// [`list`]: AdminModel::list
    /// [`csv_export_columns`]: AdminModel::csv_export_columns
    fn csv_export_row(&self, columns: &[&str], record: &Value) -> Vec<String> {
        columns
            .iter()
            .map(|col| {
                record
                    .get(*col)
                    .map(|v| match v {
                        Value::String(s) => escape_csv_formula(s),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Whether this model accepts `POST /admin/{slug}/import` CSV uploads.
    ///
    /// Defaults to `false` — import must be explicitly opted into because it
    /// performs bulk writes. Override to `true` and implement
    /// [`import_csv_row`] to enable the import UI.
    ///
    /// [`import_csv_row`]: AdminModel::import_csv_row
    fn supports_csv_import(&self) -> bool {
        false
    }

    /// Process a single CSV row during an admin import.
    ///
    /// Receives the **1-based line number** in the CSV file and a map of
    /// `column_name → value` (all strings; coerce as needed). Return the
    /// appropriate [`AdminImportRowResult`].
    ///
    /// The default always returns `AdminImportRowResult::Skipped` — models
    /// that set [`supports_csv_import`] to `true` should override this.
    ///
    /// [`supports_csv_import`]: AdminModel::supports_csv_import
    fn import_csv_row<'a>(
        &'a self,
        _pool: &'a diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        _line: u64,
        _row: std::collections::HashMap<String, String>,
        _mode: CsvImportMode,
    ) -> AdminFuture<'a, AdminImportRowResult> {
        Box::pin(async move { Ok(AdminImportRowResult::Skipped) })
    }

    /// Whether this model has automatic record version history enabled.
    ///
    /// When `true`, the admin panel renders a **History** affordance on the
    /// detail page and serves `/{slug}/{id}/history`.
    fn has_history(&self) -> bool {
        false
    }

    /// Retrieve a paginated page of version history entries for a record.
    ///
    /// The default implementation returns [`AdminError::Other`] so models
    /// that do not opt in get a clear error instead of a silent no-op.
    fn get_history<'a>(
        &'a self,
        _pool: &'a diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        _record_id: i64,
        _page: u64,
        _per_page: u64,
    ) -> AdminFuture<'a, AdminHistoryPage> {
        Box::pin(async move {
            Err(AdminError::Other(
                "this model does not have version history enabled; \
                 use #[repository(Model, versioned = true)] to opt in"
                    .to_owned(),
            ))
        })
    }
}

// ── VersionPage → AdminHistoryPage conversion ──────────────────────

impl From<autumn_web::version_history::VersionEntry> for AdminHistoryEntry {
    fn from(e: autumn_web::version_history::VersionEntry) -> Self {
        Self {
            id: e.id,
            actor: e.actor,
            op: e.op.to_string(),
            request_id: e.request_id,
            changes: e
                .changes
                .into_iter()
                .map(|c| serde_json::to_value(&c).unwrap_or(serde_json::Value::Null))
                .collect(),
            recorded_at: e.recorded_at,
        }
    }
}

impl From<autumn_web::version_history::VersionPage> for AdminHistoryPage {
    /// Convert a [`autumn_web::version_history::VersionPage`] returned by a
    /// versioned repository's `version_history()` method into an
    /// [`AdminHistoryPage`] for the admin panel.
    ///
    /// ```rust,ignore
    /// fn get_history<'a>(
    ///     &'a self, pool: &'a Pool<AsyncPgConnection>,
    ///     record_id: i64, page: u64, per_page: u64,
    /// ) -> AdminFuture<'a, AdminHistoryPage> {
    ///     let pool = pool.clone();
    ///     Box::pin(async move {
    ///         let repo = PgPostRepository::from_pool(pool);
    ///         let filter = autumn_web::VersionFilter { page, per_page, ..Default::default() };
    ///         repo.version_history(record_id, filter).await
    ///             .map(AdminHistoryPage::from)
    ///             .map_err(|e| AdminError::Database(e.to_string()))
    ///     })
    /// }
    /// ```
    fn from(vp: autumn_web::version_history::VersionPage) -> Self {
        Self {
            entries: vp
                .entries
                .into_iter()
                .map(AdminHistoryEntry::from)
                .collect(),
            total: vp.total,
            page: vp.page,
            per_page: vp.per_page,
        }
    }
}

/// Extract the `"id"` field of a record as `i64`.
///
/// Returns `None` when the field is missing or non-numeric. Callers in
/// mutation paths should treat `None` as an error (the model returned a
/// payload without a routable identifier); display contexts may fall back
/// to a placeholder like `"#?"`.
#[must_use]
pub fn record_id(record: &Value) -> Option<i64> {
    record.get("id").and_then(Value::as_i64)
}

// ── Query parameters ────────────────────────────────────────────────

/// Parameters for a list query.
#[derive(Debug, Clone, Default)]
pub struct ListParams {
    /// Page number (1-indexed).
    pub page: u64,
    /// Records per page.
    pub per_page: u64,
    /// Full-text search query.
    pub search: Option<String>,
    /// Column to sort by.
    pub sort_by: Option<String>,
    /// Sort direction.
    pub sort_dir: SortDirection,
    /// Active filters (`field_name` → value).
    pub filters: Vec<(String, String)>,
}

/// Sort direction for list queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    #[default]
    Asc,
    Desc,
}

impl SortDirection {
    /// URL-friendly representation (`"asc"` / `"desc"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    /// The opposite direction (used to flip sort on re-click).
    #[must_use]
    pub const fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

/// Result of a list query, containing records and pagination metadata.
#[derive(Debug, Clone)]
pub struct ListResult {
    /// The records for the current page (as JSON objects).
    pub records: Vec<Value>,
    /// Total number of records matching the query (for pagination).
    pub total: u64,
    /// Current page number.
    pub page: u64,
    /// Records per page.
    pub per_page: u64,
}

impl ListResult {
    /// Total number of pages.
    #[must_use]
    pub const fn total_pages(&self) -> u64 {
        if self.per_page == 0 {
            return 0;
        }
        self.total.div_ceil(self.per_page)
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Prefix a CSV cell value with `'` when it starts with a formula-triggering
/// character (`=`, `+`, `-`, `@`, tab, or CR) to prevent spreadsheet formula
/// injection.
fn escape_csv_formula(s: &str) -> String {
    match s.bytes().next() {
        Some(b'=' | b'+' | b'-' | b'@' | b'\t' | b'\r') => {
            let mut out = String::with_capacity(s.len() + 1);
            out.push('\'');
            out.push_str(s);
            out
        }
        _ => s.to_owned(),
    }
}

/// Convert a `snake_case` field name to a human-readable label.
///
/// `"created_at"` → `"Created At"`, `"user_id"` → `"User Id"`.
fn humanize_field_name(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for (i, word) in name.split('_').enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            s.extend(c.to_uppercase());
            s.extend(chars);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test fixture: an `AdminModel` whose `delete` records the id it was
    /// asked to delete. Doesn't override `execute_action` — that's the
    /// behaviour under test.
    struct DeletingModel {
        deleted: Mutex<Vec<i64>>,
        fail_on: Option<i64>,
    }

    impl AdminModel for DeletingModel {
        fn slug(&self) -> &'static str {
            "tracked"
        }
        fn display_name(&self) -> &'static str {
            "Tracked"
        }
        fn display_name_plural(&self) -> &'static str {
            "Tracked"
        }
        fn fields(&self) -> Vec<AdminField> {
            vec![]
        }
        fn list(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _params: ListParams,
        ) -> AdminFuture<'_, ListResult> {
            Box::pin(async {
                Ok(ListResult {
                    records: vec![],
                    total: 0,
                    page: 1,
                    per_page: 25,
                })
            })
        }
        fn get(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _id: i64,
        ) -> AdminFuture<'_, Option<Value>> {
            Box::pin(async { Ok(None) })
        }
        fn create(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            data: Value,
        ) -> AdminFuture<'_, Value> {
            Box::pin(async move { Ok(data) })
        }
        fn update(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _id: i64,
            data: Value,
        ) -> AdminFuture<'_, Value> {
            Box::pin(async move { Ok(data) })
        }
        fn delete(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            id: i64,
        ) -> AdminFuture<'_, ()> {
            let deleted = &self.deleted;
            let fail_on = self.fail_on;
            Box::pin(async move {
                if Some(id) == fail_on {
                    return Err(AdminError::Database("simulated failure".into()));
                }
                deleted.lock().unwrap().push(id);
                Ok(())
            })
        }
    }

    /// Test fixture: an `AdminModel` that supports soft delete and records
    /// which ids were restored/purged. Overrides `supports_soft_delete()` and
    /// the three soft-delete methods.
    #[derive(Default)]
    struct SoftDeleteModel {
        restored: Mutex<Vec<i64>>,
        purged: Mutex<Vec<i64>>,
    }

    impl AdminModel for SoftDeleteModel {
        fn slug(&self) -> &'static str {
            "soft"
        }
        fn display_name(&self) -> &'static str {
            "Soft"
        }
        fn display_name_plural(&self) -> &'static str {
            "Softs"
        }
        fn fields(&self) -> Vec<AdminField> {
            vec![]
        }
        fn list(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _params: ListParams,
        ) -> AdminFuture<'_, ListResult> {
            Box::pin(async {
                Ok(ListResult {
                    records: vec![],
                    total: 0,
                    page: 1,
                    per_page: 25,
                })
            })
        }
        fn get(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _id: i64,
        ) -> AdminFuture<'_, Option<Value>> {
            Box::pin(async { Ok(None) })
        }
        fn create(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            data: Value,
        ) -> AdminFuture<'_, Value> {
            Box::pin(async move { Ok(data) })
        }
        fn update(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _id: i64,
            data: Value,
        ) -> AdminFuture<'_, Value> {
            Box::pin(async move { Ok(data) })
        }
        fn delete(
            &self,
            _pool: &diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _id: i64,
        ) -> AdminFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn supports_soft_delete(&self) -> bool {
            true
        }
        fn restore<'a>(
            &'a self,
            _pool: &'a diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            id: i64,
        ) -> AdminFuture<'a, ()> {
            Box::pin(async move {
                self.restored.lock().unwrap().push(id);
                Ok(())
            })
        }
        fn purge<'a>(
            &'a self,
            _pool: &'a diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            id: i64,
        ) -> AdminFuture<'a, ()> {
            Box::pin(async move {
                self.purged.lock().unwrap().push(id);
                Ok(())
            })
        }
        fn list_deleted<'a>(
            &'a self,
            _pool: &'a diesel_async::pooled_connection::deadpool::Pool<
                diesel_async::AsyncPgConnection,
            >,
            _params: ListParams,
        ) -> AdminFuture<'a, ListResult> {
            Box::pin(async {
                Ok(ListResult {
                    records: vec![],
                    total: 0,
                    page: 1,
                    per_page: 25,
                })
            })
        }
    }

    /// Build a `Pool` whose manager would fail to connect — the test models
    /// never call `pool.get()`, so the pool itself just sits unused.
    fn dummy_pool()
    -> diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection> {
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use diesel_async::pooled_connection::deadpool::Pool;
        let mgr = AsyncDieselConnectionManager::<diesel_async::AsyncPgConnection>::new(
            "postgresql://test",
        );
        Pool::builder(mgr).build().expect("build pool")
    }

    #[tokio::test]
    async fn default_execute_action_delete_invokes_delete_for_each_id() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let pool = dummy_pool();
        let count = model
            .execute_action(&pool, "delete", vec![10, 20, 30])
            .await
            .expect("default delete should succeed");
        assert_eq!(count, 3);
        assert_eq!(*model.deleted.lock().unwrap(), vec![10, 20, 30]);
    }

    #[tokio::test]
    async fn default_execute_action_delete_aborts_on_first_failure() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: Some(20),
        };
        let pool = dummy_pool();
        let err = model
            .execute_action(&pool, "delete", vec![10, 20, 30])
            .await
            .expect_err("delete should propagate failure");
        assert!(matches!(err, AdminError::Database(_)));
        // Only the pre-failure id was committed.
        assert_eq!(*model.deleted.lock().unwrap(), vec![10]);
    }

    #[tokio::test]
    async fn default_execute_action_rejects_unknown_action() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let pool = dummy_pool();
        let err = model
            .execute_action(&pool, "promote", vec![1])
            .await
            .expect_err("unknown actions must error, not silently no-op");
        assert!(
            matches!(err, AdminError::Other(msg) if msg.contains("promote")),
            "error should name the unhandled action"
        );
        assert!(model.deleted.lock().unwrap().is_empty());
    }

    #[test]
    fn humanize_converts_snake_case() {
        assert_eq!(humanize_field_name("created_at"), "Created At");
        assert_eq!(humanize_field_name("user_id"), "User Id");
        assert_eq!(humanize_field_name("name"), "Name");
        assert_eq!(humanize_field_name(""), "");
    }

    #[test]
    fn list_result_total_pages() {
        let result = ListResult {
            records: vec![],
            total: 25,
            page: 1,
            per_page: 10,
        };
        assert_eq!(result.total_pages(), 3);
    }

    #[test]
    fn list_result_total_pages_exact() {
        let result = ListResult {
            records: vec![],
            total: 20,
            page: 1,
            per_page: 10,
        };
        assert_eq!(result.total_pages(), 2);
    }

    #[test]
    fn list_result_total_pages_zero_per_page() {
        let result = ListResult {
            records: vec![],
            total: 20,
            page: 1,
            per_page: 0,
        };
        assert_eq!(result.total_pages(), 0);
    }

    #[test]
    fn admin_field_builder() {
        let field = AdminField::new("email", AdminFieldKind::Text)
            .label("Email Address")
            .searchable()
            .filterable()
            .optional();

        assert_eq!(field.name, "email");
        assert_eq!(field.label, "Email Address");
        assert!(field.searchable);
        assert!(field.filterable);
        assert!(!field.required);
        assert!(field.editable);
    }

    #[test]
    fn record_id_extracts_numeric_id() {
        assert_eq!(record_id(&serde_json::json!({"id": 42})), Some(42));
    }

    #[test]
    fn record_id_returns_none_for_missing_or_non_numeric() {
        assert_eq!(record_id(&serde_json::json!({})), None);
        assert_eq!(record_id(&serde_json::json!({"id": null})), None);
        assert_eq!(record_id(&serde_json::json!({"id": "abc"})), None);
        // Floats aren't valid IDs either — only integers.
        assert_eq!(record_id(&serde_json::json!({"id": 1.5})), None);
    }

    #[test]
    fn hidden_fields_default_to_not_editable() {
        // AdminFieldKind::Hidden is documented as "not editable". Ensure the
        // default matches the contract so admins who skip `.readonly()` still
        // get safe behaviour.
        let hidden = AdminField::new("owner_id", AdminFieldKind::Hidden);
        assert!(
            !hidden.editable,
            "Hidden fields must default to editable=false"
        );

        // Other kinds remain editable by default.
        let text = AdminField::new("name", AdminFieldKind::Text);
        assert!(text.editable);
    }

    // ── Soft-delete admin support (issue #689) ────────────────────

    #[test]
    fn admin_model_supports_soft_delete_defaults_to_false() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        assert!(
            !model.supports_soft_delete(),
            "AdminModel::supports_soft_delete() must default to false"
        );
    }

    #[tokio::test]
    async fn admin_model_restore_returns_error_when_soft_delete_not_supported() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let pool = dummy_pool();
        let err = model
            .restore(&pool, 1)
            .await
            .expect_err("restore must error when supports_soft_delete() is false");
        assert!(
            matches!(err, AdminError::Other(_)),
            "restore on non-soft-delete model must return AdminError::Other: {err:?}"
        );
    }

    #[tokio::test]
    async fn admin_model_purge_returns_error_when_soft_delete_not_supported() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let pool = dummy_pool();
        let err = model
            .purge(&pool, 1)
            .await
            .expect_err("purge must error when supports_soft_delete() is false");
        assert!(
            matches!(err, AdminError::Other(_)),
            "purge on non-soft-delete model must return AdminError::Other: {err:?}"
        );
    }

    #[tokio::test]
    async fn admin_model_list_deleted_returns_error_when_soft_delete_not_supported() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let pool = dummy_pool();
        let params = ListParams {
            page: 1,
            per_page: 25,
            ..Default::default()
        };
        let err = model
            .list_deleted(&pool, params)
            .await
            .expect_err("list_deleted must error when supports_soft_delete() is false");
        assert!(
            matches!(err, AdminError::Other(_)),
            "list_deleted on non-soft-delete model must return AdminError::Other: {err:?}"
        );
    }

    #[test]
    fn default_actions_returns_only_delete_when_soft_delete_not_supported() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let acts = model.actions();
        assert_eq!(
            acts.len(),
            1,
            "default model must advertise exactly one action"
        );
        assert_eq!(acts[0].name, "delete");
    }

    #[test]
    fn actions_includes_restore_and_purge_when_soft_delete_supported() {
        let model = SoftDeleteModel::default();
        let acts = model.actions();
        let names: Vec<&str> = acts.iter().map(|a| a.name).collect();
        assert!(
            names.contains(&"restore"),
            "soft-delete model must advertise restore action; got: {names:?}"
        );
        assert!(
            names.contains(&"purge"),
            "soft-delete model must advertise purge action; got: {names:?}"
        );
    }

    #[tokio::test]
    async fn execute_action_restore_dispatches_to_restore_method() {
        let model = SoftDeleteModel::default();
        let pool = dummy_pool();
        let count = model
            .execute_action(&pool, "restore", vec![10, 20])
            .await
            .expect("restore action should succeed on soft-delete model");
        assert_eq!(
            count, 2,
            "restore action must return count of restored records"
        );
        assert_eq!(*model.restored.lock().unwrap(), vec![10, 20]);
    }

    #[tokio::test]
    async fn execute_action_purge_dispatches_to_purge_method() {
        let model = SoftDeleteModel::default();
        let pool = dummy_pool();
        let count = model
            .execute_action(&pool, "purge", vec![5])
            .await
            .expect("purge action should succeed on soft-delete model");
        assert_eq!(count, 1, "purge action must return count of purged records");
        assert_eq!(*model.purged.lock().unwrap(), vec![5]);
    }

    // ── Version history tests (issue #700) ────────────────────────

    #[test]
    fn admin_model_has_history_defaults_to_false() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        assert!(
            !model.has_history(),
            "AdminModel::has_history() must default to false"
        );
    }

    #[tokio::test]
    async fn admin_model_get_history_returns_error_when_not_opted_in() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let pool = dummy_pool();
        let err = model
            .get_history(&pool, 42, 1, 25)
            .await
            .expect_err("get_history must error when has_history() is false");
        assert!(
            matches!(err, AdminError::Other(_)),
            "get_history on non-versioned model must return AdminError::Other: {err:?}"
        );
    }

    #[test]
    fn admin_history_page_total_pages() {
        let page = AdminHistoryPage {
            entries: vec![],
            total: 51,
            page: 1,
            per_page: 25,
        };
        assert_eq!(page.total_pages(), 3);
    }

    #[test]
    fn admin_history_page_has_next_page() {
        let page = AdminHistoryPage {
            entries: vec![],
            total: 50,
            page: 1,
            per_page: 25,
        };
        assert!(page.has_next_page());
    }

    #[test]
    fn admin_history_page_no_next_on_last() {
        let page = AdminHistoryPage {
            entries: vec![],
            total: 50,
            page: 2,
            per_page: 25,
        };
        assert!(!page.has_next_page());
    }

    #[test]
    fn admin_history_page_zero_per_page() {
        let page = AdminHistoryPage {
            entries: vec![],
            total: 10,
            page: 1,
            per_page: 0,
        };
        assert_eq!(page.total_pages(), 0);
    }

    // ── SortDirection and AdminField builder coverage ─────────────

    #[test]
    fn sort_direction_as_str_returns_correct_values() {
        assert_eq!(SortDirection::Asc.as_str(), "asc");
        assert_eq!(SortDirection::Desc.as_str(), "desc");
    }

    #[test]
    fn sort_direction_flipped_returns_opposite() {
        assert_eq!(SortDirection::Asc.flipped(), SortDirection::Desc);
        assert_eq!(SortDirection::Desc.flipped(), SortDirection::Asc);
    }

    #[test]
    fn admin_field_readonly_sets_editable_false() {
        let field = AdminField::new("created_at", AdminFieldKind::DateTime).readonly();
        assert!(!field.editable, "readonly() must set editable = false");
    }

    #[test]
    fn admin_field_hide_from_list_sets_list_display_false() {
        let field = AdminField::new("internal_token", AdminFieldKind::Text).hide_from_list();
        assert!(
            !field.list_display,
            "hide_from_list() must set list_display = false"
        );
    }

    #[test]
    fn admin_model_record_display_includes_display_name_and_id() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let record = serde_json::json!({"id": 7, "name": "foo"});
        assert_eq!(model.record_display(&record), "Tracked #7");
    }

    #[test]
    fn admin_model_record_display_placeholder_when_no_id() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let record = serde_json::json!({"name": "bar"});
        assert_eq!(model.record_display(&record), "Tracked <no id>");
    }

    #[test]
    fn admin_model_per_page_default_is_25() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        assert_eq!(model.per_page(), 25);
    }

    #[test]
    fn version_page_converts_to_admin_history_page() {
        use autumn_web::version_history::{ColumnChange, VersionEntry, VersionOp, VersionPage};
        use chrono::Utc;

        let entry = VersionEntry {
            id: 1,
            table_name: "posts".to_owned(),
            record_id: 42,
            op: VersionOp::Update,
            actor: "admin".to_owned(),
            request_id: Some("req-1".to_owned()),
            changes: vec![ColumnChange::new(
                "title",
                Some(serde_json::json!("old")),
                Some(serde_json::json!("new")),
            )],
            recorded_at: Utc::now(),
        };
        let vp = VersionPage {
            entries: vec![entry],
            total: 1,
            page: 1,
            per_page: 25,
        };

        let ap = AdminHistoryPage::from(vp);
        assert_eq!(ap.total, 1);
        assert_eq!(ap.page, 1);
        assert_eq!(ap.per_page, 25);
        assert_eq!(ap.entries.len(), 1);
        let e = &ap.entries[0];
        assert_eq!(e.id, 1);
        assert_eq!(e.actor, "admin");
        assert_eq!(e.op, "update");
        assert_eq!(e.request_id.as_deref(), Some("req-1"));
        assert_eq!(e.changes.len(), 1);
    }

    #[test]
    fn version_entry_converts_to_admin_history_entry() {
        use autumn_web::version_history::{ColumnChange, VersionEntry, VersionOp};
        use chrono::Utc;

        let entry = VersionEntry {
            id: 7,
            table_name: "users".to_owned(),
            record_id: 3,
            op: VersionOp::Delete,
            actor: "system".to_owned(),
            request_id: None,
            changes: vec![ColumnChange::sensitive("password_digest")],
            recorded_at: Utc::now(),
        };

        let admin_entry = AdminHistoryEntry::from(entry);
        assert_eq!(admin_entry.id, 7);
        assert_eq!(admin_entry.actor, "system");
        assert_eq!(admin_entry.op, "delete");
        assert!(admin_entry.request_id.is_none());
        assert_eq!(admin_entry.changes.len(), 1);
    }

    // ── CsvImportMode ────────────────────────────────────────────────

    #[test]
    fn csv_import_mode_from_form_value_recognises_insert() {
        assert_eq!(
            CsvImportMode::from_form_value("insert"),
            Some(CsvImportMode::Insert)
        );
        assert_eq!(
            CsvImportMode::from_form_value("Insert"),
            Some(CsvImportMode::Insert)
        );
    }

    #[test]
    fn csv_import_mode_from_form_value_recognises_dry_run() {
        assert_eq!(
            CsvImportMode::from_form_value("dry_run"),
            Some(CsvImportMode::DryRun)
        );
        assert_eq!(
            CsvImportMode::from_form_value("DryRun"),
            Some(CsvImportMode::DryRun)
        );
        assert_eq!(
            CsvImportMode::from_form_value("dry-run"),
            Some(CsvImportMode::DryRun)
        );
    }

    #[test]
    fn csv_import_mode_from_form_value_rejects_unknown() {
        assert_eq!(CsvImportMode::from_form_value("upsert"), None);
        assert_eq!(CsvImportMode::from_form_value(""), None);
        assert_eq!(CsvImportMode::from_form_value("INSERT"), None);
        assert_eq!(CsvImportMode::from_form_value("DRY_RUN"), None);
    }

    #[test]
    fn csv_import_mode_default_is_insert() {
        assert_eq!(CsvImportMode::default(), CsvImportMode::Insert);
    }

    // ── escape_csv_formula ──────────────────────────────────────────

    #[test]
    fn escape_csv_formula_prefixes_equals_sign() {
        assert_eq!(escape_csv_formula("=SUM(A1)"), "'=SUM(A1)");
    }

    #[test]
    fn escape_csv_formula_prefixes_plus_and_minus_and_at() {
        assert_eq!(escape_csv_formula("+cmd"), "'+cmd");
        assert_eq!(escape_csv_formula("-1+1"), "'-1+1");
        assert_eq!(escape_csv_formula("@A1"), "'@A1");
    }

    #[test]
    fn escape_csv_formula_prefixes_tab_and_cr() {
        assert_eq!(escape_csv_formula("\thello"), "'\thello");
        assert_eq!(escape_csv_formula("\rhello"), "'\rhello");
    }

    #[test]
    fn escape_csv_formula_leaves_normal_strings_unchanged() {
        assert_eq!(escape_csv_formula("hello world"), "hello world");
        assert_eq!(escape_csv_formula("123"), "123");
        assert_eq!(escape_csv_formula(""), "");
        assert_eq!(escape_csv_formula("normal,value"), "normal,value");
    }

    // ── AdminModel CSV defaults ──────────────────────────────────────

    #[test]
    fn admin_model_supports_csv_export_defaults_to_false() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        assert!(
            !model.supports_csv_export(),
            "supports_csv_export must default to false to require explicit opt-in"
        );
    }

    #[test]
    fn admin_model_supports_csv_import_defaults_to_false() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        assert!(
            !model.supports_csv_import(),
            "supports_csv_import must default to false"
        );
    }

    #[test]
    fn csv_export_row_extracts_columns_and_escapes_formulas() {
        let model = DeletingModel {
            deleted: Mutex::new(vec![]),
            fail_on: None,
        };
        let record = serde_json::json!({
            "id": 1,
            "name": "Alice",
            "formula": "=EVIL()",
            "amount": 42.5,
            "active": true,
            "notes": null,
        });
        let columns = &[
            "id", "name", "formula", "amount", "active", "notes", "missing",
        ];
        let row = model.csv_export_row(columns, &record);
        assert_eq!(row[0], "1");
        assert_eq!(row[1], "Alice");
        assert_eq!(row[2], "'=EVIL()", "formula-leading value must be escaped");
        assert_eq!(row[3], "42.5");
        assert_eq!(row[4], "true");
        assert_eq!(row[5], "", "null becomes empty string");
        assert_eq!(row[6], "", "missing column becomes empty string");
    }
}

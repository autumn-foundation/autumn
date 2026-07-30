//! Engine-agnostic search vocabulary.
//!
//! Shared by `#[model] #[searchable]` and the optional `autumn-search` plugin.
//!
//! Issue #842 gave `#[model] #[searchable]` a Postgres `tsvector` column and
//! `#[repository(searchable)]` a `search()` method — "search *this table*".
//! Issue #1191 turns the same declaration into a **search subsystem**: the
//! attribute now also produces an engine-agnostic [`IndexDefinition`] and a
//! per-record [`SearchDocument`], which the optional `autumn-search` plugin
//! feeds to a pluggable `SearchBackend` (Postgres + `pgvector` today; an
//! external engine such as Meilisearch, Tantivy, or a vector store tomorrow).
//!
//! Only the *vocabulary* lives in core — the types the `#[model]` macro emits
//! and every backend speaks. Backends, indexing jobs, the query client, and
//! embedding live in the `autumn-search` crate, so an app that never installs
//! the plugin pays nothing beyond these plain data types.
//!
//! ```rust,ignore
//! #[autumn_web::model]
//! #[searchable(language = "english")]
//! pub struct Article {
//!     #[id]
//!     pub id: i64,
//!     #[searchable(weight = "A")]
//!     pub title: String,
//!     #[searchable(weight = "B", embed)]   // ← also embedded for vector search
//!     pub body: String,
//! }
//!
//! let def = Article::index_definition();   // name, language, weighted fields
//! let doc = article.search_document();     // id + extracted field values
//! ```

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate".
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]

use std::borrow::Cow;
use std::fmt;

use serde::Serialize;

use crate::repository::AutumnSearchableModel;

/// The `tsvector`-style relevance weights, most significant first.
///
/// Mirrors #842's `#[searchable(weight = "A"|"B"|"C"|"D")]` and Postgres'
/// `setweight` classes. `D` is the default for a bare `#[searchable]` field.
pub const SEARCH_WEIGHTS: [char; 4] = ['A', 'B', 'C', 'D'];

/// Relative multiplier for a weight class, used by backends that rank in Rust
/// (and as the basis for the Postgres `ts_rank_cd` weight array).
///
/// The values match the per-column bm25 weights the in-core `SQLite` FTS5 path
/// already uses (`A → 10, B → 5, C → 2, else 1`), so ranking stays consistent
/// across the in-core search and every plugin backend.
#[must_use]
pub const fn weight_factor(weight: char) -> f32 {
    match weight {
        'A' => 10.0,
        'B' => 5.0,
        'C' => 2.0,
        _ => 1.0,
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Rejection reason from [`IndexDefinition::validate`].
///
/// Index and field names are interpolated into backend queries (a Postgres
/// `json_build_object(...)` projection, a Meilisearch attribute list, …), so
/// they are validated as bare identifiers before they ever reach a backend —
/// even though they originate from developer-written attributes rather than
/// request input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InvalidIndexDefinition {
    /// Human-readable explanation.
    pub message: String,
}

impl fmt::Display for InvalidIndexDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InvalidIndexDefinition {}

/// Returns `true` when `value` is a bare SQL/engine identifier: an ASCII
/// letter or `_` followed by ASCII alphanumerics or `_`, at most 63 bytes
/// (Postgres' `NAMEDATALEN - 1`).
#[must_use]
pub fn is_valid_index_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > 63 {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ── Index definition ────────────────────────────────────────────────────────

/// One indexed field of a searchable model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SearchIndexField {
    /// Column / attribute name as declared on the model.
    pub name: &'static str,
    /// Relevance class — one of [`SEARCH_WEIGHTS`].
    pub weight: char,
}

impl SearchIndexField {
    /// Construct a field descriptor.
    #[must_use]
    pub const fn new(name: &'static str, weight: char) -> Self {
        Self { name, weight }
    }

    /// Ranking multiplier for this field's weight class.
    #[must_use]
    pub const fn factor(&self) -> f32 {
        weight_factor(self.weight)
    }
}

/// The engine-agnostic description of one model's search index.
///
/// Produced by [`SearchIndexed::index_definition`], which `#[model]` derives
/// from the `#[searchable]` attributes — never written by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct IndexDefinition {
    /// Logical index name. Defaults to the model's table name, which is what
    /// makes an index addressable by a string from a job payload or the CLI.
    pub name: &'static str,
    /// Text-search dictionary (`#[searchable(language = "…")]`, default
    /// `"simple"`). Backends that have no dictionary concept ignore it.
    pub language: &'static str,
    /// Indexed fields in declaration order, with their weight classes.
    pub fields: Cow<'static, [SearchIndexField]>,
    /// Field whose value is embedded for vector search
    /// (`#[searchable(embed)]`), if any.
    pub embed_field: Option<&'static str>,
    /// Whether the model carries a `tenant_id` column.
    ///
    /// Set by `#[model]` from the struct's own fields. A consumer **must**
    /// fail closed on a tenant-scoped index queried with no tenant context —
    /// the same posture `#[repository(tenant_scoped)]` takes, where a missing
    /// tenant is an error rather than an unscoped read.
    pub tenant_scoped: bool,
}

impl IndexDefinition {
    /// Construct a definition. Prefer [`SearchIndexed::index_definition`].
    #[must_use]
    pub const fn new(
        name: &'static str,
        language: &'static str,
        fields: &'static [SearchIndexField],
        embed_field: Option<&'static str>,
        tenant_scoped: bool,
    ) -> Self {
        Self {
            name,
            language,
            fields: Cow::Borrowed(fields),
            embed_field,
            tenant_scoped,
        }
    }

    /// Whether this index can answer vector / "find similar" queries.
    ///
    /// True exactly when the model declares a `#[searchable(embed)]` field.
    #[must_use]
    pub const fn supports_vector_search(&self) -> bool {
        self.embed_field.is_some()
    }

    /// Field names in declaration order.
    pub fn field_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.fields.iter().map(|f| f.name)
    }

    /// Whether `field` is one of this index's declared fields.
    ///
    /// The allowlist every backend must consult before interpolating a field
    /// name into a query: a `SearchFilter` predicate keyed on anything else is
    /// caller-supplied text and must never reach SQL.
    #[must_use]
    pub fn has_field(&self, field: &str) -> bool {
        self.fields.iter().any(|f| f.name == field)
    }

    /// The declared weight class for `field`, if it is indexed.
    ///
    /// Backends rank with **this** weight rather than the one on an incoming
    /// document, so a hand-built or third-party
    /// [`SearchDocument`](crate::search::SearchDocument) cannot smuggle an
    /// out-of-range weight into a query.
    #[must_use]
    pub fn weight_of(&self, field: &str) -> Option<char> {
        self.fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| f.weight)
    }

    /// Validate every identifier a backend may interpolate into a query, plus
    /// the weight classes and the embed-field reference.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidIndexDefinition`] when the index name, a field name,
    /// or the language is not a bare identifier, when a weight is outside
    /// [`SEARCH_WEIGHTS`], when there are no fields, or when `embed_field`
    /// names a field that is not indexed.
    pub fn validate(&self) -> Result<(), InvalidIndexDefinition> {
        let invalid = |message: String| Err(InvalidIndexDefinition { message });

        if !is_valid_index_identifier(self.name) {
            return invalid(format!(
                "search index name {:?} is not an identifier",
                self.name
            ));
        }
        if !is_valid_index_identifier(self.language) {
            return invalid(format!(
                "search language {:?} on index {:?} is not an identifier",
                self.language, self.name
            ));
        }
        if self.fields.is_empty() {
            return invalid(format!("search index {:?} declares no fields", self.name));
        }
        for field in self.fields.iter() {
            if !is_valid_index_identifier(field.name) {
                return invalid(format!(
                    "search field {:?} on index {:?} is not an identifier",
                    field.name, self.name
                ));
            }
            if !SEARCH_WEIGHTS.contains(&field.weight) {
                return invalid(format!(
                    "search field {:?} on index {:?} has weight {:?}, expected one of A, B, C, D",
                    field.name, self.name, field.weight
                ));
            }
        }
        if let Some(embed) = self.embed_field
            && !self.fields.iter().any(|f| f.name == embed)
        {
            return invalid(format!(
                "embed field {embed:?} on index {:?} is not one of its indexed fields",
                self.name
            ));
        }
        Ok(())
    }
}

// ── Documents ───────────────────────────────────────────────────────────────

/// One extracted field value of a [`SearchDocument`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SearchFieldValue {
    /// Column / attribute name.
    pub name: &'static str,
    /// Relevance class — one of [`SEARCH_WEIGHTS`].
    pub weight: char,
    /// The extracted text. A `None` column extracts as `""` rather than being
    /// dropped, so a document's field list always matches its index
    /// definition positionally.
    pub value: String,
}

/// A single record, extracted into the shape every backend indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SearchDocument {
    /// Index this document belongs to (the model's [`IndexDefinition::name`]).
    pub index: &'static str,
    /// Primary key of the source record.
    pub id: i64,
    /// Extracted field values, in index-definition order.
    pub fields: Vec<SearchFieldValue>,
    /// Text to embed for vector search, from the `#[searchable(embed)]` field.
    pub embed_text: Option<String>,
    /// Tenant the record belongs to, when the model is tenant-scoped. Backends
    /// must persist and filter on this so a search never crosses tenants.
    pub tenant_id: Option<String>,
}

impl SearchDocument {
    /// Construct a document for `index` / `id` with no fields.
    #[must_use]
    pub const fn new(index: &'static str, id: i64) -> Self {
        Self {
            index,
            id,
            fields: Vec::new(),
            embed_text: None,
            tenant_id: None,
        }
    }

    /// Append an extracted field value.
    #[must_use]
    pub fn with_field(
        mut self,
        name: &'static str,
        weight: char,
        value: impl Into<String>,
    ) -> Self {
        self.fields.push(SearchFieldValue {
            name,
            weight,
            value: value.into(),
        });
        self
    }

    /// Set the text to embed for vector search.
    #[must_use]
    pub fn with_embed_text(mut self, text: impl Into<String>) -> Self {
        self.embed_text = Some(text.into());
        self
    }

    /// Set the owning tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = Some(tenant_id.into());
        self
    }

    /// All field values joined by a single space, in declaration order.
    ///
    /// This is the fallback document body for backends with no per-field
    /// weighting; weighted backends read [`SearchDocument::fields`] directly.
    /// Empty values are skipped so the result never contains a double space.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for field in &self.fields {
            if field.value.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&field.value);
        }
        out
    }

    /// The value of the named field, if the document carries it.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.as_str())
    }
}

// ── Text extraction seam ────────────────────────────────────────────────────

/// Converts a model field into the text a search backend indexes.
///
/// `#[model]` calls this for every `#[searchable]` field, so the supported
/// column types are exactly the impls below. A `#[searchable]` field whose
/// type is not covered is a compile error naming this trait — implement it for
/// the newtype (or index a derived `String` field instead).
///
/// `Option<T>` extracts to `""` when `None`: an absent value must still occupy
/// its position in the document (see [`SearchDocument::fields`]).
pub trait SearchTextValue {
    /// Extract the indexable text for this value.
    fn search_text_value(&self) -> String;
}

impl SearchTextValue for String {
    fn search_text_value(&self) -> String {
        self.clone()
    }
}

impl SearchTextValue for str {
    fn search_text_value(&self) -> String {
        self.to_owned()
    }
}

impl<T: SearchTextValue + ?Sized> SearchTextValue for &T {
    fn search_text_value(&self) -> String {
        (**self).search_text_value()
    }
}

impl<T: SearchTextValue> SearchTextValue for Option<T> {
    fn search_text_value(&self) -> String {
        self.as_ref().map(T::search_text_value).unwrap_or_default()
    }
}

macro_rules! impl_search_text_value_via_display {
    ($($ty:ty),+ $(,)?) => {
        $(impl SearchTextValue for $ty {
            fn search_text_value(&self) -> String {
                ::std::string::ToString::to_string(self)
            }
        })+
    };
}

impl_search_text_value_via_display!(
    bool,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    char,
    chrono::NaiveDate,
    chrono::NaiveTime,
    chrono::NaiveDateTime,
    uuid::Uuid,
);

impl<Tz: chrono::TimeZone> SearchTextValue for chrono::DateTime<Tz>
where
    Tz::Offset: fmt::Display,
{
    fn search_text_value(&self) -> String {
        self.to_rfc3339()
    }
}

// ── The derived trait ───────────────────────────────────────────────────────

/// A model that `#[model] #[searchable]` has given a search index.
///
/// Implemented by the `#[model]` macro for every model that carries a
/// model-level `#[searchable]` attribute **and** an `i64` primary key. Never
/// implemented by hand — the attribute is the source of truth, which is what
/// makes "mark a model searchable" require zero glue.
///
/// `autumn-search` is the consumer: it registers an index per `SearchIndexed`
/// model, keeps it in sync from the record lifecycle, and queries it through
/// its `SearchBackend` trait.
pub trait SearchIndexed: AutumnSearchableModel + Send + Sync + 'static {
    /// Logical index name (the model's table name).
    const SEARCH_INDEX: &'static str;

    /// The `#[searchable(embed)]` field, if the model declares one.
    const SEARCH_EMBED_FIELD: Option<&'static str>;

    /// This model's engine-agnostic index definition.
    fn index_definition() -> IndexDefinition;

    /// Primary key of this record.
    fn search_id(&self) -> i64;

    /// Extract this record into the document a backend indexes.
    fn search_document(&self) -> SearchDocument;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[SearchIndexField] = &[
        SearchIndexField::new("title", 'A'),
        SearchIndexField::new("body", 'B'),
    ];

    fn def() -> IndexDefinition {
        IndexDefinition::new("articles", "english", FIELDS, Some("body"), false)
    }

    #[test]
    fn weight_factors_match_the_in_core_fts_ranking() {
        assert!((weight_factor('A') - 10.0).abs() < f32::EPSILON);
        assert!((weight_factor('B') - 5.0).abs() < f32::EPSILON);
        assert!((weight_factor('C') - 2.0).abs() < f32::EPSILON);
        assert!((weight_factor('D') - 1.0).abs() < f32::EPSILON);
        assert!((weight_factor('Z') - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn identifier_validation_rejects_injection_shapes() {
        assert!(is_valid_index_identifier("articles"));
        assert!(is_valid_index_identifier("_a1"));
        assert!(!is_valid_index_identifier(""));
        assert!(!is_valid_index_identifier("1articles"));
        assert!(!is_valid_index_identifier("arti cles"));
        assert!(!is_valid_index_identifier(
            "articles\"; DROP TABLE users; --"
        ));
        assert!(!is_valid_index_identifier("naïve"));
        assert!(!is_valid_index_identifier(&"a".repeat(64)));
        assert!(is_valid_index_identifier(&"a".repeat(63)));
    }

    #[test]
    fn valid_definition_passes() {
        assert!(def().validate().is_ok());
    }

    #[test]
    fn definition_rejects_a_non_identifier_name() {
        let mut d = def();
        d.name = "arti cles";
        assert!(d.validate().is_err());
    }

    #[test]
    fn definition_rejects_a_non_identifier_language() {
        let mut d = def();
        d.language = "english'; --";
        assert!(d.validate().is_err());
    }

    #[test]
    fn definition_rejects_an_empty_field_list() {
        let mut d = def();
        d.fields = Cow::Borrowed(&[]);
        assert!(d.validate().is_err());
    }

    #[test]
    fn definition_rejects_a_bad_weight() {
        let mut d = def();
        d.fields = Cow::Owned(vec![SearchIndexField::new("title", 'Z')]);
        d.embed_field = None;
        assert!(d.validate().is_err());
    }

    #[test]
    fn definition_rejects_an_embed_field_that_is_not_indexed() {
        let mut d = def();
        d.embed_field = Some("nope");
        assert!(d.validate().is_err());
    }

    #[test]
    fn has_field_is_the_allowlist_backends_consult() {
        let d = def();
        assert!(d.has_field("title"));
        assert!(d.has_field("body"));
        assert!(!d.has_field("tenant_id"), "not a declared searchable field");
        assert!(!d.has_field("title' = 'x' OR '1'='1"));
    }

    #[test]
    fn weight_of_returns_the_declared_weight_not_a_documents_claim() {
        let d = def();
        assert_eq!(d.weight_of("title"), Some('A'));
        assert_eq!(d.weight_of("body"), Some('B'));
        assert_eq!(d.weight_of("missing"), None);
    }

    #[test]
    fn tenant_scoped_is_carried_so_consumers_can_fail_closed() {
        assert!(!def().tenant_scoped);
        assert!(IndexDefinition::new("articles", "english", FIELDS, None, true).tenant_scoped);
    }

    #[test]
    fn supports_vector_search_tracks_the_embed_field() {
        assert!(def().supports_vector_search());
        let mut d = def();
        d.embed_field = None;
        assert!(!d.supports_vector_search());
    }

    #[test]
    fn document_text_skips_empty_values() {
        let doc = SearchDocument::new("articles", 1)
            .with_field("title", 'A', "Hello")
            .with_field("summary", 'C', "")
            .with_field("body", 'B', "World");
        assert_eq!(doc.text(), "Hello World");
    }

    #[test]
    fn document_field_lookup() {
        let doc = SearchDocument::new("articles", 1).with_field("title", 'A', "Hello");
        assert_eq!(doc.field("title"), Some("Hello"));
        assert_eq!(doc.field("missing"), None);
    }

    #[test]
    fn a_document_serializes_to_the_shape_an_external_engine_receives() {
        // `Serialize` only, deliberately: the index and field names are
        // `&'static str` derived from the model's own attributes, so there is
        // nothing meaningful to deserialize them back into. The type that
        // crosses a process boundary is `ReindexArgs` (owned `String` index),
        // not this.
        let doc = SearchDocument::new("articles", 9)
            .with_field("title", 'A', "Hi")
            .with_embed_text("Hi")
            .with_tenant("acme");
        let json = serde_json::to_value(&doc).expect("serialize");

        assert_eq!(json["index"], serde_json::json!("articles"));
        assert_eq!(json["id"], serde_json::json!(9));
        assert_eq!(json["tenant_id"], serde_json::json!("acme"));
        assert_eq!(json["embed_text"], serde_json::json!("Hi"));
        assert_eq!(json["fields"][0]["name"], serde_json::json!("title"));
        assert_eq!(json["fields"][0]["weight"], serde_json::json!("A"));
        assert_eq!(json["fields"][0]["value"], serde_json::json!("Hi"));
    }

    #[test]
    fn optional_text_extracts_to_empty_string() {
        assert_eq!(None::<String>.search_text_value(), "");
        assert_eq!(Some("x".to_owned()).search_text_value(), "x");
    }
}

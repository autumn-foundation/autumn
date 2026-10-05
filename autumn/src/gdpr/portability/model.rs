//! Capsule registration, manifest, and error types.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The `format` value in every capsule manifest.
pub const DATA_CAPSULE_FORMAT: &str = "autumn-data-capsule";

/// The capsule format version that this build writes and reads.
pub const DATA_CAPSULE_FORMAT_VERSION: u32 = 1;

/// One record: a JSON object of column name to value.
pub type Record = serde_json::Map<String, serde_json::Value>;

/// A `belongs_to` link from a column to a row of a different model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Relationship {
    /// The column that holds the key of the target row.
    pub column: String,
    /// The target table.
    pub target: String,
    /// The key column in the target table.
    pub target_column: String,
}

/// The capsule specification of one model.
///
/// Register it with [`GdprRegistry::capsule`](crate::gdpr::GdprRegistry::capsule).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CapsuleModel {
    /// The table name.
    pub table: String,
    /// The primary-key column. The default is `id`.
    pub primary_key: String,
    /// The column that holds the subject id (for example `user_id`).
    pub subject_column: String,
    /// The `belongs_to` links of this model.
    pub relationships: Vec<Relationship>,
    /// Columns that hold a `storage::Blob` or a blob key.
    pub blob_columns: Vec<String>,
    /// Columns that export leaves out, for example a password hash.
    pub excluded: Vec<String>,
}

impl CapsuleModel {
    /// Make a model. `subject_column` selects the rows of one subject.
    #[must_use]
    pub fn new(table: impl Into<String>, subject_column: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            primary_key: "id".to_owned(),
            subject_column: subject_column.into(),
            relationships: Vec::new(),
            blob_columns: Vec::new(),
            excluded: Vec::new(),
        }
    }

    /// Set the primary-key column.
    #[must_use]
    pub fn primary_key(mut self, column: impl Into<String>) -> Self {
        self.primary_key = column.into();
        self
    }

    /// Add a link from `column` to the `id` column of `target`.
    #[must_use]
    pub fn belongs_to(self, column: impl Into<String>, target: impl Into<String>) -> Self {
        self.references(column, target, "id")
    }

    /// Add a link from `column` to `target_column` of `target`.
    #[must_use]
    pub fn references(
        mut self,
        column: impl Into<String>,
        target: impl Into<String>,
        target_column: impl Into<String>,
    ) -> Self {
        self.relationships.push(Relationship {
            column: column.into(),
            target: target.into(),
            target_column: target_column.into(),
        });
        self
    }

    /// Mark `column` as a blob column. The capsule then holds the blob bytes.
    #[must_use]
    pub fn blob(mut self, column: impl Into<String>) -> Self {
        self.blob_columns.push(column.into());
        self
    }

    /// Leave `column` out of the capsule. Use it for secrets, such as a
    /// password hash or a token.
    ///
    /// Import cannot restore an excluded column. The column must accept
    /// `NULL` or have a default.
    #[must_use]
    pub fn exclude(mut self, column: impl Into<String>) -> Self {
        self.excluded.push(column.into());
        self
    }
}

/// One column of a model, as the manifest records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FieldSpec {
    /// The column name.
    pub name: String,
    /// The database type name (for example `bigint`).
    pub data_type: String,
    /// `true` when the column accepts `NULL`.
    #[serde(default)]
    pub nullable: bool,
    /// `true` for a generated column. Import does not write it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generated: bool,
}

impl FieldSpec {
    /// Make a `NOT NULL` column.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: data_type.into(),
            nullable: false,
            generated: false,
        }
    }

    /// Mark the column as nullable.
    #[must_use]
    pub const fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    /// Mark the column as generated.
    #[must_use]
    pub const fn generated(mut self) -> Self {
        self.generated = true;
        self
    }
}

/// The manifest entry of one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelManifest {
    /// The table name.
    pub table: String,
    /// The primary-key column.
    pub primary_key: String,
    /// The column that holds the subject id.
    pub subject_column: String,
    /// The columns of the table.
    pub fields: Vec<FieldSpec>,
    /// The `belongs_to` links of the table.
    pub relationships: Vec<Relationship>,
    /// The blob columns of the table.
    #[serde(default)]
    pub blob_columns: Vec<String>,
    /// The number of records in the capsule.
    pub record_count: u64,
    /// The record file, relative to the capsule root.
    pub file: String,
}

/// The manifest entry of one blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BlobEntry {
    /// The blob key in the `storage::BlobStore`.
    pub key: String,
    /// The hex SHA-256 of the bytes. The file is `blobs/<sha256>`.
    pub sha256: String,
    /// The MIME type.
    pub content_type: String,
    /// The size in bytes.
    pub byte_size: u64,
}

impl BlobEntry {
    /// The blob file, relative to the capsule root.
    #[must_use]
    pub fn file(&self) -> String {
        format!("blobs/{}", self.sha256)
    }
}

/// The `manifest.json` of a capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CapsuleManifest {
    /// Always [`DATA_CAPSULE_FORMAT`].
    pub format: String,
    /// The format version. Import accepts only [`DATA_CAPSULE_FORMAT_VERSION`].
    pub format_version: u32,
    /// The subject id.
    pub subject: String,
    /// RFC 3339 time of the export.
    pub generated_at: String,
    /// The Autumn version that wrote the capsule.
    pub framework_version: String,
    /// The models in the capsule.
    pub models: Vec<ModelManifest>,
    /// The blobs in the capsule.
    #[serde(default)]
    pub blobs: Vec<BlobEntry>,
    /// The SHA-256 of each file, by path. `write_dir` sets it.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

impl CapsuleManifest {
    pub(super) fn new(subject: &str) -> Self {
        Self {
            format: DATA_CAPSULE_FORMAT.to_owned(),
            format_version: DATA_CAPSULE_FORMAT_VERSION,
            subject: subject.to_owned(),
            generated_at: crate::time::ambient_now().to_rfc3339(),
            framework_version: env!("CARGO_PKG_VERSION").to_owned(),
            models: Vec::new(),
            blobs: Vec::new(),
            files: BTreeMap::new(),
        }
    }

    /// The manifest entry of `table`.
    #[must_use]
    pub fn model(&self, table: &str) -> Option<&ModelManifest> {
        self.models.iter().find(|m| m.table == table)
    }

    /// The blob entry of `key`.
    #[must_use]
    pub fn blob(&self, key: &str) -> Option<&BlobEntry> {
        self.blobs.iter().find(|b| b.key == key)
    }
}

/// An error from a capsule operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DataCapsuleError {
    /// A file operation failed.
    #[error("capsule I/O error at {path}: {source}")]
    Io {
        /// The path of the operation.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// A JSON file is not valid.
    #[error("capsule file {file} is not valid: {message}")]
    Json {
        /// The file, relative to the capsule root.
        file: String,
        /// The parser message.
        message: String,
    },
    /// A table, column, or path name is not safe.
    #[error("name is not safe: {0:?}")]
    InvalidName(String),
    /// An input value is not valid, for example an empty subject id.
    #[error("input is not valid: {0}")]
    InvalidInput(String),
    /// The signature or a file hash does not agree with the content.
    #[error("capsule integrity check failed: {0}")]
    Integrity(String),
    /// The capsule format or version is not known.
    #[error("unsupported capsule format: {0}")]
    UnsupportedFormat(String),
    /// No signing secret is set.
    #[error("no signing secret: set AUTUMN_SECURITY__SIGNING_SECRET")]
    MissingSigningSecret,
    /// The app did not register this table for capsules.
    #[error("table is not registered for capsules: {0}")]
    UnknownTable(String),
    /// The `belongs_to` links make a cycle.
    #[error("relationship cycle between tables: {0}")]
    RelationshipCycle(String),
    /// A record already exists in the target.
    #[error("record conflict: {0}")]
    Conflict(String),
    /// The output directory is not empty.
    #[error("directory is not empty: {}", .0.display())]
    NotEmpty(PathBuf),
    /// The data store failed.
    #[error("capsule store error: {0}")]
    Store(String),
    /// The blob store failed.
    #[error("capsule blob error: {0}")]
    Blob(String),
    /// A part that capsules need is not configured.
    #[error("data capsules are not configured: {0}")]
    NotConfigured(String),
}

impl DataCapsuleError {
    pub(super) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// `true` for a plain SQL identifier: 1 to 63 ASCII letters, digits, or `_`,
/// not first a digit.
#[must_use]
pub(super) fn is_safe_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 63
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(super) fn check_ident(name: &str) -> Result<(), DataCapsuleError> {
    if is_safe_ident(name) {
        Ok(())
    } else {
        Err(DataCapsuleError::InvalidName(name.to_owned()))
    }
}

/// Check the names of one model entry.
pub(super) fn check_model_names(
    table: &str,
    primary_key: &str,
    subject_column: &str,
    relationships: &[Relationship],
    blob_columns: &[String],
) -> Result<(), DataCapsuleError> {
    check_ident(table)?;
    check_ident(primary_key)?;
    check_ident(subject_column)?;
    for rel in relationships {
        check_ident(&rel.column)?;
        check_ident(&rel.target)?;
        check_ident(&rel.target_column)?;
    }
    blob_columns.iter().try_for_each(|c| check_ident(c))
}

/// The record file of `table`.
pub(super) fn record_file(table: &str) -> String {
    format!("records/{table}.json")
}

/// The text key of a JSON scalar, for subject and key matching.
pub(super) fn value_key(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_ident_accepts_plain_names() {
        for name in ["users", "_t", "Post2", "a_b_c"] {
            assert!(is_safe_ident(name), "{name}");
        }
    }

    #[test]
    fn safe_ident_rejects_unsafe_names() {
        let long = "a".repeat(64);
        for name in [
            "",
            "1a",
            "a-b",
            "a b",
            "a.b",
            "a\"b",
            "../x",
            "é",
            long.as_str(),
        ] {
            assert!(!is_safe_ident(name), "{name}");
        }
    }

    #[test]
    fn value_key_reads_scalars_only() {
        assert_eq!(value_key(&serde_json::json!(7)).as_deref(), Some("7"));
        assert_eq!(value_key(&serde_json::json!("a")).as_deref(), Some("a"));
        assert_eq!(value_key(&serde_json::json!(true)).as_deref(), Some("true"));
        assert_eq!(value_key(&serde_json::json!(null)), None);
        assert_eq!(value_key(&serde_json::json!({"a": 1})), None);
    }

    #[test]
    fn model_builder_sets_defaults() {
        let m = CapsuleModel::new("posts", "author_id")
            .belongs_to("author_id", "users")
            .references("org", "orgs", "slug")
            .blob("cover")
            .primary_key("post_id");
        assert_eq!(m.primary_key, "post_id");
        assert_eq!(m.relationships[0].target_column, "id");
        assert_eq!(m.relationships[1].target_column, "slug");
        assert_eq!(m.blob_columns, ["cover"]);
    }

    #[test]
    fn generated_flag_is_not_written_when_false() {
        let json = serde_json::to_string(&FieldSpec::new("a", "text")).unwrap();
        assert!(!json.contains("generated"), "{json}");
        let json = serde_json::to_string(&FieldSpec::new("a", "text").generated()).unwrap();
        assert!(json.contains("\"generated\":true"), "{json}");
    }
}

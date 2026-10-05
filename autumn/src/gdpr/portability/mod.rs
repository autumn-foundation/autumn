//! Portable data capsules (issue #1811).
//!
//! A capsule holds all data of one subject (a user, a tenant, or an account)
//! in one signed directory:
//!
//! - `records/<table>.json`: the records of each registered model.
//! - `manifest.json`: the models, fields, relationships, blobs, and the
//!   SHA-256 of each file.
//! - `signature.json`: an HMAC-SHA256 of the manifest, made with
//!   `[security.signing_secret]`.
//! - `blobs/<sha256>`: the referenced blob bytes (feature `storage`).
//! - `viewer/index.html`: an offline HTML viewer. It has no script and needs no
//!   server.
//!
//! The app can import a capsule that it wrote, with no loss of data.
//!
//! # Example
//!
//! ```rust,no_run
//! use autumn_web::gdpr::GdprRegistry;
//! use autumn_web::gdpr::portability::{
//!     CapsuleModel, CapsuleSigner, DataCapsule, MemoryCapsuleStore, export_subject,
//!     import_capsule,
//! };
//!
//! # async fn demo(store: &MemoryCapsuleStore, target: &MemoryCapsuleStore)
//! # -> Result<(), autumn_web::gdpr::portability::CapsuleError> {
//! let registry = GdprRegistry::new()
//!     .capsule(CapsuleModel::new("users", "id"))
//!     .capsule(CapsuleModel::new("posts", "author_id").belongs_to("author_id", "users"));
//! let signer = CapsuleSigner::new(b"a-secret-of-at-least-32-bytes-long!");
//!
//! let capsule = export_subject(registry.capsule_models(), store, "42").await?;
//! capsule.write_dir("capsule-42".as_ref(), &signer)?;
//!
//! let loaded = DataCapsule::read_dir("capsule-42".as_ref(), &signer)?;
//! import_capsule(&loaded, registry.capsule_models(), target).await?;
//! # Ok(()) }
//! ```
//!
//! See `docs/guide/data-capsules.md`.

// autumn-determinism-gate: see the note in `gdpr/mod.rs`.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

mod archive;
#[cfg(feature = "storage")]
mod blobs;
mod model;
#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod pg;
mod service;
mod store;
mod viewer;

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;

pub use archive::{CapsuleSigner, VerifyReport, verify_dir};
#[cfg(feature = "storage")]
pub use blobs::{collect_blobs, restore_blobs};
pub use model::{
    BlobEntry, CapsuleError, CapsuleManifest, CapsuleModel, FORMAT, FORMAT_VERSION, FieldSpec,
    ModelManifest, Record, Relationship,
};
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub use pg::PgCapsuleStore;
pub use service::{CapsuleDirectory, CapsuleService, ExportReport};
pub use store::{CapsuleFuture, CapsuleStore, ImportBatch, MemoryCapsuleStore};

use model::{check_model_names, record_file};

/// The data of one subject, in memory.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DataCapsule {
    /// The manifest.
    pub manifest: CapsuleManifest,
    /// The records of each table.
    pub records: BTreeMap<String, Vec<Record>>,
    /// The blob bytes, by hex SHA-256.
    pub blobs: BTreeMap<String, Bytes>,
}

impl DataCapsule {
    /// The records of `table`. Empty when the capsule has no such table.
    #[must_use]
    pub fn records(&self, table: &str) -> &[Record] {
        self.records.get(table).map_or(&[], Vec::as_slice)
    }
}

/// The result of [`import_capsule`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct ImportSummary {
    /// Each table and its record count, in import order.
    pub tables: Vec<(String, u64)>,
    /// The total number of records.
    pub records: u64,
}

fn check_models(models: &[CapsuleModel]) -> Result<(), CapsuleError> {
    let mut seen = BTreeSet::new();
    for m in models {
        check_model_names(
            &m.table,
            &m.primary_key,
            &m.subject_column,
            &m.relationships,
            &m.blob_columns,
        )?;
        if !seen.insert(m.table.as_str()) {
            return Err(CapsuleError::InvalidName(format!(
                "{} is registered two times",
                m.table
            )));
        }
    }
    Ok(())
}

/// Export the records of `subject` from each model in `models`.
///
/// Use [`GdprRegistry::capsule_models`](crate::gdpr::GdprRegistry::capsule_models)
/// for `models`.
///
/// # Errors
///
/// [`CapsuleError::InvalidName`] for an empty subject or an unsafe or
/// duplicate name, or an error from `store`.
pub async fn export_subject(
    models: &[CapsuleModel],
    store: &dyn CapsuleStore,
    subject: &str,
) -> Result<DataCapsule, CapsuleError> {
    if subject.is_empty() {
        return Err(CapsuleError::InvalidName(
            "the subject id is empty".to_owned(),
        ));
    }
    check_models(models)?;
    let mut manifest = CapsuleManifest::new(subject);
    let mut records = BTreeMap::new();
    for model in models {
        let fields = store.describe(model).await?;
        let rows = store.fetch(model, subject).await?;
        manifest.models.push(ModelManifest {
            table: model.table.clone(),
            primary_key: model.primary_key.clone(),
            subject_column: model.subject_column.clone(),
            fields,
            relationships: model.relationships.clone(),
            blob_columns: model.blob_columns.clone(),
            record_count: rows.len() as u64,
            file: record_file(&model.table),
        });
        records.insert(model.table.clone(), rows);
    }
    Ok(DataCapsule {
        manifest,
        records,
        blobs: BTreeMap::new(),
    })
}

/// Import `capsule` into `store`, parents before children, as one unit.
///
/// Each table in the capsule must be in `models`. Use a capsule from
/// [`DataCapsule::read_dir`], which verifies the signature first.
///
/// # Errors
///
/// [`CapsuleError::UnknownTable`], [`CapsuleError::RelationshipCycle`],
/// [`CapsuleError::UnsupportedFormat`], or an error from `store` (for example
/// [`CapsuleError::Conflict`]).
pub async fn import_capsule(
    capsule: &DataCapsule,
    models: &[CapsuleModel],
    store: &dyn CapsuleStore,
) -> Result<ImportSummary, CapsuleError> {
    let manifest = &capsule.manifest;
    if manifest.format != FORMAT || manifest.format_version != FORMAT_VERSION {
        return Err(CapsuleError::UnsupportedFormat(format!(
            "{} version {}",
            manifest.format, manifest.format_version
        )));
    }
    check_models(models)?;
    for model in &manifest.models {
        check_model_names(
            &model.table,
            &model.primary_key,
            &model.subject_column,
            &model.relationships,
            &model.blob_columns,
        )?;
        if !models.iter().any(|m| m.table == model.table) {
            return Err(CapsuleError::UnknownTable(model.table.clone()));
        }
    }
    let order = import_order(&manifest.models)?;
    let batches: Vec<ImportBatch<'_>> = order
        .iter()
        .map(|model| ImportBatch {
            model,
            records: capsule.records(&model.table),
        })
        .collect();
    store.insert_all(&batches).await?;
    let tables: Vec<(String, u64)> = batches
        .iter()
        .map(|b| (b.model.table.clone(), b.records.len() as u64))
        .collect();
    let records = tables.iter().map(|(_, n)| n).sum();
    Ok(ImportSummary { tables, records })
}

/// Sort models so that each `belongs_to` target comes first.
///
/// Links to tables outside the capsule and links to the same table do not
/// count. The sort keeps manifest order where it can.
fn import_order(models: &[ModelManifest]) -> Result<Vec<&ModelManifest>, CapsuleError> {
    let tables: BTreeSet<&str> = models.iter().map(|m| m.table.as_str()).collect();
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let mut order = Vec::with_capacity(models.len());
    while order.len() < models.len() {
        let next = models.iter().find(|m| {
            !placed.contains(m.table.as_str())
                && m.relationships.iter().all(|r| {
                    r.target == m.table
                        || !tables.contains(r.target.as_str())
                        || placed.contains(r.target.as_str())
                })
        });
        let Some(next) = next else {
            let rest: Vec<&str> = models
                .iter()
                .map(|m| m.table.as_str())
                .filter(|t| !placed.contains(t))
                .collect();
            return Err(CapsuleError::RelationshipCycle(rest.join(", ")));
        };
        placed.insert(next.table.as_str());
        order.push(next);
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(table: &str, targets: &[&str]) -> ModelManifest {
        let mut model = CapsuleModel::new(table, "id");
        for t in targets {
            model = model.belongs_to(format!("{t}_id"), *t);
        }
        ModelManifest {
            table: model.table,
            primary_key: model.primary_key,
            subject_column: model.subject_column,
            fields: Vec::new(),
            relationships: model.relationships,
            blob_columns: Vec::new(),
            record_count: 0,
            file: record_file(table),
        }
    }

    fn order(models: &[ModelManifest]) -> Vec<&str> {
        import_order(models)
            .unwrap()
            .iter()
            .map(|m| m.table.as_str())
            .collect()
    }

    #[test]
    fn order_puts_targets_first() {
        let models = [
            manifest("comments", &["posts", "users"]),
            manifest("posts", &["users"]),
            manifest("users", &[]),
        ];
        assert_eq!(order(&models), ["users", "posts", "comments"]);
    }

    #[test]
    fn order_ignores_self_links_and_outside_tables() {
        let models = [manifest("nodes", &["nodes", "orgs"]), manifest("a", &[])];
        assert_eq!(order(&models), ["nodes", "a"]);
    }

    #[test]
    fn order_reports_a_cycle() {
        let models = [
            manifest("a", &["b"]),
            manifest("b", &["a"]),
            manifest("c", &[]),
        ];
        let err = import_order(&models).unwrap_err();
        assert!(
            matches!(err, CapsuleError::RelationshipCycle(ref t) if t == "a, b"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_model_is_rejected() {
        let models = [CapsuleModel::new("a", "id"), CapsuleModel::new("a", "id")];
        assert!(matches!(
            check_models(&models),
            Err(CapsuleError::InvalidName(_))
        ));
    }
}

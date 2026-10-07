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
//! # -> Result<(), autumn_web::gdpr::portability::DataCapsuleError> {
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
mod root;
mod service;
mod store;
mod viewer;

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;

pub use archive::{CapsuleSigner, VerifyReport, verify_dir};
#[cfg(feature = "storage")]
pub use blobs::{collect_blobs, rebind_blobs, restore_blobs};
pub use model::{
    BlobEntry, CapsuleManifest, CapsuleModel, DATA_CAPSULE_FORMAT, DATA_CAPSULE_FORMAT_VERSION,
    DataCapsuleError, FieldSpec, ModelManifest, Record, Relationship,
};
#[cfg(all(feature = "db", not(feature = "sqlite")))]
pub use pg::PgCapsuleStore;
pub use service::{CapsuleDirectory, CapsuleService, ExportReport};
pub use store::{CapsuleFuture, CapsuleStore, ImportBatch, MemoryCapsuleStore, ModelData};

use model::{check_model_names, record_file, value_key};

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
    pub tables: Vec<TableCount>,
    /// The total number of records.
    pub records: u64,
}

/// The record count of one table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct TableCount {
    /// The table name.
    pub table: String,
    /// The number of records.
    pub records: u64,
}

fn check_models(models: &[CapsuleModel]) -> Result<(), DataCapsuleError> {
    let mut seen = BTreeSet::new();
    for m in models {
        check_model_names(
            &m.table,
            &m.primary_key,
            &m.subject_column,
            &m.relationships,
            &m.blob_columns,
        )?;
        for column in &m.excluded {
            model::check_ident(column)?;
            if *column == m.primary_key || *column == m.subject_column {
                return Err(DataCapsuleError::InvalidInput(format!(
                    "{}.{column}: export cannot leave out the key or subject column",
                    m.table
                )));
            }
        }
        if !seen.insert(m.table.as_str()) {
            return Err(DataCapsuleError::InvalidInput(format!(
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
/// [`DataCapsuleError::InvalidInput`] for an empty subject, a duplicate
/// model, or a subject, primary-key, excluded, blob, or relationship column
/// that the table does not have,
/// [`DataCapsuleError::InvalidName`] for an unsafe name, or an error from
/// `store`.
pub async fn export_subject(
    models: &[CapsuleModel],
    store: &dyn CapsuleStore,
    subject: &str,
) -> Result<DataCapsule, DataCapsuleError> {
    if subject.is_empty() {
        return Err(DataCapsuleError::InvalidInput(
            "the subject id is empty".to_owned(),
        ));
    }
    check_models(models)?;
    let data = store.fetch_subject(models, subject).await?;
    // A short result must not give a capsule without some models.
    if data.len() != models.len() {
        return Err(DataCapsuleError::Store(format!(
            "the store gave data for {} of {} models",
            data.len(),
            models.len()
        )));
    }
    let mut manifest = CapsuleManifest::new(subject);
    let mut records = BTreeMap::new();
    let mut described: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (model, (mut fields, mut rows)) in models.iter().zip(data) {
        // Fail closed: a typo must not export a secret column, or give a
        // capsule without its records, blobs or links. A custom store can
        // give an empty result for a subject column that does not exist.
        let configured = [&model.subject_column, &model.primary_key]
            .into_iter()
            .chain(&model.excluded)
            .chain(&model.blob_columns)
            .chain(model.relationships.iter().map(|r| &r.column));
        for name in configured {
            if !fields.iter().any(|f| &f.name == name) {
                return Err(DataCapsuleError::InvalidInput(format!(
                    "{} names column {name:?}, but the table has no such column",
                    model.table
                )));
            }
        }
        // A row must hold only described columns. A column outside `fields`
        // would be in the capsule, but not in the manifest or the viewer.
        for row in &rows {
            if let Some(column) = row.keys().find(|k| !fields.iter().any(|f| &f.name == *k)) {
                return Err(DataCapsuleError::InvalidInput(format!(
                    "the store gave column {column:?} of {}, which its description of the \
                     table does not have",
                    model.table
                )));
            }
            // The viewer anchors a record at its key, and import writes both
            // columns: a row needs a value in each.
            for column in [&model.primary_key, &model.subject_column] {
                if row.get(column).and_then(value_key).is_none() {
                    return Err(DataCapsuleError::InvalidInput(format!(
                        "the store gave a row of {} without a value in {column:?}",
                        model.table
                    )));
                }
            }
        }
        described.insert(
            model.table.as_str(),
            fields.iter().map(|f| f.name.clone()).collect(),
        );
        fields.retain(|f| !model.excluded.contains(&f.name));
        for row in &mut rows {
            row.retain(|column, _| !model.excluded.contains(column));
        }
        manifest.models.push(ModelManifest {
            table: model.table.clone(),
            primary_key: model.primary_key.clone(),
            subject_column: model.subject_column.clone(),
            fields,
            relationships: model.relationships.clone(),
            // An excluded column holds nothing in the capsule. Declaring it
            // a blob column would make the capsule hold a column that the
            // model excludes, which import refuses.
            blob_columns: model
                .blob_columns
                .iter()
                .filter(|c| !model.excluded.contains(c))
                .cloned()
                .collect(),
            record_count: rows.len() as u64,
            file: record_file(&model.table),
        });
        records.insert(model.table.clone(), rows);
    }
    // A link to a model in this export must name a column of that model:
    // a typo would lose the links in the viewer.
    for model in models {
        for rel in &model.relationships {
            if let Some(columns) = described.get(rel.target.as_str())
                && !columns.contains(&rel.target_column)
            {
                return Err(DataCapsuleError::InvalidInput(format!(
                    "{}.{} links to {}.{:?}, but {} has no such column",
                    model.table, rel.column, rel.target, rel.target_column, rel.target
                )));
            }
        }
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
/// [`DataCapsuleError::UnknownTable`], [`DataCapsuleError::RelationshipCycle`],
/// [`DataCapsuleError::UnsupportedFormat`],
/// [`DataCapsuleError::InvalidInput`] for a record that names a blob the
/// capsule does not hold, for a column that the model now excludes, or for a
/// table whose subject column or key the model has changed, or an error from
/// `store` (for example
/// [`DataCapsuleError::Conflict`]).
pub async fn import_capsule(
    capsule: &DataCapsule,
    models: &[CapsuleModel],
    store: &dyn CapsuleStore,
) -> Result<ImportSummary, DataCapsuleError> {
    let order = check_importable(capsule, models)?;
    let batches: Vec<ImportBatch<'_>> = order
        .iter()
        .map(|model| ImportBatch::new(model, capsule.records(&model.table)))
        .collect();
    store.insert_all(&batches).await?;
    let tables: Vec<TableCount> = batches
        .iter()
        .map(|b| TableCount {
            table: b.model.table.clone(),
            records: b.records.len() as u64,
        })
        .collect();
    let records = tables.iter().map(|t| t.records).sum();
    Ok(ImportSummary { tables, records })
}

/// The checks of [`import_capsule`] that need no store: the format, the
/// models, the blob references, and an import order. Gives that order.
///
/// `CapsuleService` runs them before it writes a blob, so a capsule that can
/// never be imported leaves no blob behind.
pub(super) fn check_importable<'c>(
    capsule: &'c DataCapsule,
    models: &[CapsuleModel],
) -> Result<Vec<&'c ModelManifest>, DataCapsuleError> {
    let manifest = &capsule.manifest;
    archive::check_format(manifest)?;
    check_models(models)?;
    for model in &manifest.models {
        check_model_names(
            &model.table,
            &model.primary_key,
            &model.subject_column,
            &model.relationships,
            &model.blob_columns,
        )?;
        let current = models
            .iter()
            .find(|m| m.table == model.table)
            .ok_or_else(|| DataCapsuleError::UnknownTable(model.table.clone()))?;
        // The subject column scopes the records, and import writes by the
        // key. A capsule of another scope or key is not one of this model:
        // it could restore a part of what the model now holds for a subject.
        if model.subject_column != current.subject_column
            || model.primary_key != current.primary_key
        {
            return Err(DataCapsuleError::InvalidInput(format!(
                "{} in the capsule has subject column {:?} and key {:?}, but the model now \
                 has {:?} and {:?}",
                model.table,
                model.subject_column,
                model.primary_key,
                current.subject_column,
                current.primary_key
            )));
        }
        // A capsule from before the app excluded a column still holds it, for
        // example a password hash. The exclusion holds for import too: the
        // column, and a blob it names, must not be written back.
        let held = model
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .chain(model.blob_columns.iter().map(String::as_str))
            .chain(
                capsule
                    .records(&model.table)
                    .iter()
                    .flat_map(|row| row.keys().map(String::as_str)),
            );
        for column in held {
            if current.excluded.iter().any(|e| e == column) {
                return Err(DataCapsuleError::InvalidInput(format!(
                    "{}.{column} is excluded from capsules, but the capsule holds it",
                    model.table
                )));
            }
        }
        // Export refuses a row without a value in its key or subject column.
        // A capsule built or changed through the public API can still carry
        // one.
        for row in capsule.records(&model.table) {
            for column in [&model.primary_key, &model.subject_column] {
                if row.get(column).and_then(value_key).is_none() {
                    return Err(DataCapsuleError::InvalidInput(format!(
                        "a row of {} has no value in {column:?}",
                        model.table
                    )));
                }
            }
        }
        // Export skips a blob that its store does not have, but the record
        // still names it. Such a record would point at nothing, or at other
        // bytes that the target keeps under that key. A column that the app
        // marked as a blob after the export holds keys too.
        for row in capsule.records(&model.table) {
            for column in blob_columns(model, current) {
                if let Some(key) = row.get(column).and_then(viewer::blob_key)
                    && manifest.blob(key).is_none()
                {
                    return Err(DataCapsuleError::InvalidInput(format!(
                        "{}.{column} refers to blob {key:?}, which the capsule does not hold",
                        model.table
                    )));
                }
            }
        }
    }
    import_order(&manifest.models)
}

/// The blob columns of `model` in a capsule: those of its manifest, and those
/// that `current`, the model of the app now, marks as blobs and does not
/// exclude.
fn blob_columns<'a>(model: &'a ModelManifest, current: &'a CapsuleModel) -> Vec<&'a String> {
    let mut columns: Vec<&String> = model.blob_columns.iter().collect();
    for column in &current.blob_columns {
        if !current.excluded.contains(column) && !columns.contains(&column) {
            columns.push(column);
        }
    }
    columns
}

/// Add the blob columns of `models` to the manifest of `capsule`, so that a
/// restore rebinds the handles in a column that the app marked as a blob
/// after the export. Run [`check_importable`] first.
#[cfg(feature = "storage")]
pub(super) fn adopt_blob_columns(capsule: &mut DataCapsule, models: &[CapsuleModel]) {
    for model in &mut capsule.manifest.models {
        if let Some(current) = models.iter().find(|m| m.table == model.table) {
            let columns: Vec<String> = blob_columns(model, current).into_iter().cloned().collect();
            model.blob_columns = columns;
        }
    }
}

/// Sort models so that each `belongs_to` target comes first.
///
/// Links to tables outside the capsule and links to the same table do not
/// count. The sort keeps manifest order where it can.
fn import_order(models: &[ModelManifest]) -> Result<Vec<&ModelManifest>, DataCapsuleError> {
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
            return Err(DataCapsuleError::RelationshipCycle(rest.join(", ")));
        };
        placed.insert(next.table.as_str());
        order.push(next);
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store that gives the data of the first model only.
    struct FirstOnly(MemoryCapsuleStore);

    impl CapsuleStore for FirstOnly {
        fn describe<'a>(&'a self, model: &'a CapsuleModel) -> CapsuleFuture<'a, Vec<FieldSpec>> {
            self.0.describe(model)
        }

        fn fetch<'a>(
            &'a self,
            model: &'a CapsuleModel,
            subject: &'a str,
        ) -> CapsuleFuture<'a, Vec<Record>> {
            self.0.fetch(model, subject)
        }

        fn fetch_subject<'a>(
            &'a self,
            models: &'a [CapsuleModel],
            subject: &'a str,
        ) -> CapsuleFuture<'a, Vec<ModelData>> {
            Box::pin(async move {
                let mut data = self.0.fetch_subject(models, subject).await?;
                data.truncate(1);
                Ok(data)
            })
        }

        fn insert_all<'a>(&'a self, batches: &'a [ImportBatch<'a>]) -> CapsuleFuture<'a, ()> {
            self.0.insert_all(batches)
        }
    }

    #[tokio::test]
    async fn export_refuses_a_store_that_skips_a_model() {
        let fields = || vec![FieldSpec::new("id", "bigint")];
        let store = FirstOnly(
            MemoryCapsuleStore::new()
                .table("users", fields())
                .table("posts", fields()),
        );
        let models = [
            CapsuleModel::new("users", "id"),
            CapsuleModel::new("posts", "id"),
        ];
        let err = export_subject(&models, &store, "1")
            .await
            .expect_err("posts is missing");
        assert!(matches!(err, DataCapsuleError::Store(_)), "{err:?}");
    }

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
            matches!(err, DataCapsuleError::RelationshipCycle(ref t) if t == "a, b"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_model_is_rejected() {
        let models = [CapsuleModel::new("a", "id"), CapsuleModel::new("a", "id")];
        assert!(matches!(
            check_models(&models),
            Err(DataCapsuleError::InvalidInput(_))
        ));
    }

    #[test]
    fn the_key_and_subject_columns_cannot_be_excluded() {
        for column in ["id", "owner_id"] {
            let models = [CapsuleModel::new("a", "owner_id").exclude(column)];
            assert!(
                matches!(
                    check_models(&models),
                    Err(DataCapsuleError::InvalidInput(_))
                ),
                "{column}"
            );
        }
    }
}

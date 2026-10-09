//! The data-store seam of capsule export and import.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use super::model::{CapsuleModel, DataCapsuleError, FieldSpec, ModelManifest, Record, value_key};

/// The future type of [`CapsuleStore`] methods.
pub type CapsuleFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, DataCapsuleError>> + Send + 'a>>;

/// The records of one table to import.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ImportBatch<'a> {
    /// The manifest entry of the table.
    pub model: &'a ModelManifest,
    /// The records to write.
    pub records: &'a [Record],
}

impl<'a> ImportBatch<'a> {
    /// Make a batch.
    #[must_use]
    pub const fn new(model: &'a ModelManifest, records: &'a [Record]) -> Self {
        Self { model, records }
    }
}

/// The columns and the records of one model.
pub type ModelData = (Vec<FieldSpec>, Vec<Record>);

/// A data source and target for capsules.
///
/// [`MemoryCapsuleStore`] is for tests. `PgCapsuleStore` is for Postgres
/// (feature `db`, not with `sqlite`).
pub trait CapsuleStore: Send + Sync {
    /// Give the columns of the table of `model`.
    fn describe<'a>(&'a self, model: &'a CapsuleModel) -> CapsuleFuture<'a, Vec<FieldSpec>>;

    /// Give the records of `model` whose subject column is `subject`.
    fn fetch<'a>(
        &'a self,
        model: &'a CapsuleModel,
        subject: &'a str,
    ) -> CapsuleFuture<'a, Vec<Record>>;

    /// Give the columns and records of each model, in the order of `models`.
    /// Give one entry for each model: export refuses a different count.
    ///
    /// The default calls [`describe`](Self::describe) and
    /// [`fetch`](Self::fetch) for each model. A store with transactions
    /// overrides it to read all models from one snapshot.
    fn fetch_subject<'a>(
        &'a self,
        models: &'a [CapsuleModel],
        subject: &'a str,
    ) -> CapsuleFuture<'a, Vec<ModelData>> {
        Box::pin(async move {
            let mut data = Vec::with_capacity(models.len());
            for model in models {
                data.push((
                    self.describe(model).await?,
                    self.fetch(model, subject).await?,
                ));
            }
            Ok(data)
        })
    }

    /// Write all batches in the given order, as one atomic unit.
    ///
    /// If one record fails, the store must write no record.
    fn insert_all<'a>(&'a self, batches: &'a [ImportBatch<'a>]) -> CapsuleFuture<'a, ()>;
}

#[derive(Debug, Default)]
struct MemoryTable {
    fields: Vec<FieldSpec>,
    rows: Vec<Record>,
}

/// An in-memory [`CapsuleStore`] for tests and examples.
#[derive(Debug, Default)]
pub struct MemoryCapsuleStore {
    tables: Mutex<BTreeMap<String, MemoryTable>>,
}

impl MemoryCapsuleStore {
    /// Make an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an empty table with these columns.
    #[must_use]
    pub fn table(self, name: impl Into<String>, fields: Vec<FieldSpec>) -> Self {
        self.lock().insert(
            name.into(),
            MemoryTable {
                fields,
                rows: Vec::new(),
            },
        );
        self
    }

    /// Add one row to `table`.
    ///
    /// # Panics
    ///
    /// Panics when `table` is not known or `row` is not a JSON object.
    pub fn insert(&self, table: &str, row: serde_json::Value) {
        let serde_json::Value::Object(row) = row else {
            panic!("MemoryCapsuleStore::insert needs a JSON object");
        };
        self.lock()
            .get_mut(table)
            .unwrap_or_else(|| panic!("MemoryCapsuleStore has no table {table:?}"))
            .rows
            .push(row);
    }

    /// Give all rows of `table`.
    #[must_use]
    pub fn rows(&self, table: &str) -> Vec<Record> {
        self.lock()
            .get(table)
            .map(|t| t.rows.clone())
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, MemoryTable>> {
        self.tables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn unknown(table: &str) -> DataCapsuleError {
    DataCapsuleError::Store(format!("unknown table {table:?}"))
}

fn key_of(row: &Record, column: &str) -> Option<String> {
    row.get(column).and_then(value_key)
}

impl CapsuleStore for MemoryCapsuleStore {
    fn describe<'a>(&'a self, model: &'a CapsuleModel) -> CapsuleFuture<'a, Vec<FieldSpec>> {
        let result = self
            .lock()
            .get(&model.table)
            .map(|t| t.fields.clone())
            .ok_or_else(|| unknown(&model.table));
        Box::pin(async move { result })
    }

    fn fetch<'a>(
        &'a self,
        model: &'a CapsuleModel,
        subject: &'a str,
    ) -> CapsuleFuture<'a, Vec<Record>> {
        let result = self
            .lock()
            .get(&model.table)
            .map(|t| {
                t.rows
                    .iter()
                    .filter(|row| key_of(row, &model.subject_column).as_deref() == Some(subject))
                    .cloned()
                    .collect()
            })
            .ok_or_else(|| unknown(&model.table));
        Box::pin(async move { result })
    }

    fn insert_all<'a>(&'a self, batches: &'a [ImportBatch<'a>]) -> CapsuleFuture<'a, ()> {
        let result = (|| {
            let mut tables = self.lock();
            // Check all batches first, then write: the import is atomic.
            for batch in batches {
                let table = tables
                    .get(&batch.model.table)
                    .ok_or_else(|| unknown(&batch.model.table))?;
                let pk = batch.model.primary_key.as_str();
                let mut seen: std::collections::BTreeSet<String> =
                    table.rows.iter().filter_map(|r| key_of(r, pk)).collect();
                for record in batch.records {
                    if let Some(key) = key_of(record, pk)
                        && !seen.insert(key.clone())
                    {
                        return Err(DataCapsuleError::Conflict(format!(
                            "{}.{pk} = {key} already exists",
                            batch.model.table
                        )));
                    }
                }
            }
            for batch in batches {
                if let Some(table) = tables.get_mut(&batch.model.table) {
                    table.rows.extend(batch.records.iter().cloned());
                }
            }
            Ok(())
        })();
        Box::pin(async move { result })
    }
}

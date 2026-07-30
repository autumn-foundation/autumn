//! Where reindex and backfill read records from.
//!
//! The reindex job deliberately does **not** carry the record in its payload.
//! It carries `(index, id)` and re-reads the row, which is what makes the
//! operation idempotent and self-healing: a replayed job, a lost delete, or a
//! row changed by direct SQL all converge to the same index state. The
//! backfill walks the same source, so bootstrapping and incremental sync can
//! never disagree about how a record is extracted.
//!
//! [`crate::PostgresSearchStore`] implements this generically from the
//! [`IndexDefinition`] alone — no per-model generated code.

use std::collections::BTreeMap;
use std::sync::RwLock;

use autumn_web::search::{IndexDefinition, SearchDocument, SearchIndexed};

use crate::backend::BoxFuture;
use crate::error::{SearchError, SearchResult};

/// Reads records out of the system of record and extracts them into
/// [`SearchDocument`]s.
pub trait DocumentSource: Send + Sync + 'static {
    /// Fetch the documents for `ids`.
    ///
    /// Ids with no row are simply **absent** from the result — that absence is
    /// the signal the reindex job uses to delete a document, so it must not be
    /// an error.
    fn fetch<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<Vec<SearchDocument>>>;

    /// Return up to `limit` documents whose id is greater than `after`,
    /// ordered by ascending id.
    ///
    /// Keyset pagination rather than `OFFSET`: a backfill of a live table must
    /// not skip or repeat rows when concurrent writes shift offsets.
    fn scan<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        after: Option<i64>,
        limit: usize,
    ) -> BoxFuture<'a, SearchResult<Vec<SearchDocument>>>;
}

/// An in-memory [`DocumentSource`] for tests and for apps that already hold
/// their corpus in process.
///
/// Also records how many times each id was fetched, so a test can assert that
/// an operation did **not** re-read the source.
#[derive(Debug, Default)]
pub struct MemoryDocumentSource {
    documents: RwLock<BTreeMap<(String, i64), SearchDocument>>,
    fetches: RwLock<BTreeMap<i64, usize>>,
}

impl MemoryDocumentSource {
    /// Create an empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace `record`'s document.
    pub fn upsert<M: SearchIndexed>(&self, record: &M) {
        let document = record.search_document();
        if let Ok(mut guard) = self.documents.write() {
            guard.insert((document.index.to_owned(), document.id), document);
        }
    }

    /// Insert or replace an already-extracted document.
    pub fn upsert_document(&self, document: SearchDocument) {
        if let Ok(mut guard) = self.documents.write() {
            guard.insert((document.index.to_owned(), document.id), document);
        }
    }

    /// Remove a record from every index in this source.
    pub fn remove(&self, id: i64) {
        if let Ok(mut guard) = self.documents.write() {
            guard.retain(|(_, key), _| *key != id);
        }
    }

    /// How many times `id` has been fetched.
    #[must_use]
    pub fn fetch_calls_for(&self, id: i64) -> usize {
        self.fetches
            .read()
            .map_or(0, |guard| guard.get(&id).copied().unwrap_or(0))
    }

    fn record_fetch(&self, ids: &[i64]) {
        if let Ok(mut guard) = self.fetches.write() {
            for id in ids {
                *guard.entry(*id).or_insert(0) += 1;
            }
        }
    }
}

impl DocumentSource for MemoryDocumentSource {
    fn fetch<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<Vec<SearchDocument>>> {
        Box::pin(async move {
            self.record_fetch(ids);
            let guard = self
                .documents
                .read()
                .map_err(|_| SearchError::Backend("document source lock poisoned".to_owned()))?;
            Ok(ids
                .iter()
                .filter_map(|id| guard.get(&(definition.name.to_owned(), *id)).cloned())
                .collect())
        })
    }

    fn scan<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        after: Option<i64>,
        limit: usize,
    ) -> BoxFuture<'a, SearchResult<Vec<SearchDocument>>> {
        Box::pin(async move {
            let guard = self
                .documents
                .read()
                .map_err(|_| SearchError::Backend("document source lock poisoned".to_owned()))?;
            Ok(guard
                .iter()
                .filter(|((index, id), _)| {
                    index == definition.name && after.is_none_or(|after| *id > after)
                })
                .take(limit)
                .map(|(_, document)| document.clone())
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::search::SearchIndexField;

    use super::*;

    const FIELDS: &[SearchIndexField] = &[SearchIndexField::new("title", 'A')];

    fn definition() -> IndexDefinition {
        IndexDefinition::new("articles", "simple", FIELDS, None)
    }

    fn document(id: i64) -> SearchDocument {
        SearchDocument::new("articles", id).with_field("title", 'A', format!("record {id}"))
    }

    #[tokio::test]
    async fn fetching_a_missing_id_is_an_absence_not_an_error() {
        let source = MemoryDocumentSource::new();
        source.upsert_document(document(1));

        let found = source
            .fetch(&definition(), &[1, 2])
            .await
            .expect("fetch must not error on a missing row");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, 1);
    }

    #[tokio::test]
    async fn scanning_walks_ids_in_order_via_keyset_pagination() {
        let source = MemoryDocumentSource::new();
        for id in 1..=5 {
            source.upsert_document(document(id));
        }

        let first = source.scan(&definition(), None, 2).await.expect("scan");
        assert_eq!(first.iter().map(|d| d.id).collect::<Vec<_>>(), vec![1, 2]);

        let second = source.scan(&definition(), Some(2), 2).await.expect("scan");
        assert_eq!(second.iter().map(|d| d.id).collect::<Vec<_>>(), vec![3, 4]);

        let last = source.scan(&definition(), Some(5), 2).await.expect("scan");
        assert!(last.is_empty());
    }

    #[tokio::test]
    async fn scanning_is_scoped_to_one_index() {
        let source = MemoryDocumentSource::new();
        source.upsert_document(document(1));
        source.upsert_document(SearchDocument::new("notes", 1).with_field("title", 'A', "note"));

        let articles = source.scan(&definition(), None, 10).await.expect("scan");
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].index, "articles");
    }

    #[tokio::test]
    async fn fetch_calls_are_counted_per_id() {
        let source = MemoryDocumentSource::new();
        source.upsert_document(document(1));
        assert_eq!(source.fetch_calls_for(1), 0);
        source.fetch(&definition(), &[1]).await.expect("fetch");
        source.fetch(&definition(), &[1]).await.expect("fetch");
        assert_eq!(source.fetch_calls_for(1), 2);
        assert_eq!(source.fetch_calls_for(9), 0);
    }
}

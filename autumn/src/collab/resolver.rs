//! Convergent conflict resolution for the offline-sync engine.
//!
//! The sync engine's default is last-write-wins: when two devices write the
//! same row, the older write is discarded. That is the right default for a
//! scalar column and the wrong one for a collaborative field, where both
//! writes are real text somebody typed.
//!
//! [`CollabResolver`] wraps another [`ConflictResolver`] and changes the
//! verdict for the named fields only: it merges the two documents and keeps
//! the wrapped resolver's answer for everything else in the row.
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use autumn_web::collab::CollabResolver;
//! use autumn_web::sync::{SyncBackend, server};
//!
//! # fn wire(backend: Arc<dyn SyncBackend>) -> axum::Router {
//! server::router(backend, Arc::new(CollabResolver::new(["body"])))
//! # }
//! ```

use serde_json::{Map, Value};

use crate::sync::protocol::{Change, Op, RemoteRow};
use crate::sync::resolver::{ConflictResolver, LwwResolver, Resolution};

use super::registry::collaborative_columns_for_table;
use super::text::{CollabText, MAX_WIRE_ELEMENTS, MAX_WIRE_PENDING};

/// A conflict resolver that merges collaborative fields instead of
/// overwriting them.
///
/// Every other field keeps the wrapped resolver's verdict, so a row that
/// mixes a collaborative body with ordinary columns behaves the way the app
/// already expects for those columns.
///
/// [`CollabResolver::new`] matches a field name in **every** collection;
/// [`CollabResolver::for_table`] and [`CollabResolver::in_collection`] scope
/// it to one. Either way the merge only engages when **both** sides of the
/// conflict decode as documents, so a same-named field holding anything else
/// keeps the wrapped verdict.
#[derive(Debug, Clone)]
pub struct CollabResolver<R = LwwResolver> {
    fields: Vec<String>,
    /// Collection this resolver applies to. `None` means every collection.
    collection: Option<String>,
    inner: R,
}

impl CollabResolver<LwwResolver> {
    /// Merge the named fields in **every** collection; fall back to
    /// last-write-wins for the rest.
    ///
    /// Use [`for_table`](Self::for_table), or
    /// [`in_collection`](Self::in_collection), when a same-named field on
    /// another collection must keep last-write-wins.
    #[must_use]
    pub fn new<S: Into<String>>(fields: impl IntoIterator<Item = S>) -> Self {
        Self::with_fallback(fields, LwwResolver)
    }

    /// Merge every `#[collaborative]` column registered for `table`, **in
    /// that collection only**.
    ///
    /// Reads the registry the `#[model]` macro fills in, so adding a marker
    /// to the model is enough — the resolver does not need editing too.
    #[must_use]
    pub fn for_table(table: &str) -> Self {
        Self::new(collaborative_columns_for_table(table)).in_collection(table)
    }
}

impl<R> CollabResolver<R> {
    /// Merge the named fields; hand every other decision to `inner`.
    #[must_use]
    pub fn with_fallback<S: Into<String>>(fields: impl IntoIterator<Item = S>, inner: R) -> Self {
        Self {
            fields: fields.into_iter().map(Into::into).collect(),
            collection: None,
            inner,
        }
    }

    /// Apply only to `collection`; every other collection keeps the wrapped
    /// resolver's verdict.
    #[must_use]
    pub fn in_collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = Some(collection.into());
        self
    }

    /// The field names this resolver merges.
    #[must_use]
    pub fn fields(&self) -> &[String] {
        &self.fields
    }

    /// The collection this resolver is scoped to, if any.
    #[must_use]
    pub fn collection(&self) -> Option<&str> {
        self.collection.as_deref()
    }
}

impl<R: ConflictResolver> ConflictResolver for CollabResolver<R> {
    fn resolve(&self, client_device_id: &str, client: &Change, server: &RemoteRow) -> Resolution {
        let verdict = self.inner.resolve(client_device_id, client, server);

        // Out of scope: another collection's same-named field keeps the
        // wrapped verdict.
        if self
            .collection
            .as_ref()
            .is_some_and(|only| only != &client.collection)
        {
            return verdict;
        }

        // A row-level conflict is not a field merge: a delete on either side
        // is a decision about whether the row exists, which the wrapped
        // resolver owns. Merging would resurrect a deleted row silently.
        if client.op == Op::Delete || server.deleted {
            return verdict;
        }
        let (Some(Value::Object(from_client)), Some(Value::Object(from_server))) =
            (client.payload.as_ref(), server.payload.as_ref())
        else {
            return verdict;
        };

        // Start from whichever document the wrapped resolver chose, then
        // overwrite only the collaborative fields.
        let mut merged: Map<String, Value> = match &verdict {
            Resolution::TakeClient => from_client.clone(),
            Resolution::KeepServer => from_server.clone(),
            Resolution::Merge(Value::Object(base)) => base.clone(),
            // A resolver that merged into something other than an object has
            // already replaced the row's shape; do not second-guess it.
            Resolution::Merge(_) => return verdict,
        };

        let mut changed = false;
        for field in &self.fields {
            match (from_client.get(field), from_server.get(field)) {
                (Some(mine), Some(theirs)) => {
                    // Both sides must be documents. If either is not, the
                    // field is not really collaborative on this row and the
                    // wrapped verdict already covers it.
                    let (Ok(mut mine), Ok(theirs)) = (
                        serde_json::from_value::<CollabText>(mine.clone()),
                        serde_json::from_value::<CollabText>(theirs.clone()),
                    ) else {
                        continue;
                    };
                    mine.merge(&theirs);
                    // Two documents inside the limits can merge into one that
                    // is not: 6 000 distinct elements each make 12 000. Storing
                    // it would be worse than not merging — every later read
                    // would refuse it, this resolver would fall through to
                    // last-write-wins, and the side it drops would be gone for
                    // good. Leave the wrapped verdict instead, which is at
                    // least a document that can still be read.
                    if mine.element_count() > MAX_WIRE_ELEMENTS
                        || mine.pending_len() > MAX_WIRE_PENDING
                    {
                        tracing::warn!(
                            field,
                            elements = mine.element_count(),
                            pending = mine.pending_len(),
                            "collab: merged document is past the wire limits; leaving the \
                             wrapped verdict rather than storing one that cannot be read"
                        );
                        continue;
                    }
                    let Ok(encoded) = serde_json::to_value(&mine) else {
                        continue;
                    };
                    merged.insert(field.clone(), encoded);
                    changed = true;
                }
                // Present on one side only: that side is the whole document,
                // whichever way the wrapped resolver leaned.
                (Some(only), None) | (None, Some(only)) => {
                    if serde_json::from_value::<CollabText>(only.clone()).is_ok() {
                        merged.insert(field.clone(), only.clone());
                        changed = true;
                    }
                }
                (None, None) => {}
            }
        }

        if changed {
            Resolution::Merge(Value::Object(merged))
        } else {
            verdict
        }
    }
}

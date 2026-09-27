//! Off-request, durable index maintenance.
//!
//! AC: indexing runs "via `#[job]`, so indexing is off-request and durable".
//!
//! Two jobs, both **index-name keyed** rather than model-keyed:
//!
//! - `autumn_search_reindex` — converge one record.
//! - `autumn_search_backfill` — rebuild one index (or all of them).
//!
//! Keying on the index name is what keeps adding a searchable model to a
//! one-line `SearchPlugin::index::<Model>()` instead of a new job per model:
//! the handler looks the definition up in the registry and drives the generic
//! [`crate::DocumentSource`]. Nothing here is generated per model.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use autumn_web::job::{JobHandler, JobInfo, JobUniqueness, JobUniquenessWindow};
use autumn_web::{AppState, AutumnError, AutumnResult};
use serde::{Deserialize, Serialize};

use crate::client::{BackfillOptions, SearchClient};

/// Job name for the per-record reindex. A wire contract: in-flight payloads
/// outlive a deploy, so this string must not change.
pub const REINDEX_JOB: &str = "autumn_search_reindex";

/// Job name for the full backfill.
pub const BACKFILL_JOB: &str = "autumn_search_backfill";

/// What a reindex instruction should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReindexOp {
    /// The record was created or updated. Re-reads the row and writes it — or
    /// deletes the document if the row is gone. Create and update are the same
    /// instruction, because both mean "the row changed; make the index agree".
    Upsert,
    /// The record was deleted. Still re-reads the source and converges — a
    /// row that has been recreated under the same primary key must survive a
    /// late or retried delete. With no `DocumentSource` installed this falls
    /// back to removing the document outright, which needs no source.
    Delete,
}

/// Payload field naming the per-record concurrency scope.
///
/// The framework resolves a concurrency scope from **one** payload field, so
/// the composite `(index, id)` key has to exist as a field rather than be
/// computed from two. See [`reindex_concurrency`].
pub const REINDEX_SCOPE_FIELD: &str = "scope";

/// At most one in-flight reindex per record.
///
/// Scoped by [`REINDEX_SCOPE_FIELD`], so distinct records still reindex fully
/// in parallel — the cap is per `(index, id)`, not per job type.
#[must_use]
pub fn reindex_concurrency() -> autumn_web::job::JobConcurrency {
    autumn_web::job::JobConcurrency {
        limit: 1,
        key: Some(REINDEX_SCOPE_FIELD.to_owned()),
    }
}

/// Payload of the [`REINDEX_JOB`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReindexArgs {
    /// The index to update.
    pub index: String,
    /// Primary key of the record.
    pub id: i64,
    /// The operation.
    pub op: ReindexOp,
    /// `"{index}:{id}"` — the per-record concurrency scope.
    ///
    /// Denormalized into the payload on purpose: it is redundant with `index`
    /// and `id`, but the concurrency limiter reads a single named field, and
    /// scoping on `id` alone would needlessly serialize record 7 of one index
    /// against record 7 of another (auto-increment keys collide constantly
    /// across tables).
    ///
    /// `#[serde(default)]` so a payload enqueued by an older deploy still
    /// deserializes; it lands in a shared scope, which over-serializes for the
    /// length of the rollout rather than losing the guarantee.
    #[serde(default)]
    pub scope: String,
}

impl ReindexArgs {
    /// An upsert instruction (create or update).
    #[must_use]
    pub fn upsert(index: impl Into<String>, id: i64) -> Self {
        Self::new(index, id, ReindexOp::Upsert)
    }

    /// A delete instruction.
    #[must_use]
    pub fn delete(index: impl Into<String>, id: i64) -> Self {
        Self::new(index, id, ReindexOp::Delete)
    }

    /// Build an instruction, deriving [`scope`](Self::scope).
    fn new(index: impl Into<String>, id: i64, op: ReindexOp) -> Self {
        let index = index.into();
        Self {
            scope: format!("{index}:{id}"),
            index,
            id,
            op,
        }
    }

    /// The dedup key for this instruction.
    ///
    /// Repeated writes to the same record inside one queue window collapse to
    /// one reindex — every instruction re-reads the row, so they are all the
    /// same operation and only one need run. Deliberately **not** keyed on
    /// `op`: an upsert and a delete for one record are interchangeable under
    /// this scheme, so collapsing them is safe rather than lossy.
    #[must_use]
    pub fn unique_key(&self) -> String {
        self.scope.clone()
    }
}

/// Payload of the [`BACKFILL_JOB`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BackfillArgs {
    /// Index to rebuild. `None` rebuilds every registered index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Rows per batch. `None` uses the configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<usize>,
    /// Clear each index before rebuilding it.
    #[serde(default)]
    pub purge: bool,
}

impl BackfillArgs {
    /// Rebuild one index.
    #[must_use]
    pub fn for_index(index: impl Into<String>) -> Self {
        Self {
            index: Some(index.into()),
            ..Self::default()
        }
    }

    /// The [`BackfillOptions`] this payload describes.
    #[must_use]
    pub fn options(&self, default_batch_size: usize) -> BackfillOptions {
        BackfillOptions::default()
            .batch_size(self.batch_size.unwrap_or(default_batch_size))
            .purge(self.purge)
    }
}

/// The boxed future a [`JobHandler`] returns.
type JobFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;

/// Pull the installed [`SearchClient`] off application state.
///
/// # Errors
///
/// Returns an error naming the missing builder call when the plugin was not
/// installed, rather than silently skipping the index update.
pub fn client_from_state(state: &AppState) -> AutumnResult<Arc<SearchClient>> {
    state.extension::<SearchClient>().ok_or_else(|| {
        AutumnError::internal_server_error_msg(
            "the SearchClient extension is not installed; add \
             `.plugin(SearchPlugin::new()…)` to the app builder",
        )
    })
}

fn reindex_handler(state: AppState, args: serde_json::Value) -> JobFuture {
    Box::pin(async move {
        let args: ReindexArgs = serde_json::from_value(args).map_err(|e| {
            AutumnError::internal_server_error_msg(format!("invalid reindex payload: {e}"))
        })?;
        let client = client_from_state(&state)?;
        client.reindex(&args).await?;
        Ok(())
    })
}

fn backfill_handler(state: AppState, args: serde_json::Value) -> JobFuture {
    Box::pin(async move {
        let args: BackfillArgs = serde_json::from_value(args).map_err(|e| {
            AutumnError::internal_server_error_msg(format!("invalid backfill payload: {e}"))
        })?;
        let client = client_from_state(&state)?;
        // The app's configured `[search] batch_size`, not the compiled-in
        // default — otherwise configuring it would only affect the CLI path.
        let options = args.options(client.default_batch_size());
        match &args.index {
            Some(index) => {
                client.backfill(index, &options).await?;
            }
            None => {
                client.backfill_all(&options).await?;
            }
        }
        Ok(())
    })
}

/// The [`JobInfo`] set for the search jobs, routed to `queue`.
///
/// Mirrors `autumn-media-plugin`'s `media_job_infos`: the queue is rewritten
/// at registration so an application's `SearchPlugin::queue(...)` override
/// actually takes effect (the enqueue chokepoint routes by the registered
/// `JobInfo`).
#[must_use]
pub fn search_job_infos(queue: &str) -> Vec<JobInfo> {
    let reindex: JobHandler = reindex_handler;
    let backfill: JobHandler = backfill_handler;

    // Repeated writes to the same record collapse to one pending reindex. The
    // job re-reads the row, so N queued reindexes of the same id would each
    // read the same final state — only the last does distinct work. Keyed on
    // `index` + `id` (NOT `op`), because an upsert and a delete for the same
    // record must not both sit in the queue racing each other.
    let mut reindex = JobInfo::new(REINDEX_JOB, 5, 250, reindex);
    reindex.uniqueness = Some(JobUniqueness {
        by: vec!["index".to_owned(), "id".to_owned()],
        // Released when the job starts, not when it finishes: a write that
        // lands *while* a reindex is running still needs its own reindex, or
        // the index would keep the pre-write text.
        window: JobUniquenessWindow::Pending,
    });
    // …and because the key is released at start, two jobs for one record can
    // be in flight at once on a multi-worker deployment. Both re-read the
    // source, so they interleave as: A reads (state 1) → write lands (state 2)
    // → B reads (state 2) → B writes state 2 → A writes state 1. The index
    // keeps the STALE text until the next mutation or backfill, and the same
    // ordering resurrects a document whose row B had just seen deleted. The
    // window is not narrow either: an embedding provider call sits between A's
    // read and A's write.
    //
    // A per-record cap of one closes it without giving up the follow-up. The
    // second job is still enqueued immediately (that is what `Pending` buys),
    // it simply cannot start until the first finishes — at which point it
    // re-reads and converges on the latest state.
    reindex.concurrency = Some(reindex_concurrency());

    // A backfill is long and expensive; a tight retry loop would stampede, and
    // two concurrent full rebuilds of one index are pure waste.
    let mut backfill = JobInfo::new(BACKFILL_JOB, 3, 30_000, backfill);
    backfill.uniqueness = Some(JobUniqueness {
        by: vec!["index".to_owned()],
        window: JobUniquenessWindow::Running,
    });

    [reindex, backfill]
        .into_iter()
        .map(|mut info| {
            queue.clone_into(&mut info.queue);
            info
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_jobs_are_registered_on_the_requested_queue() {
        let infos = search_job_infos("indexing");
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec![REINDEX_JOB, BACKFILL_JOB]);
        assert!(infos.iter().all(|i| i.queue == "indexing"));
    }

    #[test]
    fn the_backfill_backs_off_much_harder_than_a_reindex() {
        let infos = search_job_infos("search");
        let reindex = infos
            .iter()
            .find(|i| i.name == REINDEX_JOB)
            .expect("reindex");
        let backfill = infos
            .iter()
            .find(|i| i.name == BACKFILL_JOB)
            .expect("backfill");
        assert!(backfill.initial_backoff_ms > reindex.initial_backoff_ms);
    }

    #[test]
    fn reindex_is_capped_at_one_in_flight_job_per_record() {
        // Uniqueness releases the key when the job STARTS, so without this cap
        // two jobs for one record run concurrently and the slower one's stale
        // read overwrites the newer one's write.
        let reindex = search_job_infos("search")
            .into_iter()
            .find(|i| i.name == REINDEX_JOB)
            .expect("reindex");
        let concurrency = reindex.concurrency.expect("a per-record cap");
        assert_eq!(concurrency.limit, 1);
        assert_eq!(concurrency.key.as_deref(), Some(REINDEX_SCOPE_FIELD));
    }

    #[test]
    fn the_concurrency_scope_is_a_real_payload_field_and_is_per_record() {
        // The limiter resolves the scope by looking `key` up in the serialized
        // payload. If the field name and the declared key ever drift, EVERY
        // payload resolves to the same missing-field scope and a limit of 1
        // silently becomes a global serialization of all reindexing — slow,
        // and with no signal that the per-record guarantee was lost.
        let args = ReindexArgs::upsert("articles", 7);
        let json = serde_json::to_value(&args).expect("serialize");
        assert_eq!(
            json.get(REINDEX_SCOPE_FIELD).and_then(|v| v.as_str()),
            Some("articles:7"),
            "the declared concurrency key must name a field that is actually there: {json}"
        );

        // Per RECORD, not per id: auto-increment keys collide across tables,
        // and scoping on `id` alone would serialize unrelated records.
        assert_ne!(
            ReindexArgs::upsert("articles", 7).scope,
            ReindexArgs::upsert("notes", 7).scope
        );
        assert_ne!(
            ReindexArgs::upsert("articles", 7).scope,
            ReindexArgs::upsert("articles", 8).scope
        );
        // An upsert and a delete for one record DO share a scope: they are the
        // pair that must not interleave.
        assert_eq!(
            ReindexArgs::upsert("articles", 7).scope,
            ReindexArgs::delete("articles", 7).scope
        );
    }

    #[test]
    fn a_payload_without_a_scope_still_deserializes() {
        // An in-flight payload enqueued by an older deploy has no `scope`. It
        // must still run — it lands in a shared concurrency scope, which
        // over-serializes for the length of the rollout rather than failing.
        let args: ReindexArgs =
            serde_json::from_str(r#"{"index":"articles","id":7,"op":"upsert"}"#).expect("legacy");
        assert_eq!(args.index, "articles");
        assert_eq!(args.id, 7);
        assert_eq!(args.op, ReindexOp::Upsert);
        assert_eq!(args.scope, "");
    }

    #[test]
    fn reindex_args_round_trip() {
        for args in [
            ReindexArgs::upsert("articles", 1),
            ReindexArgs::delete("articles", 2),
        ] {
            let json = serde_json::to_value(&args).expect("serialize");
            let back: ReindexArgs = serde_json::from_value(json).expect("deserialize");
            assert_eq!(back, args);
        }
    }

    #[test]
    fn the_reindex_op_wire_format_is_snake_case() {
        let json = serde_json::to_value(ReindexArgs::delete("articles", 1)).expect("serialize");
        assert_eq!(json["op"], serde_json::json!("delete"));
        assert_eq!(json["index"], serde_json::json!("articles"));
        assert_eq!(json["id"], serde_json::json!(1));
    }

    #[test]
    fn the_dedup_key_collapses_repeated_writes_to_one_record() {
        assert_eq!(
            ReindexArgs::upsert("articles", 7).unique_key(),
            ReindexArgs::delete("articles", 7).unique_key()
        );
        assert_ne!(
            ReindexArgs::upsert("articles", 7).unique_key(),
            ReindexArgs::upsert("articles", 8).unique_key()
        );
        assert_ne!(
            ReindexArgs::upsert("articles", 7).unique_key(),
            ReindexArgs::upsert("notes", 7).unique_key()
        );
    }

    #[test]
    fn the_reindex_job_actually_declares_that_dedup_to_the_queue() {
        // The key above is only real if it is registered: `JobInfo.uniqueness`
        // is what the enqueue chokepoint consults. Without this the doc on
        // `unique_key` would be a claim about nothing.
        let infos = search_job_infos("search");
        let reindex = infos
            .iter()
            .find(|i| i.name == REINDEX_JOB)
            .expect("reindex");
        let uniqueness = reindex.uniqueness.as_ref().expect("reindex must dedup");
        assert_eq!(uniqueness.by, vec!["index".to_owned(), "id".to_owned()]);
        // Not keyed on `op`: an upsert and a delete for one record must not
        // both sit in the queue racing each other.
        assert!(!uniqueness.by.contains(&"op".to_owned()));
        // Released when the job starts, so a write landing mid-reindex still
        // gets its own reindex.
        assert_eq!(uniqueness.window, JobUniquenessWindow::Pending);
    }

    #[test]
    fn concurrent_full_rebuilds_of_one_index_are_deduped_while_running() {
        let infos = search_job_infos("search");
        let backfill = infos
            .iter()
            .find(|i| i.name == BACKFILL_JOB)
            .expect("backfill");
        let uniqueness = backfill.uniqueness.as_ref().expect("backfill must dedup");
        assert_eq!(uniqueness.by, vec!["index".to_owned()]);
        assert_eq!(uniqueness.window, JobUniquenessWindow::Running);
    }

    #[test]
    fn backfill_args_default_to_every_index() {
        let args = BackfillArgs::default();
        assert!(args.index.is_none());
        assert!(!args.purge);
        assert_eq!(args.options(500).batch_size, 500);
        assert_eq!(args.options(500).effective_batch_size(), 500);
    }

    #[test]
    fn backfill_args_round_trip_and_omit_absent_fields() {
        let args = BackfillArgs::for_index("articles");
        let json = serde_json::to_value(&args).expect("serialize");
        assert_eq!(json["index"], serde_json::json!("articles"));
        assert!(json.get("batch_size").is_none());
        let back: BackfillArgs = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, args);
    }

    #[test]
    fn an_explicit_batch_size_overrides_the_configured_default() {
        let args = BackfillArgs {
            batch_size: Some(10),
            ..BackfillArgs::default()
        };
        assert_eq!(args.options(500).batch_size, 10);
    }
}

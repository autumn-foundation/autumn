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

/// Payload of the [`REINDEX_JOB`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReindexArgs {
    /// The index to update.
    pub index: String,
    /// Primary key of the record.
    pub id: i64,
    /// The operation.
    pub op: ReindexOp,
}

impl ReindexArgs {
    /// An upsert instruction (create or update).
    #[must_use]
    pub fn upsert(index: impl Into<String>, id: i64) -> Self {
        Self {
            index: index.into(),
            id,
            op: ReindexOp::Upsert,
        }
    }

    /// A delete instruction.
    #[must_use]
    pub fn delete(index: impl Into<String>, id: i64) -> Self {
        Self {
            index: index.into(),
            id,
            op: ReindexOp::Delete,
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
        format!("{}:{}", self.index, self.id)
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

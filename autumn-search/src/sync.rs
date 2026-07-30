//! Lifecycle-driven index sync.
//!
//! AC: "create/update/delete to a searchable model **enqueues a reindex**".
//!
//! [`SearchSyncHooks`] is a ready-made `MutationHooks` implementation that
//! does exactly that and nothing else. Wire it once per searchable repository:
//!
//! ```rust,ignore
//! #[repository(
//!     Article,
//!     hooks = SearchSyncHooks<Article, NewArticle, UpdateArticle>,
//!     commit_hooks = true,
//! )]
//! pub trait ArticleRepository {}
//! ```
//!
//! Two deliberate choices:
//!
//! - It hangs off the **`after_*_commit`** hooks, so a rolled-back transaction
//!   never leaves a phantom document, and (with `commit_hooks = true`) the
//!   intent is written to Autumn's durable commit-hook queue inside the same
//!   transaction as the mutation. A process dying between commit and enqueue
//!   is recovered by the queue, not lost.
//! - It enqueues `(index, id)` and **not the record**. The job re-reads the
//!   row, so a stale or reordered payload cannot write stale text into the
//!   index — see [`crate::SearchClient::reindex`].
//!
//! An app that already has hooks composes instead of replacing: call
//! [`enqueue_reindex`] from its own `after_*_commit`.

use std::marker::PhantomData;

use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks};
use autumn_web::search::SearchIndexed;

use crate::jobs::{REINDEX_JOB, ReindexArgs};

/// Enqueue a reindex for one record.
///
/// The single seam an application needs if it is writing its own hooks, a
/// bulk-import routine, or a repair script.
///
/// `MutationHooks` receives no `AppState`, so this goes through autumn-web's
/// process-wide name-keyed enqueue chokepoint, which resolves the registered
/// `JobInfo` — and therefore the plugin's configured queue — from the job
/// registry.
///
/// # Errors
///
/// Propagates the enqueue failure. Callers inside an `after_*_commit` hook
/// should return it: with `commit_hooks = true` the durable queue retries the
/// hook, which is exactly the recovery you want.
pub async fn enqueue_reindex(args: &ReindexArgs) -> AutumnResult<()> {
    autumn_web::job::enqueue(REINDEX_JOB, serde_json::to_value(args)?).await
}

/// Enqueue an upsert reindex for `record`.
///
/// # Errors
///
/// As [`enqueue_reindex`].
pub async fn enqueue_reindex_for<M: SearchIndexed>(record: &M) -> AutumnResult<()> {
    enqueue_reindex(&ReindexArgs::upsert(M::SEARCH_INDEX, record.search_id())).await
}

/// Enqueue a delete reindex for `record`.
///
/// # Errors
///
/// As [`enqueue_reindex`].
pub async fn enqueue_unindex_for<M: SearchIndexed>(record: &M) -> AutumnResult<()> {
    enqueue_reindex(&ReindexArgs::delete(M::SEARCH_INDEX, record.search_id())).await
}

/// `MutationHooks` that keep a searchable model's index in sync.
///
/// Generic over the model and its two changesets because that is the shape
/// `#[repository]` requires; nothing else about it is per-model.
pub struct SearchSyncHooks<M, N, U> {
    // `fn() -> (…)` rather than the tuple itself, so the hooks are `Send +
    // Sync` regardless of the model's own auto traits.
    _model: ModelMarker<M, N, U>,
}

/// Variance/auto-trait marker for [`SearchSyncHooks`].
type ModelMarker<M, N, U> = PhantomData<fn() -> (M, N, U)>;

impl<M, N, U> SearchSyncHooks<M, N, U> {
    /// Construct the hooks.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            _model: PhantomData,
        }
    }
}

impl<M, N, U> Default for SearchSyncHooks<M, N, U> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M, N, U> Clone for SearchSyncHooks<M, N, U> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<M, N, U> std::fmt::Debug for SearchSyncHooks<M, N, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SearchSyncHooks")
    }
}

impl<M, N, U> MutationHooks for SearchSyncHooks<M, N, U>
where
    M: SearchIndexed,
    N: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    type Model = M;
    type NewModel = N;
    type UpdateModel = U;

    fn after_create_commit(
        &self,
        _ctx: &mut MutationContext,
        record: &Self::Model,
    ) -> impl Future<Output = AutumnResult<()>> + Send {
        let args = ReindexArgs::upsert(M::SEARCH_INDEX, record.search_id());
        async move { enqueue_reindex(&args).await }
    }

    fn after_update_commit(
        &self,
        _ctx: &mut MutationContext,
        record: &Self::Model,
    ) -> impl Future<Output = AutumnResult<()>> + Send {
        let args = ReindexArgs::upsert(M::SEARCH_INDEX, record.search_id());
        async move { enqueue_reindex(&args).await }
    }

    fn after_delete_commit(
        &self,
        _ctx: &mut MutationContext,
        record: &Self::Model,
    ) -> impl Future<Output = AutumnResult<()>> + Send {
        let args = ReindexArgs::delete(M::SEARCH_INDEX, record.search_id());
        async move { enqueue_reindex(&args).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hooks_type_is_default_and_clone_as_repositories_require() {
        // `#[repository(hooks = …)]` constructs via `Default` and the
        // generated struct derives `Clone`, so both must hold for a type with
        // no `Default`/`Clone` bound on its parameters.
        struct NotClonable;
        let hooks: SearchSyncHooks<NotClonable, NotClonable, NotClonable> =
            SearchSyncHooks::default();
        let _cloned = hooks.clone();
        assert_eq!(format!("{hooks:?}"), "SearchSyncHooks");
    }
}

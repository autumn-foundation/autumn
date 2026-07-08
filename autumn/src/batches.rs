//! Bounded-memory, keyset-based iteration over an entire table.
//!
//! `find_all()` materializes the whole table into one `Vec` — an instant OOM
//! on a million-row table inside a `#[autumn_web::task]`, scheduled sweep, or
//! job. The [`BatchSource`]-backed iterators here walk the table in
//! `batch_size`-sized chunks using a primary-key ascending **keyset** cursor
//! (`WHERE id > last ORDER BY id ASC LIMIT batch_size`), so peak additional
//! memory is bounded by `batch_size` regardless of table size, and deep
//! iteration never degrades the way `LIMIT`/`OFFSET` does.
//!
//! The `#[autumn_web::repository]` macro generates two entry points on every
//! repository:
//!
//! - [`find_in_batches(batch_size)`](FindInBatches) — yields successive
//!   `Vec<Model>` chunks of at most `batch_size` rows.
//! - [`find_each(batch_size)`](FindEach) — a convenience over the former that
//!   yields one `Model` at a time while still fetching in bounded batches.
//!
//! Both inherit the repository's soft-delete filter, tenant scoping and read
//! routing (replica / `primary_reads`) for free, because they share the same
//! connection-acquisition path as `find_all`/`cursor_page`.
//!
//! Iteration ends at the first short batch (fewer than `batch_size` rows);
//! rows inserted after that point are not seen by the current handle — start
//! a new iteration to pick them up (matching Rails' `find_each`).
//!
//! # Errors are retryable
//!
//! When a batch query fails, [`FindInBatches::next_batch`] returns the error
//! **without** ending the iteration: the keyset cursor is only advanced on
//! success, so calling `next_batch()` again re-issues the same
//! `WHERE id > last` query and resumes exactly where the failure happened —
//! no duplicated and no skipped rows. A failed run is therefore never
//! mistaken for a completed one: `Ok(None)` means the table is exhausted,
//! nothing else.
//!
//! # Example — `find_each` in a task doing a per-row update
//!
//! ```rust,ignore
//! #[autumn_web::task(name = "backfill-slugs")]
//! pub async fn backfill_slugs(repo: PgPostRepository) -> AutumnResult<()> {
//!     let mut each = repo.find_each(500);
//!     while let Some(post) = each.next().await? {
//!         let slug = slugify(&post.title);
//!         repo.update(post.id, &UpdatePost { slug: Patch::Set(slug), ..Default::default() })
//!             .await?;
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Example — `find_in_batches` + `upsert_many` recompute loop
//!
//! ```rust,ignore
//! let mut batches = repo.find_in_batches(1_000);
//! while let Some(chunk) = batches.next_batch().await? {
//!     let recomputed: Vec<Account> = chunk
//!         .into_iter()
//!         .map(|mut a| { a.balance = recompute(&a); a })
//!         .collect();
//!     repo.upsert_many(&recomputed).await?; // O(batch_size) memory, not O(table)
//! }
//! ```
//!
//! (`upsert_many` writes back existing models; it is not generated on
//! repositories with hooks configured — fall back to per-row `update` there.)

use std::future::Future;

use crate::{AutumnError, AutumnResult};

/// A repository that can fetch a keyset batch of models ordered by ascending
/// primary key.
///
/// Implemented by the generated repository struct; you normally don't
/// implement this by hand — the macro generates it.
///
/// The generated implementation applies the repository's soft-delete filter,
/// tenant scoping, and read routing, mirroring `find_all`/`cursor_page`.
pub trait BatchSource: Send + Sync {
    /// The model type yielded by iteration.
    type Model: Send;

    /// Fetch up to `limit` models with `id > after_id` (or from the start of
    /// the table when `after_id` is `None`), ordered by `id` ascending.
    fn fetch_batch_after(
        &self,
        after_id: Option<i64>,
        limit: i64,
    ) -> impl Future<Output = AutumnResult<Vec<Self::Model>>> + Send;

    /// The primary key of a model, used to advance the keyset cursor.
    fn batch_key(model: &Self::Model) -> i64;
}

/// Iterator over successive `Vec<Model>` chunks of a table, each holding at
/// most `batch_size` rows.
///
/// Created by the generated `find_in_batches(batch_size)` repository method.
/// Drive it with [`next_batch`](FindInBatches::next_batch) in a `while let`
/// loop; each chunk should be dropped before requesting the next so that at
/// most one `batch_size` chunk of models is resident at a time.
pub struct FindInBatches<'a, S: BatchSource> {
    source: &'a S,
    batch_size: usize,
    /// `batch_size` as an `i64` query limit, clamped to `i64::MAX` rather
    /// than wrapping negative on a `> i64::MAX` batch size (a nonsensical
    /// request that would never fit in memory anyway).
    limit: i64,
    /// Exclusive lower bound on `id` for the next batch (`None` == from
    /// start). Only advanced on success, so a failed batch can be retried by
    /// calling `next_batch()` again.
    after: Option<i64>,
    /// Set once the table is exhausted (an empty or short batch was
    /// observed); further calls return `Ok(None)`. Never set on error.
    ended: bool,
}

impl<'a, S: BatchSource> FindInBatches<'a, S> {
    /// Construct a new batched iterator. Prefer the generated
    /// `find_in_batches` method over calling this directly.
    #[must_use]
    pub fn new(source: &'a S, batch_size: usize) -> Self {
        Self {
            source,
            batch_size,
            limit: i64::try_from(batch_size).unwrap_or(i64::MAX),
            after: None,
            ended: false,
        }
    }

    /// Fetch the next chunk of at most `batch_size` models.
    ///
    /// Returns `Ok(Some(chunk))` with `1..=batch_size` models, or `Ok(None)`
    /// once the table is exhausted (first short or empty batch). `Ok(None)`
    /// always means completion — a failed batch never turns into `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns an error if `batch_size` is `0` (a programmer error — every
    /// call errors), or if the underlying batch query fails. Errors are
    /// **retryable**: the keyset cursor is not advanced on failure, so
    /// calling `next_batch()` again re-issues the same `WHERE id > last`
    /// query and resumes with no duplicated or skipped rows.
    pub async fn next_batch(&mut self) -> AutumnResult<Option<Vec<S::Model>>> {
        if self.ended {
            return Ok(None);
        }
        if self.batch_size == 0 {
            // Programmer error (task/job code, not client input); surfaced on
            // every call so a retrying caller can never read it as success.
            return Err(AutumnError::internal_server_error_msg(
                "find_in_batches: batch_size must be greater than zero",
            ));
        }

        // On `Err` the cursor is untouched: the caller may retry this same
        // batch by calling `next_batch()` again.
        let batch = self
            .source
            .fetch_batch_after(self.after, self.limit)
            .await?;

        if batch.is_empty() {
            self.ended = true;
            return Ok(None);
        }

        // Advance the keyset cursor past the last row of this chunk.
        if let Some(last) = batch.last() {
            self.after = Some(S::batch_key(last));
        }

        // A short batch means the table is exhausted; the next query would
        // return empty, so end now and avoid the extra round-trip.
        if batch.len() < self.batch_size {
            self.ended = true;
        }

        Ok(Some(batch))
    }
}

/// Iterator that yields individual models one at a time while still fetching
/// the underlying rows in bounded `batch_size` chunks.
///
/// Created by the generated `find_each(batch_size)` repository method. A thin
/// convenience over [`FindInBatches`].
pub struct FindEach<'a, S: BatchSource> {
    batches: FindInBatches<'a, S>,
    buffer: std::vec::IntoIter<S::Model>,
}

impl<'a, S: BatchSource> FindEach<'a, S> {
    /// Construct a new per-row iterator. Prefer the generated `find_each`
    /// method over calling this directly.
    #[must_use]
    pub fn new(source: &'a S, batch_size: usize) -> Self {
        Self {
            batches: FindInBatches::new(source, batch_size),
            buffer: Vec::new().into_iter(),
        }
    }

    /// Fetch the next model, pulling a fresh batch when the current buffer is
    /// drained.
    ///
    /// Returns `Ok(Some(model))`, or `Ok(None)` once the table is exhausted.
    ///
    /// # Errors
    ///
    /// Returns an error if `batch_size` is `0` or an underlying batch query
    /// fails. Like [`FindInBatches::next_batch`], errors are retryable:
    /// calling `next()` again retries the failed batch from the same keyset
    /// position.
    pub async fn next(&mut self) -> AutumnResult<Option<S::Model>> {
        loop {
            if let Some(model) = self.buffer.next() {
                return Ok(Some(model));
            }
            match self.batches.next_batch().await? {
                Some(chunk) => self.buffer = chunk.into_iter(),
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Live/high-water-mark counters shared between a fake source and the
    /// guards embedded in the models it hands out.
    #[derive(Debug, Default)]
    struct Counters {
        live: AtomicUsize,
        max_live: AtomicUsize,
    }

    /// Drop-tracking guard: increments the live count on creation (recording
    /// the high-water mark) and decrements it on drop.
    #[derive(Debug)]
    struct Guard {
        counters: Arc<Counters>,
    }

    impl Guard {
        fn new(counters: Arc<Counters>) -> Self {
            let live = counters.live.fetch_add(1, Ordering::SeqCst) + 1;
            counters.max_live.fetch_max(live, Ordering::SeqCst);
            Self { counters }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.counters.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct FakeModel {
        id: i64,
        _guard: Guard,
    }

    /// In-memory `BatchSource` over ids `1..=total` that instruments model
    /// residency, and can be told to fail specific fetch calls.
    struct FakeSource {
        total: i64,
        counters: Arc<Counters>,
        calls: AtomicUsize,
        /// 1-based indices of fetch calls that should fail.
        fail_on_calls: &'static [usize],
    }

    impl FakeSource {
        fn new(total: i64) -> Self {
            Self {
                total,
                counters: Arc::new(Counters::default()),
                calls: AtomicUsize::new(0),
                fail_on_calls: &[],
            }
        }
    }

    impl BatchSource for FakeSource {
        type Model = FakeModel;

        async fn fetch_batch_after(
            &self,
            after_id: Option<i64>,
            limit: i64,
        ) -> AutumnResult<Vec<FakeModel>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_on_calls.contains(&call) {
                return Err(AutumnError::internal_server_error_msg(
                    "injected batch failure",
                ));
            }
            let start = after_id.unwrap_or(0) + 1;
            Ok((start..=self.total)
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .map(|id| FakeModel {
                    id,
                    _guard: Guard::new(Arc::clone(&self.counters)),
                })
                .collect())
        }

        fn batch_key(model: &FakeModel) -> i64 {
            model.id
        }
    }

    /// AC3: `find_in_batches` never holds more than `batch_size` live models
    /// at once — instrumented via drop-counting guards, asserting the actual
    /// high-water mark, not just per-chunk length.
    #[tokio::test]
    async fn find_in_batches_high_water_mark_is_bounded_by_batch_size() {
        const N: i64 = 25;
        const B: usize = 4;

        let source = FakeSource::new(N);
        let mut yielded = Vec::new();
        let mut batches = FindInBatches::new(&source, B);
        while let Some(chunk) = batches.next_batch().await.expect("fetch") {
            assert!(chunk.len() <= B);
            for model in &chunk {
                yielded.push(model.id);
            }
            // Chunk dropped here, before the next fetch.
        }

        assert_eq!(yielded, (1..=N).collect::<Vec<_>>(), "exactly N, in order");
        let max = source.counters.max_live.load(Ordering::SeqCst);
        assert!(max <= B, "high-water mark {max} exceeded batch_size {B}");
        assert_eq!(
            source.counters.live.load(Ordering::SeqCst),
            0,
            "all models dropped after iteration"
        );
    }

    /// AC3: `find_each` buffers at most one batch, so its live-model
    /// high-water mark is also bounded by `batch_size`.
    #[tokio::test]
    async fn find_each_high_water_mark_is_bounded_by_batch_size() {
        const N: i64 = 25;
        const B: usize = 4;

        let source = FakeSource::new(N);
        let mut yielded = Vec::new();
        let mut each = FindEach::new(&source, B);
        while let Some(model) = each.next().await.expect("fetch") {
            yielded.push(model.id);
            // Model dropped here, before the next call.
        }

        assert_eq!(yielded, (1..=N).collect::<Vec<_>>(), "exactly N, in order");
        let max = source.counters.max_live.load(Ordering::SeqCst);
        assert!(max <= B, "high-water mark {max} exceeded batch_size {B}");
        assert_eq!(source.counters.live.load(Ordering::SeqCst), 0);
    }

    /// M2: a failed batch is retryable — the cursor does not advance on
    /// error, so the retry resumes from the same keyset position with no
    /// duplicated or skipped rows, and an error is never fused into
    /// `Ok(None)`.
    #[tokio::test]
    async fn failed_batch_is_retryable_without_dupes_or_gaps() {
        const N: i64 = 10;
        const B: usize = 3;

        let source = FakeSource {
            fail_on_calls: &[2],
            ..FakeSource::new(N)
        };
        let mut batches = FindInBatches::new(&source, B);

        let first = batches.next_batch().await.expect("call 1 ok").unwrap();
        assert_eq!(first.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 2, 3]);

        // Call 2 fails; the iterator must surface it, not end.
        batches
            .next_batch()
            .await
            .expect_err("call 2 injected failure");

        // Retry resumes from id > 3: same batch, no dupes, no gaps.
        let mut yielded: Vec<i64> = first.iter().map(|m| m.id).collect();
        while let Some(chunk) = batches.next_batch().await.expect("retry ok") {
            yielded.extend(chunk.iter().map(|m| m.id));
        }
        assert_eq!(yielded, (1..=N).collect::<Vec<_>>());
    }

    /// m2: `batch_size == 0` is a programmer error — every call errors (it is
    /// never converted into an `Ok(None)` "completed" signal), and the error
    /// is the internal-server-error kind, not a client `bad_request`.
    #[tokio::test]
    async fn batch_size_zero_errors_on_every_call() {
        let source = FakeSource::new(5);
        let mut batches = FindInBatches::new(&source, 0);
        for _ in 0..2 {
            let err = batches.next_batch().await.expect_err("must error");
            assert!(err.to_string().contains("batch_size"), "got: {err}");
            assert_eq!(err.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        }
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            0,
            "no query is ever issued for batch_size == 0"
        );
    }
}

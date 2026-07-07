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
//! # Example — `find_in_batches` + `save_many` recompute loop
//!
//! ```rust,ignore
//! let mut batches = repo.find_in_batches(1_000);
//! while let Some(chunk) = batches.next_batch().await? {
//!     let recomputed: Vec<Account> = chunk
//!         .into_iter()
//!         .map(|mut a| { a.balance = recompute(&a); a })
//!         .collect();
//!     repo.save_many(&recomputed).await?; // O(batch_size) memory, not O(table)
//! }
//! ```

use std::future::Future;

use crate::{AutumnError, AutumnResult};

/// A repository that can fetch a keyset batch of models ordered by ascending
/// primary key. Implemented by the generated repository struct; you never
/// implement this by hand.
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
    /// Exclusive lower bound on `id` for the next batch (`None` == from start).
    after: Option<i64>,
    /// Set once the table is exhausted or an error occurred; further calls
    /// return `Ok(None)` rather than silently resuming.
    ended: bool,
}

impl<'a, S: BatchSource> FindInBatches<'a, S> {
    /// Construct a new batched iterator. Prefer the generated
    /// `find_in_batches` method over calling this directly.
    #[must_use]
    pub const fn new(source: &'a S, batch_size: usize) -> Self {
        Self {
            source,
            batch_size,
            after: None,
            ended: false,
        }
    }

    /// Fetch the next chunk of at most `batch_size` models.
    ///
    /// Returns `Ok(Some(chunk))` with `1..=batch_size` models, `Ok(None)` once
    /// the table is exhausted. After an error or exhaustion the iterator stays
    /// ended: subsequent calls return `Ok(None)` and never silently resume.
    ///
    /// # Errors
    ///
    /// Returns an error if `batch_size` is `0`, or if the underlying batch
    /// query fails. The error surfaces on the failing batch and stops
    /// iteration; progress already yielded is not swallowed.
    pub async fn next_batch(&mut self) -> AutumnResult<Option<Vec<S::Model>>> {
        if self.ended {
            return Ok(None);
        }
        if self.batch_size == 0 {
            self.ended = true;
            return Err(AutumnError::bad_request_msg(
                "find_in_batches: batch_size must be greater than zero",
            ));
        }

        // Clamp to `i64::MAX` rather than wrapping negative on a >i64::MAX
        // batch_size (a nonsensical request that would never fit in memory).
        let limit = i64::try_from(self.batch_size).unwrap_or(i64::MAX);
        let batch = match self.source.fetch_batch_after(self.after, limit).await {
            Ok(batch) => batch,
            Err(err) => {
                // Surface the error on the failing batch and stop iterating;
                // do not swallow it or resume on the next call.
                self.ended = true;
                return Err(err);
            }
        };

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
    /// fails (after which the iterator stays ended).
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

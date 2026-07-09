//! Typed grouped aggregate queries (`GROUP BY` roll-ups), issue #1364.
//!
//! `count`, `sum`, `avg`, `min` and `max` over a table, bucketed by a group
//! column, expressed declaratively on a `#[autumn_web::repository]` trait:
//!
//! ```rust,ignore
//! #[autumn_web::repository(Vote, table = "votes")]
//! pub trait VoteRepository {
//!     /// SUM(value) GROUP BY post_id → `Vec<(post_id, Option<sum>)>`.
//!     fn sum_value_grouped_by_post_id() -> Vec<(i64, Option<i64>)>;
//!     /// COUNT(*) GROUP BY variant → `Vec<(variant, count)>`.
//!     fn count_grouped_by_variant() -> Vec<(String, i64)>;
//! }
//! ```
//!
//! Each declared method becomes an **inherent method** on the generated `Pg*`
//! repository struct that returns a lazy [`GroupedAggregate`] builder (mirroring
//! `find_in_batches`). Nothing runs until a terminal [`load`](GroupedAggregate::load):
//!
//! ```rust,ignore
//! // Top-5 posts by score, highest first.
//! let top: Vec<(i64, Option<i64>)> = repo
//!     .sum_value_grouped_by_post_id()
//!     .order_by_aggregate_desc()
//!     .limit(5)
//!     .load()
//!     .await?;
//!
//! // A day-bucketed time series over a bounded window.
//! let per_day: Vec<(chrono::NaiveDateTime, i64)> = repo
//!     .count_grouped_by_created_at()
//!     .bucket(DateBucket::Day)
//!     .filter_range(window_start, window_end)
//!     .load()
//!     .await?;
//! ```
//!
//! ## Value (`V`) and key (`K`) type rules
//!
//! The trait method declares the pair type `Vec<(K, V)>`; the macro reads `K`
//! and `V` from it and bakes the matching Postgres bind/result SQL types.
//!
//! | method              | `V`                            |
//! |---------------------|--------------------------------|
//! | `count_grouped_by_` | `i64`                          |
//! | `sum_*_grouped_by_` | `Option<T>` (`T` = column type)|
//! | `min_*_grouped_by_` | `Option<T>`                    |
//! | `max_*_grouped_by_` | `Option<T>`                    |
//! | `avg_*_grouped_by_` | `Option<f64>`                  |
//!
//! `K` is the group column's Rust type (or, under [`bucket`](GroupedAggregate::bucket),
//! the bucket-start timestamp's type). `sum`/`min`/`max`/`avg` are null-safe:
//! a group whose values are all `NULL` yields `None`, and an empty result set
//! is an empty `Vec`.
//!
//! ## Scoping
//!
//! The generated query composes the repository's soft-delete filter, tenant
//! scoping and read routing exactly like `count`, and acquires its connection
//! through the same read-route helper — so replica routing and multi-tenancy
//! come for free. `sum`/`avg`/`min`/`max` cannot be merged across shards, so a
//! sharded, tenant-scoped repository used via `across_tenants()` rejects
//! grouped aggregates rather than returning a per-shard-partial answer.

use std::future::Future;
use std::pin::Pin;

use crate::AutumnResult;

/// Time-bucket granularity for a `date_trunc`-grouped aggregate (AC5).
///
/// Passing a bucket to [`GroupedAggregate::bucket`] groups by
/// `date_trunc('<unit>', <group_col>)` instead of the raw column, so the key
/// `K` becomes the bucket-start timestamp. The group column must be a
/// timestamp type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateBucket {
    /// `date_trunc('day', …)`.
    Day,
    /// `date_trunc('week', …)` — ISO weeks starting Monday.
    Week,
    /// `date_trunc('month', …)`.
    Month,
}

impl DateBucket {
    /// The `date_trunc` field literal (`"day"`, `"week"`, `"month"`).
    ///
    /// A fixed set of internal constants — never interpolated from user input —
    /// so it is safe to embed directly in the generated SQL.
    #[must_use]
    pub const fn as_trunc_unit(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

/// Ordering applied to the aggregated value for top-N queries (AC3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AggregateOrder {
    /// No explicit ordering (database-defined group order).
    #[default]
    Unordered,
    /// Ascending by the aggregated value.
    Asc,
    /// Descending by the aggregated value.
    Desc,
}

/// The mutable builder state a [`GroupedAggregate`] threads to its executor.
///
/// Public so the macro-generated executor can read it; construct one only via
/// the generated repository methods and the chainable builder setters.
#[derive(Clone, Debug)]
pub struct AggregateOptions<K> {
    /// Ordering on the aggregated value (top-N support).
    pub order: AggregateOrder,
    /// Optional `LIMIT` on the number of groups returned.
    pub limit: Option<i64>,
    /// Optional pre-group equality filter on the group column (bound as a
    /// parameter, never interpolated).
    pub eq: Option<K>,
    /// Optional pre-group inclusive range filter `[low, high]` on the group
    /// column (both bounds bound as parameters).
    pub range: Option<(K, K)>,
    /// Optional time-bucket applied to the group column.
    pub bucket: Option<DateBucket>,
}

impl<K> Default for AggregateOptions<K> {
    fn default() -> Self {
        Self {
            order: AggregateOrder::Unordered,
            limit: None,
            eq: None,
            range: None,
            bucket: None,
        }
    }
}

/// A boxed, owned future produced by a grouped-aggregate executor.
type AggFuture<'a, K, V> = Pin<Box<dyn Future<Output = AutumnResult<Vec<(K, V)>>> + Send + 'a>>;

/// The macro-generated executor: given the finalized options, runs the
/// parameterized `GROUP BY` query against the repository it captured.
type AggExec<'a, K, V> = Box<dyn Fn(AggregateOptions<K>) -> AggFuture<'a, K, V> + Send + 'a>;

/// A lazy, chainable builder for one grouped aggregate query.
///
/// Created by a generated `count_grouped_by_*` / `sum_*_grouped_by_*` /
/// `avg_*` / `min_*` / `max_*` repository method. Chain the setters, then call
/// [`load`](Self::load) to execute. Dropping the builder without loading runs
/// no query.
///
/// `K` is the group-column key type and `V` the aggregated value type (see the
/// [module docs](crate::aggregate) for the `V` rules).
pub struct GroupedAggregate<'a, K, V> {
    opts: AggregateOptions<K>,
    exec: AggExec<'a, K, V>,
}

impl<'a, K, V> GroupedAggregate<'a, K, V> {
    /// Wrap a macro-generated executor. Prefer the generated repository methods
    /// over calling this directly.
    #[must_use]
    pub fn new(exec: AggExec<'a, K, V>) -> Self {
        Self {
            opts: AggregateOptions::default(),
            exec,
        }
    }

    /// Order the results by the aggregated value, largest first (AC3). Combine
    /// with [`limit`](Self::limit) for a top-N roll-up.
    #[must_use]
    pub const fn order_by_aggregate_desc(mut self) -> Self {
        self.opts.order = AggregateOrder::Desc;
        self
    }

    /// Order the results by the aggregated value, smallest first (AC3).
    #[must_use]
    pub const fn order_by_aggregate_asc(mut self) -> Self {
        self.opts.order = AggregateOrder::Asc;
        self
    }

    /// Cap the number of groups returned (AC3, top-N).
    #[must_use]
    pub const fn limit(mut self, n: i64) -> Self {
        self.opts.limit = Some(n);
        self
    }

    /// Keep only rows whose group column equals `value`, applied **before**
    /// grouping (AC4). Bound as a query parameter.
    #[must_use]
    pub fn filter_eq(mut self, value: K) -> Self {
        self.opts.eq = Some(value);
        self
    }

    /// Keep only rows whose group column falls in the inclusive range
    /// `[low, high]`, applied **before** grouping (AC4). Works for date/time
    /// ranges too. Both bounds are bound as query parameters.
    #[must_use]
    pub fn filter_range(mut self, low: K, high: K) -> Self {
        self.opts.range = Some((low, high));
        self
    }

    /// Group by `date_trunc('<unit>', <group_col>)` instead of the raw column,
    /// producing a time series keyed by bucket start (AC5).
    #[must_use]
    pub const fn bucket(mut self, bucket: DateBucket) -> Self {
        self.opts.bucket = Some(bucket);
        self
    }

    /// Execute the aggregate and collect `(key, value)` pairs, consuming the
    /// builder.
    ///
    /// Takes `self` by value (rather than `&self`) so this inherent method wins
    /// method resolution over diesel's by-value `RunQueryDsl::load` when that
    /// trait is in scope.
    ///
    /// An empty result set yields an empty `Vec`; `sum`/`avg`/`min`/`max`
    /// groups with only `NULL` values yield a `None` value (AC7).
    ///
    /// # Errors
    ///
    /// Surfaces any database error, and — on a sharded, tenant-scoped
    /// repository used via `across_tenants()` — a `bad_request` rejecting the
    /// unsupported cross-shard aggregate merge.
    pub async fn load(self) -> AutumnResult<Vec<(K, V)>> {
        (self.exec)(self.opts).await
    }
}

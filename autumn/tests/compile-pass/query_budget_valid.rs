//! `#[query_budget(N)]` accepts every handler shape whose reachable paths stay
//! inside the declared ceiling (#1667).
//!
//! The types here are local stand-ins named exactly like the framework ones —
//! the analysis keys on the query-issuing *surface* (`…Repository` / `Db`
//! handles, `preload`), so the fixture proves the gate without dragging a
//! database feature into a compile-time test.

use autumn_web::query_budget;

struct Post {
    id: i64,
}

struct PreloadSpec;

impl Post {
    fn preload() -> PreloadSpec {
        PreloadSpec
    }
}

impl PreloadSpec {
    fn author(self) -> Self {
        self
    }
    fn tags(self) -> Self {
        self
    }
}

struct PgPostRepository;

impl PgPostRepository {
    async fn find_all(&self) -> Result<Vec<Post>, ()> {
        Ok(Vec::new())
    }
    async fn count(&self) -> Result<i64, ()> {
        Ok(0)
    }
    async fn preload(&self, rows: Vec<Post>, _spec: PreloadSpec) -> Result<Vec<Post>, ()> {
        Ok(rows)
    }
    fn on_primary(&self) -> &Self {
        self
    }
}

async fn opaque_helper(_repo: &PgPostRepository) -> Result<usize, ()> {
    Ok(0)
}

/// Straight-line queries sum.
#[query_budget(2)]
async fn flat(repo: PgPostRepository) -> Result<usize, ()> {
    let posts = repo.find_all().await?;
    let total = repo.count().await?;
    Ok(posts.len() + total as usize)
}

/// A builder prefix refines *how* the query runs; it is not a query.
#[query_budget(1)]
async fn builder_prefix(repo: PgPostRepository) -> Result<usize, ()> {
    let posts = repo.on_primary().find_all().await?;
    Ok(posts.len())
}

/// The AC's green build: the per-row lookup is batched into `preload`, so the
/// loop over rows issues nothing at all.
#[query_budget(3)]
async fn preloaded(repo: PgPostRepository) -> Result<i64, ()> {
    let posts = repo.find_all().await?;
    let posts = repo
        .preload(posts, Post::preload().author().tags())
        .await?;
    let mut sum = 0;
    for post in &posts {
        sum += post.id;
    }
    Ok(sum)
}

/// Only one arm runs, so the budget is the worst arm, not their sum.
#[query_budget(1)]
async fn branches(repo: PgPostRepository, flag: bool) -> Result<usize, ()> {
    if flag {
        Ok(repo.find_all().await?.len())
    } else {
        Ok(repo.count().await? as usize)
    }
}

/// A literal loop bound is a compile-time multiplier, not an unknown.
#[query_budget(3)]
async fn const_bounded(repo: PgPostRepository) -> Result<usize, ()> {
    let mut seen = 0;
    for _ in 0..3 {
        seen += repo.find_all().await?.len();
    }
    Ok(seen)
}

/// Escape hatch 1: the handler opts out of a finite budget, with its reason
/// recorded next to the code it excuses.
#[query_budget(unbounded, reason = "operator backfill, bounded by an explicit page size")]
async fn backfill(repo: PgPostRepository, ids: Vec<i64>) -> Result<usize, ()> {
    let mut seen = 0;
    for _id in ids {
        seen += repo.find_all().await?.len();
    }
    Ok(seen)
}

/// Escape hatch 2: an opaque helper declares its own cost at the call site.
#[query_budget(3)]
async fn declared_helper(repo: PgPostRepository) -> Result<usize, ()> {
    let posts = repo.find_all().await?;
    #[query_cost(2)]
    let extra = opaque_helper(&repo).await?;
    Ok(posts.len() + extra)
}

/// Escape hatch 3: a call site verified query-free is dropped from the ledger.
#[query_budget(1)]
async fn exempt_helper(repo: PgPostRepository) -> Result<usize, ()> {
    let posts = repo.find_all().await?;
    #[query_exempt(reason = "reads the warm cache only; verified query-free")]
    let extra = opaque_helper(&repo).await?;
    Ok(posts.len() + extra)
}

fn main() {
    // The expansion leaves a readable proof behind for tests and tooling.
    assert_eq!(__AUTUMN_QUERY_BUDGET_flat.declared, Some(2));
    assert_eq!(__AUTUMN_QUERY_BUDGET_flat.proven_max, Some(2));
    assert_eq!(__AUTUMN_QUERY_BUDGET_flat.headroom(), Some(0));

    assert_eq!(__AUTUMN_QUERY_BUDGET_builder_prefix.proven_max, Some(1));
    assert_eq!(__AUTUMN_QUERY_BUDGET_preloaded.proven_max, Some(3));
    assert_eq!(__AUTUMN_QUERY_BUDGET_branches.proven_max, Some(1));
    assert_eq!(__AUTUMN_QUERY_BUDGET_const_bounded.proven_max, Some(3));
    assert_eq!(__AUTUMN_QUERY_BUDGET_declared_helper.proven_max, Some(3));
    assert_eq!(__AUTUMN_QUERY_BUDGET_exempt_helper.proven_max, Some(1));

    assert!(__AUTUMN_QUERY_BUDGET_backfill.is_unbounded());
    assert_eq!(__AUTUMN_QUERY_BUDGET_backfill.declared, None);
    assert_eq!(__AUTUMN_QUERY_BUDGET_backfill.proven_max, None);
}

//! Prospect assay follow-up (PR #2546 review, Codex P2, round 4): an
//! *awaited* call whose name collides with a `HANDLE_BUILDERS` entry (here
//! `page`, normally a query-DSL refinement) is correctly counted as the
//! terminal query by the cost counter itself — "a user finder may share a
//! builder's name" — but its *result* must not also be promoted to a
//! handle by `awaited_expr_is_fresh_handle`, or a harmless `rows.len()`
//! gets miscounted as a second query. Only `HANDLE_ACCESSORS` names survive
//! an await/`?` peel into a fresh handle; `HANDLE_BUILDERS` names do not.

use autumn_web::query_budget;

struct Post;

struct PgPostRepository;

impl PgPostRepository {
    // Named like a builder refinement, but here it is the actual terminal
    // async call — the framework's documented "a user finder may share a
    // builder's name" case.
    async fn page(&self, _n: i64) -> Result<Vec<Post>, ()> {
        Ok(Vec::new())
    }
}

#[query_budget(1)]
async fn index(repo: PgPostRepository) -> Result<usize, ()> {
    let rows = repo.page(1).await?;
    Ok(rows.len())
}

fn main() {}

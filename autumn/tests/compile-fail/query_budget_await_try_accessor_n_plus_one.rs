//! Prospect assay follow-up (PR #2546 review, Codex P2, second round): the
//! accessor-tracking fixtures added earlier all obtain their handle through
//! a synchronous, infallible accessor call (`state.db()`). The review
//! correctly pointed out a real, named production shape this misses:
//! `autumn-search/src/postgres.rs`'s `PostgresSearchStore::write_documents`
//! binds its handle as `let mut conn = self.conn().await?;` — an async,
//! fallible accessor — and neither `Analyzer::expr_is_handle` nor
//! `chain_root_is_handle` peeled `Expr::Await`/`Expr::Try` before this fix,
//! so `conn` fell through to "not a handle" with **no diagnostic at all**:
//! every later query issued through it was silently uncounted. This is a
//! real soundness gap the accessor-tracking fixtures in this PR could not
//! have caught (they never exercised an `.await?`-wrapped accessor).
//!
//! Fixed by peeling `Expr::Await`/`Expr::Try` in `expr_is_handle`,
//! `chain_root_is_handle`, and `expr_carries_handle`. This fixture pins the
//! fix: before it, this file **compiled clean** (a false negative); after
//! it, the N+1 through `conn` is caught with the standard diagnostic.

use autumn_web::query_budget;

struct Conn;

struct Query;

impl Query {
    // Diesel-async style: the connection is an *argument* to `execute`, not
    // the receiver — the real shape at `autumn-search/src/postgres.rs:575`
    // (`bind_all(...).execute(&mut conn)`).
    async fn execute(&self, _conn: &mut Conn) -> Result<(), ()> {
        Ok(())
    }
}

struct Pool;

impl Pool {
    async fn conn(&self) -> Result<Conn, ()> {
        Ok(Conn)
    }
}

struct Store {
    pool: Pool,
}

impl Store {
    async fn conn(&self) -> Result<Conn, ()> {
        self.pool.conn().await
    }

    #[query_budget(1)]
    async fn write_documents(&self, ids: Vec<i64>) -> Result<(), ()> {
        let mut conn = self.conn().await?;
        for _id in ids {
            Query.execute(&mut conn).await?;
        }
        Ok(())
    }
}

fn main() {}

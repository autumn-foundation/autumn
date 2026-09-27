//! Codex review, PR #2762 (making `LazyDb` public, #2264): promoting the
//! framework's internal `DeferredDb` to a public `LazyDb` gave any handler
//! author the documented idiom `let mut db = lazy_db.checkout().await?;` —
//! but neither `expr_is_handle` nor `awaited_expr_is_fresh_handle` recognized
//! `checkout` as handle-producing, so `db` fell through to "not a handle"
//! with no diagnostic at all: every query later issued through it (including
//! an N+1 loop) went uncounted.
//!
//! Fixed by `HANDLE_TRANSITIONS` (`checkout`), gated on the receiver already
//! being a tracked handle — `lazy_db`'s type ends in `Db`, so it is seeded
//! from the signature exactly like a plain `Db` parameter. This fixture pins
//! the fix: before it, this file **compiled clean** (a false negative); after
//! it, the N+1 through `db` is caught with the standard diagnostic.

use autumn_web::query_budget;

struct Conn;

struct Query;

impl Query {
    // Diesel-async style: the connection is an *argument* to `execute`, not
    // the receiver.
    async fn execute(&self, _conn: &mut Conn) -> Result<(), ()> {
        Ok(())
    }
}

struct LazyDb;

impl LazyDb {
    async fn checkout(self) -> Result<Conn, ()> {
        Ok(Conn)
    }
}

#[query_budget(1)]
async fn post_comment(lazy_db: LazyDb, ids: Vec<i64>) -> Result<(), ()> {
    let mut db = lazy_db.checkout().await?;
    for _id in ids {
        Query.execute(&mut db).await?;
    }
    Ok(())
}

fn main() {}

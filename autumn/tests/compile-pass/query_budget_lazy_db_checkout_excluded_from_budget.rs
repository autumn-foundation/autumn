//! Codex review, PR #2762 (#2264), second round: fixing `checkout`'s handle
//! provenance (`HANDLE_TRANSITIONS` in `awaited_expr_is_fresh_handle`) left
//! `method_chain`'s cost side untouched, so the checkout call itself was
//! still counted as a query — the documented `LazyDb` idiom followed by
//! exactly one real query compiled as *two* queries and was rejected under
//! `#[query_budget(1)]`. Must compile clean — proves `checkout` costs
//! nothing on its own, the same way a plain `HANDLE_ACCESSORS` call never
//! does (`query_budget_job_shaped_accessor_batched.rs`).

use autumn_web::query_budget;

struct Conn;

struct Query;

impl Query {
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
async fn post_comment(lazy_db: LazyDb) -> Result<(), ()> {
    let mut db = lazy_db.checkout().await?;
    Query.execute(&mut db).await
}

fn main() {}

//! Codex review, PR #2762 (making `LazyDb` public, #2264): before
//! `TRANSPARENT_BUILDERS` recognized `checkout`, `let mut db =
//! lazy_db.checkout().await?;` left `db` untracked, so handing `&mut db` to
//! an opaque helper compiled clean — the helper could write, call out, or
//! enqueue outside the declared grant with nothing to catch it. This fixture
//! pins the fix: before it, this file compiled clean; after it, the opaque
//! helper is reported exactly like a plain `Db` parameter would be (see
//! `agent_authority_opaque_helper.rs`).

use autumn_web::agent_operable;

struct Refund;

struct Db;

struct LazyDb;

impl LazyDb {
    async fn checkout(self) -> Result<Db, ()> {
        Ok(Db)
    }
}

async fn finalize(_db: &mut Db, _id: i64) -> Result<Refund, ()> {
    Ok(Refund)
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn draft(lazy_db: LazyDb) -> Result<Refund, ()> {
    let mut db = lazy_db.checkout().await?;
    finalize(&mut db, 1).await
}

fn main() {}

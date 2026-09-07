//! Prospect assay follow-up (PR #2546 review, Codex P2, round 5): the same
//! accessor-tracking gap as the `.await?` fixtures, but for the
//! `.expect(...)`/`.unwrap()` idiom `autumn/src/seed.rs` documents as its
//! own canonical usage: `let mut db = ctx.conn().await.expect("db
//! connection");`. Before this fix, `expr_is_handle` recognized `Expr::Try`
//! (the `?` operator) but not a plain `.expect(...)`/`.unwrap()` call on an
//! awaited accessor, so `db` fell through to "not a handle" here just as
//! `conn` did in the `?`-shaped fixtures, with the same silent-uncounted
//! result.

use autumn_web::query_budget;

struct Db;

impl Db {
    async fn execute(&mut self, _sql: &str) -> Result<(), ()> {
        Ok(())
    }
}

struct Ctx;

impl Ctx {
    async fn conn(&self) -> Result<Db, ()> {
        Ok(Db)
    }
}

#[query_budget(1)]
async fn seed_rows(ctx: Ctx, ids: Vec<i64>) -> Result<(), ()> {
    let mut db = ctx.conn().await.expect("db connection");
    for id in ids {
        db.execute(&format!("insert {id}")).await?;
    }
    Ok(())
}

fn main() {}

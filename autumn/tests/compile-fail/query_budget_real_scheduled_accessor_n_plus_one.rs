//! Prospect assay follow-up (PR #2546 review, Codex P2): the one-argument
//! counterpart to `query_budget_real_job_accessor_n_plus_one.rs`, stacking
//! the real `autumn_web::scheduled` attribute (single `AppState` argument,
//! per `docs/guide/jobs.md`'s scheduled examples) with `#[query_budget]`.

use autumn_web::{AppState, AutumnResult, query_budget, scheduled};

struct Db;

impl Db {
    async fn find_recipient(&self, _id: i64) -> AutumnResult<()> {
        Ok(())
    }
}

trait StateDbExt {
    fn db(&self) -> Db;
}

impl StateDbExt for AppState {
    fn db(&self) -> Db {
        Db
    }
}

#[scheduled(every = "1h", name = "digest-sweep")]
#[query_budget(1)]
async fn digest_sweep(state: AppState) -> AutumnResult<()> {
    let ids: Vec<i64> = vec![1, 2, 3];
    for id in ids {
        state.db().find_recipient(id).await?;
    }
    Ok(())
}

fn main() {}

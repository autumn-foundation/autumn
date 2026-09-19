//! Prospect assay control (ledger: query_budget job/scheduled
//! generalization, 2026-09-06): the job-shaped counterpart to
//! `query_budget_job_shaped_accessor_n_plus_one.rs` with the per-row lookup
//! batched ahead of the loop, same accessor (`state.db()`). Must compile
//! clean — proves the accessor path isn't just always rejecting job-shaped
//! functions, it is actually counting.

use autumn_web::query_budget;

struct AppState;

struct Db;

impl Db {
    async fn find_recipients(&self, _ids: &[i64]) -> Result<Vec<()>, ()> {
        Ok(Vec::new())
    }
}

impl AppState {
    fn db(&self) -> Db {
        Db
    }
}

struct SendDigestArgs {
    recipient_ids: Vec<i64>,
}

#[query_budget(1)]
async fn send_digest_batched(state: AppState, args: SendDigestArgs) -> Result<usize, ()> {
    let recipients = state.db().find_recipients(&args.recipient_ids).await?;
    Ok(recipients.len())
}

fn main() {}

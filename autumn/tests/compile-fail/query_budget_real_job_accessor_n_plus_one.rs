//! Prospect assay follow-up (PR #2546 review, Codex P2): the accessor-shaped
//! N+1 fixtures added alongside this one used a local stand-in function
//! matching `#[job]`'s enforced signature shape, not the real `#[job]`
//! attribute — so they could not prove the two macros actually compose.
//! This fixture stacks the real `autumn_web::job` attribute with
//! `#[query_budget]`, using the real `AppState` type and a serializable args
//! struct, exactly as `docs/guide/jobs.md`'s own examples are shaped.
//!
//! `db()` is added to `AppState` via a local extension trait — the walker's
//! `HANDLE_ACCESSORS` check is a syntactic match on the method name
//! (`autumn-macros/src/query_budget.rs`'s `expr_is_handle`), not a type
//! resolution, so this is a faithful stand-in for an app defining its own
//! `AppState::db()` convenience method without dragging the `db` feature's
//! real connection pool into a compile-time-only fixture.

use autumn_web::{AppState, AutumnResult, job, query_budget};
use serde::{Deserialize, Serialize};

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

#[derive(Serialize, Deserialize)]
struct SendDigestArgs {
    recipient_ids: Vec<i64>,
}

#[job(name = "send_digest")]
#[query_budget(1)]
async fn send_digest(state: AppState, args: SendDigestArgs) -> AutumnResult<()> {
    for id in args.recipient_ids {
        state.db().find_recipient(id).await?;
    }
    Ok(())
}

fn main() {}

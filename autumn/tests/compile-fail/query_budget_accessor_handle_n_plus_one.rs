//! Prospect assay (ledger: query_budget job/scheduled generalization, 2026-09-06):
//! does the handle-tracking analysis catch an N+1 reached through a
//! *conventionally-named accessor call* (`state.db()`) on a parameter whose
//! type is NOT itself recognized as a handle — the shape a `#[job]`/
//! `#[scheduled]` function is structurally limited to, since both macros
//! constrain their handler to `(AppState, Args[, JobContext])` /
//! `(AppState)` and never a typed `Db`/`…Repository` parameter?
//!
//! `AppState` here is a local stand-in (does not end in `Db`/`Repository`,
//! is not in `HANDLE_TYPES`), so `state` is NOT seeded into the tracked
//! handle set from the signature. If this still fails to build, the walker's
//! accessor-call check (`HANDLE_ACCESSORS` — `db`, `repo`, `repository`,
//! `pool`, `conn`, `connection`) is receiver-type-agnostic, exactly as
//! `autumn-macros/src/query_budget.rs`'s `expr_is_handle` reads.

use autumn_web::query_budget;

struct AppState;

struct Db;

impl Db {
    async fn find_author(&self, _id: i64) -> Result<(), ()> {
        Ok(())
    }
}

impl AppState {
    fn db(&self) -> Db {
        Db
    }
}

#[query_budget(1)]
async fn accessor_n_plus_one(state: AppState, ids: Vec<i64>) -> Result<(), ()> {
    for id in ids {
        state.db().find_author(id).await?;
    }
    Ok(())
}

fn main() {}

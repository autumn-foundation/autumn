//! Prospect assay (ledger: query_budget job/scheduled generalization,
//! 2026-09-06): the same accessor-reached N+1 as
//! `query_budget_accessor_handle_n_plus_one.rs`, but on a function matching
//! `#[job]`'s own enforced signature shape — `async fn(AppState, Args)`
//! (`autumn-macros/src/job.rs` rejects anything else) — to confirm the
//! result holds for the exact arity/parameter-naming a real job handler is
//! constrained to, not just an arbitrary two-argument function.
//!
//! Deliberately does **not** apply the real `#[job]` attribute: `job.rs` and
//! `scheduled.rs` both re-emit the input `ItemFn` completely unchanged
//! (`quote! { #input_fn ... }`, no wrapping async block/closure, unlike
//! `#[secured]`/`#[cached]`), so there is no macro-expansion shape for
//! `#[query_budget]` to compose with — confirmed by reading both macros
//! rather than re-proving it here. This fixture isolates the one part that
//! *is* a hypothesis: whether the accessor-based tracking reaches through
//! the job-shaped signature's parameter naming and body shape.

use autumn_web::query_budget;

struct AppState;

struct Db;

impl Db {
    async fn find_recipient(&self, _id: i64) -> Result<(), ()> {
        Ok(())
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
async fn send_digest(state: AppState, args: SendDigestArgs) -> Result<(), ()> {
    for id in args.recipient_ids {
        state.db().find_recipient(id).await?;
    }
    Ok(())
}

fn main() {}

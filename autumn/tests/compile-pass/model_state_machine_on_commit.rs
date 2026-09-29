//! Issue #1973: a `#[state_machine(transitions(...))]` edge can name an
//! `on_commit = <Job>` effect. When such an edge is declared the model gains a
//! connection-taking `transition_{field}_to_on_conn` method that validates the
//! transition and enqueues the named job transactionally on the caller's
//! connection with a derived idempotency key. Guards and effects compose on one
//! edge. This is the compile-pass companion to the pure `model_state_machine.rs`
//! case.

use autumn_web::model;
use autumn_web::prelude::*;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use serde::{Deserialize, Serialize};

// A `#[job]` whose args are the framework-provided transition context. The
// derived idempotency key rides on the payload, so declaring the job
// `unique, by = ["idempotency_key"]` coalesces a retried transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EffectArgs {
    idempotency_key: String,
}

#[job(name = "send_shipped_email")]
async fn send_shipped_email(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

#[job(name = "announce_archive")]
async fn announce_archive(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

diesel::table! {
    orders (id) {
        id -> BigInt,
        status -> Text,
    }
}

#[model(table = "orders")]
pub struct Order {
    #[id]
    pub id: i64,
    #[state_machine(transitions(
        pending -> processing,
        processing -> shipped: on_commit = SendShippedEmailJob,
        shipped -> archived: guard = "can_archive", on_commit = AnnounceArchiveJob,
    ))]
    pub status: String,
}

impl Order {
    fn can_archive(&self) -> bool {
        true
    }
}

// The pure validator is unchanged and still generated.
fn _assert_pure_validator_compiles(order: &Order) {
    let _ = order.can_transition_status_to("processing");
    let _ = order.transition_status_to("processing");
}

// The additive connection-taking method is generated because at least one edge
// declares `on_commit`. It takes an explicit connection so the effect enqueues
// inside the caller's transaction.
async fn _assert_on_conn_compiles(
    order: &Order,
    conn: &mut AsyncPgConnection,
) -> AutumnResult<String> {
    order.transition_status_to_on_conn(conn, "shipped").await
}

fn main() {
    let _ = _assert_pure_validator_compiles;
    let _ = _assert_on_conn_compiles;
    let _ = send_shipped_email;
    let _ = announce_archive;
}

//! Issue #1973: a `#[state_machine(transitions(...))]` edge can declare a sync
//! `on = "handler"` in-transaction effect — an inherent
//! `async fn(&self, conn) -> AutumnResult<()>` method run inside the
//! transition's transaction when that edge fires (`Err` rolls it back).
//! Declaring `on` (like `on_commit`) makes the model gain the connection-taking
//! `transition_{field}_to_on_conn` method. `on` composes with `guard` and
//! `on_commit` on one edge. This is the compile-pass companion to the pure
//! `model_state_machine.rs` case.

use autumn_web::model;
use autumn_web::prelude::*;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use serde::{Deserialize, Serialize};

// A `#[job]` whose args are the framework-provided transition context.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EffectArgs {
    idempotency_key: String,
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
        // A pure sync in-transaction effect.
        processing -> shipped: on = "record_audit",
        // Guard + sync `on` + after-commit `on_commit` compose on one edge.
        shipped -> archived: guard = "can_archive", on = "record_audit", on_commit = AnnounceArchiveJob,
    ))]
    pub status: String,
}

impl Order {
    fn can_archive(&self) -> bool {
        true
    }

    // The sync in-transaction effect runs inside the transition's transaction;
    // a returned `Err` rolls the transition back.
    async fn record_audit(&self, _conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        Ok(())
    }
}

// The pure validator is unchanged and still generated.
fn _assert_pure_validator_compiles(order: &Order) {
    let _ = order.can_transition_status_to("processing");
    let _ = order.transition_status_to("processing");
}

// The additive connection-taking method is generated because at least one edge
// declares an effect (`on` and/or `on_commit`). It takes an explicit connection
// so the sync `on` handler and the `on_commit` enqueue both run inside the
// caller's transaction.
async fn _assert_on_conn_compiles(
    order: &Order,
    conn: &mut AsyncPgConnection,
) -> AutumnResult<String> {
    order.transition_status_to_on_conn(conn, "shipped").await
}

fn main() {
    let _ = _assert_pure_validator_compiles;
    let _ = _assert_on_conn_compiles;
    let _ = announce_archive;
}

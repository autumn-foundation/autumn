//! Issue #1973: a `#[state_machine(transitions(...))]` edge can declare a sync
//! `on = "handler"` in-transaction effect. `handler` is an inherent
//! `async fn(&self, conn) -> AutumnResult<()>` method run inside the
//! transition's transaction when that edge fires; a returned `Err` rolls the
//! transition back. Declaring `on` (like `on_commit`) makes the model generate
//! the connection-taking `transition_{field}_to_on_conn` method. `on` composes
//! with `guard` and `on_commit` on a single edge.
//!
//! This is a lightweight compilation + pure-validation contract test that runs
//! WITHOUT Docker (it never opens a connection): it constructs models by hand
//! and calls the generated pure validators, and references the connection-taking
//! method behind a never-called async fn so the generated effect codegen is
//! type-checked. Run it standalone with:
//!
//! ```text
//! cargo test -p autumn-web --test state_machine_on
//! ```

#![cfg(feature = "db")]

use autumn_web::model;
use autumn_web::prelude::*;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use serde::{Deserialize, Serialize};

// The framework-provided transition context is this job's args. The derived
// idempotency key rides on the payload, so declaring the job
// `unique, by = ["idempotency_key"]` coalesces a retried transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EffectArgs {
    idempotency_key: String,
}

#[job(name = "on_sync_announce_archive")]
#[allow(clippy::unused_async)] // stub handler: no real work in this codegen test
async fn announce_archive(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

diesel::table! {
    on_sync_orders (id) {
        id -> BigInt,
        status -> Text,
    }
}

#[model(table = "on_sync_orders")]
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
    // A stub guard for the codegen test; real guards do non-trivial work.
    #[allow(clippy::missing_const_for_fn)]
    fn can_archive(&self) -> bool {
        !self.status.is_empty()
    }

    // The sync in-transaction effect: runs inside the transition's transaction,
    // and `Err` rolls the transition back. A no-op stub here.
    #[allow(clippy::unused_async)] // stub handler: no real work in this codegen test
    async fn record_audit(&self, _conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        Ok(())
    }
}

fn order(status: &str) -> Order {
    Order {
        id: 7,
        status: status.to_string(),
    }
}

// The pure validator is byte-for-byte unchanged by adding `on`.
#[test]
fn pure_validator_still_governs_allowed_edges() {
    assert!(order("pending").can_transition_status_to("processing"));
    assert!(order("processing").can_transition_status_to("shipped"));
    // The guard on the composed edge is still honoured.
    assert!(order("shipped").can_transition_status_to("archived"));
    // Undeclared edges remain denied.
    assert!(!order("pending").can_transition_status_to("shipped"));
    assert!(order("pending").transition_status_to("shipped").is_err());
    assert_eq!(
        order("processing")
            .transition_status_to("shipped")
            .expect("declared edge"),
        "shipped"
    );
}

// Compilation contract: the additive connection-taking method exists, takes a
// connection, and returns the new state. Never called (no runtime/DB here) — its
// mere existence type-checks the composed sync `on` + `on_commit` effect codegen
// end to end.
#[allow(dead_code)]
async fn _assert_on_conn_signature(
    order: &Order,
    conn: &mut AsyncPgConnection,
) -> AutumnResult<String> {
    order.transition_status_to_on_conn(conn, "shipped").await
}

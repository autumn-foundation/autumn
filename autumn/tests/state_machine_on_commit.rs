//! Issue #1973: a `#[state_machine(transitions(...))]` edge can declare an
//! `on_commit = <Job>` after-commit effect. Declaring one makes the model
//! generate a connection-taking `transition_{field}_to_on_conn` method that
//! validates the transition and enqueues the named job **transactionally** on
//! the caller's connection with a derived idempotency key. Guards and effects
//! compose on a single edge.
//!
//! This is a lightweight compilation + pure-validation contract test that runs
//! WITHOUT Docker (it never opens a connection): it constructs models by hand
//! and calls the generated pure validators, and references the connection-taking
//! method behind a never-called async fn so the generated effect codegen is
//! type-checked. Run it standalone with:
//!
//! ```text
//! cargo test -p autumn-web --test state_machine_on_commit
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

#[job(name = "on_commit_send_shipped_email")]
#[allow(clippy::unused_async)] // stub handler: no real work in this codegen test
async fn send_shipped_email(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

#[job(name = "on_commit_announce_archive")]
#[allow(clippy::unused_async)] // stub handler: no real work in this codegen test
async fn announce_archive(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

diesel::table! {
    on_commit_orders (id) {
        id -> BigInt,
        status -> Text,
    }
}

#[model(table = "on_commit_orders")]
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
    // A stub guard for the codegen test; real guards do non-trivial work.
    #[allow(clippy::missing_const_for_fn)]
    fn can_archive(&self) -> bool {
        !self.status.is_empty()
    }
}

fn order(status: &str) -> Order {
    Order {
        id: 7,
        status: status.to_string(),
    }
}

// The pure validator is byte-for-byte unchanged by adding `on_commit`.
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
// mere existence type-checks the generated effect codegen end to end.
#[allow(dead_code)]
async fn _assert_on_conn_signature(
    order: &Order,
    conn: &mut AsyncPgConnection,
) -> AutumnResult<String> {
    order.transition_status_to_on_conn(conn, "shipped").await
}

// The `TransitionEffect` payload the generated effect enqueues is public and
// serializable, and its idempotency key composes the transition context.
#[test]
fn transition_effect_payload_is_public_and_serializable() {
    let effect = autumn_web::TransitionEffect {
        model: "Order".to_string(),
        field: "status".to_string(),
        record_id: "7".to_string(),
        from_state: "processing".to_string(),
        to_state: "shipped".to_string(),
        idempotency_key: "Order:status:7:processing:shipped".to_string(),
    };
    let json = serde_json::to_value(&effect).expect("serializes");
    assert_eq!(json["idempotency_key"], "Order:status:7:processing:shipped");
    let round: autumn_web::TransitionEffect = serde_json::from_value(json).expect("round-trips");
    assert_eq!(round, effect);
}

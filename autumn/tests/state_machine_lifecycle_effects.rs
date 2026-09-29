//! Issue #1973: a `#[state_machine(lifecycle = <Enum>)]` binding can declare
//! per-edge effects at the binding site via an `effects(...)` clause that reuses
//! the EXACT per-edge grammar of the inline `transitions(...)` path (`on = "..."`
//! sync in-transaction effect and/or `on_commit = <Job>` after-commit enqueue),
//! EXCEPT guards are rejected (lifecycle transitions are unguarded). The
//! transition TABLE still comes from the `#[lifecycle]` enum; only the effects
//! are declared at the binding site. Both the inline and lifecycle paths route
//! their effects through the SAME `transition_{field}_to_on_conn` codegen, so a
//! lifecycle binding with `effects(...)` gains the identical connection-taking
//! method.
//!
//! This is a lightweight compilation + pure-validation contract test that runs
//! WITHOUT Docker (it never opens a connection): it constructs models by hand
//! and calls the generated pure validators, and references the connection-taking
//! method behind a never-called async fn so the generated effect codegen is
//! type-checked. Run it standalone with:
//!
//! ```text
//! cargo test -p autumn-web --test state_machine_lifecycle_effects
//! ```

#![cfg(feature = "db")]

use autumn_web::model;
use autumn_web::prelude::*;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::{Lifecycle, lifecycle};
use serde::{Deserialize, Serialize};

// ── The typed source of truth for the transition table ────────────────────────

#[lifecycle(
    initial = Draft,
    terminal(Archived),
    transitions(
        Draft -> Published,
        Published -> Archived,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Draft,
    Published,
    Archived,
}

// The after-commit effect job. The derived idempotency key rides on the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EffectArgs {
    idempotency_key: String,
}

#[job(name = "lifecycle_effects_announce_publish")]
#[allow(clippy::unused_async)] // stub handler: no real work in this codegen test
async fn announce_publish(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

diesel::table! {
    lifecycle_effects_articles (id) {
        id -> BigInt,
        status -> Text,
    }
}

/// The transition table comes from `OrderState`; the per-edge effects are
/// declared at the binding site via `effects(...)`. The state strings in the
/// effect edges match the `PascalCase` the enum's variant names render to.
#[model(table = "lifecycle_effects_articles")]
pub struct Article {
    #[id]
    pub id: i64,
    #[state_machine(lifecycle = OrderState, effects(
        // After-commit enqueue on this edge.
        Draft -> Published: on_commit = AnnouncePublishJob,
        // Sync in-transaction handler on this edge.
        Published -> Archived: on = "record_archive",
    ))]
    pub status: String,
}

impl Article {
    // The sync in-transaction effect: runs inside the transition's transaction,
    // and `Err` rolls the transition back. A no-op stub here.
    #[allow(clippy::unused_async)] // stub handler: no real work in this codegen test
    async fn record_archive(&self, _conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        Ok(())
    }
}

fn article(status: &str) -> Article {
    Article {
        id: 7,
        status: status.to_string(),
    }
}

// The pure validators are byte-for-byte unchanged by the `effects(...)` clause —
// the allowed/denied set still comes from the enum's transition table.
#[test]
fn pure_validator_still_governs_lifecycle_edges() {
    // Declared edges are allowed.
    assert!(article("Draft").can_transition_status_to("Published"));
    assert!(article("Published").can_transition_status_to("Archived"));
    // Undeclared edges remain denied.
    assert!(!article("Draft").can_transition_status_to("Archived"));
    assert!(!article("Archived").can_transition_status_to("Draft"));

    // `transition_*_to` mirrors the predicate.
    assert_eq!(
        article("Draft")
            .transition_status_to("Published")
            .expect("declared edge"),
        "Published"
    );
    assert!(article("Draft").transition_status_to("Archived").is_err());

    // The table is a direct alias of the enum's typed table.
    assert_eq!(
        Article::__AUTUMN_SM_STATUS_TRANSITIONS,
        <OrderState as Lifecycle>::STATE_MACHINE_TRANSITIONS
    );
}

// Compilation contract: the additive connection-taking method exists, takes a
// connection, and returns the new state. Never called (no runtime/DB here) — its
// mere existence type-checks the lifecycle-path `on_commit` + `on` effect codegen
// end to end, proving the shared emitter converged both paths.
#[allow(dead_code)]
async fn _assert_on_conn_signature(
    article: &Article,
    conn: &mut AsyncPgConnection,
) -> AutumnResult<String> {
    article
        .transition_status_to_on_conn(conn, "Published")
        .await
}

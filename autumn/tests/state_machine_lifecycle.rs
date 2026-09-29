//! Issue #1911: a field-level `#[state_machine(lifecycle = Enum)]` derives its
//! runtime transitions table from a `#[lifecycle]` enum, and must behave
//! IDENTICALLY to the equivalent inline `#[state_machine(transitions(...))]`.
//!
//! These are pure in-memory unit tests over the generated validation logic —
//! they construct model structs by hand and call the generated
//! `can_transition_*_to` / `transition_*_to` methods. No database connection is
//! opened, so the suite runs WITHOUT Docker:
//!
//! ```text
//! cargo test -p autumn-web --test state_machine_lifecycle
//! ```

#![cfg(feature = "db")]

use autumn_web::{Lifecycle, lifecycle, model};

// ── The single, typed source of truth ────────────────────────────────────────

#[lifecycle(
    initial = Pending,
    terminal(Delivered, Cancelled),
    transitions(
        Pending -> Paid,
        Pending -> Cancelled,
        Paid -> Shipped,
        Paid -> Cancelled,
        Shipped -> Delivered,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Pending,
    Paid,
    Shipped,
    Delivered,
    Cancelled,
}

// ── Two models with the SAME edges, declared two different ways ───────────────

diesel::table! {
    orders_inline (id) {
        id -> BigInt,
        status -> Text,
    }
}

/// Inline shorthand: edges written out with the same `PascalCase` state strings
/// the lifecycle enum's variant names render to.
#[model(table = "orders_inline")]
pub struct OrderInline {
    #[id]
    pub id: i64,
    #[state_machine(transitions(
        Pending -> Paid,
        Pending -> Cancelled,
        Paid -> Shipped,
        Paid -> Cancelled,
        Shipped -> Delivered,
    ))]
    pub status: String,
}

diesel::table! {
    orders_lifecycle (id) {
        id -> BigInt,
        status -> Text,
    }
}

/// Lifecycle-derived: transitions come from `OrderState` — defined once, typed.
#[model(table = "orders_lifecycle")]
pub struct OrderLifecycle {
    #[id]
    pub id: i64,
    #[state_machine(lifecycle = OrderState)]
    pub status: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Every state string plus a bogus target that is in neither table.
const STATES: &[&str] = &[
    "Pending",
    "Paid",
    "Shipped",
    "Delivered",
    "Cancelled",
    "Nowhere", // not a declared state — must always be denied
];

fn inline(status: &str) -> OrderInline {
    OrderInline {
        id: 1,
        status: status.to_string(),
    }
}

fn lifecycle_model(status: &str) -> OrderLifecycle {
    OrderLifecycle {
        id: 1,
        status: status.to_string(),
    }
}

// ── AC3: identical allowed/denied set ─────────────────────────────────────────

#[test]
fn lifecycle_and_inline_agree_on_every_transition() {
    for &from in STATES {
        for &to in STATES {
            let inline_can = inline(from).can_transition_status_to(to);
            let lifecycle_can = lifecycle_model(from).can_transition_status_to(to);
            assert_eq!(
                inline_can, lifecycle_can,
                "disagreement on `{from}` -> `{to}`: inline={inline_can}, lifecycle={lifecycle_can}"
            );
        }
    }
}

#[test]
fn lifecycle_and_inline_agree_on_transition_result() {
    for &from in STATES {
        for &to in STATES {
            let inline_res = inline(from).transition_status_to(to);
            let lifecycle_res = lifecycle_model(from).transition_status_to(to);
            match (inline_res, lifecycle_res) {
                (Ok(a), Ok(b)) => assert_eq!(
                    a, b,
                    "both accept `{from}` -> `{to}` but yield different values"
                ),
                (Err(_), Err(_)) => {}
                (a, b) => panic!(
                    "Ok/Err mismatch on `{from}` -> `{to}`: inline={:?}, lifecycle={:?}",
                    a.is_ok(),
                    b.is_ok()
                ),
            }
        }
    }
}

#[test]
fn lifecycle_derived_allows_exactly_the_declared_edges() {
    // Spot-check the concrete edge set so a silent all-true / all-false
    // regression can't hide behind the cross-check above.
    let m = lifecycle_model("Pending");
    assert!(m.can_transition_status_to("Paid"));
    assert!(m.can_transition_status_to("Cancelled"));
    assert!(!m.can_transition_status_to("Shipped"));
    assert!(!m.can_transition_status_to("Delivered"));

    assert!(lifecycle_model("Paid").can_transition_status_to("Shipped"));
    assert!(lifecycle_model("Shipped").can_transition_status_to("Delivered"));

    // Terminal states have no outgoing edges.
    assert!(!lifecycle_model("Delivered").can_transition_status_to("Pending"));
    assert!(!lifecycle_model("Cancelled").can_transition_status_to("Pending"));
}

#[test]
fn lifecycle_derived_table_equals_the_enum_trait_table() {
    // The model's transitions const is a direct alias of the enum's typed table,
    // so the "defined once" guarantee is structural, not a copy.
    assert_eq!(
        OrderLifecycle::__AUTUMN_SM_STATUS_TRANSITIONS,
        <OrderState as Lifecycle>::STATE_MACHINE_TRANSITIONS
    );
    // And it equals the inline model's table entry-for-entry.
    assert_eq!(
        OrderLifecycle::__AUTUMN_SM_STATUS_TRANSITIONS,
        OrderInline::__AUTUMN_SM_STATUS_TRANSITIONS
    );
}

#[test]
fn transition_error_names_field_and_states() {
    let err = lifecycle_model("Pending")
        .transition_status_to("Delivered")
        .expect_err("Pending -> Delivered is not a declared edge");
    let msg = err.to_string();
    assert!(msg.contains("status"), "error should name the field: {msg}");
    assert!(
        msg.contains("Pending"),
        "error should name the from-state: {msg}"
    );
    assert!(
        msg.contains("Delivered"),
        "error should name the to-state: {msg}"
    );
}

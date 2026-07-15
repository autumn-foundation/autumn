//! Issue #1911: `#[state_machine(lifecycle = T)]` where `T` is NOT a
//! `#[lifecycle]` enum must be a compile error. The derived transitions const
//! reads `<T as ::autumn_web::Lifecycle>::STATE_MACHINE_TRANSITIONS`, so a type
//! that does not implement `Lifecycle` fails with an unsatisfied trait bound —
//! a comprehensible error, not a cryptic "no associated const".

use autumn_web::model;

// A plain enum WITHOUT `#[lifecycle]` — it does not implement `Lifecycle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotALifecycle {
    A,
    B,
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
    #[state_machine(lifecycle = NotALifecycle)]
    pub status: String,
}

fn main() {}

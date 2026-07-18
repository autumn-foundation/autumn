//! Issue #1911: a field-level `#[state_machine(lifecycle = Enum)]` derives its
//! transitions table from a `#[lifecycle]` enum. This is the "happy path"
//! compile-pass companion to the inline `model_state_machine.rs` case.

use autumn_web::{lifecycle, model};

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

diesel::table! {
    orders (id) {
        id -> BigInt,
        status -> Text,
    }
}

// The lifecycle enum is the single source of truth for the column's transitions.
#[model(table = "orders")]
pub struct Order {
    #[id]
    pub id: i64,
    #[state_machine(lifecycle = OrderState)]
    pub status: String,
}

fn _assert_methods_compile(order: &Order) {
    let _ = order.can_transition_status_to("Paid");
    let _ = order.transition_status_to("Paid");
}

fn _assert_const_is_the_enum_table() {
    // The model's transitions const is exactly the enum's typed table.
    let _: &[(&str, &str, Option<&str>)] = Order::__AUTUMN_SM_STATUS_TRANSITIONS;
    let _: &[(&str, &str, Option<&str>)] =
        <OrderState as autumn_web::Lifecycle>::STATE_MACHINE_TRANSITIONS;
}

fn main() {}

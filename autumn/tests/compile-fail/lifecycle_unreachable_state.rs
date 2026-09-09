//! An orphan state no transition targets is a compile error (#1675): a state no
//! record can ever enter is a modelling mistake, not a runtime concern.

use autumn_web::lifecycle;

#[lifecycle(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> Paid,
        Paid -> Delivered,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Pending,
    Paid,
    // No edge targets `Refunded`.
    Refunded,
    Delivered,
}

fn main() {}

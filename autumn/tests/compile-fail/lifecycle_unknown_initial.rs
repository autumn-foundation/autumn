//! Issue #1675: declaring an `initial` state that is not a variant of the enum
//! is a macro error naming the offending state.

use autumn_web::lifecycle;

#[lifecycle(
    initial = Nonexistent,
    terminal(Delivered, Cancelled),
    transitions(
        Pending -> Paid,
        Paid -> Shipped,
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

fn main() {}

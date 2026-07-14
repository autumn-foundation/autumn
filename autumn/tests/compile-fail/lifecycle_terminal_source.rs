//! Issue #1675: a declared terminal state must have no outgoing transitions.
//! Declaring `terminal(Delivered)` together with a `Delivered -> Reopened`
//! edge is a macro error naming the offending terminal and its target — a
//! terminal state that can be left is not actually terminal.

use autumn_web::lifecycle;

#[lifecycle(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> Paid,
        Paid -> Shipped,
        Shipped -> Delivered,
        Delivered -> Reopened,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Pending,
    Paid,
    Shipped,
    Delivered,
    Reopened,
}

fn main() {}

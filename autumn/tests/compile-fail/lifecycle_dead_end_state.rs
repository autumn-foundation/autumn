//! A reachable non-terminal state with no path to a terminal is a compile error
//! (#1675): records that enter it are trapped forever.

use autumn_web::lifecycle;

#[lifecycle(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> OnHold,
        Pending -> Paid,
        Paid -> Delivered,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Pending,
    // `OnHold` can be entered but never left.
    OnHold,
    Paid,
    Delivered,
}

fn main() {}

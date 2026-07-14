//! Issue #1675: firing an undeclared transition does not compile.

use autumn_web::lifecycle;

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

fn main() {
    // `Pending -> Delivered` is not a declared edge, so `to_delivered` does
    // not exist on `Machine<Pending>` — this line cannot compile.
    let _ = order_state::Machine::start().to_delivered();
}

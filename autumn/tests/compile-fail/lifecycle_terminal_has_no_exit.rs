//! Issue #1675: a terminal state has no outgoing transitions, so any attempt
//! to leave it does not compile.

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
    let machine = order_state::Machine::start()
        .to_paid()
        .to_shipped()
        .to_delivered(); // Machine<Delivered> — a terminal state.

    // Delivered is terminal: it is the source of no declared edge, so it has
    // no outgoing transition methods at all.
    let _ = machine.to_paid();
}

//! Issue #1675: `start()` exists only on the declared initial state marker, so
//! starting a lifecycle from any other state does not compile.

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
    // `start()` is defined only on `Machine<Pending>`, the initial state.
    let _ = order_state::Machine::<order_state::Paid>::start();
}

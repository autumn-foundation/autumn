//! A well-formed `#[lifecycle]` order lifecycle: exercises the typestate
//! `Machine` (start on the initial state, chained declared transitions,
//! `current()`), the generated metadata consts, and `can_transition_to`.

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
    // Typestate walk: start on the initial state and chain declared edges.
    let machine = order_state::Machine::start(); // Machine<Pending>
    let machine = machine.to_paid(); // Machine<Paid>
    let machine = machine.to_shipped(); // Machine<Shipped>
    let machine = machine.to_delivered(); // Machine<Delivered>
    assert_eq!(machine.current(), OrderState::Delivered);

    // Generated metadata consts.
    let _: OrderState = OrderState::LIFECYCLE_INITIAL;
    let _: &[OrderState] = OrderState::LIFECYCLE_TERMINALS;
    let _: &[OrderState] = OrderState::LIFECYCLE_STATES;
    let _: &[(OrderState, OrderState)] = OrderState::LIFECYCLE_TRANSITIONS;

    // Runtime transition check.
    assert!(OrderState::Pending.can_transition_to(&OrderState::Paid));
    assert!(!OrderState::Pending.can_transition_to(&OrderState::Delivered));
}

// `#[event_consumer]` finds no payload binding. It refuses, because a check
// that sees no payload checks nothing.
use autumn_web::outbox::OutboxMessage;
use autumn_web::prelude::*;

#[event(name = "order.placed")]
pub struct OrderPlaced {
    pub id: i64,
}

#[event_consumer(OrderPlaced)]
async fn post_to_ledger(_state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let _ = message.payload;
    Ok(())
}

fn main() {}

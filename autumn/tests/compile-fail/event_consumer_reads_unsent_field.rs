// An outbox topic handler reads a field that the event never puts on the
// wire. `#[event_consumer]` finds the payload by its type and fails the build.
use autumn_web::outbox::OutboxMessage;
use autumn_web::prelude::*;

#[event(name = "order.placed")]
pub struct OrderPlaced {
    pub id: i64,
    #[serde(skip_serializing)]
    pub total_cents: i64,
}

#[event_consumer(OrderPlaced)]
async fn post_to_ledger(_state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let order: OrderPlaced = message.payload_as()?;
    let _ = order.total_cents;
    Ok(())
}

fn main() {}

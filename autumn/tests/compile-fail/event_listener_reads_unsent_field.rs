// A listener reads a field that the event never puts on the wire. The Rust
// field still exists, so the types agree. Only the event contract sees it.
use autumn_web::prelude::*;

#[event(name = "contact.updated")]
pub struct ContactUpdated {
    pub id: i64,
    #[serde(skip_serializing)]
    pub email: String,
}

#[listener(ContactUpdated, durable)]
async fn sync_marketing(_state: AppState, event: ContactUpdated) -> AutumnResult<()> {
    let _ = event.email;
    Ok(())
}

fn main() {}

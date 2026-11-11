// A listener reads a field that the event no longer has. The build fails, and
// the message names the event and the field.
use autumn_web::prelude::*;

#[event(name = "contact.updated")]
pub struct ContactUpdated {
    pub id: i64,
}

#[listener(ContactUpdated)]
async fn sync_marketing(_state: AppState, event: ContactUpdated) -> AutumnResult<()> {
    let _ = event.email;
    Ok(())
}

fn main() {}

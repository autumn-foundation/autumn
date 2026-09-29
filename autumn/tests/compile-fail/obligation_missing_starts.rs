//! Issue #1826: `#[obligation]` needs a `starts` field.

use autumn_web::obligation;

#[obligation(name = first_response, within = "2 business days")]
pub struct Ticket {
    pub id: i64,
}

fn main() {}

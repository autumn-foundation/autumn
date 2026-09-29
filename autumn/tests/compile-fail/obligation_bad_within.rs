//! Issue #1826: a bad business duration in `#[obligation]` is a compile
//! error on the literal, not a runtime surprise.

use autumn_web::obligation;

#[obligation(
    name = first_response,
    within = "2 bussiness days",
    starts = opened_at,
)]
pub struct Ticket {
    pub id: i64,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}

fn main() {}

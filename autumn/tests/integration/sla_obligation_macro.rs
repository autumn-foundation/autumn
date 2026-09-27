//! `#[obligation]` declares a business-time obligation on a model (#1826).

use autumn_web::obligation;
use autumn_web::sla::{BusinessDuration, Obligation};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

#[obligation(
    first_response,
    within = "2 business days",
    calendar = "support",
    starts = opened_at,
    met = responded_at,
    zone = customer_zone
)]
#[obligation(resolution, within = "5 business days, 4 business hours", starts = opened_at)]
#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: i64,
    pub opened_at: DateTime<Utc>,
    pub responded_at: Option<DateTime<Utc>>,
    pub customer_zone: String,
}

#[obligation(refund_window, within = "30 business minutes", starts = placed_at, subject = number, zone = zone)]
pub struct Order {
    pub number: String,
    pub placed_at: DateTime<Utc>,
    pub zone: Option<Tz>,
}

fn opened() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 5, 15, 0, 0).unwrap()
}

fn ticket() -> Ticket {
    Ticket {
        id: 42,
        opened_at: opened(),
        responded_at: None,
        customer_zone: "America/New_York".to_owned(),
    }
}

#[test]
fn obligation_macro_builds_the_declared_obligation() {
    let ny: Tz = "America/New_York".parse().unwrap();
    let want = Obligation::new("first_response", "ticket:42")
        .within(BusinessDuration::days(2))
        .calendar("support")
        .starting_at(opened())
        .met_at(None)
        .zone(ny);
    assert_eq!(ticket().first_response_obligation(), want);
    assert_eq!(ticket().first_response_obligation().key(), "first_response/ticket:42");
}

#[test]
fn obligation_macro_reads_the_met_field() {
    let mut t = ticket();
    let at = Utc.with_ymd_and_hms(2024, 1, 8, 10, 0, 0).unwrap();
    t.responded_at = Some(at);
    assert_eq!(t.first_response_obligation().met(), Some(at));
}

#[test]
fn obligation_macro_allows_many_obligations_and_defaults() {
    let ob = ticket().resolution_obligation();
    assert_eq!(ob.name(), "resolution");
    assert_eq!(ob.calendar_name(), Obligation::DEFAULT_CALENDAR);
    assert_eq!(ob.time_zone(), None);
    assert_eq!(ob.met(), None);
    assert_eq!(
        ob.budget(),
        "5 business days, 4 business hours".parse().unwrap()
    );
}

#[test]
fn obligation_macro_uses_the_subject_field_and_optional_zone() {
    let order = Order {
        number: "A-7".to_owned(),
        placed_at: opened(),
        zone: None,
    };
    let ob = order.refund_window_obligation();
    assert_eq!(ob.subject(), "order:A-7");
    assert_eq!(ob.budget(), BusinessDuration::minutes(30));
    assert_eq!(ob.time_zone(), None);
}

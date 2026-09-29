//! `#[obligation]` declares a business-time obligation on a model (#1826).

use autumn_web::obligation;
use autumn_web::sla::{BusinessDuration, Obligation};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

#[obligation(
    name = first_response,
    within = "2 business days",
    calendar = "support",
    starts = opened_at,
    met = responded_at,
    zone = customer_zone
)]
#[obligation(name = resolution, within = "5 business days, 4 business hours", starts = opened_at)]
#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: i64,
    pub opened_at: DateTime<Utc>,
    pub responded_at: Option<DateTime<Utc>>,
    pub customer_zone: String,
}

#[obligation(name = refund_window, within = "30 business minutes", starts = placed_at, subject = number, zone = zone)]
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
    assert_eq!(
        ticket().first_response_obligation().key(),
        "first_response/ticket:42"
    );
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

/// The macro parses `within` at compile time with its own parser. Each row
/// must agree with the runtime parser.
macro_rules! parity {
    ($($ty:ident => $text:literal),* $(,)?) => {
        $(
            #[obligation(name = check, within = $text, starts = at)]
            #[allow(dead_code)]
            struct $ty {
                id: u8,
                at: DateTime<Utc>,
            }
        )*

        #[test]
        fn obligation_macro_and_runtime_parse_within_the_same_way() {
            $(
                let value = $ty { id: 1, at: opened() };
                let runtime: BusinessDuration = $text.parse().unwrap();
                assert_eq!(value.check_obligation().budget(), runtime, "{}", $text);
            )*
        }
    };
}

parity! {
    P1 => "2 business days",
    P2 => "1 business day",
    P3 => "4 business hours",
    P4 => "30 business minutes",
    P5 => "1 day, 4 hours",
    P6 => "1 DAY AND 30 MINUTES",
    P7 => "90 seconds",
    P8 => "2 days, 3 business hours and 15 minutes",
}

#[obligation(name = r#type, within = "1 business day", starts = opened_at)]
pub struct Raw {
    pub id: i64,
    pub opened_at: DateTime<Utc>,
}

#[test]
fn obligation_macro_strips_raw_identifiers() {
    let raw = Raw {
        id: 3,
        opened_at: opened(),
    };
    assert_eq!(raw.type_obligation().name(), "type");
}

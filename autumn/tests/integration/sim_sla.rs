//! Calendar-aware SLA obligations under the deterministic sim (issue #1826).
//!
//! Each test moves business time across weekends and holidays with the
//! injected clock. No test sleeps.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use autumn_web::job;
use autumn_web::obligation;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::sla::{
    BusinessCalendar, BusinessDuration, CHECK_JOB, ESCALATE_JOB, Obligation, ObligationState, Sla,
    SlaBreach, SlaPlugin,
};
use autumn_web::test::TestApp;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;

type Fired = Arc<Mutex<Vec<(SlaBreach, DateTime<Utc>)>>>;

fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
}

const fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// A plugin with a "support" calendar and a breach handler that records
/// each escalation and the virtual instant when it ran.
fn support_plugin(calendar: BusinessCalendar) -> (SlaPlugin, Fired) {
    let fired: Fired = Arc::default();
    let sink = Arc::clone(&fired);
    let plugin = SlaPlugin::new().calendar("support", calendar).on_breach(
        "first_response",
        move |state: AppState, breach: SlaBreach| {
            let sink = Arc::clone(&sink);
            async move {
                let now = state.clock().now();
                sink.lock().unwrap().push((breach, now));
                Ok(())
            }
        },
    );
    (plugin, fired)
}

fn fired_keys(fired: &Fired) -> Vec<String> {
    fired
        .lock()
        .unwrap()
        .iter()
        .map(|(b, _)| b.key.clone())
        .collect()
}

// ── Handlers: declare, read and meet an obligation ───────────────────────────

#[post("/tickets/{id}")]
async fn open_ticket(sla: Sla, Path(id): Path<u32>) -> AutumnResult<String> {
    let obligation = Obligation::new("first_response", format!("ticket:{id}"))
        .within("2 business days".parse()?)
        .calendar("support");
    let status = sla.track(&obligation).await?;
    Ok(status.due_at.map(|d| d.to_rfc3339()).unwrap_or_default())
}

#[get("/tickets/{id}/sla")]
async fn ticket_sla(sla: Sla, Path(id): Path<u32>) -> AutumnResult<String> {
    let key = format!("first_response/ticket:{id}");
    let status = sla
        .get(&key)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg("no sla"))?;
    Ok(format!("{:?} {}", status.state, status.remaining.as_secs()))
}

#[post("/tickets/{id}/respond")]
async fn respond(sla: Sla, Path(id): Path<u32>) -> AutumnResult<String> {
    let key = format!("first_response/ticket:{id}");
    let status = sla
        .meet(&key)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg("no sla"))?;
    Ok(format!("{:?}", status.state))
}

fn desk_app(plugin: SlaPlugin) -> TestApp {
    TestApp::new()
        .routes(routes![open_ticket, ticket_sla, respond])
        .plugin(plugin)
}

async fn body(sim: &Sim, path: &str) -> String {
    sim.client().get(path).send().await.text()
}

async fn post(sim: &Sim, path: &str) -> String {
    let response = sim.client().post(path).send().await;
    response.assert_ok();
    response.text()
}

#[sim_test]
async fn sim_sla_clock_pauses_on_weekend_and_holiday_then_escalates_once(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Monday 2020-01-06 is a holiday.
    let calendar =
        BusinessCalendar::weekdays("09:00-17:00".parse().unwrap()).holiday(date(2020, 1, 6));
    let (plugin, fired) = support_plugin(calendar);
    sim.build(desk_app(plugin));

    // Friday 15:00: 2h on Friday, the holiday, 8h on Tuesday, 6h on Wednesday.
    sim.advance_to(&utc(2020, 1, 3, 15, 0)).await;
    let due = post(&sim, "/tickets/1").await;
    assert_eq!(due, utc(2020, 1, 8, 15, 0).to_rfc3339());
    post(&sim, "/tickets/2").await;
    assert_eq!(body(&sim, "/tickets/1/sla").await, "Running 57600");

    // The clock stops over the weekend and on the holiday.
    sim.advance_to(&utc(2020, 1, 4, 12, 0)).await;
    sim.run_to_idle().await;
    assert_eq!(body(&sim, "/tickets/1/sla").await, "Paused 50400");
    sim.advance_to(&utc(2020, 1, 6, 12, 0)).await;
    sim.run_to_idle().await;
    assert_eq!(body(&sim, "/tickets/1/sla").await, "Paused 50400");

    // Ticket 2 gets a response on Tuesday.
    sim.advance_to(&utc(2020, 1, 7, 10, 0)).await;
    assert_eq!(post(&sim, "/tickets/2/respond").await, "Met");

    // One minute before the deadline: no escalation.
    sim.advance_to(&utc(2020, 1, 8, 14, 59)).await;
    sim.run_to_idle().await;
    assert!(fired_keys(&fired).is_empty());
    assert_eq!(body(&sim, "/tickets/1/sla").await, "Running 60");

    // After the deadline: ticket 1 escalates once, ticket 2 does not.
    sim.advance_to(&utc(2020, 1, 8, 15, 1)).await;
    sim.run_to_idle().await;
    assert_eq!(fired_keys(&fired), ["first_response/ticket:1"]);
    assert_eq!(body(&sim, "/tickets/1/sla").await, "Breached 0");
    assert_eq!(body(&sim, "/tickets/2/sla").await, "Met 46800");
    sim.client().assert_job_enqueued(ESCALATE_JOB);

    let (breach, ran_at) = fired.lock().unwrap()[0].clone();
    assert_eq!(breach.due_at, utc(2020, 1, 8, 15, 0));
    assert_eq!(breach.subject, "ticket:1");
    assert_eq!(breach.zone, "UTC");
    assert!(ran_at >= breach.due_at);

    // Time goes on; nothing fires again.
    sim.advance(Duration::from_secs(14 * 24 * 3600)).await;
    sim.run_to_idle().await;
    assert_eq!(fired_keys(&fired).len(), 1);

    job::clear_global_job_client();
}

#[sim_test]
async fn sim_sla_repeated_track_and_duplicate_checks_escalate_once(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let calendar = BusinessCalendar::weekdays("09:00-17:00".parse().unwrap());
    let (plugin, fired) = support_plugin(calendar);
    sim.build(desk_app(plugin));

    // Wednesday 2020-01-01 is a working day on this calendar.
    sim.advance_to(&utc(2020, 1, 1, 9, 0)).await;
    for _ in 0..3 {
        post(&sim, "/tickets/7").await;
    }
    sim.run_to_idle().await;

    // Due Thursday 17:00. Go past it.
    sim.advance_to(&utc(2020, 1, 2, 17, 30)).await;
    sim.run_to_idle().await;
    assert_eq!(fired_keys(&fired), ["first_response/ticket:7"]);

    // A replica that runs the same check again must not escalate again.
    let payload = serde_json::json!({
        "key": "first_response/ticket:7",
        "due_at": utc(2020, 1, 2, 17, 0),
    });
    job::enqueue(CHECK_JOB, payload.clone()).await.unwrap();
    job::enqueue(CHECK_JOB, payload).await.unwrap();
    // Tracking again after the breach must not escalate again.
    post(&sim, "/tickets/7").await;
    sim.advance(Duration::from_secs(3600)).await;
    sim.run_to_idle().await;
    assert_eq!(fired_keys(&fired), ["first_response/ticket:7"]);

    job::clear_global_job_client();
}

#[sim_test]
async fn sim_sla_status_is_readable_without_tracking(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let calendar = BusinessCalendar::weekdays("09:00-17:00".parse().unwrap());
    let (plugin, _fired) = support_plugin(calendar);
    sim.build(desk_app(plugin));
    sim.advance_to(&utc(2020, 1, 3, 16, 0)).await;

    let sla = Sla::from_state(sim.client().state()).unwrap();
    let ob = Obligation::new("first_response", "ticket:9")
        .within("4 business hours".parse().unwrap())
        .calendar("support")
        .starting_at(utc(2020, 1, 3, 15, 0));
    let status = sla.status(&ob).unwrap();
    assert_eq!(status.state, ObligationState::Running);
    assert_eq!(status.remaining, Duration::from_secs(3 * 3600));
    assert_eq!(status.due_at, Some(utc(2020, 1, 6, 11, 0)));

    let unknown = ob.clone().calendar("nope");
    assert!(sla.status(&unknown).is_err());
    let no_budget = Obligation::new("first_response", "ticket:10").calendar("support");
    assert!(
        sla.track(&no_budget).await.is_err(),
        "a zero budget is refused"
    );
    assert!(
        sla.get(&ob.key()).await.unwrap().is_none(),
        "status does not track"
    );

    job::clear_global_job_client();
}

#[sim_test]
async fn sim_sla_zone_resolves_from_obligation_then_calendar_then_app(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let ny: Tz = "America/New_York".parse().unwrap();
    let tokyo: Tz = "Asia/Tokyo".parse().unwrap();
    let hours = "09:00-17:00".parse().unwrap();
    let plugin = SlaPlugin::new()
        .calendar("home", BusinessCalendar::weekdays(hours).zone(ny))
        .calendar("plain", BusinessCalendar::weekdays(hours));
    sim.build(TestApp::new().plugin(plugin));

    let sla = Sla::from_state(sim.client().state()).unwrap();
    let ob = |calendar: &str| {
        Obligation::new("first_response", "ticket:1")
            .within(BusinessDuration::hours(1))
            .calendar(calendar)
    };
    // 1. The zone of the obligation wins.
    assert_eq!(sla.status(&ob("home").zone(tokyo)).unwrap().zone, tokyo);
    // 2. Then the home zone of the calendar.
    assert_eq!(sla.status(&ob("home")).unwrap().zone, ny);
    // 3. Then the app default (`[time_zone]`, UTC here).
    assert_eq!(sla.status(&ob("plain")).unwrap().zone, Tz::UTC);

    // `track` keeps the zone it found.
    let tracked = sla.track(&ob("home")).await.unwrap();
    assert_eq!(tracked.zone, ny);
    let stored = sla.get(&ob("home").key()).await.unwrap().unwrap();
    assert_eq!(stored.zone, ny);

    job::clear_global_job_client();
}

#[sim_test]
async fn sim_sla_track_refuses_obligations_that_cannot_escalate(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let hours = "09:00-17:00".parse().unwrap();
    let plugin = SlaPlugin::new()
        .calendar(
            "zero_day",
            BusinessCalendar::weekdays(hours).business_day(Duration::ZERO),
        )
        .calendar("closed", BusinessCalendar::new());
    sim.build(TestApp::new().plugin(plugin));
    let sla = Sla::from_state(sim.client().state()).unwrap();

    // Two days of a zero-length business day is zero working time.
    let zero = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::days(2))
        .calendar("zero_day");
    assert!(sla.track(&zero).await.is_err());

    // No working time: no deadline. The new record is not kept.
    let closed = Obligation::new("first_response", "ticket:2")
        .within(BusinessDuration::hours(1))
        .calendar("closed");
    assert!(sla.track(&closed).await.is_err());
    assert!(sla.get(&closed.key()).await.unwrap().is_none());

    job::clear_global_job_client();
}

// ── A quarter of a support desk ──────────────────────────────────────────────

/// A support ticket. The obligation is declared on the model.
#[obligation(
    name = first_response,
    within = "1 business day",
    calendar = "support",
    starts = opened_at,
    met = responded_at,
    zone = customer_zone
)]
#[derive(Debug, Clone)]
struct Ticket {
    id: u32,
    opened_at: DateTime<Utc>,
    responded_at: Option<DateTime<Utc>>,
    customer_zone: String,
}

const Q1_HOLIDAYS: [(u32, u32); 3] = [(1, 1), (1, 20), (2, 17)];

/// An independent oracle: step one minute at a time and count the minutes
/// in working hours (Mon-Fri 09:00-17:00 local, not a holiday).
fn oracle_due(opened: DateTime<Utc>, zone: Tz, holidays: &[(u32, u32)]) -> DateTime<Utc> {
    let mut at = opened;
    let mut minutes_left = 8 * 60;
    loop {
        let local = at.with_timezone(&zone);
        let weekday = local.weekday();
        let working = !matches!(weekday, Weekday::Sat | Weekday::Sun)
            && !holidays.contains(&(local.month(), local.day()))
            && (9..17).contains(&local.hour());
        if working {
            minutes_left -= 1;
        }
        at += chrono::Duration::minutes(1);
        if minutes_left == 0 {
            return at;
        }
    }
}

#[sim_test]
async fn sim_sla_support_desk_quarter(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let mut calendar = BusinessCalendar::weekdays("09:00-17:00".parse().unwrap());
    for (month, day) in Q1_HOLIDAYS {
        calendar = calendar.annual_holiday(month, day);
    }
    let (plugin, fired) = support_plugin(calendar);
    sim.build(TestApp::new().plugin(plugin));

    // Two tickets a day, every day of Q1 2020 (weekends too), at 14:00Z.
    // Replies: after 1 hour, never, or after 60 hours.
    let zones = ["UTC", "America/New_York"];
    let mut tickets = BTreeMap::new();
    let mut events: Vec<(DateTime<Utc>, u32)> = Vec::new();
    let mut day = utc(2020, 1, 1, 14, 0);
    let mut id = 0_u32;
    while day < utc(2020, 4, 1, 0, 0) {
        for zone in zones {
            id += 1;
            let reply = match id % 3 {
                0 => Some(day + chrono::Duration::hours(1)),
                1 => None,
                _ => Some(day + chrono::Duration::hours(60)),
            };
            tickets.insert(
                id,
                (
                    Ticket {
                        id,
                        opened_at: day,
                        responded_at: None,
                        customer_zone: zone.to_owned(),
                    },
                    reply,
                ),
            );
            events.push((day, id));
            if let Some(reply) = reply {
                events.push((reply, id));
            }
        }
        day += chrono::Duration::days(1);
    }
    events.sort();

    // The oracle decides which tickets must breach.
    let mut expected = BTreeMap::new();
    let mut holiday_matters = false;
    for (ticket, reply) in tickets.values() {
        let zone: Tz = ticket.customer_zone.parse().unwrap();
        let due = oracle_due(ticket.opened_at, zone, &Q1_HOLIDAYS);
        holiday_matters |= due != oracle_due(ticket.opened_at, zone, &[]);
        if reply.is_none_or(|r| r > due) {
            expected.insert(format!("first_response/ticket:{}", ticket.id), due);
        }
    }
    assert!(holiday_matters, "the scenario must cross a holiday");
    assert!(!expected.is_empty());

    let sla = Sla::from_state(sim.client().state()).unwrap();
    let wall_start = Instant::now();
    for (at, id) in events {
        sim.advance_to(&at).await;
        let (ticket, _) = tickets.get_mut(&id).unwrap();
        if ticket.opened_at < at {
            // No drain first: a late reply is still a breach, even when
            // the check job has not run yet.
            ticket.responded_at = Some(at);
        }
        sla.track(&ticket.first_response_obligation())
            .await
            .unwrap();
    }
    sim.advance_to(&utc(2020, 4, 30, 0, 0)).await;
    sim.run_to_idle().await;
    let wall = wall_start.elapsed();

    let fired = fired.lock().unwrap().clone();
    eprintln!(
        "support desk quarter: {} tickets, {} escalations, {wall:?} of wall time",
        tickets.len(),
        fired.len()
    );
    let keys: Vec<_> = fired.iter().map(|(b, _)| b.key.clone()).collect();
    let unique: BTreeSet<_> = keys.iter().cloned().collect();
    assert_eq!(keys.len(), unique.len(), "each escalation fires once");
    assert_eq!(unique, expected.keys().cloned().collect());
    for (breach, ran_at) in &fired {
        assert_eq!(breach.due_at, expected[&breach.key], "{}", breach.key);
        assert!(*ran_at >= breach.due_at, "{} fired early", breach.key);
        // The sim steps from event to event (at most one day apart), so the
        // claim happens at the first step after the deadline.
        assert!(
            breach.escalated_at <= breach.due_at + chrono::Duration::days(1),
            "{} escalated late, at {}",
            breach.key,
            breach.escalated_at
        );
    }
    // About 0.25 s on a Linux debug build. The limit is loose for slow
    // runners; it catches a real sleep, not jitter.
    assert!(
        wall < Duration::from_secs(5),
        "a quarter of business time took {wall:?} of wall time"
    );

    job::clear_global_job_client();
}

//! Unit tests of the pure SLA engine (issue #1826).

use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;

use super::*;

const HOUR: Duration = Duration::from_secs(3600);

fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn office() -> BusinessCalendar {
    BusinessCalendar::weekdays("09:00-17:00".parse().unwrap())
}

// ── WorkingHours ─────────────────────────────────────────────────────────────

#[test]
fn working_hours_parse_accepts_a_window() {
    let hours: WorkingHours = "09:00-17:00".parse().unwrap();
    assert_eq!(hours.length(), 8 * HOUR);
}

#[test]
fn working_hours_parse_accepts_end_of_day() {
    let hours: WorkingHours = "00:00-24:00".parse().unwrap();
    assert_eq!(hours, WorkingHours::ALL_DAY);
}

#[test]
fn working_hours_parse_rejects_bad_text() {
    for text in [
        "17:00-09:00",
        "09:00-09:00",
        "9-5",
        "",
        "09:00",
        "25:00-26:00",
        "09:60-10:00",
        "09:+5-17:00",
        "+9:00-17:00",
    ] {
        assert!(
            matches!(text.parse::<WorkingHours>(), Err(SlaError::InvalidHours(_))),
            "{text:?} must be rejected"
        );
    }
}

#[test]
fn working_hours_new_treats_midnight_close_as_end_of_day() {
    let open = NaiveTime::from_hms_opt(22, 0, 0).unwrap();
    let hours = WorkingHours::new(open, NaiveTime::MIN).unwrap();
    assert_eq!(hours.length(), 2 * HOUR);
}

// ── BusinessCalendar: working time ───────────────────────────────────────────

#[test]
fn working_time_inside_one_day() {
    let cal = office();
    // 2024-01-08 is a Monday.
    let got = cal.working_time(utc(2024, 1, 8, 10, 0), utc(2024, 1, 8, 12, 0), Tz::UTC);
    assert_eq!(got, 2 * HOUR);
}

#[test]
fn working_time_skips_the_weekend() {
    let cal = office();
    // Friday 16:00 to Monday 10:00: 1h on Friday and 1h on Monday.
    let got = cal.working_time(utc(2024, 1, 5, 16, 0), utc(2024, 1, 8, 10, 0), Tz::UTC);
    assert_eq!(got, 2 * HOUR);
}

#[test]
fn working_time_skips_a_holiday() {
    let cal = office().holiday(date(2024, 1, 8));
    // Friday 16:00 to Tuesday 10:00, Monday is a holiday.
    let got = cal.working_time(utc(2024, 1, 5, 16, 0), utc(2024, 1, 9, 10, 0), Tz::UTC);
    assert_eq!(got, 2 * HOUR);
}

#[test]
fn working_time_skips_an_annual_holiday_every_year() {
    let cal = office().annual_holiday(12, 25);
    assert!(cal.is_holiday(date(2024, 12, 25)));
    assert!(cal.is_holiday(date(2030, 12, 25)));
    // 2024-12-25 is a Wednesday.
    let got = cal.working_time(utc(2024, 12, 25, 0, 0), utc(2024, 12, 26, 0, 0), Tz::UTC);
    assert_eq!(got, Duration::ZERO);
}

#[test]
fn working_time_is_zero_for_a_reversed_range() {
    let cal = office();
    let got = cal.working_time(utc(2024, 1, 8, 12, 0), utc(2024, 1, 8, 10, 0), Tz::UTC);
    assert_eq!(got, Duration::ZERO);
}

#[test]
fn working_time_uses_local_hours_of_the_zone() {
    let cal = office();
    let ny: Tz = "America/New_York".parse().unwrap();
    // 20:00Z-23:00Z on a January Monday is 15:00-18:00 in New York (EST).
    let from = utc(2024, 1, 8, 20, 0);
    let to = utc(2024, 1, 8, 23, 0);
    assert_eq!(cal.working_time(from, to, ny), 2 * HOUR);
    assert_eq!(cal.working_time(from, to, Tz::UTC), Duration::ZERO);
}

#[test]
fn working_time_counts_real_hours_on_a_dst_day() {
    let ny: Tz = "America/New_York".parse().unwrap();
    let cal = BusinessCalendar::new().hours(Weekday::Sun, WorkingHours::ALL_DAY);
    // 2024-03-10 is a Sunday; clocks go forward at 02:00, so the day has 23h.
    let start = ny
        .with_ymd_and_hms(2024, 3, 10, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let end = ny
        .with_ymd_and_hms(2024, 3, 11, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(cal.working_time(start, end, ny), 23 * HOUR);
}

#[test]
fn overlapping_windows_merge() {
    let cal = BusinessCalendar::new()
        .hours(Weekday::Mon, "09:00-12:00".parse().unwrap())
        .hours(Weekday::Mon, "11:00-14:00".parse().unwrap());
    let got = cal.working_time(utc(2024, 1, 8, 0, 0), utc(2024, 1, 9, 0, 0), Tz::UTC);
    assert_eq!(got, 5 * HOUR);
}

#[test]
fn a_lunch_break_is_not_working_time() {
    let cal = BusinessCalendar::weekdays("09:00-12:00".parse().unwrap())
        .hours(Weekday::Mon, "13:00-17:00".parse().unwrap());
    let got = cal.working_time(utc(2024, 1, 8, 0, 0), utc(2024, 1, 9, 0, 0), Tz::UTC);
    assert_eq!(got, 7 * HOUR);
    assert!(!cal.is_working(utc(2024, 1, 8, 12, 30), Tz::UTC));
    assert!(cal.is_working(utc(2024, 1, 8, 13, 0), Tz::UTC));
}

// ── BusinessCalendar: deadline ───────────────────────────────────────────────

#[test]
fn deadline_crosses_the_weekend() {
    let cal = office();
    // Friday 15:00 + 16h: Friday 2h, Monday 8h, Tuesday 6h.
    let due = cal.deadline(utc(2024, 1, 5, 15, 0), 16 * HOUR, Tz::UTC);
    assert_eq!(due, Some(utc(2024, 1, 9, 15, 0)));
}

#[test]
fn deadline_crosses_a_holiday() {
    let cal = office().holiday(date(2024, 1, 8));
    let due = cal.deadline(utc(2024, 1, 5, 15, 0), 16 * HOUR, Tz::UTC);
    assert_eq!(due, Some(utc(2024, 1, 10, 15, 0)));
}

#[test]
fn deadline_from_outside_hours_starts_at_the_next_opening() {
    let cal = office();
    // Saturday noon + 1h is Monday 10:00.
    let due = cal.deadline(utc(2024, 1, 6, 12, 0), HOUR, Tz::UTC);
    assert_eq!(due, Some(utc(2024, 1, 8, 10, 0)));
}

#[test]
fn deadline_that_uses_a_full_window_is_the_close() {
    let cal = office();
    let due = cal.deadline(utc(2024, 1, 8, 9, 0), 8 * HOUR, Tz::UTC);
    assert_eq!(due, Some(utc(2024, 1, 8, 17, 0)));
}

#[test]
fn deadline_is_none_without_working_time() {
    let cal = BusinessCalendar::new();
    assert_eq!(cal.deadline(utc(2024, 1, 8, 9, 0), HOUR, Tz::UTC), None);
    assert_eq!(
        cal.next_working_instant(utc(2024, 1, 8, 9, 0), Tz::UTC),
        None
    );
}

#[test]
fn deadline_and_working_time_agree() {
    let cal = office().holiday(date(2024, 1, 15));
    let ny: Tz = "America/New_York".parse().unwrap();
    let start = utc(2024, 1, 3, 13, 17);
    for hours in [1_u64, 7, 8, 9, 40, 100] {
        let budget = Duration::from_secs(hours * 3600);
        let due = cal.deadline(start, budget, ny).unwrap();
        assert_eq!(cal.working_time(start, due, ny), budget, "{hours}h");
    }
}

#[test]
fn next_working_instant_on_a_weekend_is_monday_opening() {
    let cal = office();
    assert_eq!(
        cal.next_working_instant(utc(2024, 1, 6, 12, 0), Tz::UTC),
        Some(utc(2024, 1, 8, 9, 0))
    );
    assert_eq!(
        cal.next_working_instant(utc(2024, 1, 8, 10, 0), Tz::UTC),
        Some(utc(2024, 1, 8, 10, 0))
    );
}

#[test]
fn day_length_defaults_to_the_longest_day() {
    let cal = office().hours(Weekday::Sat, "10:00-12:00".parse().unwrap());
    assert_eq!(cal.day_length(), 8 * HOUR);
    assert_eq!(cal.business_day(6 * HOUR).day_length(), 6 * HOUR);
}

#[test]
fn calendar_keeps_a_home_zone() {
    let ny: Tz = "America/New_York".parse().unwrap();
    assert_eq!(office().home_zone(), None);
    assert_eq!(office().zone(ny).home_zone(), Some(ny));
}

#[test]
fn zero_budget_outside_hours_is_due_at_the_next_opening() {
    let due = office().deadline(utc(2024, 1, 6, 12, 0), Duration::ZERO, Tz::UTC);
    assert_eq!(due, Some(utc(2024, 1, 8, 9, 0)));
}

#[test]
fn a_long_budget_inside_the_horizon_has_a_deadline() {
    // 3000 business days is about 11.5 years.
    let budget = BusinessDuration::days(3000).resolve(&office());
    let due = office().deadline(utc(2024, 1, 8, 9, 0), budget, Tz::UTC);
    assert!(due.is_some_and(|d| d > utc(2035, 1, 1, 0, 0)));
}

#[test]
fn far_future_instants_do_not_panic() {
    let kiritimati: Tz = "Pacific/Kiritimati".parse().unwrap();
    let far = DateTime::<Utc>::MAX_UTC;
    assert!(!office().is_working(far, kiritimati));
    let _ = office().working_time(utc(2024, 1, 8, 9, 0), far, Tz::UTC);
}

#[test]
fn a_skipped_local_day_keeps_the_day_before() {
    // Samoa skipped 2011-12-30. The window of Thursday 2011-12-29 stays whole.
    let apia: Tz = "Pacific/Apia".parse().unwrap();
    let cal = BusinessCalendar::weekdays(WorkingHours::ALL_DAY);
    let start = apia
        .with_ymd_and_hms(2011, 12, 29, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let end = apia
        .with_ymd_and_hms(2011, 12, 31, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(cal.working_time(start, end, apia), 24 * HOUR);
    let noon = apia
        .with_ymd_and_hms(2011, 12, 29, 12, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert!(cal.is_working(noon, apia));
}

#[test]
fn a_window_that_opens_in_a_dst_gap_moves_forward() {
    let ny: Tz = "America/New_York".parse().unwrap();
    // 2024-03-10 is a Sunday; 02:00-03:00 does not exist.
    let cal = BusinessCalendar::new().hours(Weekday::Sun, "02:30-04:00".parse().unwrap());
    let start = ny
        .with_ymd_and_hms(2024, 3, 10, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let end = ny
        .with_ymd_and_hms(2024, 3, 11, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    // 02:30 moves to 03:30, so the window is 03:30-04:00.
    assert_eq!(cal.working_time(start, end, ny), Duration::from_secs(1800));
}

#[test]
fn a_long_run_of_dated_holidays_does_not_stop_the_scan() {
    let start = date(2024, 1, 1);
    let cal = (0..500)
        .map(|n| start + chrono::Days::new(n))
        .fold(office(), BusinessCalendar::holiday);
    // Work resumes on Thursday 2025-05-15, after 500 days of holidays.
    let due = cal.deadline(utc(2024, 1, 1, 9, 0), HOUR, Tz::UTC);
    assert_eq!(due, Some(utc(2025, 5, 15, 10, 0)));
    assert_eq!(
        cal.working_time(utc(2024, 1, 1, 9, 0), due.unwrap(), Tz::UTC),
        HOUR
    );
}

#[test]
fn sparse_annual_working_days_are_found_inside_the_horizon() {
    // Work every day, but every date except February 29 is a holiday.
    let mut cal = BusinessCalendar::new();
    for day in [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ] {
        cal = cal.hours(day, WorkingHours::ALL_DAY);
    }
    for month in 1..=12 {
        for day in 1..=31 {
            if (month, day) != (2, 29) {
                cal = cal.annual_holiday(month, day);
            }
        }
    }
    let from = utc(2024, 3, 1, 0, 0);
    assert_eq!(
        cal.next_working_instant(from, Tz::UTC),
        Some(utc(2028, 2, 29, 0, 0))
    );
    assert_eq!(
        cal.deadline(from, HOUR, Tz::UTC),
        Some(utc(2028, 2, 29, 1, 0))
    );
}

// ── BusinessDuration ─────────────────────────────────────────────────────────

#[test]
fn duration_parses_units() {
    let cases = [
        ("2 business days", BusinessDuration::days(2)),
        ("1 business day", BusinessDuration::days(1)),
        ("4 business hours", BusinessDuration::hours(4)),
        ("30 business minutes", BusinessDuration::minutes(30)),
        ("2 days", BusinessDuration::days(2)),
        ("2 BUSINESS DAYS", BusinessDuration::days(2)),
        (
            "1 business day, 4 business hours",
            BusinessDuration::from_parts(1, 4 * 3600),
        ),
        (
            "1 day and 30 minutes",
            BusinessDuration::from_parts(1, 30 * 60),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(text.parse::<BusinessDuration>(), Ok(want), "{text:?}");
    }
}

#[test]
fn duration_rejects_bad_text() {
    for text in [
        "",
        "two days",
        "2 fortnights",
        "2",
        "business days",
        "2 business",
        "-1 days",
        "+2 days",
        "and 2 and days",
        "1 day 2 hours",
        "1 day,",
    ] {
        assert!(
            matches!(
                text.parse::<BusinessDuration>(),
                Err(SlaError::InvalidDuration(_))
            ),
            "{text:?} must be rejected"
        );
    }
}

#[test]
fn duration_display_round_trips() {
    for text in [
        "2 business days",
        "1 business day",
        "4 business hours",
        "1 business day, 4 business hours, 30 business minutes",
        "0 business minutes",
    ] {
        let parsed: BusinessDuration = text.parse().unwrap();
        assert_eq!(parsed.to_string(), text);
    }
}

#[test]
fn duration_resolves_days_with_the_calendar_day_length() {
    let budget = BusinessDuration::from_parts(2, 3600);
    assert_eq!(budget.resolve(&office()), 17 * HOUR);
}

#[test]
fn huge_duration_saturates() {
    let budget = BusinessDuration::from_parts(u32::MAX, u64::MAX);
    assert_eq!(budget.resolve(&office()), Duration::MAX);
}

// ── Obligation status ────────────────────────────────────────────────────────

fn ticket(start: DateTime<Utc>) -> Obligation {
    Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::days(2))
        .calendar("support")
        .starting_at(start)
}

#[test]
fn obligation_key_joins_name_and_subject() {
    assert_eq!(
        ticket(utc(2024, 1, 5, 15, 0)).key(),
        "first_response/ticket:1"
    );
}

#[test]
fn status_runs_in_working_time() {
    let status =
        ticket(utc(2024, 1, 8, 9, 0)).status_with(&office(), Tz::UTC, utc(2024, 1, 8, 12, 0));
    assert_eq!(status.state, ObligationState::Running);
    assert_eq!(status.elapsed, 3 * HOUR);
    assert_eq!(status.remaining, 13 * HOUR);
    assert_eq!(status.budget, 16 * HOUR);
    assert_eq!(status.due_at, Some(utc(2024, 1, 9, 17, 0)));
    assert_eq!(status.resumes_at, None);
}

#[test]
fn status_pauses_on_the_weekend_and_keeps_the_budget() {
    let ob = ticket(utc(2024, 1, 5, 15, 0));
    let friday_close = ob.status_with(&office(), Tz::UTC, utc(2024, 1, 5, 17, 0));
    let sunday = ob.status_with(&office(), Tz::UTC, utc(2024, 1, 7, 12, 0));
    assert_eq!(sunday.state, ObligationState::Paused);
    assert_eq!(sunday.remaining, friday_close.remaining);
    assert_eq!(sunday.remaining, 14 * HOUR);
    assert_eq!(sunday.resumes_at, Some(utc(2024, 1, 8, 9, 0)));
}

#[test]
fn status_breaches_at_the_deadline() {
    let ob = ticket(utc(2024, 1, 5, 15, 0));
    let before = ob.status_with(&office(), Tz::UTC, utc(2024, 1, 9, 14, 59));
    assert_eq!(before.state, ObligationState::Running);
    assert_eq!(before.remaining, Duration::from_secs(60));
    let at = ob.status_with(&office(), Tz::UTC, utc(2024, 1, 9, 15, 0));
    assert_eq!(at.state, ObligationState::Breached);
    assert_eq!(at.remaining, Duration::ZERO);
}

#[test]
fn status_is_met_when_met_in_time() {
    let ob = ticket(utc(2024, 1, 5, 15, 0)).met_at(utc(2024, 1, 8, 10, 0));
    let status = ob.status_with(&office(), Tz::UTC, utc(2024, 2, 1, 0, 0));
    assert_eq!(status.state, ObligationState::Met);
    assert_eq!(status.elapsed, 3 * HOUR);
    assert_eq!(status.remaining, 13 * HOUR);
}

#[test]
fn status_is_met_when_met_at_the_deadline() {
    let ob = ticket(utc(2024, 1, 5, 15, 0)).met_at(utc(2024, 1, 9, 15, 0));
    let status = ob.status_with(&office(), Tz::UTC, utc(2024, 2, 1, 0, 0));
    assert_eq!(status.state, ObligationState::Met);
}

#[test]
fn status_is_breached_when_met_late() {
    let ob = ticket(utc(2024, 1, 5, 15, 0)).met_at(utc(2024, 1, 9, 16, 0));
    let status = ob.status_with(&office(), Tz::UTC, utc(2024, 2, 1, 0, 0));
    assert_eq!(status.state, ObligationState::Breached);
    assert_eq!(status.remaining, Duration::ZERO);
}

#[test]
fn status_with_no_start_starts_now() {
    let ob = Obligation::new("x", "y").within(BusinessDuration::hours(1));
    let status = ob.status_with(&office(), Tz::UTC, utc(2024, 1, 8, 10, 0));
    assert_eq!(status.started_at, utc(2024, 1, 8, 10, 0));
    assert_eq!(status.due_at, Some(utc(2024, 1, 8, 11, 0)));
}

#[test]
fn status_before_the_start_is_paused() {
    // Monday 10:00 is working time, but the clock starts on Tuesday.
    let ob = ticket(utc(2024, 1, 9, 9, 0));
    let status = ob.status_with(&office(), Tz::UTC, utc(2024, 1, 8, 10, 0));
    assert_eq!(status.state, ObligationState::Paused);
    assert_eq!(status.elapsed, Duration::ZERO);
    assert_eq!(status.resumes_at, Some(utc(2024, 1, 9, 9, 0)));
}

#[test]
fn status_never_breaches_without_working_time() {
    let ob = ticket(utc(2024, 1, 5, 15, 0));
    let status = ob.status_with(&BusinessCalendar::new(), Tz::UTC, utc(2030, 1, 1, 0, 0));
    assert_eq!(status.due_at, None);
    assert_eq!(status.state, ObligationState::Paused);
}

#[test]
fn zone_from_reads_names_and_options() {
    let ny: Tz = "America/New_York".parse().unwrap();
    let ob = Obligation::new("x", "y").zone_from("America/New_York");
    assert_eq!(ob.time_zone(), Some(ny));
    let ob = Obligation::new("x", "y")
        .zone(ny)
        .zone_from(&None::<String>);
    assert_eq!(ob.time_zone(), Some(ny), "no value keeps the zone");
    let ob = Obligation::new("x", "y").zone_from(&"Not/AZone".to_owned());
    assert_eq!(ob.time_zone(), None);
}

// ── MemoryObligationStore ────────────────────────────────────────────────────

const KEY: &str = "first_response/ticket:1";

/// The generation of the first record in each store test.
const GEN: uuid::Uuid = uuid::Uuid::from_u128(1);

fn record(start: DateTime<Utc>, generation: uuid::Uuid) -> ObligationRecord {
    ObligationRecord::new(ticket(start).zone(Tz::UTC), generation)
}

async fn store_with_one() -> MemoryObligationStore {
    let store = MemoryObligationStore::new();
    store
        .insert(record(utc(2024, 1, 5, 15, 0), GEN))
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn store_insert_keeps_the_first_record() {
    let store = MemoryObligationStore::new();
    let (first, created) = store
        .insert(record(utc(2024, 1, 5, 15, 0), GEN))
        .await
        .unwrap();
    assert!(created);
    let other = uuid::Uuid::from_u128(2);
    let (second, created) = store
        .insert(record(utc(2024, 1, 8, 9, 0), other))
        .await
        .unwrap();
    assert!(!created, "the second insert does not own the record");
    assert_eq!(first, second);
    assert_eq!(second.generation, GEN);
    assert_eq!(store.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn store_claims_an_escalation_once() {
    let store = store_with_one().await;
    let due = utc(2024, 1, 9, 15, 0);
    assert!(store.claim_escalation(KEY, GEN, due, due).await.unwrap());
    assert!(!store.claim_escalation(KEY, GEN, due, due).await.unwrap());
    // A release with another claim instant does nothing.
    store
        .release_escalation(KEY, GEN, utc(2024, 1, 1, 0, 0))
        .await
        .unwrap();
    assert!(!store.claim_escalation(KEY, GEN, due, due).await.unwrap());
    store.release_escalation(KEY, GEN, due).await.unwrap();
    assert!(store.claim_escalation(KEY, GEN, due, due).await.unwrap());
}

#[tokio::test]
async fn store_does_not_claim_a_met_or_missing_obligation() {
    let store = store_with_one().await;
    let due = utc(2024, 1, 9, 15, 0);
    assert!(
        store
            .mark_met(KEY, GEN, utc(2024, 1, 8, 10, 0))
            .await
            .unwrap()
    );
    assert!(
        !store
            .mark_met(KEY, GEN, utc(2024, 1, 8, 11, 0))
            .await
            .unwrap()
    );
    assert!(!store.claim_escalation(KEY, GEN, due, due).await.unwrap());
    assert!(!store.claim_escalation("nope", GEN, due, due).await.unwrap());
    let stored = store.get(KEY).await.unwrap().unwrap();
    assert_eq!(stored.obligation.met(), Some(utc(2024, 1, 8, 10, 0)));
}

#[tokio::test]
async fn store_claims_an_obligation_met_after_the_deadline() {
    let store = store_with_one().await;
    let due = utc(2024, 1, 9, 15, 0);
    store
        .mark_met(KEY, GEN, utc(2024, 1, 9, 16, 0))
        .await
        .unwrap();
    let later = utc(2024, 1, 9, 17, 0);
    assert!(store.claim_escalation(KEY, GEN, due, later).await.unwrap());
}

#[tokio::test]
async fn store_does_not_roll_back_a_scheduled_record() {
    // Call A creates the record. Call B adopts it and pins it.
    let store = store_with_one().await;
    assert!(store.mark_scheduled(KEY, GEN).await.unwrap());
    // Call A fails later: its rollback must not remove B's record.
    assert!(!store.remove_unscheduled(KEY, GEN).await.unwrap());
    assert!(store.get(KEY).await.unwrap().is_some());
}

#[tokio::test]
async fn store_rolls_back_an_unscheduled_record() {
    let store = store_with_one().await;
    assert!(store.remove_unscheduled(KEY, GEN).await.unwrap());
    assert!(
        !store.mark_scheduled(KEY, GEN).await.unwrap(),
        "no record to pin"
    );
}

#[tokio::test]
async fn store_writes_touch_only_their_own_generation() {
    // A slow call loaded generation GEN. Then `forget` and `track` made a
    // new record with the same start but another generation.
    let store = store_with_one().await;
    store.remove(KEY).await.unwrap();
    let new = uuid::Uuid::from_u128(2);
    store
        .insert(record(utc(2024, 1, 5, 15, 0), new))
        .await
        .unwrap();
    let due = utc(2024, 1, 9, 15, 0);

    assert!(!store.claim_escalation(KEY, GEN, due, due).await.unwrap());
    assert!(!store.mark_met(KEY, GEN, due).await.unwrap());
    assert!(!store.mark_scheduled(KEY, GEN).await.unwrap());
    assert!(!store.remove_unscheduled(KEY, GEN).await.unwrap());
    let stored = store.get(KEY).await.unwrap().unwrap();
    assert_eq!(stored.generation, new);
    assert_eq!(stored.escalated_at, None);
    assert_eq!(stored.obligation.met(), None);
    assert!(!stored.scheduled);

    // The new claim is not released by the old instance.
    assert!(store.claim_escalation(KEY, new, due, due).await.unwrap());
    store.release_escalation(KEY, GEN, due).await.unwrap();
    assert_eq!(
        store.get(KEY).await.unwrap().unwrap().escalated_at,
        Some(due)
    );
}

#[tokio::test]
async fn store_clones_share_records() {
    let store = MemoryObligationStore::new();
    let replica = store.clone();
    store
        .insert(record(utc(2024, 1, 5, 15, 0), GEN))
        .await
        .unwrap();
    assert!(
        replica
            .get("first_response/ticket:1")
            .await
            .unwrap()
            .is_some()
    );
    assert!(replica.remove("first_response/ticket:1").await.unwrap());
    assert!(
        store
            .get("first_response/ticket:1")
            .await
            .unwrap()
            .is_none()
    );
}

// ── Serialization ────────────────────────────────────────────────────────────

#[test]
fn status_serializes_to_json() {
    let status =
        ticket(utc(2024, 1, 8, 9, 0)).status_with(&office(), Tz::UTC, utc(2024, 1, 8, 12, 0));
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["state"], "running");
    assert_eq!(json["zone"], "UTC");
    assert_eq!(json["remaining"], 13 * 3600);
    assert_eq!(json["due_at"], "2024-01-09T17:00:00Z");
}

#[test]
fn duration_serializes_as_text() {
    let budget = BusinessDuration::from_parts(1, 4 * 3600);
    let json = serde_json::to_string(&budget).unwrap();
    assert_eq!(json, "\"1 business day, 4 business hours\"");
    assert_eq!(
        serde_json::from_str::<BusinessDuration>(&json).unwrap(),
        budget
    );
    assert!(serde_json::from_str::<BusinessDuration>("\"soon\"").is_err());
}

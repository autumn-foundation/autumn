# SLA Obligations and Business Calendars

Use `autumn_web::sla` to set a deadline in business time, such as "respond
within 2 business days". The clock of the deadline runs only in working hours.
It stops on weekends, on holidays and at night. When the deadline passes,
Autumn puts one escalation job on the queue.

The engine reads time only from the injected `Clock`. Thus a test can move a
full quarter of business time forward in less than one second, with no
`sleep`.

> **Status:** always on. No Cargo feature is necessary. Issue #1826.

---

## Quick start

### 1. Add a business calendar and a breach handler

```rust,ignore
use autumn_web::prelude::*;
use autumn_web::sla::{BusinessCalendar, SlaPlugin};
use chrono::NaiveDate;

let support = BusinessCalendar::weekdays("09:00-17:00".parse()?)
    .holiday(NaiveDate::from_ymd_opt(2026, 12, 25).unwrap());

let sla = SlaPlugin::new()
    .calendar("support", support)
    .on_breach("first_response", |state, breach| async move {
        // `breach` is a typed `SlaBreach`.
        tracing::warn!(key = %breach.key, due = %breach.due_at, "SLA breached");
        Ok(())
    });

autumn_web::app().plugin(sla) /* .routes(...) */;
```

### 2. Track an obligation in a handler

```rust,ignore
use autumn_web::sla::{Obligation, Sla};

#[post("/tickets/{id}")]
async fn open_ticket(sla: Sla, tz: TimeZone, Path(id): Path<i64>) -> AutumnResult<String> {
    let obligation = Obligation::new("first_response", format!("ticket:{id}"))
        .within("2 business days".parse()?)
        .calendar("support")
        .zone(*tz);
    let status = sla.track(&obligation).await?;
    Ok(format!("due at {:?}", status.due_at))
}
```

### 3. Read the remaining budget

```rust,ignore
#[get("/tickets/{id}/sla")]
async fn ticket_sla(sla: Sla, Path(id): Path<i64>) -> AutumnResult<String> {
    let key = format!("first_response/ticket:{id}");
    let status = sla.get(&key).await?.ok_or_else(|| AutumnError::not_found_msg("no SLA"))?;
    Ok(format!("{:?}, {} s left", status.state, status.remaining.as_secs()))
}
```

### 4. Meet the obligation

```rust,ignore
sla.meet("first_response/ticket:42").await?;
```

---

## Declare an obligation on a model

`#[obligation]` adds a `<name>_obligation(&self)` method to a struct.

```rust,ignore
use autumn_web::obligation;

#[obligation(
    name = first_response,
    within = "2 business days",
    calendar = "support",
    starts = opened_at,
    met = responded_at,
    zone = customer_zone,
)]
pub struct Ticket {
    pub id: i64,
    pub opened_at: chrono::DateTime<chrono::Utc>,
    pub responded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub customer_zone: String,
}

sla.track(&ticket.first_response_obligation()).await?;
```

| Argument | Necessary | Meaning |
| --- | --- | --- |
| `name` | yes | The obligation name, as an identifier. |
| `within` | yes | The budget. A bad budget is a compile error. |
| `starts` | yes | A `DateTime<Utc>` field. The clock starts there. |
| `calendar` | no | The calendar name. The default is `"default"`. |
| `met` | no | An `Option<DateTime<Utc>>` field. Set it when the obligation is met. |
| `zone` | no | A time zone field: a `Tz`, an IANA name, or an `Option` of one. |
| `subject` | no | The identity field. The default is `id`. |

The subject is `"<snake_case type>:<subject field>"`, for example
`"ticket:42"`. The key of the obligation is `"<name>/<subject>"`. A `/` or `%`
in the name is percent-encoded, so two obligations never share a key. You can put
more than one `#[obligation]` on one struct. Two types with the same name make
the same subject; give them different obligation names.

When you track the obligation again after `met` is set, Autumn marks it met.

---

## Business durations

`BusinessDuration` parses these forms. The word `business` is optional.

- `"2 business days"`
- `"4 business hours"`
- `"30 business minutes"`
- `"1 business day, 4 business hours"`

A day is one business day of the calendar. By default, that is the working
time of the longest working day (8 hours for `09:00-17:00`). Change it with
`BusinessCalendar::business_day`. Hours and minutes are working time.

---

## Business calendars

| Method | Effect |
| --- | --- |
| `BusinessCalendar::weekdays(hours)` | Work `hours` from Monday to Friday. |
| `.hours(Weekday::Sat, hours)` | Add a window to a day. Windows that overlap merge. |
| `.holiday(date)` | Stop the clock on one date. |
| `.annual_holiday(12, 25)` | Stop the clock on the same date each year. |
| `.zone(tz)` | Set the home time zone. |
| `.business_day(length)` | Set the length of one business day. |

Write a lunch break as two windows: `"09:00-12:00"` and `"13:00-17:00"`.
`"00:00-24:00"` is the full day.

### Time zone

The hours are local wall time. Autumn finds the zone in this order:

1. the zone of the obligation (`.zone(tz)` or `#[obligation(zone = ...)]`),
2. the home zone of the calendar,
3. the app default (`[time_zone] identifier` in `autumn.toml`).

`track` keeps the zone it found, so the deadline does not change later. On a
daylight-saving day, the clock counts real hours. A window time in a skipped
hour moves forward by the length of the gap (02:30 becomes 03:30).

---

## Status

`Sla::status`, `Sla::get` and `Sla::statuses` give an `ObligationStatus`:

| Field | Meaning |
| --- | --- |
| `state` | `Running`, `Paused` (outside working time or before the start), `Met` or `Breached`. |
| `due_at` | The deadline. `None` when there is no deadline in one hundred years. |
| `budget` | The budget as working time. |
| `elapsed` | The working time used. |
| `remaining` | The working time that is left. Zero when breached. |
| `resumes_at` | The next working instant while `Paused`. |
| `escalated_at` | The instant when the escalation was claimed. |

`Sla::status` does not track the obligation. Use it for a preview.
`ObligationStatus` serializes to JSON: the zone is its IANA name, and the
durations are whole seconds.

An obligation met exactly at the deadline is `Met`. An obligation met after
the deadline is `Breached`.

---

## Escalation fires once

`track` puts a check job (`autumn_sla_check`) on the job queue at the
deadline. At the deadline the check job reads the store:

- If the obligation was met on time, it stops.
- If not, it claims the escalation in the store. Then it puts one
  `autumn_sla_escalate` job on the queue. That job runs your `on_breach`
  handler with a typed `SlaBreach`.

Before the handler runs, the escalate job reads the record again. If a met
instant on or before the deadline arrived in the meantime (a late `track`), it
releases the claim and does not run the handler.

An obligation that is met after the deadline is still a breach. It escalates
even when the check job runs late. Thus the result does not change with the
speed of the workers.

The claim in the `ObligationStore` is the lock. A second check does not claim
again. The unique key of `autumn_sla_escalate` stays held for one day after
the enqueue, also after the job ran. Thus if an enqueue reports an error after
the queue stored the job, the retry of the check does not run the handler
again.

A failed handler runs again, up to 5 attempts in all, with a first delay of
1 s. Use
`on_any_breach` for obligations that have no named handler.

`track` is safe to call again. If it fails after it stored the record (for
example, the job queue refused the check), the record stays and the next call
schedules the check.

If the enqueue fails, the check releases the claim, and the queue runs the
check again. A failed release is tried again, up to 5 attempts. If the process
stops after the claim and before the enqueue, or all release attempts fail,
that escalation is lost. The record keeps `escalated_at`, so `Sla::statuses`
shows it.

### More than one replica

The default `MemoryObligationStore` is local to one process. A check job on
one replica cannot see an obligation that another replica tracked. For more
than one replica, put an `ObligationStore` on your database with
`SlaPlugin::store`, and use it on all replicas.

Each record has a `generation`, a unique id that `track` makes. The writes
after the insert (`mark_met`, `set_due`, `claim_escalation`,
`release_escalation`) must change the record only when the key and the
generation both match, in one atomic step. Thus a slow call never changes a
record that `forget` and a new `track` replaced.

Each record also keeps its deadline (`due_at`). `track` sets it, and
`Sla::reconcile` changes it. A claim must match the stored deadline. Thus a
replica with an old calendar, during a rolling deploy, cannot claim before
the deadline that a new replica stored. For example:

```sql
UPDATE sla_obligations SET escalated_at = $4
WHERE key = $1 AND generation = $2 AND escalated_at IS NULL
  AND (due_at IS NULL OR due_at = $3)
  AND (met_at IS NULL OR met_at > $3)
```

---

## Test SLA logic

Use `#[sim_test]`. `Sim::advance_to` moves the injected clock and the job
timers together, so a test crosses weekends and holidays with no real time.

```rust,ignore
use autumn_web::sim::Sim;
use autumn_web::sim_test;

#[sim_test]
async fn first_response_breaches_after_the_holiday(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![open_ticket]).plugin(sla_plugin()));

    sim.advance_to(&friday_3pm).await;
    sim.client().post("/tickets/1").send().await.assert_ok();

    sim.advance_to(&wednesday_3_01pm).await;
    sim.run_to_idle().await;
    // assert the breach handler ran one time
}
```

`autumn/tests/integration/sim_sla.rs` runs a full quarter of a support desk:
182 tickets, three holidays and a daylight-saving change. It checks each
deadline against a minute-by-minute oracle. It takes about 0.25 s of wall
time.

---

## Limits

- Calendars are set in code. There is no holiday import and no editor.
- If a deploy changes a calendar, call `Sla::reconcile` once from the new
  version (for example, from an `on_startup` hook). It stores each open
  record's new deadline and puts a check on the queue there. If the new
  calendar gives no deadline, the record never escalates. Without it, a
  record keeps the deadline that `track` stored.
- The clock stops only outside working time. A manual pause is not available.
- A window cannot cross midnight. Use two windows, such as `"22:00-24:00"` and
  `"00:00-06:00"`.
- `track` refuses a zero budget. Set it with `Obligation::within`.
- If a check job runs before its deadline on the app clock (the job queue
  clock leads the app clock), it puts itself on the queue again after the
  difference. This uses no retry attempt.
- The scan horizon is one hundred years. `track` refuses an obligation with no
  deadline in that time (`SlaError::NoDeadline`).

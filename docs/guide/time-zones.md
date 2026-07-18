# Per-user time zones

Autumn ships a `TimeZone` extractor that resolves the requesting user's IANA
time zone and makes it available in handlers as a first-class parameter. Pair
it with the `Clock` extractor for fully deterministic, test-injectable date/time
rendering — no globals, no `Utc::now()` calls in templates.

> **Status:** ships in `v0.5.x` always-on (no Cargo feature needed). `chrono`
> and `chrono-tz` are unconditional dependencies, so the extractor adds zero
> overhead to apps that don't use it.

---

## Quick start

### 1. (Optional) Configure the app default

```toml
# autumn.toml
[time_zone]
identifier = "America/New_York"   # IANA id; defaults to "UTC"
```

If the block is omitted, the default falls back to `UTC`.

### 2. Use `TimeZone` in a handler

```rust,ignore
use autumn_web::prelude::*;
use autumn_web::time_zone::local_datetime;

#[get("/events")]
async fn events_index(clock: Clock, tz: TimeZone) -> Markup {
    let now = clock.now();
    html! {
        p { "Current time: " (local_datetime(now, *tz)) }
    }
}
```

That's it. No TZ-conversion code inside the handler — `TimeZone` resolves the
zone for you and `local_datetime` renders a semantic `<time>` element.

### 3. Wire your auth middleware to set the user's zone

When a user has a stored `time_zone` preference, insert it into request
extensions so the resolver picks it up automatically:

```rust,ignore
// In your current-user middleware:
if let Some(tz_str) = &user.time_zone {
    if let Some(tz) = autumn_web::time_zone::parse_iana(tz_str) {
        parts.extensions.insert(UserTimeZone(tz));
    }
}
```

The `time_zone` column is generated automatically for you if you scaffold your
auth with `autumn generate auth` (see §User model below).

---

## Resolution order

The `TimeZone` extractor walks the request in this order, returning the first
valid IANA zone found:

1. **`UserTimeZone` extension** — inserted by your auth middleware for the
   authenticated user's stored preference (highest priority).
2. **Signed session** — `autumn_time_zone` key in the HMAC-signed session
   cookie, set via `autumn_web::time_zone::set_time_zone_in_session`.
3. **Plain cookie** — unsigned `autumn_time_zone=<iana>` cookie, set via
   `set_time_zone_cookie`. Useful when sessions are not enabled.
4. **`?tz=<iana>` query parameter** — developer/test override.

If none of the above yields a valid zone, the configured `default_locale`
(from `[time_zone]` in `autumn.toml`) is used. If no config is present,
`UTC` is the ultimate fallback.

The order is stable. Applications can rely on it.

### Implementing a zone switcher

```rust,ignore
use autumn_web::time_zone::set_time_zone_in_session;

#[post("/tz/{tz}")]
async fn switch_tz(session: Session, Path(tz): Path<String>) -> impl IntoResponse {
    set_time_zone_in_session(&session, &tz).await;
    Redirect::to("/")
}
```

---

## View helpers

All helpers live in `autumn_web::time_zone` and are re-exported from
`autumn_web::prelude` (gated on the `maud` feature).

### `local_datetime(dt, tz)`

Renders a `<time datetime="<rfc3339-utc>">YYYY-MM-DD HH:MM TZ</time>` element.
The `datetime` attribute is always UTC for machine readers.

```rust,ignore
(local_datetime(clock.now(), *tz))
```

### `local_date(dt, tz)`

Same but shows only the date (`YYYY-MM-DD`). Useful when the time-of-day is
irrelevant.

### `time_ago(dt, now, tz)`

Renders a relative string ("3 minutes ago", "in 2 hours"). Pass `clock.now()`
as `now` so tests can freeze it.

```rust,ignore
(time_ago(post.created_at, clock.now(), *tz))
```

---

## Form round-trip: `datetime-local` inputs

Browser `<input type="datetime-local">` values have no offset — they are
interpreted as local time in the user's zone. Autumn provides two helpers for a
lossless round-trip:

```rust,ignore
use autumn_web::time_zone::{parse_local_datetime, to_local_input_value};

// Incoming form POST: parse "2025-06-14T15:30" as Tokyo time → UTC
let utc: DateTime<Utc> = parse_local_datetime(&form.starts_at, Tz::Asia__Tokyo)?;

// Repopulating the form: UTC → Tokyo local string
let input_value = to_local_input_value(stored_utc, *tz);
```

Round-trip is lossless to minute granularity (the resolution of
`datetime-local`).

---

## User model: the `time_zone` column

`autumn generate auth` automatically adds a `time_zone TEXT NULL` column to the
generated users table, a `pub time_zone: Option<String>` field on the model,
and the corresponding Diesel schema entry. No extra flags needed.

```sql
-- Generated migration (excerpt)
CREATE TABLE users (
    id BIGSERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    time_zone TEXT NULL,        -- IANA id, e.g. "America/New_York"
    password_digest TEXT NOT NULL,
    ...
);
```

---

## Using in `#[mailer]` and `#[job]`

Capture the zone at enqueue time and re-establish it when the background
task renders:

```rust,ignore
use autumn_web::time_zone::{with_request_time_zone, ambient_time_zone};

// In your handler — capture the zone and pass it into the job:
job::send_report(user.id, tz.iana()).enqueue(&state).await?;

// In the job handler — re-enter the zone:
#[job]
async fn send_report(user_id: i64, tz_name: String, state: AppState) {
    let tz = autumn_web::time_zone::parse_iana(&tz_name).unwrap_or(Tz::UTC);
    with_request_time_zone(tz, async move {
        // Pass the captured zone to the view helpers explicitly; the ambient
        // zone set above is available via `ambient_time_zone()` if you'd rather
        // read it inside a deeply-nested renderer.
        let body = render_report(user_id, &state, tz).await;
        // ...
    }).await;
}
```

---

## Testing

Pin both clock and zone for fully deterministic tests:

```rust,ignore
use autumn_web::test::TestApp;
use autumn_web::time::FixedClock;
use chrono::{TimeZone as _, Utc};

#[tokio::test]
async fn report_shows_tokyo_time() {
    let pinned = Utc.with_ymd_and_hms(2025, 6, 14, 12, 0, 0).unwrap();
    let client = TestApp::new()
        .routes(routes![my_handler])
        .with_clock(FixedClock::at(pinned))
        .build();

    // Drive with ?tz= to override the zone without any DB user
    let body = client.get("/report?tz=Asia/Tokyo").send().await.text();
    assert!(body.contains("21:00")); // 12:00 UTC = 21:00 Tokyo
}
```

---

## Config fail-fast

An invalid `identifier` in `autumn.toml` causes the app to fail at startup
rather than at the first request:

```toml
[time_zone]
identifier = "Mars/Phobos"  # → ConfigError::Validation at load time
```

```text
error: time_zone identifier `Mars/Phobos` is not a valid IANA time zone
```

---

## Out of scope

- DST-aware scheduling ("fire at 9 AM local time every weekday"). Use a
  cron-with-tz library for that; the extractor only handles rendering.
- Right-to-left date ordering. Configure your locale's date format in the
  template layer.
- `datetime-local` inputs with seconds precision. The browser format is
  `HH:MM`; sub-minute offsets are dropped.
- Hot-reloading the `[time_zone]` config. Restart the app to pick up changes.

---

## See also

- [`autumn_web::time_zone`](https://docs.rs/autumn-web/latest/autumn_web/time_zone/index.html) — full API reference.
- [`autumn_web::time::Clock`](https://docs.rs/autumn-web/latest/autumn_web/time/struct.Clock.html) — injectable clock, compose with `TimeZone`.
- [docs/guide/i18n.md](./i18n.md) — locale-aware text rendering.
- [Project chrono-tz](https://docs.rs/chrono-tz) — the IANA database backing `parse_iana`.

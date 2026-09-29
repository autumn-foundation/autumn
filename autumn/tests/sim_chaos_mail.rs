//! W5.a (item 5) Definition-of-Done proof (sim-testing, issue #1797): the chaos
//! lane can install a **deterministic, explicit per-send SMTP transport fault
//! schedule** — "send #7 fails, #8 times out" — so a test can adversarially
//! exercise a throttled-resume / retry path against reproducible faults.
//!
//! This stacks on the W5.0 chaos scaffolding (the [`Chaos`] builder, the seeded
//! streams, and the [`ChaosEvent`] / [`ChaosHook`] log read through
//! `Sim::__chaos_events()`). It:
//!
//! 1. Builds a [`Sim`] whose [`Chaos`] carries
//!    `smtp_faults([(7, MailFault::Fail), (8, MailFault::Timeout)])` — the exact
//!    ratified example — mounted on a fresh `SQLite` substrate with a working
//!    [`Log`](autumn_web::mail::Transport::Log) mail transport.
//! 2. Drives real sends through the [`Mailer`](autumn_web::mail::Mailer)
//!    extractor (each request = one transport send, which the fault-injecting
//!    mail interceptor sits on) and asserts send #7 returns a `Fail`-shaped
//!    error, send #8 a `Timeout`-shaped error, and every other send succeeds.
//! 3. Asserts **two same-seed runs produce identical mail schedules** and that
//!    the fault falls on the *send sequence* — so a retry/resume that re-sends is
//!    deterministically exercisable (throttled-resume safety).
//! 4. Confirms a **default (no-mail-chaos) sim records zero `MailSend` events**.
//!
//! Needs no Docker: the substrate is in-memory `SQLite` and mail uses the `Log`
//! transport, so this is a fast, non-`#[ignore]`d test. It runs on the same
//! paused, current-thread runtime `#[sim_test]` uses (`start_paused = true`),
//! which the sim's [`advance`](autumn_web::sim::Sim::advance) requires.
//!
//! A **standalone** `[[test]]` binary (like `sim_chaos.rs`); it needs the `mail`
//! feature for the mail interceptor/transport surface, so it is
//! `#![cfg(all(feature = "sqlite", feature = "test-support", feature = "mail"))]`
//! — a default `cargo test` compiles it to an empty (passing) binary. Run it
//! with:
//! `cargo test -p autumn-web --features "sqlite,test-support,mail" --test sim_chaos_mail`.

#![cfg(all(feature = "sqlite", feature = "test-support", feature = "mail"))]

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::mail::{Mail, MailError, Mailer, Transport};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{Chaos, ChaosEvent, ChaosHook, MailFault, Sim};
use autumn_web::test::TestApp;

/// Reuse the substrate lane's migration set so the mounted app has a live schema
/// behind the `Db` extractor (mail itself needs no schema, but the sim substrate
/// wants a migrated pool).
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sim_sqlite_substrate");

/// One outbound send per request, through the `Mailer` extractor. The response
/// body encodes the outcome so the test can distinguish success / `Fail` /
/// `Timeout` without a side channel:
/// * `"ok"` — the real (`Log`) transport delivered it.
/// * `"fail"` — a scheduled [`MailFault::Fail`] (`RuntimeUnavailable`) fired.
/// * `"timeout"` — a scheduled [`MailFault::Timeout`] (`Io`/`TimedOut`) fired.
#[get("/send-mail")]
async fn send_mail(mailer: Mailer) -> String {
    let mail = Mail::builder()
        .to("recipient@example.com")
        .subject("chaos")
        .text("body")
        .build()
        .expect("mail builds");
    match mailer.send(mail).await {
        Ok(()) => "ok".to_owned(),
        Err(MailError::Io(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
            "timeout".to_owned()
        }
        Err(MailError::RuntimeUnavailable(_)) => "fail".to_owned(),
        Err(other) => format!("other:{other}"),
    }
}

/// Registers the `/send-mail` route on the sim app.
struct MailChaosPlugin;

impl Plugin for MailChaosPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![send_mail])
    }
}

/// The ratified schedule: send #7 fails, send #8 times out.
const FAIL_INDEX: usize = 7;
const TIMEOUT_INDEX: usize = 8;
/// Enough sends to drive the whole schedule plus a trailing success.
const SENDS: usize = 9;

/// A `Chaos` carrying exactly the ratified per-send fault schedule.
fn scheduled_chaos() -> Chaos {
    Chaos::default().smtp_faults([
        (FAIL_INDEX, MailFault::Fail),
        (TIMEOUT_INDEX, MailFault::Timeout),
    ])
}

/// A config with a working `Log` mail transport and a `from` address, so
/// `Mailer::send` reaches the (fault-injected) transport.
fn mail_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.mail.transport = Transport::Log;
    config.mail.from = Some("noreply@example.com".to_owned());
    config
}

/// Drive `SENDS` sequential sends against `seed` and return the per-send outcome
/// bodies plus the recorded chaos event schedule.
async fn run_mail_scenario(seed: u64) -> (Vec<String>, Vec<ChaosEvent>) {
    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");

    let mut sim = Sim::from_seed(seed);
    sim.chaos(scheduled_chaos());

    // A working mail transport: `Log` sends succeed unless the chaos interceptor
    // faults them first.
    let app = TestApp::new()
        .plugin(MailChaosPlugin)
        .config(mail_config())
        .with_db(substrate.pool());
    sim.build(app);

    let mut outcomes = Vec::with_capacity(SENDS);
    for _ in 0..SENDS {
        let body = sim.client().get("/send-mail").send().await.text();
        outcomes.push(body);
    }

    let events = sim.__chaos_events();
    (outcomes, events)
}

/// The W5.a `DoD`: the explicit per-send schedule faults exactly sends #7/#8 in the
/// ratified way, every other send succeeds, and the recorded schedule is
/// identical across two same-seed runs.
#[tokio::test(start_paused = true)]
async fn scheduled_smtp_faults_are_deterministic_by_send_index() {
    let (outcomes_a, events_a) = run_mail_scenario(0x5EED).await;
    let (outcomes_b, events_b) = run_mail_scenario(0x5EED).await;

    // Send #7 fails, #8 times out (1-based), everything else succeeds.
    for (i, outcome) in outcomes_a.iter().enumerate() {
        let send_index = i + 1;
        let expected = match send_index {
            FAIL_INDEX => "fail",
            TIMEOUT_INDEX => "timeout",
            _ => "ok",
        };
        assert_eq!(
            outcome, expected,
            "send #{send_index} outcome mismatch (got {outcome:?})"
        );
    }

    // Determinism: the explicit schedule replays byte-for-byte across two
    // same-seed runs — outcomes *and* the recorded chaos event log.
    assert_eq!(
        outcomes_a, outcomes_b,
        "same schedule must produce identical per-send outcomes"
    );
    assert_eq!(
        events_a, events_b,
        "same seed + schedule must replay the exact same MailSend log"
    );

    // Exactly one MailSend decision per send, and exactly two fired (the two
    // scheduled faults).
    let mail_events: Vec<&ChaosEvent> = events_a
        .iter()
        .filter(|e| e.hook == ChaosHook::MailSend)
        .collect();
    assert_eq!(
        mail_events.len(),
        SENDS,
        "one MailSend decision recorded per send"
    );
    let fired: Vec<u64> = mail_events
        .iter()
        .filter(|e| e.fired)
        .map(|e| e.seq)
        .collect();
    // `seq` is 0-based; the 1-based sends #7 and #8 are seq 6 and 7.
    assert_eq!(
        fired,
        vec![(FAIL_INDEX - 1) as u64, (TIMEOUT_INDEX - 1) as u64],
        "only the two scheduled sends fired a fault, at their send-sequence positions"
    );
}

/// Throttled-resume safety: because the fault is keyed on send *sequence*, a
/// resume that re-sends the failed-then-timed-out mails hits the **next** send
/// indices — deterministically, with no fault this time — so a retry path can be
/// exercised end-to-end without a flaky timing source. Here the two sends after
/// the scheduled window (a hypothetical retry of #7 and #8) both succeed.
#[tokio::test(start_paused = true)]
async fn resume_after_scheduled_window_is_deterministically_clean() {
    // A run long enough that the two sends *following* the scheduled window
    // model the resume of the two faulted mails.
    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");
    let mut sim = Sim::from_seed(0xA5A5);
    sim.chaos(scheduled_chaos());
    sim.build(
        TestApp::new()
            .plugin(MailChaosPlugin)
            .config(mail_config())
            .with_db(substrate.pool()),
    );

    // Sends #1..=8 exhaust the schedule (#7 fail, #8 timeout).
    let mut bodies = Vec::new();
    for _ in 0..TIMEOUT_INDEX {
        bodies.push(sim.client().get("/send-mail").send().await.text());
    }
    assert_eq!(bodies[FAIL_INDEX - 1], "fail");
    assert_eq!(bodies[TIMEOUT_INDEX - 1], "timeout");

    // The "resume": re-send the two faulted mails. These are sends #9 and #10,
    // which the schedule leaves clean — so the retry deterministically succeeds.
    let resume_fail = sim.client().get("/send-mail").send().await.text();
    let resume_timeout = sim.client().get("/send-mail").send().await.text();
    assert_eq!(
        resume_fail, "ok",
        "resuming the failed send hits a clean send index and succeeds"
    );
    assert_eq!(
        resume_timeout, "ok",
        "resuming the timed-out send hits a clean send index and succeeds"
    );
}

/// A default (empty) `Chaos` installs no mail interceptor: no `MailSend` events
/// are recorded and every send succeeds — the pre-W5.a behavior, unchanged.
#[tokio::test(start_paused = true)]
async fn default_sim_records_no_mail_send_events() {
    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");

    // No `sim.chaos(...)` call: default Chaos is inactive.
    let mut sim = Sim::from_seed(0x5EED);
    sim.build(
        TestApp::new()
            .plugin(MailChaosPlugin)
            .config(mail_config())
            .with_db(substrate.pool()),
    );

    for _ in 0..SENDS {
        assert_eq!(
            sim.client().get("/send-mail").send().await.text(),
            "ok",
            "no mail chaos: every send succeeds"
        );
    }

    assert!(
        sim.__chaos_events()
            .iter()
            .all(|e| e.hook != ChaosHook::MailSend),
        "an inactive chaos config records no MailSend events"
    );
    assert!(
        sim.__chaos_events().is_empty(),
        "an inactive chaos config records no events at all"
    );
}

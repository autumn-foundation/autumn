//! Issue #3062: with the outbox on, `deliver_later` survives a process
//! restart.
//!
//! `deliver_later` writes the mail to the outbox. The app dies before the
//! relay sends it. A new process on the same database sends it exactly once.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support,mail" --test sim_outbox_mail`.

#![cfg(all(feature = "sqlite", feature = "test-support", feature = "mail"))]

use autumn_web::config::{AutumnConfig, OutboxConfig};
use autumn_web::mail::{Mail, Mailer, Transport};
use autumn_web::outbox;
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::Sim;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::test::TestApp;

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<autumn_web::db::RuntimeConnection>;

#[post("/signup")]
async fn signup(mailer: Mailer) -> &'static str {
    let mail = Mail::builder()
        .to("new-user@example.com")
        .subject("Welcome")
        .text("Hello")
        .build()
        .expect("mail builds");
    mailer.deliver_later(mail);
    "ok"
}

fn app(pool: SqlitePool) -> TestApp {
    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.mail.transport = Transport::Log;
    config.mail.from = Some("noreply@example.com".to_owned());
    TestApp::new()
        .config(config)
        .with_db(pool)
        .with_outbox(OutboxConfig::default())
        .routes(routes![signup])
}

/// Outbox rows, and how many of them the relay sent.
async fn outbox_rows(pool: &SqlitePool) -> (i64, i64) {
    #[derive(diesel::QueryableByName)]
    struct Counts {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        total: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        sent: i64,
    }
    let mut conn = pool.get().await.expect("connection");
    let counts = diesel::sql_query(
        "SELECT COUNT(*) AS total, COUNT(dispatched_at) AS sent FROM autumn_outbox",
    )
    .get_result::<Counts>(&mut conn)
    .await
    .expect("count");
    (counts.total, counts.sent)
}

#[tokio::test(start_paused = true)]
async fn sim_deliver_later_survives_restart_with_outbox() {
    let substrate = SqliteSubstrate::new().expect("substrate");
    let pool = substrate.pool();
    outbox::ensure_schema(&pool).await.expect("outbox tables");

    let mut sim = Sim::from_seed(0x3062);
    sim.build(app(pool.clone()));
    sim.client().post("/signup").send().await.assert_ok();

    // `deliver_later` writes the row from a spawned task. The count waits for
    // the single substrate connection, so it sees the write once it is done.
    let mut rows = (0, 0);
    for _ in 0..100 {
        rows = outbox_rows(&pool).await;
        if rows.0 == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(rows, (1, 0), "the mail waits in the outbox");
    sim.client().assert_no_email_sent();

    // The process dies before the relay runs. A new one starts.
    sim.crash_and_restart(app(pool.clone()));
    sim.run_to_idle().await;

    sim.client()
        .assert_email_count(1)
        .assert_email_sent(|mail| mail.subject == "Welcome" && mail.to == ["new-user@example.com"]);
    assert_eq!(outbox_rows(&pool).await, (1, 1));

    // A second drain sends nothing more.
    sim.run_to_idle().await;
    sim.client().assert_email_count(1);
}

/// `Outbox::deliver_mail` writes on the transaction connection: a rollback
/// sends nothing, and a commit survives a restart.
#[tokio::test(start_paused = true)]
async fn sim_deliver_mail_in_tx_follows_the_commit_and_survives_restart() {
    use autumn_web::outbox::Outbox;
    use scoped_futures::ScopedFutureExt as _;

    let substrate = SqliteSubstrate::new().expect("substrate");
    let pool = substrate.pool();
    outbox::ensure_schema(&pool).await.expect("outbox tables");

    let mut sim = Sim::from_seed(0x3063);
    sim.build(app(pool.clone()));
    let state = sim.client().state().clone();
    for (subject, commit) in [("Rolled back", false), ("Committed", true)] {
        let outbox = Outbox::new(&state);
        let mail = Mail::builder()
            .to("new-user@example.com")
            .subject(subject)
            .text("Hello")
            .build()
            .expect("mail builds");
        let mut conn = pool.get().await.expect("connection");
        let result: Result<(), AutumnError> =
            autumn_web::db::scoped_transaction(&mut *conn, |conn| {
                async move {
                    outbox.deliver_mail(conn, mail).await?;
                    if commit {
                        Ok(())
                    } else {
                        Err(AutumnError::bad_request_msg("roll back"))
                    }
                }
                .scope_boxed()
            })
            .await;
        assert_eq!(result.is_ok(), commit);
    }
    drop(state);
    assert_eq!(outbox_rows(&pool).await, (1, 0));

    sim.crash_and_restart(app(pool.clone()));
    sim.run_to_idle().await;
    sim.client()
        .assert_email_count(1)
        .assert_email_sent(|mail| {
            mail.subject == "Committed" && mail.from.as_deref() == Some("noreply@example.com")
        });
}

//! End-to-end coverage for RFC 8058 one-click List-Unsubscribe (issue #838):
//! declare a list mailer, send, observe the signed link + headers, click
//! unsubscribe, observe suppression, re-send, confirm the recipient is skipped.
#![cfg(feature = "mail")]

use autumn_web::config::AutumnConfig;
use autumn_web::mail::{InMemorySuppressionStore, Mail, Mailer};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

pub struct WeeklyDigestMailer;

#[mailer(list_unsubscribe = "weekly_digest")]
impl WeeklyDigestMailer {
    #[allow(clippy::unused_self)]
    fn digest(&self, to: String) -> Mail {
        Mail::builder()
            .to(to)
            .subject("Weekly Digest")
            .html("<p>This week in Autumn</p>")
            .text("This week in Autumn")
            .build()
            .expect("valid mail")
    }
}

#[mailer_preview]
impl WeeklyDigestMailer {
    fn digest_preview() -> Mail {
        Mail::builder()
            .to("preview@example.com")
            .subject("Weekly Digest")
            .html("<p>This week in Autumn</p>")
            .text("This week in Autumn")
            .build()
            .expect("valid preview mail")
    }
}

#[get("/send")]
async fn send_digest(mailer: Mailer) -> AutumnResult<&'static str> {
    WeeklyDigestMailer
        .send_digest(&mailer, "ada@example.com".to_owned())
        .await?;
    Ok("sent")
}

/// Calls the template method directly (not the generated `send_*` helper) and
/// sends the returned `Mail`. The macro tags the template method body too, so
/// this path must still emit RFC 8058 headers and apply suppression.
#[get("/send-direct")]
async fn send_digest_direct(mailer: Mailer) -> AutumnResult<&'static str> {
    let mail = WeeklyDigestMailer.digest("ada@example.com".to_owned());
    mailer
        .send(mail)
        .await
        .map_err(AutumnError::internal_server_error)?;
    Ok("sent")
}

fn dev_mail_config(dir: &std::path::Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("dev".to_owned()),
        ..AutumnConfig::default()
    };
    config.mail.transport = autumn_web::mail::Transport::File;
    config.mail.file_dir = dir.to_path_buf();
    config.mail.from = Some("Autumn <noreply@example.com>".to_owned());
    config.mail.unsubscribe_base_url = Some("https://app.example.com".to_owned());
    config.mail.unsubscribe_mailto = Some("unsub@example.com".to_owned());
    config
}

fn read_single_eml(dir: &std::path::Path) -> String {
    let mut emls: Vec<_> = std::fs::read_dir(dir)
        .expect("mail dir exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("eml"))
        .collect();
    assert_eq!(emls.len(), 1, "expected exactly one captured message");
    std::fs::read_to_string(emls.remove(0)).expect("read eml")
}

fn count_emls(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |rd| {
        rd.filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("eml"))
            .count()
    })
}

/// Extract the `token` query value from a `List-Unsubscribe:` header line.
fn token_from_eml(eml: &str) -> String {
    let line = eml
        .lines()
        .find(|l| l.starts_with("List-Unsubscribe:"))
        .expect("List-Unsubscribe header present");
    let start = line.find("token=").expect("token in unsubscribe URL") + "token=".len();
    let rest = &line[start..];
    let end = rest.find('>').unwrap_or(rest.len());
    rest[..end].to_owned()
}

#[tokio::test]
async fn newsletter_unsubscribe_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    // 1. Send → message carries both RFC 8058 headers and a signed link.
    client.get("/send").send().await.assert_ok();
    let eml = read_single_eml(dir.path());
    assert!(
        eml.contains("List-Unsubscribe: <https://app.example.com/_autumn/unsubscribe?token="),
        "missing one-click List-Unsubscribe header:\n{eml}"
    );
    assert!(
        eml.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"),
        "missing List-Unsubscribe-Post header:\n{eml}"
    );

    // 2. Click unsubscribe (RFC 8058 one-click POST).
    let token = token_from_eml(&eml);
    client
        .post(&format!("/_autumn/unsubscribe?token={token}"))
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .assert_ok();

    // 3. Re-send → recipient is suppressed, no new message is written.
    client.get("/send").send().await.assert_ok();
    assert_eq!(
        count_emls(dir.path()),
        1,
        "suppressed recipient must not receive a second message"
    );
}

#[tokio::test]
async fn unsubscribe_get_renders_confirmation_form() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    client.get("/send").send().await.assert_ok();
    let token = token_from_eml(&read_single_eml(dir.path()));

    client
        .get(&format!("/_autumn/unsubscribe?token={token}"))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Unsubscribe")
        .assert_body_contains("method=\"post\"");
}

#[tokio::test]
async fn unsubscribe_post_rejects_invalid_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    client
        .post("/_autumn/unsubscribe?token=not-a-valid-token")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn endpoint_not_mounted_without_opt_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Same config, but no `.mount_unsubscribe_endpoint()` — a JSON-only app.
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    client
        .post("/_autumn/unsubscribe?token=anything")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .assert_status(404);
}

/// Same flow as [`newsletter_unsubscribe_end_to_end`] but against the auto-wired
/// Diesel `DbSuppressionStore` and the CLI-generated `mail_unsubscribes` table.
#[cfg(feature = "db")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn newsletter_unsubscribe_end_to_end_db_backed() {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    // Apply the same schema the CLI migration generates.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS mail_unsubscribes (\
                id BIGSERIAL PRIMARY KEY, subscriber TEXT NOT NULL, list_id TEXT NOT NULL, \
                unsubscribed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
                UNIQUE (subscriber, list_id))",
        )
        .execute(&mut conn)
        .await
        .expect("create table");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_db(pool)
        .routes(routes![send_digest])
        .build();

    client.get("/send").send().await.assert_ok();
    let token = token_from_eml(&read_single_eml(dir.path()));
    client
        .post(&format!("/_autumn/unsubscribe?token={token}"))
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .assert_ok();
    client.get("/send").send().await.assert_ok();
    assert_eq!(
        count_emls(dir.path()),
        1,
        "DB-suppressed recipient must not receive a second message"
    );
}

/// A direct call to the template method (then `Mailer::send`) must still emit the
/// RFC 8058 headers — the macro tags the template method body, not only helpers.
#[tokio::test]
async fn direct_template_call_still_emits_unsubscribe_headers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest_direct])
        .build();

    client.get("/send-direct").send().await.assert_ok();
    let eml = read_single_eml(dir.path());
    assert!(
        eml.contains("List-Unsubscribe: <https://app.example.com/_autumn/unsubscribe?token="),
        "direct template call must still carry the one-click header:\n{eml}"
    );
    assert!(
        eml.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"),
        "direct template call must still carry the post header:\n{eml}"
    );
}

/// POST with an invalid body (not `List-Unsubscribe=One-Click`) must return 400.
#[tokio::test]
async fn unsubscribe_post_rejects_bad_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    // Obtain a legitimately signed token.
    client.get("/send").send().await.assert_ok();
    let token = token_from_eml(&read_single_eml(dir.path()));

    // Body does not match the RFC 8058 one-click requirement.
    client
        .post(&format!("/_autumn/unsubscribe?token={token}"))
        .body("some-other-body")
        .send()
        .await
        .assert_status(400);
}

/// POST with a valid token + correct body but no suppression backend configured
/// must return 503 — the framework must never confirm an unsubscribe it cannot
/// actually honor.
#[tokio::test]
async fn unsubscribe_post_without_suppression_store_returns_503() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Opt in to the endpoint but deliberately omit with_suppression_store().
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .routes(routes![send_digest])
        .build();

    // Mail is still delivered (suppression check is skipped when no store).
    client.get("/send").send().await.assert_ok();
    let token = token_from_eml(&read_single_eml(dir.path()));

    client
        .post(&format!("/_autumn/unsubscribe?token={token}"))
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .assert_status(503);
}

/// GET with a bad token must return 400 with an error HTML body.
#[tokio::test]
async fn unsubscribe_get_invalid_token_returns_400() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .mount_unsubscribe_endpoint()
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    client
        .get("/_autumn/unsubscribe?token=not-a-valid-token")
        .send()
        .await
        .assert_status(400);
}

/// Send with mailto-only config: the `List-Unsubscribe` header appears but
/// `List-Unsubscribe-Post` (one-click) must NOT be emitted — RFC 2369 only.
#[tokio::test]
async fn send_list_mail_mailto_only_no_one_click_post_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = dev_mail_config(dir.path());
    config.mail.unsubscribe_base_url = None; // mailto only, no HTTPS endpoint
    let client = TestApp::new()
        .config(config)
        .with_suppression_store(InMemorySuppressionStore::new())
        .routes(routes![send_digest])
        .build();

    client.get("/send").send().await.assert_ok();
    let eml = read_single_eml(dir.path());
    assert!(
        eml.contains("List-Unsubscribe:"),
        "List-Unsubscribe header should be present for mailto-only config:\n{eml}"
    );
    assert!(
        !eml.contains("List-Unsubscribe-Post:"),
        "List-Unsubscribe-Post must NOT be emitted without an HTTPS one-click URL:\n{eml}"
    );
}

#[tokio::test]
async fn dev_preview_shows_unsubscribe_headers_and_link() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(dev_mail_config(dir.path()))
        .state_initializer(|state| {
            state.insert_extension(autumn_web::mail::MailPreviewRegistry::new(mail_previews![
                WeeklyDigestMailer
            ]));
        })
        .build();

    client
        .get("/_autumn/mail/previews/WeeklyDigestMailer/digest_preview")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("List-Unsubscribe")
        .assert_body_contains("/_autumn/unsubscribe?token=");
}

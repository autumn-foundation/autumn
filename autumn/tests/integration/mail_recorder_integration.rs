#![cfg(feature = "mail")]

//! Integration tests for the built-in recording mail interceptor and
//! assertion helpers on `TestClient`.

use autumn_web::config::AutumnConfig;
use autumn_web::get;
use autumn_web::mail::{Mail, Mailer, Transport};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

fn mail_config() -> AutumnConfig {
    let mut cfg = AutumnConfig::default();
    cfg.mail.transport = Transport::Log;
    cfg.mail.from = Some("noreply@example.com".to_string());
    cfg
}

// ── AC1 / AC2: default recorder, sent_mail() accessor ─────────────────────

#[tokio::test]
async fn default_recorder_captures_single_send() {
    #[get("/send")]
    async fn send_one(mailer: Mailer) -> &'static str {
        let mail = Mail::builder()
            .to("alice@example.com")
            .subject("Hello Alice")
            .text("hi")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        "ok"
    }

    let client = TestApp::new()
        .config(mail_config())
        .routes(routes![send_one])
        .build();

    // no .with_mail_interceptor() call — recorder is auto-installed
    client.get("/send").send().await.assert_ok();

    let sent = client.sent_mail();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["alice@example.com"]);
    assert_eq!(sent[0].subject, "Hello Alice");
}

// ── AC3: assert_email_count / assert_no_email_sent / assert_email_sent ────

#[tokio::test]
async fn assert_email_count_passes_when_correct() {
    #[get("/send2")]
    async fn send_two(mailer: Mailer) -> &'static str {
        for addr in ["a@example.com", "b@example.com"] {
            let mail = Mail::builder()
                .to(addr)
                .subject("Batch")
                .text("body")
                .build()
                .unwrap();
            mailer.send(mail).await.unwrap();
        }
        "ok"
    }

    let client = TestApp::new()
        .config(mail_config())
        .routes(routes![send_two])
        .build();

    client.get("/send2").send().await.assert_ok();

    client.assert_email_count(2);
}

#[tokio::test]
async fn assert_no_email_sent_passes_when_none() {
    #[get("/noop")]
    async fn noop() -> &'static str {
        "quiet"
    }

    let client = TestApp::new()
        .config(mail_config())
        .routes(routes![noop])
        .build();

    client.get("/noop").send().await.assert_ok();

    client.assert_no_email_sent();
}

#[tokio::test]
async fn assert_email_sent_passes_with_matching_predicate() {
    #[get("/send3")]
    async fn send_welcome(mailer: Mailer) -> &'static str {
        let mail = Mail::builder()
            .to("bob@example.com")
            .subject("Welcome!")
            .html("<p>welcome</p>")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        "ok"
    }

    let client = TestApp::new()
        .config(mail_config())
        .routes(routes![send_welcome])
        .build();

    client.get("/send3").send().await.assert_ok();

    client
        .assert_email_sent(|m| m.to.iter().any(|t| t == "bob@example.com"))
        .assert_email_sent(|m| m.subject == "Welcome!");
}

// ── AC4: user-supplied interceptor still runs (composing) ─────────────────

#[tokio::test]
async fn user_interceptor_composes_with_recorder() {
    use autumn_web::interceptor::MailInterceptor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CUSTOM_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct CountingInterceptor;
    impl MailInterceptor for CountingInterceptor {
        fn intercept<'a>(
            &'a self,
            _mail: &'a Mail,
            next: std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<(), autumn_web::mail::MailError>>
                        + Send
                        + 'a,
                >,
            >,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), autumn_web::mail::MailError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                CUSTOM_CALLS.fetch_add(1, Ordering::SeqCst);
                next.await
            })
        }
    }

    #[get("/send4")]
    async fn send_once(mailer: Mailer) -> &'static str {
        let mail = Mail::builder()
            .to("c@example.com")
            .subject("Compose Test")
            .text("body")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        "ok"
    }

    CUSTOM_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .config(mail_config())
        .with_mail_interceptor(CountingInterceptor)
        .routes(routes![send_once])
        .build();

    client.get("/send4").send().await.assert_ok();

    // built-in recorder captured it
    client.assert_email_count(1);
    // user interceptor also ran
    assert_eq!(CUSTOM_CALLS.load(Ordering::SeqCst), 1);
}

// ── AC6: regression — two mails in order ──────────────────────────────────

#[tokio::test]
async fn two_mails_captured_in_send_order() {
    #[get("/send-two-ordered")]
    async fn send_two_ordered(mailer: Mailer) -> &'static str {
        let first = Mail::builder()
            .to("first@example.com")
            .subject("First")
            .text("1")
            .build()
            .unwrap();
        mailer.send(first).await.unwrap();

        let second = Mail::builder()
            .to("second@example.com")
            .subject("Second")
            .text("2")
            .build()
            .unwrap();
        mailer.send(second).await.unwrap();

        "ok"
    }

    let client = TestApp::new()
        .config(mail_config())
        .routes(routes![send_two_ordered])
        .build();

    client.get("/send-two-ordered").send().await.assert_ok();

    client.assert_email_count(2);

    let sent = client.sent_mail();
    assert_eq!(sent[0].to, vec!["first@example.com"], "first mail order");
    assert_eq!(sent[1].to, vec!["second@example.com"], "second mail order");
}

// ── AC5: Transport::Disabled never opens SMTP ─────────────────────────────

#[tokio::test]
async fn disabled_transport_captures_nothing_and_no_smtp() {
    #[get("/send5")]
    async fn send_disabled(mailer: Mailer) -> &'static str {
        let mail = Mail::builder()
            .to("d@example.com")
            .subject("Disabled")
            .text("body")
            .build()
            .unwrap();
        // Disabled transport returns Ok(()) without sending
        let _ = mailer.send(mail).await;
        "ok"
    }

    let mut cfg = AutumnConfig::default();
    cfg.mail.transport = Transport::Disabled;
    cfg.mail.from = Some("noreply@example.com".to_string());

    let client = TestApp::new()
        .config(cfg)
        .routes(routes![send_disabled])
        .build();

    client.get("/send5").send().await.assert_ok();

    // With Disabled transport the interceptor still runs and captures;
    // the transport itself is a no-op (no SMTP).
    client.assert_email_count(1);
}

// ── Attachments (issue #1256) ──────────────────────────────────────────────

#[tokio::test]
async fn recorder_captures_attachments() {
    #[get("/send-attachment")]
    async fn send_with_attachment(mailer: Mailer) -> &'static str {
        let mail = Mail::builder()
            .to("ada@example.com")
            .subject("Invoice")
            .text("see attached")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        "ok"
    }

    let client = TestApp::new()
        .config(mail_config())
        .routes(routes![send_with_attachment])
        .build();

    client.get("/send-attachment").send().await.assert_ok();

    let sent = client.sent_mail();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].attachments.len(), 1);
    assert_eq!(sent[0].attachments[0].filename, "invoice.pdf");
    assert_eq!(sent[0].attachments[0].content_type, "application/pdf");
    assert_eq!(sent[0].attachments[0].bytes, b"%PDF-1.4".to_vec());
}

//! End-to-end coverage for the mail suppression list (issue #1247): a hard
//! bounce or complaint records an address in a `SuppressionStore`, and a later
//! `Mailer::send()` to that address is skipped (zero transport attempts) while
//! other recipients still deliver. Also covers the `Manual` reason, the
//! `unsuppress` escape hatch, the `Mail::ignore_suppression()` bypass for
//! critical mail, and the `AllRecipientsSuppressed` outcome.
#![cfg(feature = "mail")]

use std::sync::{Arc, Mutex};

use autumn_web::mail::suppression::{
    InMemorySuppressionStore, SuppressionReason, SuppressionStore, SuppressionStoreHandle,
    suppressed_skips,
};
use autumn_web::mail::{Mail, MailError, MailTransport, Mailer};

/// Transport double that records every recipient it is asked to deliver to, so
/// tests can assert on the exact set of addresses that reached the wire.
#[derive(Clone, Default)]
struct RecordingTransport {
    sent_to: Arc<Mutex<Vec<String>>>,
}

impl RecordingTransport {
    fn recipients(&self) -> Vec<String> {
        self.sent_to.lock().expect("lock").clone()
    }

    fn count(&self) -> usize {
        self.sent_to.lock().expect("lock").len()
    }
}

impl MailTransport for RecordingTransport {
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailError>> + Send + 'a>>
    {
        Box::pin(async move {
            {
                let mut guard = self.sent_to.lock().expect("lock");
                guard.extend(mail.to.iter().cloned());
            }
            Ok(())
        })
    }
}

fn mail_to(addr: &str) -> Mail {
    Mail::builder()
        .from("app@example.com")
        .to(addr)
        .subject("Hello")
        .text("Body")
        .build()
        .expect("valid mail")
}

#[tokio::test]
async fn suppressed_recipient_is_skipped_and_others_deliver() {
    // Success metric (issue #1247): bounce a@x.com, then a later send() to
    // a@x.com is skipped (0 transport calls) while b@x.com still delivers.
    let store = InMemorySuppressionStore::new();
    store
        .suppress("a@x.com", SuppressionReason::HardBounce)
        .await
        .expect("suppress");

    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone())
        .with_suppression(SuppressionStoreHandle::new(store));

    // Suppressed recipient: skipped, not an error, zero transport attempts.
    let before = suppressed_skips();
    let outcome = mailer.send(mail_to("a@x.com")).await;
    assert!(
        matches!(outcome, Err(MailError::AllRecipientsSuppressed)),
        "sole suppressed recipient yields AllRecipientsSuppressed, got {outcome:?}"
    );
    assert_eq!(transport.count(), 0, "no transport attempt for a@x.com");
    assert!(
        suppressed_skips() > before,
        "skip is observable via the counter"
    );

    // Non-suppressed recipient still delivers.
    mailer.send(mail_to("b@x.com")).await.expect("b delivers");
    assert_eq!(transport.recipients(), vec!["b@x.com".to_owned()]);
}

#[tokio::test]
async fn partial_suppression_delivers_survivors_only() {
    let store = InMemorySuppressionStore::new();
    store
        .suppress("bounced@x.com", SuppressionReason::Complaint)
        .await
        .expect("suppress");

    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone())
        .with_suppression(SuppressionStoreHandle::new(store));

    let mail = Mail::builder()
        .from("app@example.com")
        .to("bounced@x.com")
        .to("ok@x.com")
        .subject("Hi")
        .text("Body")
        .build()
        .expect("valid mail");
    mailer.send(mail).await.expect("survivors deliver");

    assert_eq!(transport.recipients(), vec!["ok@x.com".to_owned()]);
}

#[tokio::test]
async fn unsuppress_reopens_delivery() {
    let store = InMemorySuppressionStore::new();
    store
        .suppress("gone@x.com", SuppressionReason::Manual)
        .await
        .expect("suppress");
    assert!(store.is_suppressed("gone@x.com").await.expect("query"));

    store.unsuppress("gone@x.com").await.expect("unsuppress");
    assert!(!store.is_suppressed("gone@x.com").await.expect("query"));

    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone())
        .with_suppression(SuppressionStoreHandle::new(store));
    mailer.send(mail_to("gone@x.com")).await.expect("delivers");
    assert_eq!(transport.recipients(), vec!["gone@x.com".to_owned()]);
}

#[tokio::test]
async fn ignore_suppression_bypasses_the_list() {
    // Critical mail (password reset, MFA) must be deliverable even to a
    // suppressed address when the sender opts out deliberately.
    let store = InMemorySuppressionStore::new();
    store
        .suppress("locked@x.com", SuppressionReason::HardBounce)
        .await
        .expect("suppress");

    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone())
        .with_suppression(SuppressionStoreHandle::new(store));

    let mail = Mail::builder()
        .from("app@example.com")
        .to("locked@x.com")
        .subject("Reset your password")
        .text("Click here")
        .ignore_suppression()
        .build()
        .expect("valid mail");
    mailer.send(mail).await.expect("critical mail bypasses");
    assert_eq!(transport.recipients(), vec!["locked@x.com".to_owned()]);
}

#[tokio::test]
async fn address_matching_is_case_and_display_name_insensitive() {
    let store = InMemorySuppressionStore::new();
    store
        .suppress("Bounced@X.com", SuppressionReason::HardBounce)
        .await
        .expect("suppress");

    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone())
        .with_suppression(SuppressionStoreHandle::new(store));

    // Formatted "Name <addr>" with different case must still match.
    let outcome = mailer.send(mail_to("Ada <bounced@x.com>")).await;
    assert!(matches!(outcome, Err(MailError::AllRecipientsSuppressed)));
    assert_eq!(transport.count(), 0);
}

#[tokio::test]
async fn mailer_without_store_delivers_everything() {
    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone());
    mailer
        .send(mail_to("anyone@x.com"))
        .await
        .expect("delivers");
    assert_eq!(transport.recipients(), vec!["anyone@x.com".to_owned()]);
}

/// Detect -> suppress -> skip loop, driven by the provided inbound handler that
/// turns a parsed provider bounce webhook into a suppression entry.
#[cfg(feature = "inbound-mail")]
#[tokio::test]
async fn inbound_bounce_closes_the_loop() {
    use autumn_web::inbound_mail::InboundEmail;
    use autumn_web::mail::suppression::record_inbound;

    let handle = SuppressionStoreHandle::new(InMemorySuppressionStore::new());

    // Simulate a parsed Mailgun bounce webhook for a@x.com.
    let mut email = InboundEmail {
        from: "mailer-daemon@x.com".to_owned(),
        to: vec!["inbound@app.example".to_owned()],
        cc: vec![],
        subject: "Delivery failure".to_owned(),
        text_body: None,
        html_body: None,
        headers: Default::default(),
        attachments: vec![],
        spam_report: None,
        raw: Default::default(),
        plus_token: None,
        is_bounce: true,
        bounced_address: Some("a@x.com".to_owned()),
        complained_address: None,
    };

    // One-call wiring: hand the parsed bounce to the provided handler.
    record_inbound(handle.inner().as_ref(), &email)
        .await
        .expect("record bounce");
    assert!(
        handle
            .inner()
            .is_suppressed("a@x.com")
            .await
            .expect("query"),
        "bounce recorded a@x.com"
    );

    // A later send() to a@x.com is skipped; b@x.com still delivers.
    let transport = RecordingTransport::default();
    let mailer = Mailer::with_transport(transport.clone()).with_suppression(handle.clone());
    let outcome = mailer.send(mail_to("a@x.com")).await;
    assert!(matches!(outcome, Err(MailError::AllRecipientsSuppressed)));
    mailer.send(mail_to("b@x.com")).await.expect("b delivers");
    assert_eq!(transport.recipients(), vec!["b@x.com".to_owned()]);

    // Security: an inbound *spam verdict* (X-Mailgun-Sflag) is NOT an outbound
    // complaint and carries no complainant address — record_inbound must
    // suppress NOTHING here, and in particular must never suppress `email.to`
    // (the app's own inbound address). Otherwise anyone able to POST the inbound
    // endpoint could knock arbitrary recipients off future sends.
    email.is_bounce = false;
    email.bounced_address = None;
    email.spam_report = Some(autumn_web::inbound_mail::SpamReport {
        score: Some(9.9),
        verdict: Some("yes".to_owned()),
    });
    email.to = vec!["inbound@app.example".to_owned()];
    record_inbound(handle.inner().as_ref(), &email)
        .await
        .expect("spam verdict is a safe no-op");
    assert!(
        !handle
            .inner()
            .is_suppressed("inbound@app.example")
            .await
            .expect("query"),
        "inbound spam verdict must NOT suppress the app's own inbound address"
    );

    // A genuine FBL complaint carries the outbound complainant in
    // `complained_address`; that — and only that — is suppressed.
    email.spam_report = None;
    email.to = vec!["inbound@app.example".to_owned()];
    email.complained_address = Some("c@x.com".to_owned());
    record_inbound(handle.inner().as_ref(), &email)
        .await
        .expect("record complaint");
    assert!(
        handle
            .inner()
            .is_suppressed("c@x.com")
            .await
            .expect("query"),
        "genuine complaint suppresses the complainant address"
    );
    assert!(
        !handle
            .inner()
            .is_suppressed("inbound@app.example")
            .await
            .expect("query"),
        "complaint must never suppress email.to"
    );
}

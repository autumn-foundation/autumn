//! End-to-end coverage for CSS inlining of HTML email (issue #1254): a
//! `<style>` block is transformed into element `style="…"` attributes on the
//! delivered message, opt-in works both on the builder and via `MailConfig`,
//! per-message overrides beat the config default in both directions, and text
//! bodies / no-`<style>` bodies are left untouched.
#![cfg(feature = "mail")]
// CSS rule braces like `.btn{color:#fff}` in these HTML literals look like
// format placeholders to this lint, but they are literal test fixtures.
#![allow(clippy::literal_string_with_formatting_args)]

use std::sync::{Arc, Mutex};

use autumn_web::mail::{Mail, MailConfig, MailError, MailTransport, Mailer, Transport};

/// A `<style>` block plus a class-styled anchor — the canonical inlining input.
const STYLED_HTML: &str = r#"<style>.btn{color:#fff;background:#06c}</style><a class="btn">Go</a>"#;

/// Transport double that captures the most recently delivered [`Mail`] so tests
/// can inspect the exact body that reached the wire (after inlining).
#[derive(Clone, Default)]
struct CapturingTransport {
    last: Arc<Mutex<Option<Mail>>>,
}

impl CapturingTransport {
    fn last_html(&self) -> String {
        self.last
            .lock()
            .expect("lock")
            .as_ref()
            .expect("a mail was delivered")
            .html
            .clone()
            .expect("delivered mail has an html body")
    }

    fn last_text(&self) -> Option<String> {
        self.last
            .lock()
            .expect("lock")
            .as_ref()
            .expect("a mail was delivered")
            .text
            .clone()
    }
}

impl MailTransport for CapturingTransport {
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailError>> + Send + 'a>>
    {
        Box::pin(async move {
            *self.last.lock().expect("lock") = Some(mail);
            Ok(())
        })
    }
}

fn styled_mail() -> Mail {
    Mail::builder()
        .from("app@example.com")
        .to("user@example.com")
        .subject("Hi")
        .html(STYLED_HTML)
        .build()
        .expect("valid mail")
}

/// The anchor gained an inline `style` carrying the class rule, and the inlined
/// selector was stripped from the retained `<style>` block.
fn assert_inlined(html: &str) {
    assert!(
        html.contains("style=") && html.contains("#fff"),
        "expected the anchor to carry an inline style with the class rule; got: {html}"
    );
    assert!(
        !html.contains(".btn{color") && !html.contains(".btn {"),
        "expected the inlined `.btn` selector to be stripped from the <style> block; got: {html}"
    );
}

/// The body was delivered verbatim: the class rule is still only in the
/// `<style>` block and the anchor carries no inline style.
fn assert_not_inlined(html: &str) {
    assert!(
        html.contains(".btn{color:#fff;background:#06c}"),
        "expected the raw <style> rule to be delivered unchanged; got: {html}"
    );
    assert!(
        html.contains(r#"<a class="btn">Go</a>"#),
        "expected the anchor to be delivered without an inline style; got: {html}"
    );
}

// ── AC1 + AC2 (builder): explicit opt-in inlines at send ──────────────────────

#[tokio::test]
async fn builder_opt_in_inlines_html_at_send() {
    let transport = CapturingTransport::default();
    let mailer = Mailer::with_transport(transport.clone());

    let mail = Mail::builder()
        .from("app@example.com")
        .to("user@example.com")
        .subject("Hi")
        .html(STYLED_HTML)
        .inline_css(true)
        .build()
        .expect("valid mail");
    mailer.send(mail).await.expect("send succeeds");

    assert_inlined(&transport.last_html());
}

// ── Default off: unaffected unless opted in ───────────────────────────────────

#[tokio::test]
async fn default_off_leaves_html_untouched() {
    let transport = CapturingTransport::default();
    let mailer = Mailer::with_transport(transport.clone());

    mailer.send(styled_mail()).await.expect("send succeeds");

    assert_not_inlined(&transport.last_html());
}

// ── AC3: text bodies are never inlined ────────────────────────────────────────

#[tokio::test]
async fn text_body_is_never_inlined() {
    let transport = CapturingTransport::default();
    let mailer = Mailer::with_transport(transport.clone());

    // A text part that *contains* a `<style>`-looking string must pass through
    // untouched even with inlining forced on.
    let mail = Mail::builder()
        .from("app@example.com")
        .to("user@example.com")
        .subject("Hi")
        .text("<style>.btn{color:#fff}</style> literal text")
        .inline_css(true)
        .build()
        .expect("valid mail");
    mailer.send(mail).await.expect("send succeeds");

    assert_eq!(
        transport.last_text().as_deref(),
        Some("<style>.btn{color:#fff}</style> literal text"),
        "text bodies must never be inlined"
    );
}

// ── AC2 (config) + AC "delivered message": config default inlines via MIME ────

#[tokio::test]
async fn config_default_on_inlines_delivered_mime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = MailConfig {
        transport: Transport::File,
        file_dir: dir.path().to_path_buf(),
        inline_css: true,
        ..MailConfig::default()
    };
    let mailer = Mailer::from_config(&config).expect("mailer from config");

    mailer.send(styled_mail()).await.expect("send succeeds");

    let eml = read_only_eml(dir.path());
    assert!(
        eml.contains("text/html"),
        "the delivered MIME must contain the html part; got: {eml}"
    );
    assert_inlined(&eml);
}

// ── AC2: per-message override beats the config default, both directions ───────

#[tokio::test]
async fn per_message_opt_out_overrides_config_default_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = MailConfig {
        transport: Transport::File,
        file_dir: dir.path().to_path_buf(),
        inline_css: true, // environment defaults inlining ON
        ..MailConfig::default()
    };
    let mailer = Mailer::from_config(&config).expect("mailer from config");

    // This single message opts OUT.
    let mail = Mail::builder()
        .from("app@example.com")
        .to("user@example.com")
        .subject("Hi")
        .html(STYLED_HTML)
        .inline_css(false)
        .build()
        .expect("valid mail");
    mailer.send(mail).await.expect("send succeeds");

    assert_not_inlined(&read_only_eml(dir.path()));
}

#[tokio::test]
async fn per_message_opt_in_overrides_config_default_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = MailConfig {
        transport: Transport::File,
        file_dir: dir.path().to_path_buf(),
        // Environment default OFF (MailConfig::inline_css defaults to false).
        ..MailConfig::default()
    };
    assert!(
        !config.inline_css,
        "config default must be off for this test"
    );
    let mailer = Mailer::from_config(&config).expect("mailer from config");

    // This single message opts IN.
    let mail = Mail::builder()
        .from("app@example.com")
        .to("user@example.com")
        .subject("Hi")
        .html(STYLED_HTML)
        .inline_css(true)
        .build()
        .expect("valid mail");
    mailer.send(mail).await.expect("send succeeds");

    assert_inlined(&read_only_eml(dir.path()));
}

/// Read the single `.eml` file the file transport wrote into `dir`.
fn read_only_eml(dir: &std::path::Path) -> String {
    let entry = std::fs::read_dir(dir)
        .expect("read mail dir")
        .filter_map(Result::ok)
        .find(|e| e.path().extension().is_some_and(|ext| ext == "eml"))
        .expect("exactly one .eml was written");
    std::fs::read_to_string(entry.path()).expect("read .eml")
}

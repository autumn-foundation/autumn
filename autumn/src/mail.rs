//! Transactional email support.
//!
//! The public surface is intentionally small: build a [`Mail`] value, send it
//! through the cloneable [`Mailer`] extractor, and swap transports through the
//! [`MailTransport`] trait when SMTP is not the right coffin lining.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::FromRequestParts;
use axum::response::{Html, IntoResponse, Response};
use lettre::message::header::{ContentTransferEncoding, ContentType};
use lettre::message::{
    Attachment as LettreAttachment, Body as LettreBody, Mailbox, MultiPart, SinglePart,
};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AppState, AutumnError, AutumnResult};

/// Mail transport selected by `[mail].transport`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Write full email contents to the tracing log at INFO.
    Log,
    /// Write RFC 822 `.eml` files under `target/mail` or a configured dir.
    File,
    /// Send through SMTP using Lettre.
    Smtp,
    /// Drop all email sends successfully.
    #[default]
    Disabled,
}

impl Transport {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "log" => Some(Self::Log),
            "file" => Some(Self::File),
            "smtp" => Some(Self::Smtp),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// SMTP TLS mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Plain connection; useful only for local test SMTP sinks.
    Disabled,
    /// Upgrade with STARTTLS.
    #[default]
    StartTls,
    /// Connect with wrapper TLS.
    Tls,
}

impl TlsMode {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" => Some(Self::Disabled),
            "starttls" | "start_tls" => Some(Self::StartTls),
            "tls" => Some(Self::Tls),
            _ => None,
        }
    }
}

/// SMTP configuration nested under `[mail.smtp]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SmtpConfig {
    /// SMTP host name.
    #[serde(default)]
    pub host: Option<String>,
    /// SMTP port. Defaults to 587 for STARTTLS, 465 for TLS, and 25 for disabled TLS.
    #[serde(default)]
    pub port: Option<u16>,
    /// Optional SMTP username.
    #[serde(default)]
    pub username: Option<String>,
    /// Environment variable containing the SMTP password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// TLS behavior.
    #[serde(default)]
    pub tls: TlsMode,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: None,
            username: None,
            password_env: None,
            tls: TlsMode::StartTls,
        }
    }
}

/// `[mail]` config section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // independent transport/prod/unsubscribe toggles
pub struct MailConfig {
    /// Active transport.
    #[serde(default)]
    pub transport: Transport,
    /// Default From header.
    #[serde(default)]
    pub from: Option<String>,
    /// Default Reply-To header.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Permit log transport in `prod`.
    #[serde(default)]
    pub allow_log_in_production: bool,
    /// Acknowledge that `deliver_later` may use the in-process Tokio fallback in
    /// `prod`. Without a registered durable [`MailDeliveryQueue`], this is the
    /// only way to start the app in `prod` with an active mail transport.
    #[serde(default)]
    pub allow_in_process_deliver_later_in_production: bool,
    /// Directory for file transport.
    #[serde(default = "default_file_dir")]
    pub file_dir: PathBuf,
    /// Force-enable the dev mail preview UI.
    ///
    /// The UI is auto-enabled in `dev` when `mail.transport = "file"`.
    /// Setting this flag outside `dev` is rejected at startup.
    #[serde(default)]
    pub preview: bool,
    /// Base URL for RFC 8058 one-click `List-Unsubscribe` links, e.g.
    /// `https://app.example.com`. Required (alongside or instead of
    /// [`unsubscribe_mailto`](Self::unsubscribe_mailto)) for any `#[mailer]`
    /// that declares `list_unsubscribe`.
    #[serde(default)]
    pub unsubscribe_base_url: Option<String>,
    /// `mailto:` fallback address for the `List-Unsubscribe` header, e.g.
    /// `unsubscribe@example.com`.
    #[serde(default)]
    pub unsubscribe_mailto: Option<String>,
    /// Validity window for signed unsubscribe tokens, in days.
    #[serde(default = "default_unsubscribe_ttl_days")]
    pub unsubscribe_token_ttl_days: i64,
    /// Opt in to mounting the framework's default one-click unsubscribe endpoint
    /// (`GET`/`POST /_autumn/unsubscribe`). Off by default so JSON-only apps
    /// never get an HTML endpoint they didn't ask for; also settable via
    /// [`AppBuilder::mount_unsubscribe_endpoint`](crate::app::AppBuilder::mount_unsubscribe_endpoint).
    #[serde(default)]
    pub mount_unsubscribe_endpoint: bool,
    /// Default for CSS inlining of HTML mail bodies (issue #1254).
    ///
    /// When `true`, every HTML body sent through a [`Mailer`] built from this
    /// config has its `<style>` rules inlined onto matching elements as
    /// `style="…"` attributes at send time, so it renders styled in clients
    /// that strip `<head>`/`<style>` (Gmail, Outlook). Off by default —
    /// existing apps are unaffected until they opt in. A per-message
    /// [`MailBuilder::inline_css`] call overrides this default in either
    /// direction (explicit builder value wins).
    #[serde(default)]
    pub inline_css: bool,
    /// SMTP settings.
    #[serde(default)]
    pub smtp: SmtpConfig,
}

/// Whether `url` is an absolute `https://` URL with a non-empty host and no
/// query/fragment, e.g. `https://app.example.com` or `…/base`. Rejects bare
/// `https://`, `https:///path`, and bases carrying `?`/`#` (the unsubscribe
/// path/token is appended afterwards, so a query/fragment base would not route).
fn is_valid_https_base_url(url: &str) -> bool {
    // Reject characters that are unsafe inside an RFC 2369 angle-bracket URI or
    // would survive into the raw header: `Url::parse` percent-encodes a space or
    // `<`/`>` in the path, but the *original* string is what gets rendered as
    // `<…?token=…>`, so a raw `<`/`>`/whitespace/control char would close or
    // corrupt the `List-Unsubscribe` value.
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'))
    {
        return false;
    }
    // Require the raw input to be literally `https://<authority>…`. `url::Url`
    // normalizes a missing/short authority (`https:`, `https:app.example.com`,
    // `https:/app.example.com`, `https:///path`) into a valid HTTPS URL with a
    // host, but the *original* malformed string is what gets rendered into the
    // header — so reject anything that isn't `https://` followed by a non-`/`
    // authority character.
    match url.strip_prefix("https://") {
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') => {}
        _ => return false,
    }
    let Ok(parsed) = ::url::Url::parse(url) else {
        return false;
    };
    // Require an absolute https:// URL with a real host and a valid authority.
    // Parsing (rather than splitting on `/`) rejects malformed authorities like
    // `https://app.example.com:abc` (bad port) or `https://@/base` (empty host).
    // No credentials in the link, and no query/fragment — either would break the
    // appended `?token=…`.
    parsed.scheme() == "https"
        && parsed.host_str().is_some_and(|h| !h.is_empty())
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

/// Whether `value` is a usable unsubscribe mailbox — a bare `local@domain` or a
/// `mailto:local@domain` URI, with non-empty parts and no whitespace.
fn is_valid_mailto_address(value: &str) -> bool {
    // Reject control characters and RFC 2369 delimiters anywhere in the value
    // (including inside a `?subject=…` query): the value is rendered verbatim
    // inside `<mailto:…>`, so a control char (CRLF injection, e.g. an extra
    // `Bcc:`) or a `<`/`>`/`,` (which would close the entry and inject an extra
    // `List-Unsubscribe` target) must not pass.
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ','))
    {
        return false;
    }
    let address = value
        .trim()
        .strip_prefix("mailto:")
        .unwrap_or_else(|| value.trim());
    // Drop any `?subject=…` parameters before validating the address itself.
    let address = address.split('?').next().unwrap_or("");
    match address.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !address.contains(char::is_whitespace)
                // Reject any other URI scheme (e.g. `https://unsub@example.com`):
                // `:` / `/` here mean the value is not a bare mailbox, and it
                // would otherwise render as a bogus `<mailto:https://…>` header.
                && !address.contains([':', '/'])
        }
        None => false,
    }
}

const fn default_unsubscribe_ttl_days() -> i64 {
    crate::mail::unsubscribe::DEFAULT_TOKEN_TTL_DAYS
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            transport: Transport::Disabled,
            from: None,
            reply_to: None,
            allow_log_in_production: false,
            allow_in_process_deliver_later_in_production: false,
            file_dir: default_file_dir(),
            preview: false,
            unsubscribe_base_url: None,
            unsubscribe_mailto: None,
            unsubscribe_token_ttl_days: default_unsubscribe_ttl_days(),
            mount_unsubscribe_endpoint: false,
            inline_css: false,
            smtp: SmtpConfig::default(),
        }
    }
}

impl MailConfig {
    /// Validate semantic mail configuration.
    ///
    /// # Errors
    ///
    /// Returns [`crate::config::ConfigError::Validation`] for unsafe profile
    /// combinations or missing SMTP settings.
    pub fn validate(&self, profile: Option<&str>) -> Result<(), crate::config::ConfigError> {
        if matches!(profile, Some("prod" | "production"))
            && self.transport == Transport::Log
            && !self.allow_log_in_production
        {
            return Err(crate::config::ConfigError::Validation(
                "mail.transport = \"log\" is disabled in prod; set mail.allow_log_in_production = true to acknowledge this explicitly".to_owned(),
            ));
        }

        if self.transport == Transport::Smtp
            && self.smtp.host.as_deref().map_or("", str::trim).is_empty()
        {
            return Err(crate::config::ConfigError::Validation(
                "mail.smtp.host is required when mail.transport = \"smtp\"".to_owned(),
            ));
        }

        if self.preview && !matches!(profile, Some("dev" | "development")) {
            return Err(crate::config::ConfigError::Validation(
                "mail.preview = true is only allowed in dev; refusing to mount /_autumn/mail outside the dev profile".to_owned(),
            ));
        }

        if self.unsubscribe_token_ttl_days <= 0 {
            return Err(crate::config::ConfigError::Validation(
                "mail.unsubscribe_token_ttl_days must be a positive number of days; a non-positive value would make every unsubscribe token immediately expired".to_owned(),
            ));
        }

        if matches!(profile, Some("prod" | "production"))
            && let Some(base) = self.unsubscribe_base_url.as_deref().map(str::trim)
            && !base.is_empty()
            && !is_valid_https_base_url(base)
        {
            return Err(crate::config::ConfigError::Validation(
                "mail.unsubscribe_base_url must be an absolute https:// URL with a host in prod; mailbox providers require HTTPS for RFC 8058 one-click unsubscribe".to_owned(),
            ));
        }

        if matches!(profile, Some("prod" | "production"))
            && let Some(mailto) = self.unsubscribe_mailto.as_deref().map(str::trim)
            && !mailto.is_empty()
            && !is_valid_mailto_address(mailto)
        {
            return Err(crate::config::ConfigError::Validation(
                "mail.unsubscribe_mailto must be a bare mailbox address (or mailto: URI) like unsubscribe@example.com".to_owned(),
            ));
        }

        Ok(())
    }

    pub(crate) fn preview_routes_enabled(&self, profile: Option<&str>) -> bool {
        matches!(profile, Some("dev" | "development"))
            && (self.preview || self.transport == Transport::File)
    }

    /// Whether a base URL is configured. A `mailto`-only configuration emits a
    /// `List-Unsubscribe: <mailto:…>` header but needs no HTTP endpoint.
    pub(crate) fn unsubscribe_base_url_set(&self) -> bool {
        self.unsubscribe_base_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// Whether the framework's default one-click unsubscribe endpoint should be
    /// mounted: the app opted in **and** a base URL is configured. Opt-in keeps
    /// JSON-only apps free of an HTML endpoint they never requested.
    pub(crate) fn should_mount_unsubscribe_endpoint(&self) -> bool {
        self.mount_unsubscribe_endpoint && self.unsubscribe_base_url_set()
    }
}

fn default_file_dir() -> PathBuf {
    PathBuf::from("target/mail")
}

/// Renderable mail body input.
pub trait IntoMailBody {
    /// Convert into owned body text.
    fn into_mail_body(self) -> String;
}

impl IntoMailBody for String {
    fn into_mail_body(self) -> String {
        self
    }
}

impl IntoMailBody for &str {
    fn into_mail_body(self) -> String {
        self.to_owned()
    }
}

impl IntoMailBody for maud::Markup {
    fn into_mail_body(self) -> String {
        self.into_string()
    }
}

/// Placeholder token in shared mailer layouts marking where the per-mailer body
/// fragment is inserted.
///
/// Layouts that do not contain this marker are ignored and the raw body is
/// delivered instead (prevents silent content loss).
pub const MAIL_LAYOUT_CONTENT_MARKER: &str = "{{ content }}";

/// Compose a `layout` string and a `body` fragment by replacing
/// [`MAIL_LAYOUT_CONTENT_MARKER`] with `body`.
///
/// If the layout does not contain the marker, `body` is returned unchanged so
/// content is never silently dropped.
#[must_use]
pub fn compose_layout(layout: &str, body: &str) -> String {
    if layout.contains(MAIL_LAYOUT_CONTENT_MARKER) {
        layout.replace(MAIL_LAYOUT_CONTENT_MARKER, body)
    } else {
        body.to_owned()
    }
}

/// Whether `html` contains a `<style` tag (case-insensitive), i.e. there is any
/// embedded stylesheet worth inlining. Allocation-free ASCII scan — the fast
/// path for the common case of plain-text or already-inlined bodies.
fn html_contains_style_block(html: &str) -> bool {
    html.as_bytes()
        .windows(6)
        .any(|window| window.eq_ignore_ascii_case(b"<style"))
}

/// Whether `html` looks like a full HTML *document* — it carries a `<!doctype`,
/// `<html`, or `<body` marker — rather than a bare fragment. Autumn permits raw
/// fragment bodies when no layout wraps them, so [`inline_css_html`] uses this to
/// decide whether to strip the synthetic document wrappers `css-inline` adds. A
/// user-authored `<body>` is therefore recognized as a document and its
/// structure is left untouched. Allocation-free case-insensitive ASCII scan.
fn html_is_full_document(html: &str) -> bool {
    let bytes = html.as_bytes();
    bytes.windows(5).any(|w| w.eq_ignore_ascii_case(b"<html"))
        || bytes.windows(5).any(|w| w.eq_ignore_ascii_case(b"<body"))
        || bytes
            .windows(9)
            .any(|w| w.eq_ignore_ascii_case(b"<!doctype"))
}

/// Strip the synthetic `<html>`/`<head>`/`<body>` wrappers that `css-inline`'s
/// document mode adds around fragment input, reconstructing the fragment.
///
/// [`inline_css_html`] always inlines in document mode because `css-inline`'s
/// fragment mode drops retained `@media`/at-rules (it re-homes them to `<head>`,
/// which a fragment lacks). Document mode instead wraps a fragment body in
/// synthetic structural tags. This is only ever called on output we produced by
/// document-inlining a body we already determined was a *fragment*, so those
/// wrappers are always `css-inline`'s own, never user-authored.
///
/// `css-inline` (via `html5ever`) serializes a document as a canonical,
/// attribute-free `<html><head>…</head><body>…</body></html>`, so exact-literal
/// matching is safe. The result is the `<head>` contents (the retained `<style>`
/// block carrying un-inlinable `@media`/pseudo rules, per AC5) followed by the
/// `<body>` contents — preserving the original fragment ordering of `<style>`
/// before body content. If the expected shape is absent, the input is returned
/// unchanged rather than risking corruption.
fn unwrap_synthetic_document(doc: &str) -> String {
    // html5ever emits these exact byte sequences, lowercased and without
    // attributes or whitespace, for the wrappers it synthesizes.
    let inner = doc
        .strip_prefix("<html>")
        .and_then(|rest| rest.strip_suffix("</html>"))
        .unwrap_or(doc);
    let (head, after_head) = match inner.strip_prefix("<head>") {
        Some(rest) => match rest.split_once("</head>") {
            Some(split) => split,
            // Malformed/unexpected shape: don't risk corrupting the body.
            None => return doc.to_owned(),
        },
        None => ("", inner),
    };
    let body = after_head
        .strip_prefix("<body>")
        .map_or(after_head, |rest| {
            rest.strip_suffix("</body>").unwrap_or(rest)
        });
    format!("{head}{body}")
}

/// Inline the `<style>` rules of an HTML mail body onto matching elements as
/// `style="…"` attributes, so the message renders styled in clients that strip
/// `<head>`/`<style>` (Gmail, Outlook). See issue #1254.
///
/// Behavior:
/// - Bodies with no `<style>` block are returned unchanged (fast path), so
///   plain-text and already-fully-inlined bodies pass through byte-for-byte.
/// - `<style>` blocks are retained, but rules that were successfully inlined are
///   stripped from them — so what remains is exactly the un-inlinable
///   `@media`/pseudo-class rules, which still reach clients that honor them.
///   Because the inlinable rules are removed from the retained block, running
///   this again is a no-op: inlining is idempotent.
/// - Remote/`<link>` stylesheets are never fetched (the `css-inline` network
///   feature is not compiled in) — only embedded `<style>` CSS is inlined. The
///   `<link rel="stylesheet">` tags themselves are preserved in the body so the
///   linked CSS still reaches clients rather than being silently dropped.
/// - A raw *fragment* body (no `<html>`/`<body>`/doctype) stays a fragment.
///   `css-inline`'s document mode wraps fragment output in synthetic
///   `<html>`/`<head>`/`<body>` tags; those wrappers are stripped back off (see
///   [`unwrap_synthetic_document`]) so opting into inlining never promotes a
///   fragment MIME body into a full document. Full-document bodies keep their
///   structure unchanged.
///
/// # Errors
///
/// Returns [`MailError::CssInline`] if the body cannot be parsed/inlined, rather
/// than returning a silently corrupted body.
fn inline_css_html(html: &str) -> Result<String, MailError> {
    // Fast path: nothing to inline. Keeps text-like, fragment, and
    // already-inlined bodies byte-identical and makes re-inlining idempotent.
    if !html_contains_style_block(html) {
        return Ok(html.to_owned());
    }
    let inliner = css_inline::CSSInliner::options()
        // Retain `<style>` so un-inlinable rules survive…
        .keep_style_tags(true)
        // …including `@media`/other at-rules (dropped by default), so responsive
        // tweaks still work in clients that honor them.
        .keep_at_rules(true)
        // …but drop the rules we did inline, leaving only the un-inlinable ones
        // in the retained block (also what makes a second pass a no-op).
        .remove_inlined_selectors(true)
        // Never reach out to the network for `<link>`ed stylesheets.
        .load_remote_stylesheets(false)
        // …but since we do NOT fetch them, keep the `<link rel="stylesheet">`
        // tags in the body (dropped by default) so the linked CSS still reaches
        // clients rather than being silently discarded from the delivered body.
        .keep_link_tags(true)
        // Also emit the presentational HTML `width`/`height` attributes (from the
        // inlined CSS dimensions) on `table`/`td`/`th`/`img` — both default off.
        // Outlook-family clients ignore CSS `width`/`height`, so without these
        // attributes those elements lose their intended sizing there.
        .apply_width_attributes(true)
        .apply_height_attributes(true)
        .build();
    // Inline in document mode. Its fragment mode would avoid the `<html>`/`<body>`
    // wrapping but drops retained `@media`/at-rules (it re-homes them to `<head>`,
    // which a fragment lacks) — breaking AC5. So document-inline unconditionally,
    // then, for a fragment body, strip the synthetic wrappers back off so the
    // MIME body stays a fragment. Full documents keep their structure as-is.
    let is_fragment = !html_is_full_document(html);
    let rendered = inliner
        .inline(html)
        // Defensive / effectively unreachable: with remote-stylesheet loading
        // disabled above and no file loader configured, `css-inline` only errors
        // on IO/network — both compiled out here. It is fully lenient toward
        // malformed CSS/HTML (garbage `<style>` bodies inline to an unchanged
        // fragment, never an error). We still surface the typed error rather than
        // `expect`ing, to keep the API stable if those loaders are ever enabled.
        .map_err(|error| MailError::CssInline(error.to_string()))?;
    Ok(if is_fragment {
        unwrap_synthetic_document(&rendered)
    } else {
        rendered
    })
}

/// A file attached to a [`Mail`] message.
///
/// Built via [`MailBuilder::attach`]. Carries raw, undecoded bytes so it
/// round-trips byte-identical through every transport and through a durable
/// [`MailDeliveryQueue`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAttachment {
    /// Attachment filename, as presented to the recipient's mail client.
    pub filename: String,
    /// Declared MIME content type (e.g. `"application/pdf"`).
    pub content_type: String,
    /// Raw attachment bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for MailAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailAttachment")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

/// A transactional email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mail {
    /// Optional From header. Falls back to [`Mailer`]'s default.
    pub from: Option<String>,
    /// Optional Reply-To header. Falls back to [`Mailer`]'s default.
    pub reply_to: Option<String>,
    /// To recipients.
    pub to: Vec<String>,
    /// Subject header.
    pub subject: String,
    /// HTML body.
    pub html: Option<String>,
    /// Plain-text body.
    pub text: Option<String>,
    /// Logical list / suppression scope for RFC 8058 one-click
    /// `List-Unsubscribe` (e.g. `"weekly_digest"`). Set by the
    /// `#[mailer(list_unsubscribe = "...")]` macro. `None` for transactional
    /// mail that must never carry unsubscribe headers (password resets, MFA
    /// codes, security alerts). See [`crate::mail::unsubscribe`].
    pub list_unsubscribe: Option<String>,
    /// Additional raw headers emitted on the wire by every transport. Used to
    /// carry the computed `List-Unsubscribe` / `List-Unsubscribe-Post` headers,
    /// but available for any custom header.
    pub extra_headers: Vec<(String, String)>,
    /// Files attached to this message, in declared order.
    #[serde(default)]
    pub attachments: Vec<MailAttachment>,
    /// When `true`, [`Mailer::send`] delivers this message even to addresses on
    /// the bounce/complaint [`suppression`] list. Set via
    /// [`MailBuilder::ignore_suppression`] for genuinely critical mail
    /// (password resets, MFA codes, security alerts) that must reach the
    /// recipient regardless of prior delivery failures. `false` by default.
    #[serde(default)]
    pub ignore_suppression: bool,
    /// Per-message override for CSS inlining (issue #1254).
    ///
    /// `Some(true)`/`Some(false)` force inlining on/off for this message,
    /// overriding the [`Mailer`]'s configured default; `None` (the default)
    /// defers to [`MailConfig::inline_css`]. Set via
    /// [`MailBuilder::inline_css`]. On the deferred/durable path a `None` is
    /// frozen to the originating mailer's default before the message is
    /// persisted to a [`MailDeliveryQueue`], so the enqueued job is
    /// self-describing and deferred mail inlines consistently with an immediate
    /// send even when a different worker consumes the queue.
    #[serde(default)]
    pub inline_css: Option<bool>,
}

/// Stable root path for the dev mail preview UI.
pub const MAIL_PREVIEW_PATH: &str = "/_autumn/mail";

const MAIL_PREVIEW_MESSAGE_PATH: &str = "/_autumn/mail/messages/{message_id}";
const MAIL_PREVIEW_TEMPLATE_PATH: &str = "/_autumn/mail/previews/{mailer}/{method}";

/// A developer-authored, zero-argument mail template preview.
#[derive(Clone)]
pub struct MailPreview {
    mailer: &'static str,
    method: &'static str,
    render: fn() -> Mail,
}

impl std::fmt::Debug for MailPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailPreview")
            .field("mailer", &self.mailer)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

impl MailPreview {
    /// Register a mail preview for the dev mail preview UI.
    #[must_use]
    pub const fn new(mailer: &'static str, method: &'static str, render: fn() -> Mail) -> Self {
        Self {
            mailer,
            method,
            render,
        }
    }

    /// Mailer type label used in preview URLs.
    #[must_use]
    pub const fn mailer(&self) -> &'static str {
        self.mailer
    }

    /// Preview method label used in preview URLs.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        self.method
    }

    /// Render the preview without invoking any configured transport.
    ///
    /// # Errors
    ///
    /// Returns [`MailPreviewError::PreviewPanicked`] if the preview function
    /// panics while constructing sample data.
    pub fn render(&self) -> Result<Mail, MailPreviewError> {
        std::panic::catch_unwind(|| (self.render)()).map_err(|_| {
            MailPreviewError::PreviewPanicked {
                mailer: self.mailer,
                method: self.method,
            }
        })
    }
}

/// Collection of registered mail previews stored on [`AppState`].
#[derive(Debug, Clone, Default)]
pub struct MailPreviewRegistry {
    previews: Arc<Vec<MailPreview>>,
}

impl MailPreviewRegistry {
    /// Create a registry from preview registrations.
    #[must_use]
    pub fn new(previews: Vec<MailPreview>) -> Self {
        Self {
            previews: Arc::new(previews),
        }
    }

    /// Registered previews.
    #[must_use]
    pub fn previews(&self) -> &[MailPreview] {
        &self.previews
    }

    fn find(&self, mailer: &str, method: &str) -> Option<MailPreview> {
        self.previews
            .iter()
            .find(|preview| preview.mailer == mailer && preview.method == method)
            .cloned()
    }
}

/// Dev mail preview UI errors.
#[derive(Debug, Error)]
pub enum MailPreviewError {
    /// File transport preview IO failed.
    #[error("mail preview file IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// Requested captured message was not found.
    #[error("captured mail message not found: {0}")]
    NotFound(String),
    /// Requested message id is not a single `.eml` filename.
    #[error("invalid captured mail message id: {0}")]
    InvalidMessageId(String),
    /// Developer-authored preview panicked while rendering sample data.
    #[error("mail preview {mailer}::{method} panicked while rendering")]
    PreviewPanicked {
        /// Mailer label.
        mailer: &'static str,
        /// Method label.
        method: &'static str,
    },
}

impl Mail {
    /// Start building a mail message.
    #[must_use]
    pub fn builder() -> MailBuilder {
        MailBuilder::default()
    }

    fn with_defaults(mut self, defaults: &MailerDefaults) -> Self {
        if self.from.is_none() {
            self.from.clone_from(&defaults.from);
        }
        if self.reply_to.is_none() {
            self.reply_to.clone_from(&defaults.reply_to);
        }
        self
    }
}

/// Builder for [`Mail`].
#[derive(Debug, Clone, Default)]
pub struct MailBuilder {
    from: Option<String>,
    reply_to: Option<String>,
    to: Vec<String>,
    subject: Option<String>,
    html: Option<String>,
    text: Option<String>,
    html_layout: Option<String>,
    text_layout: Option<String>,
    list_unsubscribe: Option<String>,
    extra_headers: Vec<(String, String)>,
    attachments: Vec<MailAttachment>,
    ignore_suppression: bool,
    inline_css: Option<bool>,
}

impl MailBuilder {
    /// Set a message-specific From header.
    #[must_use]
    pub fn from(mut self, from: impl Into<String>) -> Self {
        self.from = Some(from.into());
        self
    }

    /// Set a message-specific Reply-To header.
    #[must_use]
    pub fn reply_to(mut self, reply_to: impl Into<String>) -> Self {
        self.reply_to = Some(reply_to.into());
        self
    }

    /// Add a To recipient.
    #[must_use]
    pub fn to(mut self, to: impl Into<String>) -> Self {
        self.to.push(to.into());
        self
    }

    /// Set the subject.
    #[must_use]
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Set the HTML body.
    #[must_use]
    pub fn html(mut self, html: impl IntoMailBody) -> Self {
        self.html = Some(html.into_mail_body());
        self
    }

    /// Set the plain-text body.
    #[must_use]
    pub fn text(mut self, text: impl IntoMailBody) -> Self {
        self.text = Some(text.into_mail_body());
        self
    }

    /// Tag this message with a logical list / suppression scope, opting it into
    /// RFC 8058 one-click `List-Unsubscribe` handling at send time.
    ///
    /// Authors normally set this declaratively via
    /// `#[mailer(list_unsubscribe = "...")]`; this builder method exists for
    /// hand-rolled mail and previews.
    #[must_use]
    pub fn list_unsubscribe(mut self, scope: impl Into<String>) -> Self {
        self.list_unsubscribe = Some(scope.into());
        self
    }

    /// Bypass the bounce/complaint [`suppression`] list for this message.
    ///
    /// [`Mailer::send`] normally skips recipients that have hard-bounced or
    /// filed a spam complaint. Call this for genuinely critical mail —
    /// password resets, MFA codes, security alerts — that must be delivered
    /// even to a suppressed address. Use sparingly: repeatedly sending to a
    /// hard-bounced address is exactly what damages sender reputation.
    #[must_use]
    pub const fn ignore_suppression(mut self) -> Self {
        self.ignore_suppression = true;
        self
    }

    /// Force CSS inlining on or off for this message, overriding the
    /// [`Mailer`]'s configured [`MailConfig::inline_css`] default.
    ///
    /// When enabled, the HTML body's `<style>` rules are inlined onto matching
    /// elements as `style="…"` attributes at send time so the message renders
    /// styled in clients that strip `<head>`/`<style>` (Gmail, Outlook).
    /// Un-inlinable `@media`/pseudo-class rules are preserved in a retained
    /// `<style>` block. Text bodies and HTML with no `<style>` block are left
    /// untouched.
    ///
    /// Precedence: an explicit call here always wins over the config default —
    /// `inline_css(false)` opts a single message out even when the environment
    /// defaults inlining on, and `inline_css(true)` opts a single message in
    /// when the default is off.
    #[must_use]
    pub const fn inline_css(mut self, enabled: bool) -> Self {
        self.inline_css = Some(enabled);
        self
    }

    /// Add a raw header emitted by every transport.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Attach a file. Calling this repeatedly appends attachments in the
    /// order they were declared; the SMTP and file transports both encode
    /// them as `multipart/mixed` parts with a `base64`
    /// `Content-Transfer-Encoding`.
    ///
    /// ```rust,ignore
    /// let mail = Mail::builder()
    ///     .to("ada@example.com")
    ///     .subject("Your invoice")
    ///     .text("Your invoice is attached.")
    ///     .attach("invoice.pdf", "application/pdf", pdf_bytes)
    ///     .build()?;
    /// ```
    #[must_use]
    pub fn attach(
        mut self,
        filename: impl Into<String>,
        content_type: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.attachments.push(MailAttachment {
            filename: filename.into(),
            content_type: content_type.into(),
            bytes: bytes.into(),
        });
        self
    }

    /// Wrap the HTML and text bodies in a shared layout.
    ///
    /// The layout strings must contain [`MAIL_LAYOUT_CONTENT_MARKER`]
    /// (`{{ content }}`) where the per-mailer body fragment should be inserted.
    /// If a layout does not contain the marker the raw body is delivered
    /// unchanged (content is never silently dropped).
    ///
    /// Call this method with the shared `_layout.html` and `_layout.txt`
    /// templates. Omitting the call delivers the raw body — use that as the
    /// per-mailer opt-out for fully-custom or one-line plaintext messages.
    #[must_use]
    pub fn layout(
        mut self,
        html_layout: impl IntoMailBody,
        text_layout: impl IntoMailBody,
    ) -> Self {
        self.html_layout = Some(html_layout.into_mail_body());
        self.text_layout = Some(text_layout.into_mail_body());
        self
    }

    /// Build the mail.
    ///
    /// # Errors
    ///
    /// Returns [`MailError::InvalidMessage`] when required fields are missing.
    pub fn build(self) -> Result<Mail, MailError> {
        if self.to.is_empty() {
            return Err(MailError::InvalidMessage(
                "mail must have at least one recipient".to_owned(),
            ));
        }
        let subject = self
            .subject
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| MailError::InvalidMessage("mail subject is required".to_owned()))?;
        if self.html.is_none() && self.text.is_none() {
            return Err(MailError::InvalidMessage(
                "mail must include html or text body".to_owned(),
            ));
        }
        for attachment in &self.attachments {
            if attachment.filename.trim().is_empty()
                || attachment.filename.chars().any(char::is_control)
            {
                return Err(MailError::InvalidMessage(format!(
                    "attachment filename {:?} must be non-empty and free of control characters",
                    attachment.filename
                )));
            }
            if let Err(error) = ContentType::parse(&attachment.content_type) {
                return Err(MailError::InvalidMessage(format!(
                    "attachment {:?} has invalid content type {:?}: {error}",
                    attachment.filename, attachment.content_type
                )));
            }
        }
        // A layout is only applied when the corresponding body is present.
        // If only one of html/text is set, the other layout half is intentionally
        // skipped rather than erroring — a text-only mailer may legitimately pass
        // an html_layout that has no effect, and vice-versa.
        let html = match (self.html, self.html_layout) {
            (Some(body), Some(layout)) => Some(compose_layout(&layout, &body)),
            (html, _) => html, // layout without a body: silently unused (by design)
        };
        let text = match (self.text, self.text_layout) {
            (Some(body), Some(layout)) => Some(compose_layout(&layout, &body)),
            (text, _) => text, // layout without a body: silently unused (by design)
        };
        Ok(Mail {
            from: self.from,
            reply_to: self.reply_to,
            to: self.to,
            subject,
            html,
            text,
            list_unsubscribe: self.list_unsubscribe,
            extra_headers: self.extra_headers,
            attachments: self.attachments,
            ignore_suppression: self.ignore_suppression,
            inline_css: self.inline_css,
        })
    }
}

/// Mailer errors.
#[derive(Debug, Error)]
pub enum MailError {
    /// Message could not be built or validated.
    #[error("invalid mail message: {0}")]
    InvalidMessage(String),
    /// Deferred delivery could not be scheduled.
    #[error("mail runtime unavailable: {0}")]
    RuntimeUnavailable(String),
    /// Address parsing failed.
    #[error("invalid mail address {address:?}: {source}")]
    InvalidAddress {
        /// Address that failed to parse.
        address: String,
        /// Lettre parse error.
        source: lettre::address::AddressError,
    },
    /// Lettre message construction failed.
    #[error("failed to build mail message: {0}")]
    Build(#[from] lettre::error::Error),
    /// SMTP transport failed.
    #[error("smtp send failed: {0}")]
    Smtp(#[from] lettre::transport::smtp::Error),
    /// File transport failed.
    #[error("file mail transport failed: {0}")]
    Io(#[from] std::io::Error),
    /// Every recipient of the message is on the bounce/complaint
    /// [`suppression`] list, so nothing was delivered. Distinct from success:
    /// callers can distinguish "sent" from "intentionally dropped". Bypass with
    /// [`MailBuilder::ignore_suppression`] for critical mail.
    #[error("all recipients are on the mail suppression list; nothing was sent")]
    AllRecipientsSuppressed,
    /// CSS inlining of the HTML body failed (issue #1254). `send` fails loudly
    /// with this typed error instead of delivering a corrupted body — the
    /// message is not sent, so callers can decide how to recover. Defensive:
    /// with remote and file loaders disabled, `css-inline` is fully lenient and
    /// this path is effectively unreachable, but the variant keeps the API
    /// stable if those loaders are ever enabled.
    #[error("failed to inline CSS into HTML mail body: {0}")]
    CssInline(String),
    /// `deliver_later`/`deliver_later_eager` was called in `prod` with a
    /// non-disabled transport, no durable [`MailDeliveryQueue`] registered, and
    /// no explicit [`MailConfig::allow_in_process_deliver_later_in_production`]
    /// opt-in (issue #2142). Enforced lazily at the first deferred send rather
    /// than at boot, so applications that never call `deliver_later` are
    /// unaffected.
    #[error(
        "mail.deliver_later has no durable backend in prod: register a MailDeliveryQueueHandle on AppState or set mail.allow_in_process_deliver_later_in_production = true to opt into the in-process Tokio fallback"
    )]
    NoDurableQueueInProduction,
}

/// Escape hatch for custom transports.
pub trait MailTransport: Send + Sync {
    /// Send a mail message.
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

    /// Returns `true` if this transport is intentionally a no-op (e.g.
    /// [`Transport::Disabled`] for review apps and tests).
    ///
    /// When `true`, [`Mailer::deliver_later`] short-circuits before the queue
    /// or in-process fallback so deferred mail honors the same "drop
    /// everything" contract as immediate sends. Custom transports that mean
    /// "drop all mail" can override this to opt into the same behavior; the
    /// default of `false` preserves the existing contract for transports that
    /// merely capture mail (file, log, etc.) or send it (SMTP, custom APIs).
    fn is_disabled(&self) -> bool {
        false
    }
}

/// Durable backend for [`Mailer::deliver_later`].
///
/// Implementors persist the mail (DB row, Redis stream, Harvest job, etc.) and
/// return as soon as the handoff is durable. The framework's in-process Tokio
/// fallback is intentionally not durable; production deployments should
/// register a real implementation via [`MailDeliveryQueueHandle`] before
/// `install_mailer` runs, or set
/// [`MailConfig::allow_in_process_deliver_later_in_production`] to opt into the
/// fallback explicitly.
pub trait MailDeliveryQueue: Send + Sync {
    /// Enqueue a mail for durable later delivery.
    fn enqueue<'a>(
        &'a self,
        mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;
}

/// Cloneable handle to a [`MailDeliveryQueue`].
///
/// Designed for storage on [`AppState`] extensions. Plugins
/// (Harvest, custom Redis, etc.) install this before `install_mailer` runs and
/// the mailer picks it up.
#[derive(Clone)]
pub struct MailDeliveryQueueHandle(Arc<dyn MailDeliveryQueue>);

impl MailDeliveryQueueHandle {
    /// Wrap a queue implementation in a cloneable handle.
    #[must_use]
    pub fn new(queue: impl MailDeliveryQueue + 'static) -> Self {
        Self(Arc::new(queue))
    }

    /// Wrap an already-shared queue implementation.
    #[must_use]
    pub fn from_arc(queue: Arc<dyn MailDeliveryQueue>) -> Self {
        Self(queue)
    }

    /// Borrow the inner queue.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn MailDeliveryQueue> {
        &self.0
    }
}

impl std::fmt::Debug for MailDeliveryQueueHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailDeliveryQueueHandle").finish()
    }
}

// ── RFC 8058 List-Unsubscribe ────────────────────────────────────────────────

/// Stable root path for the framework's default one-click unsubscribe endpoint.
pub const UNSUBSCRIBE_PATH: &str = "/_autumn/unsubscribe";

/// Compile-time registration of a `#[mailer(list_unsubscribe = "...")]`.
///
/// Emitted by the `#[mailer]` macro. Lets production startup and `autumn doctor`
/// enumerate which logical lists exist so they can fail closed when the app has
/// no unsubscribe destination configured.
#[derive(Debug)]
pub struct MailerListUnsubscribeDescriptor {
    /// Mailer type name (e.g. `WeeklyDigestMailer`).
    pub mailer: &'static str,
    /// Logical list / suppression scope (e.g. `weekly_digest`).
    pub scope: &'static str,
}

inventory::collect!(MailerListUnsubscribeDescriptor);

/// Every `list_unsubscribe` declaration registered across the binary.
#[must_use]
pub fn registered_list_unsubscribe_scopes() -> Vec<&'static MailerListUnsubscribeDescriptor> {
    inventory::iter::<MailerListUnsubscribeDescriptor>
        .into_iter()
        .collect()
}

/// Returns `true` when any `#[mailer]` in this binary opted into
/// `list_unsubscribe`.
#[must_use]
pub fn has_list_unsubscribe_mailers() -> bool {
    inventory::iter::<MailerListUnsubscribeDescriptor>
        .into_iter()
        .next()
        .is_some()
}

/// Whether production startup must fail closed: a `#[mailer]` declares
/// `list_unsubscribe` but the app configured no unsubscribe destination.
#[must_use]
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) const fn unsubscribe_config_fail_closed(
    enforce: bool,
    in_production: bool,
    has_list_mailers: bool,
    unsubscribe_configured: bool,
) -> bool {
    enforce && in_production && has_list_mailers && !unsubscribe_configured
}

/// Encrypted, short-lived, stateless unsubscribe tokens.
///
/// A token is `base64url(version ‖ nonce ‖ AES-256-GCM(payload))`, where the
/// inner payload is `base64url(subscriber).base64url(list_id).expiry`. The cipher
/// key is derived from the app signing key (`ResolvedSigningKeys`) via HMAC-SHA256
/// with a domain-separation label. AES-256-GCM provides both confidentiality and
/// authenticity: unlike a plain signed token the recipient address is **not**
/// recoverable from the URL (so it can't leak from proxy/browser/link-scanner
/// logs), and the GCM tag makes the token tamper-proof. Verification tries the
/// current key, then any rotation-grace `previous` keys. Stateless — no
/// server-side token storage.
pub mod unsubscribe {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    use crate::security::config::ResolvedSigningKeys;

    /// Default validity window for unsubscribe tokens, in days.
    pub const DEFAULT_TOKEN_TTL_DAYS: i64 = 30;

    const ENGINE: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    /// Token format version (first byte of the encrypted blob).
    const TOKEN_VERSION: u8 = 1;
    /// AES-GCM nonce length in bytes.
    const NONCE_LEN: usize = 12;
    /// Domain-separation label for deriving the token cipher key from a signing
    /// key, so it is independent of other uses of the signing secret.
    const KEY_CONTEXT: &[u8] = b"autumn:unsubscribe-token:v1";

    /// A verified unsubscribe request decoded from a signed token.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Unsubscribed {
        /// Opaque subscriber identifier (email address by default).
        pub subscriber: String,
        /// Logical list / suppression scope.
        pub list_id: String,
    }

    /// Reasons an unsubscribe token fails to verify.
    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    pub enum TokenError {
        /// Structure or encoding is invalid.
        #[error("unsubscribe token is malformed")]
        Malformed,
        /// Signature did not match any current or previous signing key.
        #[error("unsubscribe token signature is invalid")]
        BadSignature,
        /// Token is past its expiry.
        #[error("unsubscribe token has expired")]
        Expired,
    }

    /// Derive a 32-byte AES-256 key from a signing key via HMAC-SHA256 with a
    /// domain-separation label.
    #[allow(
        clippy::expect_used,
        reason = "infallible: HMAC accepts any key length"
    )]
    fn derive_key(signing_key: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(signing_key)
            .expect("HMAC accepts any key length");
        mac.update(KEY_CONTEXT);
        let bytes = mac.finalize().into_bytes();
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        key
    }

    /// The inner authenticated plaintext: `b64(subscriber).b64(list_id).expiry`.
    fn plaintext(subscriber: &str, list_id: &str, expiry_unix: i64) -> String {
        format!(
            "{}.{}.{expiry_unix}",
            ENGINE.encode(subscriber.as_bytes()),
            ENGINE.encode(list_id.as_bytes()),
        )
    }

    /// Mint an encrypted unsubscribe token valid until `expiry_unix`.
    ///
    /// The subscriber/list/expiry (seconds since epoch) are encrypted and
    /// authenticated with AES-256-GCM, so they are not recoverable from the URL.
    ///
    /// # Panics
    ///
    /// Panics if the OS RNG is unavailable.
    #[must_use]
    #[allow(
        clippy::expect_used,
        reason = "infallible crypto: AES-256-GCM over a 32-byte derived key; an OS RNG failure is an unrecoverable environment fault surfaced as a documented panic"
    )]
    pub fn sign_token(
        keys: &ResolvedSigningKeys,
        subscriber: &str,
        list_id: &str,
        expiry_unix: i64,
    ) -> String {
        let key = derive_key(&keys.current);
        let cipher = Aes256Gcm::new_from_slice(&key).expect("derived key is always 32 bytes");
        let mut nonce_bytes = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce_bytes).expect("OS RNG failed");
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                plaintext(subscriber, list_id, expiry_unix).as_bytes(),
            )
            .expect("AES-GCM encryption cannot fail for valid inputs");
        let mut blob = Vec::with_capacity(ciphertext.len().saturating_add(1 + NONCE_LEN));
        blob.push(TOKEN_VERSION);
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        ENGINE.encode(blob)
    }

    /// Verify a token and decode its subscriber/list, rejecting bad signatures
    /// and expired tokens.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] when the token is malformed, its signature is
    /// invalid, or it has expired relative to `now_unix`.
    #[allow(
        clippy::indexing_slicing,
        reason = "blob.len() is checked to be >= 1 + NONCE_LEN above, so these indices are in bounds"
    )]
    pub fn verify_token(
        keys: &ResolvedSigningKeys,
        token: &str,
        now_unix: i64,
    ) -> Result<Unsubscribed, TokenError> {
        let blob = ENGINE.decode(token).map_err(|_| TokenError::Malformed)?;
        if blob.len() < 1 + NONCE_LEN {
            return Err(TokenError::Malformed);
        }
        if blob[0] != TOKEN_VERSION {
            return Err(TokenError::Malformed);
        }
        let nonce = Nonce::from_slice(&blob[1..=NONCE_LEN]);
        let ciphertext = &blob[1 + NONCE_LEN..];
        // Try the current key first, then any rotation-grace `previous` keys. A
        // wrong key (or any tampering) fails AES-GCM authentication.
        let payload = std::iter::once(&keys.current)
            .chain(keys.previous.iter())
            .find_map(|signing_key| {
                let key = derive_key(signing_key);
                // The derived key is always 32 bytes, so construction never fails;
                // `.ok()?` keeps this panic-free regardless.
                let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
                cipher.decrypt(nonce, ciphertext).ok()
            })
            .ok_or(TokenError::BadSignature)?;
        let payload = String::from_utf8(payload).map_err(|_| TokenError::Malformed)?;
        let mut parts = payload.split('.');
        let subscriber_b64 = parts.next().ok_or(TokenError::Malformed)?;
        let list_b64 = parts.next().ok_or(TokenError::Malformed)?;
        let expiry_s = parts.next().ok_or(TokenError::Malformed)?;
        if parts.next().is_some() {
            return Err(TokenError::Malformed);
        }
        let expiry: i64 = expiry_s.parse().map_err(|_| TokenError::Malformed)?;
        if now_unix > expiry {
            return Err(TokenError::Expired);
        }
        let subscriber = decode_field(subscriber_b64)?;
        let list_id = decode_field(list_b64)?;
        Ok(Unsubscribed {
            subscriber,
            list_id,
        })
    }

    fn decode_field(encoded: &str) -> Result<String, TokenError> {
        let bytes = ENGINE.decode(encoded).map_err(|_| TokenError::Malformed)?;
        String::from_utf8(bytes).map_err(|_| TokenError::Malformed)
    }

    /// Build the one-click unsubscribe URL for `token` rooted at `base_url`.
    #[must_use]
    pub fn unsubscribe_url(base_url: &str, token: &str) -> String {
        format!(
            "{}{}?token={token}",
            base_url.trim_end_matches('/'),
            super::UNSUBSCRIBE_PATH,
        )
    }
}

/// Persistent record of recipients who unsubscribed from a logical list.
///
/// Implementors store one row per `(subscriber, list_id)` and answer
/// suppression queries at send time. Mirrors [`MailDeliveryQueue`]: register a
/// [`SuppressionStoreHandle`] on [`AppState`] (or let the framework auto-wire a
/// `db`-feature `DbSuppressionStore` backend) before `install_mailer` runs.
pub trait SuppressionStore: Send + Sync {
    /// Returns `true` when `subscriber` has unsubscribed from `list_id`.
    fn is_suppressed<'a>(
        &'a self,
        subscriber: &'a str,
        list_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>>;

    /// Returns the subset of `subscribers` that have unsubscribed from
    /// `list_id`.
    ///
    /// The list-mail send path calls this once per outgoing message instead
    /// of [`is_suppressed`](Self::is_suppressed) once per recipient, so a
    /// batch backend (like the `db`-feature `DbSuppressionStore`) can
    /// resolve the whole recipient list in a single round trip. The default
    /// implementation loops over `is_suppressed`, calling it in `subscribers`
    /// order and stopping at the first error — the same sequential behavior
    /// as before this method existed — so implementors that don't override
    /// it keep working unchanged.
    fn is_suppressed_many<'a>(
        &'a self,
        subscribers: &'a [&'a str],
        list_id: &'a str,
    ) -> Pin<
        Box<dyn Future<Output = Result<std::collections::HashSet<String>, MailError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut suppressed = std::collections::HashSet::new();
            for &subscriber in subscribers {
                if self.is_suppressed(subscriber, list_id).await? {
                    suppressed.insert(subscriber.to_owned());
                }
            }
            Ok(suppressed)
        })
    }

    /// Record that `subscriber` unsubscribed from `list_id` (idempotent).
    fn suppress<'a>(
        &'a self,
        subscriber: &'a str,
        list_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;
}

/// Cloneable handle to a [`SuppressionStore`] for storage on [`AppState`].
#[derive(Clone)]
pub struct SuppressionStoreHandle(Arc<dyn SuppressionStore>);

impl SuppressionStoreHandle {
    /// Wrap a store implementation.
    #[must_use]
    pub fn new(store: impl SuppressionStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    /// Wrap an already-shared store implementation.
    #[must_use]
    pub fn from_arc(store: Arc<dyn SuppressionStore>) -> Self {
        Self(store)
    }

    /// Borrow the inner store.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn SuppressionStore> {
        &self.0
    }
}

impl std::fmt::Debug for SuppressionStoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuppressionStoreHandle").finish()
    }
}

/// In-memory [`SuppressionStore`] for tests, review apps, and single-process dev.
///
/// State is process-local and lost on restart; use `DbSuppressionStore` in
/// production.
#[derive(Debug, Default, Clone)]
pub struct InMemorySuppressionStore {
    suppressed: Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
}

impl InMemorySuppressionStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SuppressionStore for InMemorySuppressionStore {
    fn is_suppressed<'a>(
        &'a self,
        subscriber: &'a str,
        list_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>> {
        Box::pin(async move {
            let key = (subscriber.to_owned(), list_id.to_owned());
            let suppressed = self
                .suppressed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&key);
            Ok(suppressed)
        })
    }

    fn suppress<'a>(
        &'a self,
        subscriber: &'a str,
        list_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
        Box::pin(async move {
            let key = (subscriber.to_owned(), list_id.to_owned());
            self.suppressed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key);
            Ok(())
        })
    }
}

/// Runtime wiring for List-Unsubscribe.
///
/// Holds where to point unsubscribe links, how to sign tokens, and where
/// suppression lives. Shared (via `Arc`) between the [`Mailer`] that signs
/// links and the endpoint that verifies them so tokens always validate within a
/// process.
pub struct UnsubscribeRuntime {
    /// Base URL for unsubscribe links (e.g. `https://app.example.com`).
    pub base_url: Option<String>,
    /// `mailto:` fallback address for the `List-Unsubscribe` header.
    pub mailto: Option<String>,
    /// Signing keys used for token HMACs.
    pub signing_keys: Arc<crate::security::config::ResolvedSigningKeys>,
    /// Token validity window, in days.
    pub ttl_days: i64,
    /// Suppression backend (absent in pure-header configurations).
    pub suppression: Option<Arc<dyn SuppressionStore>>,
}

impl std::fmt::Debug for UnsubscribeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnsubscribeRuntime")
            .field("base_url", &self.base_url)
            .field("mailto", &self.mailto)
            .field("ttl_days", &self.ttl_days)
            .field("has_suppression", &self.suppression.is_some())
            .finish_non_exhaustive()
    }
}

impl UnsubscribeRuntime {
    /// Build the `List-Unsubscribe` header value for `subscriber` on `list_id`:
    /// `<https://…?token=…>, <mailto:…>` per RFC 8058 §2. Returns `None` when
    /// neither a base URL nor a mailto is configured.
    #[must_use]
    pub fn list_unsubscribe_header(&self, subscriber: &str, list_id: &str) -> Option<String> {
        let mut entries: Vec<String> = Vec::new();
        if let Some(base) = self.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
            let expiry = current_unix_time().saturating_add(self.ttl_days.saturating_mul(86_400));
            let token = unsubscribe::sign_token(&self.signing_keys, subscriber, list_id, expiry);
            entries.push(format!("<{}>", unsubscribe::unsubscribe_url(base, &token)));
        }
        if let Some(mailto) = self.mailto.as_deref().filter(|s| !s.trim().is_empty()) {
            // Accept both a bare address and a full `mailto:` URI without
            // double-prefixing the scheme. Render only the bare mailbox (drop any
            // configured `?query`) before appending the canonical subject, so a
            // value like `mailto:u@x?subject=a\r\nBcc: v@x` can't inject extra
            // headers into the raw `List-Unsubscribe` value.
            let trimmed = mailto.trim();
            let address = trimmed.strip_prefix("mailto:").unwrap_or(trimmed);
            let address = address.split('?').next().unwrap_or(address);
            entries.push(format!("<mailto:{address}?subject=unsubscribe>"));
        }
        if entries.is_empty() {
            None
        } else {
            Some(entries.join(", "))
        }
    }

    /// Whether RFC 8058 one-click is available — i.e. an HTTPS unsubscribe URL is
    /// configured. `List-Unsubscribe-Post` is only valid alongside such a URL; a
    /// `mailto`-only configuration is a plain RFC 2369 unsubscribe, not one-click.
    #[must_use]
    pub fn supports_one_click(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
}

fn current_unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[derive(Debug, Clone, Default)]
struct MailerDefaults {
    from: Option<String>,
    reply_to: Option<String>,
}

/// Cloneable email sender. Extract it in handlers as `mailer: Mailer`.
#[derive(Clone)]
pub struct Mailer {
    defaults: Arc<MailerDefaults>,
    transport: Arc<dyn MailTransport>,
    delivery_queue: Option<Arc<dyn MailDeliveryQueue>>,
    unsubscribe: Option<Arc<UnsubscribeRuntime>>,
    /// Bounce/complaint suppression list consulted before transport. See
    /// [`suppression`]. `None` disables the check (suppression is opt-in on a
    /// hand-built [`Mailer`]; the framework wires a default in-memory store).
    suppression: Option<Arc<dyn suppression::SuppressionStore>>,
    /// Default for CSS inlining of HTML bodies when a message does not set its
    /// own [`Mail::inline_css`] override. Sourced from [`MailConfig::inline_css`].
    inline_css_default: bool,
    /// Set by `install_mailer` when running in `prod` with a non-disabled
    /// transport but no durable [`MailDeliveryQueue`] and no explicit
    /// [`MailConfig::allow_in_process_deliver_later_in_production`] ack
    /// (issue #2142). Rather than failing app startup for apps that never call
    /// `deliver_later`, the check is deferred to the first actual call.
    block_deliver_later_without_durable_queue: bool,
}

impl Mailer {
    /// Build a mailer manually.
    #[must_use]
    pub fn builder() -> MailerBuilder {
        MailerBuilder::default()
    }

    /// Build a mailer from resolved config.
    ///
    /// # Errors
    ///
    /// Returns an error when SMTP or address configuration is invalid.
    pub fn from_config(config: &MailConfig) -> Result<Self, MailError> {
        Self::from_config_inner(config, None)
    }

    pub(crate) fn from_config_inner(
        config: &MailConfig,
        resilience: Option<Arc<crate::config::ResilienceConfig>>,
    ) -> Result<Self, MailError> {
        let mut builder = Self::builder()
            .transport(config.transport)
            .inline_css(config.inline_css)
            .resilience_config(resilience);
        if let Some(from) = &config.from {
            builder = builder.from(from.clone());
        }
        if let Some(reply_to) = &config.reply_to {
            builder = builder.reply_to(reply_to.clone());
        }
        if config.transport == Transport::File {
            builder = builder.file_dir(config.file_dir.clone());
        }
        if config.transport == Transport::Smtp {
            builder = builder.smtp(config.smtp.clone());
        }
        builder.build()
    }

    /// Build a mailer from any custom transport.
    #[must_use]
    pub fn with_transport(transport: impl MailTransport + 'static) -> Self {
        Self {
            defaults: Arc::new(MailerDefaults::default()),
            transport: Arc::new(transport),
            delivery_queue: None,
            unsubscribe: None,
            suppression: None,
            inline_css_default: false,
            block_deliver_later_without_durable_queue: false,
        }
    }

    /// Attach a durable [`MailDeliveryQueue`] used by [`Self::deliver_later`].
    #[must_use]
    pub fn with_delivery_queue(mut self, queue: impl MailDeliveryQueue + 'static) -> Self {
        self.delivery_queue = Some(Arc::new(queue));
        self
    }

    /// Attach the List-Unsubscribe runtime used to sign links, emit RFC 8058
    /// headers, and skip suppressed recipients.
    #[must_use]
    pub fn with_unsubscribe(mut self, runtime: Arc<UnsubscribeRuntime>) -> Self {
        self.unsubscribe = Some(runtime);
        self
    }

    /// Attach the bounce/complaint [`suppression`] list consulted before
    /// transport. Recipients on the list are skipped (and the skip is logged +
    /// counted) unless the message opts out via
    /// [`MailBuilder::ignore_suppression`].
    #[must_use]
    pub fn with_suppression(mut self, store: suppression::SuppressionStoreHandle) -> Self {
        self.suppression = Some(store.into_inner());
        self
    }

    /// Returns whether a durable [`MailDeliveryQueue`] is attached.
    #[must_use]
    pub fn has_durable_delivery_queue(&self) -> bool {
        self.delivery_queue.is_some()
    }

    /// Returns `true` when the active transport is intentionally a no-op
    /// (i.e. `transport = "disabled"` in `autumn.toml`).
    ///
    /// Handlers that require mail (e.g. forgot-password) can guard against
    /// silently dropped messages by checking this before attempting to send.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.transport.is_disabled()
    }

    /// Send mail immediately.
    ///
    /// Before transport, recipients on the bounce/complaint [`suppression`] list
    /// (hard bounce or complaint) are skipped — each skip emits a structured
    /// `outcome = "skipped_suppressed"` log line and increments
    /// [`suppression::suppressed_skips`]. When **every** recipient is suppressed,
    /// returns [`MailError::AllRecipientsSuppressed`] rather than reporting a
    /// phantom success. A message built with
    /// [`Mail::ignore_suppression`](MailBuilder::ignore_suppression) bypasses
    /// this check entirely (critical mail).
    ///
    /// When the message carries a [`list_unsubscribe`](Mail::list_unsubscribe)
    /// scope and a [`UnsubscribeRuntime`] is attached, recipients with a
    /// matching suppression row are skipped (with a structured log event) and
    /// every delivered message gains RFC 8058 `List-Unsubscribe` /
    /// `List-Unsubscribe-Post` headers scoped to the recipient. Such messages
    /// are delivered one recipient at a time so each unsubscribe link is
    /// personalized.
    ///
    /// # Errors
    ///
    /// Returns [`MailError::AllRecipientsSuppressed`] when every recipient is
    /// suppressed, an error from the selected transport, or from the suppression
    /// store when a suppression check fails.
    pub async fn send(&self, mail: Mail) -> Result<(), MailError> {
        let mut mail = mail.with_defaults(&self.defaults);

        // Inline `<style>` CSS into element `style="…"` attributes before
        // transport, so every transport (SMTP, file, log, preview) delivers the
        // inlined body and it renders styled in clients that strip
        // `<head>`/`<style>`. Doing it here — ahead of the list-mail branch that
        // clones per recipient — inlines exactly once regardless of path.
        self.apply_css_inlining(&mut mail)?;

        // Consult the bounce/complaint suppression list *before* transport.
        // Suppressed recipients are dropped from `to` (skipped, not an error)
        // unless the message opts out via `Mail::ignore_suppression`. When every
        // recipient is suppressed we return `AllRecipientsSuppressed` rather than
        // reporting a phantom success.
        if !mail.ignore_suppression
            && let Some(store) = self.suppression.as_ref()
            && !mail.to.is_empty()
        {
            let mut kept: Vec<String> = Vec::with_capacity(mail.to.len());
            for recipient in &mail.to {
                // The store canonicalizes internally, so pass the raw recipient
                // and only canonicalize on the (rare) suppressed path for the
                // log line — no allocation for delivered recipients.
                if store.is_suppressed(recipient).await? {
                    suppression::note_skip(&canonical_subscriber(recipient));
                } else {
                    kept.push(recipient.clone());
                }
            }
            if kept.is_empty() {
                return Err(MailError::AllRecipientsSuppressed);
            }
            mail.to = kept;
        }

        if let Some(list_id) = mail.list_unsubscribe.clone() {
            if let Some(runtime) = self.unsubscribe.clone() {
                return self.send_list_mail(mail, list_id, &runtime).await;
            }
            // Opted into a list (e.g. via MailBuilder::list_unsubscribe) but no
            // unsubscribe runtime is wired — send without headers/suppression,
            // but make the compliance gap loud rather than silent.
            tracing::warn!(
                target: "mail",
                list_id = %list_id,
                "sending list mail without an unsubscribe runtime: no List-Unsubscribe headers or suppression applied (set mail.unsubscribe_base_url / mail.unsubscribe_mailto)"
            );
        }
        self.transport.send(mail).await
    }

    /// Resolve the CSS-inlining decision for a message and, when enabled, inline
    /// its HTML body in place (issue #1254).
    ///
    /// Precedence: a per-message [`Mail::inline_css`] override wins; otherwise
    /// the [`Mailer`]'s configured [`MailConfig::inline_css`] default applies.
    /// On inliner failure `send` fails loudly: a typed [`MailError::CssInline`]
    /// is returned and the message is not delivered, rather than shipping a
    /// corrupted body. Text bodies are never touched.
    fn apply_css_inlining(&self, mail: &mut Mail) -> Result<(), MailError> {
        let enabled = mail.inline_css.unwrap_or(self.inline_css_default);
        if !enabled {
            return Ok(());
        }
        let Some(html) = mail.html.as_deref() else {
            return Ok(());
        };
        match inline_css_html(html) {
            Ok(inlined) => {
                mail.html = Some(inlined);
                Ok(())
            }
            Err(error) => {
                // Leave `mail.html` as the original body (not corrupted) and make
                // the failure loud rather than silently shipping broken HTML.
                tracing::warn!(
                    target: "mail",
                    error = %error,
                    "CSS inlining failed; HTML body left un-inlined"
                );
                Err(error)
            }
        }
    }

    /// Freeze this mailer's CSS-inlining default onto a message before it is
    /// handed to a durable [`MailDeliveryQueue`] for deferred delivery (issue
    /// #1254).
    ///
    /// A worker that later dequeues the persisted job resolves
    /// [`Mail::inline_css`] against ITS OWN mailer's default via
    /// [`apply_css_inlining`](Self::apply_css_inlining), which may differ from
    /// (or be off by default relative to) the originating mailer. Recording the
    /// originating decision here makes the persisted job self-describing, so
    /// deferred mail inlines consistently with an immediate send. Only `None`
    /// is resolved — explicit `Some(true)`/`Some(false)` per-message overrides
    /// are preserved. The body itself is left un-inlined so the single inline
    /// pass still happens once at the consumer's `send()`, keeping delivery
    /// idempotent and avoiding a bloated persisted body.
    const fn freeze_inline_css_default(&self, mail: &mut Mail) {
        if mail.inline_css.is_none() {
            mail.inline_css = Some(self.inline_css_default);
        }
    }

    /// Deliver a list mail recipient-by-recipient, applying suppression and
    /// per-recipient RFC 8058 headers.
    async fn send_list_mail(
        &self,
        mail: Mail,
        list_id: String,
        runtime: &UnsubscribeRuntime,
    ) -> Result<(), MailError> {
        // Resolve every recipient — address validity AND suppression decision —
        // before delivering anything. The delivery loop below sends one message
        // per recipient; if validation or a suppression-store lookup failed
        // mid-loop it could deliver to earlier recipients and then return an
        // error, so a caller retrying the send would duplicate those earlier
        // deliveries. Non-list mail builds and validates the full message before
        // any send — match that atomicity here.
        //
        // Each entry is `(recipient_display, canonical_subscriber)`. The canonical
        // bare address is used for the suppression / token key so a formatted
        // `Ada <ada@example.com>` recipient matches an opt-out recorded as
        // `ada@example.com`; the display string is preserved for actual delivery.
        //
        // Validate every recipient's address format first (in order, so a
        // malformed address still fails fast the same way it always has),
        // then resolve suppression for the whole batch in one call instead of
        // one store round trip per recipient — `is_suppressed_many` is the
        // only DB-backed lookup in this function's hot path, and it used to
        // scale linearly with the recipient count.
        let mut candidates: Vec<(String, String)> = Vec::with_capacity(mail.to.len());
        for recipient in &mail.to {
            parse_mailbox(recipient)?;
            candidates.push((recipient.clone(), canonical_subscriber(recipient)));
        }

        let suppressed = if let Some(store) = runtime.suppression.as_ref() {
            let subscribers: Vec<&str> = candidates
                .iter()
                .map(|(_, subscriber)| subscriber.as_str())
                .collect();
            store.is_suppressed_many(&subscribers, &list_id).await?
        } else {
            std::collections::HashSet::new()
        };

        let mut deliveries: Vec<(String, String)> = Vec::with_capacity(candidates.len());
        for (recipient, subscriber) in candidates {
            if suppressed.contains(&subscriber) {
                tracing::info!(
                    target: "mail",
                    list_id = %list_id,
                    outcome = "skipped_suppressed",
                    "skipping suppressed list-unsubscribe recipient"
                );
                continue;
            }
            deliveries.push((recipient, subscriber));
        }

        for (recipient, subscriber) in deliveries {
            let mut per_recipient = mail.clone();
            per_recipient.to = vec![recipient];
            if let Some(value) = runtime.list_unsubscribe_header(&subscriber, &list_id) {
                // A migration to `#[mailer(list_unsubscribe)]` replaces, not
                // duplicates, any header the template set by hand: drop an existing
                // List-Unsubscribe / List-Unsubscribe-Post first so the generated
                // per-recipient one-click header is authoritative (otherwise a
                // stale manual header would suppress RFC 8058 compliance).
                per_recipient.extra_headers.retain(|(name, _)| {
                    !name.eq_ignore_ascii_case("List-Unsubscribe")
                        && !name.eq_ignore_ascii_case("List-Unsubscribe-Post")
                });
                per_recipient
                    .extra_headers
                    .push(("List-Unsubscribe".to_owned(), value));
                // `List-Unsubscribe-Post` is only valid with an HTTPS one-click
                // URL; a mailto-only header is plain RFC 2369.
                if runtime.supports_one_click() {
                    per_recipient.extra_headers.push((
                        "List-Unsubscribe-Post".to_owned(),
                        "List-Unsubscribe=One-Click".to_owned(),
                    ));
                }
            }
            self.transport.send(per_recipient).await?;
        }
        Ok(())
    }

    /// Queue mail for later delivery.
    ///
    /// When called **inside a [`Db::tx`](autumn_web::db::Db::tx) block**, the
    /// delivery is automatically deferred until the transaction commits. On
    /// rollback the mail is silently dropped — no orphaned sends.
    ///
    /// This deferral is process-local. It prevents mail for rolled-back writes,
    /// but it does not make the post-commit mail handoff crash-safe unless the
    /// configured [`MailDeliveryQueue`] records a durable outbox/queue entry.
    ///
    /// When called outside any active transaction the behaviour is unchanged:
    /// the mail is dispatched in a background Tokio task immediately.
    ///
    /// Use [`deliver_later_eager`](Self::deliver_later_eager) when you need the
    /// mail to fire regardless of whether the surrounding transaction commits
    /// (e.g. security alerts that must go out on any code path).
    pub fn deliver_later(&self, mail: Mail) {
        if let Err(error) = self.try_deliver_later(mail) {
            tracing::error!(error = %error, "background mail delivery was not scheduled");
        }
    }

    /// Queue mail for later delivery, **bypassing any active transaction**.
    ///
    /// Unlike [`deliver_later`](Self::deliver_later), this method always
    /// spawns the delivery immediately — it does not check for an active
    /// `db.tx` block. Use this when the mail must be sent even if the
    /// surrounding transaction rolls back (e.g. "someone tried to log in"
    /// security alerts, rate-limit notices).
    pub fn deliver_later_eager(&self, mail: Mail) {
        if let Err(error) = self.try_deliver_later_eager(mail) {
            tracing::error!(error = %error, "background mail delivery was not scheduled");
        }
    }

    /// Queue mail for later delivery, deferring when inside a `db.tx`.
    ///
    /// # Errors
    ///
    /// Returns [`MailError::NoDurableQueueInProduction`] when this `Mailer` was
    /// installed in `prod` with no durable [`MailDeliveryQueue`] and no
    /// [`MailConfig::allow_in_process_deliver_later_in_production`] opt-in (issue
    /// #2142). Otherwise returns an error when no active Tokio runtime is
    /// available to host the background task.
    ///
    /// # Panics
    ///
    /// Panics if the internal after-commit registry mutex is poisoned.
    pub fn try_deliver_later(&self, mail: Mail) -> Result<(), MailError> {
        if self.transport.is_disabled() {
            return Ok(());
        }
        if self.block_deliver_later_without_durable_queue && self.delivery_queue.is_none() {
            return Err(MailError::NoDurableQueueInProduction);
        }
        let mut mail = mail.with_defaults(&self.defaults);
        // Resolve the CSS-inlining default onto the message once, at the top of
        // the deferred path, so BOTH the durable-queue branch (persisted for a
        // possibly-different worker to consume) and the in-process fallback
        // branch carry the originating mailer's decision. Only `None` is frozen;
        // explicit per-message overrides are preserved (issue #1254).
        self.freeze_inline_css_default(&mut mail);

        // When inside a db.tx, push the spawn as an after-commit callback so
        // the mail only fires if the transaction commits successfully.
        #[cfg(feature = "db")]
        {
            let mailer = self.clone();
            let deferred = mail.clone();
            let mut f_opt: Option<(Self, Mail)> = Some((mailer, deferred));
            // Capture the caller's span now; the after-commit callback runs in a
            // fresh task with no request span, so spawn_mail_delivery would see an
            // empty span and lose trace correlation without this.
            let deliver_span = tracing::Span::current();

            crate::db::AFTER_COMMIT_REGISTRY
                .try_with(|registry| {
                    #[allow(
                        clippy::expect_used,
                        reason = "unreachable: try_with closure body runs at most once"
                    )]
                    let (m, m_mail) = f_opt.take().expect("once");
                    let span = deliver_span.clone();
                    let boxed: crate::db::CommitCallback = Box::new(move || {
                        Box::pin(tracing::Instrument::instrument(
                            async move {
                                if let Some(queue) = m.delivery_queue.clone() {
                                    queue.enqueue(m_mail).await.map_err(|e| {
                                        crate::AutumnError::internal_server_error_msg(e.to_string())
                                    })
                                } else {
                                    m.spawn_mail_delivery(m_mail).map_err(|e| {
                                        crate::AutumnError::internal_server_error_msg(e.to_string())
                                    })
                                }
                            },
                            span,
                        ))
                    });
                    registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(boxed);
                })
                .ok();

            if f_opt.is_none() {
                // Successfully registered for after-commit; skip the eager spawn.
                return Ok(());
            }
        }

        // Outside a transaction (or `db` feature not enabled) — spawn immediately.
        self.spawn_mail_delivery(mail)
    }

    /// Queue mail for later delivery, always spawning immediately.
    ///
    /// # Errors
    ///
    /// Returns [`MailError::NoDurableQueueInProduction`] when this `Mailer` was
    /// installed in `prod` with no durable [`MailDeliveryQueue`] and no
    /// [`MailConfig::allow_in_process_deliver_later_in_production`] opt-in (issue
    /// #2142). Otherwise returns an error when no active Tokio runtime is
    /// available.
    pub fn try_deliver_later_eager(&self, mail: Mail) -> Result<(), MailError> {
        if self.transport.is_disabled() {
            return Ok(());
        }
        if self.block_deliver_later_without_durable_queue && self.delivery_queue.is_none() {
            return Err(MailError::NoDurableQueueInProduction);
        }
        let mut mail = mail.with_defaults(&self.defaults);
        self.freeze_inline_css_default(&mut mail);
        self.spawn_mail_delivery(mail)
    }

    fn spawn_mail_delivery(&self, mail: Mail) -> Result<(), MailError> {
        // Honor the disabled-transport contract: if the operator turned mail off
        // for this profile, deliver_later must drop the message just like
        // immediate `send` does — even when a queue is attached.
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            MailError::RuntimeUnavailable(
                "deliver_later requires an active Tokio runtime".to_owned(),
            )
        })?;
        let parent_span = tracing::Span::current();
        if let Some(queue) = self.delivery_queue.clone() {
            handle.spawn(tracing::Instrument::instrument(
                async move {
                    if let Err(error) = queue.enqueue(mail).await {
                        tracing::error!(error = %error, "durable mail enqueue failed");
                    }
                },
                parent_span,
            ));
        } else {
            let mailer = self.clone();
            handle.spawn(tracing::Instrument::instrument(
                async move {
                    if let Err(error) = mailer.send(mail).await {
                        tracing::error!(error = %error, "background mail delivery failed");
                    }
                },
                parent_span,
            ));
        }
        Ok(())
    }
}

impl FromRequestParts<AppState> for Mailer {
    type Rejection = AutumnError;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        state
            .extension::<Self>()
            .as_deref()
            .cloned()
            .ok_or_else(|| AutumnError::service_unavailable_msg("Mailer is not configured"))
    }
}

/// Builder for [`Mailer`].
#[derive(Clone)]
pub struct MailerBuilder {
    transport: Transport,
    from: Option<String>,
    reply_to: Option<String>,
    file_dir: PathBuf,
    smtp: Option<SmtpConfig>,
    delivery_queue: Option<Arc<dyn MailDeliveryQueue>>,
    resilience_config: Option<Arc<crate::config::ResilienceConfig>>,
    inline_css: bool,
}

impl Default for MailerBuilder {
    fn default() -> Self {
        Self {
            transport: Transport::Log,
            from: None,
            reply_to: None,
            file_dir: default_file_dir(),
            smtp: None,
            delivery_queue: None,
            resilience_config: None,
            inline_css: false,
        }
    }
}

impl MailerBuilder {
    /// Select the transport.
    #[must_use]
    pub const fn transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Set default From header.
    #[must_use]
    pub fn from(mut self, from: impl Into<String>) -> Self {
        self.from = Some(from.into());
        self
    }

    /// Set default Reply-To header.
    #[must_use]
    pub fn reply_to(mut self, reply_to: impl Into<String>) -> Self {
        self.reply_to = Some(reply_to.into());
        self
    }

    /// Set file output directory.
    #[must_use]
    pub fn file_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.file_dir = dir.as_ref().to_path_buf();
        self
    }

    /// Set SMTP config.
    #[must_use]
    pub fn smtp(mut self, smtp: SmtpConfig) -> Self {
        self.smtp = Some(smtp);
        self
    }

    /// Attach a durable [`MailDeliveryQueue`] used by
    /// [`Mailer::deliver_later`].
    #[must_use]
    pub fn delivery_queue(mut self, queue: impl MailDeliveryQueue + 'static) -> Self {
        self.delivery_queue = Some(Arc::new(queue));
        self
    }

    /// Attach an already-shared durable [`MailDeliveryQueue`].
    #[must_use]
    pub fn delivery_queue_arc(mut self, queue: Arc<dyn MailDeliveryQueue>) -> Self {
        self.delivery_queue = Some(queue);
        self
    }

    #[must_use]
    pub fn resilience_config(mut self, rc: Option<Arc<crate::config::ResilienceConfig>>) -> Self {
        self.resilience_config = rc;
        self
    }

    /// Set the default for CSS inlining of HTML bodies (issue #1254). Applied to
    /// every message that does not carry its own [`MailBuilder::inline_css`]
    /// override. Mirrors [`MailConfig::inline_css`].
    #[must_use]
    pub const fn inline_css(mut self, enabled: bool) -> Self {
        self.inline_css = enabled;
        self
    }

    /// Build the mailer.
    ///
    /// # Errors
    ///
    /// Returns an error when the SMTP transport or default addresses cannot be configured.
    pub fn build(self) -> Result<Mailer, MailError> {
        if let Some(from) = &self.from {
            parse_mailbox(from)?;
        }
        if let Some(reply_to) = &self.reply_to {
            parse_mailbox(reply_to)?;
        }

        let transport: Arc<dyn MailTransport> = match self.transport {
            Transport::Log => Arc::new(LogTransport),
            Transport::File => Arc::new(FileTransport { dir: self.file_dir }),
            Transport::Disabled => Arc::new(DisabledTransport),
            Transport::Smtp => Arc::new(SmtpTransport::new(
                self.smtp.unwrap_or_default(),
                self.resilience_config.clone(),
            )?),
        };

        Ok(Mailer {
            defaults: Arc::new(MailerDefaults {
                from: self.from,
                reply_to: self.reply_to,
            }),
            transport,
            delivery_queue: self.delivery_queue,
            unsubscribe: None,
            suppression: None,
            inline_css_default: self.inline_css,
            block_deliver_later_without_durable_queue: false,
        })
    }
}

struct DisabledTransport;

impl MailTransport for DisabledTransport {
    fn send<'a>(
        &'a self,
        _mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn is_disabled(&self) -> bool {
        true
    }
}

struct LogTransport;

impl MailTransport for LogTransport {
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!(
                from = ?mail.from,
                reply_to = ?mail.reply_to,
                to = ?mail.to,
                subject = %mail.subject,
                text = ?mail.text,
                html = ?mail.html,
                attachments = mail.attachments.len(),
                "mail captured by log transport"
            );
            Ok(())
        })
    }
}

struct FileTransport {
    dir: PathBuf,
}

static FILE_TRANSPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl MailTransport for FileTransport {
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.dir).await?;
            let filename = file_transport_filename(&mail);
            let path = self.dir.join(filename);
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await?;
            let eml = render_eml(&mail);
            tokio::io::AsyncWriteExt::write_all(&mut file, eml.as_bytes()).await?;
            tokio::io::AsyncWriteExt::flush(&mut file).await?;
            file.sync_all().await?;
            Ok(())
        })
    }
}

struct SmtpTransport {
    inner: AsyncSmtpTransport<Tokio1Executor>,
    resilience_config: Option<Arc<crate::config::ResilienceConfig>>,
}

impl SmtpTransport {
    fn new(
        config: SmtpConfig,
        resilience_config: Option<Arc<crate::config::ResilienceConfig>>,
    ) -> Result<Self, MailError> {
        let host = config
            .host
            .filter(|host| !host.trim().is_empty())
            .ok_or_else(|| MailError::InvalidMessage("mail.smtp.host is required".to_owned()))?;
        let mut builder = match config.tls {
            TlsMode::Disabled => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host),
            TlsMode::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?,
            TlsMode::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?,
        };
        if let Some(port) = config.port {
            builder = builder.port(port);
        }
        if let Some(username) = config.username {
            let password_env = config.password_env.ok_or_else(|| {
                MailError::InvalidMessage(
                    "mail.smtp.password_env is required when mail.smtp.username is set".to_owned(),
                )
            })?;
            let password = std::env::var(&password_env)
                .map_err(|error| smtp_password_env_error(&password_env, &error))?;
            builder = builder.credentials(Credentials::new(username, password));
        }
        Ok(Self {
            inner: builder.build(),
            resilience_config,
        })
    }
}

/// Builds the startup error for a failed SMTP password lookup without ever
/// embedding the environment variable's *value*: [`std::env::VarError`]'s
/// `NotUnicode` variant carries the raw contents of the variable — the SMTP
/// password itself — in both its `Display` and `Debug` output, so the error
/// kind is mapped to a static description instead of being formatted. The
/// variable *name* is ordinary configuration and is kept for diagnostics.
fn smtp_password_env_error(password_env: &str, error: &std::env::VarError) -> MailError {
    let reason = match error {
        std::env::VarError::NotPresent => "environment variable is not set",
        std::env::VarError::NotUnicode(_) => "environment variable contains non-unicode data",
    };
    MailError::InvalidMessage(format!(
        "mail.smtp.password_env={password_env:?} could not be resolved: {reason}"
    ))
}

impl MailTransport for SmtpTransport {
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
        Box::pin(async move {
            let breaker = self.resilience_config.as_ref().map_or_else(
                || {
                    crate::circuit_breaker::global_registry().get_or_create(
                        "smtp_mailer",
                        crate::circuit_breaker::CircuitBreakerPolicy::default(),
                    )
                },
                |rc| {
                    let policy = crate::circuit_breaker::CircuitBreakerPolicy::from_config(
                        rc,
                        "smtp_mailer",
                    );
                    crate::circuit_breaker::global_registry()
                        .get_or_create_with_config("smtp_mailer", policy)
                },
            );

            if breaker.before_call().is_err() {
                return Err(MailError::RuntimeUnavailable(
                    "smtp mailer circuit breaker is open".to_owned(),
                ));
            }
            let guard = crate::circuit_breaker::CircuitBreakerGuard::new(breaker.clone());

            let message = lettre_message(&mail)?;
            let res = self.inner.send(message).await;
            if res.is_ok() {
                guard.success();
            } else {
                guard.failure();
            }

            res.map(|_| ()).map_err(Into::into)
        })
    }
}

fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// RFC 2231 `attr-char`: alphanumerics plus these ASCII punctuation marks may
/// appear unescaped in an extended parameter value; everything else
/// (including all non-ASCII and control bytes) is percent-encoded.
const RFC2231_ATTR_CHAR: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// Strips CR/LF and other ASCII/Unicode control characters from a header
/// value written by the hand-rolled `.eml` renderer, so untrusted `Mail`
/// field content (which may arrive via `Deserialize` from a durable queue,
/// bypassing [`MailBuilder::build`]'s validation) can never inject an extra
/// header line.
fn strip_header_controls(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

fn quote_header_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Builds the `Content-Disposition: attachment; …` parameter section for the
/// hand-rolled `.eml` renderer. Always ASCII and CR/LF-free by construction:
/// control characters are stripped, and non-ASCII filenames are RFC 2231
/// percent-encoded (`filename*=UTF-8''…`) alongside an ASCII fallback
/// `filename="…"` for readers that don't understand extended parameters.
fn content_disposition_params(filename: &str) -> String {
    let mut clean = strip_header_controls(filename);
    if clean.trim().is_empty() {
        "attachment".clone_into(&mut clean);
    }
    if clean.is_ascii() {
        format!("filename={}", quote_header_value(&clean))
    } else {
        let fallback: String = clean
            .chars()
            .map(|ch| if ch.is_ascii() { ch } else { '_' })
            .collect();
        let encoded = percent_encoding::utf8_percent_encode(&clean, RFC2231_ATTR_CHAR);
        format!(
            "filename={}; filename*=UTF-8''{encoded}",
            quote_header_value(&fallback)
        )
    }
}

/// Base64-encodes `bytes` and hard-wraps at 76 columns per RFC 2045.
fn base64_wrap76(bytes: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    if encoded.len() <= 76 {
        return encoded;
    }
    // One newline between each pair of 76-column chunks.
    let newlines = encoded.len().div_ceil(76).saturating_sub(1);
    let mut wrapped = String::with_capacity(encoded.len().saturating_add(newlines));
    for chunk in encoded.as_bytes().chunks(76) {
        if !wrapped.is_empty() {
            wrapped.push('\n');
        }
        #[allow(
            clippy::expect_used,
            reason = "infallible: base64 output is always ASCII"
        )]
        let chunk_str = std::str::from_utf8(chunk).expect("base64 output is always ASCII");
        wrapped.push_str(chunk_str);
    }
    wrapped
}

fn file_transport_filename(mail: &Mail) -> String {
    let sequence = FILE_TRANSPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}-{:016x}-{}.eml",
        chrono::Utc::now().format("%Y%m%d%H%M%S%6f"),
        std::process::id(),
        sequence,
        sanitize_filename(mail.to.first().map_or("unknown", String::as_str))
    )
}

fn render_eml(mail: &Mail) -> String {
    let mut out = String::new();
    if let Some(from) = &mail.from {
        out.push_str("From: ");
        out.push_str(&strip_header_controls(from));
        out.push('\n');
    }
    for to in &mail.to {
        out.push_str("To: ");
        out.push_str(&strip_header_controls(to));
        out.push('\n');
    }
    if let Some(reply_to) = &mail.reply_to {
        out.push_str("Reply-To: ");
        out.push_str(&strip_header_controls(reply_to));
        out.push('\n');
    }
    out.push_str("Date: ");
    out.push_str(&chrono::Utc::now().to_rfc2822());
    out.push('\n');
    out.push_str("Message-Id: <");
    out.push_str(&uuid::Uuid::new_v4().to_string());
    out.push_str("@autumn.local>\n");
    out.push_str("Subject: ");
    out.push_str(&strip_header_controls(&mail.subject));
    out.push('\n');
    for (name, value) in &mail.extra_headers {
        out.push_str(&strip_header_controls(name));
        out.push_str(": ");
        out.push_str(&strip_header_controls(value));
        out.push('\n');
    }
    out.push_str("MIME-Version: 1.0\n");
    if mail.attachments.is_empty() {
        render_eml_bodies(mail, &mut out);
    } else {
        // Random per-message boundary: text/html bodies are caller-controlled
        // and may legitimately contain a line matching a fixed boundary
        // (e.g. `--autumn-mixed`), which would truncate or split the
        // rendered MIME structure. A boundary the caller cannot predict in
        // advance can't collide with body content.
        use std::fmt::Write as _;
        let boundary = format!("autumn-mixed-{}", uuid::Uuid::new_v4().simple());
        let _ = write!(
            out,
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\n\n"
        );
        let _ = writeln!(out, "--{boundary}");
        render_eml_bodies(mail, &mut out);
        for attachment in &mail.attachments {
            let _ = writeln!(out, "--{boundary}");
            out.push_str("Content-Type: ");
            let content_type = strip_header_controls(&attachment.content_type);
            if ContentType::parse(&content_type).is_ok() {
                out.push_str(&content_type);
            } else {
                out.push_str("application/octet-stream");
            }
            out.push('\n');
            out.push_str("Content-Disposition: attachment; ");
            out.push_str(&content_disposition_params(&attachment.filename));
            out.push('\n');
            out.push_str("Content-Transfer-Encoding: base64\n\n");
            out.push_str(&base64_wrap76(&attachment.bytes));
            out.push('\n');
        }
        let _ = writeln!(out, "--{boundary}--");
    }
    out
}

/// Renders the html/text body part(s) of an `.eml` message — everything
/// after the `MIME-Version` header, before any `multipart/mixed` attachment
/// wrapper. Pulled out of [`render_eml`] so the attachment-less code path is
/// provably byte-identical to what it was before attachments existed.
fn render_eml_bodies(mail: &Mail, out: &mut String) {
    if mail.html.is_some() && mail.text.is_some() {
        out.push_str("Content-Type: multipart/alternative; boundary=\"autumn-mail\"\n\n");
        if let Some(text) = &mail.text {
            out.push_str("--autumn-mail\nContent-Type: text/plain; charset=utf-8\n\n");
            out.push_str(text);
            out.push('\n');
        }
        if let Some(html) = &mail.html {
            out.push_str("--autumn-mail\nContent-Type: text/html; charset=utf-8\n\n");
            out.push_str(html);
            out.push('\n');
        }
        out.push_str("--autumn-mail--\n");
    } else if let Some(html) = &mail.html {
        out.push_str("Content-Type: text/html; charset=utf-8\n\n");
        out.push_str(html);
        out.push('\n');
    } else if let Some(text) = &mail.text {
        out.push_str("Content-Type: text/plain; charset=utf-8\n\n");
        out.push_str(text);
        out.push('\n');
    }
}

#[derive(Debug, Clone)]
struct ParsedMail {
    headers: Vec<(String, String)>,
    to: Vec<String>,
    subject: String,
    date: Option<String>,
    html: Option<String>,
    text: Option<String>,
    attachments: Vec<ParsedAttachment>,
    raw: String,
}

impl ParsedMail {
    fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// An attachment as surfaced by the dev mail preview: just enough to list it
/// (filename, declared content type) without decoding its body.
#[derive(Debug, Clone)]
struct ParsedAttachment {
    filename: String,
    content_type: String,
}

#[derive(Debug, Clone)]
struct CapturedMailSummary {
    id: String,
    to: Vec<String>,
    subject: String,
    timestamp: String,
    modified: SystemTime,
}

pub(crate) fn mail_preview_router<S>(file_dir: PathBuf) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    AppState: axum::extract::FromRef<S>,
{
    let file_dir = Arc::new(file_dir);
    axum::Router::new()
        .route(
            MAIL_PREVIEW_PATH,
            axum::routing::get({
                let file_dir = Arc::clone(&file_dir);
                move |axum::extract::State(state): axum::extract::State<AppState>| {
                    let file_dir = Arc::clone(&file_dir);
                    async move { list_mail_preview(file_dir, state).await }
                }
            }),
        )
        .route(
            MAIL_PREVIEW_MESSAGE_PATH,
            axum::routing::get({
                let file_dir = Arc::clone(&file_dir);
                move |axum::extract::Path(message_id): axum::extract::Path<String>| {
                    let file_dir = Arc::clone(&file_dir);
                    async move { show_captured_mail(file_dir, message_id).await }
                }
            }),
        )
        .route(
            MAIL_PREVIEW_TEMPLATE_PATH,
            axum::routing::get(
                |axum::extract::Path((mailer, method)): axum::extract::Path<(String, String)>,
                 axum::extract::State(state): axum::extract::State<AppState>| async move {
                    show_template_preview(&state, &mailer, &method)
                },
            ),
        )
}

async fn list_mail_preview(file_dir: Arc<PathBuf>, state: AppState) -> Response {
    match captured_messages(&file_dir).await {
        Ok(messages) => {
            let previews = state
                .extension::<MailPreviewRegistry>()
                .map(|registry| registry.previews().to_vec())
                .unwrap_or_default();
            html_response(render_mail_index(&messages, &previews, &file_dir))
        }
        Err(error) => preview_error_response(&error),
    }
}

async fn show_captured_mail(file_dir: Arc<PathBuf>, message_id: String) -> Response {
    match read_captured_message(&file_dir, &message_id).await {
        Ok(parsed) => html_response(render_mail_detail(&parsed, "Captured message")),
        Err(error) => preview_error_response(&error),
    }
}

fn show_template_preview(state: &AppState, mailer: &str, method: &str) -> Response {
    let preview = state
        .extension::<MailPreviewRegistry>()
        .and_then(|registry| registry.find(mailer, method));
    let Some(preview) = preview else {
        return preview_error_response(&MailPreviewError::NotFound(format!("{mailer}/{method}")));
    };

    match preview.render() {
        Ok(mail) => {
            let mut mail = apply_preview_unsubscribe_headers(state, mailer, mail);
            // Match Mailer::send: inline <style> CSS so the preview reflects what
            // strict clients (Gmail/Outlook) actually receive. Reuses the send-time
            // decision (per-message override vs. the mailer's inline_css_default).
            if let Some(m) = state.extension::<Mailer>() {
                // Dev preview: degrade gracefully on inliner error (leaves html un-inlined)
                // rather than failing the preview; the inliner is effectively infallible here.
                let _ = m.apply_css_inlining(&mut mail);
            }
            let raw = render_eml(&mail);
            let parsed = parse_eml(&raw);
            html_response(render_mail_detail(&parsed, "Template preview"))
        }
        Err(error) => preview_error_response(&error),
    }
}

/// Inject sample RFC 8058 headers into a preview so authors can confirm wiring
/// without sending. Uses the configured [`UnsubscribeRuntime`] when present,
/// otherwise a sample base URL with an ephemeral key purely for display.
fn apply_preview_unsubscribe_headers(state: &AppState, mailer_label: &str, mut mail: Mail) -> Mail {
    let scope = mail.list_unsubscribe.clone().or_else(|| {
        registered_list_unsubscribe_scopes()
            .into_iter()
            .find(|descriptor| descriptor.mailer == mailer_label)
            .map(|descriptor| descriptor.scope.to_owned())
    });
    let Some(scope) = scope else {
        return mail;
    };
    mail.list_unsubscribe = Some(scope.clone());
    let recipient = mail.to.first().map_or_else(
        || "subscriber@example.com".to_owned(),
        |to| canonical_subscriber(to),
    );
    // Use the configured runtime when present, otherwise a sample with an
    // ephemeral key purely for display. Compute the header inside each branch so
    // the sample need not outlive this expression.
    let (header, one_click) = state.extension::<UnsubscribeRuntime>().map_or_else(
        || {
            let sample = UnsubscribeRuntime {
                base_url: Some("https://example.com".to_owned()),
                mailto: None,
                signing_keys: Arc::new(crate::security::config::resolve_signing_keys(
                    &crate::security::config::SigningSecretConfig::default(),
                )),
                ttl_days: unsubscribe::DEFAULT_TOKEN_TTL_DAYS,
                suppression: None,
            };
            (
                sample.list_unsubscribe_header(&recipient, &scope),
                sample.supports_one_click(),
            )
        },
        |runtime| {
            (
                runtime.list_unsubscribe_header(&recipient, &scope),
                runtime.supports_one_click(),
            )
        },
    );
    if let Some(value) = header {
        // Mirror send: the generated header replaces, not duplicates, any header
        // the preview author set by hand, so the preview reflects what is sent.
        mail.extra_headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("List-Unsubscribe")
                && !name.eq_ignore_ascii_case("List-Unsubscribe-Post")
        });
        mail.extra_headers
            .push(("List-Unsubscribe".to_owned(), value));
        if one_click {
            mail.extra_headers.push((
                "List-Unsubscribe-Post".to_owned(),
                "List-Unsubscribe=One-Click".to_owned(),
            ));
        }
    }
    mail
}

async fn captured_messages(dir: &Path) -> Result<Vec<CapturedMailSummary>, MailPreviewError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut messages = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("eml"))
        {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let metadata = entry.metadata().await?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let raw = tokio::fs::read_to_string(&path).await?;
        let parsed = parse_eml(&raw);
        messages.push(CapturedMailSummary {
            id: id.to_owned(),
            to: parsed.to,
            subject: parsed.subject,
            timestamp: parsed.date.unwrap_or_else(|| format_system_time(modified)),
            modified,
        });
    }

    messages.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(messages)
}

async fn read_captured_message(
    dir: &Path,
    message_id: &str,
) -> Result<ParsedMail, MailPreviewError> {
    if !valid_message_id(message_id) {
        return Err(MailPreviewError::InvalidMessageId(message_id.to_owned()));
    }
    let path = dir.join(message_id);
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(MailPreviewError::NotFound(message_id.to_owned()));
        }
        Err(error) => return Err(error.into()),
    };
    Ok(parse_eml(&raw))
}

fn valid_message_id(message_id: &str) -> bool {
    !message_id.is_empty()
        && Path::new(message_id)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("eml"))
        && !message_id.contains('/')
        && !message_id.contains('\\')
        && !message_id.contains("..")
}

fn parse_eml(raw: &str) -> ParsedMail {
    let normalized = raw.replace("\r\n", "\n");
    let (headers, body) = split_headers_body(&normalized);
    let content_type = header_value(&headers, "Content-Type").unwrap_or_default();
    let (html, text, attachments) = parse_mail_body(&content_type, body);
    let to = header_values(&headers, "To");
    let subject = header_value(&headers, "Subject").unwrap_or_else(|| "(no subject)".to_owned());
    let date = header_value(&headers, "Date");

    ParsedMail {
        headers,
        to,
        subject,
        date,
        html,
        text,
        attachments,
        raw: raw.to_owned(),
    }
}

fn split_headers_body(raw: &str) -> (Vec<(String, String)>, &str) {
    let Some((header_block, body)) = raw.split_once("\n\n") else {
        return (parse_header_block(raw), "");
    };
    (parse_header_block(header_block), body)
}

fn parse_header_block(header_block: &str) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    let mut current: Option<(String, String)> = None;

    for line in header_block.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some(header) = current.take() {
            headers.push(header);
        }
        if let Some((name, value)) = line.split_once(':') {
            current = Some((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    if let Some(header) = current {
        headers.push(header);
    }
    headers
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn header_values(headers: &[(String, String)], name: &str) -> Vec<String> {
    headers
        .iter()
        .filter(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .collect()
}

fn parse_mail_body(
    content_type: &str,
    body: &str,
) -> (Option<String>, Option<String>, Vec<ParsedAttachment>) {
    let lower = content_type.to_ascii_lowercase();
    if lower.contains("multipart/mixed")
        && let Some(boundary) = content_type_boundary(content_type)
    {
        return parse_multipart_mixed(body, &boundary);
    }

    if lower.contains("multipart/alternative")
        && let Some(boundary) = content_type_boundary(content_type)
    {
        let (html, text) = parse_multipart_alternative(body, &boundary);
        return (html, text, Vec::new());
    }

    if lower.contains("text/html") {
        (Some(trim_body(body)), None, Vec::new())
    } else {
        (None, Some(trim_body(body)), Vec::new())
    }
}

/// Parses a `multipart/mixed` body: the first non-attachment part is
/// recursed into for html/text (it is itself typically a nested
/// `multipart/alternative`), and every part with an `attachment`
/// `Content-Disposition` is collected into the returned attachment list.
fn parse_multipart_mixed(
    body: &str,
    boundary: &str,
) -> (Option<String>, Option<String>, Vec<ParsedAttachment>) {
    let marker = format!("--{boundary}");
    let mut html = None;
    let mut text = None;
    let mut attachments = Vec::new();

    for segment in body.split(&marker).skip(1) {
        let segment = segment.trim_start_matches(['\n', '\r']);
        if segment.starts_with("--") {
            break;
        }
        let (headers, part_body) = split_headers_body(segment);
        let disposition = header_value(&headers, "Content-Disposition").unwrap_or_default();
        let part_content_type = header_value(&headers, "Content-Type").unwrap_or_default();
        let disposition_type = split_mime_params(&disposition)
            .first()
            .copied()
            .unwrap_or("");
        if disposition_type.eq_ignore_ascii_case("attachment") {
            attachments.push(ParsedAttachment {
                filename: extract_attachment_filename(&disposition),
                content_type: content_type_without_params(&part_content_type),
            });
        } else {
            let (nested_html, nested_text, _) = parse_mail_body(&part_content_type, part_body);
            html = html.or(nested_html);
            text = text.or(nested_text);
        }
    }

    (html, text, attachments)
}

/// Splits a `Content-Disposition`/`Content-Type` parameter list on `;`,
/// respecting RFC 2045 quoted-string boundaries so a value like
/// `filename="a;b.txt"` is not mistaken for two parameters.
fn split_mime_params(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = value;
    // Peel one parameter off the front at a time. Quote/escape state is always
    // "outside a quoted string" at an unquoted `;`, so restarting the scan on
    // the remainder is equivalent to carrying the state across the whole
    // string — and it keeps every boundary a `split_at_checked` result rather
    // than hand-computed index arithmetic.
    while let Some(sep) = unquoted_semicolon(rest) {
        let Some((param, after)) = rest.split_at_checked(sep) else {
            break;
        };
        parts.push(param.trim());
        rest = after.strip_prefix(';').unwrap_or(after);
    }
    parts.push(rest.trim());
    parts
}

/// Byte offset of the first `;` in `value` that sits outside an RFC 2045
/// quoted string, if there is one.
fn unquoted_semicolon(value: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => return Some(i),
            _ => {}
        }
    }
    None
}

/// Reverses RFC 2045 quoted-string escaping (`\\` → `\`, `\"` → `"`), the
/// inverse of [`quote_header_value`].
fn unescape_quoted_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.next()
        {
            result.push(next);
        } else {
            result.push(ch);
        }
    }
    result
}

/// Extracts a filename from a `Content-Disposition: attachment; …` header
/// value, preferring the RFC 2231 extended `filename*=charset'lang'…`
/// parameter (percent-decoded, case-insensitive charset/param name, any
/// language tag) over the plain `filename="…"` fallback when both are
/// present.
fn extract_attachment_filename(disposition: &str) -> String {
    let params = split_mime_params(disposition);
    if let Some(value) = params.iter().skip(1).find_map(|part| {
        let (key, val) = part.split_once('=')?;
        key.trim().eq_ignore_ascii_case("filename*").then_some(val)
    }) {
        let encoded = value.splitn(3, '\'').nth(2).unwrap_or(value);
        return percent_encoding::percent_decode_str(encoded)
            .decode_utf8_lossy()
            .into_owned();
    }
    if let Some(value) = params.iter().skip(1).find_map(|part| {
        let (key, val) = part.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("filename")
            .then_some(val.trim())
    }) {
        if let Some(inner) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            return unescape_quoted_string(inner);
        }
        return value.to_owned();
    }
    "attachment".to_owned()
}

fn content_type_without_params(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_owned()
}

fn parse_multipart_alternative(body: &str, boundary: &str) -> (Option<String>, Option<String>) {
    let marker = format!("--{boundary}");
    let mut html = None;
    let mut text = None;

    for segment in body.split(&marker).skip(1) {
        let segment = segment.trim_start_matches(['\n', '\r']);
        if segment.starts_with("--") {
            break;
        }
        let (headers, part_body) = split_headers_body(segment);
        let content_type = header_value(&headers, "Content-Type").unwrap_or_default();
        if content_type.to_ascii_lowercase().contains("text/html") {
            html = Some(trim_body(part_body));
        } else if content_type.to_ascii_lowercase().contains("text/plain") {
            text = Some(trim_body(part_body));
        }
    }

    (html, text)
}

fn content_type_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let part = part.trim();
        let (name, value) = part.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        Some(value.trim().trim_matches('"').to_owned())
    })
}

fn trim_body(body: &str) -> String {
    body.trim_matches(['\r', '\n']).to_owned()
}

fn format_system_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn render_mail_index(
    messages: &[CapturedMailSummary],
    previews: &[MailPreview],
    file_dir: &Path,
) -> String {
    let mut body = String::new();
    body.push_str("<h1>Autumn Mail</h1>");
    body.push_str("<section><h2>Captured messages</h2>");
    if messages.is_empty() {
        body.push_str("<p class=\"empty\">No captured emails yet. Set <code>mail.transport = &quot;file&quot;</code>, send an email, then refresh this page. Autumn reads <code>");
        body.push_str(&escape_html(&file_dir.display().to_string()));
        body.push_str("</code>.</p>");
    } else {
        body.push_str(
            "<table><thead><tr><th>Timestamp</th><th>To</th><th>Subject</th></tr></thead><tbody>",
        );
        for message in messages {
            body.push_str("<tr><td>");
            body.push_str(&escape_html(&message.timestamp));
            body.push_str("</td><td>");
            body.push_str(&escape_html(&message.to.join(", ")));
            body.push_str("</td><td><a href=\"");
            body.push_str(MAIL_PREVIEW_PATH);
            body.push_str("/messages/");
            body.push_str(&escape_html(&message.id));
            body.push_str("\">");
            body.push_str(&escape_html(&message.subject));
            body.push_str("</a></td></tr>");
        }
        body.push_str("</tbody></table>");
    }
    body.push_str("</section><section><h2>Template previews</h2>");
    if previews.is_empty() {
        body.push_str("<p class=\"empty\">No mailer previews registered.</p>");
    } else {
        body.push_str("<table><thead><tr><th>Mailer</th><th>Preview</th></tr></thead><tbody>");
        for preview in previews {
            body.push_str("<tr><td>");
            body.push_str(&escape_html(preview.mailer()));
            body.push_str("</td><td><a href=\"");
            body.push_str(MAIL_PREVIEW_PATH);
            body.push_str("/previews/");
            body.push_str(&escape_html(preview.mailer()));
            body.push('/');
            body.push_str(&escape_html(preview.method()));
            body.push_str("\">");
            body.push_str(&escape_html(preview.method()));
            body.push_str("</a></td></tr>");
        }
        body.push_str("</tbody></table>");
    }
    body.push_str("</section>");
    render_mail_preview_layout("Autumn Mail", &body)
}

fn render_mail_detail(parsed: &ParsedMail, label: &str) -> String {
    let mut body = String::new();
    body.push_str("<p><a href=\"");
    body.push_str(MAIL_PREVIEW_PATH);
    body.push_str("\">Back to mail</a></p><h1>");
    body.push_str(&escape_html(&parsed.subject));
    body.push_str("</h1><p class=\"muted\">");
    body.push_str(&escape_html(label));
    body.push_str("</p>");

    if let Some(html) = &parsed.html {
        body.push_str("<iframe title=\"Rendered HTML email\" sandbox srcdoc=\"");
        body.push_str(&escape_html(html));
        body.push_str("\"></iframe>");
    } else {
        body.push_str("<p class=\"empty\">No HTML body was found for this email.</p>");
    }

    body.push_str("<details><summary>Plain text</summary><pre>");
    body.push_str(&escape_html(parsed.text.as_deref().unwrap_or("")));
    body.push_str("</pre></details>");

    if !parsed.attachments.is_empty() {
        body.push_str("<details open><summary>Attachments (");
        body.push_str(&parsed.attachments.len().to_string());
        body.push_str(")</summary><ul>");
        for attachment in &parsed.attachments {
            body.push_str("<li>");
            body.push_str(&escape_html(&attachment.filename));
            body.push_str(" <span class=\"muted\">(");
            body.push_str(&escape_html(&attachment.content_type));
            body.push_str(")</span></li>");
        }
        body.push_str("</ul></details>");
    }

    body.push_str("<details><summary>Headers</summary><dl>");
    for header in [
        "From",
        "To",
        "Reply-To",
        "Subject",
        "Date",
        "Message-Id",
        "List-Unsubscribe",
        "List-Unsubscribe-Post",
    ] {
        if let Some(value) = parsed.header_value(header) {
            body.push_str("<dt>");
            body.push_str(header);
            body.push_str("</dt><dd>");
            body.push_str(&escape_html(value));
            body.push_str("</dd>");
        }
    }
    body.push_str("</dl></details>");

    body.push_str("<details><summary>Raw .eml</summary><pre>");
    body.push_str(&escape_html(&parsed.raw));
    body.push_str("</pre></details>");

    render_mail_preview_layout(&parsed.subject, &body)
}

fn render_mail_preview_layout(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>{}</style></head><body>{}</body></html>",
        escape_html(title),
        MAIL_PREVIEW_CSS,
        body
    )
}

const MAIL_PREVIEW_CSS: &str = r#"
body{margin:0;padding:24px;font-family:system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#1f2933;background:#f6f8fa}
h1{margin:0 0 16px;font-size:28px}
h2{margin:28px 0 12px;font-size:18px}
table{width:100%;border-collapse:collapse;background:white;border:1px solid #d9e2ec}
th,td{padding:10px 12px;border-bottom:1px solid #e5eaf0;text-align:left;font-size:14px;vertical-align:top}
th{background:#edf2f7;color:#394b59;font-weight:650}
a{color:#0b63ce;text-decoration:none}
a:hover{text-decoration:underline}
.empty,.muted{color:#52616f}
code,pre{font-family:ui-monospace,SFMono-Regular,Consolas,monospace}
pre{white-space:pre-wrap;background:#111827;color:#f8fafc;padding:12px;overflow:auto}
iframe{width:100%;min-height:420px;border:1px solid #cbd5e1;background:white}
details{margin-top:14px;background:white;border:1px solid #d9e2ec;padding:10px 12px}
summary{cursor:pointer;font-weight:650}
dt{font-weight:650;margin-top:8px}
dd{margin:2px 0 8px}
"#;

fn html_response(html: String) -> Response {
    Html(html).into_response()
}

fn preview_error_response(error: &MailPreviewError) -> Response {
    let status = match error {
        MailPreviewError::NotFound(_) | MailPreviewError::InvalidMessageId(_) => {
            http::StatusCode::NOT_FOUND
        }
        MailPreviewError::Io(_) | MailPreviewError::PreviewPanicked { .. } => {
            http::StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (
        status,
        Html(render_mail_preview_layout(
            "Mail preview error",
            &format!(
                "<h1>Mail preview error</h1><p>{}</p>",
                escape_html(&error.to_string())
            ),
        )),
    )
        .into_response()
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn parse_mailbox(address: &str) -> Result<Mailbox, MailError> {
    address.parse().map_err(|source| MailError::InvalidAddress {
        address: address.to_owned(),
        source,
    })
}

/// Canonical, case-insensitive bare address used as the suppression / token key.
///
/// Strips any display name (`Ada <ada@example.com>` → `ada@example.com`) and
/// lowercases, so an opt-out matches future sends regardless of formatting.
/// Falls back to the trimmed, lowercased input when the address cannot be parsed.
fn canonical_subscriber(recipient: &str) -> String {
    parse_mailbox(recipient).map_or_else(
        |_| recipient.trim().to_ascii_lowercase(),
        |mailbox| mailbox.email.to_string().to_ascii_lowercase(),
    )
}

/// The html/text body of a message, before any attachment wrapping is
/// decided. Kept as an enum so the attachment-less code path can hand a
/// `SinglePart` straight to `Message::builder().singlepart(...)` exactly as
/// it did before attachments existed — a `MultiPart::mixed()` wrapper is
/// only introduced when there is at least one attachment.
enum MailBodyPart {
    Single(SinglePart),
    Multi(MultiPart),
}

fn lettre_body_part(mail: &Mail) -> Result<MailBodyPart, MailError> {
    match (&mail.text, &mail.html) {
        (Some(text), Some(html)) => Ok(MailBodyPart::Multi(
            MultiPart::alternative()
                .singlepart(SinglePart::plain(text.clone()))
                .singlepart(SinglePart::html(html.clone())),
        )),
        (Some(text), None) => Ok(MailBodyPart::Single(SinglePart::plain(text.clone()))),
        (None, Some(html)) => Ok(MailBodyPart::Single(SinglePart::html(html.clone()))),
        (None, None) => Err(MailError::InvalidMessage(
            "mail must include html or text body".to_owned(),
        )),
    }
}

fn lettre_attachment_part(attachment: &MailAttachment) -> Result<SinglePart, MailError> {
    let content_type = ContentType::parse(&attachment.content_type).map_err(|error| {
        MailError::InvalidMessage(format!(
            "attachment {:?} has invalid content type {:?}: {error}",
            attachment.filename, attachment.content_type
        ))
    })?;
    // Force base64 regardless of content: lettre's automatic encoder picks
    // `7bit` for short ASCII byte buffers, but attachments must always carry
    // a `base64` Content-Transfer-Encoding per the framework's contract.
    #[allow(
        clippy::expect_used,
        reason = "infallible: base64 encoding is always valid for any byte buffer"
    )]
    let body =
        LettreBody::new_with_encoding(attachment.bytes.clone(), ContentTransferEncoding::Base64)
            .expect("base64 encoding is always valid for any byte buffer");
    Ok(LettreAttachment::new(attachment.filename.clone()).body(body, content_type))
}

fn lettre_message(mail: &Mail) -> Result<Message, MailError> {
    let from = mail
        .from
        .as_deref()
        .ok_or_else(|| MailError::InvalidMessage("mail from address is required".to_owned()))?;
    let mut builder = Message::builder().from(parse_mailbox(from)?);
    for to in &mail.to {
        builder = builder.to(parse_mailbox(to)?);
    }
    if let Some(reply_to) = &mail.reply_to {
        builder = builder.reply_to(parse_mailbox(reply_to)?);
    }
    builder = builder.subject(mail.subject.clone());

    for (name, value) in &mail.extra_headers {
        use lettre::message::header::{HeaderName, HeaderValue};
        match HeaderName::new_from_ascii(name.clone()) {
            Ok(header_name) => {
                builder = builder.raw_header(HeaderValue::new(header_name, value.clone()));
            }
            Err(error) => {
                tracing::warn!(
                    header_name = %name,
                    error = %error,
                    "skipping mail header with invalid name"
                );
            }
        }
    }

    let body_part = lettre_body_part(mail)?;

    if mail.attachments.is_empty() {
        return Ok(match body_part {
            MailBodyPart::Multi(multi) => builder.multipart(multi)?,
            MailBodyPart::Single(single) => builder.singlepart(single)?,
        });
    }

    let mut mixed = match body_part {
        MailBodyPart::Multi(multi) => MultiPart::mixed().multipart(multi),
        MailBodyPart::Single(single) => MultiPart::mixed().singlepart(single),
    };
    for attachment in &mail.attachments {
        mixed = mixed.singlepart(lettre_attachment_part(attachment)?);
    }
    Ok(builder.multipart(mixed)?)
}

struct InterceptedMailTransport {
    inner: Arc<dyn MailTransport>,
    interceptor: Arc<dyn crate::interceptor::MailInterceptor>,
}

impl MailTransport for InterceptedMailTransport {
    fn send<'a>(
        &'a self,
        mail: Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
        Box::pin(async move {
            let inner = Arc::clone(&self.inner);
            let mail_for_next = mail.clone();
            let next = Box::pin(async move { inner.send(mail_for_next).await });
            self.interceptor.intercept(&mail, next).await
        })
    }

    fn is_disabled(&self) -> bool {
        self.inner.is_disabled()
    }
}

/// Install the configured mailer into app state.
///
/// Picks up a runtime-installed [`MailDeliveryQueueHandle`] from
/// [`AppState`] extensions when present, so plugins (Harvest, Redis-backed,
/// etc.) can register durable delivery before this runs. In `prod` with a
/// non-`Disabled` transport, when neither a durable queue nor
/// [`MailConfig::allow_in_process_deliver_later_in_production`] is set, startup
/// still succeeds — apps that never call `deliver_later` should not be crashed
/// for a code path they don't use (issue #2142). Instead, a startup warning is
/// logged and the installed [`Mailer`] is marked so that
/// [`Mailer::try_deliver_later`]/[`Mailer::try_deliver_later_eager`] fail with
/// [`MailError::NoDurableQueueInProduction`] the first time deferred delivery
/// is actually attempted. `enforce_durable_guard` set to `false` (used by
/// short-lived contexts like static-site builds where `deliver_later`
/// semantics don't apply) skips this check entirely.
///
/// # Errors
///
/// Returns an Autumn error when the configured transport cannot be created.
#[allow(clippy::too_many_lines)]
pub(crate) fn install_mailer(
    state: &AppState,
    config: &MailConfig,
    enforce_durable_guard: bool,
) -> AutumnResult<()> {
    let resilience = state
        .extension::<crate::config::AutumnConfig>()
        .map(|c| Arc::new(c.resilience.clone()));
    let mut mailer =
        Mailer::from_config_inner(config, resilience).map_err(AutumnError::service_unavailable)?;

    if let Some(interceptor) = state.extension::<Arc<dyn crate::interceptor::MailInterceptor>>() {
        mailer.transport = Arc::new(InterceptedMailTransport {
            inner: Arc::clone(&mailer.transport),
            interceptor: (*interceptor).clone(),
        });
    }

    let in_production = matches!(state.profile(), "prod" | "production");
    let transport_sends_mail = config.transport != Transport::Disabled;

    // Honor the disabled transport contract: if the operator turned mail off
    // for this profile (tests, review apps, etc.), `deliver_later` must also
    // be a no-op — even when a durable queue was registered globally.
    if transport_sends_mail {
        let queue_handle = state.extension::<MailDeliveryQueueHandle>();
        if let Some(handle) = queue_handle.as_ref() {
            mailer.delivery_queue = Some(Arc::clone(handle.inner()));
        }
    }

    if enforce_durable_guard && in_production && transport_sends_mail {
        let has_durable_queue = mailer.delivery_queue.is_some();
        if !has_durable_queue && !config.allow_in_process_deliver_later_in_production {
            // Issue #2142: don't hard-fail app boot over an unused code path.
            // Apps that only call `send()` are never affected; apps that do
            // call `deliver_later` in this state find out at the call site
            // (see `Mailer::try_deliver_later`), not by crashing at startup.
            tracing::warn!(
                "mail.deliver_later has no durable backend in prod: deliver_later/deliver_later_eager will fail if called; \
                 register a MailDeliveryQueueHandle on AppState, or set mail.allow_in_process_deliver_later_in_production = true \
                 to opt into the non-durable in-process Tokio fallback. Apps that only use mail.send() are unaffected."
            );
            mailer.block_deliver_later_without_durable_queue = true;
        } else if !has_durable_queue {
            tracing::warn!(
                "mail.deliver_later is using the in-process Tokio fallback in prod; this is acknowledged via mail.allow_in_process_deliver_later_in_production but is not durable across restarts or replicas"
            );
        }
    }

    // ── List-Unsubscribe wiring ──────────────────────────────────────────────
    let base_url = config
        .unsubscribe_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mailto = config
        .unsubscribe_mailto
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let unsubscribe_configured = base_url.is_some() || mailto.is_some();

    // Resolve the suppression backend: an explicitly registered handle wins;
    // otherwise auto-wire a Diesel-backed store when a DB pool is available.
    let suppression: Option<Arc<dyn SuppressionStore>> = {
        let explicit = state
            .extension::<SuppressionStoreHandle>()
            .map(|handle| Arc::clone(handle.inner()));
        #[cfg(feature = "db")]
        let resolved = explicit.or_else(|| {
            state
                .pool()
                .map(|pool| Arc::new(db_suppression::DbSuppressionStore::new(pool.clone())) as _)
        });
        #[cfg(not(feature = "db"))]
        let resolved = explicit;
        resolved
    };

    // Fail closed: any mailer that declares `list_unsubscribe` needs a place to
    // point the unsubscribe link/mailto, or Gmail/Yahoo will reject the mail.
    // Skipped when the transport is disabled — no list mail is emitted, so the
    // disabled-transport contract (review apps, tests) can boot without it.
    if transport_sends_mail
        && unsubscribe_config_fail_closed(
            enforce_durable_guard,
            in_production,
            has_list_unsubscribe_mailers(),
            unsubscribe_configured,
        )
    {
        return Err(AutumnError::service_unavailable_msg(
            "a #[mailer] declares list_unsubscribe but neither mail.unsubscribe_base_url nor mail.unsubscribe_mailto is configured: set at least one so RFC 8058 List-Unsubscribe headers can be emitted",
        ));
    }

    // Fail closed: when we will actually emit one-click links (active transport,
    // a list mailer, and a base URL), the endpoint must be able to record
    // opt-outs — otherwise a successful unsubscribe POST is a silent no-op.
    if enforce_durable_guard
        && in_production
        && transport_sends_mail
        && has_list_unsubscribe_mailers()
        && base_url.is_some()
        && suppression.is_none()
    {
        return Err(AutumnError::service_unavailable_msg(
            "mail.unsubscribe_base_url is set but no suppression backend is available: configure a database pool or register a SuppressionStore so one-click unsubscribes can be persisted",
        ));
    }

    // Warn (don't fail — a custom route is a valid choice) when one-click links
    // will be advertised but the built-in endpoint is not opted in. We can't see
    // app-registered routes here, so this is a heads-up, not a hard gate.
    if in_production
        && transport_sends_mail
        && has_list_unsubscribe_mailers()
        && base_url.is_some()
        && !config.mount_unsubscribe_endpoint
    {
        tracing::warn!(
            target: "mail",
            path = UNSUBSCRIBE_PATH,
            "list mail will advertise one-click unsubscribe URLs but the default endpoint is not mounted; call AppBuilder::mount_unsubscribe_endpoint() or serve the path yourself"
        );
    }

    if unsubscribe_configured || suppression.is_some() {
        let signing_keys = Arc::new(crate::security::config::resolve_signing_keys(
            &state
                .extension::<crate::config::AutumnConfig>()
                .map(|c| c.security.signing_secret.clone())
                .unwrap_or_default(),
        ));
        let ttl_days = config.unsubscribe_token_ttl_days;
        let make_runtime = || UnsubscribeRuntime {
            base_url: base_url.map(str::to_owned),
            mailto: mailto.map(str::to_owned),
            signing_keys: Arc::clone(&signing_keys),
            ttl_days,
            suppression: suppression.clone(),
        };
        // Always share the wiring with the endpoint handler (mounted whenever an
        // unsubscribe destination is configured, independent of transport) so a
        // live unsubscribe link never 404s. Only the *sender* skips when the
        // transport is intentionally a no-op.
        state.insert_extension(make_runtime());
        if transport_sends_mail {
            mailer.unsubscribe = Some(Arc::new(make_runtime()));
        }
    }

    // ── Bounce/complaint suppression wiring (issue #1247) ────────────────────
    // Zero-config: default to an in-memory store so the detect→suppress loop
    // works out of the box on a single instance. An explicitly registered
    // handle (e.g. a Postgres-backed `PgSuppressionStore` via
    // `AppBuilder::with_mail_suppression_store`) wins. Unlike List-Unsubscribe
    // suppression, no db-backed store is auto-wired: `send()` consults this on
    // *every* message, so silently pointing it at a table that may not exist
    // would break all outbound mail — durable backends are opt-in.
    //
    // The resolved handle is registered on `AppState` so inbound bounce/complaint
    // handlers can share the exact store the `Mailer` consults.
    if transport_sends_mail {
        let handle = state
            .extension::<suppression::SuppressionStoreHandle>()
            .map_or_else(
                || {
                    let handle = suppression::SuppressionStoreHandle::new(
                        suppression::InMemorySuppressionStore::new(),
                    );
                    state.insert_extension(handle.clone());
                    handle
                },
                |arc| (*arc).clone(),
            );
        mailer.suppression = Some(Arc::clone(handle.inner()));
    }

    state.insert_extension(mailer);
    Ok(())
}

/// Run the optional [`MailDeliveryQueue`] factory and install the configured
/// mailer.
///
/// Centralizes the wiring used by every [`AppBuilder`](crate::app::AppBuilder)
/// build path: optionally invoke `queue_factory` against the live `AppState`,
/// register the resulting [`MailDeliveryQueueHandle`], then call
/// [`install_mailer`]. The factory is skipped entirely when
/// `enforce_durable_guard` is `false` (static-site builds), since the queue
/// may capture infrastructure (Redis, Harvest, etc.) that isn't available in
/// the asset-build environment.
///
/// # Errors
///
/// Propagates errors from the queue factory and from [`install_mailer`].
pub(crate) fn install_mailer_with_factory<F>(
    state: &AppState,
    config: &MailConfig,
    queue_factory: Option<F>,
    enforce_durable_guard: bool,
) -> AutumnResult<()>
where
    F: FnOnce(&AppState) -> AutumnResult<Arc<dyn MailDeliveryQueue>>,
{
    // Honor the disabled transport contract: a profile that turned mail off
    // (tests, review apps, etc.) must not open queue infrastructure either,
    // since all sends — immediate and deferred — are supposed to be no-ops.
    let transport_sends_mail = config.transport != Transport::Disabled;
    if enforce_durable_guard
        && transport_sends_mail
        && let Some(factory) = queue_factory
    {
        let queue = factory(state)?;
        state.insert_extension(MailDeliveryQueueHandle::from_arc(queue));
    }
    install_mailer(state, config, enforce_durable_guard)
}

// ── Default one-click unsubscribe endpoint ───────────────────────────────────

#[derive(Deserialize)]
struct UnsubscribeParams {
    #[serde(default)]
    token: String,
}

/// Router for the framework's default unsubscribe endpoint.
///
/// Mounted automatically when `mail.unsubscribe_base_url` or
/// `mail.unsubscribe_mailto` is configured, unless the app registers its own
/// route at [`UNSUBSCRIBE_PATH`] (the documented override hook). Requires no
/// end-user auth; the global rate-limit layer applies.
pub(crate) fn unsubscribe_router() -> axum::Router<AppState> {
    axum::Router::new().route(
        UNSUBSCRIBE_PATH,
        axum::routing::get(unsubscribe_get_handler).post(unsubscribe_post_handler),
    )
}

/// RFC 8058 one-click POST: verify the token and record the suppression.
async fn unsubscribe_post_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<UnsubscribeParams>,
    body: String,
) -> Response {
    // RFC 8058 §3.1: the one-click POST carries `List-Unsubscribe=One-Click`.
    // Requiring it avoids recording opt-outs from arbitrary POSTs to the URL
    // (e.g. link scanners that don't send the body).
    if !is_one_click_body(&body) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "expected List-Unsubscribe=One-Click body",
        )
            .into_response();
    }
    let Some(runtime) = state.extension::<UnsubscribeRuntime>() else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            "unsubscribe is not configured",
        )
            .into_response();
    };
    match unsubscribe::verify_token(&runtime.signing_keys, &params.token, current_unix_time()) {
        Ok(decoded) => {
            let Some(store) = runtime.suppression.as_ref() else {
                // No backend to record the opt-out — never confirm an unsubscribe
                // we cannot actually honor.
                tracing::error!(
                    target: "mail",
                    "unsubscribe POST received but no suppression backend is configured"
                );
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "unsubscribe storage is not configured",
                )
                    .into_response();
            };
            if let Err(error) = store.suppress(&decoded.subscriber, &decoded.list_id).await {
                tracing::error!(error = %error, "failed to record unsubscribe suppression");
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "could not process unsubscribe",
                )
                    .into_response();
            }
            tracing::info!(
                target: "mail",
                list_id = %decoded.list_id,
                outcome = "unsubscribed",
                "recorded one-click unsubscribe"
            );
            (
                axum::http::StatusCode::OK,
                Html(unsubscribe_confirmation_html(&decoded.list_id)),
            )
                .into_response()
        }
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            Html(unsubscribe_error_html(&error.to_string())),
        )
            .into_response(),
    }
}

/// Whether a urlencoded body contains `List-Unsubscribe=One-Click` (RFC 8058).
fn is_one_click_body(body: &str) -> bool {
    body.split('&').any(|pair| {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        let value = kv.next().unwrap_or("");
        key.eq_ignore_ascii_case("List-Unsubscribe") && value.eq_ignore_ascii_case("One-Click")
    })
}

/// Click-through GET: render a minimal confirmation page with a one-click form.
async fn unsubscribe_get_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<UnsubscribeParams>,
) -> Response {
    let Some(runtime) = state.extension::<UnsubscribeRuntime>() else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            "unsubscribe is not configured",
        )
            .into_response();
    };
    match unsubscribe::verify_token(&runtime.signing_keys, &params.token, current_unix_time()) {
        Ok(decoded) => Html(unsubscribe_form_html(&decoded.list_id, &params.token)).into_response(),
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            Html(unsubscribe_error_html(&error.to_string())),
        )
            .into_response(),
    }
}

fn unsubscribe_form_html(list_id: &str, token: &str) -> String {
    // Relative action (`?token=…`) posts back to the current URL, preserving any
    // base-path prefix added by a reverse proxy.
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Unsubscribe</title></head>\
         <body><h1>Unsubscribe</h1>\
         <p>Stop receiving <strong>{}</strong> emails?</p>\
         <form method=\"post\" action=\"?token={}\">\
         <input type=\"hidden\" name=\"List-Unsubscribe\" value=\"One-Click\">\
         <button type=\"submit\">Unsubscribe</button></form></body></html>",
        escape_html(list_id),
        escape_html(token),
    )
}

fn unsubscribe_confirmation_html(list_id: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Unsubscribed</title></head>\
         <body><h1>You're unsubscribed</h1>\
         <p>You will no longer receive <strong>{}</strong> emails.</p></body></html>",
        escape_html(list_id),
    )
}

fn unsubscribe_error_html(detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Unsubscribe</title></head>\
         <body><h1>Unsubscribe link is not valid</h1><p>{}</p></body></html>",
        escape_html(detail),
    )
}

/// Bounce/complaint mail suppression list (issue #1247).
///
/// Autumn already *detects* delivery failure: `inbound_mail` parses provider
/// bounce signals and spam complaints. This module closes the loop — it records
/// the addresses that hard-bounced or complained and has [`Mailer::send`] skip
/// them before transport, so a sending domain's reputation survives contact
/// with real recipients.
///
/// This is distinct from the recipient-initiated List-Unsubscribe suppression
/// in [`crate::mail::unsubscribe`] (issue #838): that keys on
/// `(subscriber, list_id)` and is driven by a user clicking "unsubscribe";
/// this keys on a bare address and is driven by a *provider-reported* failure.
///
/// # Backends
///
/// [`InMemorySuppressionStore`] is the zero-config default (process-local,
/// lost on restart — perfect for a single instance, tests, and review apps).
/// [`PgSuppressionStore`](suppression::PgSuppressionStore) (feature `db`) persists to a `mail_suppressions`
/// table for multi-instance deploys, mirroring the memory/durable split used
/// by sessions and jobs. That table is **not** auto-created — provision it
/// yourself (see [`PgSuppressionStore`](suppression::PgSuppressionStore)).
///
/// # Closing the loop
///
/// Wire the provided `record_inbound` handler into the inbound router's
/// `on_bounce` hook (or call [`SuppressionStore::suppress`] yourself) to turn a
/// parsed provider bounce into a suppression entry. autumn's `on_spam` signal
/// is an *inbound spam verdict*, not an outbound FBL complaint — see
/// `record_inbound` for why routing it here is a safe no-op rather than
/// suppressing the wrong address.
pub mod suppression {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{MailError, canonical_subscriber};

    /// Why an address is on the suppression list.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SuppressionReason {
        /// A permanent delivery failure (5xx SMTP / DSN hard bounce).
        HardBounce,
        /// A spam complaint / feedback-loop (FBL) report.
        Complaint,
        /// Added by an operator, not by a provider signal.
        Manual,
    }

    impl SuppressionReason {
        /// Stable lowercase token used in storage rows and log lines.
        #[must_use]
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::HardBounce => "hard_bounce",
                Self::Complaint => "complaint",
                Self::Manual => "manual",
            }
        }
    }

    impl std::fmt::Display for SuppressionReason {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.as_str())
        }
    }

    /// Persistent set of addresses that must not receive mail because they
    /// hard-bounced or filed a spam complaint.
    ///
    /// All three methods canonicalize the address (strip any display name and
    /// lowercase) so a suppression recorded as `Bounced@X.com` matches a later
    /// send to `Ada <bounced@x.com>`.
    pub trait SuppressionStore: Send + Sync {
        /// Returns `true` when `address` must not be delivered to.
        fn is_suppressed<'a>(
            &'a self,
            address: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>>;

        /// Record `address` on the suppression list (idempotent). A repeat call
        /// with a different `reason` updates the recorded reason.
        fn suppress<'a>(
            &'a self,
            address: &'a str,
            reason: SuppressionReason,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

        /// Remove `address` from the suppression list — the manual escape hatch
        /// (e.g. a recipient fixed their mailbox). No-op when absent.
        fn unsuppress<'a>(
            &'a self,
            address: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;
    }

    /// Cloneable handle to a [`SuppressionStore`] for storage on `AppState` and
    /// attachment to a [`Mailer`](crate::mail::Mailer).
    #[derive(Clone)]
    pub struct SuppressionStoreHandle(Arc<dyn SuppressionStore>);

    impl SuppressionStoreHandle {
        /// Wrap a store implementation.
        #[must_use]
        pub fn new(store: impl SuppressionStore + 'static) -> Self {
            Self(Arc::new(store))
        }

        /// Wrap an already-shared store implementation.
        #[must_use]
        pub fn from_arc(store: Arc<dyn SuppressionStore>) -> Self {
            Self(store)
        }

        /// Borrow the inner store.
        #[must_use]
        pub fn inner(&self) -> &Arc<dyn SuppressionStore> {
            &self.0
        }

        /// Consume the handle, yielding the shared store.
        #[must_use]
        pub fn into_inner(self) -> Arc<dyn SuppressionStore> {
            self.0
        }
    }

    impl std::fmt::Debug for SuppressionStoreHandle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SuppressionStoreHandle")
                .finish_non_exhaustive()
        }
    }

    /// In-memory [`SuppressionStore`] — the zero-config default.
    ///
    /// State is process-local and lost on restart; use [`PgSuppressionStore`]
    /// for multi-instance deploys that must share suppression across replicas.
    #[derive(Debug, Default, Clone)]
    pub struct InMemorySuppressionStore {
        entries: Arc<std::sync::Mutex<std::collections::HashMap<String, SuppressionReason>>>,
    }

    impl InMemorySuppressionStore {
        /// Create an empty in-memory store.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl SuppressionStore for InMemorySuppressionStore {
        fn is_suppressed<'a>(
            &'a self,
            address: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>> {
            Box::pin(async move {
                let key = canonical_subscriber(address);
                Ok(self
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&key))
            })
        }

        fn suppress<'a>(
            &'a self,
            address: &'a str,
            reason: SuppressionReason,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            Box::pin(async move {
                let key = canonical_subscriber(address);
                self.entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key, reason);
                Ok(())
            })
        }

        fn unsuppress<'a>(
            &'a self,
            address: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            Box::pin(async move {
                let key = canonical_subscriber(address);
                self.entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);
                Ok(())
            })
        }
    }

    // ── Observability: a suppressed drop is never truly silent ───────────────
    static SUPPRESSED_SKIPS: AtomicU64 = AtomicU64::new(0);

    /// Recipients [`Mailer::send`](crate::mail::Mailer::send) has skipped as suppressed, process-wide.
    ///
    /// Counted since startup. Pair with the structured `outcome =
    /// "skipped_suppressed"` log line emitted per skip.
    #[must_use]
    pub fn suppressed_skips() -> u64 {
        SUPPRESSED_SKIPS.load(Ordering::Relaxed)
    }

    /// Record and log a skip. Internal to the `send` path.
    pub(crate) fn note_skip(canonical_address: &str) {
        SUPPRESSED_SKIPS.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            target: "mail",
            outcome = "skipped_suppressed",
            address = %canonical_address,
            "skipping suppressed recipient (hard bounce or complaint); \
             pass Mail::ignore_suppression() to override for critical mail"
        );
    }

    /// Provided inbound handler: turn a parsed provider bounce/complaint webhook
    /// into a suppression entry, closing the detect→suppress loop in one call.
    ///
    /// It only ever suppresses the *provider-reported failed/complaining
    /// address*, never `email.to` — on an inbound webhook `to` is the app's own
    /// inbound address, so suppressing it would let anyone who can POST to the
    /// endpoint knock arbitrary recipients off future sends.
    ///
    /// - A bounce (`email.is_bounce`) suppresses the provider-reported
    ///   [`bounced_address`](crate::inbound_mail::InboundEmail::bounced_address)
    ///   with [`SuppressionReason::HardBounce`]. A bounce flagged with no
    ///   address is logged and dropped (nothing suppressed).
    /// - A complaint suppresses
    ///   [`complained_address`](crate::inbound_mail::InboundEmail::complained_address)
    ///   with [`SuppressionReason::Complaint`] — populated only by parsers that
    ///   surface a genuine FBL complainant. autumn's built-in `on_spam` signal
    ///   is an *inbound spam verdict* (`X-Mailgun-Sflag`), not an outbound FBL
    ///   complaint, and carries no complainant address, so wiring `on_spam`
    ///   here is a safe no-op (logged) rather than suppressing the wrong party.
    ///
    /// Wire it into the inbound router (see the crate `suppression` module docs
    /// for the full shared-store example):
    ///
    /// ```rust,ignore
    /// InboundMailRouter::new()
    ///     .endpoint(InboundMailEndpointConfig::mailgun("/mail/inbound", key))
    ///     .on_bounce(|email| Box::pin(async move {
    ///         record_inbound(SUPPRESSION.get().unwrap().inner().as_ref(), &email).await?;
    ///         Ok(())
    ///     }));
    /// ```
    ///
    /// # Errors
    ///
    /// Propagates any [`MailError`] returned by the store while recording the
    /// suppression (e.g. a database backend being unavailable).
    #[cfg(feature = "inbound-mail")]
    pub async fn record_inbound(
        store: &dyn SuppressionStore,
        email: &crate::inbound_mail::InboundEmail,
    ) -> Result<(), MailError> {
        if email.is_bounce {
            // Only the provider-reported bounced address is the failed
            // recipient; `email.to` on a bounce webhook is the app's own
            // inbound address, so never suppress that.
            if let Some(addr) = email.bounced_address.as_deref() {
                store.suppress(addr, SuppressionReason::HardBounce).await?;
            } else {
                tracing::warn!(
                    target: "mail",
                    "inbound bounce webhook set is_bounce with no bounced_address; nothing suppressed"
                );
            }
            return Ok(());
        }
        // Complaint / FBL: suppress the genuine complainant only. Never fall
        // back to `email.to`. autumn's `on_spam` is an inbound spam verdict, not
        // an outbound complaint, so `complained_address` is `None` there and we
        // log rather than suppress the wrong address.
        if let Some(addr) = email.complained_address.as_deref() {
            store.suppress(addr, SuppressionReason::Complaint).await?;
        } else if email
            .spam_report
            .as_ref()
            .and_then(|r| r.verdict.as_deref())
            .is_some_and(|v| v.eq_ignore_ascii_case("yes"))
        {
            tracing::warn!(
                target: "mail",
                "inbound spam verdict carries no outbound complainant address; \
                 nothing suppressed (wire a real FBL/complaint source that \
                 populates InboundEmail::complained_address)"
            );
        }
        Ok(())
    }

    #[cfg(feature = "db")]
    pub use pg::PgSuppressionStore;

    #[cfg(feature = "db")]
    mod pg {
        use std::future::Future;
        use std::pin::Pin;

        use diesel::prelude::*;
        use diesel_async::AsyncPgConnection;
        use diesel_async::RunQueryDsl;
        use diesel_async::pooled_connection::deadpool::Pool;

        use super::super::canonical_subscriber;
        use super::{MailError, SuppressionReason, SuppressionStore};

        diesel::table! {
            mail_suppressions (address) {
                address -> Text,
                reason -> Text,
                suppressed_at -> Timestamptz,
            }
        }

        #[derive(Insertable)]
        #[diesel(table_name = mail_suppressions)]
        struct NewSuppression<'a> {
            address: &'a str,
            reason: &'a str,
        }

        /// Postgres-backed bounce/complaint [`SuppressionStore`].
        ///
        /// Suppression is shared across every instance that points at the same
        /// database.
        ///
        /// # Required table (no migration is shipped)
        ///
        /// This store does **not** create or migrate its table — provision it
        /// yourself (same convention as the List-Unsubscribe `mail_unsubscribes`
        /// store). Every `send` errors on the suppression lookup until it
        /// exists:
        ///
        /// ```sql
        /// CREATE TABLE mail_suppressions (
        ///     address       TEXT PRIMARY KEY,
        ///     reason        TEXT NOT NULL,
        ///     suppressed_at TIMESTAMPTZ NOT NULL DEFAULT now()
        /// );
        /// ```
        #[derive(Clone)]
        pub struct PgSuppressionStore {
            pool: Pool<AsyncPgConnection>,
        }

        impl PgSuppressionStore {
            /// Create a store backed by `pool`.
            #[must_use]
            pub const fn new(pool: Pool<AsyncPgConnection>) -> Self {
                Self { pool }
            }
        }

        impl SuppressionStore for PgSuppressionStore {
            fn is_suppressed<'a>(
                &'a self,
                address: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>> {
                Box::pin(async move {
                    let key = canonical_subscriber(address);
                    let mut conn = self.pool.get().await.map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression pool: {e}"))
                    })?;
                    let count: i64 = mail_suppressions::table
                        .filter(mail_suppressions::address.eq(&key))
                        .count()
                        .get_result(&mut conn)
                        .await
                        .map_err(|e| {
                            MailError::RuntimeUnavailable(format!("suppression query: {e}"))
                        })?;
                    Ok(count > 0)
                })
            }

            fn suppress<'a>(
                &'a self,
                address: &'a str,
                reason: SuppressionReason,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async move {
                    let key = canonical_subscriber(address);
                    let reason_str = reason.as_str();
                    let mut conn = self.pool.get().await.map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression pool: {e}"))
                    })?;
                    diesel::insert_into(mail_suppressions::table)
                        .values(NewSuppression {
                            address: &key,
                            reason: reason_str,
                        })
                        .on_conflict(mail_suppressions::address)
                        .do_update()
                        // Refresh both the reason and the timestamp so a
                        // re-suppression (e.g. an old hard bounce now also a
                        // complaint) reflects the latest event, not stale data.
                        .set((
                            mail_suppressions::reason.eq(reason_str),
                            mail_suppressions::suppressed_at.eq(diesel::dsl::now),
                        ))
                        .execute(&mut conn)
                        .await
                        .map_err(|e| {
                            MailError::RuntimeUnavailable(format!("suppression insert: {e}"))
                        })?;
                    Ok(())
                })
            }

            fn unsuppress<'a>(
                &'a self,
                address: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async move {
                    let key = canonical_subscriber(address);
                    let mut conn = self.pool.get().await.map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression pool: {e}"))
                    })?;
                    diesel::delete(
                        mail_suppressions::table.filter(mail_suppressions::address.eq(&key)),
                    )
                    .execute(&mut conn)
                    .await
                    .map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression delete: {e}"))
                    })?;
                    Ok(())
                })
            }
        }
    }
}

/// Diesel-backed [`SuppressionStore`].
#[cfg(feature = "db")]
pub mod db_suppression {
    use std::future::Future;
    use std::pin::Pin;

    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use diesel_async::pooled_connection::deadpool::Pool;

    use super::{MailError, SuppressionStore};

    diesel::table! {
        mail_unsubscribes (id) {
            id -> Int8,
            subscriber -> Text,
            list_id -> Text,
            unsubscribed_at -> Timestamptz,
        }
    }

    #[derive(Insertable)]
    #[diesel(table_name = mail_unsubscribes)]
    struct NewUnsubscribe<'a> {
        subscriber: &'a str,
        list_id: &'a str,
    }

    /// Postgres-backed suppression list keyed by `(subscriber, list_id)`.
    ///
    /// Backed by the `mail_unsubscribes` table provisioned by the migration that
    /// `autumn generate mailer --list-unsubscribe` writes into the app.
    #[derive(Clone)]
    pub struct DbSuppressionStore {
        pool: Pool<crate::db::RuntimeConnection>,
    }

    impl DbSuppressionStore {
        /// Create a store backed by `pool`.
        #[must_use]
        pub const fn new(pool: Pool<crate::db::RuntimeConnection>) -> Self {
            Self { pool }
        }
    }

    impl SuppressionStore for DbSuppressionStore {
        fn is_suppressed<'a>(
            &'a self,
            subscriber: &'a str,
            list_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>> {
            Box::pin(async move {
                let mut conn =
                    self.pool.get().await.map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression pool: {e}"))
                    })?;
                let count: i64 = mail_unsubscribes::table
                    .filter(mail_unsubscribes::subscriber.eq(subscriber))
                    .filter(mail_unsubscribes::list_id.eq(list_id))
                    .count()
                    .get_result(&mut conn)
                    .await
                    .map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression query: {e}"))
                    })?;
                Ok(count > 0)
            })
        }

        fn is_suppressed_many<'a>(
            &'a self,
            subscribers: &'a [&'a str],
            list_id: &'a str,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<std::collections::HashSet<String>, MailError>>
                    + Send
                    + 'a,
            >,
        > {
            // Chunked, not one unbounded `= ANY(...)`, as a backstop against a
            // truly pathological single send (hundreds of thousands of
            // recipients) binding an unbounded array into one statement.
            //
            // On Postgres, `eq_any` binds the whole array as ONE parameter
            // (`= ANY($n)`), so `CHUNK_SIZE` is deliberately large, not
            // tight: measured against a production-shaped
            // `mail_unsubscribes` fixture, `subscriber = ANY(...)` keeps
            // using the `(subscriber, list_id)` index up to a few thousand
            // array elements, then the planner switches to a `Parallel Seq
            // Scan` of the whole table — a plan whose cost is ~flat per
            // statement regardless of how many more elements are in the
            // array (it's already paying for the full scan). A chunk size
            // near that crossover would needlessly re-pay the full-scan cost
            // once per chunk; staying well above it keeps ordinary sends —
            // even a full-list newsletter blast — in one statement. See
            // docs/reports/2026-08-15-ledger-mail-suppression-batch/README.md
            // for the measurements behind the Postgres number.
            //
            // SQLite has no array bind type: Diesel lowers `eq_any` to
            // `IN (?, ?, ...)`, one bind parameter per element, so a
            // 50,000-element chunk plus the `list_id` parameter would blow
            // past `SQLITE_MAX_VARIABLE_NUMBER` (32,766 by default) and fail
            // the whole send with "too many SQL variables". Reuse
            // `repository::MAX_BIND_PARAMS` — the same backend-aware limit
            // generated bulk-write code already chunks against — minus one
            // for the `list_id` parameter, so this never depends on a second
            // hand-picked constant drifting out of sync with that one.
            #[cfg(not(feature = "sqlite"))]
            const CHUNK_SIZE: usize = 50_000;
            #[cfg(feature = "sqlite")]
            const CHUNK_SIZE: usize = crate::repository::MAX_BIND_PARAMS - 1;
            Box::pin(async move {
                let mut suppressed = std::collections::HashSet::with_capacity(subscribers.len());
                if subscribers.is_empty() {
                    return Ok(suppressed);
                }
                let mut conn =
                    self.pool.get().await.map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression pool: {e}"))
                    })?;
                for chunk in subscribers.chunks(CHUNK_SIZE) {
                    let owned: Vec<&str> = chunk.to_vec();
                    let hits: Vec<String> = mail_unsubscribes::table
                        .filter(mail_unsubscribes::list_id.eq(list_id))
                        .filter(mail_unsubscribes::subscriber.eq_any(owned))
                        .select(mail_unsubscribes::subscriber)
                        .load(&mut conn)
                        .await
                        .map_err(|e| {
                            MailError::RuntimeUnavailable(format!("suppression query: {e}"))
                        })?;
                    suppressed.extend(hits);
                }
                Ok(suppressed)
            })
        }

        fn suppress<'a>(
            &'a self,
            subscriber: &'a str,
            list_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            Box::pin(async move {
                let mut conn =
                    self.pool.get().await.map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression pool: {e}"))
                    })?;
                diesel::insert_into(mail_unsubscribes::table)
                    .values(NewUnsubscribe {
                        subscriber,
                        list_id,
                    })
                    .on_conflict((mail_unsubscribes::subscriber, mail_unsubscribes::list_id))
                    .do_nothing()
                    .execute(&mut conn)
                    .await
                    .map_err(|e| {
                        MailError::RuntimeUnavailable(format!("suppression insert: {e}"))
                    })?;
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CSS inlining (issue #1254) ────────────────────────────────────────

    #[test]
    fn html_contains_style_block_is_case_insensitive() {
        assert!(html_contains_style_block("<STYLE>.a{}</STYLE>"));
        assert!(html_contains_style_block("<p>x</p><style>.a{}</style>"));
        assert!(!html_contains_style_block("<p style=\"color:red\">x</p>"));
        assert!(!html_contains_style_block("just plain text, no tags"));
    }

    #[test]
    fn inline_css_applies_class_style_to_anchor() {
        // AC1: a `<style>` block + a class-styled `<a>` yields an equivalent
        // inline `style="…"` on the anchor.
        let html = r#"<style>.btn{color:#fff;background:#06c}</style><a class="btn">Go</a>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        // Inspect the `<a …>` opening tag specifically so every assertion is
        // discriminating: `#fff`/`#06c` also appear in the retained `<style>`
        // block, so a no-op would pass a bare `out.contains(...)`.
        let anchor = out
            .split("<a")
            .nth(1)
            .expect("an <a> tag is present in the output");
        let anchor_open = &anchor[..anchor.find('>').expect("anchor tag closes")];
        assert!(
            anchor_open.contains("style="),
            "anchor must gain an inline style attribute; got tag: {anchor_open}"
        );
        assert!(
            anchor_open.contains("#fff"),
            "anchor's inline style must carry the color rule; got tag: {anchor_open}"
        );
        assert!(
            anchor_open.contains("#06c") || anchor_open.contains("background"),
            "anchor's inline style must carry the background rule; got tag: {anchor_open}"
        );
    }

    #[test]
    fn inline_css_applies_class_style_to_table() {
        // AC7: a class-styled `<table>` gains the expected inline style.
        let html = r#"<style>.wrap{width:600px;background:#eee}</style><table class="wrap"><tr><td>x</td></tr></table>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        let table = out
            .split("<table")
            .nth(1)
            .expect("a <table> tag is present in the output");
        let table_open = &table[..table.find('>').expect("table tag closes")];
        assert!(
            table_open.contains("style=") && table_open.contains("600px"),
            "table must carry an inline style with the width rule; got tag: {table_open}"
        );
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_emits_outlook_width_height_attributes() {
        // Outlook-family clients ignore CSS `width`/`height`, so the inliner must
        // also emit the presentational HTML `width`/`height` attributes on the
        // supported elements (`table`/`td`/`th`/`img`) — not only the CSS `style=`.
        let html = r#"<style>table{width:600px}img{height:40px}</style><table><tr><td><img src="/x.png"></td></tr></table>"#;
        let out = inline_css_html(html).expect("inlining succeeds");

        let table_open = {
            let table = out
                .split("<table")
                .nth(1)
                .expect("a <table> tag is present in the output");
            &table[..table.find('>').expect("table tag closes")]
        };
        // Discriminating: the CSS style must be present AND the HTML attribute too.
        assert!(
            table_open.contains("style=") && table_open.contains("600px"),
            "table must still carry the inline CSS width; got tag: {table_open}"
        );
        assert!(
            table_open.contains(r#"width="600""#),
            "table must gain the presentational HTML width attribute Outlook needs; got tag: {table_open}"
        );

        let img_open = {
            let img = out
                .split("<img")
                .nth(1)
                .expect("an <img> tag is present in the output");
            &img[..img.find('>').expect("img tag closes")]
        };
        assert!(
            img_open.contains("style=") && img_open.contains("40px"),
            "img must still carry the inline CSS height; got tag: {img_open}"
        );
        assert!(
            img_open.contains(r#"height="40""#),
            "img must gain the presentational HTML height attribute Outlook needs; got tag: {img_open}"
        );
    }

    #[test]
    fn inline_css_passthrough_without_style_block_is_byte_identical() {
        // AC3: bodies already fully inlined (no `<style>`) pass through unchanged.
        let html = r#"<p style="color:red">Hello</p><a href="/x">link</a>"#;
        let out = inline_css_html(html).expect("no-op inlining succeeds");
        assert_eq!(out, html, "no-<style> body must be returned unchanged");
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_retains_link_stylesheet_tags() {
        // We never fetch `<link>` stylesheets, so the `<link rel="stylesheet">`
        // tag must survive inlining rather than being silently dropped from the
        // delivered body — otherwise a message combining an embedded `<style>`
        // with a linked stylesheet would lose the linked CSS. The embedded rule
        // is still inlined onto the element.
        let html = r#"<style>.x{color:red}</style><link rel="stylesheet" href="https://example.com/app.css"><p class="x">Hi</p>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        assert!(
            out.contains("<link") && out.contains(r#"rel="stylesheet""#),
            "the <link rel=\"stylesheet\"> tag must be preserved; got: {out}"
        );
        assert!(
            out.contains("app.css"),
            "the linked stylesheet href must be preserved; got: {out}"
        );
        let para = out
            .split("<p")
            .nth(1)
            .expect("a <p> tag is present in the output");
        let para_open = &para[..para.find('>').expect("paragraph tag closes")];
        assert!(
            para_open.contains("style=") && para_open.contains("red"),
            "the embedded rule must still be inlined onto the paragraph; got tag: {para_open}"
        );
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_is_idempotent() {
        // AC3: inlining twice equals inlining once.
        let html = r#"<style>.btn{color:#fff}p{margin:0}</style><a class="btn">Go</a><p>hi</p>"#;
        let once = inline_css_html(html).expect("first pass");
        let twice = inline_css_html(&once).expect("second pass");
        assert_eq!(once, twice, "inlining must be idempotent");
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_retains_uninlinable_media_queries() {
        // AC5: `@media` rules that cannot be inlined survive in a retained
        // `<style>` block rather than being dropped.
        let html = r#"<style>.btn{color:#fff}@media (max-width:600px){.btn{color:#000}}</style><a class="btn">Go</a>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        assert!(
            out.contains("@media") && out.contains("max-width"),
            "the @media rule must be preserved in a retained <style> block; got: {out}"
        );
        // And the inlinable rule was still applied to the element.
        assert!(
            out.contains("<a") && out.contains("style="),
            "the inlinable rule must still be inlined onto the anchor; got: {out}"
        );
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_fragment_body_stays_a_fragment() {
        // A no-layout FRAGMENT body must stay a fragment after inlining:
        // opting into CSS inlining must not promote it into a full document by
        // introducing synthetic `<html>`/`<head>`/`<body>` wrappers. The class
        // rule is still inlined onto the element. See issue #1254 / PR #1681.
        let html = r#"<style>.x{color:red}</style><p class="x">Hi</p>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        assert!(
            !out.to_ascii_lowercase().contains("<html")
                && !out.to_ascii_lowercase().contains("<body")
                && !out.to_ascii_lowercase().contains("<head"),
            "fragment body must not gain document wrappers; got: {out}"
        );
        let para = out
            .split("<p")
            .nth(1)
            .expect("a <p> tag is present in the output");
        let para_open = &para[..para.find('>').expect("paragraph tag closes")];
        assert!(
            para_open.contains("style=") && para_open.contains("red"),
            "the class rule must be inlined onto the paragraph; got tag: {para_open}"
        );
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_fragment_body_retains_media_query_without_wrapping() {
        // AC5 + fragment: a FRAGMENT body with an un-inlinable `@media` rule
        // must stay a fragment (no synthetic wrappers) yet still carry the
        // retained `@media` block. Document-mode inlining hoists that retained
        // `<style>` into the synthetic `<head>`; the unwrap must fold it back
        // into the fragment rather than dropping it. See PR #1681.
        let html = r#"<style>.btn{color:#fff}@media (max-width:600px){.btn{color:#000}}</style><a class="btn">Go</a>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        assert!(
            !out.to_ascii_lowercase().contains("<html")
                && !out.to_ascii_lowercase().contains("<body")
                && !out.to_ascii_lowercase().contains("<head"),
            "fragment body must not gain document wrappers; got: {out}"
        );
        assert!(
            out.contains("@media") && out.contains("max-width"),
            "the retained @media block must survive the unwrap; got: {out}"
        );
        let anchor = out
            .split("<a")
            .nth(1)
            .expect("an <a> tag is present in the output");
        let anchor_open = &anchor[..anchor.find('>').expect("anchor tag closes")];
        assert!(
            anchor_open.contains("style=") && anchor_open.contains("#fff"),
            "the inlinable rule must still be inlined onto the anchor; got tag: {anchor_open}"
        );
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    fn inline_css_full_document_body_stays_a_document() {
        // A FULL-DOCUMENT body keeps document-mode handling: its authored
        // `<html>`/`<body>` structure survives and the class rule is inlined.
        let html = r#"<html><head><style>.x{color:red}</style></head><body><p class="x">Hi</p></body></html>"#;
        let out = inline_css_html(html).expect("inlining succeeds");
        assert!(
            out.to_ascii_lowercase().contains("<html")
                && out.to_ascii_lowercase().contains("<body"),
            "full-document body must retain its structure; got: {out}"
        );
        let para = out
            .split("<p")
            .nth(1)
            .expect("a <p> tag is present in the output");
        let para_open = &para[..para.find('>').expect("paragraph tag closes")];
        assert!(
            para_open.contains("style=") && para_open.contains("red"),
            "the class rule must be inlined onto the paragraph; got tag: {para_open}"
        );
    }

    #[test]
    fn inline_css_stripped_style_renders_same_computed_styling() {
        // AC7: a `<style>`-stripped copy of the inlined output renders the same
        // computed styling — i.e. the visual styling lives in the inline
        // `style="…"` attribute, independent of any `<head>`/`<style>` the
        // client might drop.
        let html = r#"<style>.btn{color:#fff;padding:8px}</style><a class="btn">Go</a>"#;
        let inlined = inline_css_html(html).expect("inlining succeeds");

        // Strip every <style>…</style> block (what Gmail/Outlook effectively do).
        let mut stripped = String::new();
        let mut rest = inlined.as_str();
        while let Some(start) = rest.to_ascii_lowercase().find("<style") {
            stripped.push_str(&rest[..start]);
            let after = &rest[start..];
            let end = after
                .to_ascii_lowercase()
                .find("</style>")
                .map_or(after.len(), |e| e + "</style>".len());
            rest = &after[end..];
        }
        stripped.push_str(rest);

        // The anchor's inline style survives the strip, so styling is unchanged.
        let anchor = stripped
            .split("<a")
            .nth(1)
            .expect("anchor present after stripping <style>");
        let anchor_open = &anchor[..anchor.find('>').expect("anchor closes")];
        assert!(
            anchor_open.contains("style=") && anchor_open.contains("#fff"),
            "computed styling must be carried inline so a style-stripped copy looks identical; got: {anchor_open}"
        );
    }

    // ── Preview honours send-time CSS inlining (issue #1254) ──────────────

    /// Render `show_template_preview` for a single registered preview and return
    /// the full response body as a string. A [`Mailer`] is installed on the
    /// state so the handler can reuse the send-time inlining decision — mirrors
    /// the app build, where the mailer is always present before the preview
    /// registry.
    async fn preview_body_for(preview: MailPreview) -> String {
        let state = crate::AppState::for_test();
        state.insert_extension(MailPreviewRegistry::new(vec![preview]));
        let mailer = Mailer::builder()
            .build()
            .expect("log-transport mailer builds");
        state.insert_extension(mailer);

        let response = show_template_preview(&state, "test", "styled");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("preview body collects");
        String::from_utf8(bytes.to_vec()).expect("preview body is utf-8")
    }

    /// Escaped opening `<a …>` tag of the email body as it appears in the
    /// preview page (the email HTML is HTML-escaped into an `<iframe srcdoc>`).
    /// Isolating the anchor keeps assertions discriminating: the colour rule
    /// also lives in the retained/original `<style>` block.
    fn escaped_anchor_open_tag(body: &str) -> String {
        let after = body
            .split("&lt;a")
            .nth(1)
            .expect("an <a> tag is present in the escaped preview body");
        let open = &after[..after.find("&gt;").expect("anchor tag closes")];
        open.to_owned()
    }

    #[tokio::test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    async fn preview_inlines_style_block_when_inlining_enabled() {
        // A preview whose Mail opts into inlining must reflect what strict
        // clients receive: the `.btn` class rule inlined onto the anchor.
        let preview = MailPreview::new("test", "styled", || {
            Mail::builder()
                .to("user@example.com")
                .subject("Styled")
                .html(r#"<style>.btn{color:#ff0000}</style><a class="btn">Go</a>"#)
                .inline_css(true)
                .build()
                .expect("preview mail builds")
        });

        let body = preview_body_for(preview).await;
        let anchor = escaped_anchor_open_tag(&body);
        assert!(
            anchor.contains("style="),
            "preview must inline the <style> block onto the anchor; got tag: {anchor}"
        );
        assert!(
            anchor.contains("#ff0000"),
            "the .btn colour rule must be carried inline; got tag: {anchor}"
        );
    }

    #[tokio::test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "CSS rule braces are literal HTML, not format placeholders"
    )]
    async fn preview_leaves_style_block_raw_when_inlining_disabled() {
        // The discriminating counterpart: with inlining off the anchor keeps no
        // inline style and the raw `<style>` block survives untouched.
        let preview = MailPreview::new("test", "styled", || {
            Mail::builder()
                .to("user@example.com")
                .subject("Styled")
                .html(r#"<style>.btn{color:#ff0000}</style><a class="btn">Go</a>"#)
                .inline_css(false)
                .build()
                .expect("preview mail builds")
        });

        let body = preview_body_for(preview).await;
        let anchor = escaped_anchor_open_tag(&body);
        assert!(
            !anchor.contains("style="),
            "inlining is off, so the anchor must not gain an inline style; got tag: {anchor}"
        );
        assert!(
            body.contains("&lt;style&gt;"),
            "the raw <style> block must survive when inlining is off"
        );
    }

    #[test]
    fn mail_builder_inline_css_sets_per_message_override() {
        let on = Mail::builder()
            .to("a@example.com")
            .subject("s")
            .html("<p>x</p>")
            .inline_css(true)
            .build()
            .expect("valid mail");
        assert_eq!(on.inline_css, Some(true));

        let off = Mail::builder()
            .to("a@example.com")
            .subject("s")
            .html("<p>x</p>")
            .inline_css(false)
            .build()
            .expect("valid mail");
        assert_eq!(off.inline_css, Some(false));

        let unset = Mail::builder()
            .to("a@example.com")
            .subject("s")
            .html("<p>x</p>")
            .build()
            .expect("valid mail");
        assert_eq!(
            unset.inline_css, None,
            "unset builder must defer to the mailer/config default"
        );
    }

    #[test]
    fn mail_config_inline_css_defaults_off() {
        assert!(
            !MailConfig::default().inline_css,
            "inlining must default off so existing apps are unaffected"
        );
    }

    // ── Attachments (issue #1256): pinning tests ──────────────────────────
    //
    // These prove attachment support introduces zero byte-for-byte regression
    // to attachment-less mail. `pinned_render_eml_no_attachments` is a frozen
    // copy of `render_eml`'s pre-attachment body captured before any
    // attachment code was added. Do not "fix" drift here — a diff against
    // this function IS the regression signal (AC: "pure additive, no
    // regression to existing email output").

    fn pinned_render_eml_no_attachments(mail: &Mail) -> String {
        let mut out = String::new();
        if let Some(from) = &mail.from {
            out.push_str("From: ");
            out.push_str(from);
            out.push('\n');
        }
        for to in &mail.to {
            out.push_str("To: ");
            out.push_str(to);
            out.push('\n');
        }
        if let Some(reply_to) = &mail.reply_to {
            out.push_str("Reply-To: ");
            out.push_str(reply_to);
            out.push('\n');
        }
        out.push_str("Date: ");
        out.push_str("PINNED-DATE");
        out.push('\n');
        out.push_str("Message-Id: <");
        out.push_str("PINNED-ID");
        out.push_str("@autumn.local>\n");
        out.push_str("Subject: ");
        out.push_str(&mail.subject);
        out.push('\n');
        for (name, value) in &mail.extra_headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        out.push_str("MIME-Version: 1.0\n");
        if mail.html.is_some() && mail.text.is_some() {
            out.push_str("Content-Type: multipart/alternative; boundary=\"autumn-mail\"\n\n");
            if let Some(text) = &mail.text {
                out.push_str("--autumn-mail\nContent-Type: text/plain; charset=utf-8\n\n");
                out.push_str(text);
                out.push('\n');
            }
            if let Some(html) = &mail.html {
                out.push_str("--autumn-mail\nContent-Type: text/html; charset=utf-8\n\n");
                out.push_str(html);
                out.push('\n');
            }
            out.push_str("--autumn-mail--\n");
        } else if let Some(html) = &mail.html {
            out.push_str("Content-Type: text/html; charset=utf-8\n\n");
            out.push_str(html);
            out.push('\n');
        } else if let Some(text) = &mail.text {
            out.push_str("Content-Type: text/plain; charset=utf-8\n\n");
            out.push_str(text);
            out.push('\n');
        }
        out
    }

    fn mask_nondeterministic(eml: &str) -> String {
        eml.lines()
            .map(|line| {
                if line.starts_with("Date: ") {
                    "Date: PINNED-DATE"
                } else if line.starts_with("Message-Id: ") {
                    "Message-Id: <PINNED-ID@autumn.local>"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_eml_without_attachments_matches_pinned_shape() {
        let mails = [
            Mail::builder()
                .from("from@example.com")
                .to("user@example.com")
                .subject("Hi")
                .text("hello text")
                .html("<p>hello html</p>")
                .build()
                .expect("mail should build"),
            Mail::builder()
                .from("from@example.com")
                .to("user@example.com")
                .subject("Hi")
                .text("hello text only")
                .build()
                .expect("mail should build"),
            Mail::builder()
                .from("from@example.com")
                .to("user@example.com")
                .subject("Hi")
                .html("<p>hello html only</p>")
                .build()
                .expect("mail should build"),
        ];
        for mail in mails {
            let actual = mask_nondeterministic(&render_eml(&mail));
            let pinned = mask_nondeterministic(&pinned_render_eml_no_attachments(&mail));
            assert_eq!(
                actual, pinned,
                "render_eml must be byte-identical for attachment-less mail"
            );
            assert!(!actual.contains("multipart/mixed"));
        }
    }

    #[test]
    fn lettre_message_without_attachments_has_no_mixed_part() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .html("<p>hello</p>")
            .build()
            .expect("mail should build");
        let message = lettre_message(&mail).expect("lettre message should build");
        let formatted = String::from_utf8_lossy(&message.formatted()).into_owned();
        assert!(formatted.contains("multipart/alternative"));
        assert!(!formatted.contains("multipart/mixed"));
    }

    // ── Attachments (issue #1256): model & builder ────────────────────────

    #[test]
    fn mail_builder_attach_preserves_order_and_count() {
        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach("a.txt", "text/plain", b"aaa".to_vec())
            .attach("b.txt", "text/plain", b"bbb".to_vec())
            .attach("c.txt", "text/plain", b"ccc".to_vec())
            .build()
            .expect("mail should build");
        assert_eq!(mail.attachments.len(), 3);
        assert_eq!(
            mail.attachments
                .iter()
                .map(|a| a.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["a.txt", "b.txt", "c.txt"]
        );
        assert_eq!(mail.attachments[1].content_type, "text/plain");
        assert_eq!(mail.attachments[1].bytes, b"bbb".to_vec());
    }

    #[test]
    fn mail_serde_round_trips_attachments() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach("invoice.pdf", "application/pdf", vec![0_u8, 1, 2, 255])
            .build()
            .expect("mail should build");
        let json = serde_json::to_string(&mail).expect("mail should serialize");
        let round_tripped: Mail = serde_json::from_str(&json).expect("mail should deserialize");
        assert_eq!(round_tripped, mail);
    }

    #[test]
    fn mail_builder_rejects_control_chars_in_attachment_filename() {
        let err = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach(
                "evil\r\nX-Injected: 1.pdf",
                "application/pdf",
                b"x".to_vec(),
            )
            .build()
            .expect_err("CRLF in filename should be rejected");
        assert!(err.to_string().contains("filename"));

        let err = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach("\0evil.pdf", "application/pdf", b"x".to_vec())
            .build()
            .expect_err("NUL in filename should be rejected");
        assert!(err.to_string().contains("filename"));

        let err = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach("   ", "application/pdf", b"x".to_vec())
            .build()
            .expect_err("empty filename should be rejected");
        assert!(err.to_string().contains("filename"));
    }

    #[test]
    fn mail_builder_rejects_invalid_attachment_content_type() {
        let err = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach("a.pdf", "not a mime type", b"x".to_vec())
            .build()
            .expect_err("invalid content type should be rejected");
        assert!(err.to_string().contains("content type"));
    }

    #[test]
    fn mail_attachment_debug_hides_bytes() {
        let attachment = MailAttachment {
            filename: "secret.bin".to_owned(),
            content_type: "application/octet-stream".to_owned(),
            bytes: vec![1, 2, 3, 4, 5],
        };
        let debug = format!("{attachment:?}");
        assert!(debug.contains("secret.bin"));
        assert!(debug.contains('5'), "byte length should appear: {debug}");
        assert!(
            !debug.contains("[1, 2, 3, 4, 5]"),
            "raw byte values must not appear: {debug}"
        );
    }

    // ── Attachments (issue #1256): render_eml (file transport) ───────────

    fn blob_all_byte_values() -> Vec<u8> {
        (0_u8..=255).cycle().take(4096).collect()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Extracts the random `multipart/mixed` boundary rendered by
    /// `render_eml` for an attachment message, so tests can assert against
    /// it without depending on a fixed boundary string.
    fn mixed_boundary(eml: &str) -> String {
        let line = eml
            .lines()
            .find(|line| line.starts_with("Content-Type: multipart/mixed;"))
            .expect("multipart/mixed Content-Type header present");
        content_type_boundary(line).expect("boundary parameter present")
    }

    #[test]
    fn render_eml_with_attachment_emits_multipart_mixed() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Invoice")
            .text("see attached")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let boundary = mixed_boundary(&eml);
        assert!(eml.contains(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary}\""
        )));
        assert!(eml.contains("Content-Disposition: attachment; filename=\"invoice.pdf\""));
        assert!(eml.contains("Content-Type: application/pdf"));
        assert!(eml.contains("Content-Transfer-Encoding: base64"));
        assert!(eml.contains(&format!("--{boundary}--")));
    }

    #[test]
    fn render_eml_boundary_is_unpredictable_and_body_cannot_forge_it() {
        // A fixed boundary (e.g. a literal `"autumn-mixed"`) lets a body
        // containing a `--autumn-mixed` line be mistaken for a real MIME
        // delimiter, truncating or splitting the message. The boundary must
        // vary per render and not be derivable from body content alone.
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Spoof attempt")
            .text("line one\n--autumn-mixed--\nX-Spoofed: header\nline two")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let parsed = parse_eml(&eml);
        assert_eq!(
            parsed.text.as_deref(),
            Some("line one\n--autumn-mixed--\nX-Spoofed: header\nline two"),
            "body content resembling the old fixed boundary must not truncate the message"
        );
        assert_eq!(parsed.attachments.len(), 1);

        let other = render_eml(
            &Mail::builder()
                .from("from@example.com")
                .to("user@example.com")
                .subject("Second message")
                .text("hi")
                .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
                .build()
                .expect("mail should build"),
        );
        assert_ne!(
            mixed_boundary(&eml),
            mixed_boundary(&other),
            "boundary must vary per message, not be a fixed/predictable string"
        );
    }

    #[test]
    fn render_eml_attachment_bytes_round_trip_sha256() {
        use base64::Engine as _;
        let blob = blob_all_byte_values();
        let expected_digest = sha256_hex(&blob);
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Blob")
            .text("see attached")
            .attach("blob.bin", "application/octet-stream", blob)
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let boundary = mixed_boundary(&eml);

        let start = eml
            .find("Content-Transfer-Encoding: base64\n\n")
            .expect("base64 section present")
            + "Content-Transfer-Encoding: base64\n\n".len();
        let rest = &eml[start..];
        let end = rest
            .find(&format!("--{boundary}"))
            .expect("closing boundary present");
        let encoded: String = rest[..end].chars().filter(|c| !c.is_whitespace()).collect();

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("attachment body should be valid base64");
        assert_eq!(sha256_hex(&decoded), expected_digest);
    }

    #[test]
    fn render_eml_preserves_attachment_order() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Multi")
            .text("see attached")
            .attach("a.txt", "text/plain", b"a".to_vec())
            .attach("b.txt", "text/plain", b"b".to_vec())
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let a_pos = eml.find("filename=\"a.txt\"").expect("a.txt present");
        let b_pos = eml.find("filename=\"b.txt\"").expect("b.txt present");
        assert!(a_pos < b_pos, "attachments must render in declared order");
    }

    #[test]
    fn render_eml_with_attachment_nests_alternative_body() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Both bodies")
            .text("plain")
            .html("<p>html</p>")
            .attach("a.txt", "text/plain", b"a".to_vec())
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        assert!(eml.contains("Content-Type: multipart/alternative; boundary=\"autumn-mail\""));
        assert!(eml.contains("plain"));
        assert!(eml.contains("<p>html</p>"));
    }

    #[test]
    fn render_eml_blocks_filename_header_injection() {
        // Hand-built Mail bypasses `build()`'s validation entirely — a `Mail`
        // can also arrive via `Deserialize` from a durable queue, so the
        // render layer must be injection-proof independent of the builder.
        let mail = Mail {
            from: Some("from@example.com".to_owned()),
            reply_to: None,
            to: vec!["user@example.com".to_owned()],
            subject: "Hi".to_owned(),
            html: None,
            text: Some("hello".to_owned()),
            list_unsubscribe: None,
            extra_headers: Vec::new(),
            attachments: vec![MailAttachment {
                filename: "evil\r\nX-Injected: 1.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                bytes: b"x".to_vec(),
            }],
            ignore_suppression: false,
            inline_css: None,
        };
        let eml = render_eml(&mail);
        assert!(
            !eml.lines().any(|line| line.starts_with("X-Injected")),
            "CRLF in filename must not inject a header: {eml}"
        );
        assert!(!eml.contains('\r'));
    }

    #[test]
    fn render_eml_blocks_header_injection_in_all_deserialized_fields() {
        // Same threat model as `render_eml_blocks_filename_header_injection`,
        // but for the pre-existing `subject`/`to`/`from`/`reply_to`/
        // `extra_headers` fields — these are just as reachable via an
        // untrusted `Deserialize`d `Mail` as the attachment filename is.
        let mail = Mail {
            from: Some("from@example.com\r\nX-From-Injected: 1".to_owned()),
            reply_to: Some("reply@example.com\r\nX-Reply-Injected: 1".to_owned()),
            to: vec!["user@example.com\r\nX-To-Injected: 1".to_owned()],
            subject: "Hi\r\nX-Subject-Injected: 1".to_owned(),
            html: None,
            text: Some("hello".to_owned()),
            list_unsubscribe: None,
            extra_headers: vec![(
                "X-Custom\r\nX-Header-Injected".to_owned(),
                "1\r\nX-Value-Injected: 1".to_owned(),
            )],
            attachments: Vec::new(),
            ignore_suppression: false,
            inline_css: None,
        };
        let eml = render_eml(&mail);
        assert!(
            !eml.lines().any(|line| line.starts_with("X-From-Injected")
                || line.starts_with("X-Reply-Injected")
                || line.starts_with("X-To-Injected")
                || line.starts_with("X-Subject-Injected")
                || line.starts_with("X-Header-Injected")
                || line.starts_with("X-Value-Injected")),
            "CRLF in any header-bound field must not inject a standalone header line: {eml}"
        );
        assert!(!eml.contains('\r'));
    }

    #[test]
    fn render_eml_falls_back_to_octet_stream_for_invalid_content_type() {
        // A `Mail` bypassing `build()` could carry a syntactically invalid
        // content type; the file transport must not write it verbatim, to
        // stay consistent with the SMTP transport (which rejects it).
        let mail = Mail {
            from: Some("from@example.com".to_owned()),
            reply_to: None,
            to: vec!["user@example.com".to_owned()],
            subject: "Hi".to_owned(),
            html: None,
            text: Some("hello".to_owned()),
            list_unsubscribe: None,
            extra_headers: Vec::new(),
            attachments: vec![MailAttachment {
                filename: "file.bin".to_owned(),
                content_type: "not a mime type".to_owned(),
                bytes: b"x".to_vec(),
            }],
            ignore_suppression: false,
            inline_css: None,
        };
        let eml = render_eml(&mail);
        assert!(eml.contains("Content-Type: application/octet-stream"));
        assert!(!eml.contains("not a mime type"));
    }

    #[test]
    fn render_eml_encodes_non_ascii_filename_rfc2231() {
        let mail = Mail {
            from: Some("from@example.com".to_owned()),
            reply_to: None,
            to: vec!["user@example.com".to_owned()],
            subject: "Hi".to_owned(),
            html: None,
            text: Some("hello".to_owned()),
            list_unsubscribe: None,
            extra_headers: Vec::new(),
            attachments: vec![MailAttachment {
                filename: "Résumé façade.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                bytes: b"x".to_vec(),
            }],
            ignore_suppression: false,
            inline_css: None,
        };
        let eml = render_eml(&mail);
        assert!(eml.contains("filename*=UTF-8''"));
        let disposition_line = eml
            .lines()
            .find(|line| line.starts_with("Content-Disposition:"))
            .expect("Content-Disposition header present");
        assert!(disposition_line.is_ascii());
    }

    #[test]
    fn content_disposition_params_table() {
        assert_eq!(content_disposition_params("a.txt"), "filename=\"a.txt\"");
        assert_eq!(
            content_disposition_params("weird\"na\\me.txt"),
            "filename=\"weird\\\"na\\\\me.txt\""
        );
        assert_eq!(
            content_disposition_params("evil\r\nX: 1"),
            "filename=\"evilX: 1\""
        );
        assert_eq!(content_disposition_params(""), "filename=\"attachment\"");
        assert_eq!(content_disposition_params("   "), "filename=\"attachment\"");
        let non_ascii = content_disposition_params("café.txt");
        assert!(non_ascii.contains("filename*=UTF-8''caf%C3%A9.txt"));
        assert!(non_ascii.is_ascii());
    }

    #[test]
    fn render_eml_base64_lines_wrap_at_76() {
        let blob = blob_all_byte_values();
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Blob")
            .text("see attached")
            .attach("blob.bin", "application/octet-stream", blob)
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let boundary = mixed_boundary(&eml);
        let start = eml
            .find("Content-Transfer-Encoding: base64\n\n")
            .expect("base64 section present")
            + "Content-Transfer-Encoding: base64\n\n".len();
        let rest = &eml[start..];
        let end = rest
            .find(&format!("--{boundary}"))
            .expect("closing boundary present");
        for line in rest[..end].lines() {
            assert!(
                line.len() <= 76,
                "base64 line too long: {} chars",
                line.len()
            );
        }
    }

    // ── Attachments (issue #1256): lettre_message (SMTP transport) ───────

    #[test]
    fn lettre_message_with_attachment_is_multipart_mixed() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Invoice")
            .text("see attached")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .build()
            .expect("mail should build");
        let message = lettre_message(&mail).expect("lettre message should build");
        let formatted = String::from_utf8_lossy(&message.formatted()).into_owned();
        assert!(formatted.contains("multipart/mixed"));
        assert!(formatted.contains("Content-Disposition: attachment"));
        assert!(formatted.contains("invoice.pdf"));
        assert!(formatted.contains("base64"));
    }

    fn extract_boundary(text: &str) -> String {
        let marker = "boundary=\"";
        let start = text.find(marker).expect("boundary present") + marker.len();
        let rest = &text[start..];
        let end = rest.find('"').expect("boundary closing quote");
        rest[..end].to_owned()
    }

    fn extract_attachment_base64(formatted_lf: &str, boundary: &str) -> String {
        let marker = format!("--{boundary}");
        for segment in formatted_lf.split(&marker).skip(1) {
            let segment = segment.trim_start_matches(['\n', '\r']);
            if segment.starts_with("--") {
                break;
            }
            let (headers, body) = split_headers_body(segment);
            if header_value(&headers, "Content-Disposition")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("attachment")
            {
                return body.chars().filter(|c| !c.is_whitespace()).collect();
            }
        }
        panic!("attachment part not found in: {formatted_lf}");
    }

    #[test]
    fn lettre_message_attachment_round_trips_sha256() {
        use base64::Engine as _;
        let blob = blob_all_byte_values();
        let expected_digest = sha256_hex(&blob);
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Blob")
            .text("see attached")
            .attach("blob.bin", "application/octet-stream", blob)
            .build()
            .expect("mail should build");
        let message = lettre_message(&mail).expect("lettre message should build");
        let formatted = String::from_utf8_lossy(&message.formatted()).into_owned();
        let normalized = formatted.replace("\r\n", "\n");
        let boundary = extract_boundary(&normalized);
        let encoded = extract_attachment_base64(&normalized, &boundary);

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("attachment body should be valid base64");
        assert_eq!(sha256_hex(&decoded), expected_digest);
    }

    #[test]
    fn lettre_message_attachment_headers_ascii_and_injection_free() {
        for filename in ["evil\r\nX-Injected: 1.pdf", "Résumé façade.pdf"] {
            let mail = Mail {
                from: Some("from@example.com".to_owned()),
                reply_to: None,
                to: vec!["user@example.com".to_owned()],
                subject: "Hi".to_owned(),
                html: None,
                text: Some("hello".to_owned()),
                list_unsubscribe: None,
                extra_headers: Vec::new(),
                attachments: vec![MailAttachment {
                    filename: filename.to_owned(),
                    content_type: "application/pdf".to_owned(),
                    bytes: b"x".to_vec(),
                }],
                ignore_suppression: false,
                inline_css: None,
            };
            let message = lettre_message(&mail).expect("lettre message should build");
            let formatted = String::from_utf8_lossy(&message.formatted()).into_owned();
            let header_section = formatted
                .split("\r\n\r\n")
                .next()
                .expect("header section present");
            assert!(
                header_section.is_ascii(),
                "headers must stay ASCII for filename {filename:?}: {header_section}"
            );
            assert!(
                !formatted.lines().any(|line| line.starts_with("X-Injected")),
                "CRLF in filename must not inject a header for {filename:?}"
            );
        }
    }

    #[test]
    fn lettre_message_attachment_with_invalid_content_type_errors() {
        let mail = Mail {
            from: Some("from@example.com".to_owned()),
            reply_to: None,
            to: vec!["user@example.com".to_owned()],
            subject: "Hi".to_owned(),
            html: None,
            text: Some("hello".to_owned()),
            list_unsubscribe: None,
            extra_headers: Vec::new(),
            attachments: vec![MailAttachment {
                filename: "a.bin".to_owned(),
                content_type: "not a mime type".to_owned(),
                bytes: b"x".to_vec(),
            }],
            ignore_suppression: false,
            inline_css: None,
        };
        let err = lettre_message(&mail).expect_err("invalid content type should error");
        assert!(matches!(err, MailError::InvalidMessage(_)));
    }

    // ── Attachments (issue #1256): dev preview ────────────────────────────

    #[test]
    fn parse_eml_extracts_attachment_list() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Invoice")
            .text("plain")
            .html("<p>html</p>")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .attach("receipt.csv", "text/csv", b"a,b,c".to_vec())
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let parsed = parse_eml(&eml);
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].filename, "invoice.pdf");
        assert_eq!(parsed.attachments[0].content_type, "application/pdf");
        assert_eq!(parsed.attachments[1].filename, "receipt.csv");
        assert_eq!(parsed.html.as_deref(), Some("<p>html</p>"));
        assert_eq!(parsed.text.as_deref(), Some("plain"));
    }

    #[test]
    fn extract_attachment_filename_handles_semicolon_in_quoted_filename() {
        // Naive `disposition.split(';')` would truncate this at "invoice".
        assert_eq!(
            extract_attachment_filename(r#"attachment; filename="invoice;2026.pdf""#),
            "invoice;2026.pdf"
        );
    }

    #[test]
    fn extract_attachment_filename_unescapes_quoted_pairs() {
        assert_eq!(
            extract_attachment_filename(r#"attachment; filename="weird\"na\\me.txt""#),
            "weird\"na\\me.txt"
        );
    }

    #[test]
    fn extract_attachment_filename_is_case_insensitive_and_handles_language_tag() {
        assert_eq!(
            extract_attachment_filename("attachment; filename*=utf-8'en'r%C3%A9sum%C3%A9.pdf"),
            "résumé.pdf"
        );
    }

    #[test]
    fn extract_attachment_filename_round_trips_through_dev_preview() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Invoice")
            .text("plain")
            .attach(r#"a;b"c\d.txt"#, "text/plain", b"x".to_vec())
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        let parsed = parse_eml(&eml);
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, r#"a;b"c\d.txt"#);
    }

    #[test]
    fn parse_multipart_mixed_does_not_misclassify_inline_as_attachment() {
        let (_, _, attachments) = parse_multipart_mixed(
            "--b\nContent-Disposition: inline; filename=\"my-attachment-notes.pdf\"\nContent-Type: text/plain\n\nhi\n--b--\n",
            "b",
        );
        assert!(
            attachments.is_empty(),
            "an `inline` disposition must not be classified as an attachment: {attachments:?}"
        );
    }

    #[test]
    fn mail_deserializes_from_pre_attachments_json_shape() {
        // `Mail::attachments` must default on a missing key so a `Mail`
        // serialized by an older binary (before this field existed) still
        // deserializes from a durable delivery queue during a rolling
        // deploy.
        let json = r#"{"from":null,"reply_to":null,"to":["a@example.com"],"subject":"hi","html":null,"text":"hello","list_unsubscribe":null,"extra_headers":[]}"#;
        let mail: Mail =
            serde_json::from_str(json).expect("pre-attachments JSON should deserialize");
        assert!(mail.attachments.is_empty());
    }

    #[test]
    fn render_mail_detail_lists_attachments() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Invoice")
            .text("plain")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .attach("receipt.csv", "text/csv", b"a,b,c".to_vec())
            .build()
            .expect("mail should build");
        let parsed = parse_eml(&render_eml(&mail));
        let detail = render_mail_detail(&parsed, "captured");
        assert!(detail.contains("Attachments (2)"));
        assert!(detail.contains("invoice.pdf"));
        assert!(detail.contains("receipt.csv"));
    }

    #[test]
    fn render_mail_detail_without_attachments_omits_section() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Plain")
            .text("plain")
            .build()
            .expect("mail should build");
        let parsed = parse_eml(&render_eml(&mail));
        let detail = render_mail_detail(&parsed, "captured");
        assert!(!detail.contains("Attachments"));
    }

    #[test]
    fn mail_builder_rejects_missing_body() {
        let err = Mail::builder()
            .to("user@example.com")
            .subject("Hello")
            .build()
            .expect_err("body should be required");
        assert!(err.to_string().contains("html or text"));
    }

    #[test]
    fn filename_sanitizer_keeps_safe_characters() {
        assert_eq!(
            sanitize_filename("Ada Lovelace <ada@example.com>"),
            "Ada_Lovelace__ada_example.com_"
        );
    }

    #[test]
    fn transport_default_is_disabled() {
        assert_eq!(Transport::default(), Transport::Disabled);
    }

    // ── List-Unsubscribe: Mail surface (Component 1) ─────────────────────────

    #[test]
    fn mail_defaults_have_no_unsubscribe_or_extra_headers() {
        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .build()
            .expect("mail should build");
        assert_eq!(mail.list_unsubscribe, None);
        assert!(mail.extra_headers.is_empty());
        assert!(mail.attachments.is_empty());
    }

    #[test]
    fn mail_builder_sets_list_unsubscribe_and_headers() {
        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .list_unsubscribe("weekly_digest")
            .header("X-Custom", "1")
            .build()
            .expect("mail should build");
        assert_eq!(mail.list_unsubscribe.as_deref(), Some("weekly_digest"));
        assert_eq!(
            mail.extra_headers,
            vec![("X-Custom".to_owned(), "1".to_owned())]
        );
    }

    // ── List-Unsubscribe: token signing (Component 2) ────────────────────────

    fn test_keys() -> crate::security::config::ResolvedSigningKeys {
        crate::security::config::ResolvedSigningKeys::new(
            b"unit-test-signing-key-0123456789".to_vec(),
            vec![],
        )
    }

    #[test]
    fn token_roundtrips_and_hides_subscriber() {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let keys = test_keys();
        let token =
            unsubscribe::sign_token(&keys, "ada@example.com", "weekly_digest", 4_000_000_000);
        assert!(
            !token.contains("ada@example.com"),
            "raw subscriber must not appear in the token: {token}"
        );
        // The address is encrypted, not merely base64-encoded: its base64url form
        // (which the old signed-token format embedded) must not appear either.
        assert!(
            !token.contains(&engine.encode("ada@example.com")),
            "base64 of subscriber must not appear — the payload must be encrypted: {token}"
        );
        let decoded = unsubscribe::verify_token(&keys, &token, 1_000).expect("token should verify");
        assert_eq!(decoded.subscriber, "ada@example.com");
        assert_eq!(decoded.list_id, "weekly_digest");
    }

    #[test]
    fn token_rejects_tamper_and_expiry() {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let keys = test_keys();
        let token =
            unsubscribe::sign_token(&keys, "ada@example.com", "weekly_digest", 4_000_000_000);
        // Flip a bit in the trailing GCM tag: AES-GCM authentication must reject it.
        let mut blob = engine.decode(&token).expect("token is base64");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        let tampered = engine.encode(&blob);
        assert_eq!(
            unsubscribe::verify_token(&keys, &tampered, 1_000),
            Err(unsubscribe::TokenError::BadSignature)
        );
        // Expired (now > expiry).
        let short = unsubscribe::sign_token(&keys, "ada@example.com", "weekly_digest", 100);
        assert_eq!(
            unsubscribe::verify_token(&keys, &short, 200),
            Err(unsubscribe::TokenError::Expired)
        );
    }

    #[test]
    fn token_verifies_under_rotated_previous_key() {
        let signer = crate::security::config::ResolvedSigningKeys::new(
            b"old-key-old-key-old-key-old-key!".to_vec(),
            vec![],
        );
        let token = unsubscribe::sign_token(&signer, "ada@example.com", "list", 4_000_000_000);
        let rotated = crate::security::config::ResolvedSigningKeys::new(
            b"new-key-new-key-new-key-new-key!".to_vec(),
            vec![b"old-key-old-key-old-key-old-key!".to_vec()],
        );
        assert!(unsubscribe::verify_token(&rotated, &token, 1_000).is_ok());
    }

    #[test]
    fn unsubscribe_url_includes_token_and_path() {
        let url = unsubscribe::unsubscribe_url("https://app.example.com/", "TOK");
        assert_eq!(url, "https://app.example.com/_autumn/unsubscribe?token=TOK");
    }

    // ── List-Unsubscribe: suppression store (Component 3) ────────────────────

    #[tokio::test]
    async fn in_memory_suppression_transitions() {
        let store = InMemorySuppressionStore::new();
        assert!(!store.is_suppressed("a@x.com", "list").await.unwrap());
        store.suppress("a@x.com", "list").await.unwrap();
        assert!(store.is_suppressed("a@x.com", "list").await.unwrap());
        // Scoped to (subscriber, list).
        assert!(!store.is_suppressed("a@x.com", "other").await.unwrap());
        assert!(!store.is_suppressed("b@x.com", "list").await.unwrap());
    }

    // ── List-Unsubscribe: header emission + send (Component 4) ───────────────

    #[test]
    fn render_eml_emits_extra_headers() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .header("List-Unsubscribe", "<https://x/u?token=t>, <mailto:u@x>")
            .header("List-Unsubscribe-Post", "List-Unsubscribe=One-Click")
            .build()
            .expect("mail should build");
        let eml = render_eml(&mail);
        assert!(eml.contains("List-Unsubscribe: <https://x/u?token=t>, <mailto:u@x>"));
        assert!(eml.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"));
    }

    #[test]
    fn render_eml_without_headers_has_no_unsubscribe() {
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .build()
            .expect("mail should build");
        assert!(!render_eml(&mail).contains("List-Unsubscribe"));
    }

    #[derive(Clone)]
    struct CapturingTransport {
        sent: Arc<std::sync::Mutex<Vec<Mail>>>,
    }

    impl MailTransport for CapturingTransport {
        fn send<'a>(
            &'a self,
            mail: Mail,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            Box::pin(async move {
                self.sent.lock().expect("sent lock").push(mail);
                Ok(())
            })
        }
    }

    fn unsubscribe_runtime(
        suppression: Option<Arc<dyn SuppressionStore>>,
    ) -> Arc<UnsubscribeRuntime> {
        Arc::new(UnsubscribeRuntime {
            base_url: Some("https://app.example.com".to_owned()),
            mailto: Some("unsub@example.com".to_owned()),
            signing_keys: Arc::new(test_keys()),
            ttl_days: 30,
            suppression,
        })
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn send_adds_headers_for_list_mail() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let mailer = Mailer::with_transport(transport).with_unsubscribe(unsubscribe_runtime(None));
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Digest")
            .text("hello")
            .list_unsubscribe("weekly_digest")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        let captured = sent.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let headers = &captured[0].extra_headers;
        assert!(headers.iter().any(|(n, v)| n == "List-Unsubscribe"
            && v.contains("/_autumn/unsubscribe?token=")
            && v.contains("mailto:unsub@example.com")));
        assert!(
            headers
                .iter()
                .any(|(n, v)| n == "List-Unsubscribe-Post" && v == "List-Unsubscribe=One-Click")
        );
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn send_replaces_manual_list_unsubscribe_with_generated_one_click() {
        // A template that opts into list_unsubscribe but also set a hand-rolled
        // List-Unsubscribe must end up with the generated per-recipient one-click
        // header (replace, not suppress), so RFC 8058 compliance isn't lost.
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let mailer = Mailer::with_transport(transport).with_unsubscribe(unsubscribe_runtime(None));
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Digest")
            .text("hello")
            .header("List-Unsubscribe", "<mailto:old@example.com>")
            .list_unsubscribe("weekly_digest")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        let captured = sent.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let headers = &captured[0].extra_headers;
        // Exactly one List-Unsubscribe, and it's the generated one-click (not the
        // stale manual value).
        let unsub: Vec<&String> = headers
            .iter()
            .filter(|(n, _)| n == "List-Unsubscribe")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(unsub.len(), 1);
        assert!(unsub[0].contains("/_autumn/unsubscribe?token="));
        assert!(!unsub[0].contains("old@example.com"));
        assert!(
            headers
                .iter()
                .any(|(n, v)| n == "List-Unsubscribe-Post" && v == "List-Unsubscribe=One-Click")
        );
    }

    #[tokio::test]
    async fn send_list_mail_rejects_invalid_recipient_before_delivery() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let mailer = Mailer::with_transport(transport).with_unsubscribe(unsubscribe_runtime(None));
        // Second recipient is syntactically invalid. The send must fail before
        // delivering to the first, so a retry cannot duplicate that send.
        let mail = Mail::builder()
            .from("from@example.com")
            .to("good@example.com")
            .to("not a valid address")
            .subject("Digest")
            .text("hello")
            .list_unsubscribe("weekly_digest")
            .build()
            .unwrap();
        let result = mailer.send(mail).await;
        assert!(result.is_err(), "invalid recipient must fail the send");
        assert!(
            sent.lock().unwrap().is_empty(),
            "no recipient may be delivered when the list contains an invalid address"
        );
    }

    #[tokio::test]
    async fn send_list_mail_suppression_error_fails_before_any_delivery() {
        // A suppression store that errors for one specific subscriber.
        struct FailingStore {
            fail_for: String,
        }
        impl SuppressionStore for FailingStore {
            fn is_suppressed<'a>(
                &'a self,
                subscriber: &'a str,
                _list_id: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>> {
                let fails = subscriber == self.fail_for;
                Box::pin(async move {
                    if fails {
                        Err(MailError::RuntimeUnavailable(
                            "store unavailable".to_owned(),
                        ))
                    } else {
                        Ok(false)
                    }
                })
            }
            fn suppress<'a>(
                &'a self,
                _subscriber: &'a str,
                _list_id: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async move { Ok(()) })
            }
        }

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let store: Arc<dyn SuppressionStore> = Arc::new(FailingStore {
            fail_for: "second@example.com".to_owned(),
        });
        let mailer =
            Mailer::with_transport(transport).with_unsubscribe(unsubscribe_runtime(Some(store)));
        // The second recipient's suppression lookup errors. The whole send must
        // fail before the first recipient is delivered, so a retry can't duplicate
        // that delivery.
        let mail = Mail::builder()
            .from("from@example.com")
            .to("first@example.com")
            .to("second@example.com")
            .subject("Digest")
            .text("hello")
            .list_unsubscribe("weekly_digest")
            .build()
            .unwrap();
        let result = mailer.send(mail).await;
        assert!(
            result.is_err(),
            "suppression-store error must fail the send"
        );
        assert!(
            sent.lock().unwrap().is_empty(),
            "no recipient may be delivered when a later suppression lookup fails"
        );
    }

    #[tokio::test]
    async fn send_skips_suppressed_recipient() {
        let store = Arc::new(InMemorySuppressionStore::new());
        store
            .suppress("user@example.com", "weekly_digest")
            .await
            .unwrap();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let mailer =
            Mailer::with_transport(transport).with_unsubscribe(unsubscribe_runtime(Some(store)));
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Digest")
            .text("hello")
            .list_unsubscribe("weekly_digest")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        assert!(
            sent.lock().unwrap().is_empty(),
            "suppressed recipient must be skipped"
        );
    }

    #[tokio::test]
    async fn send_list_mail_resolves_suppression_in_one_batched_call() {
        use std::sync::atomic::AtomicUsize;

        // A store that counts how many times each trait method is invoked, so
        // this test proves `send_list_mail` calls `is_suppressed_many` once
        // for the whole recipient batch instead of `is_suppressed` once per
        // recipient (the N+1 this change eliminates).
        struct CountingStore {
            is_suppressed_calls: AtomicUsize,
            is_suppressed_many_calls: AtomicUsize,
            suppressed: std::collections::HashSet<String>,
        }
        impl SuppressionStore for CountingStore {
            fn is_suppressed<'a>(
                &'a self,
                subscriber: &'a str,
                _list_id: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<bool, MailError>> + Send + 'a>> {
                self.is_suppressed_calls.fetch_add(1, Ordering::SeqCst);
                let hit = self.suppressed.contains(subscriber);
                Box::pin(async move { Ok(hit) })
            }
            fn is_suppressed_many<'a>(
                &'a self,
                subscribers: &'a [&'a str],
                _list_id: &'a str,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<std::collections::HashSet<String>, MailError>>
                        + Send
                        + 'a,
                >,
            > {
                self.is_suppressed_many_calls.fetch_add(1, Ordering::SeqCst);
                let hits: std::collections::HashSet<String> = subscribers
                    .iter()
                    .filter(|s| self.suppressed.contains(**s))
                    .map(|s| (*s).to_owned())
                    .collect();
                Box::pin(async move { Ok(hits) })
            }
            fn suppress<'a>(
                &'a self,
                _subscriber: &'a str,
                _list_id: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async move { Ok(()) })
            }
        }

        let mut suppressed = std::collections::HashSet::new();
        suppressed.insert("banned@example.com".to_owned());
        let store = Arc::new(CountingStore {
            is_suppressed_calls: AtomicUsize::new(0),
            is_suppressed_many_calls: AtomicUsize::new(0),
            suppressed,
        });
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let mailer = Mailer::with_transport(transport)
            .with_unsubscribe(unsubscribe_runtime(Some(store.clone())));
        let mail = Mail::builder()
            .from("from@example.com")
            .to("first@example.com")
            .to("banned@example.com")
            .to("third@example.com")
            .subject("Digest")
            .text("hello")
            .list_unsubscribe("weekly_digest")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();

        assert_eq!(
            store.is_suppressed_many_calls.load(Ordering::SeqCst),
            1,
            "suppression for the whole recipient batch must resolve in exactly one call, \
             regardless of recipient count"
        );
        assert_eq!(
            store.is_suppressed_calls.load(Ordering::SeqCst),
            0,
            "the per-recipient is_suppressed path must not be used when a batch override exists"
        );
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "only the two non-suppressed recipients are delivered"
        );
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn send_without_scope_is_unchanged() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = CapturingTransport { sent: sent.clone() };
        let mailer = Mailer::with_transport(transport).with_unsubscribe(unsubscribe_runtime(None));
        let mail = Mail::builder()
            .from("from@example.com")
            .to("user@example.com")
            .subject("Reset")
            .text("hello")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        let captured = sent.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].extra_headers.is_empty(),
            "non-list mail must not gain headers"
        );
    }

    // ── List-Unsubscribe: startup fail-closed (Component 6) ──────────────────

    #[test]
    fn fail_closed_only_in_prod_with_mailers_and_no_config() {
        assert!(unsubscribe_config_fail_closed(true, true, true, false));
        // configured → ok
        assert!(!unsubscribe_config_fail_closed(true, true, true, true));
        // no list mailers → ok
        assert!(!unsubscribe_config_fail_closed(true, true, false, false));
        // not production → ok
        assert!(!unsubscribe_config_fail_closed(true, false, true, false));
        // not enforced (static build) → ok
        assert!(!unsubscribe_config_fail_closed(false, true, true, false));
    }

    #[test]
    fn validate_rejects_non_positive_unsubscribe_ttl() {
        let ttl = |days: i64| MailConfig {
            unsubscribe_token_ttl_days: days,
            ..MailConfig::default()
        };
        assert!(ttl(0).validate(Some("dev")).is_err());
        assert!(ttl(-1).validate(Some("dev")).is_err());
        assert!(ttl(30).validate(Some("dev")).is_ok());
    }

    #[test]
    fn unsubscribe_base_url_set_tracks_config() {
        let with = |base: Option<&str>, mailto: Option<&str>| MailConfig {
            unsubscribe_base_url: base.map(str::to_owned),
            unsubscribe_mailto: mailto.map(str::to_owned),
            ..MailConfig::default()
        };
        assert!(!with(None, None).unsubscribe_base_url_set());
        // mailto-only is not a base URL (RFC 2369, not one-click).
        assert!(!with(None, Some("u@example.com")).unsubscribe_base_url_set());
        assert!(with(Some("https://x"), None).unsubscribe_base_url_set());
        assert!(!with(Some("   "), None).unsubscribe_base_url_set());
    }

    #[test]
    fn should_mount_unsubscribe_endpoint_requires_opt_in_and_base_url() {
        let cfg = |base: Option<&str>, opt_in: bool| MailConfig {
            unsubscribe_base_url: base.map(str::to_owned),
            mount_unsubscribe_endpoint: opt_in,
            ..MailConfig::default()
        };
        // base URL alone does not mount — opt-in is required.
        assert!(!cfg(Some("https://x"), false).should_mount_unsubscribe_endpoint());
        assert!(cfg(Some("https://x"), true).should_mount_unsubscribe_endpoint());
        // opt-in without a base URL does not mount.
        assert!(!cfg(None, true).should_mount_unsubscribe_endpoint());
    }

    #[test]
    fn validate_rejects_malformed_mailto_in_prod() {
        let cfg = |mailto: &str| MailConfig {
            unsubscribe_mailto: Some(mailto.to_owned()),
            ..MailConfig::default()
        };
        assert!(
            cfg("unsubscribe example.com")
                .validate(Some("prod"))
                .is_err()
        );
        assert!(cfg("not-an-email").validate(Some("prod")).is_err());
        assert!(cfg("unsub@example.com").validate(Some("prod")).is_ok());
        // a full mailto: URI is accepted too.
        assert!(
            cfg("mailto:unsub@example.com")
                .validate(Some("prod"))
                .is_ok()
        );
        // dev is lenient.
        assert!(cfg("whatever").validate(Some("dev")).is_ok());
    }

    #[test]
    fn validate_requires_https_base_url_in_prod() {
        let cfg = |url: &str| MailConfig {
            unsubscribe_base_url: Some(url.to_owned()),
            ..MailConfig::default()
        };
        assert!(
            cfg("http://app.example.com")
                .validate(Some("prod"))
                .is_err()
        );
        assert!(
            cfg("https://app.example.com")
                .validate(Some("prod"))
                .is_ok()
        );
        // dev allows http for local testing.
        assert!(cfg("http://localhost:3000").validate(Some("dev")).is_ok());
        // https prefix without a real host is rejected in prod.
        assert!(cfg("https://").validate(Some("prod")).is_err());
        assert!(cfg("https:///path").validate(Some("prod")).is_err());
        // query/fragment bases would break the appended ?token=… link.
        assert!(
            cfg("https://app.example.com?t=acme")
                .validate(Some("prod"))
                .is_err()
        );
        assert!(
            cfg("https://app.example.com#x")
                .validate(Some("prod"))
                .is_err()
        );
        assert!(
            cfg("https://app.example.com/base")
                .validate(Some("prod"))
                .is_ok()
        );
    }

    #[test]
    fn canonical_subscriber_strips_name_and_lowercases() {
        assert_eq!(
            canonical_subscriber("Ada Lovelace <Ada@Example.com>"),
            "ada@example.com"
        );
        assert_eq!(canonical_subscriber("USER@EXAMPLE.COM"), "user@example.com");
    }

    #[test]
    fn mailto_only_runtime_does_not_support_one_click() {
        let runtime = UnsubscribeRuntime {
            base_url: None,
            mailto: Some("u@example.com".to_owned()),
            signing_keys: Arc::new(test_keys()),
            ttl_days: 30,
            suppression: None,
        };
        assert!(!runtime.supports_one_click());
        let header = runtime
            .list_unsubscribe_header("a@x.com", "list")
            .expect("mailto header");
        assert!(header.contains("mailto:u@example.com"));
        assert!(!header.contains("token="));
    }

    #[test]
    fn mailto_value_with_scheme_is_not_double_prefixed() {
        let runtime = UnsubscribeRuntime {
            base_url: None,
            mailto: Some("mailto:u@example.com".to_owned()),
            signing_keys: Arc::new(test_keys()),
            ttl_days: 30,
            suppression: None,
        };
        let header = runtime
            .list_unsubscribe_header("a@x.com", "list")
            .expect("mailto header");
        assert!(header.contains("<mailto:u@example.com?subject=unsubscribe>"));
        assert!(!header.contains("mailto:mailto:"));
    }

    #[test]
    fn one_click_body_detection() {
        assert!(is_one_click_body("List-Unsubscribe=One-Click"));
        assert!(is_one_click_body("foo=bar&List-Unsubscribe=One-Click"));
        assert!(is_one_click_body("list-unsubscribe=one-click")); // case-insensitive
        assert!(!is_one_click_body(""));
        assert!(!is_one_click_body("List-Unsubscribe=Nope"));
        assert!(!is_one_click_body("something=else"));
    }

    #[test]
    fn smtp_config_validation_rejects_whitespace_only_host() {
        let config = MailConfig {
            transport: Transport::Smtp,
            smtp: SmtpConfig {
                host: Some("   ".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = config
            .validate(Some("dev"))
            .expect_err("whitespace SMTP host should be rejected");

        assert!(error.to_string().contains("mail.smtp.host is required"));
    }

    #[test]
    fn transport_env_value_is_trimmed_and_case_insensitive() {
        assert_eq!(Transport::from_env_value(" SMTP "), Some(Transport::Smtp));
        assert_eq!(Transport::from_env_value(" LoG "), Some(Transport::Log));
    }

    #[test]
    fn tls_mode_env_value_is_trimmed_and_case_insensitive() {
        assert_eq!(TlsMode::from_env_value(" TLS "), Some(TlsMode::Tls));
        assert_eq!(
            TlsMode::from_env_value(" START_TLS "),
            Some(TlsMode::StartTls)
        );
        assert_eq!(
            TlsMode::from_env_value(" disabled "),
            Some(TlsMode::Disabled)
        );
    }

    #[test]
    fn file_transport_filename_is_unique_for_same_recipient() {
        let mail = Mail::builder()
            .to("Ada Lovelace <ada@example.com>")
            .subject("Hello")
            .text("body")
            .build()
            .expect("mail should build");

        let first = file_transport_filename(&mail);
        let second = file_transport_filename(&mail);

        assert_ne!(first, second);
        assert!(
            Path::new(&first)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("eml"))
        );
        assert!(
            Path::new(&second)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("eml"))
        );
    }

    #[test]
    fn smtp_transport_rejects_missing_password_env_when_username_is_set() {
        let missing_key = format!(
            "AUTUMN_TEST_MISSING_SMTP_PASSWORD_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let Err(error) = SmtpTransport::new(
            SmtpConfig {
                host: Some("smtp.example.com".to_owned()),
                port: Some(587),
                username: Some("mailer".to_owned()),
                password_env: Some(missing_key.clone()),
                tls: TlsMode::StartTls,
            },
            None,
        ) else {
            panic!("missing password env should fail at startup");
        };

        let displayed = error.to_string();
        assert!(displayed.contains(&missing_key));
        assert!(displayed.contains("environment variable is not set"));
    }

    #[test]
    fn smtp_password_env_error_never_embeds_the_secret_value() {
        // `std::env::VarError::NotUnicode` carries the raw contents of the
        // environment variable — i.e. the SMTP password itself. Formatting
        // that error directly (`{error}` or `{error:?}`) would leak the
        // secret into startup logs, so the redacting helper must map it to a
        // static reason instead.
        let secret = "hunter2-super-secret-password";
        let error = std::env::VarError::NotUnicode(std::ffi::OsString::from(secret));
        // Sanity check: the raw VarError does expose the value, which is
        // exactly why it must never be formatted into a MailError.
        assert!(error.to_string().contains(secret));
        assert!(format!("{error:?}").contains(secret));

        let mail_error = smtp_password_env_error("APP_SMTP_PASSWORD", &error);
        let displayed = mail_error.to_string();
        let debugged = format!("{mail_error:?}");
        assert!(
            !displayed.contains(secret),
            "Display output leaked the SMTP password: {displayed}"
        );
        assert!(
            !debugged.contains(secret),
            "Debug output leaked the SMTP password: {debugged}"
        );
        // The env var *name* is ordinary configuration and stays in the
        // message so operators can tell which variable is misconfigured.
        assert!(displayed.contains("APP_SMTP_PASSWORD"));
        assert!(displayed.contains("environment variable contains non-unicode data"));
    }

    #[test]
    fn smtp_password_env_error_redacts_missing_variable_details() {
        let error = std::env::VarError::NotPresent;
        let mail_error = smtp_password_env_error("APP_SMTP_PASSWORD", &error);
        let displayed = mail_error.to_string();
        assert!(displayed.contains("APP_SMTP_PASSWORD"));
        assert!(displayed.contains("environment variable is not set"));
    }

    #[test]
    fn smtp_transport_rejects_missing_password_env_key_when_username_is_set() {
        let Err(error) = SmtpTransport::new(
            SmtpConfig {
                host: Some("smtp.example.com".to_owned()),
                port: Some(587),
                username: Some("mailer".to_owned()),
                password_env: None,
                tls: TlsMode::StartTls,
            },
            None,
        ) else {
            panic!("missing password_env setting should fail at startup");
        };

        assert!(error.to_string().contains("mail.smtp.password_env"));
    }

    #[test]
    fn mailer_builder_rejects_invalid_default_from_address() {
        let Err(error) = Mailer::builder().from("not an email address").build() else {
            panic!("invalid default from should fail fast");
        };

        match error {
            MailError::InvalidAddress { address, .. } => {
                assert_eq!(address, "not an email address");
            }
            other => panic!("expected invalid address error, got {other:?}"),
        }
    }

    #[test]
    fn mailer_from_config_rejects_invalid_default_reply_to_address() {
        let config = MailConfig {
            transport: Transport::Smtp,
            from: Some("Autumn <noreply@example.com>".to_owned()),
            reply_to: Some("definitely not an address".to_owned()),
            smtp: SmtpConfig {
                host: Some("smtp.example.com".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };

        let Err(error) = Mailer::from_config(&config) else {
            panic!("invalid configured reply-to should fail at construction");
        };

        match error {
            MailError::InvalidAddress { address, .. } => {
                assert_eq!(address, "definitely not an address");
            }
            other => panic!("expected invalid address error, got {other:?}"),
        }
    }

    #[test]
    fn try_deliver_later_returns_error_without_runtime() {
        let mailer = Mailer::builder().build().expect("mailer should build");
        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hello")
            .text("hello")
            .build()
            .expect("mail should build");

        let error = mailer
            .try_deliver_later(mail)
            .expect_err("missing runtime should return an error");

        assert!(error.to_string().contains("active Tokio runtime"));
    }

    #[test]
    fn deliver_later_does_not_panic_without_runtime() {
        let mailer = Mailer::builder().build().expect("mailer should build");
        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hello")
            .text("hello")
            .build()
            .expect("mail should build");

        mailer.deliver_later(mail);
    }

    fn sample_smtp_config() -> MailConfig {
        MailConfig {
            transport: Transport::Smtp,
            from: Some("Autumn <noreply@example.com>".to_owned()),
            smtp: SmtpConfig {
                host: Some("smtp.example.com".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn sample_mail() -> Mail {
        Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .build()
            .expect("mail should build")
    }

    struct NoopQueue;

    impl MailDeliveryQueue for NoopQueue {
        fn enqueue<'a>(
            &'a self,
            _mail: Mail,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn install_mailer_boots_in_prod_without_ack_but_blocks_deliver_later() {
        // Issue #2142: a missing durable queue + ack must not crash app
        // startup — only the `deliver_later` call path should fail, and only
        // if the app actually reaches it.
        let state = crate::AppState::for_test().with_profile("prod");
        let config = sample_smtp_config();

        install_mailer(&state, &config, true)
            .expect("missing durable queue/ack must not fail app boot");

        let installed = state
            .extension::<Mailer>()
            .expect("install_mailer should store a Mailer extension");

        let error = installed
            .try_deliver_later(sample_mail())
            .expect_err("deliver_later without a durable queue or ack must fail lazily in prod");
        let message = error.to_string();
        assert!(
            message.contains("allow_in_process_deliver_later_in_production"),
            "error should explain how to opt in: {message}"
        );

        let error = installed
            .try_deliver_later_eager(sample_mail())
            .expect_err("deliver_later_eager must fail the same way");
        assert!(
            error
                .to_string()
                .contains("allow_in_process_deliver_later_in_production")
        );
    }

    #[test]
    fn install_mailer_allows_in_process_fallback_in_prod_with_explicit_ack() {
        let state = crate::AppState::for_test().with_profile("prod");
        let config = MailConfig {
            allow_in_process_deliver_later_in_production: true,
            ..sample_smtp_config()
        };

        install_mailer(&state, &config, true).expect("explicit ack should permit fallback in prod");
    }

    #[test]
    fn install_mailer_allows_durable_queue_in_prod_without_ack() {
        let state = crate::AppState::for_test().with_profile("prod");
        state.insert_extension(MailDeliveryQueueHandle::new(NoopQueue));
        let config = sample_smtp_config();

        install_mailer(&state, &config, true)
            .expect("a registered durable queue should satisfy the prod guard");
    }

    #[test]
    fn install_mailer_does_not_require_ack_outside_production() {
        let state = crate::AppState::for_test().with_profile("dev");
        let config = sample_smtp_config();

        install_mailer(&state, &config, true).expect("non-prod profiles should not require an ack");
    }

    #[test]
    fn install_mailer_does_not_require_ack_when_transport_is_disabled() {
        let state = crate::AppState::for_test().with_profile("prod");
        let config = MailConfig::default();

        install_mailer(&state, &config, true)
            .expect("disabled transport never sends mail so it should not need an ack");
    }

    struct CapturingQueue {
        tx: tokio::sync::mpsc::UnboundedSender<Mail>,
    }

    impl MailDeliveryQueue for CapturingQueue {
        fn enqueue<'a>(
            &'a self,
            mail: Mail,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            let tx = self.tx.clone();
            Box::pin(async move {
                tx.send(mail)
                    .map_err(|err| MailError::RuntimeUnavailable(err.to_string()))?;
                Ok(())
            })
        }
    }

    #[cfg(feature = "db")]
    struct FailingQueue {
        tx: tokio::sync::mpsc::UnboundedSender<Mail>,
    }

    #[cfg(feature = "db")]
    impl MailDeliveryQueue for FailingQueue {
        fn enqueue<'a>(
            &'a self,
            mail: Mail,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            let tx = self.tx.clone();
            Box::pin(async move {
                tx.send(mail)
                    .map_err(|err| MailError::RuntimeUnavailable(err.to_string()))?;
                Err(MailError::RuntimeUnavailable("queue offline".to_owned()))
            })
        }
    }

    #[cfg(feature = "db")]
    async fn drain_after_commit_callbacks_for_test(
        registry: &std::sync::Arc<std::sync::Mutex<Vec<crate::db::CommitCallback>>>,
    ) {
        let callbacks: Vec<crate::db::CommitCallback> = {
            let mut reg = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *reg)
        };

        for cb in callbacks {
            if let Err(error) = cb().await {
                crate::db::record_after_commit_failure();
                tracing::error!("test drain: after_commit callback failed: {error}");
            }
        }
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn deferred_deliver_later_queue_failure_increments_after_commit_counter() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Mail>();
        let mailer = Mailer::builder()
            .delivery_queue(FailingQueue { tx })
            .build()
            .expect("mailer should build");
        let registry = std::sync::Arc::new(std::sync::Mutex::new(
            Vec::<crate::db::CommitCallback>::new(),
        ));
        let before =
            crate::db::AFTER_COMMIT_FAILURES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);

        crate::db::AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                mailer
                    .try_deliver_later(sample_mail())
                    .expect("registering deferred mail should succeed");
            })
            .await;

        drain_after_commit_callbacks_for_test(&registry).await;

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("queue should be called within 1s")
            .expect("queue should receive the mail");
        assert_eq!(received.subject, "Hi");

        let after =
            crate::db::AFTER_COMMIT_FAILURES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after > before,
            "deferred durable mail handoff failures should count as after_commit failures"
        );
    }

    #[tokio::test]
    async fn deliver_later_routes_through_configured_queue() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Mail>();

        let mailer = Mailer::builder()
            .delivery_queue(CapturingQueue { tx })
            .build()
            .expect("mailer should build");

        mailer
            .try_deliver_later(sample_mail())
            .expect("scheduling onto the queue should succeed");

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("queue should receive within 1s")
            .expect("queue should receive the mail");

        assert_eq!(received.subject, "Hi");
    }

    #[tokio::test]
    async fn deliver_later_preserves_attachments_through_queue() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Mail>();

        let mailer = Mailer::builder()
            .delivery_queue(CapturingQueue { tx })
            .build()
            .expect("mailer should build");

        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .text("hello")
            .attach("invoice.pdf", "application/pdf", b"%PDF-1.4".to_vec())
            .build()
            .expect("mail should build");

        mailer
            .try_deliver_later(mail.clone())
            .expect("scheduling onto the queue should succeed");

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("queue should receive within 1s")
            .expect("queue should receive the mail");

        // The deferred path freezes the originating mailer's CSS-inlining default
        // onto the message before enqueue (issue #1254); this mailer defaults
        // inlining off, so the enqueued job carries `Some(false)` where the
        // source had `None`. Everything else (notably attachments) is untouched.
        let mut expected = mail;
        expected.inline_css = Some(false);
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn deferred_enqueue_freezes_originating_inline_css_default() {
        // A mailer whose config defaults CSS inlining ON must record that
        // decision on the persisted job when the message carries no explicit
        // override, so a worker consuming the durable queue (with a possibly
        // different/off default) still inlines. Only the flag is frozen — the
        // body is left un-inlined so the single inline pass happens once at the
        // consumer's send() (issue #1254).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Mail>();
        let mailer = Mailer::builder()
            .inline_css(true)
            .delivery_queue(CapturingQueue { tx })
            .build()
            .expect("mailer should build");

        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .html(
                "<html><head><style>p { color: red; }</style></head><body><p>hi</p></body></html>",
            )
            .build()
            .expect("mail should build");
        assert_eq!(mail.inline_css, None, "sample relies on the mailer default");

        mailer
            .try_deliver_later(mail)
            .expect("scheduling onto the queue should succeed");

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("queue should receive within 1s")
            .expect("queue should receive the mail");

        assert_eq!(
            received.inline_css,
            Some(true),
            "the originating mailer's inlining default must be frozen onto the enqueued job"
        );
        assert!(
            received
                .html
                .as_deref()
                .expect("html body")
                .contains("<style>"),
            "the body must be left un-inlined at enqueue time; inlining happens once at the consumer's send()"
        );
    }

    #[tokio::test]
    async fn deferred_enqueue_preserves_explicit_inline_css_override() {
        // An explicit per-message `inline_css(false)` opt-out must survive the
        // durable-queue handoff and never be clobbered by the mailer default.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Mail>();
        let mailer = Mailer::builder()
            .inline_css(true)
            .delivery_queue(CapturingQueue { tx })
            .build()
            .expect("mailer should build");

        let mail = Mail::builder()
            .to("user@example.com")
            .subject("Hi")
            .html(
                "<html><head><style>p { color: red; }</style></head><body><p>hi</p></body></html>",
            )
            .inline_css(false)
            .build()
            .expect("mail should build");

        mailer
            .try_deliver_later(mail)
            .expect("scheduling onto the queue should succeed");

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("queue should receive within 1s")
            .expect("queue should receive the mail");

        assert_eq!(
            received.inline_css,
            Some(false),
            "an explicit per-message override must be preserved through the queue, not overwritten by the mailer default"
        );
    }

    #[tokio::test]
    async fn deliver_later_without_queue_sends_via_transport_directly() {
        // When no delivery queue is configured, `spawn_mail_delivery` falls back to
        // calling `mailer.send()` in a background task.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TrackingSend(Arc<AtomicBool>);
        impl MailTransport for TrackingSend {
            fn send<'a>(
                &'a self,
                _mail: Mail,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                self.0.store(true, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
        }

        let sent = Arc::new(AtomicBool::new(false));
        let mailer = Mailer::with_transport(TrackingSend(sent.clone()));

        mailer
            .try_deliver_later(sample_mail())
            .expect("should succeed without queue");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            sent.load(Ordering::SeqCst),
            "mail should have been sent directly via transport"
        );
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn deferred_deliver_later_without_queue_sends_after_commit() {
        // After-commit callback with no queue falls back to `spawn_mail_delivery`
        // which calls `mailer.send()` in a spawned task.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TrackingSend(Arc<AtomicBool>);
        impl MailTransport for TrackingSend {
            fn send<'a>(
                &'a self,
                _mail: Mail,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                self.0.store(true, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
        }

        let sent = Arc::new(AtomicBool::new(false));
        let mailer = Mailer::with_transport(TrackingSend(sent.clone()));
        let registry = std::sync::Arc::new(std::sync::Mutex::new(
            Vec::<crate::db::CommitCallback>::new(),
        ));

        crate::db::AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                mailer
                    .try_deliver_later(sample_mail())
                    .expect("should succeed");
            })
            .await;

        drain_after_commit_callbacks_for_test(&registry).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            sent.load(Ordering::SeqCst),
            "mail should have been sent after commit via direct transport"
        );
    }

    #[tokio::test]
    async fn mailer_with_transport_starts_without_delivery_queue() {
        let mailer = Mailer::with_transport(NoopTransport);
        assert!(
            !mailer.has_durable_delivery_queue(),
            "with_transport should default to no durable queue"
        );
        // Exercise NoopTransport::send so its body is also covered.
        mailer
            .send(sample_mail())
            .await
            .expect("noop transport should always succeed");
    }

    struct NoopTransport;
    impl MailTransport for NoopTransport {
        fn send<'a>(
            &'a self,
            _mail: Mail,
        ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn deliver_later_is_noop_when_transport_disabled_even_with_queue() {
        // The Mailer-level builder lets callers attach a queue *and* pick
        // Transport::Disabled. The disabled-transport contract requires
        // deliver_later to drop the message in that case — the queue must
        // not persist mail when the operator has turned mail off entirely.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Mail>();
        let mailer = Mailer::builder()
            .transport(Transport::Disabled)
            .delivery_queue(CapturingQueue { tx })
            .build()
            .expect("mailer should build");

        mailer
            .try_deliver_later(sample_mail())
            .expect("disabled transport should succeed as a no-op");

        // Wait briefly for any spawn that might erroneously fire to land.
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(
            received.is_err(),
            "queue must not be invoked when transport is disabled"
        );
    }

    #[tokio::test]
    async fn deliver_later_uses_in_process_fallback_when_no_queue() {
        // The default Mailer has no durable queue, so deliver_later should
        // still spawn the in-process Tokio task and not call any queue.
        let mailer = Mailer::builder().build().expect("mailer should build");

        mailer
            .try_deliver_later(sample_mail())
            .expect("in-process fallback should still schedule");
    }

    #[test]
    fn mail_delivery_queue_handle_round_trips_via_from_arc_and_inner() {
        let arc: Arc<dyn MailDeliveryQueue> = Arc::new(NoopQueue);
        let handle = MailDeliveryQueueHandle::from_arc(Arc::clone(&arc));

        assert!(Arc::ptr_eq(handle.inner(), &arc));
    }

    #[test]
    fn mail_delivery_queue_handle_debug_does_not_panic() {
        let handle = MailDeliveryQueueHandle::new(NoopQueue);
        let rendered = format!("{handle:?}");
        assert!(rendered.contains("MailDeliveryQueueHandle"));
    }

    #[test]
    fn mailer_has_durable_delivery_queue_reflects_attachment() {
        let plain = Mailer::builder().build().expect("mailer should build");
        assert!(!plain.has_durable_delivery_queue());

        let with_queue = Mailer::builder()
            .delivery_queue(NoopQueue)
            .build()
            .expect("mailer should build");
        assert!(with_queue.has_durable_delivery_queue());
    }

    #[test]
    fn mailer_with_delivery_queue_post_build_attaches_queue() {
        let mailer = Mailer::builder()
            .build()
            .expect("mailer should build")
            .with_delivery_queue(NoopQueue);

        assert!(mailer.has_durable_delivery_queue());
    }

    #[test]
    fn mailer_builder_delivery_queue_arc_attaches_shared_queue() {
        let arc: Arc<dyn MailDeliveryQueue> = Arc::new(NoopQueue);
        let mailer = Mailer::builder()
            .delivery_queue_arc(arc)
            .build()
            .expect("mailer should build");

        assert!(mailer.has_durable_delivery_queue());
    }

    #[test]
    fn install_mailer_warns_but_succeeds_with_explicit_ack_in_prod() {
        // Same as the explicit-ack test, but also asserts the mailer was
        // actually inserted and has no durable queue attached.
        let state = crate::AppState::for_test().with_profile("prod");
        let config = MailConfig {
            allow_in_process_deliver_later_in_production: true,
            ..sample_smtp_config()
        };

        install_mailer(&state, &config, true).expect("explicit ack should permit fallback in prod");

        let installed = state
            .extension::<Mailer>()
            .expect("install_mailer should store a Mailer extension");
        assert!(
            !installed.has_durable_delivery_queue(),
            "no queue was registered, so installed mailer should fall back in-process"
        );
    }

    #[test]
    fn install_mailer_attaches_registered_queue_to_mailer() {
        let state = crate::AppState::for_test().with_profile("prod");
        state.insert_extension(MailDeliveryQueueHandle::new(NoopQueue));
        let config = sample_smtp_config();

        install_mailer(&state, &config, true).expect("durable queue should permit prod startup");

        let installed = state
            .extension::<Mailer>()
            .expect("install_mailer should store a Mailer extension");
        assert!(
            installed.has_durable_delivery_queue(),
            "registered queue handle should be attached to the installed mailer"
        );
    }

    #[test]
    fn install_mailer_with_factory_runs_factory_and_attaches_queue() {
        let state = crate::AppState::for_test().with_profile("prod");
        let config = sample_smtp_config();
        let factory_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = Arc::clone(&factory_called);

        let factory = move |_state: &crate::AppState| {
            captured.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, crate::AutumnError>(Arc::new(NoopQueue) as Arc<dyn MailDeliveryQueue>)
        };

        install_mailer_with_factory(&state, &config, Some(factory), true)
            .expect("factory should produce a queue and satisfy the prod guard");

        assert!(
            factory_called.load(std::sync::atomic::Ordering::SeqCst),
            "factory must run when enforce_durable_guard is true"
        );
        let installed = state
            .extension::<Mailer>()
            .expect("install_mailer should store a Mailer extension");
        assert!(
            installed.has_durable_delivery_queue(),
            "factory's queue should be wired into the installed Mailer"
        );
    }

    #[test]
    fn install_mailer_with_factory_skips_factory_when_not_enforced() {
        let state = crate::AppState::for_test().with_profile("prod");
        let config = sample_smtp_config();
        let factory_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = Arc::clone(&factory_called);

        let factory = move |_state: &crate::AppState| {
            captured.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, crate::AutumnError>(Arc::new(NoopQueue) as Arc<dyn MailDeliveryQueue>)
        };

        install_mailer_with_factory(&state, &config, Some(factory), false)
            .expect("static-build path should skip factory and install cleanly");

        assert!(
            !factory_called.load(std::sync::atomic::Ordering::SeqCst),
            "factory must be skipped when enforce_durable_guard is false"
        );
    }

    #[test]
    fn install_mailer_with_factory_propagates_factory_errors() {
        let state = crate::AppState::for_test().with_profile("prod");
        let config = sample_smtp_config();

        let factory = |_state: &crate::AppState| {
            Err::<Arc<dyn MailDeliveryQueue>, _>(crate::AutumnError::service_unavailable_msg(
                "queue offline",
            ))
        };

        let error = install_mailer_with_factory(&state, &config, Some(factory), true)
            .expect_err("factory error should propagate");
        assert!(error.to_string().contains("queue offline"));
    }

    #[test]
    fn install_mailer_with_factory_skips_factory_when_transport_disabled() {
        // Even when enforce_durable_guard=true (normal server path), a
        // profile with transport=disabled must not run the factory: the
        // factory might open Redis/Harvest/DB connections, but all mail in
        // this profile is supposed to be a no-op.
        let state = crate::AppState::for_test().with_profile("dev");
        let config = MailConfig::default(); // transport = Disabled
        let factory_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = Arc::clone(&factory_called);

        let factory = move |_state: &crate::AppState| {
            captured.store(true, std::sync::atomic::Ordering::SeqCst);
            Err::<Arc<dyn MailDeliveryQueue>, _>(crate::AutumnError::service_unavailable_msg(
                "queue must not be reached",
            ))
        };

        install_mailer_with_factory(&state, &config, Some(factory), true)
            .expect("disabled transport should bypass the factory entirely");
        assert!(
            !factory_called.load(std::sync::atomic::Ordering::SeqCst),
            "factory must not run when transport = disabled"
        );
    }

    #[test]
    fn install_mailer_with_factory_works_without_factory() {
        type FactoryFn = fn(&crate::AppState) -> AutumnResult<Arc<dyn MailDeliveryQueue>>;
        let state = crate::AppState::for_test().with_profile("dev");
        let config = sample_smtp_config();
        let no_factory: Option<FactoryFn> = None;

        install_mailer_with_factory(&state, &config, no_factory, true)
            .expect("absent factory should be fine in non-prod");
    }

    #[test]
    fn install_mailer_does_not_run_factory_when_not_enforced_and_no_handle() {
        // Mirrors run_build_mode: queue factory is intentionally skipped, so
        // no MailDeliveryQueueHandle is on AppState. install_mailer must
        // tolerate this and not try to enforce or warn about a missing queue.
        let state = crate::AppState::for_test().with_profile("prod");
        let config = sample_smtp_config();

        install_mailer(&state, &config, false)
            .expect("static-build mode should install cleanly with no queue handle");

        let installed = state
            .extension::<Mailer>()
            .expect("install_mailer should store a Mailer extension");
        assert!(
            !installed.has_durable_delivery_queue(),
            "no queue is expected when run_build_mode skips the factory"
        );
    }

    #[test]
    fn install_mailer_skips_production_guard_when_not_enforced() {
        // Static-site builds (run_build_mode) call install_mailer with
        // enforce_durable_guard=false because they don't run the request
        // loop and don't actually defer mail. Even with a prod profile,
        // an active SMTP transport, no queue, and no ack flag, install
        // must succeed in this mode.
        let state = crate::AppState::for_test().with_profile("prod");
        let config = sample_smtp_config();

        install_mailer(&state, &config, false)
            .expect("static-build mode should not enforce the deliver_later guard");
    }

    #[test]
    fn spawn_mail_delivery_inherits_parent_span() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};

        struct CapturingQueue(Arc<Mutex<Option<tracing::span::Id>>>);
        impl MailDeliveryQueue for CapturingQueue {
            fn enqueue<'a>(
                &'a self,
                _mail: Mail,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                let captured = self.0.clone();
                Box::pin(async move {
                    *captured.lock().unwrap() = tracing::Span::current().id();
                    Ok(())
                })
            }
        }

        let captured_span_id: Arc<Mutex<Option<tracing::span::Id>>> = Arc::new(Mutex::new(None));

        let mailer = Mailer::builder()
            .delivery_queue(CapturingQueue(captured_span_id.clone()))
            .build()
            .expect("mailer with queue should build");
        let mail = sample_mail();

        // The subscriber must remain active for the entire duration — spanning
        // both the enqueue call and the spawned task's execution — so that
        // `tracing::Span::current()` inside the task sees the same span tree
        // that was active when `try_deliver_later` was called.
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime");

            let outer = tracing::info_span!("deliver_later_outer");
            let outer_id = outer.id();

            rt.block_on(async {
                {
                    let _guard = outer.enter();
                    mailer
                        .try_deliver_later(mail)
                        .expect("deliver_later must not fail");
                }

                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });

            let in_task = captured_span_id.lock().unwrap().clone();
            assert_eq!(
                in_task, outer_id,
                "delivery task must run inside the span that called deliver_later"
            );
        });
    }

    #[tokio::test]
    async fn spawn_mail_delivery_logs_error_when_queue_fails() {
        use std::future::Future;
        use std::pin::Pin;

        struct AlwaysFailQueue;
        impl MailDeliveryQueue for AlwaysFailQueue {
            fn enqueue<'a>(
                &'a self,
                _mail: Mail,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async { Err(MailError::RuntimeUnavailable("always fails".to_owned())) })
            }
        }

        let mailer = Mailer::builder()
            .delivery_queue(AlwaysFailQueue)
            .build()
            .expect("build");

        mailer
            .try_deliver_later(sample_mail())
            .expect("should schedule");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn spawn_mail_delivery_logs_error_when_transport_fails() {
        use std::future::Future;
        use std::pin::Pin;

        struct AlwaysFailTransport;
        impl MailTransport for AlwaysFailTransport {
            fn send<'a>(
                &'a self,
                _mail: Mail,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async {
                    Err(MailError::RuntimeUnavailable(
                        "transport offline".to_owned(),
                    ))
                })
            }
        }

        let mailer = Mailer::with_transport(AlwaysFailTransport);

        mailer
            .try_deliver_later(sample_mail())
            .expect("should schedule");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[test]
    fn install_mailer_does_not_attach_queue_when_transport_disabled() {
        // When mail.transport = "disabled" the operator has explicitly turned
        // mail off for this profile (tests, review apps, etc.). A globally
        // registered queue must not turn deliver_later back into a durable
        // persist; it should remain a no-op.
        let state = crate::AppState::for_test().with_profile("dev");
        state.insert_extension(MailDeliveryQueueHandle::new(NoopQueue));
        let config = MailConfig::default(); // transport = Disabled

        install_mailer(&state, &config, true).expect("disabled transport should install cleanly");

        let installed = state
            .extension::<Mailer>()
            .expect("install_mailer should store a Mailer extension");
        assert!(
            !installed.has_durable_delivery_queue(),
            "disabled transport must suppress queue attachment so deliver_later is a no-op"
        );
    }

    #[tokio::test]
    async fn intercepted_mail_transport_short_circuit_prevents_sync_execution() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicU32, Ordering};

        static TRANSPORT_CALLS: AtomicU32 = AtomicU32::new(0);

        struct CountingTransport;
        impl MailTransport for CountingTransport {
            fn send<'a>(
                &'a self,
                _mail: Mail,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                TRANSPORT_CALLS.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(()) })
            }

            fn is_disabled(&self) -> bool {
                false
            }
        }

        struct ShortCircuitMailInterceptor;
        impl crate::interceptor::MailInterceptor for ShortCircuitMailInterceptor {
            fn intercept<'a>(
                &'a self,
                _mail: &'a Mail,
                _next: Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>,
            ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>> {
                Box::pin(async move {
                    Err(MailError::RuntimeUnavailable(
                        "blocked by interceptor".to_owned(),
                    ))
                })
            }
        }

        let transport = Arc::new(CountingTransport);
        let interceptor = Arc::new(ShortCircuitMailInterceptor);
        let intercepted = InterceptedMailTransport {
            inner: transport,
            interceptor,
        };

        let mail = Mail::builder()
            .to("test@example.com")
            .subject("test")
            .text("body")
            .build()
            .unwrap();

        TRANSPORT_CALLS.store(0, Ordering::SeqCst);

        let res = intercepted.send(mail).await;
        assert!(res.is_err());
        assert_eq!(TRANSPORT_CALLS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_smtp_transport_circuit_breaker() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();
        let policy = crate::circuit_breaker::CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: std::time::Duration::from_secs(10),
            minimum_sample_count: 3,
            open_duration: std::time::Duration::from_secs(60),
            half_open_trial_count: 2,
        };
        let breaker =
            crate::circuit_breaker::global_registry().get_or_create("smtp_mailer", policy);

        // Ensure it is closed initially
        assert_eq!(
            breaker.state(),
            crate::circuit_breaker::CircuitState::Closed
        );

        // Build an SMTP transport pointing to a bogus localhost port so it fails
        let config = SmtpConfig {
            host: Some("127.0.0.1".to_string()),
            port: Some(9999), // Bogus port
            tls: TlsMode::Disabled,
            username: None,
            password_env: None,
        };
        let transport = SmtpTransport::new(config, None).unwrap();

        let mail = Mail::builder()
            .from("sender@example.com")
            .to("test@example.com")
            .subject("test")
            .text("body")
            .build()
            .unwrap();

        // Send 3 times — all should fail and trip the breaker
        for _ in 0..3 {
            let res = transport.send(mail.clone()).await;
            assert!(res.is_err());
        }

        assert_eq!(breaker.state(), crate::circuit_breaker::CircuitState::Open);

        // 4th send should fail fast with a circuit breaker error
        let res = transport.send(mail.clone()).await;
        assert!(res.is_err());
        let err_str = res.err().unwrap().to_string();
        assert!(
            err_str.contains("circuit breaker")
                || err_str.contains("open")
                || err_str.contains("Open")
                || err_str.contains("runtime unavailable")
        );

        crate::circuit_breaker::global_registry().clear();
    }

    #[test]
    fn validate_log_transport_in_prod_fails() {
        let cfg = MailConfig {
            transport: Transport::Log,
            ..MailConfig::default()
        };
        assert!(cfg.validate(Some("prod")).is_err());
        assert!(cfg.validate(Some("production")).is_err());
        // allow flag lifts the restriction.
        let allowed = MailConfig {
            transport: Transport::Log,
            allow_log_in_production: true,
            ..MailConfig::default()
        };
        assert!(allowed.validate(Some("prod")).is_ok());
    }

    #[test]
    fn validate_preview_outside_dev_fails() {
        let cfg = MailConfig {
            preview: true,
            ..MailConfig::default()
        };
        assert!(cfg.validate(Some("prod")).is_err());
        assert!(cfg.validate(Some("dev")).is_ok());
        assert!(cfg.validate(Some("development")).is_ok());
    }

    #[test]
    fn is_valid_https_base_url_edge_cases() {
        assert!(is_valid_https_base_url("https://app.example.com"));
        assert!(is_valid_https_base_url("https://app.example.com/base"));
        assert!(!is_valid_https_base_url("http://app.example.com"));
        assert!(!is_valid_https_base_url("https://"));
        assert!(!is_valid_https_base_url("https:///path"));
        assert!(!is_valid_https_base_url("https://app.example.com?q=1"));
        assert!(!is_valid_https_base_url("https://app.example.com#frag"));
        assert!(!is_valid_https_base_url("https://host name.com"));
        // Malformed authorities that a naive `/`-split would wrongly accept.
        assert!(!is_valid_https_base_url("https://app.example.com:abc"));
        assert!(!is_valid_https_base_url("https://@/base"));
        assert!(!is_valid_https_base_url("https://user@app.example.com"));
        // A valid explicit port is fine.
        assert!(is_valid_https_base_url("https://app.example.com:8443"));
        // Characters unsafe inside an RFC 2369 angle-bracket URI are rejected,
        // even though `Url::parse` would percent-encode them.
        assert!(!is_valid_https_base_url("https://example.com/<x>"));
        assert!(!is_valid_https_base_url("https://example.com/a b"));
        assert!(!is_valid_https_base_url("https://example.com/a\r\nb"));
        // Missing/short authority that `Url::parse` would normalize to a valid
        // host — rejected so prod can't advertise an unusable one-click URL.
        assert!(!is_valid_https_base_url("https:/app.example.com"));
        assert!(!is_valid_https_base_url("https:app.example.com"));
    }

    #[test]
    fn is_valid_mailto_address_edge_cases() {
        assert!(is_valid_mailto_address("unsub@example.com"));
        assert!(is_valid_mailto_address("mailto:unsub@example.com"));
        assert!(is_valid_mailto_address(
            "mailto:unsub@example.com?subject=hi"
        ));
        assert!(!is_valid_mailto_address("not-an-email"));
        assert!(!is_valid_mailto_address("missing@dot"));
        assert!(!is_valid_mailto_address("space @example.com"));
        assert!(!is_valid_mailto_address(""));
        assert!(!is_valid_mailto_address("@example.com")); // empty local
        assert!(!is_valid_mailto_address("local@")); // empty domain
        // Other URI schemes must be rejected, not coerced into <mailto:…>.
        assert!(!is_valid_mailto_address("https://unsub@example.com"));
        assert!(!is_valid_mailto_address("mailto:https://unsub@example.com"));
        assert!(!is_valid_mailto_address("unsub@https://example.com"));
        // CRLF / control characters (header-injection attempt) are rejected even
        // when hidden behind a `?query` the address check would otherwise drop.
        assert!(!is_valid_mailto_address(
            "mailto:unsub@example.com?subject=x\r\nBcc: victim@example.com"
        ));
        assert!(!is_valid_mailto_address("unsub@example.com\nBcc: v@x.com"));
        // RFC 2369 delimiters (`<`/`>`/`,`) would close the angle-bracket entry
        // and inject an extra List-Unsubscribe target.
        assert!(!is_valid_mailto_address(
            "unsub@example.com>,<bogus@example.com"
        ));
        assert!(!is_valid_mailto_address("a@x.com,b@x.com"));
    }

    #[test]
    fn unsubscribe_header_mailto_drops_configured_query_no_injection() {
        // Even if a malformed value slipped past validation (e.g. set outside
        // prod), the rendered header must carry only the bare mailbox plus the
        // canonical subject — never an injected CRLF/Bcc.
        let runtime = UnsubscribeRuntime {
            base_url: None,
            mailto: Some("mailto:u@example.com?subject=x\r\nBcc: v@x.com".to_owned()),
            signing_keys: Arc::new(test_keys()),
            ttl_days: 30,
            suppression: None,
        };
        let header = runtime
            .list_unsubscribe_header("a@x.com", "list")
            .expect("mailto header");
        assert_eq!(header, "<mailto:u@example.com?subject=unsubscribe>");
        assert!(!header.contains('\r') && !header.contains('\n'));
        assert!(!header.contains("Bcc"));
    }

    #[test]
    fn unsubscribe_runtime_header_both_base_url_and_mailto() {
        let runtime = UnsubscribeRuntime {
            base_url: Some("https://app.example.com".to_owned()),
            mailto: Some("u@example.com".to_owned()),
            signing_keys: Arc::new(test_keys()),
            ttl_days: 30,
            suppression: None,
        };
        let header = runtime
            .list_unsubscribe_header("a@x.com", "list")
            .expect("header with both");
        assert!(header.contains("https://app.example.com/_autumn/unsubscribe?token="));
        assert!(header.contains("mailto:u@example.com?subject=unsubscribe"));
        assert!(runtime.supports_one_click());
    }

    #[test]
    fn unsubscribe_runtime_header_neither_configured_returns_none() {
        let runtime = UnsubscribeRuntime {
            base_url: None,
            mailto: None,
            signing_keys: Arc::new(test_keys()),
            ttl_days: 30,
            suppression: None,
        };
        assert!(runtime.list_unsubscribe_header("a@x.com", "list").is_none());
        assert!(!runtime.supports_one_click());
    }
}

//! Flash messages for Autumn applications.
//!
//! Provides a [`Flash`] extractor that allows storing and retrieving
//! temporary messages across HTTP redirects, backed by the user's
//! [`crate::session::Session`].
//!
//! # Examples
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use axum::response::{IntoResponse, Redirect};
//!
//! #[post("/items")]
//! async fn create_item(flash: Flash) -> impl IntoResponse {
//!     // ... create item ...
//!     flash.success("Item created successfully!").await;
//!     Redirect::to("/items")
//! }
//!
//! #[get("/items")]
//! async fn list_items(flash: Flash) -> Markup {
//!     let messages = flash.consume().await;
//!     html! {
//!         // Accessible banners with correct `role`/`aria-live` per severity —
//!         // no hand-rolled ARIA. See [`flash_messages`].
//!         (flash_messages(&messages))
//!     }
//! }
//! ```

use axum::extract::FromRequestParts;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::session::Session;

const FLASH_SESSION_KEY: &str = "__autumn_flash";

/// The severity level of a flash message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FlashLevel {
    /// Success messages (e.g., "Item created").
    Success,
    /// Informational messages (e.g., "Welcome back").
    Info,
    /// Warning messages (e.g., "Your trial ends soon").
    Warning,
    /// Error messages (e.g., "Invalid password").
    Error,
}

impl FlashLevel {
    /// Returns the level as a lowercase string (useful for CSS classes).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// A single flash message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashMessage {
    /// The severity level.
    pub level: FlashLevel,
    /// The text message.
    pub message: String,
}

/// Extractor for adding and consuming flash messages.
///
/// Backed by the session. Messages are stored as a JSON array
/// under the key `__autumn_flash`.
#[derive(Debug, Clone)]
pub struct Flash {
    session: Session,
}

impl Flash {
    /// Create a new `Flash` instance wrapping the given `Session`.
    #[must_use]
    pub const fn new(session: Session) -> Self {
        Self { session }
    }

    /// Add a new message to the flash queue.
    pub async fn push(&self, level: FlashLevel, message: impl Into<String>) {
        let mut messages = self.peek().await;
        messages.push(FlashMessage {
            level,
            message: message.into(),
        });

        if let Ok(json) = serde_json::to_string(&messages) {
            self.session.insert(FLASH_SESSION_KEY, json).await;
        }
    }

    /// Add a success message.
    pub async fn success(&self, message: impl Into<String>) {
        self.push(FlashLevel::Success, message).await;
    }

    /// Add an informational message.
    pub async fn info(&self, message: impl Into<String>) {
        self.push(FlashLevel::Info, message).await;
    }

    /// Add a warning message.
    pub async fn warning(&self, message: impl Into<String>) {
        self.push(FlashLevel::Warning, message).await;
    }

    /// Add an error message.
    pub async fn error(&self, message: impl Into<String>) {
        self.push(FlashLevel::Error, message).await;
    }

    /// Read all pending flash messages without removing them.
    pub async fn peek(&self) -> Vec<FlashMessage> {
        self.session
            .get(FLASH_SESSION_KEY)
            .await
            .map_or_else(Vec::new, |json| {
                serde_json::from_str(&json).unwrap_or_default()
            })
    }

    /// Read all pending flash messages and remove them from the session.
    pub async fn consume(&self) -> Vec<FlashMessage> {
        let messages = self.peek().await;
        if !messages.is_empty() {
            self.session.remove(FLASH_SESSION_KEY).await;
        }
        messages
    }

    /// Injects pending flash messages into an HTMX response as `HX-Trigger` events.
    ///
    /// Consumes the messages from the session and sets the `HX-Trigger` header
    /// with a JSON payload representing the messages. This allows the frontend
    /// to display flash messages without a full page reload.
    #[cfg(feature = "htmx")]
    pub async fn inject_hx_trigger<T: axum::response::IntoResponse>(
        &self,
        response: T,
    ) -> axum::response::Response {
        let messages = self.consume().await;
        let mut res = response.into_response();
        if !messages.is_empty() {
            let payload = serde_json::json!({
                "flash": messages
            });
            if let Ok(v) = http::header::HeaderValue::from_str(&payload.to_string()) {
                res.headers_mut()
                    .insert(http::header::HeaderName::from_static("hx-trigger"), v);
            }
        }
        res
    }
}

#[cfg(feature = "maud")]
impl Flash {
    /// Consume all pending flash messages and render them as HTML.
    ///
    /// This is the one-line helper for a base layout: drop `(flash.render().await)`
    /// into your template and every pending notice is rendered and cleared in a
    /// single call — no manual `consume()` + loop required.
    ///
    /// The output is wrapped in a stable `<div id="flash">` container that is
    /// *always* emitted, even when there are no messages, so it can act as the
    /// target for htmx out-of-band swaps on later requests. Messages carry
    /// `flash flash-<level>` classes; link [`FLASH_CSS_PATH`] from your layout
    /// for the default styling.
    ///
    /// Requires the `maud` feature (enabled by default).
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    ///
    /// #[get("/items")]
    /// async fn list_items(flash: Flash) -> Markup {
    ///     html! {
    ///         (flash.render().await)
    ///         h1 { "Items" }
    ///     }
    /// }
    /// ```
    pub async fn render(&self) -> maud::Markup {
        self.render_inner(false).await
    }

    /// Like [`render`](Self::render), but marks the container for an htmx
    /// out-of-band swap (`hx-swap-oob="true"`).
    ///
    /// Include `(flash.render_oob().await)` anywhere in an htmx partial response
    /// and the flash container in the already-rendered page is replaced in place,
    /// so notices appear on htmx-driven swaps — not just full-page loads. For the
    /// header-based alternative see [`inject_hx_trigger`](Self::inject_hx_trigger).
    pub async fn render_oob(&self) -> maud::Markup {
        self.render_inner(true).await
    }

    async fn render_inner(&self, oob: bool) -> maud::Markup {
        let messages = self.consume().await;
        maud::html! {
            div id="flash" class="flash-messages" role="status" aria-live="polite"
                hx-swap-oob=[oob.then_some("true")] {
                (flash_message_divs(&messages))
            }
        }
    }
}

/// Render a list of flash messages as `<div class="flash flash-<level>">` nodes.
///
/// Shared by [`Flash::render`] and other surfaces (e.g. the admin panel) so the
/// message markup and `.flash-<level>` class convention live in one place.
/// Styling comes from [`FLASH_CSS`] (served at [`FLASH_CSS_PATH`]); this emits
/// classes only — no inline `style` attributes — so it is compatible with a
/// strict `style-src 'self'` Content-Security-Policy, including nonce mode.
#[cfg(feature = "maud")]
#[must_use]
pub fn flash_message_divs(messages: &[FlashMessage]) -> maud::Markup {
    maud::html! {
        @for msg in messages {
            div class={ "flash flash-" (msg.level.as_str()) } role="alert" {
                (msg.message)
            }
        }
    }
}

impl FlashLevel {
    /// Live-region semantics for this severity, as `(role, aria-live)`.
    ///
    /// `Error`/`Warning` are assertive (`role="alert"`, `aria-live="assertive"`)
    /// so they interrupt a screen reader immediately; `Success`/`Info` are polite
    /// (`role="status"`, `aria-live="polite"`) so they announce without cutting
    /// off the current utterance.
    #[must_use]
    pub const fn live_region(&self) -> (&'static str, &'static str) {
        match self {
            Self::Error | Self::Warning => ("alert", "assertive"),
            Self::Success | Self::Info => ("status", "polite"),
        }
    }
}

/// Rendering options for [`flash_messages_with`].
///
/// Build with [`FlashMessagesConfig::new`] and chain the setters. The default
/// (used by [`flash_messages`]) renders plain banners with no dismiss control.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, Default)]
pub struct FlashMessagesConfig {
    dismissible: bool,
}

#[cfg(feature = "maud")]
impl FlashMessagesConfig {
    /// A default config: no dismiss control.
    #[must_use]
    pub const fn new() -> Self {
        Self { dismissible: false }
    }

    /// Render a no-JavaScript dismiss control on each banner.
    ///
    /// The control is a `<label>`-wrapped hidden checkbox; toggling it hides the
    /// banner via the stylesheet's `:has()` rule, so it degrades to an inert
    /// (already-visible) banner when CSS `:has()` is unavailable and never
    /// depends on JavaScript.
    #[must_use]
    pub const fn dismissible(mut self, yes: bool) -> Self {
        self.dismissible = yes;
        self
    }
}

/// Render pending flash messages as accessible, styled banners.
///
/// Each message becomes its own live region whose `role`/`aria-live` pair is
/// chosen by severity ([`FlashLevel::live_region`]): `Error`/`Warning` announce
/// assertively, `Success`/`Info` politely. Messages carry the semantic
/// `autumn-flash` / `autumn-flash--<level>` classes backed by [`FLASH_CSS`], so
/// no per-app CSS or hand-written ARIA is required. Message text is HTML-escaped
/// by Maud.
///
/// An **empty** slice renders nothing at all — no container and no empty live
/// region — so a page with no flash is byte-for-byte unchanged.
///
/// For a dismiss control use [`flash_messages_with`] with
/// [`FlashMessagesConfig::dismissible`].
///
/// # Example
///
/// ```rust
/// use autumn_web::flash::{flash_messages, FlashLevel, FlashMessage};
///
/// let messages = vec![
///     FlashMessage { level: FlashLevel::Success, message: "Saved!".into() },
///     FlashMessage { level: FlashLevel::Error, message: "Invalid email".into() },
/// ];
/// let html = flash_messages(&messages).into_string();
/// assert!(html.contains(r#"role="status""#));       // Success → polite
/// assert!(html.contains(r#"aria-live="polite""#));
/// assert!(html.contains(r#"role="alert""#));         // Error → assertive
/// assert!(html.contains(r#"aria-live="assertive""#));
/// assert!(html.contains("autumn-flash--success"));
///
/// // Nothing renders for an empty slice.
/// assert_eq!(flash_messages(&[]).into_string(), "");
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn flash_messages(messages: &[FlashMessage]) -> maud::Markup {
    flash_messages_with(messages, &FlashMessagesConfig::new())
}

/// Render pending flash messages as accessible banners, with rendering options.
///
/// See [`flash_messages`] for the semantics; this variant additionally honors
/// [`FlashMessagesConfig`] (e.g. a no-JavaScript dismiss control). An empty
/// slice still renders nothing.
#[cfg(feature = "maud")]
#[must_use]
pub fn flash_messages_with(
    messages: &[FlashMessage],
    config: &FlashMessagesConfig,
) -> maud::Markup {
    // An empty slice renders nothing — no container, no empty live region.
    if messages.is_empty() {
        return maud::html! {};
    }
    maud::html! {
        div class="autumn-flash-group" {
            @for msg in messages {
                @let (role, live) = msg.level.live_region();
                div class={ "autumn-flash autumn-flash--" (msg.level.as_str()) }
                    role=(role) aria-live=(live) {
                    span class="autumn-flash__body" { (msg.message) }
                    @if config.dismissible {
                        // The checkbox stays in tab order (visually hidden but
                        // focusable via `.autumn-flash__dismiss-toggle`, never
                        // `hidden`/`display:none`) so keyboard and screen-reader
                        // users can dismiss; the accessible name is on the
                        // control itself.
                        label class="autumn-flash__dismiss" {
                            input type="checkbox" class="autumn-flash__dismiss-toggle"
                                aria-label="Dismiss this message";
                            span aria-hidden="true" { "×" }
                        }
                    }
                }
            }
        }
    }
}

/// URL of the framework-served flash stylesheet.
///
/// The default Autumn server mounts this asset automatically. Link it from your
/// base layout — `link rel="stylesheet" href=(autumn_web::flash::FLASH_CSS_PATH);`
/// — so the `.flash` / `.flash-<level>` classes emitted by [`Flash::render`] are
/// styled out of the box.
pub const FLASH_CSS_PATH: &str = "/static/css/autumn-flash.css";

/// Default flash-message stylesheet served at [`FLASH_CSS_PATH`].
///
/// Each rule pairs a `--flash-*` custom property with a hard-coded fallback, so
/// notices are visible with zero configuration yet apps can re-theme them by
/// defining the variables on `:root`.
pub const FLASH_CSS: &str = "\
.flash-messages:empty{display:none}\
.flash{padding:.75rem 1rem;border-radius:.375rem;margin-bottom:.5rem;border:1px solid}\
.flash-success{background:var(--flash-success-bg,#ecfdf5);color:var(--flash-success-fg,#065f46);border-color:var(--flash-success-border,#6ee7b7)}\
.flash-info{background:var(--flash-info-bg,#eff6ff);color:var(--flash-info-fg,#1e3a8a);border-color:var(--flash-info-border,#93c5fd)}\
.flash-warning{background:var(--flash-warning-bg,#fffbeb);color:var(--flash-warning-fg,#92400e);border-color:var(--flash-warning-border,#fcd34d)}\
.flash-error{background:var(--flash-error-bg,#fef2f2);color:var(--flash-error-fg,#991b1b);border-color:var(--flash-error-border,#fca5a5)}\
.autumn-flash-group{display:flex;flex-direction:column;gap:.5rem}\
.autumn-flash{display:flex;align-items:flex-start;justify-content:space-between;gap:.75rem;padding:.75rem 1rem;border-radius:.375rem;border:1px solid}\
.autumn-flash__body{flex:1 1 auto}\
.autumn-flash--success{background:var(--flash-success-bg,#ecfdf5);color:var(--flash-success-fg,#065f46);border-color:var(--flash-success-border,#6ee7b7)}\
.autumn-flash--info{background:var(--flash-info-bg,#eff6ff);color:var(--flash-info-fg,#1e3a8a);border-color:var(--flash-info-border,#93c5fd)}\
.autumn-flash--warning{background:var(--flash-warning-bg,#fffbeb);color:var(--flash-warning-fg,#92400e);border-color:var(--flash-warning-border,#fcd34d)}\
.autumn-flash--error{background:var(--flash-error-bg,#fef2f2);color:var(--flash-error-fg,#991b1b);border-color:var(--flash-error-border,#fca5a5)}\
.autumn-flash__dismiss{flex:0 0 auto;cursor:pointer;line-height:1;font-size:1.25rem;color:inherit;background:none;border:0;padding:0 .25rem}\
.autumn-flash__dismiss-toggle{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap;border:0}\
.autumn-flash__dismiss:has(.autumn-flash__dismiss-toggle:focus-visible){outline:2px solid var(--primary,#7c3aed);outline-offset:2px}\
.autumn-flash:has(.autumn-flash__dismiss-toggle:checked){display:none}\
";

impl<S> FromRequestParts<S> for Flash
where
    S: Send + Sync,
{
    type Rejection = <Session as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state).await?;
        Ok(Self::new(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn flash_push_and_consume() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        let flash = Flash::new(session.clone());

        flash.success("Saved!").await;
        flash.error("Failed!").await;

        let messages = flash.peek().await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].level, FlashLevel::Success);
        assert_eq!(messages[0].message, "Saved!");
        assert_eq!(messages[1].level, FlashLevel::Error);
        assert_eq!(messages[1].message, "Failed!");

        // Still there after peek
        assert_eq!(flash.peek().await.len(), 2);

        // Consume removes them
        let consumed = flash.consume().await;
        assert_eq!(consumed.len(), 2);
        assert_eq!(flash.peek().await.len(), 0);
    }

    #[tokio::test]
    async fn flash_level_as_str() {
        assert_eq!(FlashLevel::Success.as_str(), "success");
        assert_eq!(FlashLevel::Info.as_str(), "info");
        assert_eq!(FlashLevel::Warning.as_str(), "warning");
        assert_eq!(FlashLevel::Error.as_str(), "error");
    }

    #[tokio::test]
    async fn should_not_remove_key_when_consuming_empty_flash() -> Result<(), String> {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        // Insert a dummy key to verify the session remains untouched and "dirty" flag logic
        session.insert("dummy", "val").await;

        let flash = Flash::new(session.clone());
        let messages = flash.consume().await;

        // No messages were present
        assert_eq!(messages.len(), 0);

        // "dummy" key is still there
        assert_eq!(
            session.get("dummy").await.ok_or("missing key dummy")?,
            "val"
        );
        // Flash key shouldn't be added or touched
        assert!(!session.contains_key(FLASH_SESSION_KEY).await);
        Ok(())
    }

    #[tokio::test]
    async fn should_handle_invalid_json_gracefully() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        // Insert broken JSON manually
        session
            .insert(FLASH_SESSION_KEY, "{ invalid_json: true")
            .await;

        let flash = Flash::new(session);
        let messages = flash.peek().await;

        // It should gracefully fall back to an empty vector rather than panicking
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn should_support_all_convenience_methods() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        let flash = Flash::new(session);

        flash.success("Success msg").await;
        flash.info("Info msg").await;
        flash.warning("Warning msg").await;
        flash.error("Error msg").await;

        let messages = flash.peek().await;
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].level, FlashLevel::Success);
        assert_eq!(messages[0].message, "Success msg");
        assert_eq!(messages[1].level, FlashLevel::Info);
        assert_eq!(messages[1].message, "Info msg");
        assert_eq!(messages[2].level, FlashLevel::Warning);
        assert_eq!(messages[2].message, "Warning msg");
        assert_eq!(messages[3].level, FlashLevel::Error);
        assert_eq!(messages[3].message, "Error msg");
    }

    #[tokio::test]
    #[cfg(feature = "maud")]
    async fn render_emits_messages_and_clears_them() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        let flash = Flash::new(session.clone());

        flash.success("Saved!").await;
        flash.error("Oops").await;

        let markup = flash.render().await.into_string();
        // Stable container that doubles as the htmx OOB target.
        assert!(
            markup.contains("id=\"flash\""),
            "missing container: {markup}"
        );
        assert!(markup.contains("aria-live=\"polite\""));
        // Per-message level classes and text.
        assert!(markup.contains("flash flash-success"));
        assert!(markup.contains("Saved!"));
        assert!(markup.contains("flash flash-error"));
        assert!(markup.contains("Oops"));
        // Styling is class-based (served stylesheet), not inline — keeps it
        // compatible with a strict `style-src 'self'` CSP / nonce mode.
        assert!(
            !markup.contains("style="),
            "must not emit inline styles: {markup}"
        );
        // A plain full-page render is not an out-of-band swap.
        assert!(!markup.contains("hx-swap-oob"));

        // render() consumes — the next render is empty.
        assert_eq!(flash.peek().await.len(), 0);
    }

    #[tokio::test]
    #[cfg(feature = "maud")]
    async fn render_emits_container_even_when_empty() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        let flash = Flash::new(session);

        // No messages pushed — the container must still render so htmx OOB
        // swaps have a stable target on subsequent requests.
        let markup = flash.render().await.into_string();
        assert!(
            markup.contains("id=\"flash\""),
            "missing container: {markup}"
        );
        assert!(!markup.contains("flash flash-"));
    }

    #[tokio::test]
    #[cfg(feature = "maud")]
    async fn render_oob_marks_container_for_out_of_band_swap() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        let flash = Flash::new(session.clone());

        flash.info("Updated").await;

        let markup = flash.render_oob().await.into_string();
        assert!(markup.contains("id=\"flash\""));
        assert!(
            markup.contains("hx-swap-oob=\"true\""),
            "missing OOB attr: {markup}"
        );
        assert!(markup.contains("flash flash-info"));
        assert!(markup.contains("Updated"));

        // Like render(), render_oob() consumes.
        assert_eq!(flash.peek().await.len(), 0);
    }

    #[test]
    #[cfg(feature = "maud")]
    fn flash_messages_empty_slice_renders_nothing() {
        // No container, no empty live region.
        assert_eq!(flash_messages(&[]).into_string(), "");
        assert_eq!(
            flash_messages_with(&[], &FlashMessagesConfig::new().dismissible(true)).into_string(),
            ""
        );
    }

    #[test]
    #[cfg(feature = "maud")]
    fn flash_messages_maps_severity_to_live_region() {
        // Success/Info are polite; Error/Warning are assertive.
        for (level, role, live) in [
            (FlashLevel::Success, "status", "polite"),
            (FlashLevel::Info, "status", "polite"),
            (FlashLevel::Warning, "alert", "assertive"),
            (FlashLevel::Error, "alert", "assertive"),
        ] {
            let msg = [FlashMessage {
                level,
                message: "hi".into(),
            }];
            let html = flash_messages(&msg).into_string();
            assert!(
                html.contains(&format!(r#"role="{role}""#)),
                "level {level:?} should carry role={role}: {html}"
            );
            assert!(
                html.contains(&format!(r#"aria-live="{live}""#)),
                "level {level:?} should carry aria-live={live}: {html}"
            );
            assert!(
                html.contains(&format!("autumn-flash--{}", level.as_str())),
                "level {level:?} should carry semantic class: {html}"
            );
        }
    }

    #[test]
    #[cfg(feature = "maud")]
    fn flash_messages_escapes_message_text() {
        let msg = [FlashMessage {
            level: FlashLevel::Error,
            message: "<script>alert(1)</script>".into(),
        }];
        let html = flash_messages(&msg).into_string();
        assert!(
            !html.contains("<script>"),
            "must escape message text: {html}"
        );
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    #[cfg(feature = "maud")]
    fn flash_messages_emits_no_inline_style() {
        let msg = [FlashMessage {
            level: FlashLevel::Success,
            message: "Saved!".into(),
        }];
        // Class-based styling only — CSP `style-src 'self'` / nonce-mode safe.
        assert!(!flash_messages(&msg).into_string().contains("style="));
    }

    #[test]
    #[cfg(feature = "maud")]
    fn flash_messages_dismiss_control_is_opt_in_and_js_free() {
        let msg = [FlashMessage {
            level: FlashLevel::Info,
            message: "Heads up".into(),
        }];
        // Default: no dismiss control.
        assert!(
            !flash_messages(&msg)
                .into_string()
                .contains("autumn-flash__dismiss")
        );
        // Opt-in: a checkbox-hack control, no JS attributes.
        let dismissible =
            flash_messages_with(&msg, &FlashMessagesConfig::new().dismissible(true)).into_string();
        assert!(
            dismissible.contains("autumn-flash__dismiss"),
            "{dismissible}"
        );
        assert!(dismissible.contains(r#"type="checkbox""#), "{dismissible}");
        assert!(!dismissible.contains("onclick"), "{dismissible}");
        assert!(!dismissible.contains("<script"), "{dismissible}");
        // WCAG 2.1.1: the toggle must NOT be `hidden` (that removes it from tab
        // order) — it stays focusable via the sr-only toggle class, with the
        // accessible name on the control itself.
        assert!(
            !dismissible.contains(r#"autumn-flash__dismiss-toggle" hidden"#),
            "toggle must not be `hidden`: {dismissible}"
        );
        assert!(
            dismissible.contains("autumn-flash__dismiss-toggle"),
            "sr-only focusable toggle class present: {dismissible}"
        );
        assert!(
            dismissible.contains(r#"aria-label="Dismiss this message""#),
            "{dismissible}"
        );
    }

    #[test]
    #[cfg(feature = "maud")]
    fn flash_css_backs_the_autumn_flash_classes() {
        for selector in [
            ".autumn-flash",
            ".autumn-flash--success",
            ".autumn-flash--info",
            ".autumn-flash--warning",
            ".autumn-flash--error",
            ".autumn-flash__dismiss",
            ".autumn-flash__dismiss-toggle",
        ] {
            assert!(FLASH_CSS.contains(selector), "FLASH_CSS missing {selector}");
        }
        // WCAG 2.1.1: sr-only-but-focusable toggle + a visible focus indicator.
        assert!(
            FLASH_CSS.contains("clip-path:inset(50%)"),
            "FLASH_CSS must ship the sr-only (focusable) toggle rule"
        );
        assert!(
            FLASH_CSS.contains(":focus-visible"),
            "FLASH_CSS must ship a focus indicator"
        );
    }

    #[tokio::test]
    #[cfg(feature = "htmx")]
    async fn should_inject_hx_trigger() {
        let session = Session::new_for_test("test_id".to_string(), HashMap::new());
        let flash = Flash::new(session.clone());

        flash.success("Item saved").await;

        let response = flash.inject_hx_trigger("OK").await;
        let header = response.headers().get("hx-trigger");
        assert!(header.is_some());

        let json_str = header.unwrap().to_str().unwrap();
        let payload: serde_json::Value = serde_json::from_str(json_str).unwrap();

        assert_eq!(payload["flash"][0]["level"], "success");
        assert_eq!(payload["flash"][0]["message"], "Item saved");
    }
}

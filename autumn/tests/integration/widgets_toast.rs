//! Tests for the `toast` / `toast_region` widgets (issue #1320).
//!
//! Run with `cargo test --test integration_tests widgets_toast`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{AlertVariant, DEFAULT_TOAST_REGION_ID, toast, toast_in, toast_region};

// ── region ──────────────────────────────────────────────────────────────────

#[test]
fn region_carries_default_id_and_class() {
    let out = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
    assert!(out.contains(r#"id="toast-region""#), "{out}");
    assert!(out.contains(r#"class="autumn-toast-region""#), "{out}");
}

#[test]
fn region_is_a_persistent_polite_live_region() {
    // The politeness lives on the persistent region (toasts are appended into
    // it), NOT on each non-error toast. `aria-atomic="false"` + no `role="status"`
    // (which would imply atomic) so only the newly-added toast is announced.
    let out = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
    assert!(out.contains(r#"aria-live="polite""#), "{out}");
    assert!(out.contains(r#"aria-atomic="false""#), "{out}");
    assert!(!out.contains(r#"role="status""#), "{out}");
}

#[test]
fn default_region_id_is_toast_region() {
    assert_eq!(DEFAULT_TOAST_REGION_ID, "toast-region");
}

#[test]
fn region_respects_custom_id() {
    let out = toast_region("top-toasts").into_string();
    assert!(out.contains(r#"id="top-toasts""#), "{out}");
}

// ── OOB append convention ─────────────────────────────────────────────────────

#[test]
fn toast_targets_default_region_with_beforeend() {
    let out = toast("Saved", AlertVariant::Success).into_string();
    assert!(
        out.contains(r#"hx-swap-oob="beforeend:#toast-region""#),
        "{out}"
    );
}

#[test]
fn toast_in_targets_custom_region_with_beforeend() {
    let out = toast_in("top-toasts", "Uploaded", AlertVariant::Info).into_string();
    assert!(
        out.contains(r#"hx-swap-oob="beforeend:#top-toasts""#),
        "{out}"
    );
}

#[test]
fn toast_in_normalizes_a_leading_hash_in_region_id() {
    // A caller passing a `#selector` (natural slip) must not produce an invalid
    // `beforeend:##…`; exactly one leading `#` is stripped.
    let out = toast_in("#region", "hi", AlertVariant::Info).into_string();
    assert!(out.contains(r#"hx-swap-oob="beforeend:#region""#), "{out}");
    assert!(!out.contains("beforeend:##"), "{out}");
}

#[test]
fn toast_region_normalizes_a_leading_hash_in_id() {
    let out = toast_region("#region").into_string();
    assert!(out.contains(r#"id="region""#), "{out}");
    assert!(!out.contains(r##"id="#region""##), "{out}");
}

#[test]
fn oob_swap_is_on_a_discardable_div_carrier_not_the_toast() {
    // For a positional (`beforeend:#…`) OOB swap the vendored htmx 2.0.4 inserts
    // the carrier's CHILDREN and discards the carrier itself (`vendor/htmx.min.js`
    // `oobSwap`/`He`: a non-outerHTML style swaps the carrier element, then
    // `Be`→`a` runs `while(n.childNodes.length>0)`, iterating its `childNodes`).
    // The carrier must therefore be a plain `<div>`, NOT a `<template>`: a
    // `<template>` keeps its children in `.content`, so `template.childNodes` is
    // empty and htmx would append nothing. This mirrors the repo's tested SSE
    // convention in `channels.rs` (`broadcast_publish_oob_custom_strategy`). The
    // styled/ARIA `.autumn-toast` element must be the carrier's CHILD — never the
    // OOB-attributed one.
    let out = toast("Saved", AlertVariant::Success).into_string();
    assert!(
        out.starts_with(r#"<div hx-swap-oob="beforeend:#toast-region">"#),
        "carrier must be a discardable <div hx-swap-oob>: {out}"
    );
    assert!(!out.contains("<template"), "no <template> carrier: {out}");
    // The styled `.autumn-toast` div's own opening tag must NOT carry hx-swap-oob.
    let toast_div = out.find(r#"<div class="autumn-toast"#).expect("toast div");
    let toast_tag_end = out[toast_div..].find('>').expect("div tag close") + toast_div;
    assert!(
        !out[toast_div..=toast_tag_end].contains("hx-swap-oob"),
        "the .autumn-toast div must not carry hx-swap-oob: {out}"
    );
}

#[test]
fn htmx_children_extraction_preserves_the_styled_toast() {
    // Model htmx's positional-OOB behavior: strip the `<div hx-swap-oob>` carrier
    // and keep only its children (what vendored htmx 2.0.4 actually inserts into
    // the region — `oobSwap` iterates the carrier's `childNodes`). The styled,
    // ARIA-annotated toast must survive that extraction — a regression to
    // `hx-swap-oob` on the toast div would leave only the bare <span>.
    let out = toast("Boom", AlertVariant::Error).into_string();
    let open_end = out.find('>').expect("carrier open tag") + 1;
    let inner = out[open_end..]
        .strip_suffix("</div>")
        .expect("carrier wrapper");
    assert!(
        inner.contains(r#"class="autumn-toast autumn-toast--error""#),
        "{inner}"
    );
    assert!(inner.contains(r#"role="alert""#), "{inner}");
    assert!(inner.contains(r#"aria-live="assertive""#), "{inner}");
    assert!(inner.contains(r#"aria-atomic="true""#), "{inner}");
    assert!(inner.contains("Boom"), "{inner}");
    // After extraction the carrier and its OOB attribute are gone.
    assert!(!inner.contains("hx-swap-oob"), "{inner}");
    assert!(!inner.contains("template"), "{inner}");
}

// ── accessibility ─────────────────────────────────────────────────────────────

#[test]
fn error_toast_is_assertive_alert() {
    // Error toasts announce assertively on their own — alerts ARE reliably
    // announced on insertion, so they don't rely on the persistent region.
    let out = toast("Boom", AlertVariant::Error).into_string();
    assert!(out.contains(r#"role="alert""#), "{out}");
    assert!(out.contains(r#"aria-live="assertive""#), "{out}");
    assert!(out.contains(r#"aria-atomic="true""#), "{out}");
}

#[test]
fn non_error_toasts_inherit_region_politeness() {
    // Non-error toasts carry NO own `role`/`aria-live`: politeness is inherited
    // from the persistent `toast_region` they're appended into. They keep
    // `aria-atomic="true"` so the message is announced as one unit.
    for variant in [
        AlertVariant::Success,
        AlertVariant::Info,
        AlertVariant::Warning,
    ] {
        let out = toast("ok", variant).into_string();
        assert!(!out.contains("aria-live="), "no own aria-live: {out}");
        assert!(!out.contains("role="), "no own role: {out}");
        assert!(out.contains(r#"aria-atomic="true""#), "{out}");
    }

    // End-to-end: the region wrapper the toast lands in is the polite live region.
    let region = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
    assert!(region.contains(r#"aria-live="polite""#), "{region}");
}

#[test]
fn toast_is_aria_atomic() {
    let out = toast("Saved", AlertVariant::Success).into_string();
    assert!(out.contains(r#"aria-atomic="true""#), "{out}");
}

// ── semantic classes / styling ────────────────────────────────────────────────

#[test]
fn toast_uses_semantic_variant_class() {
    assert!(
        toast("x", AlertVariant::Success)
            .into_string()
            .contains("autumn-toast--success"),
        "success variant class missing"
    );
    assert!(
        toast("x", AlertVariant::Error)
            .into_string()
            .contains("autumn-toast--error"),
        "error variant class missing"
    );
    let out = toast("x", AlertVariant::Info).into_string();
    assert!(
        out.contains(r#"class="autumn-toast autumn-toast--info""#),
        "{out}"
    );
    assert!(out.contains("autumn-toast__message"), "{out}");
}

#[test]
fn toast_emits_no_inline_style() {
    let out = toast("x", AlertVariant::Warning).into_string();
    assert!(!out.contains("style="), "no inline styles: {out}");
}

// ── no JavaScript ─────────────────────────────────────────────────────────────

#[test]
fn toast_emits_no_script() {
    let out = toast("Saved", AlertVariant::Success).into_string();
    assert!(!out.contains("<script"), "{out}");
    assert!(!out.contains("onclick"), "{out}");
    let region = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
    assert!(!region.contains("<script"), "{region}");
}

// ── security / XSS ────────────────────────────────────────────────────────────

#[test]
fn message_html_is_escaped() {
    let out = toast("<script>alert(1)</script>", AlertVariant::Error).into_string();
    assert!(!out.contains("<script>alert"), "{out}");
    assert!(out.contains("&lt;script&gt;"), "{out}");
}

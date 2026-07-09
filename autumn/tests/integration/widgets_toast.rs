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
fn oob_swap_is_on_a_discardable_template_carrier_not_the_toast() {
    // For a positional (`beforeend:#…`) OOB swap htmx inserts the carrier's
    // CHILDREN and discards the carrier itself. The carrier must therefore be a
    // `<template>` (unwrapped by htmx's HTTP swap pipeline), and the styled/ARIA
    // `.autumn-toast` element must be its CHILD — never the OOB-attributed one.
    let out = toast("Saved", AlertVariant::Success).into_string();
    assert!(
        out.starts_with("<template "),
        "carrier must be a <template>: {out}"
    );
    assert!(
        out.contains(r#"<template hx-swap-oob="beforeend:#toast-region">"#),
        "{out}"
    );
    // The toast div's own opening tag must NOT carry hx-swap-oob.
    let div_start = out.find("<div").expect("toast div");
    let div_tag_end = out[div_start..].find('>').expect("div tag close") + div_start;
    assert!(
        !out[div_start..=div_tag_end].contains("hx-swap-oob"),
        "the .autumn-toast div must not carry hx-swap-oob: {out}"
    );
}

#[test]
fn htmx_children_extraction_preserves_the_styled_toast() {
    // Model htmx's positional-OOB behavior: strip the `<template>` carrier and
    // keep only its children (what actually gets inserted into the region).
    // The styled, ARIA-annotated toast must survive that extraction — a
    // regression to `hx-swap-oob` on the div would leave only the bare <span>.
    let out = toast("Boom", AlertVariant::Error).into_string();
    let open_end = out.find('>').expect("template open tag") + 1;
    let inner = out[open_end..]
        .strip_suffix("</template>")
        .expect("template wrapper");
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
    let out = toast("Boom", AlertVariant::Error).into_string();
    assert!(out.contains(r#"role="alert""#), "{out}");
    assert!(out.contains(r#"aria-live="assertive""#), "{out}");
}

#[test]
fn non_error_toasts_are_polite_status() {
    for variant in [
        AlertVariant::Success,
        AlertVariant::Info,
        AlertVariant::Warning,
    ] {
        let out = toast("ok", variant).into_string();
        assert!(out.contains(r#"role="status""#), "{out}");
        assert!(out.contains(r#"aria-live="polite""#), "{out}");
        assert!(
            !out.contains(r#"aria-live="assertive""#),
            "non-error should not be assertive: {out}"
        );
    }
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

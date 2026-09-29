//! Tests for the server-rendered chart widgets (issue #1231):
//! `sparkline`, `bar_chart`, `line_chart` and their `_with` variants.
//!
//! Run with `cargo test --test integration_tests widgets_charts`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{
    ChartConfig, bar_chart, bar_chart_with, line_chart, line_chart_with, sparkline, sparkline_with,
};

// ── AC1/AC3: pure inline SVG, no script / no external assets ─────────────────

#[test]
fn every_helper_returns_accessible_script_free_svg() {
    let data = [("a", 1.0), ("b", 4.0), ("c", 2.0)];
    for (html, modifier) in [
        (sparkline(&data).into_string(), "autumn-chart--sparkline"),
        (bar_chart(&data).into_string(), "autumn-chart--bar"),
        (line_chart(&data).into_string(), "autumn-chart--line"),
    ] {
        assert!(html.contains(r#"role="img""#), "{html}");
        assert!(html.contains("<svg"), "{html}");
        assert!(html.contains(modifier), "{html}");
        // AC3: pure inline SVG — no script, no external asset, no inline style.
        assert!(!html.contains("<script"), "no scripts: {html}");
        assert!(!html.contains("style="), "no inline style: {html}");
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "{html}"
        );
    }
}

// ── AC5: title/desc + optional visually-hidden table fallback ────────────────

#[test]
fn title_and_desc_are_the_first_svg_children() {
    let html = line_chart(&[("a", 1.0), ("b", 2.0)]).into_string();
    let title_at = html.find("<title>").expect("has <title>");
    let desc_at = html.find("<desc>").expect("has <desc>");
    let first_shape = html
        .find("<polyline")
        .or_else(|| html.find("<circle"))
        .expect("has a shape");
    assert!(title_at < desc_at, "title before desc: {html}");
    assert!(desc_at < first_shape, "desc before geometry: {html}");
}

#[test]
fn table_fallback_is_opt_in_and_visually_hidden() {
    let data = [("Jan", 10.0), ("Feb", 20.0)];
    let without = bar_chart(&data).into_string();
    assert!(!without.contains("autumn-chart__table"), "{without}");

    let with = bar_chart_with(&data, &ChartConfig::new().with_table()).into_string();
    assert!(with.contains("autumn-chart__table"), "{with}");
    assert!(with.contains("autumn-visually-hidden"), "{with}");
    assert!(with.contains("<td>Jan</td>"), "{with}");
    assert!(with.contains("<td>20</td>"), "{with}");
}

// ── AC2: typed points, graceful empty / single / degenerate input ────────────

#[test]
fn empty_input_is_graceful_across_all_helpers() {
    for html in [
        sparkline(&[]).into_string(),
        bar_chart(&[]).into_string(),
        line_chart(&[]).into_string(),
    ] {
        assert!(html.contains(r#"role="img""#), "{html}");
        assert!(html.contains("No data"), "{html}");
        assert!(!html.contains("NaN"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }
}

#[test]
fn single_and_all_equal_inputs_do_not_panic_or_emit_nan() {
    for html in [
        sparkline(&[("x", 3.0)]).into_string(),
        bar_chart(&[("x", 3.0)]).into_string(),
        line_chart(&[("x", 3.0)]).into_string(),
        bar_chart(&[("a", 2.0), ("b", 2.0), ("c", 2.0)]).into_string(),
    ] {
        assert!(!html.contains("NaN"), "{html}");
        assert!(html.contains("<svg"), "{html}");
    }
}

// ── AC6: auto-scaling with caller override ───────────────────────────────────

#[test]
fn caller_axis_override_changes_the_geometry() {
    let data = [("a", 0.0), ("b", 5.0), ("c", 10.0)];
    let auto = line_chart(&data).into_string();
    let overridden = line_chart_with(&data, &ChartConfig::new().min(0.0).max(50.0)).into_string();
    assert_ne!(auto, overridden, "axis override must move plotted points");
}

#[test]
fn config_title_feeds_aria_label_and_svg_title() {
    let html = sparkline_with(
        &[("a", 1.0), ("b", 3.0)],
        &ChartConfig::new().title("Weekly signups"),
    )
    .into_string();
    assert!(html.contains(r#"aria-label="Weekly signups""#), "{html}");
    assert!(html.contains("<title>Weekly signups</title>"), "{html}");
}

// ── XSS: hostile labels are escaped in the table fallback ────────────────────

#[test]
fn hostile_labels_are_escaped() {
    let html = bar_chart_with(
        &[("<script>alert('x')</script>", 1.0)],
        &ChartConfig::new().with_table(),
    )
    .into_string();
    assert!(html.contains("&lt;script&gt;"), "{html}");
    assert!(!html.contains("<script>alert"), "{html}");
}

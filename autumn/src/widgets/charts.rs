//! Chart widgets: sparkline, bar chart and line chart.
//!
//! Needs the `widgets-charts` feature (on by default).

use super::merge_class;

/// Rendering options shared by [`sparkline_with`], [`bar_chart_with`], and
/// [`line_chart_with`].
///
/// Build with [`ChartConfig::new`] and chain the setters. Every option is
/// off/`None` by default: the chart auto-scales its axis from the data, uses a
/// sensible default accessible name, and omits the table fallback.
///
/// Charts never emit a `style=` attribute — colors and strokes come from the
/// `.autumn-chart__*` classes referencing design tokens, so they re-theme via
/// `tokens.css` and survive a strict `style-src`/`nonce` CSP.
#[derive(Debug, Clone, Default)]
pub struct ChartConfig<'a> {
    class: Option<&'a str>,
    title: Option<&'a str>,
    desc: Option<&'a str>,
    min: Option<f64>,
    max: Option<f64>,
    table_fallback: bool,
}

impl<'a> ChartConfig<'a> {
    /// A default config: auto-scaled axis, default accessible name, no table
    /// fallback, no extra class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an extra class to the root `autumn-chart` element (merged via the
    /// framework's class helper).
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Set the accessible name — used for the `<svg>` `aria-label` and the
    /// embedded `<title>`. Falls back to a per-kind default (`"Sparkline"`,
    /// `"Bar chart"`, `"Line chart"`) when unset.
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Set the long description emitted as the SVG `<desc>`. Falls back to a
    /// generated one-line summary of the series when unset.
    #[must_use]
    pub const fn desc(mut self, desc: &'a str) -> Self {
        self.desc = Some(desc);
        self
    }

    /// Override the axis minimum. When unset the minimum is taken from the data
    /// (auto-scaling).
    #[must_use]
    pub const fn min(mut self, min: f64) -> Self {
        self.min = Some(min);
        self
    }

    /// Override the axis maximum. When unset the maximum is taken from the data
    /// (auto-scaling).
    #[must_use]
    pub const fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Also emit a visually-hidden `<table>` of the `(label, value)` data after
    /// the chart, so assistive tech and no-CSS clients get the exact numbers.
    #[must_use]
    pub const fn with_table(mut self) -> Self {
        self.table_fallback = true;
        self
    }
}

/// Format a computed SVG coordinate to at most three decimals, trimming
/// trailing zeros so the output is compact and deterministic (raw `f64`
/// `Display` produces long, noisy tails).
fn fmt_coord(v: f64) -> String {
    let s = format!("{v:.3}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Format a data value for the visually-hidden table fallback — up to three
/// decimals, trailing zeros trimmed.
fn fmt_value(v: f64) -> String {
    fmt_coord(v)
}

/// Resolve the `(min, max)` axis bounds: caller overrides win, otherwise the
/// bounds are taken from the data. Guarantees `min <= max` even under
/// contradictory overrides, so downstream scaling never divides by a negative
/// span.
fn resolve_bounds(points: &[(&str, f64)], config: &ChartConfig<'_>) -> (f64, f64) {
    let mut data_min = f64::INFINITY;
    let mut data_max = f64::NEG_INFINITY;
    for (_, v) in points {
        if v.is_finite() {
            data_min = data_min.min(*v);
            data_max = data_max.max(*v);
        }
    }
    if !data_min.is_finite() {
        // No finite data points: fall back to a unit range.
        data_min = 0.0;
        data_max = 1.0;
    }
    let mut min = config.min.filter(|v| v.is_finite()).unwrap_or(data_min);
    let mut max = config.max.filter(|v| v.is_finite()).unwrap_or(data_max);
    if max < min {
        std::mem::swap(&mut min, &mut max);
    }
    (min, max)
}

/// Resolve the `(min, max)` axis bounds for a **bar** chart. Identical to
/// [`resolve_bounds`] except that an *automatic* bound (one the caller did not
/// pin via [`ChartConfig::min`] / [`ChartConfig::max`]) is anchored at zero:
/// the automatic lower bound is `min(0.0, data_min)` and the automatic upper
/// bound is `max(0.0, data_max)`. This is the standard bar convention — bars
/// measure magnitude from a zero baseline, so the smallest bar of an
/// all-positive series (e.g. data whose minimum is well above zero) still
/// renders with a visible, proportional height instead of collapsing to zero.
/// An explicit caller override is respected unchanged on the side it pins (no
/// zero is injected there).
fn resolve_bar_bounds(points: &[(&str, f64)], config: &ChartConfig<'_>) -> (f64, f64) {
    let mut data_min = f64::INFINITY;
    let mut data_max = f64::NEG_INFINITY;
    for (_, v) in points {
        if v.is_finite() {
            data_min = data_min.min(*v);
            data_max = data_max.max(*v);
        }
    }
    if !data_min.is_finite() {
        // No finite data points: fall back to a unit range.
        data_min = 0.0;
        data_max = 1.0;
    }
    let mut min = config
        .min
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| data_min.min(0.0));
    let mut max = config
        .max
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| data_max.max(0.0));
    if max < min {
        std::mem::swap(&mut min, &mut max);
    }
    (min, max)
}

/// Map a value to a `0.0..=1.0` fraction of the axis. Returns `0.5` when the
/// span is zero (single point, all-equal values, or `min == max` override),
/// avoiding a divide-by-zero and placing the datum at mid-height.
fn norm_frac(v: f64, min: f64, max: f64) -> f64 {
    let span = max - min;
    if span <= 0.0 || !v.is_finite() {
        0.5
    } else {
        ((v - min) / span).clamp(0.0, 1.0)
    }
}

/// Compute the plotted `(x, y)` pixel coordinates for each point within a
/// `w × h` viewBox, inset by `pad` on every edge.
///
/// The `usize`→`f64` index casts are exact for any realistic point count, and
/// the geometry reads clearer as `base + frac * span` than as `mul_add`.
#[allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]
fn chart_coords(
    points: &[(&str, f64)],
    width: f64,
    height: f64,
    pad: f64,
    min: f64,
    max: f64,
) -> Vec<(f64, f64)> {
    let n = points.len();
    let usable_w = width - 2.0 * pad;
    let usable_h = height - 2.0 * pad;
    points
        .iter()
        .enumerate()
        .map(|(i, (_, value))| {
            let x = if n <= 1 {
                width / 2.0
            } else {
                pad + (i as f64 / (n - 1) as f64) * usable_w
            };
            let frac = norm_frac(*value, min, max);
            let y = pad + (1.0 - frac) * usable_h;
            (x, y)
        })
        .collect()
}

/// The default accessible name / `<title>` per chart kind.
fn chart_default_title(kind: &str) -> &'static str {
    match kind {
        "bar" => "Bar chart",
        "line" => "Line chart",
        _ => "Sparkline",
    }
}

/// Build the SVG `<desc>` text: caller override, else a generated one-line
/// summary (or `"No data"` for an empty series).
fn chart_desc(points: &[(&str, f64)], config: &ChartConfig<'_>) -> String {
    if let Some(desc) = config.desc {
        return desc.to_string();
    }
    if points.is_empty() {
        return "No data".to_string();
    }
    // The actual data range: a `config.min`/`config.max` override affects plotting only.
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (_, v) in points {
        if v.is_finite() {
            min = min.min(*v);
            max = max.max(*v);
        }
    }
    if !min.is_finite() {
        // No finite data points: fall back to a unit range, matching axis bounds.
        min = 0.0;
        max = 1.0;
    }
    format!(
        "{} data points, ranging from {} to {}.",
        points.len(),
        fmt_value(min),
        fmt_value(max)
    )
}

/// The visually-hidden `(label, value)` table fallback (AC #5).
fn chart_table(points: &[(&str, f64)]) -> maud::Markup {
    maud::html! {
        table class="autumn-chart__table autumn-visually-hidden" {
            thead { tr { th { "Label" } th { "Value" } } }
            tbody {
                @for (label, value) in points {
                    tr {
                        td { (label) }
                        td { (fmt_value(*value)) }
                    }
                }
            }
        }
    }
}

/// Wrap chart `body` in the accessible `autumn-chart` root: a `<div>` carrying
/// the kind modifier and merged class, an `<svg role="img">` whose first
/// children are `<title>`/`<desc>`, and an optional table fallback.
fn chart_root(
    kind: &str,
    view_box: &str,
    points: &[(&str, f64)],
    config: &ChartConfig<'_>,
    body: &maud::Markup,
) -> maud::Markup {
    let root = merge_class(&format!("autumn-chart autumn-chart--{kind}"), config.class);
    let label = config.title.unwrap_or_else(|| chart_default_title(kind));
    let desc = chart_desc(points, config);
    maud::html! {
        div class=(root) {
            svg class="autumn-chart__svg" viewBox=(view_box)
                preserveAspectRatio="none" role="img" aria-label=(label)
                focusable="false" {
                title { (label) }
                desc { (desc) }
                (body)
            }
            @if config.table_fallback {
                (chart_table(points))
            }
        }
    }
}

/// Render a compact, single-series **sparkline** as inline SVG — a small trend
/// line with no axes, ideal inline with text or inside a table cell.
///
/// Shorthand for [`sparkline_with`] with a default [`ChartConfig`]. See that
/// function for axis overrides, an accessible name, and the table fallback.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::sparkline;
///
/// let html = sparkline(&[("Mon", 3.0), ("Tue", 5.0), ("Wed", 4.0)]).into_string();
/// assert!(html.contains(r#"role="img""#));
/// assert!(html.contains("autumn-chart--sparkline"));
/// assert!(!html.contains("<script"));
/// assert!(!html.contains("style="));
/// ```
#[must_use]
pub fn sparkline(points: &[(&str, f64)]) -> maud::Markup {
    sparkline_with(points, &ChartConfig::default())
}

/// Render a sparkline with a [`ChartConfig`] (axis override, accessible name,
/// table fallback). See [`sparkline`] for the common case.
#[must_use]
pub fn sparkline_with(points: &[(&str, f64)], config: &ChartConfig<'_>) -> maud::Markup {
    const W: f64 = 100.0;
    const H: f64 = 30.0;
    const PAD: f64 = 2.0;
    let (min, max) = resolve_bounds(points, config);
    let coords = chart_coords(points, W, H, PAD, min, max);
    let body = maud::html! {
        @if coords.len() == 1 {
            circle class="autumn-chart__point" cx=(fmt_coord(coords[0].0))
                cy=(fmt_coord(coords[0].1)) r="1.5" {}
        } @else if coords.len() > 1 {
            polyline class="autumn-chart__line" points=(polyline_points(&coords)) {}
        }
    };
    chart_root("sparkline", "0 0 100 30", points, config, &body)
}

/// Join plotted coordinates into an SVG `points` attribute value
/// (`"x1,y1 x2,y2 …"`). Built as a `String` so raw `f64` values are never
/// interpolated directly into markup.
fn polyline_points(coords: &[(f64, f64)]) -> String {
    let mut s = String::new();
    for (i, (x, y)) in coords.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&fmt_coord(*x));
        s.push(',');
        s.push_str(&fmt_coord(*y));
    }
    s
}

/// Render a single-series **bar chart** as inline SVG.
///
/// Shorthand for [`bar_chart_with`] with a default [`ChartConfig`]. Each
/// `(label, value)` becomes one `<rect>`; bar heights auto-scale from the data
/// unless overridden via [`ChartConfig::min`] / [`ChartConfig::max`].
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::bar_chart;
///
/// let html = bar_chart(&[("Q1", 12.0), ("Q2", 19.0), ("Q3", 7.0)]).into_string();
/// assert!(html.contains("autumn-chart--bar"));
/// assert!(html.contains("<rect"));
/// assert!(!html.contains("style="));
/// ```
#[must_use]
pub fn bar_chart(series: &[(&str, f64)]) -> maud::Markup {
    bar_chart_with(series, &ChartConfig::default())
}

/// Render a bar chart with a [`ChartConfig`]. See [`bar_chart`] for the common
/// case.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]
pub fn bar_chart_with(series: &[(&str, f64)], config: &ChartConfig<'_>) -> maud::Markup {
    const W: f64 = 100.0;
    const H: f64 = 40.0;
    const PAD: f64 = 2.0;
    let (min, max) = resolve_bar_bounds(series, config);
    let usable_h = H - 2.0 * PAD;
    let bottom = H - PAD;
    let span = max - min;
    // Bars grow up or down from the zero line, with length proportional to magnitude. A
    // span `<= 0` (only an all-zero series, since automatic bounds are zero-anchored)
    // gives every bar zero height.
    let zero_frac = if span > 0.0 {
        ((0.0 - min) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let y_zero = bottom - zero_frac * usable_h;
    let n = series.len();
    let body = maud::html! {
        @if n > 0 {
            @let slot = (W - 2.0 * PAD) / n as f64;
            @let bar_w = slot * 0.7;
            @for (i, (_, v)) in series.iter().enumerate() {
                @let v_frac = if span > 0.0 && v.is_finite() {
                    ((*v - min) / span).clamp(0.0, 1.0)
                } else {
                    zero_frac
                };
                @let y_v = bottom - v_frac * usable_h;
                @let y = y_zero.min(y_v);
                @let bar_h = (y_zero - y_v).abs();
                @let x = PAD + i as f64 * slot + (slot - bar_w) / 2.0;
                rect class="autumn-chart__bar" x=(fmt_coord(x)) y=(fmt_coord(y))
                    width=(fmt_coord(bar_w)) height=(fmt_coord(bar_h)) {}
            }
        }
    };
    chart_root("bar", "0 0 100 40", series, config, &body)
}

/// Render a single-series **line chart** as inline SVG — a trend line with a
/// marker at each data point.
///
/// Shorthand for [`line_chart_with`] with a default [`ChartConfig`]. The axis
/// auto-scales from the data unless overridden.
///
/// # Example
///
/// A 30-point trend series (e.g. daily signups), rendered to accessible,
/// script-free inline SVG:
///
/// ```rust
/// use autumn_web::widgets::line_chart;
///
/// // Build 30 points with a loop — a smooth demo trend.
/// let owned: Vec<(String, f64)> = (0..30)
///     .map(|day| {
///         let label = format!("Day {}", day + 1);
///         let value = (day as f64 * 0.4).sin() * 10.0 + 20.0;
///         (label, value)
///     })
///     .collect();
/// let points: Vec<(&str, f64)> = owned.iter().map(|(l, v)| (l.as_str(), *v)).collect();
///
/// let html = line_chart(&points).into_string();
/// assert!(html.contains(r#"role="img""#));
/// assert!(html.contains("autumn-chart--line"));
/// assert!(html.contains("<polyline"));
/// // Pure inline SVG: no scripts, no inline styles, no external assets.
/// assert!(!html.contains("<script"));
/// assert!(!html.contains("style="));
/// ```
///
/// ## Pairing with a grouped-aggregate query
///
/// Map the `Vec<(K, V)>` a repository grouped-aggregate returns into
/// `(&str, f64)` chart points. `count_*` yields `i64`; `sum_*`/`avg_*` yield an
/// `Option`, so coalesce with `.unwrap_or(0)` before converting:
///
/// ```rust,ignore
/// use autumn_web::aggregate::DateBucket;
/// use autumn_web::widgets::line_chart;
///
/// // A day-bucketed signup time series.
/// let per_day: Vec<(chrono::NaiveDateTime, i64)> = repo
///     .count_grouped_by_created_at()
///     .bucket(DateBucket::Day)
///     .filter_range(window_start, window_end)
///     .load()
///     .await?;
///
/// let labels: Vec<String> =
///     per_day.iter().map(|(day, _)| day.format("%b %d").to_string()).collect();
/// let points: Vec<(&str, f64)> = per_day
///     .iter()
///     .zip(&labels)
///     .map(|((_, count), label)| (label.as_str(), *count as f64))
///     .collect();
/// let svg = line_chart(&points).into_string();
///
/// // For a nullable aggregate (sum/avg), coalesce the Option first:
/// // let revenue: Vec<(chrono::NaiveDateTime, Option<i64>)> = ...;
/// // let v = *value; let point = v.unwrap_or(0) as f64;
/// ```
#[must_use]
pub fn line_chart(series: &[(&str, f64)]) -> maud::Markup {
    line_chart_with(series, &ChartConfig::default())
}

/// Render a line chart with a [`ChartConfig`]. See [`line_chart`] for the
/// common case.
#[must_use]
pub fn line_chart_with(series: &[(&str, f64)], config: &ChartConfig<'_>) -> maud::Markup {
    const W: f64 = 100.0;
    const H: f64 = 40.0;
    const PAD: f64 = 3.0;
    let (min, max) = resolve_bounds(series, config);
    let coords = chart_coords(series, W, H, PAD, min, max);
    let body = maud::html! {
        @if coords.len() > 1 {
            polyline class="autumn-chart__line" points=(polyline_points(&coords)) {}
        }
        @for (x, y) in &coords {
            circle class="autumn-chart__point" cx=(fmt_coord(*x)) cy=(fmt_coord(*y)) r="1.5" {}
        }
    };
    chart_root("line", "0 0 100 40", series, config, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_is_accessible_inline_svg() {
        let html = sparkline(&[("a", 1.0), ("b", 2.0), ("c", 1.5)]).into_string();
        assert!(html.contains(r#"role="img""#), "{html}");
        assert!(
            html.contains("autumn-chart autumn-chart--sparkline"),
            "{html}"
        );
        assert!(html.contains("autumn-chart__svg"), "{html}");
        assert!(html.contains("<polyline"), "{html}");
        assert!(html.contains("<title>Sparkline</title>"), "{html}");
        assert!(html.contains("<desc>"), "{html}");
    }

    #[test]
    fn bar_chart_emits_a_rect_per_datum() {
        let html = bar_chart(&[("Q1", 3.0), ("Q2", 5.0), ("Q3", 4.0)]).into_string();
        assert_eq!(html.matches("<rect").count(), 3, "{html}");
        assert!(html.contains("autumn-chart--bar"), "{html}");
        assert!(html.contains("autumn-chart__bar"), "{html}");
    }

    #[test]
    fn line_chart_emits_polyline_and_points() {
        let html = line_chart(&[("a", 1.0), ("b", 9.0), ("c", 4.0)]).into_string();
        assert!(html.contains("autumn-chart--line"), "{html}");
        assert!(html.contains("<polyline"), "{html}");
        assert_eq!(html.matches("<circle").count(), 3, "{html}");
    }

    #[test]
    fn charts_never_emit_script_or_inline_style() {
        for html in [
            sparkline(&[("a", 1.0), ("b", 2.0)]).into_string(),
            bar_chart(&[("a", 1.0), ("b", 2.0)]).into_string(),
            line_chart(&[("a", 1.0), ("b", 2.0)]).into_string(),
        ] {
            assert!(!html.contains("<script"), "no scripts: {html}");
            assert!(!html.contains("style="), "no inline style: {html}");
            assert!(!html.contains("http://"), "no external assets: {html}");
        }
    }

    #[test]
    fn chart_escapes_hostile_labels_in_table_fallback() {
        let html = bar_chart_with(
            &[("<script>alert(1)</script>", 5.0)],
            &ChartConfig::new().with_table(),
        )
        .into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
    }

    #[test]
    fn empty_input_renders_valid_svg_without_panic() {
        for html in [
            sparkline(&[]).into_string(),
            bar_chart(&[]).into_string(),
            line_chart(&[]).into_string(),
        ] {
            assert!(html.contains(r#"role="img""#), "{html}");
            assert!(html.contains("<desc>No data</desc>"), "{html}");
            assert!(!html.contains("NaN"), "no NaN coordinates: {html}");
            assert!(!html.contains("<script"), "{html}");
        }
    }

    #[test]
    fn single_point_does_not_divide_by_zero() {
        let html = sparkline(&[("only", 7.0)]).into_string();
        assert!(
            html.contains("<circle"),
            "single point renders a dot: {html}"
        );
        assert!(!html.contains("NaN"), "{html}");
        // Centered horizontally (span is zero).
        assert!(html.contains(r#"cx="50""#), "{html}");
    }

    #[test]
    fn all_equal_positive_values_render_full_height_bars() {
        let html = bar_chart(&[("a", 4.0), ("b", 4.0), ("c", 4.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        // Zero-anchored bounds are [0, 4], so every bar is the max and spans the full
        // usable height (H - 2*PAD = 36), not the 0.5 midline a flat line uses.
        assert_eq!(html.matches(r#"height="36""#).count(), 3, "{html}");
        assert!(!html.contains(r#"height="18""#), "not half height: {html}");
    }

    #[test]
    fn all_zero_bars_render_zero_height() {
        // The zero-span midline would draw misleading half-height (18) bars.
        let html = bar_chart(&[("a", 0.0), ("b", 0.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        assert_eq!(html.matches(r#"height="0""#).count(), 2, "{html}");
        assert!(
            !html.contains(r#"height="18""#),
            "no half-height bars: {html}"
        );
    }

    /// Extract every `<rect>` `height="…"` value, in document (series) order.
    #[cfg(test)]
    fn bar_heights(html: &str) -> Vec<f64> {
        html.match_indices(r#"height=""#)
            .map(|(i, m)| {
                let rest = &html[i + m.len()..];
                let end = rest.find('"').unwrap();
                rest[..end].parse::<f64>().unwrap()
            })
            .collect()
    }

    #[test]
    fn bar_auto_baseline_includes_zero_so_min_positive_bar_is_visible() {
        // The automatic baseline is zero, not the data minimum (7), so the smallest
        // bar keeps a proportional, positive height.
        let html = bar_chart(&[("Q1", 12.0), ("Q2", 19.0), ("Q3", 7.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        assert_eq!(heights.len(), 3, "{html}");
        let (q1, q2, q3) = (heights[0], heights[1], heights[2]);
        assert!(q3 > 0.0, "smallest bar must be visible, got {q3}: {html}");
        assert!(
            !html.contains(r#"height="0""#),
            "no zero-height bar: {html}"
        );
        assert!(q2 > q3, "tallest (Q2=19) taller than Q3=7: {q2} vs {q3}");
        assert!(q2 > q1 && q1 > q3, "12 between 7 and 19: {q1}");
        // The max fills the usable height (H - 2*PAD = 36).
        assert!((q2 - 36.0).abs() < 1e-6, "max bar is full height: {q2}");
        let expected_q3 = 7.0 / 19.0 * 36.0;
        assert!(
            (q3 - expected_q3).abs() < 1e-3,
            "Q3 scaled from zero baseline: {q3}"
        );
    }

    #[test]
    fn explicit_bar_min_override_wins_over_zero_baseline() {
        // A pinned `min` is used as-is, with no zero injected, so the bar at the
        // pinned minimum (7) has zero height.
        let html = bar_chart_with(
            &[("Q1", 12.0), ("Q2", 19.0), ("Q3", 7.0)],
            &ChartConfig::new().min(7.0),
        )
        .into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        assert_eq!(heights.len(), 3, "{html}");
        assert!(
            (heights[2] - 0.0).abs() < 1e-9,
            "explicit min pins Q3 to zero: {}",
            heights[2]
        );
        assert!(html.contains(r#"height="0""#), "{html}");
    }

    #[test]
    fn negative_bars_diverge_downward_largest_magnitude_is_tallest() {
        // Bounds resolve to [-10, 0], so bars grow down from the zero line and -10,
        // sitting at the axis floor, is the tallest rather than zero height.
        let html = bar_chart(&[("a", -10.0), ("b", -5.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        assert_eq!(heights.len(), 2, "{html}");
        let (a, b) = (heights[0], heights[1]);
        assert!(a > 0.0, "largest-magnitude bar must be visible: {a}");
        assert!(a > b, "|-10| taller than |-5|: {a} vs {b}");
        // zero_frac = 1 (zero at the top); usable height is 36.
        assert!((a - 36.0).abs() < 1e-6, "-10 fills usable height: {a}");
        assert!((b - 18.0).abs() < 1e-6, "-5 is half height: {b}");
        assert!(
            !html.contains(r#"height="0""#),
            "no zero-height bar for a nonzero series: {html}"
        );
    }

    /// Extract every `<rect>` `y="…"` value, in document (series) order.
    #[cfg(test)]
    fn bar_ys(html: &str) -> Vec<f64> {
        html.match_indices(r#"y=""#)
            .map(|(i, m)| {
                let rest = &html[i + m.len()..];
                let end = rest.find('"').unwrap();
                rest[..end].parse::<f64>().unwrap()
            })
            .collect()
    }

    #[test]
    fn mixed_sign_bars_share_a_zero_baseline() {
        // Bounds resolve to [-5, 10]: the +10 bar sits above the zero line and the
        // -5 bar below it, both meeting at the same baseline y.
        let html = bar_chart(&[("up", 10.0), ("down", -5.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        let ys = bar_ys(&html);
        assert_eq!(heights.len(), 2, "{html}");
        assert_eq!(ys.len(), 2, "{html}");
        assert!(heights[0] > 0.0 && heights[1] > 0.0, "both visible: {html}");
        // y_zero = bottom (H-PAD=38) - zero_frac (5/15) * usable height (36) = 26.
        let y_zero = 26.0;
        // The positive bar's bottom edge (y + height) meets the zero line.
        let up_bottom = ys[0] + heights[0];
        assert!(
            (up_bottom - y_zero).abs() < 1e-6,
            "positive bar's bottom edge on the zero line: {up_bottom} vs {y_zero}"
        );
        // The negative bar's top edge (y) meets it.
        assert!(
            (ys[1] - y_zero).abs() < 1e-6,
            "negative bar's top edge on the zero line: {} vs {y_zero}",
            ys[1]
        );
    }

    #[test]
    fn negative_values_scale_without_panic() {
        let html = line_chart(&[("a", -5.0), ("b", 0.0), ("c", 5.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        assert!(html.contains("<polyline"), "{html}");
        // Min (-5) sits at the bottom, max (5) at the top of the inset area.
        assert!(html.contains(r#"points="3,37"#), "min at bottom: {html}");
    }

    #[test]
    fn explicit_axis_override_changes_coordinates() {
        let data = [("a", 0.0), ("b", 5.0), ("c", 10.0)];
        let auto = sparkline(&data).into_string();
        let overridden =
            sparkline_with(&data, &ChartConfig::new().min(0.0).max(100.0)).into_string();
        assert_ne!(
            auto, overridden,
            "caller min/max override must move the plotted points"
        );
        // Under the 0..100 override the middle datum drops toward the baseline.
        assert!(overridden.contains("50,26.7"), "{overridden}");
        assert!(auto.contains("50,15"), "{auto}");
    }

    #[test]
    fn non_finite_axis_override_is_ignored_no_nan_in_output() {
        let data = [("a", 1.0), ("b", 2.0), ("c", 3.0)];
        // A non-finite override (e.g. from an aggregate's `Option<f64>`) is treated as
        // unset, so the SVG never contains `NaN` or `inf` coordinates.
        let bad_configs = [
            ChartConfig::new().max(f64::NAN),
            ChartConfig::new().min(f64::INFINITY),
            ChartConfig::new().min(f64::NEG_INFINITY),
            ChartConfig::new().max(f64::INFINITY),
        ];
        let check = |name: &str, render: &dyn Fn(&ChartConfig<'_>) -> String| {
            let auto = render(&ChartConfig::new());
            for config in &bad_configs {
                let overridden = render(config);
                for needle in ["NaN", "inf", "Infinity"] {
                    assert!(
                        !overridden.contains(needle),
                        "{name}: non-finite override leaked `{needle}` into SVG: {overridden}"
                    );
                }
                assert_eq!(
                    auto, overridden,
                    "{name}: non-finite override must be ignored (auto-scale used)"
                );
            }
        };
        check("sparkline", &|config| {
            sparkline_with(&data, config).into_string()
        });
        check("line_chart", &|config| {
            line_chart_with(&data, config).into_string()
        });
        check("bar_chart", &|config| {
            bar_chart_with(&data, config).into_string()
        });
    }

    #[test]
    fn desc_reports_data_range_not_axis_override() {
        let html = sparkline_with(
            &[("a", 0.0), ("b", 5.0), ("c", 10.0)],
            &ChartConfig::new().max(100.0),
        )
        .into_string();
        assert!(
            html.contains("<desc>3 data points, ranging from 0 to 10.</desc>"),
            "{html}"
        );
        assert!(
            !html.contains("ranging from 0 to 100"),
            "must not leak axis override into desc: {html}"
        );
    }

    #[test]
    fn table_fallback_lists_labels_and_values() {
        let html = line_chart_with(
            &[("Mon", 3.0), ("Tue", 5.5)],
            &ChartConfig::new().with_table(),
        )
        .into_string();
        assert!(html.contains("autumn-chart__table"), "{html}");
        assert!(html.contains("autumn-visually-hidden"), "{html}");
        assert!(html.contains("<td>Mon</td>"), "{html}");
        assert!(html.contains("<td>5.5</td>"), "{html}");
    }

    #[test]
    fn config_title_and_desc_feed_accessibility() {
        let html = sparkline_with(
            &[("a", 1.0), ("b", 2.0)],
            &ChartConfig::new()
                .title("Weekly signups")
                .desc("Signups per day for the last week"),
        )
        .into_string();
        assert!(html.contains(r#"aria-label="Weekly signups""#), "{html}");
        assert!(html.contains("<title>Weekly signups</title>"), "{html}");
        assert!(
            html.contains("<desc>Signups per day for the last week</desc>"),
            "{html}"
        );
    }

    #[test]
    fn config_class_is_merged_onto_root() {
        let html = bar_chart_with(&[("a", 1.0)], &ChartConfig::new().class("dashboard-widget"))
            .into_string();
        assert!(
            html.contains("autumn-chart autumn-chart--bar dashboard-widget"),
            "{html}"
        );
    }

    #[test]
    fn fmt_coord_trims_trailing_zeros() {
        assert_eq!(fmt_coord(10.0), "10");
        assert_eq!(fmt_coord(0.5), "0.5");
        assert_eq!(fmt_coord(0.0), "0");
        assert_eq!(fmt_coord(-0.0), "0");
        assert_eq!(fmt_coord(1.2345), "1.234");
    }
}

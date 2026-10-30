//! Framework-owned widget component CSS (issue #1215).
//!
//! Every `autumn-*` semantic class emitted by [`crate::form`], [`crate::widgets`],
//! [`crate::wizard`], [`crate::ui::pagination`], [`crate::storage::form_helper`],
//! and [`crate::job_tracking`] is backed by a rule here, so widgets render
//! styled out of the box — Tailwind or not — instead of shipping an unbacked
//! class hook the app must style itself.
//!
//! # Example
//!
//! Link the bundle from your base layout to style every widget with a single
//! `<link>`, no Tailwind build required:
//!
//! ```html
//! <link rel="stylesheet" href="/static/css/autumn-widgets.css">
//! ```
//!
//! Re-theme by overriding the [`crate::ui::tokens`] custom properties on
//! `:root` in your own stylesheet (loaded after this one) — not by forking
//! these rules.

/// Component rules only: `.autumn-field`, `.autumn-nav`, `.autumn-modal`, etc.
///
/// References the [`crate::ui::tokens`] custom properties (`var(--primary)`,
/// `var(--border)`, `var(--radius)`, …) rather than hard-coded colors, so an
/// app re-themes by overriding tokens. Does not itself define those
/// variables — see [`WIDGETS_CSS`] for the self-contained bundle.
pub const WIDGETS_COMPONENT_CSS: &str = include_str!("widgets.css");

/// Self-contained bundle: [`WIDGETS_COMPONENT_CSS`] prefixed with the shared
/// design tokens ([`crate::ui::tokens::TOKENS_CSS`]).
///
/// A single `<link>` to [`WIDGETS_CSS_PATH`] styles every widget with no
/// other stylesheet — Tailwind or otherwise — required.
pub const WIDGETS_CSS: &str = concat!(
    include_str!("tokens.css"),
    "\n",
    include_str!("widgets.css")
);

/// URL of the framework-served widget stylesheet.
///
/// The default Autumn server mounts this asset automatically. Link it from
/// your base layout — `link rel="stylesheet" href=(autumn_web::ui::WIDGETS_CSS_PATH);`
/// — so every `autumn-*` widget class renders styled with zero app-authored CSS.
pub const WIDGETS_CSS_PATH: &str = "/static/css/autumn-widgets.css";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::test_support::{Scheme, color, contrast};
    use std::fmt::Write as _;

    #[test]
    fn widgets_css_bundles_tokens_and_components() {
        assert!(WIDGETS_CSS.contains(crate::ui::tokens::TOKENS_CSS));
        assert!(WIDGETS_CSS.contains(WIDGETS_COMPONENT_CSS));
    }

    #[test]
    fn component_css_references_tokens_not_hardcoded_colors() {
        for token_var in ["var(--primary)", "var(--border)", "var(--radius)"] {
            assert!(
                WIDGETS_COMPONENT_CSS.contains(token_var),
                "WIDGETS_COMPONENT_CSS should reference {token_var}"
            );
        }
    }

    /// Issue #2354: the renamed widget classes must each have a rule in the
    /// component stylesheet. The selector boundary check keeps `.autumn-card`
    /// from passing on the strength of `.autumn-card__header` alone.
    #[test]
    fn renamed_widget_classes_are_backed_by_component_css() {
        fn has_rule(css: &str, class: &str) -> bool {
            let needle = format!(".{class}");
            css.match_indices(&needle).any(|(i, _)| {
                matches!(
                    css[i + needle.len()..].chars().next(),
                    Some('{' | ' ' | ',' | ':' | '\n' | '\t')
                )
            })
        }

        for class in [
            "autumn-alert",
            "autumn-alert--info",
            "autumn-alert__dismiss-toggle",
            "autumn-badge",
            "autumn-badge--neutral",
            "autumn-wizard-progress",
            "autumn-wizard-step",
            "autumn-wizard-step__number",
            "autumn-theme-toggle",
            "autumn-theme-toggle__option",
            "autumn-card",
            "autumn-card__header",
            "autumn-card__title",
            "autumn-card__body",
            "autumn-card__footer",
            "autumn-stat-card",
            "autumn-stat-card__label",
            "autumn-stat-card__value",
            "autumn-stat-card__link",
            "autumn-search-empty",
            "autumn-autocomplete-empty",
            "autumn-alert__icon-svg",
            "autumn-active",
        ] {
            assert!(
                has_rule(WIDGETS_COMPONENT_CSS, class),
                "widget emits .{class} but WIDGETS_COMPONENT_CSS has no rule for it"
            );
        }
    }

    /// Wayfinder audit (keyboard walkthrough, tier 1): four widget rules
    /// set `outline: none` and signalled focus with `border-color` +
    /// `box-shadow` only. Forced-colors mode (Windows High Contrast) strips
    /// `box-shadow`, so keyboard users lost the focus indicator (WCAG 2.4.7).
    /// A transparent outline is repainted in the system focus colour under
    /// forced-colors and is invisible otherwise — but only on the `:focus`
    /// state: on the base selector every unfocused field would be outlined
    /// too, and focus would be indistinguishable.
    #[test]
    fn widget_focus_states_keep_an_outline_and_base_rules_have_none() {
        assert!(
            !WIDGETS_COMPONENT_CSS.contains("outline: none"),
            "`outline: none` leaves no focus indicator under forced-colors mode"
        );
        for selector in [
            ".autumn-field__input:focus {",
            ".autumn-submit:focus {",
            ".autumn-search__input:focus {",
            ".autumn-autocomplete__input:focus {",
        ] {
            let start = WIDGETS_COMPONENT_CSS
                .find(selector)
                .unwrap_or_else(|| panic!("missing `{selector}` rule"));
            let end = WIDGETS_COMPONENT_CSS[start..].find('}').unwrap() + start;
            assert!(
                WIDGETS_COMPONENT_CSS[start..end].contains("outline: 2px solid transparent;"),
                "`{selector}` must declare a transparent outline"
            );
        }
        assert_eq!(
            WIDGETS_COMPONENT_CSS
                .matches("outline: 2px solid transparent;")
                .count(),
            4,
            "the transparent outline belongs on the four :focus rules only"
        );
    }
    /// Resolve `#hex` / `var(--x)` / `var(--x, #hex)` / `color-mix(...)`
    /// against the shipped tokens in `scheme`, the way a browser does: the
    /// token wins, and the fallback applies only when the token is undefined.
    fn resolve(value: &str, scheme: Scheme) -> String {
        let value = value.trim();
        if let Some(args) = value
            .strip_prefix("color-mix(in srgb,")
            .and_then(|v| v.strip_suffix(')'))
        {
            // `<color> <pct>%, <color>`
            let (first, second) = args.split_once("%,").unwrap();
            let (a, pct) = first.trim().rsplit_once(' ').unwrap();
            let weight = pct.parse::<u32>().unwrap();
            let (a, b) = (resolve(a, scheme), resolve(second, scheme));
            let channel =
                |h: &str, i: usize| u32::from_str_radix(&h[1 + i * 2..3 + i * 2], 16).unwrap();
            let mixed = (0..3).fold(String::from("#"), |mut out, i| {
                let c = (channel(&a, i) * weight + channel(&b, i) * (100 - weight) + 50) / 100;
                write!(out, "{c:02x}").unwrap();
                out
            });
            return mixed;
        }
        let Some(inner) = value.strip_prefix("var(").and_then(|v| v.strip_suffix(')')) else {
            return value.to_owned();
        };
        let (name, fallback) = inner
            .split_once(',')
            .map_or((inner, None), |(n, f)| (n, Some(f)));
        color(name.trim(), scheme)
            .or_else(|| fallback.map(|f| resolve(f, scheme)))
            .unwrap_or_else(|| panic!("`{name}` is neither a token nor has a fallback"))
    }

    fn rule_body(selector: &str) -> &'static str {
        let at = WIDGETS_COMPONENT_CSS
            .find(&format!("{selector} {{"))
            .unwrap_or_else(|| panic!("missing `{selector}` rule"));
        let body = &WIDGETS_COMPONENT_CSS[at..];
        &body[..body.find('}').unwrap()]
    }

    fn declared(selector: &str, prop: &str, scheme: Scheme) -> String {
        let line = rule_body(selector)
            .lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("{prop}:")))
            .next_back()
            .unwrap_or_else(|| panic!("`{selector}` has no `{prop}`"));
        resolve(line.trim_end_matches(';'), scheme)
    }

    /// Status chips must not derive their text from `--text`: a dark scheme
    /// makes `--text` light. Each chip uses its own `-text` token.
    #[test]
    fn status_chip_text_is_independent_of_the_theme_foreground() {
        for selector in [
            ".autumn-badge--success",
            ".autumn-badge--warning",
            ".autumn-alert--success",
            ".autumn-alert--warning",
        ] {
            assert!(
                !rule_body(selector).contains("var(--text"),
                "{selector} reads --text"
            );
        }
    }

    /// Wayfinder audit (contrast, tier 1), extended by #3396 to both schemes:
    /// computed text/background pairs against the shipped tokens. WCAG 1.4.3
    /// needs 4.5:1 for normal-size text.
    #[test]
    fn widget_text_meets_aa_contrast_against_its_own_background() {
        let mut failures = Vec::new();
        for scheme in Scheme::BOTH {
            for (selector, bg) in [
                (".autumn-badge--neutral", "background"),
                (".autumn-badge--info", "background"),
                (".autumn-badge--success", "background"),
                (".autumn-badge--warning", "background"),
                (".autumn-badge--danger", "background"),
                (".autumn-alert--info", "background"),
                (".autumn-alert--success", "background"),
                (".autumn-alert--warning", "background"),
                (".autumn-alert--error", "background"),
                (".autumn-job-status__success", ""),
                (".autumn-job-status__error", ""),
                (".autumn-submit", "background-color"),
                (".autumn-pager__current", "background-color"),
                (".autumn-modal__confirm", "background-color"),
                (".autumn-modal__confirm--danger", "background-color"),
                (".autumn-hero__cta--primary", "background-color"),
                (
                    ".autumn-theme-toggle__option[aria-pressed=\"true\"]",
                    "background",
                ),
                (".autumn-wizard-step__number", "background-color"),
                (
                    ".autumn-wizard-step--completed .autumn-wizard-step__number",
                    "background-color",
                ),
                (
                    ".autumn-wizard-step--current .autumn-wizard-step__number",
                    "background-color",
                ),
                (
                    ".autumn-wizard-step--upcoming .autumn-wizard-step__number",
                    "background-color",
                ),
            ] {
                let bg = if bg.is_empty() {
                    resolve("var(--surface)", scheme)
                } else {
                    declared(selector, bg, scheme)
                };
                let ratio = contrast(&declared(selector, "color", scheme), &bg);
                if ratio < 4.5 {
                    failures.push(format!("{scheme:?} {selector}: {ratio:.2}:1"));
                }
            }
        }
        assert!(failures.is_empty(), "below 4.5:1 — {failures:?}");
    }

    /// Issue #3396: a dark scheme makes `--primary` and `--danger` light, so
    /// text on a token fill must use `--on-primary` / `--on-danger`, not white.
    #[test]
    fn no_white_text_on_a_token_fill() {
        for rule in WIDGETS_COMPONENT_CSS.split('}') {
            let white = ["color: #fff;", "color: #ffffff;", "color: white;"]
                .iter()
                .any(|w| rule.contains(w));
            let token_fill = rule
                .lines()
                .any(|l| l.trim().starts_with("background") && l.contains("var(--"));
            assert!(
                !(white && token_fill),
                "white text on a token fill:{rule}}}"
            );
        }
    }

    /// The alert icon glyph sits on a `currentColor` circle. It must stand
    /// out from the circle in both schemes (WCAG 1.4.11, 3:1).
    #[test]
    fn alert_icon_glyph_is_visible_in_both_schemes() {
        let glyph = declared(".autumn-alert__icon-glyph", "fill", Scheme::Light);
        assert!(glyph.starts_with('#'), "{glyph}");
        for scheme in Scheme::BOTH {
            for variant in ["info", "success", "warning", "error"] {
                let circle = declared(&format!(".autumn-alert--{variant}"), "color", scheme);
                let glyph = declared(".autumn-alert__icon-glyph", "fill", scheme);
                let ratio = contrast(&glyph, &circle);
                assert!(ratio >= 3.0, "{scheme:?} {variant}: {ratio:.2}:1");
            }
        }
    }

    /// A widget that paints a token surface also sets its text color.
    /// Otherwise it inherits the page color, which can be the other scheme.
    #[test]
    fn a_token_surface_sets_its_own_text_color() {
        let mut missing = Vec::new();
        let mut rest = WIDGETS_COMPONENT_CSS;
        while let Some(open) = rest.find('{') {
            let selector = rest[..open].rsplit(['}', '/']).next().unwrap_or("").trim();
            let close = open + rest[open..].find('}').unwrap();
            let body = &rest[open..close];
            let paints = body.lines().any(|l| {
                let l = l.trim();
                l.starts_with("background") && l.contains("var(--surface")
            });
            let colors = body.lines().any(|l| l.trim().starts_with("color:"));
            if paints && !colors && !selector.contains(':') {
                missing.push(selector.to_owned());
            }
            rest = &rest[close + 1..];
        }
        assert!(missing.is_empty(), "surface without color: {missing:?}");
    }

    /// The modal paints the token surface. The browser's dialog colors are
    /// not the tokens, and in dark mode they hide the dialog edge.
    #[test]
    fn modal_uses_the_token_surface() {
        let body = rule_body(".autumn-modal");
        for declaration in [
            "background: var(--surface);",
            "color: var(--text);",
            "border: 1px solid var(--border);",
        ] {
            assert!(body.contains(declaration), "missing `{declaration}`:{body}");
        }
    }

    /// The exit animation cancels the region gap, which density scales.
    #[test]
    fn toast_exit_follows_the_density_gap() {
        assert!(rule_body(".autumn-toast-region").contains("gap: var(--space-2);"));
        let at = WIDGETS_COMPONENT_CSS
            .find("@keyframes autumn-toast-out")
            .unwrap();
        let frames = &WIDGETS_COMPONENT_CSS[at..at + 400];
        assert!(
            frames.contains("margin-top: calc(-1 * var(--space-2));"),
            "{frames}"
        );
    }

    /// The region is a popover. The UA popover box (centred, bordered,
    /// `Canvas` background) is reset, and `display` wins over the UA
    /// `display: none`, so toasts also show without JS (#3399).
    #[test]
    fn toast_region_resets_the_popover_box() {
        let body = rule_body(".autumn-toast-region");
        for declaration in [
            "display: flex;",
            "inset: auto 1rem 1rem auto;",
            "margin: 0;",
            "padding: 0;",
            "border: 0;",
            "background: transparent;",
            "color: inherit;",
            "overflow: visible;",
        ] {
            assert!(body.contains(declaration), "missing `{declaration}`:{body}");
        }
        assert!(rule_body(".autumn-toast-region::backdrop").contains("display: none;"));
    }

    /// The runtime sets `--autumn-toast-duration`. The default is 5 s.
    #[test]
    fn toast_dwell_reads_the_duration_property() {
        assert!(
            rule_body(".autumn-toast").contains(
                "autumn-toast-out 400ms ease-in var(--autumn-toast-duration, 5s) forwards"
            ),
            "{}",
            rule_body(".autumn-toast")
        );
    }

    /// The stylesheet with LF line ends. A Windows checkout can have CRLF.
    fn lf_css() -> String {
        WIDGETS_COMPONENT_CSS.replace("\r\n", "\n")
    }

    /// WCAG 2.2.1: hover or focus on a toast pauses every toast timer.
    #[test]
    fn toast_timer_pauses_on_hover_and_focus() {
        let list = ".autumn-toast-region:hover .autumn-toast,\n\
                    .autumn-toast-region:focus-within .autumn-toast,\n\
                    .autumn-toast:hover,\n\
                    .autumn-toast:focus-within {";
        assert!(lf_css().contains(list), "missing pause selectors");
        // Only the exit animation pauses. The entrance of a new toast runs.
        assert!(
            rule_body(".autumn-toast:focus-within")
                .contains("animation-play-state: running, paused;")
        );
    }

    /// A persistent toast fades in and never fades out.
    #[test]
    fn persistent_toast_has_no_exit_animation() {
        let body = rule_body(".autumn-toast--persistent");
        assert!(
            body.contains("animation: autumn-toast-in 200ms ease-out;"),
            "{body}"
        );
        assert!(!body.contains("autumn-toast-out"), "{body}");
    }

    /// Reduced motion: no slide, the same dwell, and no animation at all on
    /// a persistent toast.
    #[test]
    fn reduced_motion_keeps_the_dwell_and_the_persistent_toast() {
        let css = lf_css();
        let at = css
            .find("@media (prefers-reduced-motion: reduce) {\n    /* A 0s entrance")
            .expect("reduced-motion toast block");
        let rest = &css[at..];
        let block = &rest[..rest.find("\n}\n").expect("block end")];
        assert!(
            block.contains(
                "autumn-toast-in 0s,\n            \
                 autumn-toast-out 0s linear var(--autumn-toast-duration, 5s) forwards;"
            ),
            "{block}"
        );
        assert!(
            block.contains(".autumn-toast--persistent {\n        animation: none;"),
            "{block}"
        );
    }

    /// The close button stays hidden until the runtime shows it, and its
    /// target is at least 24 px (WCAG 2.5.8).
    #[test]
    fn toast_close_button_is_hidden_without_js_and_large_enough() {
        assert!(rule_body(".autumn-toast__close[hidden]").contains("display: none;"));
        // The toast sets `display: flex`, which beats the UA `[hidden]` rule.
        assert!(rule_body(".autumn-toast[hidden]").contains("display: none;"));
        let body = rule_body(".autumn-toast__close");
        assert!(body.contains("min-width: 1.5rem;"), "{body}");
        assert!(body.contains("min-height: 1.5rem;"), "{body}");
        assert!(
            rule_body(".autumn-toast__close:focus-visible")
                .contains("outline: 2px solid var(--primary);")
        );
    }

    /// WCAG 1.4.11: a text input's border is its only visible boundary, so
    /// it uses `--control-border` (3:1), not the decorative `--border`.
    #[test]
    fn text_inputs_use_the_control_border() {
        for selector in [
            ".autumn-field__input",
            ".autumn-search__input",
            ".autumn-autocomplete__input",
            ".autumn-comment-input",
        ] {
            let body = rule_body(selector);
            assert!(
                body.contains("border: 1px solid var(--control-border);"),
                "{selector}:{body}"
            );
        }
    }
}

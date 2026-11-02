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

    /// An unclosed block turns every later rule into part of it, for example
    /// an `@media` query. Braces must balance, and never go below zero.
    #[test]
    fn component_css_braces_balance() {
        let mut css = String::new();
        let mut rest = WIDGETS_COMPONENT_CSS;
        while let Some(start) = rest.find("/*") {
            css.push_str(&rest[..start]);
            let end = rest[start..]
                .find("*/")
                .map_or(rest.len(), |e| start + e + 2);
            rest = &rest[end..];
        }
        css.push_str(rest);
        let mut depth = 0_i32;
        for c in css.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "a `}}` has no matching `{{`");
        }
        assert_eq!(depth, 0, "{depth} unclosed `{{` in widgets.css");
    }

    /// The drawer paints the token surface and has a rule for each edge,
    /// size and close button.
    #[test]
    fn drawer_uses_tokens_and_styles_every_variant() {
        let body = rule_body(".autumn-drawer");
        for declaration in ["background: var(--surface);", "color: var(--text);"] {
            assert!(body.contains(declaration), "missing `{declaration}`:{body}");
        }
        for selector in [
            ".autumn-drawer--left",
            ".autumn-drawer--right",
            ".autumn-drawer--top",
            ".autumn-drawer--bottom",
            ".autumn-drawer--sm",
            ".autumn-drawer--lg",
            ".autumn-drawer--full",
            ".autumn-modal--sm",
            ".autumn-modal--lg",
            ".autumn-modal--full",
            ".autumn-modal__close",
            ".autumn-drawer__close",
        ] {
            assert!(
                WIDGETS_COMPONENT_CSS.contains(selector),
                "no rule: {selector}"
            );
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

    // ── tooltip (#3404) ──────────────────────────────────────────────

    /// The block that starts at `head`, up to its closing brace.
    fn css_block(head: &str) -> String {
        let css = lf_css();
        let at = css.find(head).unwrap_or_else(|| panic!("missing `{head}`"));
        let end = css[at..].find("\n}\n").expect("block end");
        css[at..at + end].to_owned()
    }

    /// The text is `display: none` until hover or keyboard focus, so a
    /// hidden text adds no scroll overflow. `aria-describedby` still reads
    /// it. A popover shows only when the browser opens it.
    #[test]
    fn tooltip_is_hidden_until_hover_or_keyboard_focus() {
        let body = rule_body(".autumn-tooltip__bubble");
        assert!(body.contains("display: none;"), "{body}");
        assert!(body.contains("pointer-events: auto;"), "{body}");
        // One rule for each selector. A browser without `:has()` or
        // `:popover-open` drops a whole selector list, so a list would lose
        // the hover baseline too.
        for selector in [
            ".autumn-tooltip:hover > .autumn-tooltip__bubble:not([popover])",
            ".autumn-tooltip:has(:focus-visible) > .autumn-tooltip__bubble:not([popover])",
            ".autumn-tooltip > .autumn-tooltip__bubble[popover]:popover-open",
        ] {
            assert!(
                lf_css().contains(&format!("\n{selector} {{\n    display: block;\n}}")),
                "`{selector}` must be a rule of its own"
            );
        }
        // Browsers without `:has()` show on any focus.
        let block =
            css_block("@supports not selector(:has(a)) {\n    .autumn-tooltip:focus-within");
        assert!(block.contains("display: block;"), "{block}");
        // A long link trigger stays in the text flow.
        assert!(rule_body(".autumn-tooltip").contains("display: inline;"));
    }

    /// WCAG 1.4.13 (hoverable): a transparent bridge covers the gap between
    /// the trigger and the text, so the pointer can move onto the text.
    #[test]
    fn tooltip_has_a_hover_bridge_over_the_gap() {
        let body = rule_body(".autumn-tooltip__bubble::before");
        assert!(body.contains("content: \"\";"), "{body}");
        assert!(body.contains("position: absolute;"), "{body}");
        assert!(
            body.contains("inset: calc(-1 * var(--autumn-tooltip-gap));"),
            "{body}"
        );
    }

    /// The dismiss helper sets `hidden`. An empty text never shows. Both win
    /// over each show rule.
    #[test]
    fn a_dismissed_or_empty_tooltip_does_not_show() {
        assert!(
            lf_css().contains(
                ".autumn-tooltip__bubble[hidden],\n.autumn-tooltip__bubble:empty {\n    \
                 display: none !important;"
            ),
            "missing the [hidden] / :empty rule"
        );
    }

    /// Anchor positioning applies only with `anchor-scope`. Without it, all
    /// tooltips on a page share one anchor name.
    #[test]
    fn tooltip_anchor_positioning_needs_anchor_scope() {
        let root = rule_body(".autumn-tooltip");
        assert!(root.contains("anchor-name: --autumn-tooltip;"), "{root}");
        assert!(root.contains("anchor-scope: --autumn-tooltip;"), "{root}");
        let block = css_block("@supports (anchor-scope: all) {");
        for declaration in [
            "position: fixed;",
            "position-anchor: --autumn-tooltip;",
            "position-try-fallbacks: flip-block;",
            // Start and end try the other side, then above and below, so
            // the text never covers its trigger in a narrow viewport.
            "position-try-fallbacks: flip-inline, --autumn-tooltip-above, --autumn-tooltip-below;",
            // `self-*` follows the tooltip's direction in an RTL subtree.
            "position-area: self-inline-start;",
            "position-area: self-inline-end;",
            // The text stays off its trigger when no block area fits.
            "align-self: unsafe end;",
            "align-self: unsafe start;",
        ] {
            assert!(
                block.contains(declaration),
                "missing `{declaration}`:{block}"
            );
        }
        for name in ["--autumn-tooltip-above", "--autumn-tooltip-below"] {
            assert!(
                lf_css().contains(&format!("@position-try {name} {{")),
                "missing @position-try {name}"
            );
        }
        for placement in ["top", "bottom", "start", "end"] {
            assert!(
                lf_css().contains(&format!(
                    ".autumn-tooltip--{placement} > .autumn-tooltip__bubble"
                )),
                "no rule for the {placement} placement"
            );
        }
    }

    /// On Chromium the runtime makes the text a `popover="hint"`. The CSS
    /// resets the UA popover box and places it at the implicit anchor.
    #[test]
    fn tooltip_popover_resets_the_ua_box() {
        let body = rule_body(".autumn-tooltip__bubble[popover]");
        for declaration in [
            "position: fixed;",
            "position-anchor: auto;",
            "inset: auto;",
            "overflow: visible;",
        ] {
            assert!(body.contains(declaration), "missing `{declaration}`:{body}");
        }
    }

    /// Forced colors paint a transparent border, so the text keeps an edge.
    #[test]
    fn tooltip_keeps_an_edge_in_forced_colors() {
        assert!(rule_body(".autumn-tooltip__bubble").contains("border: 1px solid transparent;"));
    }

    #[test]
    fn tooltip_sits_above_toasts_and_has_no_motion_when_reduced() {
        assert!(rule_body(".autumn-tooltip__bubble").contains("z-index: var(--z-tooltip);"));
        let block =
            css_block("@media (prefers-reduced-motion: reduce) {\n    .autumn-tooltip__bubble");
        assert!(block.contains("transition: none;"), "{block}");
    }

    #[test]
    fn tooltip_text_meets_aa_contrast_in_both_schemes() {
        for scheme in Scheme::BOTH {
            let ratio = contrast(
                &declared(".autumn-tooltip__bubble", "color", scheme),
                &declared(".autumn-tooltip__bubble", "background", scheme),
            );
            assert!(ratio >= 4.5, "{scheme:?}: {ratio:.2}:1");
        }
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

    // ── Issue #3406: the small display set ────────────────────────────────

    /// One style rule: the at-rules around it, its selector and its body.
    struct Rule {
        at: String,
        selector: String,
        body: String,
    }

    impl Rule {
        /// The `(property, value)` pairs of the body.
        fn declarations(&self) -> Vec<(&str, &str)> {
            self.body
                .split(';')
                .filter_map(|d| d.split_once(':'))
                .map(|(p, v)| (p.trim(), v.trim()))
                .filter(|(p, _)| !p.is_empty())
                .collect()
        }
    }

    /// Every style rule, with `@media` and `@supports` context. Comments
    /// are removed. `@keyframes` and `@starting-style` blocks are skipped.
    fn rules() -> Vec<Rule> {
        fn walk(css: &str, at: &str, out: &mut Vec<Rule>) {
            let mut rest = css;
            while let Some(open) = rest.find('{') {
                let head = rest[..open].trim();
                let mut depth = 0_i32;
                let mut close = rest.len();
                for (i, c) in rest[open..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                close = open + i;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let inner = &rest[open + 1..close];
                if head.starts_with("@media") || head.starts_with("@supports") {
                    walk(inner, &format!("{at} {head}"), out);
                } else if !head.starts_with('@') {
                    out.push(Rule {
                        at: at.trim().to_owned(),
                        selector: head.to_owned(),
                        body: inner.to_owned(),
                    });
                }
                rest = rest.get(close + 1..).unwrap_or("");
            }
        }
        let mut css = String::new();
        let mut rest = lf_css();
        while let Some(start) = rest.find("/*") {
            css.push_str(&rest[..start]);
            let end = rest[start..]
                .find("*/")
                .map_or(rest.len(), |e| start + e + 2);
            rest = rest[end..].to_owned();
        }
        css.push_str(&rest);
        let mut out = Vec::new();
        walk(&css, "", &mut out);
        out
    }

    /// The class prefixes the #3406 widgets own.
    const DISPLAY_SET: &[&str] = &[
        ".autumn-button",
        ".autumn-spinner",
        ".autumn-skeleton",
        ".autumn-progress",
        ".autumn-meter",
        ".autumn-separator",
        ".autumn-kbd",
        ".autumn-empty-state",
        ".autumn-avatar-group",
        ".autumn-auto-grow",
        ".autumn-indicator",
    ];

    fn display_set_rules() -> Vec<Rule> {
        rules()
            .into_iter()
            .filter(|r| DISPLAY_SET.iter().any(|p| r.selector.contains(p)))
            .collect()
    }

    /// Each class that the #3406 widgets emit has a rule.
    #[test]
    fn display_set_classes_are_backed_by_component_css() {
        let rules = rules();
        for class in [
            "autumn-button",
            "autumn-button--primary",
            "autumn-button--secondary",
            "autumn-button--ghost",
            "autumn-button--destructive",
            "autumn-button--link",
            "autumn-button--sm",
            "autumn-button--md",
            "autumn-button--lg",
            "autumn-button--icon",
            "autumn-button__icon",
            "autumn-button__label",
            "autumn-button__spinner",
            "autumn-button-group",
            "autumn-button-group--vertical",
            "autumn-spinner",
            "autumn-spinner--sm",
            "autumn-spinner--md",
            "autumn-spinner--lg",
            "autumn-spinner__icon",
            "autumn-indicator",
            "autumn-skeleton",
            "autumn-skeleton--line",
            "autumn-skeleton--block",
            "autumn-skeleton--circle",
            "autumn-skeleton__item",
            "autumn-progress",
            "autumn-progress-field",
            "autumn-progress-field__header",
            "autumn-progress-field__label",
            "autumn-progress-field__value",
            "autumn-meter",
            "autumn-separator",
            "autumn-separator--vertical",
            "autumn-kbd",
            "autumn-kbd-combo",
            "autumn-empty-state",
            "autumn-empty-state__icon",
            "autumn-empty-state__title",
            "autumn-empty-state__body",
            "autumn-empty-state__action",
            "autumn-avatar-group",
            "autumn-avatar-group__more",
            "autumn-auto-grow",
        ] {
            let needle = format!(".{class}");
            let backed = rules.iter().any(|r| {
                r.selector.match_indices(&needle).any(|(i, _)| {
                    !r.selector[i + needle.len()..]
                        .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                })
            });
            assert!(backed, "no rule for .{class}");
        }
    }

    /// The #3406 rules style only with tokens: no literal colors, and
    /// spacing, type and radius come from the token scales.
    ///
    /// Two exceptions: a `forced-colors` block must use system colors, and
    /// a `-1px` margin puts two borders on one line (it is not spacing).
    #[test]
    fn display_set_rules_style_only_with_tokens() {
        const LITERAL_COLORS: &[&str] = &[
            "#", "rgb(", "rgba(", "hsl(", "hsla(", "hwb(", "lab(", "lch(", "oklch(", "oklab(",
            "white", "black", "gray", "grey", "red", "green", "blue",
        ];
        const KEYWORDS: &[&str] = &[
            "0",
            "auto",
            "inherit",
            "none",
            "transparent",
            "currentColor",
        ];
        let mut failures = Vec::new();
        let rules = display_set_rules();
        assert!(
            rules.len() >= 40,
            "display set scan looks broken: {}",
            rules.len()
        );
        for rule in rules.iter().filter(|r| !r.at.contains("forced-colors")) {
            for (prop, value) in rule.declarations() {
                let color = matches!(
                    prop,
                    "color"
                        | "background"
                        | "background-color"
                        | "border-color"
                        | "outline-color"
                        | "fill"
                        | "stroke"
                        | "accent-color"
                        | "box-shadow"
                ) || prop.starts_with("border")
                    || prop.starts_with("outline");
                if color {
                    let lower = value.to_ascii_lowercase();
                    if LITERAL_COLORS.iter().any(|c| lower.contains(c)) {
                        failures.push(format!("{}: {prop}: {value}", rule.selector));
                    }
                }
                let scale = if prop.starts_with("padding")
                    || prop.starts_with("margin")
                    || prop.ends_with("gap")
                {
                    Some("var(--space-")
                } else if prop == "font-size" {
                    Some("var(--font-size-")
                } else if prop == "font-weight" {
                    Some("var(--font-weight-")
                } else if prop == "line-height" {
                    Some("var(--line-height-")
                } else if prop == "border-radius" {
                    Some("var(--radius")
                } else if prop == "font-family" {
                    Some("var(--font-")
                } else {
                    None
                };
                if let Some(token) = scale
                    && !value.contains(token)
                    && value != "-1px"
                    && !value.split_whitespace().all(|v| KEYWORDS.contains(&v))
                {
                    failures.push(format!("{}: {prop}: {value} (no {token}…)", rule.selector));
                }
            }
        }
        assert!(failures.is_empty(), "not token-based: {failures:#?}");
    }

    /// The spinner turns only with `prefers-reduced-motion: no-preference`.
    /// With `reduce`, it fades: an opacity change, not motion.
    #[test]
    fn spinner_turns_only_without_reduced_motion() {
        let rules = rules();
        let animation = |at: &str| {
            rules
                .iter()
                .filter(|r| r.selector == ".autumn-spinner__icon" && r.at.contains(at))
                .flat_map(Rule::declarations)
                .find(|(p, _)| *p == "animation")
                .map(|(_, v)| v.to_owned())
        };
        let turn = animation("prefers-reduced-motion: no-preference").expect("turn animation");
        assert!(turn.contains("autumn-spin"), "{turn}");
        let fade = animation("prefers-reduced-motion: reduce").expect("reduced animation");
        assert!(!fade.contains("autumn-spin"), "{fade}");
        let keyframes = lf_css();
        let at = keyframes
            .find("@keyframes autumn-pulse")
            .expect("pulse keyframes");
        let frames = &keyframes[at..at + keyframes[at..].find("\n}").unwrap()];
        assert!(
            frames.contains("opacity") && !frames.contains("transform"),
            "{frames}"
        );
        // No animation outside the motion queries.
        assert!(
            rules
                .iter()
                .filter(|r| r.selector.contains("autumn-spinner") && r.at.is_empty())
                .all(|r| !r.body.contains("animation")),
            "a spinner animation outside a prefers-reduced-motion query"
        );
    }

    /// The skeleton pulses only with `prefers-reduced-motion: no-preference`.
    #[test]
    fn skeleton_pulses_only_without_reduced_motion() {
        for rule in rules()
            .iter()
            .filter(|r| r.selector.contains("autumn-skeleton"))
        {
            if rule.body.contains("animation") {
                assert!(
                    rule.at.contains("prefers-reduced-motion: no-preference"),
                    "{} animates in `{}`",
                    rule.selector,
                    rule.at
                );
            }
        }
        assert!(
            rules()
                .iter()
                .any(|r| r.selector.contains("autumn-skeleton__item")
                    && r.body.contains("animation")),
            "the skeleton has no pulse"
        );
    }

    /// A busy button hides its icon and label with `opacity` only. They
    /// stay in the accessibility tree, so the button keeps its name.
    #[test]
    fn a_busy_button_keeps_its_label_in_the_accessibility_tree() {
        let mut hides = 0;
        for rule in display_set_rules() {
            let busy =
                rule.selector.contains("aria-busy") || rule.selector.contains("htmx-request");
            if !busy || !rule.selector.contains("autumn-button") {
                continue;
            }
            assert!(
                !rule.body.contains("visibility: hidden") && !rule.body.contains("display: none"),
                "{} removes content from the accessibility tree",
                rule.selector
            );
            if rule.body.contains("opacity: 0;") {
                hides += 1;
                assert!(
                    rule.selector.contains("autumn-button__spinner ~ *"),
                    "{}",
                    rule.selector
                );
            }
        }
        assert_eq!(hides, 1, "one rule hides the content of a busy button");
    }

    /// Class-level specificity: classes, attributes and pseudo-classes.
    /// The selectors it reads have no ids and no `:not()`.
    fn class_specificity(selector: &str) -> usize {
        selector.matches('.').count()
            + selector.matches('[').count()
            + selector.matches(':').count()
            - 2 * selector.matches("::").count()
    }

    /// Codex review (#3406): `.autumn-spinner__icon` comes later in the
    /// file and sets `display`. The rule that hides the button spinner must
    /// beat it, and each busy rule must beat the hide rule. Then the order
    /// of the rules does not matter.
    #[test]
    fn button_spinner_is_hidden_unless_the_button_is_busy() {
        const HIDE: &str = ".autumn-button > .autumn-button__spinner";
        let rules = rules();
        let sets_display = |r: &&Rule| r.declarations().iter().any(|(p, _)| *p == "display");
        let hide = rules
            .iter()
            .find(|r| r.selector == HIDE)
            .expect("the hide rule");
        assert!(hide.body.contains("display: none;"), "{}", hide.body);
        let floor = class_specificity(HIDE);
        let icon = rules
            .iter()
            .filter(sets_display)
            .filter(|r| r.selector == ".autumn-spinner__icon")
            .count();
        assert!(icon > 0, "no `.autumn-spinner__icon` display rule");
        assert!(class_specificity(".autumn-spinner__icon") < floor);
        let show = rules
            .iter()
            .filter(sets_display)
            .find(|r| {
                r.selector
                    .contains(r#".autumn-button[aria-busy="true"] > .autumn-button__spinner"#)
            })
            .expect("the busy rule");
        assert!(show.body.contains("display: block;"), "{}", show.body);
        let selectors: Vec<&str> = show.selector.split(',').map(str::trim).collect();
        assert_eq!(selectors.len(), 2, "{selectors:?}");
        for selector in selectors {
            assert!(
                class_specificity(selector) > floor,
                "{selector} does not beat {HIDE}"
            );
        }
    }

    /// An indicator is hidden until htmx marks it, or an element around
    /// it, with `htmx-request`.
    #[test]
    fn indicator_is_hidden_until_an_htmx_request() {
        let rule = rules()
            .into_iter()
            .find(|r| {
                r.selector
                    .contains(".autumn-indicator:not(.autumn-spinner):not(")
            })
            .expect("indicator rule");
        assert!(rule.at.is_empty(), "{}", rule.at);
        assert!(rule.body.contains("display: none;"), "{}", rule.body);
        // A spinner hides its content only. Its live region stays in the
        // tree, so the label that shows is read.
        assert!(
            rule.selector.contains(
                ".autumn-spinner.autumn-indicator:not(.htmx-request, .htmx-request *) > *"
            ),
            "{}",
            rule.selector
        );
    }

    /// `field-sizing: content` grows the textarea. A minimum keeps it from
    /// a one-line height, and a maximum keeps it on the screen.
    #[test]
    fn auto_grow_uses_field_sizing_content() {
        let body = rule_body(".autumn-auto-grow");
        for declaration in [
            "inline-size: 100%;",
            "field-sizing: content;",
            "min-block-size: 3lh;",
            "max-block-size: 50vh;",
        ] {
            assert!(body.contains(declaration), "missing `{declaration}`:{body}");
        }
    }

    /// WCAG 1.4.3: button, kbd, `+N` item and empty-state text in both
    /// schemes.
    /// A transparent background reads the surface.
    #[test]
    fn display_set_text_meets_aa_contrast() {
        let mut failures = Vec::new();
        for scheme in Scheme::BOTH {
            for (fg, bg, prop) in [
                (
                    ".autumn-button--primary",
                    ".autumn-button--primary",
                    "background-color",
                ),
                (
                    ".autumn-button--primary",
                    ".autumn-button--primary:hover",
                    "background-color",
                ),
                (
                    ".autumn-button--secondary",
                    ".autumn-button--secondary",
                    "background-color",
                ),
                (
                    ".autumn-button--secondary",
                    ".autumn-button--secondary:hover",
                    "background-color",
                ),
                (".autumn-button--ghost", "", ""),
                (
                    ".autumn-button--ghost",
                    ".autumn-button--ghost:hover",
                    "background-color",
                ),
                (
                    ".autumn-button--destructive",
                    ".autumn-button--destructive",
                    "background-color",
                ),
                (
                    ".autumn-button--destructive",
                    ".autumn-button--destructive:hover",
                    "background-color",
                ),
                (".autumn-button--link", "", ""),
                (".autumn-kbd", ".autumn-kbd", "background-color"),
                (
                    ".autumn-avatar-group__more",
                    ".autumn-avatar-group__more",
                    "background-color",
                ),
                (".autumn-empty-state__body", "", ""),
                (".autumn-progress-field__value", "", ""),
            ] {
                let fg_color = declared(fg, "color", scheme);
                let bg_color = if bg.is_empty() {
                    resolve("var(--surface)", scheme)
                } else {
                    declared(bg, prop, scheme)
                };
                let ratio = contrast(&fg_color, &bg_color);
                if ratio < 4.5 {
                    failures.push(format!("{scheme:?} {fg} on {bg}: {ratio:.2}:1"));
                }
            }
        }
        assert!(failures.is_empty(), "below 4.5:1 — {failures:?}");
    }

    /// WCAG 1.4.11: each meter fill stands out from the track (3:1), and the
    /// spinner ring from the surface, in both schemes.
    #[test]
    fn meter_fills_and_spinner_meet_non_text_contrast() {
        let mut failures = Vec::new();
        for scheme in Scheme::BOTH {
            let track = declared(".autumn-meter::-webkit-meter-bar", "background", scheme);
            for fill in [
                ".autumn-meter::-webkit-meter-optimum-value",
                ".autumn-meter::-webkit-meter-suboptimum-value",
                ".autumn-meter::-webkit-meter-even-less-good-value",
                ".autumn-meter:-moz-meter-optimum::-moz-meter-bar",
                ".autumn-meter:-moz-meter-sub-optimum::-moz-meter-bar",
                ".autumn-meter:-moz-meter-sub-sub-optimum::-moz-meter-bar",
            ] {
                let ratio = contrast(&declared(fill, "background", scheme), &track);
                if ratio < 3.0 {
                    failures.push(format!("{scheme:?} {fill}: {ratio:.2}:1"));
                }
            }
            let ring = declared(".autumn-spinner", "color", scheme);
            let ratio = contrast(&ring, &resolve("var(--surface)", scheme));
            if ratio < 3.0 {
                failures.push(format!("{scheme:?} spinner: {ratio:.2}:1"));
            }
        }
        assert!(failures.is_empty(), "below 3:1 — {failures:?}");
    }

    /// A skeleton paints only a background. Forced colors removes it, so a
    /// transparent border keeps an edge.
    #[test]
    fn skeleton_keeps_an_edge_in_forced_colors() {
        assert!(
            rule_body(".autumn-skeleton__item").contains("border: 1px solid transparent;"),
            "{}",
            rule_body(".autumn-skeleton__item")
        );
    }

    /// `hsl(h, s%, l%)` as `#rrggbb`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a channel is rounded and in 0..=255"
    )]
    fn hsl_to_hex(hsl: &str) -> String {
        let inner = hsl
            .trim()
            .strip_prefix("hsl(")
            .and_then(|v| v.strip_suffix(')'))
            .unwrap_or_else(|| panic!("not hsl(): {hsl}"));
        let parts: Vec<f64> = inner
            .split(',')
            .map(|p| p.trim().trim_end_matches('%').parse().expect("number"))
            .collect();
        let (h, s, l) = (parts[0] / 360.0, parts[1] / 100.0, parts[2] / 100.0);
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        let channel = |t: f64| {
            let t = t.rem_euclid(1.0);
            let c = if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            };
            format!("{:02x}", (c * 255.0).round() as u8)
        };
        format!(
            "#{}{}{}",
            channel(h + 1.0 / 3.0),
            channel(h),
            channel(h - 1.0 / 3.0)
        )
    }

    /// axe (#3406 avatar group story): white initials on each palette
    /// color, and on the fallback, meet 4.5:1.
    #[test]
    fn avatar_initials_meet_aa_contrast_on_every_palette_color() {
        assert_eq!(hsl_to_hex("hsl(0, 100%, 50%)"), "#ff0000");
        let mut failures = Vec::new();
        for class in (0..12)
            .map(|i| format!(".autumn-avatar--c{i}"))
            .chain([".autumn-avatar--initials".to_owned()])
        {
            let body = rule_body(&class);
            let value = body
                .lines()
                .filter_map(|l| l.trim().strip_prefix("background:"))
                .chain(
                    body.split_once("background:")
                        .map(|(_, v)| v.split(';').next().unwrap_or("")),
                )
                .next()
                .unwrap_or_else(|| panic!("{class} has no background"));
            let bg = hsl_to_hex(value.trim().trim_end_matches(';'));
            let ratio = contrast("#ffffff", &bg);
            if ratio < 4.5 {
                failures.push(format!("{class} {bg}: {ratio:.2}:1"));
            }
        }
        assert!(failures.is_empty(), "below 4.5:1 — {failures:?}");
    }
}

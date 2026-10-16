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
}

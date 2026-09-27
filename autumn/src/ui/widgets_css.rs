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
}

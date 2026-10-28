//! Design tokens shared by framework-owned HTML surfaces.
//!
//! Embed [`TOKENS_CSS`] inside a `<style>` tag; subsequent rules can
//! reference the custom properties via `var(--bg)`, `var(--primary)`, etc.
//!
//! Each color token is `light-dark(<light>, <dark>)` (#3396). The OS
//! selects the scheme. `data-theme="light|dark"` overrides it on any element.
//! `data-density="compact|comfortable|spacious"` scales the `--space-*`
//! tokens. The set also has type, radius, z-index and breakpoint scales. See
//! `docs/guide/widget-styling.md`.
//!
//! # Example
//!
//! ```rust
//! use autumn_web::ui::tokens::TOKENS_CSS;
//!
//! let stylesheet = format!("{TOKENS_CSS}\nbody {{ background: var(--bg); }}");
//! assert!(stylesheet.contains("--bg"));
//! ```

/// The design tokens: colors (light and dark), shadow, and the space, type,
/// radius, z-index and breakpoint scales, plus the `data-theme` and
/// `data-density` rules.
///
/// Emitted inside a `<style>` block so downstream rules can reference the
/// variables. Sourced from `tokens.css` so `include_str!` can share the
/// same literal with places that need `concat!`-time composition (e.g. the
/// error-pages fallback stylesheet).
pub const TOKENS_CSS: &str = include_str!("tokens.css");

/// Flash-message CSS consuming the shared tokens.
///
/// Paired with `FlashLevel::as_str` (if the `flash` feature is enabled) (`"success"` /
/// `"error"` / `"warning"` / `"info"`). Render a flash like:
///
/// ```html
/// <div class=\"flash flash-success\">Saved.</div>
/// ```
pub const FLASH_CSS: &str = "\
.flash {
    padding: 0.75rem 1rem;
    border-radius: 0.375rem;
    margin-bottom: 1rem;
    font-size: 0.875rem;
}
.flash-success { background: var(--success-light); color: var(--success-text); border: 1px solid var(--success); }
.flash-error { background: var(--danger-light); color: var(--danger-hover); border: 1px solid var(--danger); }
.flash-warning { background: var(--warning-light); color: var(--warning-text); border: 1px solid var(--warning); }
.flash-info { background: var(--primary-light); color: var(--primary); border: 1px solid var(--primary); }
";

/// Test helpers that read [`TOKENS_CSS`] the way a browser does.
#[cfg(test)]
pub(crate) mod test_support {
    use super::TOKENS_CSS;

    /// A color scheme. Selects one side of `light-dark()`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Scheme {
        Light,
        Dark,
    }

    impl Scheme {
        pub const BOTH: [Self; 2] = [Self::Light, Self::Dark];
    }

    /// The main `:root` block: the text before the first nested rule.
    pub fn main_block() -> &'static str {
        let start = TOKENS_CSS.find(":root {").expect(":root block");
        let body = &TOKENS_CSS[start..];
        &body[..body.find('}').expect("closed :root block")]
    }

    /// The body of the `@supports not (color: light-dark(...))` block.
    pub fn fallback_block() -> &'static str {
        let start = TOKENS_CSS
            .find("@supports not (color: light-dark(")
            .expect("@supports fallback block");
        let body = &TOKENS_CSS[start..];
        let body = &body[body.find(":root {").expect("fallback :root")..];
        &body[..body.find('}').expect("closed fallback block")]
    }

    /// `(name, value)` for each custom property in `block`.
    pub fn declarations(block: &str) -> Vec<(&str, &str)> {
        block
            .split(';')
            .filter_map(|decl| {
                // Drop a comment before the declaration.
                let decl = decl.rsplit_once("*/").map_or(decl, |(_, rest)| rest);
                let decl = &decl[decl.find("--")?..];
                let (name, value) = decl.split_once(':')?;
                Some((name.trim(), value.trim()))
            })
            .collect()
    }

    /// The `(light, dark)` pair of a `light-dark(a, b)` value.
    pub fn light_dark(value: &str) -> Option<(&str, &str)> {
        let args = value.strip_prefix("light-dark(")?.strip_suffix(')')?;
        let (light, dark) = args.split_once(',')?;
        Some((light.trim(), dark.trim()))
    }

    /// The `#rrggbb` value of color token `name` in `scheme`, or `None`.
    pub fn color(name: &str, scheme: Scheme) -> Option<String> {
        let value = declarations(main_block())
            .into_iter()
            .find(|(n, _)| *n == name)?
            .1;
        let (light, dark) = light_dark(value)?;
        Some(
            match scheme {
                Scheme::Light => light,
                Scheme::Dark => dark,
            }
            .to_owned(),
        )
    }

    /// `#rrggbb` -> WCAG relative luminance.
    pub fn luminance(hex: &str) -> f64 {
        let channel = |i: usize| {
            let c = f64::from(u8::from_str_radix(&hex[1 + i * 2..3 + i * 2], 16).unwrap()) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.0722_f64.mul_add(
            channel(2),
            0.7152_f64.mul_add(channel(1), 0.2126 * channel(0)),
        )
    }

    /// WCAG contrast ratio of two `#rrggbb` colors.
    pub fn contrast(a: &str, b: &str) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// True for `#rrggbb`.
    pub fn is_hex6(value: &str) -> bool {
        value.len() == 7
            && value.starts_with('#')
            && value[1..].chars().all(|c| c.is_ascii_hexdigit())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        Scheme, contrast, declarations, fallback_block, is_hex6, light_dark, main_block,
    };
    use super::*;

    /// Every color token. Each must have a light and a dark value.
    const COLOR_TOKENS: &[&str] = &[
        "--bg",
        "--surface",
        "--surface-muted",
        "--text",
        "--text-muted",
        "--border",
        "--primary",
        "--primary-hover",
        "--primary-light",
        "--on-primary",
        "--danger",
        "--danger-hover",
        "--danger-light",
        "--on-danger",
        "--success",
        "--success-light",
        "--success-text",
        "--warning",
        "--warning-light",
        "--warning-text",
        "--info",
        "--info-light",
    ];

    #[test]
    fn tokens_defines_expected_vars() {
        for v in [
            "--bg",
            "--surface",
            "--text",
            "--text-muted",
            "--border",
            "--primary",
            "--danger",
            "--success",
            "--warning",
            "--radius",
            "--shadow",
            "--font-family",
        ] {
            assert!(TOKENS_CSS.contains(v), "TOKENS_CSS is missing {v}");
        }
    }

    /// Issue #3396 acceptance: each color token has a light and a dark value.
    #[test]
    fn every_color_token_defines_a_light_and_a_dark_value() {
        let decls = declarations(main_block());
        for token in COLOR_TOKENS {
            let value = decls
                .iter()
                .find(|(name, _)| name == token)
                .unwrap_or_else(|| panic!("{token} is not defined"))
                .1;
            let (light, dark) =
                light_dark(value).unwrap_or_else(|| panic!("{token} is not light-dark(): {value}"));
            assert!(is_hex6(light) && is_hex6(dark), "{token}: {value}");
            assert_ne!(light, dark, "{token} has the same value in both schemes");
        }
        // A color token added later must also use light-dark().
        for (name, value) in &decls {
            let plain_color = ["#", "rgb", "hsl", "oklch"]
                .iter()
                .any(|prefix| value.starts_with(prefix));
            assert!(!plain_color, "{name} is a plain color: {value}");
            if value.starts_with("light-dark(") {
                assert!(COLOR_TOKENS.contains(name), "add {name} to COLOR_TOKENS");
            }
        }
    }

    /// A browser without `light-dark()` gets the light values.
    #[test]
    fn fallback_block_repeats_every_light_value() {
        let fallback = declarations(fallback_block());
        // Every color token, plus --shadow (its colors use light-dark()).
        assert_eq!(fallback.len(), COLOR_TOKENS.len() + 1, "{fallback:?}");
        assert!(
            fallback
                .iter()
                .any(|(name, value)| *name == "--shadow" && !value.contains("light-dark"))
        );
        for token in COLOR_TOKENS {
            let light = test_support::color(token, Scheme::Light).unwrap();
            assert!(
                fallback.contains(&(token, light.as_str())),
                "fallback must set {token}: {light}"
            );
        }
    }

    /// Issue #3396 acceptance: the token pairs pass WCAG AA in both schemes.
    /// 4.5:1 for text; 3:1 for a focus ring or a mark.
    #[test]
    fn token_pairs_meet_wcag_aa_in_both_schemes() {
        const TEXT: f64 = 4.5;
        const UI: f64 = 3.0;
        let pairs = [
            ("--text", "--bg", TEXT),
            ("--text", "--surface", TEXT),
            ("--text", "--surface-muted", TEXT),
            ("--text", "--primary-light", TEXT),
            ("--text-muted", "--bg", TEXT),
            ("--text-muted", "--surface", TEXT),
            ("--text-muted", "--surface-muted", TEXT),
            ("--primary", "--surface", TEXT),
            ("--primary", "--bg", TEXT),
            ("--primary", "--primary-light", TEXT),
            ("--on-primary", "--primary", TEXT),
            ("--on-primary", "--primary-hover", TEXT),
            ("--danger", "--surface", TEXT),
            ("--danger", "--bg", TEXT),
            ("--danger-hover", "--danger-light", TEXT),
            ("--on-danger", "--danger", TEXT),
            ("--on-danger", "--danger-hover", TEXT),
            ("--success-text", "--success-light", TEXT),
            ("--success-text", "--surface", TEXT),
            ("--warning-text", "--warning-light", TEXT),
            ("--warning-text", "--surface", TEXT),
            ("--info", "--info-light", TEXT),
            ("--info", "--surface", TEXT),
            ("--success", "--surface", UI),
            ("--warning", "--surface", UI),
        ];
        let mut failures = Vec::new();
        for scheme in Scheme::BOTH {
            for (fg, bg, min) in pairs {
                let ratio = contrast(
                    &test_support::color(fg, scheme).unwrap(),
                    &test_support::color(bg, scheme).unwrap(),
                );
                if ratio < min {
                    failures.push(format!("{scheme:?} {fg} on {bg}: {ratio:.2}:1"));
                }
            }
        }
        assert!(failures.is_empty(), "below WCAG AA — {failures:?}");
    }

    /// The OS selects the scheme. `data-theme` overrides it on any element.
    #[test]
    fn color_scheme_follows_the_os_and_data_theme() {
        assert!(main_block().contains("color-scheme: light dark;"));
        for (selector, scheme) in [
            ("[data-theme=\"light\"]", "light"),
            ("[data-theme=\"dark\"]", "dark"),
        ] {
            let rule = format!("{selector} {{ color-scheme: {scheme}; }}");
            assert!(TOKENS_CSS.contains(&rule), "missing `{rule}`");
        }
    }

    #[test]
    fn scales_are_defined() {
        for token in [
            "--space-1",
            "--space-2",
            "--space-3",
            "--space-4",
            "--space-5",
            "--space-6",
            "--space-8",
            "--font-size-xs",
            "--font-size-sm",
            "--font-size-md",
            "--font-size-lg",
            "--font-size-xl",
            "--font-size-2xl",
            "--line-height-tight",
            "--line-height-normal",
            "--font-weight-normal",
            "--font-weight-medium",
            "--font-weight-semibold",
            "--font-weight-bold",
            "--font-mono",
            "--radius-sm",
            "--radius-md",
            "--radius-lg",
            "--radius-full",
            "--z-dropdown",
            "--z-sticky",
            "--z-banner",
            "--z-toast",
            "--breakpoint-sm",
            "--breakpoint-md",
            "--breakpoint-lg",
            "--breakpoint-xl",
        ] {
            assert!(
                TOKENS_CSS.contains(&format!("{token}:")),
                "TOKENS_CSS is missing {token}"
            );
        }
    }

    /// Density scales the space tokens. The space scale is declared on each
    /// `[data-density]` element too, so a subtree can set its own density.
    #[test]
    fn density_scales_the_space_tokens() {
        assert!(TOKENS_CSS.contains(":root,\n[data-density] {"));
        let space = TOKENS_CSS
            .lines()
            .filter(|l| l.trim_start().starts_with("--space-"))
            .collect::<Vec<_>>();
        assert!(space.len() >= 6, "{space:?}");
        for line in space {
            assert!(line.contains("var(--density-scale)"), "{line}");
        }
        for (density, scale) in [
            ("compact", "0.75"),
            ("comfortable", "1"),
            ("spacious", "1.25"),
        ] {
            let rule = format!("[data-density=\"{density}\"] {{ --density-scale: {scale}; }}");
            assert!(TOKENS_CSS.contains(&rule), "missing `{rule}`");
        }
    }

    /// Flash text on its tint passes WCAG AA in both schemes (#3396).
    #[test]
    fn flash_text_meets_aa_in_both_schemes() {
        for level in ["success", "error", "warning", "info"] {
            let rule = FLASH_CSS
                .lines()
                .find(|l| l.starts_with(&format!(".flash-{level} ")))
                .unwrap_or_else(|| panic!("no .flash-{level} rule"));
            let token = |prop: &str| {
                let at = rule.find(&format!("{prop}: var(")).unwrap() + prop.len() + 6;
                rule[at..at + rule[at..].find(')').unwrap()].to_owned()
            };
            for scheme in Scheme::BOTH {
                let fg = test_support::color(&token("color"), scheme).unwrap();
                let bg = test_support::color(&token("background"), scheme).unwrap();
                let ratio = contrast(&fg, &bg);
                assert!(ratio >= 4.5, "{scheme:?} .flash-{level}: {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn flash_uses_shared_tokens() {
        for v in [
            "var(--success)",
            "var(--danger)",
            "var(--warning)",
            "var(--primary)",
        ] {
            assert!(FLASH_CSS.contains(v), "FLASH_CSS should reference {v}");
        }
    }
}

//! Coverage invariant for the shipped widget stylesheet (issue #1215).
//!
//! Every `autumn-*` class a framework widget emits must have a backing rule
//! in `autumn_web::ui::WIDGETS_COMPONENT_CSS` — the single framework-owned
//! stylesheet — so a new unbacked widget class fails the build instead of
//! shipping silently unstyled.
//!
//! Run with `cargo test --test integration_tests widget_css_coverage`.

#![cfg(feature = "maud")]

use std::path::Path;

/// Widget source files that emit `autumn-*` classes meant to be
/// styled by the shared framework stylesheet. Deliberately excludes:
/// - `src/error_pages/dev_badge.rs` — ships its own inline `<style>`;
///   `autumn-dev-*` classes are self-styled, not part of the shared sheet.
/// - `src/flash.rs` — uses the `.flash` / `.flash-<level>` convention, not
///   `autumn-*`, and is served as its own stylesheet (`FLASH_CSS`).
const WIDGET_SOURCES: &[&str] = &[
    "src/form.rs",
    "src/widgets.rs",
    "src/wizard.rs",
    "src/ui/pagination.rs",
    "src/storage/form_helper.rs",
    "src/job_tracking.rs",
];

/// Class-name prefixes the shared stylesheet backs. Since #3396 every widget
/// class has the `autumn-` prefix.
const CLASS_PREFIXES: &[&str] = &["autumn-"];

/// Extracts every `autumn-*` token emitted as a CSS class by the
/// production code in `source` (as opposed to a `data-*` attribute name,
/// doc-comment prose, or a test-only fixture string).
fn emitted_classes(source: &str) -> Vec<String> {
    // Every file here ends in a single `#[cfg(test)] mod tests { ... }` block
    // whose fixtures assert against literal ids (e.g. `"autumn-nav-menu-1"`)
    // that aren't CSS classes. Every real class is also exercised by the
    // render code above it, so stopping at the test module loses no coverage
    // while dropping that noise.
    let production_lines = source
        .lines()
        .take_while(|line| line.trim() != "mod tests {");

    let mut classes = Vec::new();
    for line in production_lines {
        // Strip `//`/`///` comments so prose mentions of the JS runtime
        // asset (`autumn-widgets.js`) aren't mistaken for a CSS class.
        let code = line.split("//").next().unwrap_or("");
        let bytes = code.as_bytes();
        for prefix in CLASS_PREFIXES {
            for (start, _) in code.match_indices(prefix) {
                // Skip `data-autumn-*` attribute names — not a CSS class.
                if start > 0 && bytes[start - 1] == b'-' {
                    continue;
                }
                let end = code[start..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .map_or(code.len(), |rel_end| start + rel_end);
                let token = code[start..end].trim_end_matches('-');
                if !token.is_empty() {
                    classes.push(token.to_string());
                }
            }
        }
    }
    classes
}

#[test]
fn every_emitted_autumn_class_has_backing_css() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut missing = Vec::new();

    for path in WIDGET_SOURCES {
        let source = std::fs::read_to_string(manifest_dir.join(path))
            .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        for class in emitted_classes(&source) {
            let selector = format!(".{class}");
            if !autumn_web::ui::WIDGETS_COMPONENT_CSS.contains(&selector) {
                missing.push(format!(
                    "{class} (emitted by {path}, expected `{selector}`)"
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "the following autumn-* classes are emitted but have no backing rule in \
         autumn_web::ui::WIDGETS_COMPONENT_CSS:\n{missing:#?}\n\
         Add a `.{{class}}` rule to autumn/src/ui/widgets.css."
    );
}

#[test]
fn widgets_css_is_self_contained_and_token_themeable() {
    // One <link> must style widgets with no Tailwind build: the bundle
    // includes the design tokens, not just component rules that assume the
    // app already defined `--primary` etc. elsewhere.
    assert!(
        autumn_web::ui::WIDGETS_CSS.contains(":root"),
        "WIDGETS_CSS must include the token :root block so it works standalone, without Tailwind"
    );
    assert!(
        autumn_web::ui::WIDGETS_CSS.contains(autumn_web::ui::tokens::TOKENS_CSS),
        "WIDGETS_CSS should be built from the shared TOKENS_CSS, not a private copy"
    );

    for token_var in [
        "var(--primary",
        "var(--border",
        "var(--radius",
        "var(--text",
    ] {
        assert!(
            autumn_web::ui::WIDGETS_COMPONENT_CSS.contains(token_var),
            "expected widget component CSS to reference {token_var} so apps re-theme by \
             overriding tokens, not by forking the component CSS"
        );
    }
}

#[test]
fn widgets_css_path_and_selectors_are_stable() {
    assert_eq!(
        autumn_web::ui::WIDGETS_CSS_PATH,
        "/static/css/autumn-widgets.css",
        "WIDGETS_CSS_PATH is a public contract apps link from their layout"
    );

    for selector in [
        ".autumn-field",
        ".autumn-submit",
        ".autumn-search",
        ".autumn-autocomplete",
        ".autumn-nav",
        ".autumn-modal",
        ".autumn-tabs",
        ".autumn-pager",
        ".autumn-breadcrumb",
        ".autumn-hero",
        ".autumn-property-list",
        ".autumn-direct-upload",
        ".autumn-job-status",
        ".autumn-wizard-progress",
        ".autumn-wizard-step",
        ".autumn-alert",
        ".autumn-badge",
        ".autumn-theme-toggle",
        ".autumn-accordion",
        ".autumn-collapsible",
    ] {
        assert!(
            autumn_web::ui::WIDGETS_COMPONENT_CSS.contains(selector),
            "missing selector {selector} in WIDGETS_COMPONENT_CSS"
        );
    }
}

/// Issue #3396: `alert`, `badge` and `wizard-*` got the `autumn-` prefix.
/// Neither the widgets nor the stylesheet may use the old names.
#[test]
fn legacy_unprefixed_classes_are_gone() {
    fn starts_legacy(rest: &str) -> bool {
        ["alert", "badge", "wizard-"].iter().any(|old| {
            rest.strip_prefix(old).is_some_and(|tail| {
                old.ends_with('-')
                    || tail.is_empty()
                    || !tail.starts_with(|c: char| c.is_ascii_alphanumeric())
            })
        })
    }

    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
    let legacy_selectors: Vec<&str> = css
        .match_indices('.')
        .filter(|(i, _)| starts_legacy(&css[i + 1..]))
        .map(|(i, _)| css[i..].lines().next().unwrap_or_default())
        .collect();
    assert!(
        legacy_selectors.is_empty(),
        "unprefixed selectors in widgets.css: {legacy_selectors:?}"
    );

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in WIDGET_SOURCES {
        let source = std::fs::read_to_string(manifest_dir.join(path)).unwrap();
        for line in source.lines().take_while(|l| l.trim() != "mod tests {") {
            let code = line.split("//").next().unwrap_or("");
            for (i, _) in code.match_indices("class=\"") {
                let value = &code[i + 7..];
                let value = &value[..value.find('"').unwrap_or(value.len())];
                for class in value.split_whitespace() {
                    assert!(!starts_legacy(class), "{path} emits `{class}`: {line}");
                }
            }
        }
    }

    let alert = autumn_web::widgets::alert(
        autumn_web::widgets::AlertVariant::Error,
        maud::html! { "x" },
    )
    .into_string();
    assert!(
        alert.contains(r#"class="autumn-alert autumn-alert--error""#),
        "{alert}"
    );
    let badge =
        autumn_web::widgets::badge("x", autumn_web::widgets::BadgeVariant::Danger).into_string();
    assert!(
        badge.contains(r#"class="autumn-badge autumn-badge--danger""#),
        "{badge}"
    );
}

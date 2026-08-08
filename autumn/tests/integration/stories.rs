//! CI anti-rot harness for the widget story gallery (issue #1526).
//!
//! Renders every built-in story (no panic, non-empty, balanced HTML, unique
//! slugs), enforces the story coverage gate against `src/widgets.rs`, and
//! pins the `story!` macro's source-fidelity and zero-arg purity contracts.
//!
//! Run with `cargo test -p autumn-web --test integration_tests stories`.

#![cfg(feature = "maud")]

use autumn_web::config::{AutumnConfig, MockEnv};
use autumn_web::stories::{self, Story, StoryGallery, StoryRegistry, story};

// ── Strict balanced-HTML checker ─────────────────────────────────────────
//
// `autumn_web`'s internal `test_html` parser is crate-private (and lenient:
// it auto-closes). This local checker is strict: every non-void open tag must
// be closed by a matching close tag, in order.

/// Elements that never take a closing tag.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is raw text (no nested tags are parsed).
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "textarea", "title"];

/// Panics with a descriptive message when `html` is not well-formed
/// (mismatched, unclosed, or stray-closed tags).
fn assert_balanced_html(html: &str, context: &str) {
    let bytes = html.as_bytes();
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if html[i..].starts_with("<!--") {
            let end = html[i + 4..]
                .find("-->")
                .unwrap_or_else(|| panic!("unterminated comment in {context}:\n{html}"));
            i += 4 + end + 3;
            continue;
        }
        if html[i..].starts_with("<!") {
            // Doctype or other declaration.
            let end = html[i..]
                .find('>')
                .unwrap_or_else(|| panic!("unterminated declaration in {context}:\n{html}"));
            i += end + 1;
            continue;
        }

        let closing = html[i..].starts_with("</");
        let name_start = i + if closing { 2 } else { 1 };
        let name_len = html[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(html.len() - name_start);
        let name = html[name_start..name_start + name_len].to_ascii_lowercase();
        assert!(
            !name.is_empty(),
            "malformed tag at byte {i} in {context}:\n{html}"
        );

        // Find the true end of the tag, skipping `>` inside quoted attributes.
        let mut j = name_start + name_len;
        let mut quote: Option<u8> = None;
        while j < bytes.len() {
            match (quote, bytes[j]) {
                (Some(q), c) if c == q => quote = None,
                (None, b'"') => quote = Some(b'"'),
                (None, b'\'') => quote = Some(b'\''),
                (None, b'>') => break,
                _ => {}
            }
            j += 1;
        }
        assert!(
            j < bytes.len(),
            "unterminated tag <{name} in {context}:\n{html}"
        );
        let self_closing = j > 0 && bytes[j - 1] == b'/';

        if closing {
            let open = stack.pop().unwrap_or_else(|| {
                panic!("closing </{name}> with no open tag in {context}:\n{html}")
            });
            assert_eq!(
                open, name,
                "mismatched close tag in {context}: expected </{open}>, found </{name}>:\n{html}"
            );
            i = j + 1;
            continue;
        }

        if !self_closing && !VOID_ELEMENTS.contains(&name.as_str()) {
            if RAW_TEXT_ELEMENTS.contains(&name.as_str()) {
                // Skip raw content up to the matching close tag.
                let close = format!("</{name}");
                let rest_start = j + 1;
                let end = html[rest_start..]
                    .to_ascii_lowercase()
                    .find(&close)
                    .unwrap_or_else(|| {
                        panic!("unclosed raw-text element <{name}> in {context}:\n{html}")
                    });
                let close_gt = html[rest_start + end..]
                    .find('>')
                    .unwrap_or_else(|| panic!("unterminated </{name}> in {context}:\n{html}"));
                i = rest_start + end + close_gt + 1;
                continue;
            }
            stack.push(name);
        }
        i = j + 1;
    }

    assert!(
        stack.is_empty(),
        "unclosed tags {stack:?} in {context}:\n{html}"
    );
}

#[test]
fn balanced_html_checker_rejects_malformed_markup() {
    // Sanity-check the harness itself so a broken checker can't green-light
    // rotten widget markup.
    assert_balanced_html(
        r#"<div class="a > b"><p>ok<br></p></div><input value="x">"#,
        "self-test",
    );
    for bad in ["<div><p></div>", "<div>", "</div>", "<span><b></span></b>"] {
        let result = std::panic::catch_unwind(|| assert_balanced_html(bad, "self-test"));
        assert!(result.is_err(), "checker should reject {bad}");
    }
}

// ── T7 (AC3/AC8): every builtin story renders, panic-free and non-empty ──

#[test]
fn every_builtin_story_renders_without_panic_and_nonempty() {
    let registry = stories::builtin();
    assert!(
        !registry.stories().is_empty(),
        "stories::builtin() must ship at least one story per gallery-visible widget"
    );
    for story in registry.stories() {
        let markup = story.render().unwrap_or_else(|err| {
            panic!("builtin story `{}` failed to render: {err}", story.slug())
        });
        assert!(
            !markup.into_string().trim().is_empty(),
            "builtin story `{}` rendered empty markup",
            story.slug()
        );
    }
}

// ── T8 (AC8, R9): rendered builtin HTML is well-formed/balanced ──────────

#[test]
fn every_builtin_story_renders_balanced_html() {
    for story in stories::builtin().stories() {
        let html = story
            .render()
            .unwrap_or_else(|err| panic!("builtin story `{}` failed: {err}", story.slug()))
            .into_string();
        assert_balanced_html(&html, &format!("builtin story `{}`", story.slug()));
    }
}

// ── T9 (AC8): slugs unique + well-formed, groups/names non-empty ─────────

fn is_well_formed_slug(slug: &str) -> bool {
    // ^[a-z0-9]+(-[a-z0-9]+)*$ without pulling in a regex dependency.
    !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[test]
fn builtin_slugs_unique_groups_and_names_nonempty() {
    let registry = stories::builtin();
    let mut seen = std::collections::HashSet::new();
    for story in registry.stories() {
        assert!(
            !story.group().trim().is_empty(),
            "story `{}` has an empty group",
            story.slug()
        );
        assert!(
            !story.name().trim().is_empty(),
            "story `{}` has an empty name",
            story.slug()
        );
        assert!(
            is_well_formed_slug(story.slug()),
            "slug `{}` must match ^[a-z0-9]+(-[a-z0-9]+)*$",
            story.slug()
        );
        assert!(
            seen.insert(story.slug().to_owned()),
            "duplicate builtin slug `{}`",
            story.slug()
        );
    }
}

// ── T10 (AC9): coverage gate part 1 — inventory diffed both directions ───

/// Expected slugs: one story per gallery-visible widget in
/// `autumn/src/widgets.rs` (plan D7). Adding a widget without a story fails
/// this gate; removing a widget without pruning its story fails the reverse
/// direction.
const EXPECTED_STORY_SLUGS: &[&str] = &[
    "active-search",
    "alert",
    "autocomplete",
    "avatar",
    "badge",
    "breadcrumb",
    "card",
    "charts",
    "confirm-action",
    "data-table",
    "hero",
    "infinite-feed",
    "locale-switcher",
    "modal",
    "nav-bar",
    "nav-link",
    "property-list",
    "reaction-controls",
    "stat-card",
    "tabs",
    "toast",
    "transition-controls",
];

#[test]
fn story_coverage_inventory_matches_registry() {
    let registry = stories::builtin();
    let actual: std::collections::BTreeSet<&str> =
        registry.stories().iter().map(Story::slug).collect();

    for expected in EXPECTED_STORY_SLUGS {
        assert!(
            actual.contains(expected),
            "widget `{expected}` has no builtin story — add a story! for it in \
             autumn/src/stories/builtin.rs"
        );
    }
    for slug in &actual {
        assert!(
            EXPECTED_STORY_SLUGS.contains(slug),
            "builtin story `{slug}` is missing from EXPECTED_STORY_SLUGS — add it to the \
             inventory in autumn/tests/integration/stories.rs (or remove the stale story)"
        );
    }
}

// ── T11 (AC9, R1): coverage gate part 2 — fn-name scan of widgets.rs ─────

/// True when `needle` occurs in `haystack` as a standalone identifier — i.e.
/// not merely as a substring of a longer identifier (`card` inside
/// `stat_card`, `search` inside `active_search`). Both neighbouring bytes
/// must be non-identifier chars (`[^A-Za-z0-9_]`) or the string edge.
fn contains_identifier(haystack: &str, needle: &str) -> bool {
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let boundary_before = start == 0 || !is_ident_byte(bytes[start - 1]);
        let boundary_after = end == bytes.len() || !is_ident_byte(bytes[end]);
        if boundary_before && boundary_after {
            return true;
        }
        from = start + 1;
    }
    false
}

#[test]
fn contains_identifier_requires_identifier_boundaries() {
    // Positive: true identifier occurrences (call sites, imports).
    assert!(contains_identifier("let x = card(&config);", "card"));
    assert!(contains_identifier(
        "use autumn_web::widgets::{tabs};",
        "tabs"
    ));
    assert!(contains_identifier("card", "card"));
    // Negative: substrings of longer identifiers must NOT count — these are
    // exactly the false-positive channels of a raw `contains` gate.
    assert!(!contains_identifier("stat_card(&config)", "card"));
    assert!(!contains_identifier("CardConfig::new()", "card"));
    assert!(!contains_identifier("active_search(&config)", "search"));
    assert!(!contains_identifier("modal_close_button(..)", "modal"));
    assert!(!contains_identifier("nav_link_matched(..)", "nav_link"));
    assert!(!contains_identifier("data_table(&config)", "table"));
}

#[test]
fn every_public_widget_fn_is_exercised_by_a_story() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(manifest_dir.join("src/widgets.rs"))
        .expect("failed to read src/widgets.rs");

    // Production lines only (widget_css_coverage.rs discipline), stripped of
    // `//` comments. Top-level widget fns start at column 0; config-struct
    // methods are indented and deliberately excluded.
    let mut widget_fns = Vec::new();
    for line in source.lines().take_while(|l| l.trim() != "mod tests {") {
        let code = line.split("//").next().unwrap_or("");
        if let Some(rest) = code.strip_prefix("pub fn ") {
            let name_len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let name = &rest[..name_len];
            if !name.is_empty() {
                widget_fns.push(name.to_owned());
            }
        }
    }
    assert!(
        widget_fns.len() >= 20,
        "widgets.rs scan looks broken; found only {widget_fns:?}"
    );

    let all_sources: String = stories::builtin()
        .stories()
        .iter()
        .map(Story::source)
        .collect::<Vec<_>>()
        .join("\n");

    let missing: Vec<&String> = widget_fns
        .iter()
        .filter(|name| !contains_identifier(&all_sources, name))
        .collect();
    assert!(
        missing.is_empty(),
        "widget fn(s) with no story mentioning them: {missing:?}\n\
         Add a story!{{}} block exercising them in autumn/src/stories/builtin.rs and, for a \
         new top-level widget, add its slug to EXPECTED_STORY_SLUGS in \
         autumn/tests/integration/stories.rs."
    );
}

// ── T12 (AC1): stories are zero-arg pure fn pointers ─────────────────────

fn zero_arg_demo() -> maud::Markup {
    maud::html! { p { "pure" } }
}

#[test]
fn stories_are_zero_arg_fn_pointers() {
    // `Story::new` accepts only a plain `fn() -> Markup` pointer, so a story
    // can never smuggle in a Db handle, AppState, or request data. (The
    // negative half — a capturing block failing to compile — is the trybuild
    // case tests/compile-fail/story_captures_environment.rs.)
    let render: fn() -> maud::Markup = zero_arg_demo;
    let story = Story::new("Test", "Pure", zero_arg_demo, "zero_arg_demo()");
    assert_eq!(
        story
            .render()
            .expect("zero-arg story renders")
            .into_string(),
        render().into_string()
    );
}

// ── T13 (AC5/AC6): config defaults off + layers by profile + env override ─

#[test]
fn stories_config_defaults_off_and_layers_by_profile() {
    assert!(
        !AutumnConfig::default().stories.enabled,
        "stories gallery must be off by default"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("autumn.toml"),
        r"
[stories]
enabled = false

[profile.dev.stories]
enabled = true
",
    )
    .expect("write autumn.toml");
    let manifest_dir = dir.path().to_str().unwrap();

    let dev = MockEnv::new()
        .with("AUTUMN_MANIFEST_DIR", manifest_dir)
        .with("AUTUMN_ENV", "dev");
    assert!(
        AutumnConfig::load_with_env(&dev)
            .expect("dev config loads")
            .stories
            .enabled,
        "[profile.dev.stories] enabled = true must win over the base in dev"
    );

    let prod = MockEnv::new()
        .with("AUTUMN_MANIFEST_DIR", manifest_dir)
        .with("AUTUMN_ENV", "prod");
    assert!(
        !AutumnConfig::load_with_env(&prod)
            .expect("prod config loads")
            .stories
            .enabled,
        "base enabled = false must hold in prod when only dev overrides"
    );

    // Env override wins in any profile.
    let empty_dir = tempfile::tempdir().expect("tempdir");
    let env_override = MockEnv::new()
        .with("AUTUMN_MANIFEST_DIR", empty_dir.path().to_str().unwrap())
        .with("AUTUMN_ENV", "prod")
        .with("AUTUMN_STORIES__ENABLED", "true");
    assert!(
        AutumnConfig::load_with_env(&env_override)
            .expect("env-override config loads")
            .stories
            .enabled,
        "AUTUMN_STORIES__ENABLED=true must enable the gallery"
    );
}

// ── T14 (AC2, R2): the snippet is provably the code that rendered ────────

#[test]
fn story_macro_source_is_the_executed_code() {
    let story = story! {
        "Test",
        "Fidelity",
        {
            // fidelity-comment: proves byte-level source capture
            maud::html! { p { "fidelity-proof" } }
        }
    };

    let rendered = story
        .render()
        .expect("fidelity story renders")
        .into_string();
    assert!(
        rendered.contains("fidelity-proof"),
        "the block must be executed for the live render: {rendered}"
    );

    assert_eq!(story.group(), "Test");
    assert_eq!(story.name(), "Fidelity");
    assert_eq!(story.slug(), "fidelity");

    assert!(
        story
            .source()
            .contains("// fidelity-comment: proves byte-level source capture"),
        "comments must survive into the snippet (source-text capture, not token \
         reconstruction): {}",
        story.source()
    );
    assert!(
        story
            .source()
            .contains(r#"maud::html! { p { "fidelity-proof" } }"#),
        "the snippet must contain the exact expression text that rendered: {}",
        story.source()
    );
}

// ── T15 (AC7): gallery extend + builtin-free constructor ─────────────────

#[test]
fn gallery_extend_and_builtin_free_constructor() {
    // Pins the prelude re-exports too (plan §4.9).
    use autumn_web::prelude::{StoryGallery as PreludeStoryGallery, story as prelude_story};

    assert!(
        PreludeStoryGallery::new().stories().is_empty(),
        "StoryGallery::new() must start builtin-free"
    );

    let builtin_count = stories::builtin().stories().len();
    let custom = prelude_story! {
        "App",
        "Team badge",
        {
            maud::html! { span { "custom widget" } }
        }
    };
    let gallery = StoryGallery::builtin().extend([custom]);
    assert_eq!(
        gallery.stories().len(),
        builtin_count + 1,
        "extend must append to the builtin set"
    );
    assert!(
        gallery.stories().iter().any(|s| s.slug() == "team-badge"),
        "extended custom story must be present under its slug"
    );

    // The registry type is also part of the public surface.
    let registry: StoryRegistry = StoryRegistry::new(vec![prelude_story! {
        "App",
        "Solo",
        {
            maud::html! { p { "solo" } }
        }
    }]);
    assert_eq!(registry.stories().len(), 1);
}

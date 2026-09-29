//! Tests for the `avatar` widget (issue #1263).
//!
//! Run with `cargo test --test integration_tests widgets_avatar`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{AvatarConfig, AvatarSize, avatar};

// ── image branch ─────────────────────────────────────────────────────────────

#[test]
fn image_branch_renders_img_with_lazy_loading_and_dimensions() {
    let out = avatar(
        "Ada Lovelace",
        &AvatarConfig::new()
            .image("/u/ada.png")
            .size(AvatarSize::Large),
    )
    .into_string();
    assert!(out.contains("<img"), "{out}");
    assert!(out.contains(r#"src="/u/ada.png""#), "{out}");
    assert!(
        out.contains(r#"alt="Ada Lovelace""#),
        "non-empty alt: {out}"
    );
    assert!(out.contains(r#"loading="lazy""#), "{out}");
    assert!(out.contains(r#"width="64""#), "square width: {out}");
    assert!(out.contains(r#"height="64""#), "square height: {out}");
    assert!(out.contains("autumn-avatar--image"), "{out}");
    assert!(out.contains("autumn-avatar--lg"), "{out}");
}

#[test]
fn blank_image_url_falls_back_to_initials() {
    // A Some("") from the database must not render a broken image.
    let out = avatar("Ada Lovelace", &AvatarConfig::new().image("   ")).into_string();
    assert!(!out.contains("<img"), "{out}");
    assert!(out.contains("autumn-avatar--initials"), "{out}");
}

// ── initials branch ──────────────────────────────────────────────────────────

#[test]
fn two_word_name_yields_two_initials() {
    let out = avatar("Ada Lovelace", &AvatarConfig::new()).into_string();
    assert!(out.contains("autumn-avatar--initials"), "{out}");
    assert!(out.contains(">AL<"), "{out}");
    assert!(out.contains(r#"role="img""#), "{out}");
    assert!(out.contains(r#"aria-label="Ada Lovelace""#), "{out}");
}

#[test]
fn single_word_name_yields_one_initial() {
    let out = avatar("cher", &AvatarConfig::new()).into_string();
    assert!(out.contains(">C<"), "{out}");
}

#[test]
fn three_word_name_uses_first_and_last() {
    let out = avatar("John Fitzgerald Kennedy", &AvatarConfig::new()).into_string();
    assert!(out.contains(">JK<"), "{out}");
}

#[test]
fn non_ascii_name_is_unicode_safe() {
    let out = avatar("józef pił", &AvatarConfig::new()).into_string();
    // First char uppercased per-character (not per-byte); never panics.
    assert!(out.contains(">JP<"), "{out}");
}

#[test]
fn empty_or_whitespace_name_uses_placeholder_glyph() {
    for name in ["", "   ", "\t\n"] {
        let out = avatar(name, &AvatarConfig::new()).into_string();
        assert!(out.contains(">?<"), "placeholder for {name:?}: {out}");
        assert!(
            out.contains(r#"alt="avatar""#) || out.contains(r#"aria-label="avatar""#),
            "{out}"
        );
    }
}

// ── determinism ──────────────────────────────────────────────────────────────

/// Extract the deterministic `autumn-avatar--cN` palette class from the markup.
fn palette_class(name: &str) -> String {
    let s = avatar(name, &AvatarConfig::new()).into_string();
    let start = s
        .find("autumn-avatar--c")
        .expect("initials avatar has a palette class");
    let rest = &s[start..];
    let end = rest.find([' ', '"']).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn same_name_yields_same_color_across_renders() {
    let a = avatar("Ada Lovelace", &AvatarConfig::new()).into_string();
    let b = avatar("Ada Lovelace", &AvatarConfig::new()).into_string();
    assert_eq!(a, b, "same name must render identically");
    // The color is a stable palette class (not an inline style).
    assert_eq!(palette_class("Ada Lovelace"), palette_class("Ada Lovelace"));
    assert!(a.contains("autumn-avatar--c"), "palette class present: {a}");
}

#[test]
fn different_names_get_distinct_colors() {
    // Names spread across the palette produce more than one distinct color
    // class (color varies by name), while each name stays deterministic.
    let names = [
        "Ada Lovelace",
        "Grace Hopper",
        "Alan Turing",
        "Katherine Johnson",
        "Donald Knuth",
        "Barbara Liskov",
        "Edsger Dijkstra",
        "Margaret Hamilton",
    ];
    let classes: std::collections::HashSet<String> =
        names.iter().map(|n| palette_class(n)).collect();
    assert!(
        classes.len() > 1,
        "color must vary by name; got only {classes:?}"
    );
}

// ── styling contract (CSP: no inline style attribute) ─────────────────────────

#[test]
fn initials_branch_emits_no_inline_style() {
    // The per-name color is a palette class, NOT an inline `style=` attribute —
    // so it survives a nonce-based CSP that forbids inline styles.
    let out = avatar("Ada", &AvatarConfig::new()).into_string();
    assert!(
        !out.contains("style="),
        "avatar must emit no inline style attribute: {out}"
    );
    // The deterministic color is still applied, via the stylesheet palette class.
    assert!(
        out.contains("autumn-avatar--c"),
        "palette class applied: {out}"
    );
}

#[test]
fn sizes_map_to_fixed_square_dimensions() {
    for (size, cls) in [
        (AvatarSize::Small, "autumn-avatar--sm"),
        (AvatarSize::Medium, "autumn-avatar--md"),
        (AvatarSize::Large, "autumn-avatar--lg"),
    ] {
        let out = avatar("A", &AvatarConfig::new().size(size)).into_string();
        assert!(out.contains(cls), "{out}");
    }
    assert_eq!(AvatarSize::Small.px(), 24);
    assert_eq!(AvatarSize::Medium.px(), 40);
    assert_eq!(AvatarSize::Large.px(), 64);
}

#[test]
fn crafted_name_cannot_inject_markup() {
    let out = avatar("<img src=x onerror=alert(1)>", &AvatarConfig::new()).into_string();
    assert!(!out.contains("<img src=x"), "name must be escaped: {out}");
    assert!(out.contains("&lt;img"), "{out}");
}

//! Name conversions used across the generators.
//!
//! Pure functions, fully unit-tested — no I/O, no allocations beyond the
//! returned `String`.

/// Convert `BlogPost` → `blog_post`.
#[must_use]
pub fn pascal_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

/// Convert `blog_post` → `BlogPost`.
#[must_use]
pub fn snake_to_pascal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper_next = true;
    for ch in s.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Naive English pluraliser, intentionally limited to the cases that come up
/// in identifier-shaped words (table names, route paths, file names).
///
/// Word-level rules (irregulars, sibilant endings, consonant+`y`) live in
/// [`autumn_web::format::pluralize_word`], shared with the view-layer
/// `pluralize` template helper so generated identifiers and rendered page
/// text agree. This wraps it with `snake_case` segment-splitting: all input
/// is treated as a single ASCII identifier, pluralised on the *last* chunk
/// only — `blog_post` → `blog_posts`.
#[must_use]
pub fn pluralize(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let (prefix, last) = s
        .rfind('_')
        .map_or(("", s), |idx| (&s[..=idx], &s[idx + 1..]));
    format!("{prefix}{}", autumn_web::format::pluralize_word(last))
}

/// Convert a resource name from any reasonable casing into `snake_case`.
///
/// Tolerates already-snake-case input (`blog_post` → `blog_post`).
#[must_use]
pub fn snake(s: &str) -> String {
    if s.contains('_') || !s.chars().any(char::is_uppercase) {
        s.to_ascii_lowercase()
    } else {
        pascal_to_snake(s)
    }
}

/// Convert a resource name from any reasonable casing into `PascalCase`.
#[must_use]
pub fn pascal(s: &str) -> String {
    if s.contains('_') || !s.chars().any(char::is_uppercase) {
        snake_to_pascal(s)
    } else {
        s.to_owned()
    }
}

/// Humanize a `snake_case`/`kebab-case` token into a display label:
/// split on `_`/`-`, then title-case each word (`in_review` → `In Review`).
///
/// Shared by the admin generator's `--select` widget and the scaffold
/// generator's `enum{…}` field `<select>` widget, so both render the same
/// label for the same value.
#[must_use]
pub fn humanize_label(value: &str) -> String {
    value
        .split(['_', '-'])
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_to_snake_simple() {
        assert_eq!(pascal_to_snake("Post"), "post");
        assert_eq!(pascal_to_snake("BlogPost"), "blog_post");
        assert_eq!(pascal_to_snake("UserProfile"), "user_profile");
    }

    #[test]
    fn snake_to_pascal_simple() {
        assert_eq!(snake_to_pascal("post"), "Post");
        assert_eq!(snake_to_pascal("blog_post"), "BlogPost");
        assert_eq!(snake_to_pascal("user_profile"), "UserProfile");
    }

    #[test]
    fn pluralize_simple() {
        assert_eq!(pluralize("post"), "posts");
        assert_eq!(pluralize("user"), "users");
        assert_eq!(pluralize("comment"), "comments");
    }

    #[test]
    fn pluralize_consonant_y() {
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("country"), "countries");
    }

    #[test]
    fn pluralize_vowel_y_keeps_y() {
        assert_eq!(pluralize("day"), "days");
        assert_eq!(pluralize("key"), "keys");
    }

    #[test]
    fn pluralize_sibilant_endings() {
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(pluralize("buzz"), "buzzes");
        assert_eq!(pluralize("class"), "classes");
        assert_eq!(pluralize("watch"), "watches");
        assert_eq!(pluralize("dish"), "dishes");
    }

    #[test]
    fn pluralize_pluralises_only_last_segment() {
        assert_eq!(pluralize("blog_post"), "blog_posts");
        assert_eq!(pluralize("user_category"), "user_categories");
    }

    #[test]
    fn pluralize_empty_string() {
        assert_eq!(pluralize(""), "");
    }

    #[test]
    fn pluralize_irregulars() {
        assert_eq!(pluralize("person"), "people");
        assert_eq!(pluralize("child"), "children");
    }

    #[test]
    fn snake_idempotent_on_snake() {
        assert_eq!(snake("blog_post"), "blog_post");
    }

    #[test]
    fn snake_normalises_pascal() {
        assert_eq!(snake("BlogPost"), "blog_post");
    }

    #[test]
    fn pascal_idempotent_on_pascal() {
        assert_eq!(pascal("BlogPost"), "BlogPost");
    }

    #[test]
    fn pascal_normalises_snake() {
        assert_eq!(pascal("blog_post"), "BlogPost");
    }

    #[test]
    fn humanize_label_title_cases_snake_case() {
        assert_eq!(humanize_label("draft"), "Draft");
        assert_eq!(humanize_label("in_review"), "In Review");
        assert_eq!(humanize_label("archived-old"), "Archived Old");
    }
}

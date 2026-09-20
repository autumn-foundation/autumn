//! Naming conventions shared by the model/repository codegen.
//!
//! Table-name inference (`BlogPost` → `blog_posts`) and the pluralisation
//! rules behind it. These live here — rather than inside
//! `autumn-macros-model` — because `#[repository]` (in
//! `autumn-macros-repository`) needs the same inference when the attribute
//! names no explicit table.

/// Infer a table name from a model struct name: `PascalCase` → `snake_case`,
/// pluralising only the last segment (`BlogPost` → `blog_posts`).
#[must_use]
pub fn infer_table_name(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    let snake = pascal_to_snake(&name);
    // Pluralize only the last snake_case segment, mirroring
    // `autumn-cli`'s `naming::pluralize`: `blog_post` → `blog_posts`,
    // `category` → `categories`.
    let (prefix, last) = snake.rfind('_').map_or(("", snake.as_str()), |idx| {
        (&snake[..=idx], &snake[idx + 1..])
    });
    format!("{prefix}{}", pluralize_word(last))
}

/// English pluraliser for a single word: irregulars, sibilant endings
/// (`+es`), consonant+`y` (`y` → `ies`), otherwise `+s`.
///
/// This is a FAITHFUL copy of `autumn_web::format::pluralize_word`, which is
/// the canonical implementation (see `autumn/src/format.rs::pluralize_word`).
/// It MUST stay in sync with that function: the CLI scaffold's `src/schema.rs`
/// pluralises table names through `autumn_web::format::pluralize_word` (via
/// `naming::pluralize`), and the `#[model]`/`#[repository]` derives here must
/// produce the same table name so the generated app compiles. It is duplicated
/// rather than imported because this proc-macro crate cannot depend on
/// `autumn-web` (that would create a dependency cycle: `autumn-web` depends on
/// `autumn-macros`).
#[must_use]
pub fn pluralize_word(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    match word {
        "person" => return "people".to_owned(),
        "child" => return "children".to_owned(),
        "man" => return "men".to_owned(),
        "woman" => return "women".to_owned(),
        "mouse" => return "mice".to_owned(),
        "goose" => return "geese".to_owned(),
        _ => {}
    }
    let lower = word.to_ascii_lowercase();
    if lower.ends_with("ss")
        || lower.ends_with('x')
        || lower.ends_with('z')
        || lower.ends_with("ch")
        || lower.ends_with("sh")
    {
        return format!("{word}es");
    }
    if lower.ends_with('y') {
        // 'y' is 1-byte ASCII, so slicing off the last byte stays on a char boundary.
        let prefix = &word[..word.len() - 1];
        if let Some(prev) = prefix.chars().next_back()
            && !"aeiouAEIOU".contains(prev)
        {
            return format!("{prefix}ies");
        }
    }
    format!("{word}s")
}

#[must_use]
pub fn pascal_to_snake(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_to_snake_simple() {
        assert_eq!(pascal_to_snake("User"), "user");
    }

    #[test]
    fn pascal_to_snake_multi_word() {
        assert_eq!(pascal_to_snake("BlogPost"), "blog_post");
    }

    #[test]
    fn pascal_to_snake_three_words() {
        assert_eq!(
            pascal_to_snake("UserProfileSettings"),
            "user_profile_settings"
        );
    }

    #[test]
    fn infer_table_name_simple() {
        let ident = syn::Ident::new("User", proc_macro2::Span::call_site());
        assert_eq!(infer_table_name(&ident), "users");
    }

    #[test]
    fn infer_table_name_multi_word() {
        let ident = syn::Ident::new("BlogPost", proc_macro2::Span::call_site());
        assert_eq!(infer_table_name(&ident), "blog_posts");
    }

    // Irregular-plural inference (#1753): the derived table name MUST match the
    // CLI scaffold's `src/schema.rs`, which pluralises through
    // `autumn_web::format::pluralize_word`. These mirror the assertions in
    // `autumn/src/format.rs` and `autumn-cli/src/generate/naming.rs` so all
    // three implementations agree.
    #[test]
    fn infer_table_name_irregular_plurals() {
        let cases = [
            ("Category", "categories"),
            ("Company", "companies"),
            ("City", "cities"),
            ("Story", "stories"),
            ("Box", "boxes"),
            ("Buzz", "buzzes"),
            ("Class", "classes"),
            ("Watch", "watches"),
            ("Dish", "dishes"),
            ("Person", "people"),
            ("Child", "children"),
            ("Post", "posts"),
            ("Node", "nodes"),
            ("Comment", "comments"),
            ("BlogPost", "blog_posts"),
            ("Day", "days"),
        ];
        for (input, expected) in cases {
            let ident = syn::Ident::new(input, proc_macro2::Span::call_site());
            assert_eq!(infer_table_name(&ident), expected, "input: {input}");
        }
    }

    #[test]
    fn pluralize_word_matches_canonical_rules() {
        assert_eq!(pluralize_word(""), "");
        assert_eq!(pluralize_word("category"), "categories");
        assert_eq!(pluralize_word("day"), "days");
        assert_eq!(pluralize_word("box"), "boxes");
        assert_eq!(pluralize_word("buzz"), "buzzes");
        assert_eq!(pluralize_word("class"), "classes");
        assert_eq!(pluralize_word("watch"), "watches");
        assert_eq!(pluralize_word("dish"), "dishes");
        assert_eq!(pluralize_word("person"), "people");
        assert_eq!(pluralize_word("goose"), "geese");
        assert_eq!(pluralize_word("post"), "posts");
    }
}

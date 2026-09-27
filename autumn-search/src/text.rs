//! Tokenization shared by the in-process ranking paths.
//!
//! Defined once so the in-memory backend and the hashing embedder agree on
//! what a "token" is. Engines with their own analyzers (Postgres' `tsvector`,
//! Meilisearch) use theirs — this is the fallback for backends that rank in
//! Rust.

/// Split `text` into lowercase alphanumeric tokens.
///
/// Unicode-aware (so `Äpfel` and `äpfel` yield the same token) and free of
/// punctuation, which is what makes an operator-looking query like `rust*` or
/// `title:rust` decompose into ordinary terms rather than query syntax.
pub fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

/// Tokenize a user query, returning `None` when nothing usable remains.
///
/// The caller MUST treat `None` as an empty result set and issue **no** query
/// — never an unfiltered scan. This mirrors the fail-closed contract of
/// `autumn_web::repository::sqlite_fts5_match_query`.
#[must_use]
pub fn query_tokens(query: &str) -> Option<Vec<String>> {
    let tokens: Vec<String> = tokenize(query).collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<String> {
        tokenize(text).collect()
    }

    #[test]
    fn tokenizing_lowercases_and_drops_punctuation() {
        assert_eq!(
            tokens("Rust web frameworks!"),
            ["rust", "web", "frameworks"]
        );
    }

    #[test]
    fn operator_syntax_decomposes_into_plain_terms() {
        assert_eq!(tokens("title:rust"), ["title", "rust"]);
        assert_eq!(tokens("rust*"), ["rust"]);
        assert_eq!(
            tokens("\"; DROP TABLE users; --"),
            ["drop", "table", "users"]
        );
    }

    #[test]
    fn tokenizing_is_unicode_case_folding() {
        assert_eq!(tokens("Äpfel"), ["äpfel"]);
        assert_eq!(tokens("äpfel"), ["äpfel"]);
    }

    #[test]
    fn a_blank_query_yields_none_so_callers_fail_closed() {
        assert!(query_tokens("").is_none());
        assert!(query_tokens("   ").is_none());
        assert!(query_tokens("\t\n").is_none());
        assert!(query_tokens("-- ;").is_none());
        assert_eq!(
            query_tokens("rust").as_deref(),
            Some(["rust".to_owned()].as_slice())
        );
    }
}

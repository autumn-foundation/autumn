//! `story!{ group, name, { ... } }` proc macro implementation (issue #1526).
//!
//! The macro takes a group string literal, a name string literal, and a
//! brace-delimited block. The block is **both** executed for the live render
//! and captured as the displayed source snippet, so the shown code is provably
//! the code that rendered. Source capture uses `Span::source_text()` on the
//! brace group (byte-faithful: comments and formatting survive), falling back
//! to the token-stream rendering when source text is unavailable (e.g. in
//! proc-macro2 fallback contexts such as unit tests).
//!
//! Expansion shape:
//!
//! ```ignore
//! ::autumn_web::stories::Story::new(#group, #name, || #block, #source)
//! ```
//!
//! The `|| #block` closure is coerced to a plain `fn() -> maud::Markup`
//! pointer by `Story::new`, so any environment capture (a `Db` handle,
//! `AppState`, request data) is a compile error — stories are zero-arg pure
//! functions by construction.

#[cfg(test)]
mod tests {
    use super::story_macro;
    use quote::quote;

    // M1 (AC2): the macro expands to `Story::new` carrying the group and name
    // literals through verbatim.
    #[test]
    fn expands_to_story_new_with_group_name() {
        let out = story_macro(quote! {
            "Display", "Data table", { maud::html! { p { "demo" } } }
        });
        let rendered = out.to_string();
        assert!(
            rendered.contains("Story :: new"),
            "expansion must construct ::autumn_web::stories::Story::new: {rendered}"
        );
        assert!(
            rendered.contains("\"Display\""),
            "expansion must carry the group literal: {rendered}"
        );
        assert!(
            rendered.contains("\"Data table\""),
            "expansion must carry the name literal: {rendered}"
        );
    }

    // M2 (AC2): missing block and non-literal group/name produce targeted
    // compile errors instead of opaque downstream failures.
    #[test]
    fn rejects_missing_block_and_non_literal_names() {
        let missing_block = story_macro(quote! { "Display", "Data table" }).to_string();
        assert!(
            missing_block.contains("compile_error"),
            "missing block must produce a compile error: {missing_block}"
        );
        assert!(
            missing_block.contains("block"),
            "missing-block error should mention the expected block: {missing_block}"
        );

        let non_literal =
            story_macro(quote! { Display, "Data table", { maud::html! { p { "x" } } } })
                .to_string();
        assert!(
            non_literal.contains("compile_error"),
            "non-literal group must produce a compile error: {non_literal}"
        );
        assert!(
            non_literal.contains("string literal"),
            "non-literal error should ask for a string literal: {non_literal}"
        );
    }

    // M3 (AC2, R14): when `Span::source_text()` is unavailable (always the
    // case for proc-macro2 fallback spans, i.e. in this unit test), the
    // emitted source literal falls back to the token-stream rendering of the
    // block instead of an empty snippet.
    #[test]
    fn falls_back_to_token_stream_when_no_source_text() {
        let block = quote! { maud::html! { p { "fallback-proof" } } };
        let out = story_macro(quote! { "Display", "Fallback", { #block } });
        let rendered = out.to_string();
        let expected_literal = proc_macro2::Literal::string(&block.to_string()).to_string();
        assert!(
            rendered.contains(&expected_literal),
            "with no source text available the source literal must equal the \
             token rendering of the block\nexpected literal: {expected_literal}\nexpansion: {rendered}"
        );
    }
}

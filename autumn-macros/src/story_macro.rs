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

use proc_macro2::{Delimiter, Group, Span, TokenStream, TokenTree};
use quote::quote;

/// Expand `story!{ group, name, { ... } }` into a
/// `::autumn_web::stories::Story::new(...)` constructor call.
pub fn story_macro(input: TokenStream) -> TokenStream {
    match expand(input) {
        Ok(expanded) => expanded,
        Err(error) => error.to_compile_error(),
    }
}

fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let mut tokens = input.into_iter();

    let group_lit = parse_string_literal(tokens.next(), "group")?;
    expect_comma(tokens.next(), "the story group")?;
    let name_lit = parse_string_literal(tokens.next(), "name")?;
    expect_comma(tokens.next(), "the story name")?;
    let block_group = parse_block_group(tokens.next())?;

    // Allow one trailing comma, then reject anything else.
    match tokens.next() {
        None => {}
        Some(TokenTree::Punct(punct)) if punct.as_char() == ',' => {
            if let Some(extra) = tokens.next() {
                return Err(syn::Error::new(
                    extra.span(),
                    "unexpected tokens after the story block: \
                     story!{ \"Group\", \"Name\", { ... } } takes exactly three arguments",
                ));
            }
        }
        Some(extra) => {
            return Err(syn::Error::new(
                extra.span(),
                "unexpected tokens after the story block: \
                 story!{ \"Group\", \"Name\", { ... } } takes exactly three arguments",
            ));
        }
    }

    // Validate the block parses as a real Rust block so authors get a
    // span-correct error here instead of a confusing downstream one.
    let block: syn::Block = syn::parse2(TokenStream::from(TokenTree::Group(block_group.clone())))?;

    // Source capture: byte-faithful `Span::source_text()` on the brace group
    // (comments and formatting survive), falling back to the token-stream
    // rendering when source text is unavailable (proc-macro2 fallback spans).
    let source = block_group.span().source_text().map_or_else(
        || block_group.stream().to_string(),
        |text| dedent_block_source(&text),
    );
    let source_lit = proc_macro2::Literal::string(&source);

    // `|| #block` is coerced to a plain `fn() -> maud::Markup` pointer by
    // `Story::new`, so any environment capture is a compile error.
    Ok(quote! {
        ::autumn_web::stories::Story::new(#group_lit, #name_lit, || #block, #source_lit)
    })
}

fn parse_string_literal(token: Option<TokenTree>, what: &str) -> syn::Result<syn::LitStr> {
    let Some(token) = token else {
        return Err(syn::Error::new(
            Span::call_site(),
            format!(
                "expected a string literal for the story {what}: story!{{ \"Group\", \"Name\", {{ ... }} }}"
            ),
        ));
    };
    let span = token.span();
    syn::parse2::<syn::LitStr>(TokenStream::from(token)).map_err(|_| {
        syn::Error::new(
            span,
            format!("expected a string literal for the story {what}: story!{{ \"Group\", \"Name\", {{ ... }} }}"),
        )
    })
}

fn expect_comma(token: Option<TokenTree>, after: &str) -> syn::Result<()> {
    match token {
        Some(TokenTree::Punct(punct)) if punct.as_char() == ',' => Ok(()),
        Some(other) => Err(syn::Error::new(
            other.span(),
            format!("expected `,` after {after}"),
        )),
        None => Err(syn::Error::new(
            Span::call_site(),
            format!("expected `,` after {after}, then a brace-delimited block"),
        )),
    }
}

fn parse_block_group(token: Option<TokenTree>) -> syn::Result<Group> {
    match token {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => Ok(group),
        Some(other) => Err(syn::Error::new(
            other.span(),
            "expected a brace-delimited block as the story body: \
             story!{ \"Group\", \"Name\", { ... } }",
        )),
        None => Err(syn::Error::new(
            Span::call_site(),
            "missing the story body: expected a brace-delimited block as the third \
             argument, story!{ \"Group\", \"Name\", { ... } }",
        )),
    }
}

/// Turn the captured `{ ... }` source text into a display-ready snippet:
/// strip the outer braces, drop blank leading/trailing lines, and remove the
/// common leading indentation so the snippet reads as top-level code.
fn dedent_block_source(text: &str) -> String {
    let inner = text.trim();
    let inner = inner.strip_prefix('{').unwrap_or(inner);
    let inner = inner.strip_suffix('}').unwrap_or(inner);

    let lines: Vec<&str> = inner.lines().collect();
    let first = lines.iter().position(|line| !line.trim().is_empty());
    let Some(first) = first else {
        return inner.trim().to_owned();
    };
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    let lines = &lines[first..=last];

    if lines.len() == 1 {
        return lines[0].trim().to_owned();
    }

    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);

    lines
        .iter()
        .map(|line| line.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

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

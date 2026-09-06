//! Resolve the identifier generated code should use in place of the literal
//! `::autumn_web` crate-root segment (issue #1828).
//!
//! Every route/derive macro in this crate emits thousands of `::autumn_web::…`
//! paths via `quote!`. Those paths are absolute (crate-root-anchored), so they
//! only resolve when the invoking crate depends on `autumn-web` under its
//! literal, unrenamed name — a crate that renames the dependency (`web = {
//! package = "autumn-web" }`) or that must host two differently-keyed
//! versions at once cannot use any Autumn macro.
//!
//! Rather than threading a resolved path through every `quote!` call site,
//! this module rewrites the *final* token stream each macro entry point in
//! `lib.rs` returns: every bare `::autumn_web` path segment (and every
//! `"::autumn_web…"` substring inside a string literal, for attribute values
//! like `#[serde(deserialize_with = "...")]` built via `format!`) is replaced
//! with the resolved name. The ~3000 `::autumn_web` references throughout the
//! rest of this crate never change; they keep meaning "the real `autumn-web`
//! crate, however the invoking crate's `Cargo.toml` names it."

use std::sync::OnceLock;

use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Group, Ident, Literal, Spacing, TokenStream, TokenTree};

/// The name generated code falls back to when no rename is detected, no
/// override is given, or resolution fails for any reason (e.g. this crate's
/// own unit tests, where `autumn-web` is not a declared dependency at all).
const DEFAULT_NAME: &str = "autumn_web";

/// Resolve the name `autumn-web` should be referred to as from the crate
/// currently being compiled, honoring a Cargo `package = "autumn-web"`
/// rename. Pure — does not cache — so tests can exercise it against a fixture
/// `Cargo.toml` (via `CARGO_MANIFEST_DIR`) without disturbing the cached
/// value `default_name` uses.
pub fn resolve_autumn_web_name() -> String {
    match crate_name("autumn-web") {
        Ok(FoundCrate::Name(name)) => name,
        // `Itself`: this crate's own `Cargo.toml` is named `autumn-web` — its
        // `extern crate self as autumn_web;` (in `autumn/src/lib.rs`) is what
        // makes the unrenamed default resolve when the framework dogfoods its
        // own macros internally. `Err`: no such dependency at all (this
        // crate's own unit tests) or `CARGO_MANIFEST_DIR` unset — fall back
        // to today's unconditional behavior either way.
        Ok(FoundCrate::Itself) | Err(_) => DEFAULT_NAME.to_owned(),
    }
}

/// Cached wrapper around [`resolve_autumn_web_name`] for the common,
/// no-override call path — `CARGO_MANIFEST_DIR` cannot change between
/// invocations of the same compiler process, and re-parsing `Cargo.toml` on
/// every single macro invocation in a large crate would add up.
fn default_name() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED.get_or_init(resolve_autumn_web_name)
}

/// Parse a `crate = "..."` override out of a macro's top-level attribute
/// tokens — the escape hatch for a downstream crate that must host two
/// versions of `autumn-web` at once, where automatic resolution above is
/// ambiguous (issue #1828). Returns the remaining tokens, with the override
/// (if any) removed, for the macro's own parser to consume unchanged.
///
/// `crate` is a reserved keyword, so no macro's existing `Ident`-keyed
/// argument grammar could previously have accepted a literal `crate = "..."`
/// pair (`syn::Ident::parse` rejects keywords) — every attribute macro can
/// therefore support this override with no grammar changes and no ambiguity
/// against any existing usage.
pub fn extract_crate_override(
    attr: TokenStream,
) -> Result<(Option<String>, TokenStream), TokenStream> {
    let tokens: Vec<TokenTree> = attr.into_iter().collect();
    let mut out: Vec<TokenTree> = Vec::with_capacity(tokens.len());
    let mut found: Option<String> = None;
    let mut i = 0;
    while i < tokens.len() {
        if found.is_none() {
            match match_crate_override(&tokens, i) {
                Some(Ok(name)) => {
                    found = Some(name);
                    i += 3;
                    // Consume exactly one adjacent comma so the remaining
                    // tokens stay a well-formed comma-separated list for the
                    // macro's own parser, however the override was
                    // positioned: leading, trailing, in the middle, or alone.
                    if matches!(tokens.get(i), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
                        i += 1;
                    } else if matches!(out.last(), Some(TokenTree::Punct(p)) if p.as_char() == ',')
                    {
                        out.pop();
                    }
                    continue;
                }
                Some(Err(err)) => return Err(err),
                None => {}
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    Ok((found, TokenStream::from_iter(out)))
}

/// Try to match a `crate = "..."` pair starting at `tokens[i]`.
///
/// `None` means `tokens[i..]` doesn't start with that shape at all — not our
/// concern, the caller copies `tokens[i]` through unchanged. `Some(Err(_))`
/// means it does start with `crate = <value>`, but `<value>` isn't a usable
/// override — a dedicated compile error here beats letting the macro's own
/// argument parser fail confusingly on an unrecognized `crate` key later.
fn match_crate_override(tokens: &[TokenTree], i: usize) -> Option<Result<String, TokenStream>> {
    let Some(TokenTree::Ident(id)) = tokens.get(i) else {
        return None;
    };
    if *id != "crate" {
        return None;
    }
    let Some(TokenTree::Punct(eq)) = tokens.get(i + 1) else {
        return None;
    };
    if eq.as_char() != '=' {
        return None;
    }
    let value_tt = tokens.get(i + 2);
    let lit_str = value_tt.and_then(|tt| match tt {
        TokenTree::Literal(lit) => syn::parse_str::<syn::LitStr>(&lit.to_string()).ok(),
        _ => None,
    });
    let Some(lit_str) = lit_str else {
        let span = value_tt.map_or_else(|| eq.span(), TokenTree::span);
        return Some(Err(syn::Error::new(
            span,
            "`crate = ...` must be a string literal naming the crate, e.g. \
             `crate = \"autumn_web\"`",
        )
        .to_compile_error()));
    };
    let value = lit_str.value();
    if syn::parse_str::<Ident>(&value).is_err() {
        return Some(Err(syn::Error::new(
            lit_str.span(),
            format!("`crate = {value:?}` is not a valid Rust identifier"),
        )
        .to_compile_error()));
    }
    Some(Ok(value))
}

/// Rewrite every macro-generated `::autumn_web` path to the resolved crate
/// name — the given override if present, otherwise the automatically
/// detected one.
pub fn finalize(ts: TokenStream, crate_override: Option<&str>) -> TokenStream {
    // Not `crate_override.map_or_else(default_name, |name| name)`: rustc
    // unifies the `|name| name` branch's return lifetime with
    // `crate_override`'s short borrow before it considers coercing
    // `default_name`'s `fn() -> &'static str` into that same bound, so the
    // "clean" clippy-suggested form fails to compile with a spurious
    // "borrowed data escapes outside of function" (E0521). The explicit
    // match sidesteps the inference order entirely.
    #[allow(clippy::option_if_let_else)]
    let target: &str = match crate_override {
        Some(name) => name,
        None => default_name(),
    };
    if target == DEFAULT_NAME {
        return ts;
    }
    rewrite(ts, target)
}

/// Recursively walk a token stream, rewriting `:: autumn_web` path segments
/// (never a bare, non-`::`-prefixed `autumn_web` — that form is under the
/// user's own control, e.g. inside a handler body this crate re-emits
/// verbatim, and is out of scope here) and string literals containing
/// `"::autumn_web"`.
fn rewrite(ts: TokenStream, target: &str) -> TokenStream {
    let tokens: Vec<TokenTree> = ts.into_iter().collect();
    let mut out: Vec<TokenTree> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            TokenTree::Group(g) => {
                let mut new_g = Group::new(g.delimiter(), rewrite(g.stream(), target));
                new_g.set_span(g.span());
                out.push(TokenTree::Group(new_g));
                i += 1;
            }
            TokenTree::Punct(p1) if p1.as_char() == ':' && p1.spacing() == Spacing::Joint => {
                if let (Some(TokenTree::Punct(p2)), Some(TokenTree::Ident(id))) =
                    (tokens.get(i + 1), tokens.get(i + 2))
                    && p2.as_char() == ':'
                    && *id == "autumn_web"
                {
                    out.push(tokens[i].clone());
                    out.push(tokens[i + 1].clone());
                    out.push(TokenTree::Ident(Ident::new(target, id.span())));
                    i += 3;
                    continue;
                }
                out.push(tokens[i].clone());
                i += 1;
            }
            TokenTree::Literal(lit) => {
                out.push(rewrite_literal(lit, target));
                i += 1;
            }
            other => {
                out.push(other.clone());
                i += 1;
            }
        }
    }
    TokenStream::from_iter(out)
}

/// Rewrite a `"::autumn_web::…"` substring inside a string literal's *value*
/// (not its raw token text, so escaping stays correct) — both the shape
/// `format!("::autumn_web::form::{base}")`-built attribute values take (e.g.
/// `#[serde(deserialize_with = "...")]`) and, via a raw string, `///` doc
/// comments (`#[doc = r"...[`Foo`](::autumn_web::bar::Foo)..."]`) mentioning
/// the crate by name.
fn rewrite_literal(lit: &Literal, target: &str) -> TokenTree {
    let repr = lit.to_string();
    // A cheap pre-filter before the real (quote-style-aware) parse below: no
    // non-string literal (integer, char, byte-string, ...) can contain this
    // substring, so this never rejects a literal actually worth rewriting.
    if repr.contains("::autumn_web")
        && let Ok(syn::Lit::Str(s)) = syn::parse_str::<syn::Lit>(&repr)
    {
        let replaced = s.value().replace("::autumn_web", &format!("::{target}"));
        let mut new_lit = Literal::string(&replaced);
        new_lit.set_span(lit.span());
        return TokenTree::Literal(new_lit);
    }
    TokenTree::Literal(lit.clone())
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    fn ts_string(ts: &TokenStream) -> String {
        ts.to_string()
    }

    #[test]
    fn rewrite_bare_path_segment_to_override() {
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = rewrite(input, "renamed_web");
        let s = ts_string(&out);
        assert!(s.contains(":: renamed_web :: Route"), "got: {s}");
        assert!(!s.contains("autumn_web"), "got: {s}");
    }

    #[test]
    fn rewrite_leaves_unprefixed_ident_alone() {
        // A bare (non-`::`-rooted) reference is out of our control — it's
        // either the user's own code or simply not a path we generated.
        let input = quote! { let autumn_web = 1; autumn_web::Foo };
        let out = rewrite(input, "renamed_web");
        let s = ts_string(&out);
        assert!(s.contains("autumn_web"), "got: {s}");
        assert!(!s.contains("renamed_web"), "got: {s}");
    }

    #[test]
    fn rewrite_nested_groups() {
        let input = quote! {
            impl Foo {
                fn bar() -> ::autumn_web::Bar {
                    if true { ::autumn_web::baz() } else { unreachable!() }
                }
            }
        };
        let out = rewrite(input, "renamed_web");
        let s = ts_string(&out);
        assert_eq!(s.matches("renamed_web").count(), 2, "got: {s}");
        assert!(!s.contains("autumn_web"), "got: {s}");
    }

    #[test]
    fn rewrite_string_literal_path() {
        let input = quote! {
            #[serde(deserialize_with = "::autumn_web::form::deserialize_naive_datetime_local")]
        };
        let out = rewrite(input, "renamed_web");
        let s = ts_string(&out);
        assert!(
            s.contains("\"::renamed_web::form::deserialize_naive_datetime_local\""),
            "got: {s}"
        );
    }

    #[test]
    fn rewrite_string_literal_leaves_unrelated_strings_alone() {
        let input = quote! { let x = "hello autumn_web world"; };
        let out = rewrite(input.clone(), "renamed_web");
        assert_eq!(ts_string(&out), ts_string(&input));
    }

    #[test]
    fn finalize_is_a_no_op_when_target_matches_default() {
        // No override, and no fixture rename in effect for this crate's own
        // test run (autumn-web is not a dependency of autumn-macros), so the
        // default resolves to "autumn_web" and finalize should not touch the
        // stream at all.
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = finalize(input.clone(), None);
        assert_eq!(ts_string(&out), ts_string(&input));
    }

    #[test]
    fn finalize_applies_an_explicit_override() {
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = finalize(input, Some("web_renamed"));
        let s = ts_string(&out);
        assert!(s.contains("web_renamed"));
        assert!(!s.contains("autumn_web"));
    }

    #[test]
    fn extract_crate_override_absent_returns_tokens_unchanged() {
        let input = quote! { "/path", name = "foo" };
        let (found, rest) = extract_crate_override(input.clone()).unwrap();
        assert!(found.is_none());
        assert_eq!(ts_string(&rest), ts_string(&input));
    }

    #[test]
    fn extract_crate_override_sole_argument() {
        let input = quote! { crate = "web_renamed" };
        let (found, rest) = extract_crate_override(input).unwrap();
        assert_eq!(found.as_deref(), Some("web_renamed"));
        assert_eq!(ts_string(&rest), "");
    }

    #[test]
    fn extract_crate_override_leading_position() {
        let input = quote! { crate = "web_renamed", name = "foo" };
        let (found, rest) = extract_crate_override(input).unwrap();
        assert_eq!(found.as_deref(), Some("web_renamed"));
        assert_eq!(ts_string(&rest), ts_string(&quote! { name = "foo" }));
    }

    #[test]
    fn extract_crate_override_trailing_position() {
        let input = quote! { "/path", name = "foo", crate = "web_renamed" };
        let (found, rest) = extract_crate_override(input).unwrap();
        assert_eq!(found.as_deref(), Some("web_renamed"));
        assert_eq!(
            ts_string(&rest),
            ts_string(&quote! { "/path", name = "foo" })
        );
    }

    #[test]
    fn extract_crate_override_middle_position() {
        let input = quote! { "/path", crate = "web_renamed", name = "foo" };
        let (found, rest) = extract_crate_override(input).unwrap();
        assert_eq!(found.as_deref(), Some("web_renamed"));
        assert_eq!(
            ts_string(&rest),
            ts_string(&quote! { "/path", name = "foo" })
        );
    }

    #[test]
    fn extract_crate_override_rejects_non_string_value() {
        let input = quote! { crate = 123 };
        let err = extract_crate_override(input).unwrap_err();
        assert!(ts_string(&err).contains("compile_error"));
    }

    #[test]
    fn extract_crate_override_rejects_invalid_identifier() {
        let input = quote! { crate = "not an ident" };
        let err = extract_crate_override(input).unwrap_err();
        assert!(ts_string(&err).contains("compile_error"));
    }

    #[test]
    fn extract_crate_override_never_matches_inside_nested_groups() {
        // `crate = "..."` is only meaningful as a top-level macro argument;
        // it must not be plucked out of an unrelated nested call-shaped
        // argument that happens to reuse the same shape.
        let input = quote! { seo(title = "crate = \"x\"") };
        let (found, rest) = extract_crate_override(input.clone()).unwrap();
        assert!(found.is_none());
        assert_eq!(ts_string(&rest), ts_string(&input));
    }

    /// Point `CARGO_MANIFEST_DIR` at a fixture directory containing the given
    /// `Cargo.toml` body, run `f`, then restore the original env var.
    /// `temp_env` serializes this against `CARGO_MANIFEST_DIR` being
    /// process-wide state and `cargo test` running this module's tests
    /// concurrently by default, and restores the previous value even if `f`
    /// panics.
    fn with_fixture_manifest<R>(cargo_toml: &str, f: impl FnOnce() -> R) -> R {
        let dir = tempfile_dir();
        std::fs::write(dir.join("Cargo.toml"), cargo_toml).expect("write fixture Cargo.toml");
        let result = temp_env::with_var("CARGO_MANIFEST_DIR", Some(&dir), f);
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "autumn-macros-crate-path-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[test]
    fn resolve_autumn_web_name_default_when_unrenamed() {
        let name = with_fixture_manifest(
            r#"
                [package]
                name = "downstream"
                version = "0.1.0"

                [dependencies]
                autumn-web = "0.7"
            "#,
            resolve_autumn_web_name,
        );
        assert_eq!(name, "autumn_web");
    }

    #[test]
    fn resolve_autumn_web_name_honors_cargo_rename() {
        let name = with_fixture_manifest(
            r#"
                [package]
                name = "downstream"
                version = "0.1.0"

                [dependencies]
                web = { package = "autumn-web", version = "0.5" }
            "#,
            resolve_autumn_web_name,
        );
        assert_eq!(name, "web");
    }

    #[test]
    fn resolve_autumn_web_name_falls_back_when_dependency_absent() {
        let name = with_fixture_manifest(
            r#"
                [package]
                name = "downstream"
                version = "0.1.0"

                [dependencies]
                serde = "1.0"
            "#,
            resolve_autumn_web_name,
        );
        assert_eq!(name, "autumn_web");
    }

    #[test]
    fn resolve_autumn_web_name_dashed_rename_is_sanitized() {
        // Cargo dependency keys may themselves contain dashes (e.g.
        // `autumn-web-05 = { package = "autumn-web" }`, the exact repro in
        // #1828); `proc-macro-crate` sanitizes these to a valid identifier.
        let name = with_fixture_manifest(
            r#"
                [package]
                name = "downstream"
                version = "0.1.0"

                [dependencies]
                autumn-web-05 = { package = "autumn-web", version = "0.5" }
            "#,
            resolve_autumn_web_name,
        );
        assert_eq!(name, "autumn_web_05");
    }

    // Whole-pipeline checks: feed a realistic input through one of this
    // crate's actual generators (not a synthetic `::autumn_web` token
    // stream), then `finalize` it, and prove no literal `autumn_web` survives
    // — the real regression risk is a generator that builds its
    // `::autumn_web` path some way our generic token walk doesn't expect
    // (e.g. through a helper that doesn't route through `quote!` the way
    // every test elsewhere in this file assumes).

    /// The contract these pipeline tests check: no `::autumn_web` (crate-root
    /// anchored path, in tokens or inside a string literal) survives
    /// `finalize`. A *bare*, non-`::`-prefixed mention of `autumn_web` — e.g.
    /// inside a doc comment's prose, like `` `autumn_web::encryption` `` in a
    /// generated `///` link — is deliberately left untouched, the same as a
    /// bare token ident (see `rewrite_leaves_unprefixed_ident_alone` above):
    /// it is not a path we generated and control, just informational text,
    /// and rewriting substrings inside arbitrary prose risks false positives
    /// with no compile-correctness upside.
    fn assert_no_rooted_autumn_web_path(s: &str) {
        assert!(!s.contains("::autumn_web"), "leaked `::autumn_web` in: {s}");
    }

    #[test]
    fn route_macro_pipeline_has_no_leaked_autumn_web_after_override() {
        let generated = crate::route::route_macro(
            "GET",
            "get",
            quote! { "/users/{id}", seo(title = "User") },
            quote! {
                async fn show_user(Path(id): Path<i64>) -> AutumnResult<Json<User>> {
                    Ok(Json(repo.find_by_id(id).await?))
                }
            },
        );
        let rewritten = finalize(generated, Some("renamed_autumn_web"));
        let s = ts_string(&rewritten);
        assert_no_rooted_autumn_web_path(&s);
        assert!(s.contains("renamed_autumn_web"), "got: {s}");
    }

    #[test]
    #[cfg(feature = "db")]
    fn model_macro_pipeline_has_no_leaked_autumn_web_after_override() {
        let item = quote! {
            struct Post {
                #[id]
                id: i64,
                title: String,
            }
        };
        let generated = crate::model::model_macro(quote! {}, item);
        let rewritten = finalize(generated, Some("renamed_autumn_web"));
        let s = ts_string(&rewritten);
        assert_no_rooted_autumn_web_path(&s);
        assert!(s.contains("renamed_autumn_web"), "got: {s}");
    }

    #[test]
    #[cfg(feature = "db")]
    fn repository_macro_pipeline_has_no_leaked_autumn_web_after_override() {
        let generated = crate::repository::repository_macro(
            quote! { Post },
            quote! { pub trait PostRepository {} },
        );
        let rewritten = finalize(generated, Some("renamed_autumn_web"));
        let s = ts_string(&rewritten);
        assert_no_rooted_autumn_web_path(&s);
        assert!(s.contains("renamed_autumn_web"), "got: {s}");
    }
}

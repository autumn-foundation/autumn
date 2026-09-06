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
//! `lib.rs` returns: every bare `::autumn_web` path segment is replaced with
//! the resolved name. The ~3000 `::autumn_web` references throughout the rest
//! of this crate never change; they keep meaning "the real `autumn-web`
//! crate, however the invoking crate's `Cargo.toml` names it." Only *tokens*
//! are rewritten this way, never string literal contents: a `::`-rooted path
//! is unambiguously a path reference wherever it appears, but a string
//! literal in the final expansion may be data the macro is simply re-emitting
//! verbatim from the user's own source (a route path, a doc comment, literal
//! text a handler returns) — blindly rewriting a substring inside it risks
//! corrupting that data on a false-positive match (Codex review, #2552).
//!
//! The handful of call sites that build a string *containing* a crate-rooted
//! path (`#[serde(deserialize_with = "::autumn_web::form::...")]`,
//! `#[serde(crate = "::autumn_web::reexports::serde")]`) instead read
//! [`current_target`] directly, so they construct the resolved string from
//! the start rather than needing a later post-hoc rewrite.
//!
//! Beyond generated code, several modules ([`crate::idempotency_guard`],
//! [`crate::agent_authority`], [`crate::mailer`], [`crate::ws`]) *recognize*
//! `::autumn_web`-rooted paths — e.g. to detect that an earlier-expanded
//! stacked macro (`#[authorize]` before `#[secured]`) already injected a
//! particular guard call, so a later macro doesn't duplicate or miss it. Once
//! that earlier macro's own output has been rewritten to a renamed target,
//! recognizing it against the literal `"autumn_web"` breaks the same way
//! generation would (Codex review, #2552) — these also call
//! [`current_target`] instead of hardcoding the default.
//!
//! [`set_target`] is what makes the resolved name available to both: every
//! macro entry point in `lib.rs` calls it (via the returned guard's binding)
//! to cover its *entire* expansion, not just the later [`finalize`] pass —
//! recognizers run *during* the macro's own logic, inspecting input that may
//! already carry an earlier macro's renamed output.
//!
//! # Scope: an override rewrites the *whole* annotated item, not just
//! generated code (Codex review, #2552)
//!
//! `finalize` walks the macro's *entire* returned token stream, which
//! includes the user's own re-emitted function/struct — not only the
//! newly generated companion code — because the two are not structurally
//! separable in general (a guard macro like `#[secured]` interleaves
//! generated prologue statements into the user's own function body rather
//! than keeping the two cleanly apart). In the overwhelmingly common case
//! (automatic resolution, no explicit override) this is harmless and even
//! helpful: `::autumn_web` isn't a valid path *at all* once the dependency
//! is renamed, so any such path the user happened to hardcode themselves
//! needs exactly this same rewrite to keep working.
//!
//! It matters only for the explicit `crate = "..."` override — the
//! dual-version case, where `autumn_web` (unrenamed) and the override target
//! are simultaneously valid, distinct dependencies. An item that applies an
//! override and *also* contains the user's own fully-qualified `::autumn_web`
//! reference (intending the *other*, unrenamed instance) has that reference
//! rewritten to the override target too, potentially resolving to the wrong
//! instance. The guidance: don't mix explicit references to both instances
//! within one item that carries a `crate = "..."` override; give the
//! non-default instance its own distinct name in the reference itself
//! (whatever its own dependency key is) rather than writing bare
//! `autumn_web` and relying on it staying unrenamed.

use std::cell::RefCell;

use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Group, Ident, Spacing, TokenStream, TokenTree};

/// The name generated code falls back to when no rename is detected, no
/// override is given, or resolution fails for any reason (e.g. this crate's
/// own unit tests, where `autumn-web` is not a declared dependency at all).
const DEFAULT_NAME: &str = "autumn_web";

/// Resolve the name `autumn-web` should be referred to as from the crate
/// currently being compiled, honoring a Cargo `package = "autumn-web"`
/// rename.
///
/// Deliberately uncached on this side: `crate_name` already maintains its own
/// process-wide cache keyed by manifest directory and `Cargo.toml`
/// modification time, so a second cache here would only save a cheap
/// mutex-lock-and-lookup — not worth the risk of it going stale relative to
/// `crate_name`'s own invalidation, or (Codex review, #2552) getting
/// initialized from a test's temporarily-swapped `CARGO_MANIFEST_DIR` and
/// staying poisoned with that fixture's answer for the rest of the process.
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

thread_local! {
    // A macro invocation never spans threads and never runs concurrently
    // with another (rustc calls one macro function to completion before
    // calling the next), so a single cell — not a stack — is enough. It
    // still resets on drop rather than at the *next* `set_target` call so a
    // panic mid-expansion can't leave a stale value for whatever the
    // compiler process's next unrelated macro invocation reads.
    static CURRENT_TARGET: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// RAII guard returned by [`set_target`]; restores the previous target on
/// drop (including on unwind), so a nested or short-circuiting return can't
/// leave a stale target for whatever this same compiler process expands
/// next.
pub struct TargetGuard {
    previous: Option<String>,
}

impl Drop for TargetGuard {
    fn drop(&mut self) {
        CURRENT_TARGET.with(|cell| *cell.borrow_mut() = self.previous.take());
    }
}

/// Set the crate-name target for the rest of the current scope (until the
/// returned guard drops) — the given override, or the automatically detected
/// default when `None`. Bind the result (`let _guard = ...;`), covering the
/// entire expansion: both the later [`finalize`] pass over this macro's own
/// output, and any nested call into this crate's own generators or
/// recognizers, which need the same resolved name while *they* run, not just
/// once their result is finalized.
pub fn set_target(crate_override: Option<&str>) -> TargetGuard {
    let target: String =
        crate_override.map_or_else(resolve_autumn_web_name, std::borrow::ToOwned::to_owned);
    let previous = CURRENT_TARGET.with(|cell| cell.borrow_mut().replace(target));
    TargetGuard { previous }
}

/// The crate-name target the innermost enclosing [`set_target`] scope set,
/// or the unrenamed default if none is active (e.g. a unit test in this
/// crate calling a generator or recognizer directly, without going through
/// `lib.rs`'s macro entry points).
pub fn current_target() -> String {
    CURRENT_TARGET.with(|cell| {
        cell.borrow()
            .clone()
            .unwrap_or_else(|| DEFAULT_NAME.to_owned())
    })
}

/// Rewrite every macro-generated `::autumn_web` path to [`current_target`].
/// A no-op whenever that target is the unrenamed default — the overwhelming
/// majority of expansions, since a rename or override is rare.
pub fn finalize(ts: TokenStream) -> TokenStream {
    let target = current_target();
    if target == DEFAULT_NAME {
        return ts;
    }
    rewrite(ts, &target)
}

/// Recursively walk a token stream, rewriting `:: autumn_web` path segments —
/// never a bare, non-`::`-prefixed `autumn_web` (that form is under the
/// user's own control, e.g. inside a handler body this crate re-emits
/// verbatim, and is out of scope here), and never a string literal's
/// contents (data, not a path reference — see the module doc).
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
                    out.push(TokenTree::Ident(ident_for_target(target, id.span())));
                    i += 3;
                    continue;
                }
                out.push(tokens[i].clone());
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

/// Build the replacement `Ident` for `target`, using a raw identifier
/// (`r#type`) when it's a Rust keyword. `target` usually comes from
/// [`current_target`], which for the automatic (no-override) path is
/// whatever `proc_macro_crate` returns for the invoking crate's own
/// dependency key — unlike an explicit `crate = "..."` override (validated
/// in [`match_crate_override`]), that key was never checked against being a
/// bare, non-raw-identifier-safe Rust identifier, so a dependency renamed to
/// a keyword (`type = { package = "autumn-web" }`, unusual but valid TOML)
/// would otherwise panic `Ident::new` (Codex review, #2552).
fn ident_for_target(target: &str, span: proc_macro2::Span) -> Ident {
    let (raw, bare) = raw_escape(target);
    if raw {
        Ident::new_raw(bare, span)
    } else {
        Ident::new(bare, span)
    }
}

/// Whether `target` needs raw-identifier escaping to be used as a path
/// segment, and its bare (un-prefixed) name either way.
///
/// `target` (from [`current_target`]) takes one of three shapes:
/// - a normal name (`"autumn_web"`) — not raw, used as-is;
/// - a bare keyword (`"type"`) from the *automatic*, unvalidated resolution
///   path (`match_crate_override` already rejects a bare keyword `crate =
///   "..."` override, so this can't come from there) — needs escaping;
/// - an explicit override already spelled with the escape (`"r#type"`,
///   `match_crate_override`'s validation accepts this — `syn::parse_str`
///   parses a raw identifier as a valid `Ident`) — the literal two-character
///   `r#` prefix must be stripped *before* handing the bare name to
///   `Ident::new_raw`, which (like `Ident::new`) panics if given text
///   containing `#` (Codex review, #2552).
#[allow(clippy::option_if_let_else)]
fn raw_escape(target: &str) -> (bool, &str) {
    if let Some(bare) = target.strip_prefix("r#") {
        (true, bare)
    } else if syn::parse_str::<Ident>(target).is_ok() {
        (false, target)
    } else {
        (true, target)
    }
}

/// The source text for [`current_target`] as a path segment, escaped for
/// embedding inside a string literal that gets re-parsed as a path — the
/// shape `#[serde(crate = "...")]` and `#[serde(deserialize_with = "...")]`
/// need (`model.rs`, `event.rs`), since those never pass through the token
/// rewrite [`ident_for_target`] backs (see the module doc).
pub fn escaped_target_path_segment(target: &str) -> String {
    let (raw, bare) = raw_escape(target);
    if raw {
        format!("r#{bare}")
    } else {
        bare.to_owned()
    }
}

/// [`escaped_target_path_segment`] of [`current_target`] — what a
/// recognizer comparing a path's crate-root `Ident` against the actively
/// resolved name must compare against instead of the bare name, once a
/// keyword rename is in play. A raw identifier `Ident` (`r#type`) compares
/// equal to the *escaped* string `"r#type"`, not the bare `"type"` — using
/// [`current_target`] directly here has the same effect as skipping this
/// module's own [`ident_for_target`] when *emitting* the identifier: both
/// silently produce a token that no longer matches what it's supposed to
/// (Codex review, #2552, round 2 of the keyword-rename fix — this recognizer
/// side was still comparing against the bare name after the emission side
/// was already fixed).
pub fn current_target_path_segment() -> String {
    escaped_target_path_segment(&current_target())
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
    fn rewrite_uses_a_raw_identifier_for_a_keyword_target() {
        // A dependency renamed to a bare Rust keyword (`type = { package =
        // "autumn-web" }`) is unusual but valid TOML; `proc_macro_crate`
        // returns it verbatim (only dashes are sanitized), so the token
        // rewrite must not hand a keyword straight to `Ident::new` (which
        // panics) — it needs the raw-identifier form instead.
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = rewrite(input, "type");
        let s = ts_string(&out);
        assert!(s.contains(":: r#type :: Route"), "got: {s}");
    }

    #[test]
    fn rewrite_strips_and_reapplies_an_explicit_raw_override() {
        // The *only* way `crate = "..."` can target a keyword-named
        // dependency is already spelled with the raw-identifier escape
        // (`crate = "r#type"`) — `match_crate_override`'s own validation
        // rejects a bare `crate = "type"` the same way `Ident::new` would.
        // The literal two-character `r#` prefix in that override string
        // must never reach `Ident::new`/`Ident::new_raw` directly (both
        // panic on a literal `#` character) — it has to be stripped and
        // reapplied as raw-ness, not as text.
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = rewrite(input, "r#type");
        let s = ts_string(&out);
        assert!(s.contains(":: r#type :: Route"), "got: {s}");
    }

    #[test]
    fn escaped_target_path_segment_escapes_a_bare_keyword() {
        assert_eq!(escaped_target_path_segment("type"), "r#type");
    }

    #[test]
    fn escaped_target_path_segment_normalizes_an_already_raw_override() {
        assert_eq!(escaped_target_path_segment("r#type"), "r#type");
    }

    #[test]
    fn escaped_target_path_segment_leaves_a_normal_name_alone() {
        assert_eq!(escaped_target_path_segment("autumn_web"), "autumn_web");
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
    fn rewrite_never_touches_string_literal_contents() {
        // A string literal is data, not a path reference, even when its
        // contents happen to look like one — e.g. a route path re-emitted
        // verbatim (`#[get("/proxy/::autumn_web")]`) or a handler literally
        // returning that text. Rewriting substrings inside it would corrupt
        // that data on a false-positive match (Codex review, #2552), so
        // `rewrite` must leave every literal's token text byte-for-byte
        // alone regardless of what it contains.
        let input = quote! {
            let a = "hello autumn_web world";
            let b = "::autumn_web::form::deserialize_naive_datetime_local";
            let c = "/proxy/::autumn_web";
        };
        let out = rewrite(input.clone(), "renamed_web");
        assert_eq!(ts_string(&out), ts_string(&input));
    }

    #[test]
    fn set_target_default_when_no_scope_active() {
        assert_eq!(current_target(), "autumn_web");
    }

    #[test]
    fn set_target_applies_an_explicit_override() {
        let _guard = set_target(Some("web_renamed"));
        assert_eq!(current_target(), "web_renamed");
    }

    #[test]
    fn set_target_restores_previous_value_on_drop() {
        assert_eq!(current_target(), "autumn_web");
        {
            let _guard = set_target(Some("outer"));
            assert_eq!(current_target(), "outer");
            {
                let _guard = set_target(Some("inner"));
                assert_eq!(current_target(), "inner");
            }
            assert_eq!(current_target(), "outer");
        }
        assert_eq!(current_target(), "autumn_web");
    }

    #[test]
    fn finalize_is_a_no_op_when_target_matches_default() {
        // No override, and no fixture rename in effect for this crate's own
        // test run (autumn-web is not a dependency of autumn-macros), so the
        // default resolves to "autumn_web" and finalize should not touch the
        // stream at all.
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = finalize(input.clone());
        assert_eq!(ts_string(&out), ts_string(&input));
    }

    #[test]
    fn finalize_applies_the_active_target() {
        let _guard = set_target(Some("web_renamed"));
        let input = quote! { fn foo() -> ::autumn_web::Route { } };
        let out = finalize(input);
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

    #[test]
    fn set_target_none_re_resolves_fresh_every_call_no_stale_cache() {
        // Regression test (Codex review, #2552): `set_target(None)` must
        // read `resolve_autumn_web_name()` fresh on every call rather than
        // caching it process-wide — a cache here would risk permanently
        // poisoning every subsequent (unrelated) `set_target(None)` call in
        // this test binary with whichever fixture happened to be active the
        // first time it ran.
        let first = with_fixture_manifest(
            r#"
                [package]
                name = "downstream"
                version = "0.1.0"

                [dependencies]
                web_one = { package = "autumn-web", version = "0.1" }
            "#,
            || {
                let _guard = set_target(None);
                current_target()
            },
        );
        assert_eq!(first, "web_one");

        let second = with_fixture_manifest(
            r#"
                [package]
                name = "downstream"
                version = "0.1.0"

                [dependencies]
                web_two = { package = "autumn-web", version = "0.1" }
            "#,
            || {
                let _guard = set_target(None);
                current_target()
            },
        );
        assert_eq!(second, "web_two");
    }

    // Whole-pipeline checks: feed a realistic input through one of this
    // crate's actual generators (not a synthetic `::autumn_web` token
    // stream), then `finalize` it, and prove no literal `autumn_web` survives
    // — the real regression risk is a generator that builds its
    // `::autumn_web` path some way our generic token walk doesn't expect
    // (e.g. through a helper that doesn't route through `quote!` the way
    // every test elsewhere in this file assumes).

    /// The contract these pipeline tests check: no genuine `::autumn_web`
    /// *token* path (crate-root anchored) survives `finalize`. `to_string()`
    /// renders a real `:: Ident ::` token sequence with spaces around the
    /// identifier (`":: renamed_autumn_web ::"`, matching e.g.
    /// `rewrite_bare_path_segment_to_override` above); a doc comment or other
    /// string literal's *contents* render with no such surrounding space,
    /// however they read, since a literal is one opaque token — so this
    /// specifically will not (and must not) flag those. Doc comments
    /// generated by these pipelines legitimately still say `::autumn_web`
    /// after a rename (string literals are never rewritten — see the module
    /// doc); that's an accepted, purely cosmetic trade-off against the
    /// alternative of risking corruption of a user's own string data.
    fn assert_no_leaked_autumn_web_token_path(s: &str) {
        assert!(
            !s.contains(":: autumn_web"),
            "leaked `::autumn_web` token path in: {s}"
        );
    }

    #[test]
    fn route_macro_pipeline_has_no_leaked_autumn_web_after_override() {
        let _guard = set_target(Some("renamed_autumn_web"));
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
        let rewritten = finalize(generated);
        let s = ts_string(&rewritten);
        assert_no_leaked_autumn_web_token_path(&s);
        assert!(s.contains("renamed_autumn_web"), "got: {s}");
    }

    #[test]
    #[cfg(feature = "db")]
    fn model_macro_pipeline_has_no_leaked_autumn_web_after_override() {
        let _guard = set_target(Some("renamed_autumn_web"));
        let item = quote! {
            struct Post {
                #[id]
                id: i64,
                title: String,
            }
        };
        let generated = crate::model::model_macro(quote! {}, item);
        let rewritten = finalize(generated);
        let s = ts_string(&rewritten);
        assert_no_leaked_autumn_web_token_path(&s);
        assert!(s.contains("renamed_autumn_web"), "got: {s}");
        // The `deserialize_with`/`#[serde(crate = "...")]` string values this
        // pipeline builds (issue #1828's original literal-rewrite targets)
        // must reflect the active target too — proving `current_target()` at
        // the source beats the removed post-hoc literal rewrite.
        assert!(
            s.contains("renamed_autumn_web :: reexports :: serde"),
            "got: {s}"
        );
    }

    #[test]
    #[cfg(feature = "db")]
    fn repository_macro_pipeline_has_no_leaked_autumn_web_after_override() {
        let _guard = set_target(Some("renamed_autumn_web"));
        let generated = crate::repository::repository_macro(
            quote! { Post },
            quote! { pub trait PostRepository {} },
        );
        let rewritten = finalize(generated);
        let s = ts_string(&rewritten);
        assert_no_leaked_autumn_web_token_path(&s);
        assert!(s.contains("renamed_autumn_web"), "got: {s}");
    }

    /// Regression test for the exact scenario a Codex review on #2552 found:
    /// stacking `#[authorize]` above `#[secured]` under a rename must still
    /// let the route macro recognize the replay guard `#[authorize]`'s own
    /// (already-finalized, already-renamed) expansion injected, rather than
    /// missing it because the recognizer only knew the literal
    /// `"autumn_web"`.
    #[test]
    fn replay_guard_recognized_after_stacked_macro_rename() {
        let _guard = set_target(Some("renamed_autumn_web"));
        // Simulate what `#[authorize]` (or `#[secured]`/`#[step_up]`) leaves
        // behind once ITS OWN `finalize` has already run: a block whose
        // early-return replay check is rooted at the *renamed* crate, not
        // `autumn_web` literally.
        let block: syn::Block = syn::parse_quote! {{
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::renamed_autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        }};
        assert!(
            crate::idempotency_guard::block_has_replay_guard(&block),
            "recognizer must accept the actively-resolved crate name, not just the literal \
             \"autumn_web\""
        );
    }

    /// Regression test for a second Codex round on the same #2552 scenario:
    /// once the target is a keyword (`"type"`, from automatic resolution —
    /// see `ident_for_target`/`rewrite`), an earlier-expanded macro's own
    /// `finalize` pass emits the *raw* identifier `r#type`, not the bare
    /// `type`. A recognizer comparing against the bare `current_target()`
    /// (rather than `current_target_path_segment()`, which accounts for the
    /// raw prefix) misses the match — the same class of bug as
    /// `replay_guard_recognized_after_stacked_macro_rename` above, just
    /// triggered by a keyword target instead of a plain renamed one.
    #[test]
    fn replay_guard_recognized_after_stacked_macro_rename_with_keyword_target() {
        let _guard = set_target(Some("type"));
        let block: syn::Block = syn::parse_quote! {{
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::r#type::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        }};
        assert!(
            crate::idempotency_guard::block_has_replay_guard(&block),
            "recognizer must compare against the raw-escaped target, not the bare keyword"
        );
    }
}

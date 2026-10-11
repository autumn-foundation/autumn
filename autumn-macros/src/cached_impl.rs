//! `#[cached_impl]` proc macro implementation.
//!
//! The macro adds `scope = "<Self type>"` to each `#[cached]` method of an
//! `impl` block (#2358). A method attribute cannot see its `impl`. This one can.

use proc_macro2::TokenStream;
use quote::quote;

use crate::cached::parse_cached_args;

/// `#[cached_impl]`: add `scope = "<Self type>"` to each `#[cached]` method.
///
/// An attribute on a method cannot see its `impl`. This one can, so it passes
/// the type down (#2358).
pub fn cached_impl_macro(
    attr: TokenStream,
    item: TokenStream,
    crate_override: Option<&str>,
) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new_spanned(attr, "`#[cached_impl]` takes no arguments")
            .to_compile_error();
    }
    let mut imp: syn::ItemImpl = match syn::parse2(item) {
        Ok(imp) => imp,
        Err(err) => return err.to_compile_error(),
    };
    // The whole path, without generics: `impl a::Store` and `impl b::Store`
    // in one module must not share a scope. A leading `crate` adds nothing.
    let scope = match &*imp.self_ty {
        syn::Type::Path(path) if path.qself.is_none() => {
            let names: Vec<String> = path
                .path
                .segments
                .iter()
                .map(|seg| seg.ident.to_string())
                .skip_while(|name| name == "crate")
                .collect();
            (!names.is_empty()).then(|| names.join("::"))
        }
        _ => None,
    };
    let Some(scope) = scope else {
        return syn::Error::new_spanned(
            &imp.self_ty,
            "`#[cached_impl]` needs a named type; use `#[cached(scope = \"..\")]` instead",
        )
        .to_compile_error();
    };
    let target = autumn_macros_support::crate_path::current_target_path_segment();
    for item in &mut imp.items {
        let syn::ImplItem::Fn(method) = item else {
            continue;
        };
        for attr in &mut method.attrs {
            if is_cached_path(attr.path(), &target) {
                scope_cached_attr(attr, &scope, crate_override);
            }
        }
    }
    quote! { #imp }
}

/// Whether a path names this crate's `cached` macro.
///
/// `target` is the resolved crate name as it is spelled in a path. It may be
/// renamed (`web::cached`) or raw (`r#type::cached`).
/// Another crate's `cached` macro (`memo::cached`) stays untouched.
fn is_cached_path(path: &syn::Path, target: &str) -> bool {
    let names: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    match names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["cached"] => true,
        [krate, "cached"] | [krate, "prelude", "cached"] => {
            *krate == target || matches!(*krate, "autumn_web" | "crate")
        }
        _ => false,
    }
}

/// Add `scope = "<scope>"` and the outer `crate = ".."` override to a
/// `#[cached]` attribute. Each is added only if the attribute lacks it.
fn scope_cached_attr(attr: &mut syn::Attribute, scope: &str, crate_override: Option<&str>) {
    let path = attr.path().clone();
    let tokens = match &attr.meta {
        syn::Meta::Path(_) => TokenStream::new(),
        syn::Meta::List(list) => list.tokens.clone(),
        syn::Meta::NameValue(_) => return,
    };
    let (inner_crate, rest) =
        autumn_macros_support::crate_path::extract_crate_override(tokens.clone())
            .unwrap_or_else(|_| (None, tokens.clone()));
    let has_scope = parse_cached_args(rest).is_ok_and(|a| a.scope.is_some());
    let forward_crate = if inner_crate.is_some() {
        None
    } else {
        crate_override
    };
    if has_scope && forward_crate.is_none() {
        return;
    }
    // A trailing comma is legal in `#[cached(ttl = "5m",)]`. Drop it, so the
    // comma added below does not double it.
    let mut kept: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
    if matches!(kept.last(), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == ',') {
        kept.pop();
    }
    let tokens: TokenStream = kept.into_iter().collect();
    let mut extra: Vec<TokenStream> = Vec::new();
    if !has_scope {
        extra.push(quote! { scope = #scope });
    }
    if let Some(name) = forward_crate {
        extra.push(quote! { crate = #name });
    }
    *attr = if tokens.is_empty() {
        syn::parse_quote! { #[#path(#(#extra),*)] }
    } else {
        syn::parse_quote! { #[#path(#tokens, #(#extra),*)] }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_impl_forwards_the_crate_override() {
        let out = cached_impl_macro(
            TokenStream::new(),
            quote! { impl Products { #[cached] async fn get(id: i64) -> String { String::new() } } },
            Some("web"),
        )
        .to_string();
        assert!(
            out.contains("# [cached (scope = \"Products\" , crate = \"web\")]"),
            "{out}"
        );
    }

    #[test]
    fn cached_impl_forwards_the_crate_override_beside_an_explicit_scope() {
        let out = cached_impl_macro(
            TokenStream::new(),
            quote! { impl P { #[cached(scope = "Mine")] async fn f() {} } },
            Some("web"),
        )
        .to_string();
        assert!(
            out.contains("# [cached (scope = \"Mine\" , crate = \"web\")]"),
            "{out}"
        );
        assert_eq!(out.matches("scope").count(), 1, "{out}");
    }

    #[test]
    fn cached_impl_scopes_the_prelude_path() {
        let out = scoped(
            TokenStream::new(),
            quote! { impl P { #[autumn_web::prelude::cached] fn f() {} } },
        );
        assert!(
            out.contains("# [autumn_web :: prelude :: cached (scope = \"P\")]"),
            "{out}"
        );
    }

    #[test]
    fn cached_impl_scopes_a_keyword_renamed_crate_path() {
        let _target = autumn_macros_support::crate_path::set_target(Some("type"));
        let out = scoped(
            TokenStream::new(),
            quote! { impl P { #[r#type::cached] fn f() {} } },
        );
        assert!(out.contains("scope = \"P\""), "{out}");
    }

    #[test]
    fn cached_impl_skips_another_crates_cached_path() {
        let out = scoped(
            TokenStream::new(),
            quote! { impl P { #[memo::cached] fn f() {} } },
        );
        assert!(!out.contains("scope"), "{out}");
    }

    #[test]
    fn cached_impl_scopes_a_renamed_crate_path() {
        let _target = autumn_macros_support::crate_path::set_target(Some("web"));
        let out = scoped(
            TokenStream::new(),
            quote! { impl P { #[web::cached] async fn f() {} } },
        );
        assert!(out.contains("# [web :: cached (scope = \"P\")]"), "{out}");
    }

    #[test]
    fn cached_impl_skips_a_third_party_cached_attribute() {
        let out = scoped(
            TokenStream::new(),
            quote! {
                impl Products {
                    #[cached::proc_macro::cached]
                    fn get(id: i64) -> String { String::new() }
                }
            },
        );
        assert!(!out.contains("scope"), "{out}");
    }

    fn scoped(attr: TokenStream, item: TokenStream) -> String {
        cached_impl_macro(attr, item, None).to_string()
    }

    #[test]
    fn cached_impl_accepts_a_trailing_comma() {
        let out = scoped(
            TokenStream::new(),
            quote! { impl P { #[cached(ttl = "5m",)] async fn f() {} } },
        );
        assert!(
            out.contains("# [cached (ttl = \"5m\" , scope = \"P\")]"),
            "{out}"
        );
    }

    #[test]
    fn cached_impl_keeps_an_explicit_scope_beside_a_crate_override() {
        let out = scoped(
            TokenStream::new(),
            quote! { impl P { #[cached(scope = "Mine", crate = "web")] async fn f() {} } },
        );
        assert_eq!(out.matches("scope").count(), 1, "{out}");
    }

    #[test]
    fn cached_impl_scopes_each_cached_method_by_the_self_type() {
        let out = scoped(
            TokenStream::new(),
            quote! {
                impl Products {
                    #[cached]
                    async fn get(id: i64) -> String { String::new() }
                    #[cached(ttl = "5m")]
                    async fn list() -> Vec<String> { Vec::new() }
                    async fn plain() {}
                }
            },
        );
        assert!(out.contains("# [cached (scope = \"Products\")]"), "{out}");
        assert!(
            out.contains("# [cached (ttl = \"5m\" , scope = \"Products\")]"),
            "{out}"
        );
        assert_eq!(out.matches("scope").count(), 2, "{out}");
    }

    #[test]
    fn cached_impl_handles_a_qualified_attribute_path() {
        let out = scoped(
            TokenStream::new(),
            quote! {
                impl Reviews {
                    #[autumn_web::cached]
                    async fn get(id: i64) -> String { String::new() }
                }
            },
        );
        assert!(
            out.contains("# [autumn_web :: cached (scope = \"Reviews\")]"),
            "{out}"
        );
    }

    #[test]
    fn cached_impl_keeps_an_explicit_scope() {
        let out = scoped(
            TokenStream::new(),
            quote! {
                impl Products {
                    #[cached(scope = "Mine")]
                    async fn get(id: i64) -> String { String::new() }
                }
            },
        );
        assert_eq!(out.matches("scope").count(), 1, "{out}");
        assert!(out.contains("\"Mine\""), "{out}");
    }

    #[test]
    fn cached_impl_names_a_generic_type_by_its_last_segment() {
        let out = scoped(
            TokenStream::new(),
            quote! {
                impl<T> crate::repo::Store<T> {
                    #[cached]
                    async fn get(id: i64) -> String { String::new() }
                }
            },
        );
        assert!(out.contains("scope = \"repo::Store\""), "{out}");
    }

    #[test]
    fn cached_impl_keeps_the_qualified_path_apart() {
        let a = scoped(
            TokenStream::new(),
            quote! { impl a::Store { #[cached] fn get() {} } },
        );
        let b = scoped(
            TokenStream::new(),
            quote! { impl b::Store { #[cached] fn get() {} } },
        );
        assert!(a.contains("scope = \"a::Store\""), "{a}");
        assert!(b.contains("scope = \"b::Store\""), "{b}");
    }

    #[test]
    fn cached_impl_rejects_a_non_impl_item() {
        let out = scoped(TokenStream::new(), quote! { fn f() {} });
        assert!(out.contains("compile_error"), "{out}");
    }

    #[test]
    fn cached_impl_rejects_a_type_without_a_name() {
        let out = scoped(
            TokenStream::new(),
            quote! { impl Tr for (i64, i64) { #[cached] fn f() {} } },
        );
        assert!(out.contains("compile_error"), "{out}");
    }
}

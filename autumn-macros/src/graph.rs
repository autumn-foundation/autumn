//! Architecture-graph descriptor emission (issue #1747).
//!
//! Every macro that declares an architectural element publishes a descriptor
//! here, so the framework can assemble the application's graph at link time.
//! Node identity comes from the declaration itself and is therefore complete.
//!
//! Edges are different: a route or job states which repository it takes as an
//! extractor, but it never states which model it touches — it just names one.
//! This module reads those names off the item's tokens and hands them over as
//! *candidates*. Nothing is resolved here, because a proc-macro sees one item
//! at a time and cannot know which names are models, tables or repositories;
//! the join happens in `autumn_web::graph::manifest`, against what actually got
//! declared.
//!
//! The collection is deliberately a superset. A missing edge is a false
//! negative in an impact answer — the one failure the feature cannot afford —
//! while a candidate that resolves to nothing is simply dropped at link time.
//! There is no stop-list of "obviously not a model" names for the same reason:
//! a name an app is free to choose is a name this module must not discard.

use proc_macro2::{Ident, Spacing, TokenStream, TokenTree};
use quote::quote;

/// SQL keywords that mark a string literal as worth scanning for table names.
///
/// The gate exists so a `maud` template's prose is not swept in: only literals
/// that read as SQL contribute identifiers.
const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "FROM", "JOIN", "INTO", "TRUNCATE",
];

/// SQL keywords that mark a literal as a mutation.
const SQL_MUTATIONS: &[&str] = &["INSERT", "UPDATE", "DELETE", "TRUNCATE"];

/// Identifiers that are evidence the item mutates something.
///
/// Names, not types — the same "provable subset" reading as
/// `RouteInfo::pools`. A handler that mutates through a helper in another
/// module reads as a read here, which is why `Access` is documented as the
/// declared intent rather than an executed statement.
const MUTATION_IDENTS: &[&str] = &[
    "create",
    "create_many",
    "delete",
    "delete_by_id",
    "delete_from",
    "insert",
    "insert_into",
    "save",
    "update",
    "update_by_id",
    "upsert",
];

/// Flatten a token stream into a single list, descending into every group.
///
/// Delimiters are dropped: this is a name census, and a name means the same
/// thing inside a block, a call's parentheses or a generic argument list.
fn flatten(stream: &TokenStream, out: &mut Vec<TokenTree>) {
    for tree in stream.clone() {
        match tree {
            // Delimiters are dropped rather than recorded: a `None`-delimited
            // group (the shape a nested macro expansion arrives in) is already
            // transparent to the compiler, and the others carry no name.
            TokenTree::Group(ref group) => flatten(&group.stream(), out),
            other => out.push(other),
        }
    }
}

/// Whether the tokens at `i` are a `::` path separator.
fn is_path_sep(tokens: &[TokenTree], i: usize) -> bool {
    matches!(tokens.get(i), Some(TokenTree::Punct(p))
        if p.as_char() == ':' && p.spacing() == Spacing::Joint)
        && matches!(tokens.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
}

/// Whether an identifier starts with an uppercase ASCII letter — the shape of
/// a Rust type, and so of a model or repository name.
fn is_type_shaped(ident: &Ident) -> bool {
    ident
        .to_string()
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
}

/// Identifier-shaped words inside a string literal.
fn sql_words(literal: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in literal.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Whether a string literal reads as SQL.
fn looks_like_sql(literal: &str) -> bool {
    let upper = literal.to_ascii_uppercase();
    SQL_KEYWORDS
        .iter()
        .any(|kw| upper.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == *kw))
}

/// Candidate names read off a token stream.
///
/// An identifier is a candidate when it is adjacent to a `::` path separator
/// (`posts::table`, `models::Post`, `Post::find`) or is type-shaped (`Post`,
/// `PgPostRepository`), plus every identifier-shaped word inside a string
/// literal that reads as SQL.
#[must_use]
pub fn candidate_symbols(stream: &TokenStream) -> Vec<String> {
    let mut tokens = Vec::new();
    flatten(stream, &mut tokens);

    let mut symbols: Vec<String> = Vec::new();
    for (i, tree) in tokens.iter().enumerate() {
        match tree {
            TokenTree::Ident(ident) => {
                let followed = is_path_sep(&tokens, i + 1);
                let preceded = i >= 2 && is_path_sep(&tokens, i - 2);
                if followed || preceded || is_type_shaped(ident) {
                    symbols.push(ident.to_string());
                }
            }
            TokenTree::Literal(literal) => {
                let text = literal.to_string();
                if looks_like_sql(&text) {
                    symbols.extend(sql_words(&text));
                }
            }
            TokenTree::Punct(_) | TokenTree::Group(_) => {}
        }
    }
    symbols.sort();
    symbols.dedup();
    symbols
}

/// Candidate names read off a function's parameter types.
#[must_use]
pub fn signature_symbols(sig: &syn::Signature) -> Vec<String> {
    let mut stream = TokenStream::new();
    for arg in &sig.inputs {
        if let syn::FnArg::Typed(pat_type) = arg {
            let ty = &pat_type.ty;
            stream.extend(quote! { #ty });
        }
    }
    candidate_symbols(&stream)
}

/// Whether a token stream carries evidence that the item mutates something.
#[must_use]
pub fn is_mutating(stream: &TokenStream) -> bool {
    let mut tokens = Vec::new();
    flatten(stream, &mut tokens);
    tokens.iter().any(|tree| match tree {
        TokenTree::Ident(ident) => {
            let name = ident.to_string();
            MUTATION_IDENTS.binary_search(&name.as_str()).is_ok()
        }
        TokenTree::Literal(literal) => {
            let upper = literal.to_string().to_ascii_uppercase();
            looks_like_sql(&literal.to_string())
                && SQL_MUTATIONS.iter().any(|kw| {
                    upper
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .any(|w| w == *kw)
                })
        }
        TokenTree::Punct(_) | TokenTree::Group(_) => false,
    })
}

/// Emit a `&'static [&'static str]` literal for a symbol list.
#[must_use]
pub fn emit_symbol_slice(symbols: &[String]) -> TokenStream {
    if symbols.is_empty() {
        quote! { &[] }
    } else {
        let items = symbols.iter().map(|s| s.as_str());
        quote! { &[#(#items),*] }
    }
}

/// Emit the `inventory::submit!` for a `#[route]`/`#[static_get]` handler.
#[must_use]
pub fn emit_route_descriptor(input_fn: &syn::ItemFn, method: &str, path: &TokenStream, static_route: bool) -> TokenStream {
    let signature = emit_symbol_slice(&signature_symbols(&input_fn.sig));
    let block = &input_fn.block;
    let body_tokens = quote! { #block };
    let body = emit_symbol_slice(&candidate_symbols(&body_tokens));
    let mutating = is_mutating(&body_tokens);
    let handler = input_fn.sig.ident.to_string();
    quote! {
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::graph::RouteGraphDescriptor {
                handler: #handler,
                module_path: ::core::module_path!(),
                method: #method,
                path: #path,
                static_route: #static_route,
                mutating: #mutating,
                file: ::core::file!(),
                line: ::core::line!(),
                signature_symbols: #signature,
                body_symbols: #body,
            }
        }
    }
}

/// Emit the `inventory::submit!` for a `#[job]`/`#[scheduled]`/`#[task]` handler.
#[must_use]
pub fn emit_job_descriptor(
    input_fn: &syn::ItemFn,
    name: &TokenStream,
    kind: &str,
    schedule: &TokenStream,
) -> TokenStream {
    let signature = emit_symbol_slice(&signature_symbols(&input_fn.sig));
    let block = &input_fn.block;
    let body_tokens = quote! { #block };
    let body = emit_symbol_slice(&candidate_symbols(&body_tokens));
    let mutating = is_mutating(&body_tokens);
    let handler = input_fn.sig.ident.to_string();
    let kind_ident = syn::Ident::new(kind, proc_macro2::Span::call_site());
    quote! {
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::graph::JobGraphDescriptor {
                name: #name,
                kind: ::autumn_web::graph::JobKind::#kind_ident,
                handler: #handler,
                module_path: ::core::module_path!(),
                schedule: #schedule,
                mutating: #mutating,
                file: ::core::file!(),
                line: ::core::line!(),
                signature_symbols: #signature,
                body_symbols: #body,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    #[test]
    fn mutation_idents_are_sorted_for_the_binary_search() {
        let mut sorted = MUTATION_IDENTS.to_vec();
        sorted.sort_unstable();
        assert_eq!(MUTATION_IDENTS, sorted.as_slice());
    }

    #[test]
    fn an_identifier_followed_by_a_path_separator_is_a_candidate() {
        let symbols = candidate_symbols(&quote! { posts::table.find(id) });
        assert!(symbols.contains(&"posts".to_owned()), "{symbols:?}");
    }

    #[test]
    fn an_identifier_preceded_by_a_path_separator_is_a_candidate() {
        let symbols = candidate_symbols(&quote! { crate::schema::subreddits::table });
        assert!(symbols.contains(&"subreddits".to_owned()), "{symbols:?}");
        assert!(symbols.contains(&"schema".to_owned()), "{symbols:?}");
    }

    #[test]
    fn a_type_shaped_identifier_is_a_candidate_even_standing_alone() {
        let symbols = candidate_symbols(&quote! { let items: Vec<Post> = load(); });
        assert!(symbols.contains(&"Post".to_owned()), "{symbols:?}");
    }

    #[test]
    fn a_lowercase_identifier_with_no_path_separator_is_not_a_candidate() {
        let symbols = candidate_symbols(&quote! { let posts = 1; });
        assert!(!symbols.contains(&"posts".to_owned()), "{symbols:?}");
    }

    #[test]
    fn candidates_are_found_inside_nested_groups() {
        let symbols = candidate_symbols(&quote! {
            if x { for y in z { let _ = posts::table; } }
        });
        assert!(symbols.contains(&"posts".to_owned()), "{symbols:?}");
    }

    #[test]
    fn a_table_named_only_in_raw_sql_is_a_candidate() {
        let symbols = candidate_symbols(&quote! {
            diesel::sql_query("UPDATE posts SET hot_rank = 0")
        });
        assert!(
            symbols.contains(&"posts".to_owned()),
            "a table reachable only through raw SQL must still be found: {symbols:?}"
        );
    }

    #[test]
    fn prose_string_literals_are_not_scanned() {
        let symbols = candidate_symbols(&quote! {
            let msg = "no database pool available for posts right now";
        });
        assert!(
            !symbols.contains(&"posts".to_owned()),
            "a literal with no SQL keyword must not contribute names: {symbols:?}"
        );
    }

    #[test]
    fn candidates_are_sorted_and_deduplicated() {
        let symbols = candidate_symbols(&quote! { Post::find(); Post::all(); Alpha::x(); });
        assert_eq!(
            symbols
                .iter()
                .filter(|s| s.as_str() == "Post")
                .count(),
            1,
            "{symbols:?}"
        );
        let mut sorted = symbols.clone();
        sorted.sort();
        assert_eq!(symbols, sorted);
    }

    #[test]
    fn signature_symbols_name_the_declared_extractors() {
        let input: syn::ItemFn = syn::parse_quote! {
            async fn show(mut db: Db, repo: PgPostRepository) -> Markup { todo!() }
        };
        let symbols = signature_symbols(&input.sig);
        assert!(symbols.contains(&"PgPostRepository".to_owned()), "{symbols:?}");
        assert!(symbols.contains(&"Db".to_owned()), "{symbols:?}");
    }

    #[test]
    fn signature_symbols_reach_inside_generic_arguments() {
        let input: syn::ItemFn = syn::parse_quote! {
            async fn create(form: ChangesetForm<NewPost>) -> Markup { todo!() }
        };
        let symbols = signature_symbols(&input.sig);
        assert!(symbols.contains(&"NewPost".to_owned()), "{symbols:?}");
    }

    #[test]
    fn a_binding_name_is_never_mistaken_for_an_extractor() {
        let input: syn::ItemFn = syn::parse_quote! {
            async fn show(posts: i64) -> Markup { todo!() }
        };
        assert!(
            !signature_symbols(&input.sig).contains(&"posts".to_owned()),
            "a parameter binding name must not be read as a type"
        );
    }

    #[test]
    fn mutation_is_detected_from_a_diesel_call() {
        assert!(is_mutating(&quote! { diesel::insert_into(posts::table) }));
        assert!(is_mutating(&quote! { repo.delete_by_id(id).await }));
    }

    #[test]
    fn mutation_is_detected_from_raw_sql() {
        assert!(is_mutating(&quote! { sql_query("UPDATE posts SET x = 1") }));
        assert!(!is_mutating(&quote! { sql_query("SELECT * FROM posts") }));
    }

    #[test]
    fn a_pure_read_is_not_reported_as_a_mutation() {
        assert!(!is_mutating(&quote! { posts::table.load(&mut db).await }));
    }

    #[test]
    fn an_empty_symbol_list_emits_an_empty_slice() {
        assert_eq!(
            emit_symbol_slice(&[]).to_string(),
            quote! { &[] }.to_string()
        );
    }

    #[test]
    fn the_route_descriptor_names_the_handler_and_its_symbols() {
        let input: syn::ItemFn = syn::parse_quote! {
            async fn show(repo: PgPostRepository) -> Markup {
                let _ = posts::table;
                todo!()
            }
        };
        let generated = emit_route_descriptor(&input, "GET", &quote! { "/posts" }, false)
            .to_string();
        assert!(generated.contains("RouteGraphDescriptor"), "{generated}");
        assert!(generated.contains(r#"handler : "show""#), "{generated}");
        assert!(generated.contains(r#"method : "GET""#), "{generated}");
        assert!(generated.contains("static_route : false"), "{generated}");
        assert!(generated.contains(r#""PgPostRepository""#), "{generated}");
        assert!(generated.contains(r#""posts""#), "{generated}");
    }

    #[test]
    fn the_job_descriptor_carries_its_kind_and_schedule() {
        let input: syn::ItemFn = syn::parse_quote! {
            async fn recalculate(state: AppState) -> AutumnResult<()> {
                diesel::sql_query("UPDATE posts SET hot_rank = 0");
                Ok(())
            }
        };
        let generated = emit_job_descriptor(
            &input,
            &quote! { "hot-rank" },
            "Scheduled",
            &quote! { "15m" },
        )
        .to_string();
        assert!(generated.contains("JobGraphDescriptor"), "{generated}");
        assert!(generated.contains("JobKind :: Scheduled"), "{generated}");
        assert!(generated.contains(r#"schedule : "15m""#), "{generated}");
        assert!(generated.contains("mutating : true"), "{generated}");
        assert!(generated.contains(r#""posts""#), "{generated}");
    }
}

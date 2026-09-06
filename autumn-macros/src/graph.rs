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

/// SQL statement shapes a string literal must match to be scanned for table
/// names.
///
/// A *shape*, not a keyword, because `FROM`, `INTO`, `JOIN`, `DELETE` and
/// `UPDATE` are ordinary English words. `hx-confirm="Delete this post? This
/// cannot be undone."` and `"… for auto-slug generation and logging on post
/// create/update"` are both real literals in this workspace's example app, and
/// a bare keyword test opens the whole string to the scan — minting table edges
/// for a page that touches no database and, worse, reporting a read-only job as
/// a writer, because mutation evidence is a *claim* rather than a superset.
///
/// Each entry is `(leading verb, required companion, mutating)`. The literal
/// must start with the verb and also carry the companion — which no prose in
/// the example app does, and every real statement does.
const SQL_SHAPES: &[(&str, &str, bool)] = &[
    ("SELECT", "FROM", false),
    // A `WITH` statement is read-only only if nothing in it mutates: the
    // statement after the CTEs, or a CTE body, can be an `INSERT`/`UPDATE`/
    // `DELETE`. `sql_shape` re-checks those verbs for this shape rather than
    // trusting the `false` here.
    ("WITH", "SELECT", false),
    ("INSERT", "INTO", true),
    ("UPDATE", "SET", true),
    ("DELETE", "FROM", true),
    ("TRUNCATE", "TABLE", true),
];

/// Verbs that make any statement containing them a mutation, wherever they
/// appear — used for `WITH`, whose leading verb says nothing about its effect.
const SQL_MUTATION_VERBS: &[&str] = &["DELETE", "INSERT", "TRUNCATE", "UPDATE"];

/// Identifiers that are evidence the item mutates something.
///
/// Two tiers, because the obvious names are not the framework's. `insert`,
/// `create`, `update` and `delete` are ubiquitous on `HashMap`, `Vec`, caches
/// and metrics, and `maud`'s `hx-delete=(…)` lexes the bare ident `delete` into
/// a read-only page — so those count only when written as a qualified path
/// (`diesel::update`, `diesel::delete`), which is how the query builder is
/// actually called. The unambiguous names count anywhere.
///
/// Under-claiming is the safe direction — the same reading `RouteInfo::pools`
/// carries. This decides an edge's `access`, never whether the edge exists.
const MUTATION_IDENTS: &[&str] = &[
    "create_many",
    "delete_by_id",
    "delete_from",
    "insert_into",
    "save",
    "update_by_id",
    "upsert",
];

/// Mutation names that count only when written as a qualified path.
const QUALIFIED_MUTATION_IDENTS: &[&str] = &["create", "delete", "insert", "update"];

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

/// The text a string literal carries, with its source sigils removed.
///
/// `Literal::to_string()` hands back the *source* form, so a raw string still
/// wears its `r#"…"#` and a byte string its `b`. Left in place they contribute
/// the candidate symbols `r` and `b`, and escape sequences leak `n`/`t`/`u`.
fn literal_text(literal: &str) -> String {
    let trimmed = literal.trim_start_matches(['b', 'r', 'c']);
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    let closing = format!("\"{}", "#".repeat(hashes));
    let inner = trimmed
        .trim_start_matches('#')
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix(&closing))
        .unwrap_or(trimmed);
    // Escapes are neutralised rather than decoded, so `\n` cannot contribute `n`.
    //
    // `\n` and `\r` are the exception: they become the newline they stand for
    // rather than a space, because `strip_sql_comments` needs line structure to
    // know where a `--` comment ends. Without this, SQL written as
    // `"-- why\nDELETE FROM posts"` has no line break at all by the time the
    // comment stripper sees it, and the stripper would swallow the statement.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n' | 'r') => out.push('\n'),
                // Any other escape is neutralised, and its letter dropped with
                // it, so `\t` cannot contribute the candidate symbol `t`.
                Some(_) | None => out.push(' '),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// A SQL statement with its comments removed.
///
/// Needed because the statement's *verb* decides its shape, and a statement is
/// perfectly ordinarily written with a comment above it:
///
/// ```sql
/// -- prune rows nobody can reach any more
/// DELETE FROM posts WHERE ...
/// ```
///
/// Left in, the first word is `PRUNE`, no [`SQL_SHAPES`] entry matches, and the
/// literal contributes neither table symbols nor mutation evidence — the `posts`
/// edge is simply lost. A dropped edge is the false negative this whole module
/// is built to avoid, so the comment has to go before the verb is read.
///
/// Quote-aware, and that is the point rather than a detail: `SELECT '--' FROM
/// posts` contains a `--` that starts no comment. Treating it as one would eat
/// the rest of the statement and lose the very edge this function exists to
/// keep. Single-quoted strings (with `''` escapes) and double-quoted
/// identifiers are therefore tracked, and block comments nest, as they do in
/// Postgres.
/// The Postgres dollar-quote delimiter starting at `i`, if there is one.
///
/// Returns the whole delimiter including both `$` — `$$`, `$body$`, `$tag_1$` —
/// so the caller can look for the identical closing run.
///
/// A tag follows identifier rules and so may not begin with a digit, which is
/// precisely what separates a real delimiter from the `$1`/`$2` bind
/// placeholders that appear throughout this workspace's SQL. Mistaking `$1` for
/// an opening quote would swallow the rest of the statement.
fn dollar_tag(chars: &[char], i: usize) -> Option<String> {
    if chars.get(i) != Some(&'$') {
        return None;
    }
    let mut end = i + 1;
    // An empty tag (`$$`) is valid; a non-empty one may not start with a digit.
    if let Some(first) = chars.get(end)
        && (first.is_ascii_alphabetic() || *first == '_')
    {
        end += 1;
        while chars
            .get(end)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            end += 1;
        }
    }
    (chars.get(end) == Some(&'$')).then(|| chars[i..=end].iter().collect())
}

fn strip_sql_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut block_depth = 0_u32;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if block_depth > 0 {
            match (ch, next) {
                ('/', Some('*')) => {
                    block_depth += 1;
                    i += 2;
                }
                ('*', Some('/')) => {
                    block_depth -= 1;
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        match (ch, next) {
            ('-', Some('-')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                // Keep the newline: it separates the words either side.
                out.push('\n');
            }
            ('/', Some('*')) => {
                block_depth = 1;
                i += 2;
                // A block comment separates whatever surrounds it.
                out.push(' ');
            }
            ('$', _) if dollar_tag(&chars, i).is_some() => {
                // Postgres dollar quoting: `$$…$$` or `$tag$…$tag$`. Same
                // hazard as the ordinary quotes below — `SELECT $$-- x$$ FROM
                // posts` carries a `--` that starts no comment, and eating the
                // rest of it would drop the `posts` edge.
                //
                // `dollar_tag` is what keeps this away from `$1`/`$2` bind
                // placeholders, which are far more common in this codebase than
                // dollar-quoted strings: a tag may not start with a digit, so
                // `$1` is not an opening delimiter and falls through to be
                // copied like any other character.
                let delimiter: String = dollar_tag(&chars, i).expect("guarded above");
                out.push_str(&delimiter);
                i += delimiter.chars().count();
                // Copy through to the matching closing delimiter. An unclosed
                // one runs to the end, which is the conservative direction:
                // the text is preserved rather than discarded.
                while i < chars.len() {
                    if chars[i] == '$'
                        && chars[i..]
                            .iter()
                            .take(delimiter.chars().count())
                            .copied()
                            .eq(delimiter.chars())
                    {
                        out.push_str(&delimiter);
                        i += delimiter.chars().count();
                        break;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
            }
            ('\'' | '"', _) => {
                // Copy the quoted run verbatim; nothing inside it is a comment.
                let quote = ch;
                out.push(quote);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == quote {
                        // `''` / `""` is an escaped quote, not the end.
                        if chars.get(i + 1) == Some(&quote) {
                            out.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
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
    // A bare number is never a table name, and a statement's literals would
    // otherwise contribute a handful of them.
    words.retain(|word| !word.chars().all(|c| c.is_ascii_digit()));
    words
}

/// The SQL statement shape a literal matches, if any; `true` when it mutates.
///
/// Word-boundary aware in both directions, so `"selection"` is not a `SELECT`
/// and `"Delete this post?"` is not a `DELETE` — it carries no `FROM`.
fn sql_shape(literal: &str) -> Option<bool> {
    let text = strip_sql_comments(&literal_text(literal)).to_ascii_uppercase();
    let mut words = text
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty());
    let first = words.next()?;
    let rest: Vec<&str> = words.collect();
    let (_, _, mutating) = SQL_SHAPES
        .iter()
        .find(|(verb, companion, _)| first == *verb && rest.contains(companion))?;
    // `WITH old AS (SELECT …) DELETE FROM posts …` is a write. Its leading verb
    // is `WITH`, so the table alone cannot say; the mutation verbs can.
    let mutating =
        *mutating || (first == "WITH" && rest.iter().any(|w| SQL_MUTATION_VERBS.contains(w)));
    Some(mutating)
}

/// Whether a string literal reads as a SQL statement.
fn looks_like_sql(literal: &str) -> bool {
    sql_shape(literal).is_some()
}

/// Candidate names read off a token stream that came from *SQL literals*.
///
/// Kept apart from the rest because only these may be matched against a table
/// name case-insensitively: an unquoted SQL identifier folds, a Rust type name
/// does not. Without the split, a DTO named `Posts` would resolve to the
/// `posts` table (Codex round 5).
#[must_use]
pub fn sql_candidate_symbols(stream: &TokenStream) -> Vec<String> {
    let mut tokens = Vec::new();
    flatten(stream, &mut tokens);
    sql_symbols_of(&tokens)
}

/// [`sql_candidate_symbols`] over an already-flattened stream.
fn sql_symbols_of(tokens: &[TokenTree]) -> Vec<String> {
    let mut symbols: Vec<String> = Vec::new();
    for tree in tokens {
        if let TokenTree::Literal(literal) = tree {
            let source = literal.to_string();
            if looks_like_sql(&source) {
                symbols.extend(sql_words(&strip_sql_comments(&literal_text(&source))));
            }
        }
    }
    symbols.sort();
    symbols.dedup();
    symbols
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
    symbols_of(&tokens)
}

/// [`candidate_symbols`] over an already-flattened stream.
fn symbols_of(tokens: &[TokenTree]) -> Vec<String> {
    let mut symbols: Vec<String> = Vec::new();
    for (i, tree) in tokens.iter().enumerate() {
        match tree {
            TokenTree::Ident(ident) => {
                let followed = is_path_sep(tokens, i + 1);
                let preceded = i >= 2 && is_path_sep(tokens, i - 2);
                if followed || preceded || is_type_shaped(ident) {
                    symbols.push(ident.to_string());
                }
            }
            TokenTree::Literal(literal) => {
                let source = literal.to_string();
                if looks_like_sql(&source) {
                    symbols.extend(sql_words(&strip_sql_comments(&literal_text(&source))));
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

/// Whether an already-flattened token stream carries evidence that the item
/// mutates something.
///
/// Takes the flattened form so a caller that needs both the symbols and this
/// answer pays for one walk. See [`MUTATION_IDENTS`] for why the ambiguous
/// names require a qualified path, and [`SQL_SHAPES`] for why a literal must
/// read as a statement.
fn mutates(tokens: &[TokenTree]) -> bool {
    tokens.iter().enumerate().any(|(i, tree)| match tree {
        TokenTree::Ident(ident) => {
            let name = ident.to_string();
            MUTATION_IDENTS.contains(&name.as_str())
                || (QUALIFIED_MUTATION_IDENTS.contains(&name.as_str())
                    && i >= 2
                    && is_path_sep(tokens, i - 2))
        }
        TokenTree::Literal(literal) => sql_shape(&literal.to_string()) == Some(true),
        TokenTree::Punct(_) | TokenTree::Group(_) => false,
    })
}

/// Emit a `&'static [&'static str]` literal for a symbol list.
#[must_use]
pub fn emit_symbol_slice(symbols: &[String]) -> TokenStream {
    if symbols.is_empty() {
        quote! { &[] }
    } else {
        let items = symbols.iter().map(String::as_str);
        quote! { &[#(#items),*] }
    }
}

/// Emit the `inventory::submit!` for a `#[route]`/`#[static_get]` handler.
#[must_use]
pub fn emit_route_descriptor(
    input_fn: &syn::ItemFn,
    method: &str,
    path: &TokenStream,
    static_route: bool,
) -> TokenStream {
    let signature = emit_symbol_slice(&signature_symbols(&input_fn.sig));
    let block = &input_fn.block;
    let body_tokens = quote! { #block };
    let body = emit_symbol_slice(&candidate_symbols(&body_tokens));
    let sql = emit_symbol_slice(&sql_candidate_symbols(&body_tokens));
    let handler = input_fn.sig.ident.to_string();
    quote! {
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::graph::RouteGraphDescriptor {
                handler: #handler,
                module_path: ::core::module_path!(),
                method: #method,
                path: #path,
                static_route: #static_route,
                file: ::core::file!(),
                line: ::core::line!(),
                signature_symbols: #signature,
                body_symbols: #body,
                sql_symbols: #sql,
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
    // One walk for both answers: the symbols and the mutation evidence come
    // from the same flattened stream.
    let mut flat = Vec::new();
    flatten(&body_tokens, &mut flat);
    let body = emit_symbol_slice(&symbols_of(&flat));
    let sql = emit_symbol_slice(&sql_symbols_of(&flat));
    let mutating = mutates(&flat);
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
                sql_symbols: #sql,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    /// [`mutates`] from a token stream, which is how every test spells it.
    fn is_mutating(stream: &TokenStream) -> bool {
        let mut tokens = Vec::new();
        flatten(stream, &mut tokens);
        mutates(&tokens)
    }

    #[test]
    fn ui_copy_that_reads_like_sql_is_not_scanned() {
        // Both of these are real literals in `examples/reddit-clone`. A bare
        // keyword test made `about` — a static page that touches no database —
        // claim edges to `post`, `create`, `update` and `delete`.
        for prose in [
            " for auto-slug generation and logging on post create/update",
            "Delete this post? This cannot be undone.",
            "{n} new posts from your subreddits",
        ] {
            let symbols = candidate_symbols(&quote! { let msg = #prose; });
            assert!(
                !symbols.iter().any(|s| s == "posts" || s == "post"),
                "prose must not open the SQL scan ({prose:?}): {symbols:?}"
            );
        }
    }

    #[test]
    fn ui_copy_that_reads_like_sql_is_not_mutation_evidence() {
        assert!(
            !is_mutating(&quote! { let label = "Delete this post? This cannot be undone."; }),
            "a page that renders the word Delete is not a writer"
        );
    }

    #[test]
    fn an_ambiguous_mutation_name_counts_only_as_a_qualified_path() {
        assert!(
            !is_mutating(&quote! { vars.insert("name", user.name) }),
            "`HashMap::insert` is not a database write"
        );
        assert!(
            !is_mutating(&quote! { div hx-delete=(paths::delete_post(&slug)) }),
            "maud lexes `hx-delete` into a bare `delete` ident"
        );
        assert!(
            is_mutating(&quote! { diesel::update(posts::table).set(x.eq(1)) }),
            "the query builder is called as a qualified path"
        );
        assert!(is_mutating(&quote! { diesel::delete(posts::table) }));
    }

    #[test]
    fn a_raw_string_contributes_no_sigil_symbols() {
        let symbols = candidate_symbols(&quote! { sql_query(r#"SELECT id FROM posts"#) });
        assert!(symbols.contains(&"posts".to_owned()), "{symbols:?}");
        assert!(!symbols.contains(&"r".to_owned()), "{symbols:?}");
    }

    #[test]
    fn numbers_inside_a_statement_are_not_candidates() {
        let symbols = candidate_symbols(&quote! {
            sql_query("UPDATE posts SET hot_rank = 3600 / 1.5")
        });
        assert!(symbols.contains(&"posts".to_owned()), "{symbols:?}");
        assert!(
            !symbols.iter().any(|s| s == "3600" || s == "1"),
            "{symbols:?}"
        );
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
    fn a_word_that_merely_contains_a_sql_keyword_does_not_open_the_scan() {
        let symbols = candidate_symbols(&quote! {
            let msg = "your selection of posts is ready";
        });
        assert!(
            !symbols.contains(&"posts".to_owned()),
            "`selection` must not read as `SELECT`: {symbols:?}"
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
            symbols.iter().filter(|s| s.as_str() == "Post").count(),
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
        assert!(
            symbols.contains(&"PgPostRepository".to_owned()),
            "{symbols:?}"
        );
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
    fn a_statement_verb_without_its_companion_is_not_sql() {
        assert!(!looks_like_sql("\"Delete this post?\""));
        assert!(looks_like_sql("\"DELETE FROM posts WHERE id = $1\""));
    }

    /// The SQL-derived candidate symbols of a single literal, written as it
    /// would appear in source (sigils and all).
    fn sql_symbols(source: &str) -> Vec<String> {
        let stream: TokenStream = source.parse().expect("a parseable literal");
        sql_candidate_symbols(&stream)
    }

    /// A comment above the statement is ordinary SQL style. Reading the verb off
    /// the comment's first word loses the shape, and with it every table symbol
    /// and the mutation evidence — a dropped edge, which is the one failure this
    /// module exists to prevent.
    #[test]
    fn a_leading_comment_does_not_hide_the_statement() {
        let escaped = "\"-- prune rows nobody can reach\\nDELETE FROM posts WHERE id = $1\"";
        assert_eq!(sql_shape(escaped), Some(true), "escaped newline");
        assert!(
            sql_symbols(escaped).contains(&"posts".to_owned()),
            "the table behind a comment must still be a candidate"
        );

        let raw = "r#\"-- prune rows nobody can reach\nDELETE FROM posts WHERE id = $1\"#";
        assert_eq!(sql_shape(raw), Some(true), "real newline");
        assert!(sql_symbols(raw).contains(&"posts".to_owned()));

        let block = "\"/* housekeeping */ SELECT id FROM posts\"";
        assert_eq!(sql_shape(block), Some(false), "block comment");
        assert!(sql_symbols(block).contains(&"posts".to_owned()));
    }

    /// The same hazard for Postgres dollar quoting, which the first version of
    /// the comment stripper missed: `$$…$$` and `$tag$…$tag$` are string
    /// literals, so a `--` inside one starts no comment.
    ///
    /// The `$1` case is the reason the tag rules matter. Bind placeholders are
    /// everywhere in this workspace's SQL, and treating `$1` as an opening
    /// delimiter would swallow the rest of every parameterised statement.
    #[test]
    fn dollar_quoted_text_is_not_scanned_for_comments() {
        let bare = "\"SELECT $$-- marker$$ FROM posts\"";
        assert_eq!(sql_shape(bare), Some(false));
        assert!(
            sql_symbols(bare).contains(&"posts".to_owned()),
            "a dollar-quoted -- must not eat the FROM clause"
        );

        let tagged = "\"SELECT $body$-- marker$body$ FROM posts\"";
        assert_eq!(sql_shape(tagged), Some(false));
        assert!(sql_symbols(tagged).contains(&"posts".to_owned()));

        // `$1` is a placeholder, not a delimiter.
        let placeholder = "\"DELETE FROM posts WHERE id = $1 AND slug = $2\"";
        assert_eq!(sql_shape(placeholder), Some(true));
        assert!(sql_symbols(placeholder).contains(&"posts".to_owned()));
    }

    /// The hazard the fix introduces if it is written naively: a `--` inside a
    /// quoted string starts no comment, and treating it as one would swallow the
    /// rest of the statement — losing exactly the edge the fix is meant to keep.
    #[test]
    fn a_quoted_double_dash_is_not_a_comment() {
        let literal = "\"SELECT id FROM posts WHERE slug = '--'\"";
        assert_eq!(sql_shape(literal), Some(false));
        assert!(
            sql_symbols(literal).contains(&"posts".to_owned()),
            "a quoted -- must not eat the FROM clause"
        );
    }

    /// Comments are not evidence. A `WITH` statement's mutation check scans the
    /// whole statement, so prose in a comment could otherwise report a read-only
    /// query as a write.
    #[test]
    fn a_mutation_verb_in_a_comment_is_not_mutation_evidence() {
        let literal =
            "\"WITH recent AS (SELECT id FROM posts) SELECT * FROM recent -- never DELETE\"";
        assert_eq!(sql_shape(literal), Some(false));
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
        let generated =
            emit_route_descriptor(&input, "GET", &quote! { "/posts" }, false).to_string();
        assert!(generated.contains("RouteGraphDescriptor"), "{generated}");
        assert!(generated.contains(r#"handler : "show""#), "{generated}");
        assert!(generated.contains(r#"method : "GET""#), "{generated}");
        assert!(generated.contains("static_route : false"), "{generated}");
        assert!(
            !generated.contains("mutating"),
            "a route's access comes from its declared HTTP method, so the descriptor \
             carries no mutation flag: {generated}"
        );
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

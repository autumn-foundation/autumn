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
//!
//! ## SQL scope
//!
//! A literal counts as SQL when a statement in it has a known shape. A literal
//! may hold many statements, split on `;` outside quotes. Each one counts. The
//! literal mutates if any statement mutates. Quoted text is blanked for the verb
//! scan only. Table symbols keep every word.
//!
//! Data access is in scope: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`,
//! `TRUNCATE`, `WITH`, `COPY` and bare `TABLE name`. DDL is out by design:
//! `CREATE TABLE`, `ALTER TABLE` and `DROP TABLE` belong to migrations, not to
//! data access. A route or job does not depend on a table through them.

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
/// Each entry is `(leading verb, required companions, mutating)`. The literal
/// must start with the verb and also carry every companion — which no prose in
/// the example app does, and every real statement does.
const SQL_SHAPES: &[(&str, &[&str], bool)] = &[
    ("SELECT", &["FROM"], false),
    // `WITH` is absent on purpose: it leads a statement whose effect and whose
    // required words both live in the CTEs it introduces, not in a fixed
    // companion. See [`is_cte_statement`].
    ("INSERT", &["INTO"], true),
    ("UPDATE", &["SET"], true),
    ("DELETE", &["FROM"], true),
    // Postgres 15's upsert. It is the one shape that names more than one
    // companion, and it can afford to: `INTO`, `USING`, `ON` and a `WHEN`
    // clause are all mandatory, so requiring the set costs no recall — while
    // any one of them alone is a word an English sentence starting "Merge …"
    // borrows freely ("merge into main", "merge using the newest revision").
    ("MERGE", &["INTO", "USING", "ON", "WHEN"], true),
    // `TRUNCATE` is absent on purpose: Postgres makes its `TABLE` keyword
    // optional, so it has no companion every real statement carries. It is
    // matched on its whole grammar instead — see [`TRUNCATE_CLAUSE_WORDS`].
];

/// Verbs that make any statement containing them a mutation, wherever they
/// appear — used for `WITH`, whose leading verb says nothing about its effect.
const SQL_MUTATION_VERBS: &[&str] = &["DELETE", "INSERT", "MERGE", "TRUNCATE", "UPDATE"];

/// Verbs that can head a CTE body or the statement that follows the CTEs.
///
/// Used only to tell a `WITH` statement from an English sentence: what makes
/// one a statement is that it actually runs something.
const SQL_STATEMENT_VERBS: &[&str] = &["DELETE", "INSERT", "MERGE", "SELECT", "UPDATE", "VALUES"];

/// The optional keywords a `TRUNCATE` statement may carry.
///
/// `TRUNCATE` cannot use the companion test the other shapes do, because
/// Postgres spells the companion optional:
///
/// ```text
/// TRUNCATE [ TABLE ] [ ONLY ] name [ * ] [, …]
///          [ RESTART IDENTITY | CONTINUE IDENTITY ] [ CASCADE | RESTRICT ]
/// ```
///
/// `TRUNCATE posts` is a complete statement, and demanding `TABLE` dropped it
/// — losing the table symbol *and* the mutation evidence, so a job that clears
/// a table showed no dependency on it at all (Codex round 15). A missing edge
/// is the one failure this module cannot afford.
///
/// What stands in for the companion is the rest of the grammar: every word
/// after the verb is either one of these keywords or a table name, and a table
/// name is an identifier. A word that is neither — a number, punctuation-free
/// prose that starts with a digit — says the literal is a sentence, not a
/// statement. That is a narrower net than the companion test, not a wider one
/// in every direction: a short all-identifier imperative ("Truncate long
/// titles") still reads as a statement and would claim mutation. The trade is
/// deliberate and one-sided — that literal has to lead a `#[job]` body to cost
/// anything, and it costs an over-stated `access` on an edge, where the missing
/// alternative costs the edge itself.
const TRUNCATE_CLAUSE_WORDS: &[&str] = &[
    "CASCADE", "CONTINUE", "IDENTITY", "ONLY", "RESTART", "RESTRICT", "TABLE",
];

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
    // `a$b$` is one identifier. A quote cannot start right after a name char.
    if i > 0 && (chars[i - 1].is_ascii_alphanumeric() || matches!(chars[i - 1], '_' | '$')) {
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

/// Whether the words following a leading `WITH` are a CTE statement rather than
/// the rest of an English sentence.
///
/// The `WITH` shape used to demand a `SELECT`, which quietly assumed a CTE
/// statement reads something. It need not: `WITH removed AS (DELETE FROM posts
/// RETURNING id) UPDATE counters SET value = 0` writes twice and reads nothing,
/// and demanding `SELECT` threw the whole literal away — both tables lost, not
/// merely mis-accessed (Codex round 17).
///
/// What is actually invariant is the CTE binding: every CTE names itself with
/// `AS`, and every CTE statement runs at least one [`SQL_STATEMENT_VERBS`]
/// verb. Requiring the pair costs no recall — no valid `WITH` statement omits
/// either — and a sentence starting "With …" that borrows one alone ("with the
/// draft saved as a copy", "with luck the update lands") still reads as prose.
fn is_cte_statement(rest: &[&str]) -> bool {
    rest.contains(&"AS") && rest.iter().any(|w| SQL_STATEMENT_VERBS.contains(w))
}

/// Whether the words following a leading `TRUNCATE` are a statement's operands
/// rather than the rest of an English sentence.
///
/// True when every word is a [`TRUNCATE_CLAUSE_WORDS`] keyword or an
/// identifier, and at least one is a name rather than a keyword — a statement
/// truncates something.
fn is_truncate_operands(rest: &[&str]) -> bool {
    let mut named_a_table = false;
    for word in rest {
        if TRUNCATE_CLAUSE_WORDS.contains(word) {
            continue;
        }
        // Words are already split on everything but `[A-Z0-9_]`, so the only
        // way one fails to be an identifier is by leading with a digit.
        if !word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return false;
        }
        named_a_table = true;
    }
    named_a_table
}

/// Split comment-free SQL into statements for the verb scan.
///
/// Splits on `;` outside quotes. Blanks the text inside single quotes and
/// dollar quotes, so a verb word in a string is never read as a verb. Keeps
/// double-quoted identifiers as they are. An unterminated quote runs to the end
/// of the text.
///
/// Returns `(blanked, raw)` for each statement. Only the verb scan reads the
/// blanked text. Table symbols come from the raw text, so a quoted word is still
/// a candidate and no edge is lost.
fn verb_statements(text: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == ';' {
            statements.push((
                std::mem::take(&mut current),
                chars[start..i].iter().collect(),
            ));
            i += 1;
            start = i;
        } else if ch == '\'' {
            current.push(' ');
            i += 1;
            while i < chars.len() {
                // `''` is an escaped quote. Both halves stay inside the string.
                let closes = chars[i] == '\'' && chars.get(i + 1) != Some(&'\'');
                let escaped = chars[i] == '\'' && chars.get(i + 1) == Some(&'\'');
                i += if escaped { 2 } else { 1 };
                if closes {
                    break;
                }
            }
        } else if ch == '"' {
            current.push(ch);
            i += 1;
            while i < chars.len() {
                current.push(chars[i]);
                i += 1;
                if chars[i - 1] == '"' {
                    if chars.get(i) == Some(&'"') {
                        current.push('"');
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
        } else if let Some(tag) = dollar_tag(&chars, i) {
            let width = tag.chars().count();
            current.push(' ');
            i += width;
            while i < chars.len() && !chars[i..].iter().take(width).copied().eq(tag.chars()) {
                i += 1;
            }
            i = (i + width).min(chars.len());
        } else {
            current.push(ch);
            i += 1;
        }
    }
    statements.push((current, chars[start.min(chars.len())..].iter().collect()));
    statements
}

/// Words that can follow the name in `TABLE name`, as in `SELECT * FROM name`.
const TABLE_TRAILING_CLAUSES: &[&str] = &[
    "EXCEPT",
    "FETCH",
    "FOR",
    "INTERSECT",
    "LIMIT",
    "OFFSET",
    "ORDER",
    "UNION",
];

/// Whether the text after a leading `TABLE` is one table name.
///
/// The statement is `TABLE [ONLY] name`, with the clauses of a `SELECT` after
/// it. A name has no space in it, and any word after it must be a clause
/// keyword, so prose like "Table of contents" fails.
fn is_table_operand(rest: &str) -> bool {
    let rest = rest.trim();
    let rest = match rest.strip_prefix("ONLY") {
        Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
        _ => rest,
    };
    let (name, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let tail_ok = tail
        .split_whitespace()
        .next()
        .is_none_or(|word| TABLE_TRAILING_CLAUSES.contains(&word));
    name != "ONLY"
        && tail_ok
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '"')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '"' | '*'))
}

/// Whether the text of a `COPY` statement reads as a statement, and if so
/// whether it writes.
///
/// `COPY x FROM` loads rows into a table. `COPY x TO` and `COPY (query) TO`
/// only read. The direction is the first top-level `FROM` or `TO`: words inside
/// parentheses (a column list or a query) and inside double quotes do not
/// count, because `COPY "to" FROM STDIN` is a load.
fn copy_shape(rest_text: &str) -> Option<bool> {
    let (writes, start, end) = copy_direction(rest_text)?;
    // Prose ("Copy link from here") has no `COPY` target and no data source.
    let target = rest_text[..start].split_whitespace().next()?;
    let source = rest_text[end..].split_whitespace().next();
    let valid_source = source.is_none_or(|w| {
        matches!(
            w.trim_matches(|c: char| !c.is_ascii_alphanumeric()),
            "STDIN"
                | "STDOUT"
                | "PROGRAM"
                | ""
                | "WITH"
                | "WHERE"
                | "CSV"
                | "BINARY"
                | "TEXT"
                | "DELIMITER"
                | "NULL"
                | "HEADER"
                | "QUOTE"
                | "ESCAPE"
                | "FORMAT"
                | "FREEZE"
                | "ENCODING"
        )
    });
    (valid_source && target != "TO" && target != "FROM").then_some(writes)
}

/// The direction of a `COPY` statement and where its keyword sits in the text.
fn copy_direction(rest_text: &str) -> Option<(bool, usize, usize)> {
    let mut depth = 0_u32;
    let mut in_quote = false;
    let mut word_start = 0;
    let mut in_word = false;
    // A trailing space ends the last word.
    for (at, ch) in rest_text
        .char_indices()
        .chain(std::iter::once((rest_text.len(), ' ')))
    {
        if ch == '"' {
            in_quote = !in_quote;
            in_word = false;
        } else if in_quote {
            // Quoted identifier text never names the direction.
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            if depth == 0 && !in_word {
                in_word = true;
                word_start = at;
            }
        } else {
            if in_word {
                match &rest_text[word_start..at] {
                    "FROM" => return Some((true, word_start, at)),
                    "TO" => return Some((false, word_start, at)),
                    _ => {}
                }
                in_word = false;
            }
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    None
}

/// The shape of one statement; `true` when it mutates.
///
/// Word-boundary aware in both directions, so `"selection"` is not a `SELECT`
/// and `"Delete this post?"` is not a `DELETE` — it carries no `FROM`.
fn statement_shape(statement: &str) -> Option<bool> {
    let text = statement.to_ascii_uppercase();
    let mut words = text
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty());
    let first = words.next()?;
    let rest: Vec<&str> = words.collect();
    // After the verb, in source form: `COPY` and `TABLE` read the raw text.
    let rest_text = text.trim_start().get(first.len()..).unwrap_or("");
    // Some verbs are matched on their grammar rather than a companion.
    if first == "TRUNCATE" {
        return is_truncate_operands(&rest).then_some(true);
    }
    if first == "COPY" {
        return copy_shape(rest_text);
    }
    if first == "TABLE" {
        return is_table_operand(rest_text).then_some(false);
    }
    if first == "WITH" {
        // A CTE statement writes if anything in it does — a CTE body and the
        // statement after the CTEs can each be an `INSERT`/`UPDATE`/`DELETE`/
        // `MERGE`, and the leading `WITH` says nothing either way.
        return is_cte_statement(&rest)
            .then(|| rest.iter().any(|w| SQL_MUTATION_VERBS.contains(w)));
    }
    let (_, _, mutating) = SQL_SHAPES.iter().find(|(verb, companions, _)| {
        first == *verb && companions.iter().all(|c| rest.contains(c))
    })?;
    Some(*mutating)
}

/// The SQL shape a literal matches, if any; `true` when it mutates.
///
/// Classifies each statement of the literal. The literal mutates if any
/// recognised statement does. It reads as SQL if any statement has a shape.
fn sql_shape(literal: &str) -> Option<bool> {
    let text = strip_sql_comments(&literal_text(literal));
    verb_statements(&text)
        .iter()
        .filter_map(|(blanked, _)| statement_shape(blanked))
        .reduce(|a, b| a || b)
}

/// The tables each mutating statement writes, from a SQL literal.
///
/// Only the target of the write counts: `INSERT INTO audit SELECT * FROM users`
/// writes `audit` and only reads `users`. Reads the raw text, so a quoted name
/// is a symbol as it is for the candidate list. A mutating statement with no
/// target found gives all its words, so a write is never lost. Empty when no
/// statement mutates.
fn mutating_statement_words(literal: &str) -> Vec<String> {
    let text = strip_sql_comments(&literal_text(literal));
    verb_statements(&text)
        .iter()
        .filter(|(blanked, _)| statement_shape(blanked) == Some(true))
        .flat_map(|(blanked, raw)| {
            // The blanked text has no quoted values, so `SET note = 'UPDATE x'`
            // names no target.
            let targets = write_targets(blanked);
            if targets.is_empty() {
                sql_words(raw)
            } else {
                targets
            }
        })
        .collect()
}

/// Names that follow a write verb in one statement.
///
/// `INSERT INTO`, `UPDATE`, `DELETE FROM`, `MERGE INTO`, `COPY` (a load), and
/// every name after `TRUNCATE`. A `WITH` statement can hold several. A
/// schema-qualified name gives both parts.
fn write_targets(statement: &str) -> Vec<String> {
    let tokens: Vec<&str> = statement
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '"')))
        .filter(|t| !t.is_empty())
        .collect();
    let upper = |i: usize| tokens.get(i).map(|t| t.to_ascii_uppercase());
    let mut targets: Vec<String> = Vec::new();
    let mut take = |i: usize| {
        let mut i = i;
        if upper(i).as_deref() == Some("ONLY") {
            i += 1;
        }
        if let Some(name) = tokens.get(i) {
            targets.extend(
                name.split('.')
                    .map(|part| part.trim_matches('"').to_owned())
                    .filter(|part| !part.is_empty()),
            );
        }
    };
    for i in 0..tokens.len() {
        let word = upper(i);
        match word.as_deref() {
            Some("INTO")
                if matches!(
                    upper(i.wrapping_sub(1)).as_deref(),
                    Some("INSERT" | "MERGE")
                ) =>
            {
                take(i + 1);
            }
            Some("FROM") if upper(i.wrapping_sub(1)).as_deref() == Some("DELETE") => take(i + 1),
            Some("UPDATE" | "COPY") => take(i + 1),
            Some("TRUNCATE") => {
                let mut j = i + 1;
                while let Some(next) = upper(j) {
                    if TRUNCATE_CLAUSE_WORDS.contains(&next.as_str()) {
                        j += 1;
                        continue;
                    }
                    if next.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '"') {
                        take(j);
                        j += 1;
                    } else {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    targets
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

/// Whether an already-flattened token stream carries mutation evidence that
/// names no symbol.
///
/// SQL evidence is not here: [`mutating_sql_symbols`] names its symbols.
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
        // A mutating statement with a `{}` placeholder names its table at run
        // time. No symbol can carry that write, so it counts for every edge.
        TokenTree::Literal(literal) => has_placeholder_write(&literal.to_string()),
        TokenTree::Punct(_) | TokenTree::Group(_) => false,
    })
}

/// Whether a literal holds a mutating statement with a `{` placeholder.
fn has_placeholder_write(literal: &str) -> bool {
    let text = strip_sql_comments(&literal_text(literal));
    verb_statements(&text)
        .iter()
        .any(|(blanked, _)| statement_shape(blanked) == Some(true) && blanked.contains('{'))
}

/// Symbols of the mutating SQL statements in a flattened stream.
///
/// An edge to one of them is a write. Other symbols of the item stay reads.
fn mutating_sql_symbols(tokens: &[TokenTree]) -> Vec<String> {
    let mut symbols: Vec<String> = tokens
        .iter()
        .filter_map(|tree| match tree {
            TokenTree::Literal(literal) => Some(mutating_statement_words(&literal.to_string())),
            _ => None,
        })
        .flatten()
        .collect();
    symbols.sort();
    symbols.dedup();
    symbols
}

/// Names directly before `.create`, `.delete`, `.insert` or `.update`.
///
/// A name alone is no proof of a write: `HashMap::insert` has the same shape.
/// The manifest keeps a name only when it is a parameter whose type is a
/// declared repository. The macro cannot know that.
fn dot_mutation_receivers(tokens: &[TokenTree]) -> Vec<String> {
    let is_dot =
        |i: usize| matches!(tokens.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '.');
    let mut receivers: Vec<String> = Vec::new();
    for (i, tree) in tokens.iter().enumerate() {
        let TokenTree::Ident(verb) = tree else {
            continue;
        };
        if i >= 2
            && is_dot(i - 1)
            && QUALIFIED_MUTATION_IDENTS.contains(&verb.to_string().as_str())
            && let Some(TokenTree::Ident(receiver)) = tokens.get(i - 2)
        {
            receivers.push(receiver.to_string());
        }
    }
    receivers.sort();
    receivers.dedup();
    receivers
}

/// The name a parameter pattern binds, if it binds exactly one.
///
/// `repo`, `mut repo` and `State(repo)` bind `repo`.
fn bound_name(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Ident(ident) => Some(ident.ident.to_string()),
        syn::Pat::TupleStruct(tuple) if tuple.elems.len() == 1 => bound_name(&tuple.elems[0]),
        syn::Pat::Reference(reference) => bound_name(&reference.pat),
        syn::Pat::Paren(paren) => bound_name(&paren.pat),
        _ => None,
    }
}

/// `(parameter, type symbol)` pairs of a signature.
///
/// One pair for each candidate symbol of the parameter's type. The manifest
/// uses them to tell whether a `.update` receiver is a repository.
#[must_use]
pub fn signature_bindings(sig: &syn::Signature) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for arg in &sig.inputs {
        if let syn::FnArg::Typed(pat_type) = arg
            && let Some(name) = bound_name(&pat_type.pat)
        {
            let ty = &pat_type.ty;
            for symbol in candidate_symbols(&quote! { #ty }) {
                pairs.push((name.clone(), symbol));
            }
        }
    }
    pairs
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

/// Emit a `&'static [(&'static str, &'static str)]` literal for binding pairs.
fn emit_binding_slice(pairs: &[(String, String)]) -> TokenStream {
    let items = pairs.iter().map(|(a, b)| quote! { (#a, #b) });
    quote! { &[#(#items),*] }
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
    let mutating_symbols = emit_symbol_slice(&mutating_sql_symbols(&flat));
    let receivers = emit_symbol_slice(&dot_mutation_receivers(&flat));
    let bindings = emit_binding_slice(&signature_bindings(&input_fn.sig));
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
                mutating_symbols: #mutating_symbols,
                file: ::core::file!(),
                line: ::core::line!(),
                signature_symbols: #signature,
                signature_bindings: #bindings,
                dot_mutation_receivers: #receivers,
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

    /// Any mutation evidence, attributed or not, from a token stream.
    fn is_mutating(stream: &TokenStream) -> bool {
        let mut tokens = Vec::new();
        flatten(stream, &mut tokens);
        mutates(&tokens) || !mutating_sql_symbols(&tokens).is_empty()
    }

    fn flat(stream: &TokenStream) -> Vec<TokenTree> {
        let mut tokens = Vec::new();
        flatten(stream, &mut tokens);
        tokens
    }

    #[test]
    fn mutating_sql_names_the_symbols_of_the_writing_statement_only() {
        let tokens = flat(&quote! {
            batch_execute("SELECT * FROM users; INSERT INTO audit_events (kind) VALUES ('x')")
        });
        let symbols = mutating_sql_symbols(&tokens);
        assert!(symbols.contains(&"audit_events".to_owned()), "{symbols:?}");
        assert!(!symbols.contains(&"users".to_owned()), "{symbols:?}");
        assert!(!mutates(&tokens), "SQL evidence is attributed, not loose");
    }

    #[test]
    fn mutating_sql_keeps_quoted_words_of_the_writing_statement() {
        let tokens = flat(&quote! { sql_query("UPDATE \"Posts\" SET a = 'b; c' WHERE x = 1") });
        let symbols = mutating_sql_symbols(&tokens);
        assert!(symbols.contains(&"Posts".to_owned()), "{symbols:?}");
    }

    #[test]
    fn a_read_only_statement_names_no_mutating_symbols() {
        let tokens = flat(&quote! { sql_query("SELECT * FROM users") });
        assert!(mutating_sql_symbols(&tokens).is_empty());
    }

    #[test]
    fn evidence_that_names_no_symbol_stays_unattributed() {
        assert!(mutates(&flat(&quote! { repo.save(&post).await })));
        assert!(mutates(&flat(&quote! { diesel::update(posts::table) })));
    }

    #[test]
    fn dot_receivers_are_the_names_before_a_mutation_verb() {
        let tokens = flat(&quote! {
            repo.update(id, &patch).await?;
            vars.insert("a", 1);
            other.create(&b);
            q.delete(1);
            let x = repo.find(1);
            div hx-delete=(paths::delete_post(&slug)) {}
        });
        let mut receivers = dot_mutation_receivers(&tokens);
        receivers.sort();
        assert_eq!(receivers, ["other", "q", "repo", "vars"]);
    }

    #[test]
    fn signature_bindings_pair_a_parameter_with_each_type_symbol() {
        let sig: syn::Signature = syn::parse_quote! {
            fn run(mut repo: PgPostRepository, State(users): State<PgUserRepository>, n: u32)
        };
        let mut pairs = signature_bindings(&sig);
        pairs.sort();
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("repo", "PgPostRepository"),
                ("users", "PgUserRepository"),
                ("users", "State"),
            ]
        );
    }

    #[test]
    fn a_job_descriptor_carries_the_new_evidence() {
        let item: syn::ItemFn = syn::parse_quote! {
            async fn go(repo: PgPostRepository) {
                repo.update(1, &p).await;
                sql_query("INSERT INTO audit_events (a) VALUES (1)");
            }
        };
        let generated =
            emit_job_descriptor(&item, &quote! { "go" }, "Job", &quote! { "" }).to_string();
        for needle in [
            "mutating_symbols",
            "audit_events",
            "signature_bindings",
            "dot_mutation_receivers",
            "\"repo\"",
        ] {
            assert!(generated.contains(needle), "{needle}: {generated}");
        }
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

    /// A CTE statement need not read anything. `WITH removed AS (DELETE FROM
    /// posts RETURNING id) UPDATE counters SET value = 0` is valid Postgres and
    /// mutates two tables, but the `WITH` shape demanded a `SELECT`, so the
    /// literal was rejected before the mutation scan ever ran — losing both
    /// tables, not merely mis-stating their access (Codex round 17).
    #[test]
    fn a_write_only_cte_statement_is_still_a_statement() {
        let literal = "\"WITH removed AS (DELETE FROM posts RETURNING id) \
                       UPDATE counters SET value = 0\"";
        assert_eq!(sql_shape(literal), Some(true), "both halves write");
        let symbols = sql_symbols(literal);
        assert!(symbols.contains(&"posts".to_owned()), "the CTE's table");
        assert!(symbols.contains(&"counters".to_owned()), "the outer table");
    }

    /// What replaces the `SELECT` companion. Every CTE binds its name with
    /// `AS`, and every CTE statement runs at least one statement verb, so the
    /// pair is what a sentence starting "With …" has to clear — and prose that
    /// borrows one of them alone does not.
    #[test]
    fn a_with_sentence_is_not_a_statement() {
        assert_eq!(
            sql_shape("\"With the draft saved as a copy\""),
            None,
            "`as` without a statement verb is prose"
        );
        assert_eq!(
            sql_shape("\"With luck the update lands before the deadline\""),
            None,
            "a statement verb without `AS` is prose"
        );
    }

    /// Postgres 15 added `MERGE`, the upsert every other dialect spells
    /// differently. Its verb was absent from the shape table, so a literal that
    /// named its table only there matched nothing — no symbol, no mutation
    /// evidence, no edge (Codex round 16).
    #[test]
    fn merge_is_a_statement_and_a_mutation() {
        let literal = "\"MERGE INTO posts USING staged ON posts.id = staged.id \
                       WHEN MATCHED THEN UPDATE SET title = staged.title \
                       WHEN NOT MATCHED THEN INSERT VALUES (staged.id, staged.title)\"";
        assert_eq!(sql_shape(literal), Some(true), "MERGE always writes");
        let symbols = sql_symbols(literal);
        assert!(symbols.contains(&"posts".to_owned()), "the merge target");
        assert!(symbols.contains(&"staged".to_owned()), "the merge source");
    }

    /// `MERGE` leans on all four of its mandatory keywords rather than one
    /// companion, because "merge" leads ordinary sentences that borrow `INTO`
    /// and `USING` — a real statement always carries `ON` and a `WHEN` clause
    /// too, so requiring the set costs no recall and keeps prose out.
    #[test]
    fn a_merge_sentence_is_not_a_statement() {
        assert_eq!(
            sql_shape("\"Merge into main using the newest revision\""),
            None,
            "prose must not be scanned, and must not claim mutation"
        );
        assert_eq!(
            sql_shape("\"Merge the draft into the published post\""),
            None
        );
    }

    /// A `MERGE` behind CTEs is still a write. Its leading verb is `WITH`, so
    /// the mutation-verb scan is what has to say so.
    #[test]
    fn a_merge_behind_ctes_is_still_a_mutation() {
        let literal = "\"WITH staged AS (SELECT id, title FROM imports) \
                       MERGE INTO posts USING staged ON posts.id = staged.id \
                       WHEN MATCHED THEN DO NOTHING\"";
        assert_eq!(sql_shape(literal), Some(true));
    }

    /// Postgres makes `TABLE` optional, so `TRUNCATE posts` is a complete
    /// statement. Requiring the companion dropped the whole literal — no table
    /// symbol and no mutation evidence, so the job lost its table dependency
    /// outright. A missing edge is the one failure this module cannot afford
    /// (Codex round 15).
    #[test]
    fn truncate_without_the_optional_table_keyword_is_still_a_statement() {
        let bare = "\"TRUNCATE posts\"";
        assert_eq!(sql_shape(bare), Some(true), "bare TRUNCATE");
        assert!(
            sql_symbols(bare).contains(&"posts".to_owned()),
            "the truncated table must still be a candidate"
        );

        // The spelled-out form and every optional clause keep working.
        let spelled = "\"TRUNCATE TABLE posts\"";
        assert_eq!(sql_shape(spelled), Some(true), "TRUNCATE TABLE");
        assert!(sql_symbols(spelled).contains(&"posts".to_owned()));

        let clauses = "\"TRUNCATE ONLY posts, comments RESTART IDENTITY CASCADE\"";
        assert_eq!(sql_shape(clauses), Some(true), "every optional clause");
        let symbols = sql_symbols(clauses);
        assert!(symbols.contains(&"posts".to_owned()));
        assert!(symbols.contains(&"comments".to_owned()));
    }

    /// What replaces the companion test. Without `TABLE` to lean on, the guard
    /// against prose is the rest of the grammar: every word after `TRUNCATE` in
    /// a real statement is a clause keyword or a table name, so a word that can
    /// be neither says the literal is a sentence.
    #[test]
    fn a_truncate_sentence_is_not_a_statement() {
        assert_eq!(
            sql_shape("\"Truncate the summary to 200 characters\""),
            None,
            "prose must not be scanned, and must not claim mutation"
        );
        assert_eq!(
            sql_shape("\"Truncate a title that runs past 80 chars\""),
            None
        );
    }

    /// Text inside single quotes or dollar quotes is data. A verb word in it
    /// must not make a CTE statement a writer.
    #[test]
    fn a_cte_verb_inside_quoted_text_is_not_mutation_evidence() {
        let single = "\"WITH a AS (SELECT 'DELETE' FROM posts) SELECT * FROM a\"";
        assert_eq!(sql_shape(single), Some(false), "single quotes");
        let dollar = "\"WITH a AS (SELECT $$UPDATE$$ FROM posts) SELECT * FROM a\"";
        assert_eq!(sql_shape(dollar), Some(false), "dollar quotes");
        let tagged = "\"WITH a AS (SELECT $t$ delete $t$ FROM posts) SELECT * FROM a\"";
        assert_eq!(sql_shape(tagged), Some(false), "tagged dollar quotes");
        let real = "\"WITH a AS (DELETE FROM posts RETURNING 'x') SELECT * FROM a\"";
        assert_eq!(sql_shape(real), Some(true), "a real verb still counts");
    }

    /// Only the verb scan ignores quoted text. Symbols keep every word.
    #[test]
    fn quoted_words_stay_table_symbols() {
        let literal = "\"SELECT 'tags' FROM posts\"";
        let symbols = sql_symbols(literal);
        assert!(symbols.contains(&"tags".to_owned()));
        assert!(symbols.contains(&"posts".to_owned()));
    }

    /// An unterminated quote runs to the end of the text.
    #[test]
    fn an_unterminated_quote_is_treated_as_a_string() {
        let literal = "\"WITH a AS (SELECT 'DELETE FROM posts) SELECT 1\"";
        assert_eq!(sql_shape(literal), Some(false));
        let dollar = "\"WITH a AS (SELECT $$ UPDATE x) SELECT 1\"";
        assert_eq!(sql_shape(dollar), Some(false));
    }

    /// A literal can hold many statements. Each one counts.
    #[test]
    fn every_statement_of_a_multi_statement_literal_is_classified() {
        let read_first = "\"SELECT id FROM posts; DELETE FROM comments\"";
        assert_eq!(sql_shape(read_first), Some(true));
        let wrapped = "\"BEGIN; UPDATE posts SET a = 1; COMMIT\"";
        assert_eq!(sql_shape(wrapped), Some(true));
        assert!(looks_like_sql(wrapped));
        let reads = "\"SELECT 1; SELECT id FROM posts\"";
        assert_eq!(sql_shape(reads), Some(false));
        let commented = "\"-- note; more\nSELECT 1; DELETE FROM t\"";
        assert_eq!(sql_shape(commented), Some(true));
        assert!(is_mutating(
            &quote! { sql_query("SELECT 1 FROM a; INSERT INTO b VALUES (1)") }
        ));
    }

    /// A `;` inside a quote does not end a statement.
    #[test]
    fn a_semicolon_inside_a_quote_does_not_split() {
        let single = "\"SELECT id FROM posts WHERE a = ';DELETE FROM x'\"";
        assert_eq!(sql_shape(single), Some(false));
        let dollar = "\"SELECT id FROM posts WHERE a = $$; DELETE FROM x$$\"";
        assert_eq!(sql_shape(dollar), Some(false));
        let ident = r##"r#"SELECT "a;b" FROM posts"#"##;
        assert_eq!(sql_shape(ident), Some(false));
    }

    /// Prose with a `;` stays prose.
    #[test]
    fn prose_with_a_semicolon_is_not_sql() {
        assert_eq!(
            sql_shape("\"Delete this post; this cannot be undone\""),
            None
        );
        assert!(!is_mutating(
            &quote! { "Delete this post; this cannot be undone" }
        ));
    }

    /// `COPY x FROM` writes. `COPY x TO` reads. Both name a table.
    #[test]
    fn copy_is_a_statement_and_mutates_only_from() {
        let from = "\"COPY posts FROM STDIN\"";
        assert_eq!(sql_shape(from), Some(true));
        assert!(sql_symbols(from).contains(&"posts".to_owned()));
        let cols = "\"COPY posts (id, title) FROM '/tmp/x.csv'\"";
        assert_eq!(sql_shape(cols), Some(true));
        let to = "\"COPY posts TO STDOUT\"";
        assert_eq!(sql_shape(to), Some(false));
        assert!(sql_symbols(to).contains(&"posts".to_owned()));
        let query = "\"COPY (SELECT id FROM posts) TO STDOUT\"";
        assert_eq!(sql_shape(query), Some(false));
        assert!(sql_symbols(query).contains(&"posts".to_owned()));
        assert_eq!(sql_shape("\"Copy the link\""), None);
    }

    /// Only the write target of a mutating statement is a write symbol.
    #[test]
    fn a_write_symbol_is_the_target_not_the_source() {
        let words = |sql: &str| mutating_statement_words(&format!("{sql:?}"));
        let insert = words("INSERT INTO audit_events SELECT id FROM users");
        assert_eq!(insert, vec!["audit_events".to_owned()]);
        let update = words("UPDATE posts SET n = 1 FROM users WHERE users.id = posts.uid");
        assert_eq!(update, vec!["posts".to_owned()]);
        assert_eq!(
            words("DELETE FROM public.posts WHERE id = 1"),
            vec!["public".to_owned(), "posts".to_owned()]
        );
        let cte = words("WITH r AS (DELETE FROM a RETURNING id) UPDATE b SET v = 0");
        assert_eq!(cte, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(
            words("TRUNCATE TABLE a, b CASCADE"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(words("SELECT id FROM users").is_empty());
        let quoted = words("UPDATE posts SET note = 'UPDATE users'");
        assert_eq!(quoted, vec!["posts".to_owned()]);
    }

    /// `TABLE name` may carry the clauses of a `SELECT`.
    #[test]
    fn a_table_statement_may_carry_select_clauses() {
        let sql = "\"TABLE posts ORDER BY id LIMIT 10\"";
        assert_eq!(sql_shape(sql), Some(false));
        assert!(sql_symbols(sql).contains(&"posts".to_owned()));
        assert_eq!(sql_shape("\"Table of contents\""), None);
        assert_eq!(sql_shape("\"Table settings are saved\""), None);
    }

    /// A file `COPY` keeps its shape when options follow the blanked file name.
    #[test]
    fn copy_from_a_file_with_options_is_a_write() {
        for sql in [
            "COPY posts FROM '/tmp/posts.csv' WITH (FORMAT csv)",
            "COPY posts FROM '/tmp/posts.csv' CSV HEADER",
            "COPY posts (id) FROM '/tmp/p.csv' WHERE id > 1",
        ] {
            assert_eq!(sql_shape(&format!("{sql:?}")), Some(true), "{sql}");
        }
    }

    /// UI prose that holds `from` or `to` is not a `COPY` statement.
    #[test]
    fn copy_prose_is_not_a_statement() {
        for prose in [
            "Copy from template",
            "Copy link from here",
            "Copy the file from a to b",
            "Copy to clipboard",
        ] {
            assert_eq!(sql_shape(&format!("{prose:?}")), None, "{prose}");
        }
    }

    /// A write whose table is a `{}` placeholder names no symbol, so the job
    /// keeps the unattributed `mutating` bit.
    #[test]
    fn a_placeholder_write_is_unattributed_mutation() {
        assert!(is_mutating(
            &quote! { sql_query(format!("UPDATE {} SET hits = hits + 1", Post::TABLE)) }
        ));
        assert!(!is_mutating(
            &quote! { sql_query("SELECT id FROM posts WHERE id = {}") }
        ));
    }

    /// `$` inside a name does not open a dollar quote.
    #[test]
    fn a_dollar_inside_an_identifier_is_not_a_quote() {
        assert_eq!(
            sql_shape("\"SELECT a$b$ FROM posts; DELETE FROM q\""),
            Some(true)
        );
        assert!(sql_symbols("\"SELECT a$b$ FROM posts\"").contains(&"posts".to_owned()));
    }

    /// A quoted identifier or a column named `to`/`from` is not the direction.
    #[test]
    fn copy_direction_ignores_quoted_and_parenthesised_words() {
        assert_eq!(sql_shape(r#""COPY "to" FROM STDIN""#), Some(true));
        assert_eq!(sql_shape(r#""COPY posts ("from") TO STDOUT""#), Some(false));
        assert_eq!(
            sql_shape("\"COPY posts (\\\"to\\\") FROM STDIN\""),
            Some(true)
        );
    }

    /// Bare `TABLE name` is a read. Prose that starts with "Table" is not SQL.
    #[test]
    fn a_bare_table_statement_is_a_read() {
        let plain = "\"TABLE posts\"";
        assert_eq!(sql_shape(plain), Some(false));
        assert!(sql_symbols(plain).contains(&"posts".to_owned()));
        assert_eq!(sql_shape("\"TABLE ONLY posts\""), Some(false));
        assert_eq!(sql_shape("\"TABLE public.posts\""), Some(false));
        assert_eq!(sql_shape("\"Table of contents\""), None);
        assert_eq!(sql_shape("\"Table\""), None);
        assert_eq!(sql_shape("\"Table 1\""), None);
    }

    /// DDL is out by design. It is migration, not data access.
    #[test]
    fn ddl_is_not_a_statement() {
        assert_eq!(sql_shape("\"CREATE TABLE posts (id int)\""), None);
        assert_eq!(sql_shape("\"ALTER TABLE posts ADD COLUMN a int\""), None);
        assert_eq!(sql_shape("\"DROP TABLE posts\""), None);
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
        // SQL evidence names its symbols, so the job-wide bit stays off.
        assert!(generated.contains("mutating : false"), "{generated}");
        assert!(
            generated.contains(r#"mutating_symbols : & ["posts"]"#),
            "{generated}"
        );
    }
}

//! The rewrite engine behind `autumn upgrade` (issue #1629).
//!
//! # Why tokens and not `syn` (or `sed`)
//!
//! The guide for 0.6.0 offers a `sed 's/\bwith_pool\b/…/g'` one-liner, which is
//! exactly the class of edit this command exists to stop shipping as prose: it
//! rewrites the word inside string literals, comments, and any same-named local
//! the app happens to own. Parsing to a `syn` AST and re-printing goes too far
//! the other way — it reformats the whole file and drops every comment.
//!
//! So the engine parses to a `proc_macro2::TokenStream` (which keeps
//! `span-locations` byte offsets into the *original* text), finds identifiers in
//! call position, and splices the replacement into the original bytes. Nothing
//! outside the renamed identifiers moves: whitespace, comments, and line endings
//! survive byte-for-byte.
//!
//! # What is deliberately not rewritten
//!
//! Tokens inside a macro invocation body or an attribute are reported as
//! `manual` with `file:line` rather than guessed at (issue #1629: "No site is
//! silently skipped"). A `foo! { … }` body is arbitrary input to arbitrary code
//! — the tokens that look like a call may never become one, and a macro is free
//! to build the identifier by concatenation, so a rewrite there is a guess.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet};

use super::migrations::{AppMigration, CallForm, ReceiverShape, Rewrite};

/// Why a site was left for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualReason {
    /// Inside a `foo!(…)` / `foo! { … }` invocation body.
    Macro,
    /// Inside a `#[…]` / `#![…]` attribute.
    Attribute,
    /// The call form and arity match, but the receiver is not the type the
    /// framework generates this function on — an app's own same-named
    /// associated function, or the framework one reached through an aliased
    /// import. Rewriting would be a guess either way.
    UnexpectedReceiver,
    /// The renamed name is *referenced* rather than called — passed as a
    /// function item (`.map(Repo::with_pool)`), bound to a variable, or read as
    /// a same-named field. A rename is only safe at a call site, so the
    /// reference is reported instead of guessed at.
    NotACall,
    /// The receiver is spelled like a generated repository, but no
    /// `#[repository]` trait anywhere in the scanned source generates that
    /// type. The naming convention on its own is not evidence: an app is free
    /// to write its own `PgAuditRepository` with its own `with_pool`, and
    /// rewriting that produces a call to a method which does not exist.
    UnverifiedReceiver,
}

impl ManualReason {
    /// Human-readable phrase used in the summary.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Macro => "inside a macro invocation",
            Self::Attribute => "inside an attribute",
            Self::NotACall => "referenced without being called",
            Self::UnexpectedReceiver => "receiver is not a generated repository",
            Self::UnverifiedReceiver => {
                "no `#[repository]` trait in this app generates this receiver"
            }
        }
    }
}

/// One call site the engine acted on, or declined to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// 1-based line in the source file.
    pub line: usize,
    /// 1-based column in the source file.
    pub column: usize,
    /// [`AppMigration::id`] of the migration that matched.
    pub migration: &'static str,
    /// `None` when the site was rewritten; `Some` when it was left for a human.
    pub manual: Option<ManualReason>,
}

/// The outcome of running a set of migrations over one file's text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceRewrite {
    /// The rewritten text, or `None` when nothing matched.
    pub updated: Option<String>,
    /// Sites that were rewritten, in source order.
    pub rewritten: Vec<Site>,
    /// Sites left for a human, in source order.
    pub manual: Vec<Site>,
}

/// Rust keywords that can legally sit immediately before a `!` that is *not* a
/// macro bang (`return !(a)`, `if !(a) {}`). Without this list, the `Ident`,
/// `!`, `Group` shape of such an expression reads as a macro invocation, and
/// every call site inside the parentheses would be over-reported as `manual`.
/// Over-reporting is the safe direction, but it is still wrong.
const NON_MACRO_KEYWORDS: &[&str] = &[
    "return", "break", "if", "while", "match", "else", "in", "yield", "loop", "await", "move",
];

/// Where in the token tree we are. Tokens inside a macro body or an attribute
/// are input to code that has not run yet, so they are reported, never edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Code,
    Macro,
    Attribute,
}

impl Context {
    const fn manual_reason(self) -> Option<ManualReason> {
        match self {
            Self::Code => None,
            Self::Macro => Some(ManualReason::Macro),
            Self::Attribute => Some(ManualReason::Attribute),
        }
    }
}

/// One matched identifier, before anything is spliced.
#[derive(Debug, Clone)]
struct Hit {
    range: std::ops::Range<usize>,
    site: Site,
    /// The identifier the span is expected to contain, re-checked before the
    /// splice so a span/text disagreement can never corrupt a file. Carries the
    /// `r#` prefix when the source wrote the name as a raw identifier.
    expected: String,
    /// The identifier the site is renamed to, when it is rewritable.
    replacement: String,
}

/// A rename this run is looking for.
struct Rename {
    id: &'static str,
    from: &'static str,
    to: &'static str,
    /// Which call form the renamed function actually has.
    form: CallForm,
    /// Exact top-level argument count of the renamed function.
    args: usize,
    /// Naming shape the receiver must have, if the framework fixes one.
    receiver: Option<ReceiverShape>,
}

/// Apply `migrations` to one file's `source`.
///
/// # Errors
///
/// Returns the parse error when `source` is not valid Rust, or when a matched
/// span does not contain the identifier it claims to (which would mean the byte
/// offsets and the text disagree). Either way the caller reports the file as
/// skipped and writes nothing — this function never returns a partially
/// spliced string.
pub fn rewrite_source(
    source: &str,
    migrations: &[&'static AppMigration],
    generated: &GeneratedRepositories,
) -> Result<SourceRewrite, String> {
    let renames: Vec<Rename> = migrations
        .iter()
        .filter_map(|migration| match migration.rewrite {
            Rewrite::CallRename {
                from,
                to,
                form,
                args,
                receiver,
            } => Some(Rename {
                id: migration.id,
                from,
                to,
                form,
                args,
                receiver,
            }),
            Rewrite::GuideOnly => None,
        })
        .collect();
    if renames.is_empty() {
        return Ok(SourceRewrite::default());
    }

    let stream: TokenStream = source
        .parse()
        .map_err(|error: proc_macro2::LexError| error.to_string())?;

    let mut hits = Vec::new();
    scan(&stream, Context::Code, &renames, generated, &mut hits);
    // Token order is source order for a single stream, but nested groups are
    // walked depth-first, so the flattened list is not. Sorting makes the diff,
    // the reported sites, and the splice all read top-to-bottom.
    hits.sort_by_key(|hit| hit.range.start);

    let mut result = SourceRewrite::default();
    let mut updated = String::new();
    let mut cursor = 0;
    for hit in hits {
        if hit.site.manual.is_some() {
            result.manual.push(hit.site);
            continue;
        }
        // Fail closed: if the span and the text ever disagree, splicing would
        // corrupt the file rather than migrate it.
        let matched = source.get(hit.range.clone()).ok_or_else(|| {
            format!(
                "internal error: span {}..{} is not a character range of this file",
                hit.range.start, hit.range.end
            )
        })?;
        if matched != hit.expected {
            return Err(format!(
                "internal error: expected `{}` at byte {}, found `{matched}`",
                hit.expected, hit.range.start
            ));
        }
        if cursor > hit.range.start {
            return Err(format!(
                "internal error: overlapping rewrite at byte {}",
                hit.range.start
            ));
        }
        updated.push_str(&source[cursor..hit.range.start]);
        updated.push_str(&hit.replacement);
        cursor = hit.range.end;
        result.rewritten.push(hit.site);
    }

    if result.rewritten.is_empty() {
        return Ok(result);
    }
    updated.push_str(&source[cursor..]);
    // Belt and braces on a tool that edits source in place: the rewrite is a
    // rename, so the result must still lex. If it does not, something about the
    // splice was wrong and the file is reported as skipped rather than written.
    if let Err(error) = updated.parse::<TokenStream>() {
        return Err(format!(
            "internal error: the rewritten file no longer parses ({error}); left untouched"
        ));
    }
    result.updated = Some(updated);
    Ok(result)
}

/// Apply `migrations` **release by release**, oldest first, feeding each
/// release's output into the next.
///
/// One pass over the original text would be wrong the moment two releases chain
/// — a `0.6.0` rename `a → b` followed by a `0.7.0` rename `b → c` would leave
/// the file on the intermediate name `b`, and running the command a second time
/// would move it again, so "applying twice is a no-op" would quietly stop being
/// true. Passing release by release lands on `c` in one run and finds nothing
/// on the next.
///
/// Reported line numbers survive this because a rename never adds or removes a
/// line; a column on a line that an earlier release already touched can shift
/// by the length difference of that earlier rename.
///
/// # Errors
///
/// Propagates the parse error from [`rewrite_source`] for the first pass that
/// cannot read the file. Nothing is written by this function.
pub fn rewrite_source_for_releases(
    source: &str,
    migrations: &[&'static AppMigration],
    generated: &GeneratedRepositories,
) -> Result<SourceRewrite, String> {
    let mut current = source.to_owned();
    let mut combined = SourceRewrite::default();
    let mut rewritten_anything = false;

    for release in releases(migrations) {
        let pass = rewrite_source(&current, &release, generated)?;
        combined.rewritten.extend(pass.rewritten);
        combined.manual.extend(pass.manual);
        if let Some(updated) = pass.updated {
            current = updated;
            rewritten_anything = true;
        }
    }

    // Each pass reports in its own source order; the caller wants one list that
    // reads top-to-bottom.
    combined
        .rewritten
        .sort_by_key(|site| (site.line, site.column));
    combined.manual.sort_by_key(|site| (site.line, site.column));
    if rewritten_anything {
        combined.updated = Some(current);
    }
    Ok(combined)
}

/// Split a version-ordered selection into one group per release.
fn releases(migrations: &[&'static AppMigration]) -> Vec<Vec<&'static AppMigration>> {
    let mut groups: Vec<Vec<&'static AppMigration>> = Vec::new();
    for migration in migrations {
        match groups.last_mut() {
            Some(group) if group[0].version == migration.version => group.push(migration),
            _ => groups.push(vec![migration]),
        }
    }
    groups
}

/// Walk a token stream, recording every matching call site.
fn scan(
    stream: &TokenStream,
    context: Context,
    renames: &[Rename],
    generated: &GeneratedRepositories,
    hits: &mut Vec<Hit>,
) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    for (index, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Group(group) => {
                scan(
                    &group.stream(),
                    group_context(&trees, index, context),
                    renames,
                    generated,
                    hits,
                );
            }
            TokenTree::Ident(ident) => {
                // The separator is what makes this a call on something rather
                // than any other use of the word, and *which* separator says
                // whether it can be the renamed function at all.
                let Some(separator) = receiver_separator(&trees, index) else {
                    continue;
                };
                let name = ident.to_string();
                // `r#with_pool` is the same name written raw. Match on the bare
                // name and carry the prefix through to the splice, or the site
                // is missed in silence.
                let (prefix, bare) = name
                    .strip_prefix("r#")
                    .map_or(("", name.as_str()), |bare| ("r#", bare));
                // The form must match too. `AppState::with_pool` is a *builder
                // method* that keeps the old name, so a `.with_pool(pool)` call
                // is provably not the renamed constructor — not a site this
                // declines to rewrite, a site that is a different function.
                let Some(rename) = renames
                    .iter()
                    .find(|rename| rename.from == bare && rename.form == separator)
                else {
                    continue;
                };
                let manual = context
                    .manual_reason()
                    .or_else(|| {
                        (!takes_arguments(&trees, index, rename.args))
                            .then_some(ManualReason::NotACall)
                    })
                    .or_else(|| receiver_verdict(&trees, index, rename.receiver, generated));
                let span = ident.span();
                let start = span.start();
                hits.push(Hit {
                    range: span.byte_range(),
                    site: Site {
                        line: start.line,
                        column: start.column + 1,
                        migration: rename.id,
                        manual,
                    },
                    expected: name.clone(),
                    replacement: format!("{prefix}{}", rename.to),
                });
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
}

/// The context the contents of `trees[index]` (a group) are read in.
///
/// Once inside a macro body or an attribute, everything nested stays there —
/// an inner group of a macro body is still macro input.
fn group_context(trees: &[TokenTree], index: usize, outer: Context) -> Context {
    if outer != Context::Code {
        return outer;
    }
    let TokenTree::Group(group) = &trees[index] else {
        return outer;
    };
    // `#[...]` and `#![...]`: an attribute's body is input to a derive/attribute
    // macro, or to the compiler. Checked before the macro shape so the `!` of an
    // inner attribute is not mistaken for a macro bang.
    if group.delimiter() == Delimiter::Bracket {
        // `#[...]` sits directly after the hash; `#![...]` has the inner-attribute
        // bang in between.
        let hash_at = previous_punct(trees, index, '!').map_or_else(
            || previous_punct(trees, index, '#'),
            |bang| previous_punct(trees, bang, '#'),
        );
        if hash_at.is_some() {
            return Context::Attribute;
        }
    }
    // `name!(...)`, `name! { ... }`, `path::name![...]`.
    if let Some(bang) = previous_punct(trees, index, '!')
        && is_macro_name(trees, bang)
    {
        return Context::Macro;
    }
    // A macro *definition* body is macro input too — and the more dangerous
    // half: rewriting `macro_rules! m { ($x . with_pool (…)) => … }` edits the
    // matcher, so the macro stops accepting the calls its users write, while
    // those call sites are (correctly) reported as manual and left alone. The
    // definition name sits between the bang and the body, so the invocation
    // shape above does not see it.
    if is_macro_definition_body(trees, index) {
        return Context::Macro;
    }
    outer
}

/// Whether `trees[index]` is the body of a macro *definition*.
///
/// Two shapes: `macro_rules! name { … }` (also `( … );` and `[ … ];`), and the
/// declarative-macro-2.0 `macro name(args) { … }` / `macro name { … }`.
fn is_macro_definition_body(trees: &[TokenTree], index: usize) -> bool {
    let ident_at = |at: usize, want: &str| matches!(trees.get(at), Some(TokenTree::Ident(ident)) if ident == want);
    let is_ident = |at: usize| matches!(trees.get(at), Some(TokenTree::Ident(_)));

    // macro_rules ! name <body>
    if index >= 3
        && ident_at(index - 3, "macro_rules")
        && matches!(&trees[index - 2], TokenTree::Punct(punct) if punct.as_char() == '!')
        && is_ident(index - 1)
    {
        return true;
    }
    // macro name <body>
    if index >= 2 && ident_at(index - 2, "macro") && is_ident(index - 1) {
        return true;
    }
    // macro name (args) <body>
    index >= 3
        && ident_at(index - 3, "macro")
        && is_ident(index - 2)
        && matches!(&trees[index - 1], TokenTree::Group(group)
            if group.delimiter() == Delimiter::Parenthesis)
}

/// Index of `trees[index - 1]` when it is the punctuation `ch`.
fn previous_punct(trees: &[TokenTree], index: usize, ch: char) -> Option<usize> {
    let before = index.checked_sub(1)?;
    match &trees[before] {
        TokenTree::Punct(punct) if punct.as_char() == ch => Some(before),
        _ => None,
    }
}

/// Whether the token before `bang` names a macro rather than being the operand
/// boundary of a unary `!`.
fn is_macro_name(trees: &[TokenTree], bang: usize) -> bool {
    let Some(before) = bang.checked_sub(1) else {
        return false;
    };
    match &trees[before] {
        // `path::name!` — the `::` guarantees a path, so the bang is a macro's.
        TokenTree::Punct(punct) => punct.as_char() == ':',
        TokenTree::Ident(ident) => !NON_MACRO_KEYWORDS.contains(&ident.to_string().as_str()),
        TokenTree::Group(_) | TokenTree::Literal(_) => false,
    }
}

/// Whether an argument list follows `trees[index]`, directly or past a
/// turbofish — the half that separates a *call* from a mere reference.
///
/// A reference is not renamed: `xs.map(Repo::with_pool)` hands the function
/// itself somewhere, and the tool has no way to know the receiver's type. Such
/// a site is reported as [`ManualReason::NotACall`] rather than skipped.
fn takes_arguments(trees: &[TokenTree], index: usize, want: usize) -> bool {
    let arguments = match trees.get(index + 1) {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => group,
        // Turbofish: `name::<T>(...)` is a call; `Vec<Repo::with_pool::<T>>` is
        // a type, so the closing angle has to be followed by an argument list.
        Some(TokenTree::Punct(punct)) if punct.as_char() == ':' => {
            if !matches!(trees.get(index + 2), Some(TokenTree::Punct(second)) if second.as_char() == ':')
                || !matches!(trees.get(index + 3), Some(TokenTree::Punct(angle)) if angle.as_char() == '<')
            {
                return false;
            }
            match turbofish_argument_list(trees, index + 3) {
                Some(group) => group,
                None => return false,
            }
        }
        _ => return false,
    };
    count_arguments(arguments) == want
}

/// Top-level arguments in a call's parentheses.
///
/// A rename cannot change arity, so this is what separates the renamed
/// function from a same-named one reached through UFCS: the generated
/// `Repo::with_pool(pool)` takes one argument, while `AppState::with_pool`
/// written as `AppState::with_pool(state, pool)` takes two. Nested groups are
/// atomic token trees, so every comma seen here is an argument separator.
fn count_arguments(group: &proc_macro2::Group) -> usize {
    let mut arguments = 0;
    let mut in_argument = false;
    // Angle brackets are not a `Group`, so the comma in
    // `with_pool(make_pool::<Primary, Replica>())` sits at the same token level
    // as a real argument separator. Depth is tracked only for a *turbofish*
    // `::<`, which is unambiguous: opening on any bare `<` would let a
    // comparison (`f(a < b, c)`) swallow the separator after it and undercount,
    // and undercounting is the direction that produces a wrong edit. Other
    // comma-bearing generic syntax still overcounts, which reports the site
    // instead of rewriting it — the safe way to be wrong.
    let mut generics = 0usize;
    let mut previous_colons = 0usize;

    for tree in group.stream() {
        match &tree {
            TokenTree::Punct(punct) if punct.as_char() == ',' && generics == 0 => {
                arguments += 1;
                in_argument = false;
                previous_colons = 0;
            }
            TokenTree::Punct(punct) => {
                in_argument = true;
                match punct.as_char() {
                    ':' => previous_colons += 1,
                    '<' if previous_colons >= 2 => {
                        generics += 1;
                        previous_colons = 0;
                    }
                    // The `>` of a `->` closes nothing.
                    '>' if generics > 0 => {
                        generics -= 1;
                        previous_colons = 0;
                    }
                    _ => previous_colons = 0,
                }
            }
            _ => {
                in_argument = true;
                previous_colons = 0;
            }
        }
    }
    // A trailing comma closes the last argument rather than opening one.
    arguments + usize::from(in_argument)
}

/// Walk from the `<` at `angle` to its matching `>` and return the argument
/// list that follows it, if any.
fn turbofish_argument_list(trees: &[TokenTree], angle: usize) -> Option<&proc_macro2::Group> {
    let mut depth = 0usize;
    for at in angle..trees.len() {
        let TokenTree::Punct(punct) = &trees[at] else {
            continue;
        };
        match punct.as_char() {
            '<' => depth += 1,
            // The `>` of a `->` inside `::<fn(A) -> B>` closes nothing.
            '>' if !matches!(trees.get(at.wrapping_sub(1)),
                             Some(TokenTree::Punct(dash)) if dash.as_char() == '-') =>
            {
                depth -= 1;
                if depth == 0 {
                    return match trees.get(at + 1) {
                        Some(TokenTree::Group(group))
                            if group.delimiter() == Delimiter::Parenthesis =>
                        {
                            Some(group)
                        }
                        _ => None,
                    };
                }
            }
            _ => {}
        }
    }
    None
}

/// The concrete types `#[repository]` generates from the traits in `source`.
///
/// `#[repository]` names its type `Pg{trait}`, so a trait `PostRepository`
/// yields `PgPostRepository`. Collecting these across the scan turns the
/// receiver test from a naming convention into evidence: a call on a type no
/// scanned trait generates is the app's own, however it is spelled.
///
/// A file that does not parse contributes nothing rather than failing the scan
/// — it is already reported as skipped on its own account.
#[must_use]
pub fn generated_repository_types(source: &str) -> Vec<(String, Vec<String>)> {
    let Ok(stream) = source.parse::<TokenStream>() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_repository_types(&stream, Context::Code, &[], &mut found);
    found
}

/// Type names the source *writes out* — `struct`, `enum`, `union`, `type`.
///
/// `#[repository]` produces its type from a macro, so it never appears here. A
/// name that does appear is therefore a hand-written type, which is what makes
/// this the counter-evidence to a generated name: an app that declares
/// `#[repository] trait AuditRepository` in one place and writes
/// `struct PgAuditRepository` in another has two different types spelled the
/// same, and an unqualified call cannot be attributed to either.
#[must_use]
pub fn defined_type_names(source: &str) -> Vec<String> {
    let Ok(stream) = source.parse::<TokenStream>() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_defined_types(&stream, Context::Code, &mut found);
    found
}

fn collect_defined_types(stream: &TokenStream, context: Context, found: &mut Vec<String>) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    for (index, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Ident(keyword)
                if context == Context::Code
                    && matches!(keyword.to_string().as_str(), "struct" | "enum" | "union") =>
            {
                if let Some(TokenTree::Ident(name)) = trees.get(index + 1) {
                    found.push(name.to_string());
                }
            }
            // `type Alias = …;`, but not the `type` of an associated item
            // binding, which is followed by `=` only after a generic list.
            TokenTree::Ident(keyword) if context == Context::Code && keyword == "type" => {
                if let Some(TokenTree::Ident(name)) = trees.get(index + 1) {
                    found.push(name.to_string());
                }
            }
            TokenTree::Group(group) => {
                collect_defined_types(
                    &group.stream(),
                    group_context(&trees, index, context),
                    found,
                );
            }
            _ => {}
        }
    }
}

/// The generated repository types an app declares, and where.
///
/// The module path matters because a name on its own can be ambiguous: an app
/// with a real `repositories::PgAuditRepository` may also have an unrelated
/// `custom::PgAuditRepository`, and a call written against the second one would
/// otherwise be verified by the first.
#[derive(Debug, Default)]
pub struct GeneratedRepositories {
    declared: BTreeMap<String, BTreeSet<Vec<String>>>,
    /// Names the scanned source defines as ordinary types. See
    /// [`defined_type_names`].
    handwritten: BTreeSet<String>,
}

impl FromIterator<(String, Vec<String>)> for GeneratedRepositories {
    fn from_iter<I: IntoIterator<Item = (String, Vec<String>)>>(iter: I) -> Self {
        let mut declared: BTreeMap<String, BTreeSet<Vec<String>>> = BTreeMap::new();
        for (name, module) in iter {
            declared.entry(name).or_default().insert(module);
        }
        Self {
            declared,
            handwritten: BTreeSet::new(),
        }
    }
}

impl GeneratedRepositories {
    /// Whether a call on `name`, written with `qualifier` in front of it,
    /// refers to a type some `#[repository]` trait generates.
    ///
    /// An unqualified receiver is accepted on the name alone — that is how the
    /// overwhelming majority of call sites are written, and resolving it
    /// properly would mean following `use` declarations. A *qualified* one
    /// carries the module the author meant, so it is checked: the qualifier has
    /// to be a trailing part of some declaration's own path, which is what
    /// `repositories::…`, `crate::repositories::…` and a bare `…` inside that
    /// module all are. A qualifier naming a module that generates nothing —
    /// `custom::PgAuditRepository` — is not this type.
    /// Record the type names the scanned source writes out itself.
    pub fn note_handwritten<I: IntoIterator<Item = String>>(&mut self, names: I) {
        self.handwritten.extend(names);
    }

    #[must_use]
    pub fn accepts(&self, name: &str, qualifier: &[String]) -> bool {
        let Some(declared) = self.declared.get(name) else {
            return false;
        };
        // `crate::`, `self::` and `super::` say where to start resolving, not
        // which module declares the type. `super` cannot be resolved without
        // knowing the caller's own module, so it is dropped rather than guessed
        // at — erring toward accepting a name that is generated somewhere.
        let qualifier: Vec<String> = qualifier
            .iter()
            .skip_while(|segment| matches!(segment.as_str(), "crate" | "self" | "super" | "$crate"))
            .cloned()
            .collect();
        if qualifier.is_empty() {
            // Nothing in the call says which module it means, so a hand-written
            // type of the same name anywhere in the scan makes it ambiguous —
            // including one in a different crate, since a `use` can bring
            // either spelling into scope unqualified. Reported, not guessed at.
            return !self.handwritten.contains(name);
        }
        declared
            .iter()
            .any(|path| path.len() >= qualifier.len() && path.ends_with(&qualifier[..]))
    }
}

/// Only declarations in ordinary code count.
///
/// Tokens inside a macro definition or invocation may never be expanded, or may
/// be consumed as data — the same reason call sites there are reported rather
/// than rewritten. Reading a declaration out of one would let a template that
/// merely mentions `#[repository] trait AuditRepository` verify an unrelated
/// `PgAuditRepository` elsewhere, which is the wrong rewrite this verification
/// exists to prevent.
fn collect_repository_types(
    stream: &TokenStream,
    context: Context,
    module: &[String],
    found: &mut Vec<(String, Vec<String>)>,
) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    for (index, tree) in trees.iter().enumerate() {
        match tree {
            // `# [repository ...]` — the attribute is a `#` followed by a
            // bracket group whose first token is the macro's name.
            TokenTree::Punct(punct) if punct.as_char() == '#' && context == Context::Code => {
                let Some(TokenTree::Group(group)) = trees.get(index + 1) else {
                    continue;
                };
                if group.delimiter() != Delimiter::Bracket {
                    continue;
                }
                if !is_repository_attribute(&group.stream()) {
                    continue;
                }
                if let Some(trait_name) = trait_name_after(&trees, index + 2) {
                    found.push((format!("Pg{trait_name}"), module.to_vec()));
                }
            }
            TokenTree::Group(group) => {
                // `mod name { … }` opens a module; every other brace is just a
                // block, and the path it holds is the one it inherits.
                let nested = module_name_before(&trees, index).map(|name| {
                    let mut nested = module.to_vec();
                    nested.push(name);
                    nested
                });
                let inner = nested.as_deref().unwrap_or(module);
                collect_repository_types(
                    &group.stream(),
                    group_context(&trees, index, context),
                    inner,
                    found,
                );
            }
            _ => {}
        }
    }
}

/// Whether an attribute body names the `repository` macro.
///
/// Matched on the *last* path segment, because the qualified spelling
/// `#[autumn_web::repository(...)]` is the one the scaffold emits — insisting
/// the attribute begin with `repository` missed the common case and classified
/// every call site in a scaffolded app as unverified. The crate segment cannot
/// be pinned down either: a manifest is free to rename the dependency
/// (`autumn = { package = "autumn-web" }`), which makes the attribute
/// `#[autumn::repository]`.
///
/// Only the path is read. Anything with an argument list — `#[cfg(feature =
/// "repository")]` — is judged by its own name, not by what its arguments say.
fn is_repository_attribute(stream: &TokenStream) -> bool {
    let mut last_segment = None;
    for tree in stream.clone() {
        match tree {
            TokenTree::Ident(ident) => last_segment = Some(ident.to_string()),
            // Path separators, and a leading `::` for a fully-qualified path.
            TokenTree::Punct(punct) if punct.as_char() == ':' => {}
            // The argument list ends the path; anything else is not a path at
            // all, and either way the name is already whatever came before it.
            _ => break,
        }
    }
    last_segment.is_some_and(|segment| segment == "repository")
}

/// The module name when `trees[index]` is the body of `mod name { … }`.
fn module_name_before(trees: &[TokenTree], index: usize) -> Option<String> {
    let TokenTree::Group(group) = &trees[index] else {
        return None;
    };
    if group.delimiter() != Delimiter::Brace {
        return None;
    }
    let Some(TokenTree::Ident(name)) = trees.get(index.checked_sub(1)?) else {
        return None;
    };
    // The `mod` keyword sits directly before the name under either spelling —
    // `pub mod name { … }` puts the visibility before the keyword, not after.
    let keyword = trees.get(index.checked_sub(2)?);
    matches!(keyword, Some(TokenTree::Ident(keyword)) if keyword == "mod").then(|| name.to_string())
}

/// The name of the trait an attribute at `from` is applied to.
///
/// Skips whatever sits between the attribute and the `trait` keyword — further
/// attributes, doc comments (which are attributes by the time this sees them),
/// and a visibility modifier. Anything else means the attribute is not on a
/// trait, and nothing is generated.
fn trait_name_after(trees: &[TokenTree], from: usize) -> Option<String> {
    let mut index = from;
    while let Some(tree) = trees.get(index) {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                // Another attribute: step over it and its bracket group.
                index += 2;
            }
            TokenTree::Ident(ident) if ident == "pub" => {
                index += 1;
                // An optional `(crate)` / `(super)` restriction.
                if matches!(trees.get(index), Some(TokenTree::Group(group))
                    if group.delimiter() == Delimiter::Parenthesis)
                {
                    index += 1;
                }
            }
            TokenTree::Ident(ident) if ident == "unsafe" || ident == "auto" => index += 1,
            TokenTree::Ident(ident) if ident == "trait" => {
                return match trees.get(index + 1) {
                    Some(TokenTree::Ident(name)) => Some(name.to_string()),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// Whether the receiver path segment carries the prefix the framework gives
/// the type this function is generated on.
///
/// `#[repository]` names its concrete type `Pg{trait}` and the scaffold names
/// every trait `{Model}Repository`, so `PgPostRepository` has the shape of a
/// genuine receiver while an app's own `Cache` or `PgCache` does not. A rename
/// with no declared shape accepts any receiver.
///
/// The shape is the first of two tests. `generated` carries the types the
/// scanned `#[repository]` traits actually generate, and a receiver that looks
/// right but is not among them is the app's own — reported, never rewritten.
fn receiver_verdict(
    trees: &[TokenTree],
    index: usize,
    required: Option<ReceiverShape>,
    generated: &GeneratedRepositories,
) -> Option<ManualReason> {
    let required = required?;
    // `PgPostRepository :: with_pool` — the receiver sits three tokens back,
    // past the pair of colons. Anything else (a `>` closing `<T as Trait>`, a
    // macro-built path) is not a plain receiver and is reported rather than
    // rewritten.
    let Some(at) = index.checked_sub(3) else {
        return Some(ManualReason::UnexpectedReceiver);
    };
    let Some(TokenTree::Ident(ident)) = trees.get(at) else {
        return Some(ManualReason::UnexpectedReceiver);
    };
    let receiver = ident.to_string();
    if !required.matches(&receiver) {
        return Some(ManualReason::UnexpectedReceiver);
    }
    if generated.accepts(&receiver, &path_qualifier(trees, at)) {
        return None;
    }
    Some(ManualReason::UnverifiedReceiver)
}

/// The module path written in front of the receiver at `at`, outermost first.
///
/// `custom::PgAuditRepository::with_pool` yields `["custom"]`; a bare
/// `PgAuditRepository::with_pool` yields nothing.
fn path_qualifier(trees: &[TokenTree], at: usize) -> Vec<String> {
    let mut segments = Vec::new();
    let mut index = at;
    // Each step back over `ident ::` is one more segment.
    while index >= 3 {
        let separator = matches!(&trees[index - 1], TokenTree::Punct(punct) if punct.as_char() == ':')
            && matches!(&trees[index - 2], TokenTree::Punct(punct) if punct.as_char() == ':');
        if !separator {
            break;
        }
        let Some(TokenTree::Ident(ident)) = trees.get(index - 3) else {
            break;
        };
        segments.push(ident.to_string());
        index -= 3;
    }
    // A leading `::` on a fully-qualified path leaves no ident to read, and the
    // segments were collected innermost first.
    segments.reverse();
    segments
}

/// Which call form `trees[index]` is written in, if any: `.` makes it a method
/// call, a full `::` makes it an associated-function call.
///
/// The `::` test insists on *both* colons: a single `:` before an identifier is
/// a struct-literal field value or a type ascription (`Config { pool: with_pool }`),
/// which is not a call at all.
fn receiver_separator(trees: &[TokenTree], index: usize) -> Option<CallForm> {
    let before = index.checked_sub(1)?;
    let TokenTree::Punct(punct) = &trees[before] else {
        return None;
    };
    let preceded_by = |ch: char| {
        matches!(
            before.checked_sub(1).map(|at| &trees[at]),
            Some(TokenTree::Punct(first)) if first.as_char() == ch
        )
    };
    match punct.as_char() {
        // A second `.` makes this a range or struct-update expression
        // (`0..with_pool(n)`, `C { ..with_pool(d) }`), whose operand is a free
        // function — not a method on a receiver.
        '.' if !preceded_by('.') => Some(CallForm::Method),
        ':' if preceded_by(':') => Some(CallForm::AssociatedFunction),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::migrations::{AppMigration, CallForm, Confidence};

    /// The shipped 0.6.0 rename, used by every case below.
    static RENAME: AppMigration = AppMigration {
        id: "test-with-pool",
        version: "0.6.0",
        title: "test rename",
        confidence: Confidence::Auto,
        guide: "docs/migrations/0.6.0.md#anchor",
        rewrite: Rewrite::CallRename {
            from: "with_pool",
            to: "with_pool_untracked",
            form: CallForm::AssociatedFunction,
            args: 1,
            // Mirrors the shipped migration: `#[repository]` emits `Pg{trait}`
            // and the scaffold names every trait `{Model}Repository`.
            receiver: Some(ReceiverShape {
                prefix: "Pg",
                suffix: "Repository",
            }),
        },
    };

    /// The receivers the shape/form/arity fixtures use, taken as generated.
    ///
    /// Those tests are about whether a *call* is the renamed one — call form,
    /// arity, macro context, splicing. Requiring each of them to also declare a
    /// `#[repository]` trait would test the verification layer over and over
    /// and obscure what each case is actually pinning. `PgCache` is left out on
    /// purpose: it is the fixture for a receiver of the wrong *shape*, which
    /// must fail before verification is ever consulted.
    fn assumed_generated() -> GeneratedRepositories {
        [
            "PgPostRepository",
            "PgCommentRepository",
            "PgAlphaRepository",
            "PgBetaRepository",
            "PgGammaRepository",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), Vec::new()))
        .collect()
    }

    /// Just the generated type names, for the cases that are about detection
    /// rather than about where the trait lives.
    fn generated_names(source: &str) -> Vec<String> {
        generated_repository_types(source)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn collects_the_type_each_repository_trait_generates() {
        // Every spelling the attribute is written in, including the arguments
        // form (`tenant_scoped`) and a doc comment between the two, which is
        // itself an attribute by the time the token stream sees it.
        let types = generated_names(
            "#[repository]\npub trait PostRepository {}\n\
             #[repository(tenant_scoped)]\ntrait CommentRepository {}\n\
             #[repository]\n/// docs\npub(crate) trait TagRepository {}\n",
        );
        assert_eq!(
            types,
            vec!["PgPostRepository", "PgCommentRepository", "PgTagRepository"]
        );
    }

    #[test]
    fn recognises_the_qualified_attribute_spelling() {
        // What the scaffold emits, plus the renamed-dependency spelling a
        // manifest can produce and a fully-qualified path.
        for attribute in [
            "#[autumn_web::repository(Post, table = \"posts\")]",
            "#[autumn::repository(Post)]",
            "#[::autumn_web::repository]",
        ] {
            let types = generated_names(&format!("{attribute}\npub trait PostRepository {{}}\n"));
            assert_eq!(types, vec!["PgPostRepository"], "for {attribute}");
        }
    }

    #[test]
    fn does_not_read_repository_out_of_another_attributes_arguments() {
        // The path is what names the macro. `#[cfg(feature = "repository")]`
        // is a `cfg`, whatever its arguments happen to spell.
        assert!(
            generated_names("#[cfg(feature = \"repository\")]\npub trait PostRepository {}\n")
                .is_empty()
        );
        assert!(generated_names("#[derive(repository)]\npub trait PostRepository {}\n").is_empty());
    }

    #[test]
    fn ignores_repository_attributes_that_are_not_on_a_trait() {
        // Nothing is generated, so nothing may be claimed as generated.
        assert!(generated_names("#[repository]\nstruct Post;\n").is_empty());
        assert!(generated_names("#[serde(rename)]\ntrait Post {}\n").is_empty());
        // A file that does not parse contributes nothing rather than failing
        // the whole scan; it is reported as skipped on its own account.
        assert!(generated_names("fn f( {").is_empty());
    }

    #[test]
    fn a_handwritten_type_of_the_same_name_makes_an_unqualified_call_ambiguous() {
        let mut generated: GeneratedRepositories = std::iter::once((
            "PgAuditRepository".to_owned(),
            vec!["repositories".to_owned()],
        ))
        .collect();
        assert!(
            generated.accepts("PgAuditRepository", &[]),
            "with nothing else of that name, the generated type is the only reading"
        );

        generated.note_handwritten(std::iter::once("PgAuditRepository".to_owned()));
        assert!(
            !generated.accepts("PgAuditRepository", &[]),
            "once the source writes out a type of that name the call is ambiguous"
        );
        // A qualifier still decides it: the author said which module they meant.
        assert!(generated.accepts("PgAuditRepository", &["repositories".to_owned()]));
    }

    #[test]
    fn reads_the_type_names_the_source_writes_out() {
        assert_eq!(
            defined_type_names("pub struct PgAuditRepository;\nenum E {}\ntype Alias = u8;"),
            vec!["PgAuditRepository", "E", "Alias"]
        );
        // `#[repository]` output never appears in source, so a trait declaring
        // one contributes nothing here — that is what makes the two sets
        // independent evidence.
        assert!(defined_type_names("#[repository]\npub trait PostRepository {}").is_empty());
        // Macro input is not a definition, for the same reason it is not a
        // declaration.
        assert!(
            defined_type_names("macro_rules! m { () => { struct PgAuditRepository; } }").is_empty()
        );
    }

    #[test]
    fn a_qualifier_decides_between_same_named_types() {
        let generated: GeneratedRepositories = std::iter::once((
            "PgAuditRepository".to_owned(),
            vec!["repositories".to_owned()],
        ))
        .collect();

        // Unqualified: accepted on the name. Resolving it would mean following
        // `use` declarations, and this is how nearly every call site is written.
        assert!(generated.accepts("PgAuditRepository", &[]));
        // The module it is actually declared in, however the path is spelled.
        assert!(generated.accepts("PgAuditRepository", &["repositories".to_owned()]));
        assert!(generated.accepts(
            "PgAuditRepository",
            &["crate".to_owned(), "repositories".to_owned()]
        ));
        // A module that generates nothing of that name.
        assert!(!generated.accepts("PgAuditRepository", &["custom".to_owned()]));
        // And a name nothing generates at all.
        assert!(!generated.accepts("PgOtherRepository", &[]));
    }

    #[test]
    fn records_the_module_a_trait_is_declared_in() {
        assert_eq!(
            generated_repository_types(
                "mod repositories {\n  mod inner { #[repository] pub trait PostRepository {} }\n}"
            ),
            vec![(
                "PgPostRepository".to_owned(),
                vec!["repositories".to_owned(), "inner".to_owned()]
            )]
        );
        // A plain block is not a module and contributes no path segment.
        assert_eq!(
            generated_repository_types("fn f() { #[repository] trait PostRepository {} }"),
            vec![("PgPostRepository".to_owned(), Vec::<String>::new())]
        );
    }

    #[test]
    fn a_trait_inside_macro_input_is_not_evidence() {
        // Tokens inside a macro definition or invocation may never be expanded,
        // or may be consumed as data. Treating them as declarations would let a
        // template that merely *mentions* `#[repository] trait AuditRepository`
        // verify an unrelated `PgAuditRepository` elsewhere in the app — the
        // exact wrong rewrite the verification exists to prevent.
        assert!(
            generated_names(
                "macro_rules! template { () => { #[repository] trait AuditRepository {} } }"
            )
            .is_empty()
        );
        assert!(generated_names("declare! { #[repository] trait AuditRepository {} }").is_empty());
    }

    #[test]
    fn finds_traits_nested_inside_a_module() {
        let types = generated_names(
            "mod repositories {\n    #[repository]\n    pub trait PostRepository {}\n}\n",
        );
        assert_eq!(types, vec!["PgPostRepository"]);
    }

    #[test]
    fn a_conventional_name_no_trait_generates_is_reported_not_rewritten() {
        // The whole point of collecting the traits: `PgAuditRepository` has the
        // generated shape exactly, and is still the app's own type.
        let result = rewrite_source(
            "fn f(p: P) { PgAuditRepository::with_pool(p); }",
            &[&RENAME],
            &assumed_generated(),
        )
        .expect("parses");
        assert!(result.updated.is_none(), "nothing may be rewritten");
        assert_eq!(
            result.manual.first().map(|site| site.manual),
            Some(Some(ManualReason::UnverifiedReceiver)),
            "and the site is reported with the reason, got {:?}",
            result.manual
        );
    }

    #[test]
    fn a_receiver_of_the_wrong_shape_is_still_the_shape_complaint() {
        // Verification does not swallow the earlier test: `PgCache` fails on
        // shape, and saying "no trait generates this" would misdescribe it.
        let result = rewrite_source(
            "fn f(p: P) { PgCache::with_pool(p); }",
            &[&RENAME],
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(
            result.manual.first().map(|site| site.manual),
            Some(Some(ManualReason::UnexpectedReceiver)),
            "got {:?}",
            result.manual
        );
    }

    fn run(source: &str) -> SourceRewrite {
        rewrite_source(source, &[&RENAME], &assumed_generated()).expect("source parses")
    }

    fn rewritten(source: &str) -> String {
        let out = run(source).updated.expect("source was rewritten");
        // AC: "those sites compile without further edits". Lexing is the half
        // this layer can check, and it catches every splice-level corruption.
        out.parse::<TokenStream>()
            .unwrap_or_else(|error| panic!("rewritten source no longer parses: {error}\n{out}"));
        out
    }

    #[test]
    fn rewrites_an_associated_function_call() {
        // `#[repository]` emits `Pg{trait}`, so the concrete type an app calls
        // this on is `PgPostRepository`, never the trait `PostRepository`.
        let out = rewritten("fn f(p: Pool) { let r = PgPostRepository::with_pool(p); }");
        assert_eq!(
            out,
            "fn f(p: Pool) { let r = PgPostRepository::with_pool_untracked(p); }"
        );
    }

    #[test]
    fn leaves_a_same_named_builder_method_alone() {
        // `AppState::with_pool` and `AuthzContext::with_pool` are *current*
        // framework builder methods that keep the old name. The renamed
        // constructor takes no `self`, so a `.with_pool(pool)` call is provably
        // a different function — rewriting it would break
        // `AppState::for_test().with_pool(pool)`, which is ordinary test setup.
        let result = run("fn f(s: AppState, p: Pool) { let s = s.with_pool(p); }");
        assert_eq!(result.updated, None);
        assert!(
            result.manual.is_empty(),
            "not an ambiguous site to flag — a different function entirely: {:?}",
            result.manual
        );
    }

    #[test]
    fn rewrites_a_method_call_for_a_method_form_rename() {
        // The form is a property of the migration, not a global rule: a future
        // rename of a real method rewrites the `.` form and not the `::` one.
        static METHOD_RENAME: AppMigration = AppMigration {
            id: "test-method-rename",
            version: "0.7.0",
            title: "method rename",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.7.0.md#anchor",
            rewrite: Rewrite::CallRename {
                from: "old_step",
                to: "new_step",
                form: CallForm::Method,
                args: 1,
                receiver: None,
            },
        };
        let result = rewrite_source(
            "fn f(b: B, p: P) { b.old_step(p); }",
            &[&METHOD_RENAME],
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(
            result.updated.as_deref(),
            Some("fn f(b: B, p: P) { b.new_step(p); }")
        );
        let path_form = rewrite_source(
            "fn f(p: P) { B::old_step(p); }",
            &[&METHOD_RENAME],
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(path_form.updated, None, "a method rename leaves `::` alone");
    }

    #[test]
    fn a_turbofish_inside_the_argument_does_not_inflate_the_arity() {
        // The comma in `::<Primary, Replica>` is not an argument separator.
        let out =
            rewritten("fn f() { PgPostRepository::with_pool(make_pool::<Primary, Replica>()); }");
        assert!(
            out.contains("with_pool_untracked(make_pool::<Primary, Replica>())"),
            "{out}"
        );
    }

    #[test]
    fn a_comparison_in_the_argument_does_not_hide_a_separator() {
        // Undercounting is the direction that produces a wrong edit, so a bare
        // `<` must not open generic depth: this two-argument call stays a
        // two-argument call and is left alone.
        let result = run("fn f() { PgPostRepository::with_pool(a < b, pool); }");
        assert_eq!(result.updated, None);
    }

    #[test]
    fn leaves_a_ufcs_call_with_the_wrong_arity_alone() {
        // `AppState::with_pool(state, pool)` is the builder method reached
        // through UFCS: same name, same `::` form, one argument too many. A
        // rename cannot change arity, so arity tells them apart.
        let result = run("fn f(s: AppState, p: Pool) { let s = AppState::with_pool(s, p); }");
        assert_eq!(result.updated, None);
    }

    #[test]
    fn arity_is_counted_at_the_top_level_only() {
        // A comma nested inside an argument is not an argument separator, and
        // a trailing comma does not open a new argument.
        let out = rewritten("fn f() { PgPostRepository::with_pool(make(a, b),); }");
        assert!(out.contains("with_pool_untracked(make(a, b),)"), "{out}");
    }

    #[test]
    fn rewrites_a_turbofished_call() {
        let out = rewritten("fn f() { PgPostRepository::with_pool::<Pg>(p); }");
        assert_eq!(
            out,
            "fn f() { PgPostRepository::with_pool_untracked::<Pg>(p); }"
        );
    }

    #[test]
    fn preserves_formatting_comments_and_line_endings() {
        let source = "fn f() {\r\n    // build it with_pool, historically\r\n    let r = PgPostRepository::with_pool(p);   // trailing\r\n}\r\n";
        let out = rewritten(source);
        assert_eq!(
            out,
            "fn f() {\r\n    // build it with_pool, historically\r\n    let r = PgPostRepository::with_pool_untracked(p);   // trailing\r\n}\r\n"
        );
    }

    #[test]
    fn leaves_the_already_renamed_call_untouched() {
        // Applying twice is a no-op: the rename is matched on whole tokens, so
        // the new name is simply a different identifier.
        let result = run("fn f() { PgPostRepository::with_pool_untracked(p); }");
        assert_eq!(result.updated, None);
        assert!(result.rewritten.is_empty());
    }

    #[test]
    fn leaves_a_different_identifier_with_the_same_prefix_untouched() {
        let result = run("fn f() { PgPostRepository::with_pool_provider(p); }");
        assert_eq!(result.updated, None);
    }

    #[test]
    fn leaves_a_local_binding_of_the_same_name_untouched() {
        let result = run("fn f() { let with_pool = 1; dbg(with_pool); }");
        assert_eq!(result.updated, None);
        assert!(result.rewritten.is_empty());
        assert!(result.manual.is_empty());
    }

    #[test]
    fn leaves_a_struct_field_and_free_function_untouched() {
        // Neither is a `.`/`::` call site, so neither is the renamed API.
        let result = run(
            "struct S { with_pool: bool } fn with_pool() {} fn g(s: S) { let _ = s.with_pool; }",
        );
        assert_eq!(result.updated, None);
    }

    #[test]
    fn leaves_string_literals_and_comments_untouched() {
        let result = run("fn f() { let s = \"with_pool\"; /* with_pool */ }");
        assert_eq!(result.updated, None);
    }

    #[test]
    fn reports_a_macro_body_site_as_manual_without_rewriting_it() {
        let source = "fn f() {\n    make_repo! { PgPostRepository::with_pool(p) }\n}\n";
        let result = run(source);
        assert_eq!(result.updated, None, "a macro body is never rewritten");
        assert!(result.rewritten.is_empty());
        assert_eq!(result.manual.len(), 1, "the site must still be reported");
        let site = &result.manual[0];
        assert_eq!(site.line, 2);
        assert_eq!(site.manual, Some(ManualReason::Macro));
        assert_eq!(site.migration, "test-with-pool");
    }

    #[test]
    fn reports_a_nested_macro_body_site_as_manual() {
        let source = "fn f() {\n    outer!(inner!(PgPostRepository::with_pool(p)));\n}\n";
        let result = run(source);
        assert_eq!(result.updated, None);
        assert_eq!(result.manual.len(), 1);
        assert_eq!(result.manual[0].manual, Some(ManualReason::Macro));
    }

    #[test]
    fn reports_an_attribute_site_as_manual() {
        let source = "#[derive_repo(build = PgPostRepository::with_pool(p))]\nstruct S;\n";
        let result = run(source);
        assert_eq!(result.updated, None);
        assert_eq!(result.manual.len(), 1);
        assert_eq!(result.manual[0].manual, Some(ManualReason::Attribute));
        assert_eq!(result.manual[0].line, 1);
    }

    #[test]
    fn a_macro_path_segment_is_not_itself_a_call_site() {
        // `with_pool!(…)` is a macro of that name, not the renamed function.
        let result = run("fn f() { with_pool!(p); }");
        assert_eq!(result.updated, None);
        assert!(result.manual.is_empty());
    }

    #[test]
    fn records_line_and_column_for_every_rewritten_site() {
        let source = "fn f() {\n    let a = PgPostRepository::with_pool(p);\n    let b = PgCommentRepository::with_pool(q);\n}\n";
        let result = run(source);
        assert_eq!(result.rewritten.len(), 2);
        assert_eq!(result.rewritten[0].line, 2);
        assert_eq!(
            result.rewritten[0].column, 31,
            "1-based column of `with_pool`"
        );
        assert_eq!(result.rewritten[1].line, 3);
        assert!(result.rewritten.iter().all(|s| s.manual.is_none()));
    }

    #[test]
    fn splices_correctly_after_multibyte_characters() {
        // Byte offsets, not char offsets: an em dash before the site shifts the
        // two apart, and getting it wrong corrupts the file.
        let source =
            "fn f() {\n    // — a note —\n    let r = PgPostRepository::with_pool(p);\n}\n";
        let out = rewritten(source);
        assert_eq!(
            out,
            "fn f() {\n    // — a note —\n    let r = PgPostRepository::with_pool_untracked(p);\n}\n"
        );
    }

    #[test]
    fn rewrites_every_site_in_a_file() {
        let source = "fn f() { PgAlphaRepository::with_pool(p); PgBetaRepository::with_pool(q); PgGammaRepository::with_pool(r); }";
        let result = run(source);
        assert_eq!(result.rewritten.len(), 3);
        assert_eq!(
            result.updated.as_deref(),
            Some(
                "fn f() { PgAlphaRepository::with_pool_untracked(p); PgBetaRepository::with_pool_untracked(q); PgGammaRepository::with_pool_untracked(r); }"
            )
        );
    }

    #[test]
    fn rewrites_inside_cfg_disabled_code() {
        // `#[cfg(...)]` code is still the app's source and still has to compile
        // on the configuration that enables it.
        let source = "#[cfg(feature = \"db\")]\nfn f() { PgPostRepository::with_pool(p); }\n";
        let out = rewritten(source);
        assert!(out.contains("with_pool_untracked"));
    }

    #[test]
    fn a_guide_only_migration_rewrites_nothing() {
        static GUIDE_ONLY: AppMigration = AppMigration {
            id: "test-guide-only",
            version: "0.6.0",
            title: "guide only",
            confidence: Confidence::Manual,
            guide: "docs/migrations/0.6.0.md#anchor",
            rewrite: Rewrite::GuideOnly,
        };
        let result = rewrite_source(
            "fn f() { PgPostRepository::with_pool(p); }",
            &[&GUIDE_ONLY],
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(result.updated, None);
        assert!(result.rewritten.is_empty());
        assert!(result.manual.is_empty());
    }

    #[test]
    fn an_unparsable_file_is_an_error_not_a_rewrite() {
        let err = rewrite_source("fn f( {", &[&RENAME], &assumed_generated())
            .expect_err("must not silently succeed");
        assert!(!err.is_empty(), "the parse error is reported to the user");
    }

    #[test]
    fn a_macro_rules_definition_is_never_rewritten_matcher_or_transcriber() {
        // The dangerous half: rewriting the matcher makes the macro reject the
        // invocations its users write, while those invocations are (correctly)
        // reported as manual and left alone — a tree that does not compile.
        let source = "macro_rules! forward {\n    ($t:ident :: with_pool ( $p:expr )) => { $t::with_pool($p) };\n}\n";
        let result = run(source);
        assert_eq!(
            result.updated, None,
            "a macro definition body is macro input"
        );
        assert_eq!(
            result.manual.len(),
            2,
            "both sites are reported: {:?}",
            result.manual
        );
        assert!(
            result
                .manual
                .iter()
                .all(|s| s.manual == Some(ManualReason::Macro))
        );
    }

    #[test]
    fn every_macro_definition_shape_is_treated_as_macro_input() {
        for source in [
            "macro_rules! m { () => { PgPostRepository::with_pool(p) }; }",
            "macro_rules! m ( () => ( PgPostRepository::with_pool(p) ); );",
            "macro_rules! m [ () => [ PgPostRepository::with_pool(p) ]; ];",
            "#[macro_export]\nmacro_rules! m { () => { PgPostRepository::with_pool(p) }; }",
            "fn outer() { macro_rules! m { () => { PgPostRepository::with_pool(p) }; } }",
            "pub macro m() { PgPostRepository::with_pool(p) }",
            "pub macro m { () => { PgPostRepository::with_pool(p) } }",
        ] {
            let result = run(source);
            assert_eq!(result.updated, None, "must not rewrite: {source}");
            assert!(
                result
                    .manual
                    .iter()
                    .all(|s| s.manual == Some(ManualReason::Macro)),
                "must report as macro input: {source} -> {:?}",
                result.manual
            );
            assert!(!result.manual.is_empty(), "must not stay silent: {source}");
        }
    }

    #[test]
    fn a_range_or_struct_update_expression_is_not_a_method_call() {
        // The operand of `..` is a free function, not a method on a receiver.
        // Renaming it breaks an app that happens to own that name.
        for source in [
            "fn f() { for i in 0..with_pool(n) {} }",
            "fn f() { let c = C { a: 1, ..with_pool(d) }; }",
            "fn f() { let s = &v[a..with_pool(b)]; }",
            "fn f() { let s = &v[..with_pool(b)]; }",
        ] {
            let result = run(source);
            assert_eq!(result.updated, None, "must not rewrite: {source}");
            assert!(result.manual.is_empty(), "not a site at all: {source}");
        }
    }

    #[test]
    fn a_reference_that_is_never_called_is_reported_not_skipped() {
        // "No site is silently skipped": a function item handed somewhere else
        // still stops compiling after the rename, so it has to be reported.
        for source in [
            "fn f() { xs.iter().map(PgPostRepository::with_pool); }",
            "fn f() { let g = PgPostRepository::with_pool; g(p); }",
            "fn f() { let g: fn(P) -> R = PgPostRepository::with_pool; }",
        ] {
            let result = run(source);
            assert_eq!(
                result.updated, None,
                "a reference is not rewritten: {source}"
            );
            assert!(
                result
                    .manual
                    .iter()
                    .any(|s| s.manual == Some(ManualReason::NotACall)),
                "must be reported for a human: {source} -> {:?}",
                result.manual
            );
        }
    }

    #[test]
    fn a_raw_identifier_call_site_is_rewritten_keeping_its_prefix() {
        let out = rewritten("fn f() { PgPostRepository::r#with_pool(p); }");
        assert_eq!(
            out,
            "fn f() { PgPostRepository::r#with_pool_untracked(p); }"
        );
    }

    #[test]
    fn a_turbofish_that_is_not_a_call_is_not_rewritten() {
        // `Vec<PgPostRepository::with_pool::<T>>` is a type path, not a call.
        let result = run("fn f(x: Vec<PgPostRepository::with_pool::<T>>) {}");
        assert_eq!(result.updated, None);
        assert!(
            result
                .manual
                .iter()
                .any(|s| s.manual == Some(ManualReason::NotACall))
        );
    }

    #[test]
    fn a_turbofish_returning_a_function_type_still_reads_as_a_call() {
        // The `>` of the `->` inside the generic argument closes nothing.
        let out = rewritten("fn f() { PgPostRepository::with_pool::<fn(A) -> B>(p); }");
        assert!(
            out.contains("with_pool_untracked::<fn(A) -> B>(p)"),
            "{out}"
        );
    }

    #[test]
    fn chained_renames_across_releases_land_on_the_final_name_in_one_run() {
        // Two releases in range, the second renaming what the first produced.
        // A single pass over the original text would stop on the intermediate
        // name and make a second run change the file again.
        static FIRST: AppMigration = AppMigration {
            id: "0.6.0-first",
            version: "0.6.0",
            title: "first",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.6.0.md#a",
            rewrite: Rewrite::CallRename {
                from: "with_pool",
                to: "with_pool_untracked",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        };
        static SECOND: AppMigration = AppMigration {
            id: "0.7.0-second",
            version: "0.7.0",
            title: "second",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.7.0.md#a",
            rewrite: Rewrite::CallRename {
                from: "with_pool_untracked",
                to: "untracked_pool",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        };
        let chain: [&'static AppMigration; 2] = [&FIRST, &SECOND];

        let first_run = rewrite_source_for_releases(
            "fn f() { PgPostRepository::with_pool(p); }",
            &chain,
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(
            first_run.updated.as_deref(),
            Some("fn f() { PgPostRepository::untracked_pool(p); }"),
            "one run reaches the newest name"
        );

        let second_run = rewrite_source_for_releases(
            first_run.updated.as_deref().expect("rewritten"),
            &chain,
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(second_run.updated, None, "and a second run is a no-op");
    }

    #[test]
    fn sites_from_several_releases_are_reported_in_source_order() {
        static A: AppMigration = AppMigration {
            id: "0.6.0-a",
            version: "0.6.0",
            title: "a",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.6.0.md#a",
            rewrite: Rewrite::CallRename {
                from: "with_pool",
                to: "with_pool_untracked",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        };
        static B: AppMigration = AppMigration {
            id: "0.7.0-b",
            version: "0.7.0",
            title: "b",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.7.0.md#b",
            rewrite: Rewrite::CallRename {
                from: "old_name",
                to: "new_name",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        };
        // `old_name` (0.7.0) is on line 2, `with_pool` (0.6.0) on line 3.
        let source = "fn f() {\n    R::old_name(p);\n    PgPostRepository::with_pool(p);\n}\n";
        let result =
            rewrite_source_for_releases(source, &[&A, &B], &assumed_generated()).expect("parses");
        let lines: Vec<usize> = result.rewritten.iter().map(|site| site.line).collect();
        assert_eq!(
            lines,
            vec![2, 3],
            "reported top-to-bottom, not pass by pass"
        );
    }

    #[test]
    fn leaves_an_unrelated_associated_function_with_the_same_name_alone() {
        // `#[repository]` names its concrete type `Pg{trait}`, so an app's own
        // `Cache::with_pool(pool)` is not the renamed constructor. It is
        // reported rather than dropped, because an aliased import of a real
        // repository would look exactly like this.
        let result = run("fn f(p: Pool) { let c = Cache::with_pool(p); }");
        assert_eq!(result.updated, None, "an unrelated type is not rewritten");
        assert_eq!(result.manual.len(), 1, "and not silently skipped either");
        assert_eq!(
            result.manual[0].manual,
            Some(ManualReason::UnexpectedReceiver)
        );
    }

    #[test]
    fn leaves_an_app_type_that_merely_starts_with_pg_alone() {
        // `PgCache` is an ordinary Postgres helper someone wrote, not a type
        // `#[repository]` emitted. Reported, so an aliased repository is not
        // lost, but never rewritten.
        let result = run("fn f(p: Pool) { let c = PgCache::with_pool(p); }");
        assert_eq!(result.updated, None);
        assert_eq!(
            result.manual[0].manual,
            Some(ManualReason::UnexpectedReceiver)
        );
    }

    #[test]
    fn rewrites_a_generated_repository_receiver_however_it_is_pathed() {
        // The prefix is on the final path segment, so a fully-qualified path to
        // a generated repository still matches — provided the path leads to the
        // module the trait is declared in, which is what makes it *this* type
        // rather than a same-named one somewhere else.
        let generated: GeneratedRepositories =
            std::iter::once(("PgPostRepository".to_owned(), vec!["repos".to_owned()])).collect();
        let out = rewrite_source(
            "fn f(p: Pool) { crate::repos::PgPostRepository::with_pool(p); }",
            &[&RENAME],
            &generated,
        )
        .expect("parses")
        .updated
        .expect("source was rewritten");
        assert!(
            out.contains("PgPostRepository::with_pool_untracked(p)"),
            "{out}"
        );
    }

    #[test]
    fn a_qualified_trait_call_is_reported_not_rewritten() {
        // `<T as Repo>::with_pool(p)` has no plain receiver ident to check.
        let result = run("fn f(p: Pool) { <T as Repo>::with_pool(p); }");
        assert_eq!(result.updated, None);
        assert_eq!(
            result.manual[0].manual,
            Some(ManualReason::UnexpectedReceiver)
        );
    }

    #[test]
    fn a_rename_with_no_receiver_constraint_accepts_any_receiver() {
        static ANY: AppMigration = AppMigration {
            id: "test-any-receiver",
            version: "0.7.0",
            title: "any receiver",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.7.0.md#anchor",
            rewrite: Rewrite::CallRename {
                from: "old_ctor",
                to: "new_ctor",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        };
        let result = rewrite_source(
            "fn f(p: P) { Anything::old_ctor(p); }",
            &[&ANY],
            &assumed_generated(),
        )
        .expect("parses");
        assert_eq!(
            result.updated.as_deref(),
            Some("fn f(p: P) { Anything::new_ctor(p); }")
        );
    }

    #[test]
    fn manual_reasons_read_as_a_sentence_fragment() {
        assert_eq!(ManualReason::Macro.describe(), "inside a macro invocation");
        assert_eq!(ManualReason::Attribute.describe(), "inside an attribute");
        assert_eq!(
            ManualReason::NotACall.describe(),
            "referenced without being called"
        );
        assert_eq!(
            ManualReason::UnexpectedReceiver.describe(),
            "receiver is not a generated repository"
        );
    }
}

//! `autumn a11y verify` — a build-time accessibility audit of raw `html!`
//! markup.
//!
//! The typed primitives in [`autumn_web::a11y`](autumn_web::a11y) (`Img`,
//! `Button`, `Link`, `MenuItem`, `TextField`) discharge every accessible-name
//! obligation **at compile time**: an alt-less image or an unlabeled field
//! written through them does not compile. Code that uses those primitives is
//! therefore already proven and is intentionally *not* re-scanned here.
//!
//! This pass covers the escape hatch the type system cannot see: raw markup
//! written directly in a `maud::html! { … }` block, which bypasses the typed
//! primitives entirely. There is no walkable widget tree at runtime, so this is
//! an honest best-effort **static** scan of the `html!` token streams found in
//! the project's `.rs` files — the same token-descent strategy `autumn i18n
//! check` uses to find `t!` calls nested inside `html!`.
//!
//! # Scope and limitations
//!
//! `verify` is a build-time **static** scanner over raw `maud::html!` bodies
//! that applies the WCAG rules below (`image-alt` 1.1.1, `label` 1.3.1 / 3.3.2,
//! `button-name` / `link-name` 4.1.2). It is **best-effort and advisory**: it
//! parses maud markup heuristically at the token level and deliberately errs
//! toward *not* failing on anything it cannot statically resolve — unresolved
//! splices, complex Rust expressions, and non-maud `html!` macros are skipped
//! rather than guessed at. It should never fail valid markup, but it can
//! therefore *miss* a defect in exotic markup (a miss is acceptable by design),
//! so its raw-`html!` findings are advisory-grade signal, not a proof of
//! accessibility. The complete, by-construction guarantee lives in the typed
//! primitives in [`autumn_web::a11y`](autumn_web::a11y), where an accessible
//! name is a compile-time type obligation proven by `trybuild`; this scanner is
//! only the escape-hatch net over raw hand-written `html!`.
//!
//! # Ruleset (first slice)
//!
//! Findings reuse the WCAG success-criterion numbers, rule identifiers, and
//! [`Severity`] semantics of the runtime [`autumn check --a11y`](crate::check)
//! lint, applied to static source instead of rendered HTML:
//!
//! - **`image-alt`** — an `<img>` with no `alt` attribute (WCAG 1.1.1, Serious).
//! - **`label`** — an `<input>`/`<select>`/`<textarea>` with no associated
//!   `<label for=…>`, `aria-label`, or `aria-labelledby` (WCAG 1.3.1 / 3.3.2 /
//!   4.1.2, Serious).
//! - **`button-name`** — a `<button>` with no text content and no
//!   `aria-label`/`aria-labelledby` (WCAG 4.1.2, Serious).
//! - **`link-name`** — an `<a>` with an `href` but no text content and no
//!   `aria-label`/`aria-labelledby` (WCAG 2.4.4 / 4.1.2, Serious).
//!
//! # Known heuristic limits
//!
//! Like `autumn i18n check`, the scanner reads tokens rather than a
//! type-resolved AST, and deliberately errs toward *not* flagging anything it
//! cannot statically resolve — so it never breaks CI on a false positive:
//!
//! - A **spliced** value (`(expr)`) is unknowable, so an element whose relevant
//!   attribute/content is a splice is skipped. In particular a typed primitive
//!   spliced in as `(Img::new(src, alt))` is a `(expr)` group, not an `img`
//!   element, and is never flagged.
//! - Markup inside a `@if` / `@for` / `@match` control block is still scanned
//!   for elements, but a `<label for=…>`/`id` association that a splice makes
//!   unresolvable suppresses the corresponding `label` finding rather than
//!   risking a false positive.
//! - Only maud/autumn `html!` markup is analyzed. A different `html!` macro
//!   (e.g. a Yew/JSX island using `<tag>…</tag>` angle-bracket syntax) is
//!   detected and skipped wholesale — maud never uses JSX tags, so parsing such
//!   a body as maud would misread `<button></button>` as an empty `button` and
//!   fire a false `button-name`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use proc_macro2::{Delimiter, Literal, TokenStream, TokenTree};
use serde::Serialize;

use crate::check::Severity;

/// Output format for `autumn a11y verify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable text (default).
    Text,
    /// Machine-readable JSON conformance manifest.
    Json,
}

/// Options parsed from CLI flags.
#[derive(Debug, Clone, Copy)]
pub struct A11yVerifyOptions {
    pub format: OutputFormat,
    /// Lower the failure threshold so any finding (Moderate and above) fails.
    /// The current ruleset only emits Serious findings, so a clean run stays
    /// green either way; the flag is honored for forward compatibility and CI
    /// consistency with `autumn i18n check` / `autumn check --a11y`.
    pub strict: bool,
}

/// An accessibility rule in the raw-`html!` catalog. Each maps to a runtime
/// [`autumn check --a11y`](crate::check) rule id and a WCAG success criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// `<img>` with no `alt` attribute.
    ImageAlt,
    /// Form control with no associated label or accessible name.
    Label,
    /// `<button>` with no accessible name.
    ButtonName,
    /// `<a href>` with no accessible name.
    LinkName,
}

impl Rule {
    /// axe-core-style rule id, shared with the runtime lint in [`crate::check`].
    const fn rule_id(self) -> &'static str {
        match self {
            Self::ImageAlt => "image-alt",
            Self::Label => "label",
            Self::ButtonName => "button-name",
            Self::LinkName => "link-name",
        }
    }

    /// The WCAG 2.1 success criterion (or criteria) this rule enforces.
    const fn wcag(self) -> &'static str {
        match self {
            Self::ImageAlt => "1.1.1",
            Self::Label => "1.3.1 / 3.3.2 / 4.1.2",
            Self::ButtonName => "4.1.2",
            Self::LinkName => "2.4.4 / 4.1.2",
        }
    }

    /// Human-readable description of the violation.
    const fn message(self) -> &'static str {
        match self {
            Self::ImageAlt => "raw <img> has no alt attribute",
            Self::Label => {
                "raw form control has no associated <label for=…>, aria-label, or aria-labelledby"
            }
            Self::ButtonName => {
                "raw <button> has no text content and no aria-label/aria-labelledby"
            }
            Self::LinkName => "raw <a href> has no link text and no aria-label/aria-labelledby",
        }
    }

    /// The typed primitive that discharges this obligation at compile time.
    const fn hint(self) -> &'static str {
        match self {
            Self::ImageAlt => "use autumn_web::a11y::Img::new(src, alt) / Img::decorative(src)",
            Self::Label => "use autumn_web::a11y::TextField::new(..).label(..)",
            Self::ButtonName => {
                "use autumn_web::a11y::Button::new(name) / Button::icon(icon, name)"
            }
            Self::LinkName => "use autumn_web::a11y::Link::new(href, text) / Link::icon(..)",
        }
    }
}

/// A single accessibility finding, keyed to a WCAG success criterion.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Finding {
    /// Source file, relative to the project root.
    pub file: String,
    /// 1-based line number of the offending element.
    pub line: usize,
    /// The HTML element that triggered the finding (e.g. `img`, `button`).
    pub element: String,
    /// axe-core-style rule id, shared with `autumn check --a11y`.
    pub rule_id: &'static str,
    /// WCAG 2.1 success criterion (or criteria).
    pub wcag: &'static str,
    /// Violation severity.
    #[serde(serialize_with = "serialize_severity")]
    pub severity: Severity,
    /// Human-readable description of what is wrong.
    pub message: &'static str,
    /// The typed primitive that fixes it at compile time.
    pub hint: &'static str,
}

fn serialize_severity<S>(severity: &Severity, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&severity.to_string())
}

/// Aggregate counts by severity.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
pub struct Summary {
    pub critical: usize,
    pub serious: usize,
    pub moderate: usize,
    pub total: usize,
}

/// The full result of a verify run — the JSON conformance manifest.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    /// Number of `.rs` files scanned.
    pub files_scanned: usize,
    /// Number of `html!` blocks discovered and analyzed.
    pub html_blocks: usize,
    /// Findings, sorted by file then line.
    pub findings: Vec<Finding>,
    pub summary: Summary,
}

impl Report {
    /// Build a report from a completed scan, sorting findings deterministically.
    fn from_scan(mut scan: Scan) -> Self {
        scan.findings
            .sort_by(|a, b| (a.file.as_str(), a.line).cmp(&(b.file.as_str(), b.line)));
        let mut summary = Summary::default();
        for finding in &scan.findings {
            match finding.severity {
                Severity::Critical => summary.critical += 1,
                Severity::Serious => summary.serious += 1,
                Severity::Moderate => summary.moderate += 1,
            }
        }
        summary.total = scan.findings.len();
        Self {
            files_scanned: scan.files_scanned,
            html_blocks: scan.html_blocks,
            findings: scan.findings,
            summary,
        }
    }

    /// Process exit code. Non-zero when any finding meets the failure
    /// threshold — Serious by default, lowered to Moderate under `--strict`.
    #[must_use]
    pub fn exit_code(&self, strict: bool) -> i32 {
        let threshold = if strict {
            Severity::Moderate
        } else {
            Severity::Serious
        };
        i32::from(self.findings.iter().any(|f| f.severity >= threshold))
    }
}

// ── Scanner ────────────────────────────────────────────────────────────────

/// Accumulator threaded through the source scan.
#[derive(Debug, Default)]
struct Scan {
    findings: Vec<Finding>,
    files_scanned: usize,
    html_blocks: usize,
}

impl Scan {
    fn push(&mut self, rule: Rule, element: &str, line: usize, file: &str) {
        self.findings.push(Finding {
            file: file.to_owned(),
            line,
            element: element.to_owned(),
            rule_id: rule.rule_id(),
            wcag: rule.wcag(),
            severity: Severity::Serious,
            message: rule.message(),
            hint: rule.hint(),
        });
    }
}

/// Walk `root` recursively and scan every `.rs` file for raw-`html!` markup.
///
/// The scan ROOT read is fatal: a misspelled/unreadable/nonexistent path must
/// fail hard, not silently yield zero findings — a zero-findings run prints
/// PASS and would let a CI typo disable the whole audit. `std::fs::read_dir`
/// surfaces all three failure modes (path absent, path is not a directory, or
/// the directory is unreadable). A genuinely empty-but-readable directory reads
/// fine here and legitimately produces an empty file list (zero findings, exit
/// 0). Nested subdir read errors *during recursion* remain best-effort
/// (swallowed by [`collect_rs_files`]), mirroring `autumn i18n check`.
fn scan_project(root: &Path) -> std::io::Result<Scan> {
    std::fs::read_dir(root)?;
    let mut scan = Scan::default();
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    files.sort();
    for path in files {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        scan_source(&src, &rel, &mut scan);
        scan.files_scanned += 1;
    }
    Ok(scan)
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Never follow symlinks: a symlinked directory cycle would otherwise
        // recurse forever and hang this CI tool.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_rs_files(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Tokenize one source string and analyze every `html!` block within it.
fn scan_source(src: &str, file: &str, scan: &mut Scan) {
    let Ok(stream) = TokenStream::from_str(src) else {
        return;
    };
    find_html_blocks(&stream, file, scan);
}

/// Recursively walk a token stream, analyzing each `html! { … }` macro body and
/// descending into every group so nested `html!` invocations (e.g. spliced into
/// another block) are found too.
fn find_html_blocks(stream: &TokenStream, file: &str, scan: &mut Scan) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    let mut i = 0;
    while i < trees.len() {
        if let TokenTree::Ident(ident) = &trees[i]
            && *ident == "html"
            && matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
            && let Some(TokenTree::Group(group)) = trees.get(i + 2)
        {
            let body: Vec<TokenTree> = group.stream().into_iter().collect();
            // Only maud/autumn `html!` markup is analyzed. A project may define a
            // DIFFERENT `html!` (e.g. a Yew/JSX island using `<tag>…</tag>`
            // syntax); handing such a body to the maud parser would strip the
            // angle brackets and misread `<button></button>` as an empty maud
            // `button`, a false `button-name`. maud never uses JSX angle-bracket
            // tags, so a body that does is a different macro — skip it entirely.
            if looks_like_jsx(&body) {
                i += 3;
                continue;
            }
            let nodes = parse_nodes(&body);
            scan.html_blocks += 1;
            analyze_block(&nodes, file, scan);
            // Descend into the body for nested `html!` splices.
            find_html_blocks(&group.stream(), file, scan);
            i += 3;
            continue;
        }
        if let TokenTree::Group(group) = &trees[i] {
            find_html_blocks(&group.stream(), file, scan);
        }
        i += 1;
    }
}

/// Whether an `html!` body is JSX/Yew-style angle-bracket markup rather than
/// maud. maud markup NEVER uses `<tag …>` / `</tag>` element syntax, so a `<`
/// punct immediately followed by an ident or `/` (`<button`, `</`) — the way
/// every JSX open/close tag begins — marks the body as a DIFFERENT `html!`
/// macro. Such a body is skipped entirely rather than parsed as maud, which
/// would drop the angle brackets and misread `<button></button>` as an empty
/// maud `button` (a false `button-name`). When in doubt we skip: a missed check
/// on non-maud markup is acceptable, a false positive on valid markup is not.
fn looks_like_jsx(trees: &[TokenTree]) -> bool {
    for (idx, tree) in trees.iter().enumerate() {
        let TokenTree::Punct(p) = tree else {
            continue;
        };
        if p.as_char() != '<' {
            continue;
        }
        match trees.get(idx + 1) {
            // `<button` (open tag) or `</button` (close tag) — JSX, not maud.
            Some(TokenTree::Ident(_)) => return true,
            Some(TokenTree::Punct(q)) if q.as_char() == '/' => return true,
            _ => {}
        }
    }
    false
}

// ── Maud markup model ──────────────────────────────────────────────────────

/// A parsed attribute value.
#[derive(Debug, Clone)]
enum AttrValue {
    /// A static string literal, e.g. `alt="logo"` → `logo`.
    Literal(String),
    /// A spliced/runtime value, e.g. `alt=(expr)` — unresolvable.
    Dynamic,
    /// A boolean/presence-only or optional attribute, e.g. `disabled`,
    /// `required[cond]`.
    Boolean,
}

/// A parsed HTML attribute.
#[derive(Debug, Clone)]
struct Attr {
    name: String,
    value: AttrValue,
}

/// A parsed HTML element (best-effort).
#[derive(Debug, Clone)]
struct Element {
    name: String,
    line: usize,
    attrs: Vec<Attr>,
    children: Vec<Node>,
}

impl Element {
    /// The value of the first attribute named `name` (ASCII-case-insensitive).
    fn attr(&self, name: &str) -> Option<&AttrValue> {
        self.attrs
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
            .map(|a| &a.value)
    }

    /// Whether an attribute named `name` is present in any form.
    fn has_attr(&self, name: &str) -> bool {
        self.attr(name).is_some()
    }

    /// Whether this element carries a non-empty `aria-label`/`aria-labelledby`.
    /// A spliced value counts as a name (unresolvable → assume present).
    fn has_aria_name(&self) -> bool {
        ["aria-label", "aria-labelledby"]
            .iter()
            .any(|n| attr_is_present_name(self.attr(n)))
    }

    /// Whether this element carries a non-empty `title`. Per the ARIA
    /// accessible-name computation `title` is a last-resort name source, so a
    /// control (or descendant) with a non-empty/dynamic `title` is named. A
    /// spliced `title=(…)` counts (unresolvable → assume present).
    fn has_title_name(&self) -> bool {
        attr_is_present_name(self.attr("title"))
    }
}

/// Whether an element is removed from the accessibility tree by an explicit
/// presentational role (`role="presentation"`/`role="none"`). A spliced
/// `role=(…)` is unresolvable and could be one of those, so it is treated as
/// presentational (non-failing convention — skip rather than misfire). Note this
/// is only honored for non-focusable elements (e.g. `<img>`): ARIA conflict
/// resolution restores the implicit role on focusable controls, so it is not
/// applied to `<button>`/`<a href>`/form controls.
fn is_presentational(el: &Element) -> bool {
    match el.attr("role") {
        Some(AttrValue::Dynamic) => true,
        Some(AttrValue::Literal(r)) => {
            let r = r.to_ascii_lowercase();
            r == "presentation" || r == "none"
        }
        _ => false,
    }
}

/// Whether an attribute value supplies a non-empty accessible name: a spliced
/// value (unresolvable → assume present) or a non-empty string literal.
const fn attr_is_present_name(value: Option<&AttrValue>) -> bool {
    match value {
        Some(AttrValue::Dynamic) => true,
        Some(AttrValue::Literal(s)) => !s.is_empty(),
        _ => false,
    }
}

/// A parsed markup node.
#[derive(Debug, Clone)]
enum Node {
    Element(Element),
    /// A non-empty static text node.
    Text(String),
    /// A splice `(expr)` or a control block — an unresolvable dynamic value.
    /// `splice` records whether it is a genuine `(expr)`/`[..]` splice (which
    /// could render a `<label>` fragment beside a control) rather than an
    /// `@`-control head; only the former marks a block as possibly-labeled.
    Dynamic {
        splice: bool,
    },
}

/// Parse a maud markup token stream into a best-effort node list.
fn parse_markup(stream: &TokenStream) -> Vec<Node> {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    parse_nodes(&trees)
}

fn parse_nodes(trees: &[TokenTree]) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Ident(_) => {
                let (element, next) = parse_element(trees, i);
                nodes.push(Node::Element(element));
                i = next;
            }
            TokenTree::Literal(lit) => {
                let text = literal_text(lit);
                if !text.trim().is_empty() {
                    nodes.push(Node::Text(text));
                }
                i += 1;
            }
            // A leading `.`/`#` shorthand with NO explicit tag name is maud's
            // implicit `div`: `.input { … }` renders `<div class="input">` and
            // `#button { … }` renders `<div id="button">`. The class/id token is
            // a VALUE, not a tag, so parse it as a `div` element (which triggers
            // no rule) rather than letting the next iteration read `input`/`button`
            // as a phantom element (false positive).
            TokenTree::Punct(p) if matches!(p.as_char(), '#' | '.') => {
                let (element, next) = parse_element(trees, i);
                nodes.push(Node::Element(element));
                i = next;
            }
            TokenTree::Punct(p) if p.as_char() == '@' => {
                // Control flow (`@if`/`@for`/`@match`/`@let`): dynamic, but still
                // descend into any block so elements inside are analyzed. Not a
                // spliced fragment, so it never marks the block as possibly-labeled.
                nodes.push(Node::Dynamic { splice: false });
                if matches!(trees.get(i + 1), Some(TokenTree::Ident(id)) if *id == "match") {
                    // `@match` is special: the brace group after the scrutinee is
                    // an arm LIST, not markup. Parsing it as markup would treat
                    // each arm's pattern (e.g. `input => …`) as an element and
                    // fire a false positive. Skip past `@match`, then parse arms.
                    i = skip_match(trees, i + 2, &mut nodes);
                } else if matches!(trees.get(i + 1), Some(TokenTree::Ident(id)) if *id == "let") {
                    // `@let` is special: it binds a Rust expression, so any brace
                    // group in its initializer (`Field { input: true }`) is
                    // struct-literal syntax, NEVER markup. Unlike `@if`/`@for`,
                    // descending into it would misread a Rust field named `input`
                    // as an `<input>`. Skip the whole binding up to its `;`.
                    i = skip_let(trees, i + 2);
                } else {
                    i = skip_control(trees, i + 1, &mut nodes);
                }
            }
            TokenTree::Group(g) => {
                if g.delimiter() == Delimiter::Brace {
                    nodes.extend(parse_markup(&g.stream()));
                } else {
                    // A `(expr)` splice or `[..]` — an unresolvable dynamic value
                    // that could render a `<label>` fragment beside a control.
                    nodes.push(Node::Dynamic { splice: true });
                }
                i += 1;
            }
            TokenTree::Punct(_) => i += 1,
        }
    }
    nodes
}

/// Skip a `@if`/`@for`/`@while` head and fold each body block's children into
/// `nodes` as siblings, following a trailing `@else` chain.
///
/// The body of these controls is a brace `Block`, but a brace can also appear in
/// the HEAD and must NOT be parsed as markup:
///
/// - a `let`/`for` **pattern** may be a struct/enum-struct pattern
///   (`@for Foo { x } in xs`, `@if let Foo { x } = e`); its braces are pattern
///   syntax, and
/// - the condition/iterator is parsed with `parse_without_eager_brace`, which
///   accepts a **block-expression** condition (`@if { cond } { body }`); the
///   leading brace is then the condition, not the body.
///
/// The body is the final brace of the clause: a brace outside a pattern region,
/// and — when no condition token precedes it — only if it is not itself a
/// leading block-expression condition (i.e. not immediately followed by the real
/// body brace).
fn skip_control(trees: &[TokenTree], start: usize, nodes: &mut Vec<Node>) -> usize {
    let mut i = start;
    // Inside a `let`/`for` pattern region, where braces are pattern syntax.
    let mut in_pattern = false;
    // Whether a non-brace condition token has been seen (so a following brace is
    // the body, not a leading block-expression condition).
    let mut saw_cond = false;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Ident(id) => {
                match id.to_string().as_str() {
                    // Control keywords: not condition tokens.
                    "if" | "while" | "else" => {}
                    // `let`/`for` open a pattern region.
                    "let" | "for" => in_pattern = true,
                    // `in` closes a `@for pat in …` pattern region.
                    "in" if in_pattern => in_pattern = false,
                    // Any other ident is part of the condition/iterator expression.
                    _ => {
                        if !in_pattern {
                            saw_cond = true;
                        }
                    }
                }
                i += 1;
            }
            // `=` closes a `@if let pat = …` / `@while let pat = …` pattern region.
            TokenTree::Punct(p) if in_pattern && p.as_char() == '=' => {
                in_pattern = false;
                i += 1;
            }
            // `@let x = …;` (defensive) ends at a semicolon with no block.
            TokenTree::Punct(p) if p.as_char() == ';' => {
                i += 1;
                break;
            }
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                if in_pattern {
                    // Struct/enum-struct pattern brace — skip, not markup.
                    i += 1;
                    continue;
                }
                if !saw_cond
                    && matches!(trees.get(i + 1), Some(TokenTree::Group(n)) if n.delimiter() == Delimiter::Brace)
                {
                    // Leading block-expression condition; body is the next brace.
                    i += 1;
                    continue;
                }
                nodes.extend(parse_markup(&g.stream()));
                i += 1;
                // Only `@else` continues this chain. Any other `@`-control
                // (`@let`/`@if`/`@for`/`@match`) is a sibling that must be
                // returned to `parse_nodes` so it hits its proper handler —
                // consuming it here would misparse e.g. a following `@let`'s
                // struct-literal initializer as markup.
                if matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '@')
                    && matches!(trees.get(i + 1), Some(TokenTree::Ident(id)) if *id == "else")
                {
                    i += 1;
                    // Reset for the `@else` / `@else if …` continuation.
                    in_pattern = false;
                    saw_cond = false;
                    continue;
                }
                break;
            }
            // Any other token (paren/bracket group, operator, literal) is part of
            // the condition/iterator expression.
            _ => {
                if !in_pattern {
                    saw_cond = true;
                }
                i += 1;
            }
        }
    }
    i
}

/// Skip an `@let` binding, consuming every token up to and including its
/// terminating top-level `;`. Unlike `@if`/`@for`/`@match` — whose braces are
/// markup — an `@let` binds a Rust expression, so any brace group in its
/// initializer (`Field { input: true }`) is struct-literal syntax and must NOT
/// be parsed as markup; descending into it would misread a Rust field named
/// `input` as an `<input>` element and fire a false positive. `start` points
/// just past `@let`. Group token trees are atomic, so the `;` seen at this
/// level is always the binding terminator, never one nested in a brace.
fn skip_let(trees: &[TokenTree], start: usize) -> usize {
    let mut i = start;
    while i < trees.len() {
        if matches!(&trees[i], TokenTree::Punct(p) if p.as_char() == ';') {
            return i + 1;
        }
        i += 1;
    }
    i
}

/// Skip a `@match` head and scan only the arm BODIES. `start` points just past
/// `@match`, at the scrutinee expression. The arm list is the brace group after
/// the scrutinee; its contents are handed to [`parse_match_arms`].
///
/// A bare struct-literal scrutinee needs parens (a paren group, skipped), but the
/// scrutinee is parsed with `parse_without_eager_brace`, so it MAY be a leading
/// block expression (`@match { x } { arms }`). In that case the first brace is
/// the scrutinee and the arm list is the next brace: skip a leading brace that is
/// immediately followed by another brace when no scrutinee token precedes it.
fn skip_match(trees: &[TokenTree], start: usize, nodes: &mut Vec<Node>) -> usize {
    let mut i = start;
    let mut saw_scrut = false;
    while i < trees.len() {
        if let TokenTree::Group(g) = &trees[i]
            && g.delimiter() == Delimiter::Brace
        {
            if !saw_scrut
                && matches!(trees.get(i + 1), Some(TokenTree::Group(n)) if n.delimiter() == Delimiter::Brace)
            {
                // Leading block-expression scrutinee; the arm list is next.
                i += 1;
                continue;
            }
            let arms: Vec<TokenTree> = g.stream().into_iter().collect();
            parse_match_arms(&arms, nodes);
            return i + 1;
        }
        saw_scrut = true;
        i += 1;
    }
    i
}

/// Parse a maud `@match` arm list. For each arm, skip the pattern tokens (and
/// any `if <guard>`) up to and including `=>`, then scan ONLY the arm body as
/// markup. This keeps arm patterns from being misread as elements while still
/// analyzing the real markup each arm renders.
fn parse_match_arms(trees: &[TokenTree], nodes: &mut Vec<Node>) {
    let mut i = 0;
    while i < trees.len() {
        // Locate the `=>` that separates this arm's pattern/guard from its body.
        // A fat arrow is two joined puncts `=` `>`; the first one after the arm
        // start delimits the body (guards use `>=`/`==`, never `=>`).
        let Some(arrow) = find_fat_arrow(trees, i) else {
            break;
        };
        i = parse_arm_body(trees, arrow + 2, nodes);
    }
}

/// The index of the `=` token of the first `=>` at or after `start`, if any.
fn find_fat_arrow(trees: &[TokenTree], start: usize) -> Option<usize> {
    let mut i = start;
    while i < trees.len() {
        if let TokenTree::Punct(p) = &trees[i]
            && p.as_char() == '='
            && matches!(trees.get(i + 1), Some(TokenTree::Punct(q)) if q.as_char() == '>')
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Scan a single `@match` arm body starting at `start` (just past `=>`) and
/// return the index of the next arm. A brace-group body is parsed as markup; any
/// other shape (`(splice)`, string literal, single element) is collected up to
/// the next top-level comma and parsed as markup. An unrecognized/ambiguous
/// shape is simply skipped — a missed check is acceptable here, a false positive
/// is not.
fn parse_arm_body(trees: &[TokenTree], start: usize, nodes: &mut Vec<Node>) -> usize {
    if let Some(TokenTree::Group(g)) = trees.get(start)
        && g.delimiter() == Delimiter::Brace
    {
        nodes.extend(parse_markup(&g.stream()));
        let mut next = start + 1;
        if matches!(trees.get(next), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
            next += 1;
        }
        return next;
    }
    // Non-brace body: collect up to the next top-level comma (group contents are
    // atomic token trees, so any comma seen here is an arm separator).
    let mut j = start;
    while j < trees.len() {
        if matches!(&trees[j], TokenTree::Punct(p) if p.as_char() == ',') {
            break;
        }
        j += 1;
    }
    nodes.extend(parse_nodes(&trees[start..j]));
    if j < trees.len() { j + 1 } else { j }
}

/// Parse a single element starting at `trees[start]` (an ident, or a leading
/// `.`/`#` shorthand denoting maud's implicit `div`). Returns the element and the
/// index just past it.
fn parse_element(trees: &[TokenTree], start: usize) -> (Element, usize) {
    let line = trees[start].span().start().line;
    // A leading `.`/`#` with no explicit tag name is an implicit `div`; the
    // shorthand class/id begins AT `start` and the tag is `div`. Otherwise the
    // tag is the ident at `start` and shorthands begin just after it.
    let (name, shorthand_start) = match &trees[start] {
        TokenTree::Punct(p) if matches!(p.as_char(), '#' | '.') => ("div".to_owned(), start),
        other => (ident_string(other), start + 1),
    };
    let mut attrs = Vec::new();
    let mut i = parse_shorthand(trees, shorthand_start, &mut attrs);
    let mut children = Vec::new();
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Punct(p) if p.as_char() == ';' => {
                i += 1;
                break;
            }
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                children = parse_markup(&g.stream());
                i += 1;
                break;
            }
            TokenTree::Punct(p) if matches!(p.as_char(), '#' | '.') => {
                i = parse_shorthand(trees, i, &mut attrs);
            }
            TokenTree::Ident(_) => {
                let (attr, next) = parse_attr(trees, i);
                attrs.push(attr);
                i = next;
            }
            // Anything else (a stray splice, operator, …) ends the element.
            _ => break,
        }
    }
    (
        Element {
            name,
            line,
            attrs,
            children,
        },
        i,
    )
}

/// Parse maud `#id` / `.class` shorthands, recording `#id` as an `id` attribute.
///
/// Each shorthand's value is a maud `HtmlNameOrMarkup`: a static name (`#bar`,
/// `.foo`), a dynamic splice (`#(expr)`, `.(expr)` — a paren-group Markup), a
/// braced markup group (`.{ … }`), or a MARKUP control value
/// (`.@if primary { "primary" }`); a class may then carry a `[cond]` toggler
/// (`.foo[active]`). A dynamic/markup id value is recorded as `Dynamic`
/// (unresolvable, like `id=(expr)`). Consuming the value and toggler is
/// essential: leaving a `(expr)`/`{ … }`/`@…`/`[cond]` group unconsumed ends the
/// element early, orphaning its real `{ … }` body and firing false positives on
/// the parent (e.g. `button.(cls) { "Save" }` or `button.@if c { "x" } { "Save" }`
/// losing its text).
fn parse_shorthand(trees: &[TokenTree], start: usize, attrs: &mut Vec<Attr>) -> usize {
    let mut i = start;
    while let Some(TokenTree::Punct(p)) = trees.get(i) {
        let sym = p.as_char();
        if sym != '#' && sym != '.' {
            break;
        }
        let is_id = sym == '#';
        i += 1; // consume the `#`/`.` — guarantees progress even with no name.
        match trees.get(i) {
            // Dynamic shorthand value: `#(expr)` / `.(expr)`, or a braced markup
            // group `#{ … }` / `.{ … }` — an unresolvable markup value. (A body
            // brace never follows a `#`/`.` directly, so this is the class/id
            // value, never the element body.)
            Some(TokenTree::Group(g))
                if matches!(g.delimiter(), Delimiter::Parenthesis | Delimiter::Brace) =>
            {
                if is_id {
                    attrs.push(Attr {
                        name: "id".to_owned(),
                        value: AttrValue::Dynamic,
                    });
                }
                i += 1;
            }
            // Markup-valued shorthand via a maud control:
            // `.@if c { "x" }` / `#@match … { … }`. Fully consume the control
            // (head + brace group(s) + any `@else` chain) so the element's real
            // body that follows is still parsed. The value is unresolvable, so an
            // id records as `Dynamic`.
            Some(TokenTree::Punct(q)) if q.as_char() == '@' => {
                if is_id {
                    attrs.push(Attr {
                        name: "id".to_owned(),
                        value: AttrValue::Dynamic,
                    });
                }
                i = skip_shorthand_control(trees, i + 1);
            }
            // Static shorthand value: `#bar` / `.foo` (possibly hyphenated).
            _ => {
                let (name, next) = read_name(trees, i);
                if next > i {
                    if is_id {
                        attrs.push(Attr {
                            name: "id".to_owned(),
                            value: AttrValue::Literal(name),
                        });
                    }
                    i = next;
                }
            }
        }
        // Optional `[cond]` toggler (`.foo[active]`).
        if matches!(trees.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket)
        {
            i += 1;
        }
    }
    i
}

/// Consume a markup-valued class/id control value (`.@if c { "x" }`), returning
/// the index just past it. `start` points just past the `@`, at the control
/// keyword. Reuses the same control/match/let skippers as node parsing; any
/// markup they descend into is discarded — a class/id value is a maud
/// `NoElement` context and can render no element to scan.
fn skip_shorthand_control(trees: &[TokenTree], start: usize) -> usize {
    let mut discard = Vec::new();
    if matches!(trees.get(start), Some(TokenTree::Ident(id)) if *id == "match") {
        skip_match(trees, start + 1, &mut discard)
    } else if matches!(trees.get(start), Some(TokenTree::Ident(id)) if *id == "let") {
        skip_let(trees, start + 1)
    } else {
        skip_control(trees, start, &mut discard)
    }
}

/// Parse one attribute (`name`, `name="v"`, `name=(expr)`, `name[cond]`, …).
fn parse_attr(trees: &[TokenTree], start: usize) -> (Attr, usize) {
    let (name, i) = read_name(trees, start);
    match trees.get(i) {
        Some(TokenTree::Punct(p)) if p.as_char() == '=' => {
            let (value, next) = read_attr_value(trees, i + 1);
            (Attr { name, value }, next)
        }
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => (
            Attr {
                name,
                value: AttrValue::Boolean,
            },
            i + 1,
        ),
        Some(TokenTree::Punct(p)) if p.as_char() == '?' => {
            let next = if matches!(trees.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket)
            {
                i + 2
            } else {
                i + 1
            };
            (
                Attr {
                    name,
                    value: AttrValue::Boolean,
                },
                next,
            )
        }
        _ => (
            Attr {
                name,
                value: AttrValue::Boolean,
            },
            i,
        ),
    }
}

/// Read an attribute value token following `=`.
fn read_attr_value(trees: &[TokenTree], i: usize) -> (AttrValue, usize) {
    match trees.get(i) {
        Some(TokenTree::Literal(lit)) => string_literal_value(lit)
            .map_or((AttrValue::Dynamic, i + 1), |v| {
                (AttrValue::Literal(v), i + 1)
            }),
        Some(TokenTree::Group(_) | TokenTree::Ident(_)) => (AttrValue::Dynamic, i + 1),
        _ => (AttrValue::Dynamic, i),
    }
}

/// Read a (possibly hyphenated or colon-qualified) name — `aria-label`,
/// `hx-post`, `x-on:click`, `svg:path`, `for` — starting at `start`. Returns the
/// joined name and the index just past it. maud names are a
/// `Punctuated<HtmlNameFragment, ':' | '-'>`, so both `-` and `:` continue the
/// name; consuming the whole name (including a colon-qualified suffix like
/// `x-on:click`) is essential — otherwise the leftover `:` ends the element
/// early and orphans its real `{ … }` body, firing a false `button-name`/
/// `link-name` on the parent.
fn read_name(trees: &[TokenTree], start: usize) -> (String, usize) {
    let Some(mut name) = name_segment(trees.get(start)) else {
        return (String::new(), start);
    };
    let mut i = start + 1;
    while let Some(TokenTree::Punct(p)) = trees.get(i) {
        let sep = p.as_char();
        if sep != '-' && sep != ':' {
            break;
        }
        let Some(seg) = name_segment(trees.get(i + 1)) else {
            break;
        };
        name.push(sep);
        name.push_str(&seg);
        i += 2;
    }
    (name, i)
}

/// The identifier/literal text of a single name segment.
fn name_segment(tree: Option<&TokenTree>) -> Option<String> {
    match tree {
        Some(TokenTree::Ident(id)) => Some(id.to_string()),
        Some(TokenTree::Literal(lit)) => string_literal_value(lit),
        _ => None,
    }
}

fn ident_string(tree: &TokenTree) -> String {
    match tree {
        TokenTree::Ident(id) => id.to_string(),
        _ => String::new(),
    }
}

/// The unescaped value of a string literal, or `None` for a non-string literal.
fn string_literal_value(lit: &Literal) -> Option<String> {
    syn::parse2::<syn::LitStr>(TokenStream::from(TokenTree::Literal(lit.clone())))
        .ok()
        .map(|s| s.value())
}

/// The text of a literal node — the string value if it is a string literal,
/// otherwise its raw token text (e.g. a bare number).
fn literal_text(lit: &Literal) -> String {
    string_literal_value(lit).unwrap_or_else(|| lit.to_string())
}

// ── Analysis ───────────────────────────────────────────────────────────────

/// Analyze one `html!` block's node tree, applying every rule.
fn analyze_block(nodes: &[Node], file: &str, scan: &mut Scan) {
    let mut label_fors = BTreeSet::new();
    let mut dynamic_label_for = false;
    collect_labels(nodes, &mut label_fors, &mut dynamic_label_for);
    walk(nodes, file, &label_fors, dynamic_label_for, false, scan);
}

/// Gather every `<label for="…">` literal in the block, and note whether any
/// label's `for` is a splice (which makes association unresolvable).
fn collect_labels(nodes: &[Node], fors: &mut BTreeSet<String>, dynamic: &mut bool) {
    for node in nodes {
        match node {
            Node::Element(el) => {
                // A `<label>` inside an `aria-hidden="true"` subtree is not in the
                // accessibility tree, so its `for` association contributes no
                // accessible name — skip the element and its subtree entirely,
                // consistent with `walk`'s name derivation. A dynamic/spliced
                // `aria-hidden=(…)` stays non-hidden (conservative).
                if is_aria_hidden(el) {
                    continue;
                }
                if el.name.eq_ignore_ascii_case("label") {
                    match el.attr("for") {
                        // Only a label that actually provides a name satisfies the
                        // association: an empty `<label for=..>` contributes no
                        // accessible name, so recording it would let an unlabeled
                        // control pass. A dynamic (spliced) body still counts, per
                        // the non-failing-splice convention (`label_provides_name`).
                        Some(AttrValue::Literal(v)) if label_provides_name(el) => {
                            fors.insert(v.clone());
                        }
                        Some(AttrValue::Dynamic) if label_provides_name(el) => *dynamic = true,
                        _ => {}
                    }
                }
                collect_labels(&el.children, fors, dynamic);
            }
            // A spliced fragment `(expr)` could render a `<label for=..>`, but
            // only one that is a DIRECT SIBLING of a control can label it. That
            // sibling-scoped suppression is handled in `walk`; recording it here
            // would (wrongly) mark the WHOLE block possibly-labeled, so an
            // unrelated nested splice like `p { (user.name) }` would suppress the
            // rule for every control in the block (false negative).
            Node::Dynamic { .. } | Node::Text(_) => {}
        }
    }
}

/// Label-association context threaded through the rule walk.
struct LabelCtx<'a> {
    /// Static `<label for="…">` ids collected across the block.
    fors: &'a BTreeSet<String>,
    /// A `<label for=(…)>` with a dynamic (spliced) target exists somewhere in
    /// the block — it could associate with any control, so it is block-wide.
    dynamic_for: bool,
    /// A splice that is a DIRECT SIBLING of the control could render a `<label>`
    /// fragment for it — sibling-scoped, recomputed per node list.
    sibling_splice: bool,
    /// We are inside a `<label>` that provides a name (which wraps the control).
    in_named_label: bool,
}

/// Recursively apply rules, tracking whether we are inside a `<label>` that
/// provides a name (so a control it wraps is considered labeled).
fn walk(
    nodes: &[Node],
    file: &str,
    fors: &BTreeSet<String>,
    dynamic_for: bool,
    in_named_label: bool,
    scan: &mut Scan,
) {
    // A splice that is a DIRECT SIBLING of a control could render a `<label>`
    // fragment for it, so its presence in THIS sibling list makes the
    // association unresolvable. A splice nested inside another element belongs to
    // a different sibling list and must not suppress controls here.
    let sibling_splice = nodes
        .iter()
        .any(|n| matches!(n, Node::Dynamic { splice: true }));
    let ctx = LabelCtx {
        fors,
        dynamic_for,
        sibling_splice,
        in_named_label,
    };
    for node in nodes {
        let Node::Element(el) = node else {
            continue;
        };
        // An element removed from the accessibility tree by `aria-hidden="true"`
        // on ITSELF needs no accessible name, and neither does its subtree — skip
        // both rule application and recursion for it.
        if is_aria_hidden(el) {
            continue;
        }
        apply_rules(el, file, &ctx, scan);
        let child_named_label =
            in_named_label || (el.name.eq_ignore_ascii_case("label") && label_provides_name(el));
        walk(
            &el.children,
            file,
            fors,
            dynamic_for,
            child_named_label,
            scan,
        );
    }
}

fn apply_rules(el: &Element, file: &str, ctx: &LabelCtx, scan: &mut Scan) {
    match el.name.to_ascii_lowercase().as_str() {
        // An `<img>` is named by `alt` (even empty `alt=""`, the decorative
        // marker), or — per the axe `image-alt` checks — by an
        // `aria-label`/`aria-labelledby`, a non-empty `title`, or an explicit
        // presentational role. Only a bare image with none of these is flagged.
        "img"
            if !el.has_attr("alt")
                && !el.has_aria_name()
                && !el.has_title_name()
                && !is_presentational(el) =>
        {
            scan.push(Rule::ImageAlt, "img", el.line, file);
        }
        name @ ("input" | "select" | "textarea") => {
            check_field(el, name, file, ctx, scan);
        }
        "button" if !named_content(el) => {
            scan.push(Rule::ButtonName, "button", el.line, file);
        }
        "a" if el.has_attr("href") && !named_content(el) => {
            scan.push(Rule::LinkName, "a", el.line, file);
        }
        _ => {}
    }
}

/// Rule 2: a form control needs an associated label or accessible name.
fn check_field(el: &Element, name: &str, file: &str, ctx: &LabelCtx, scan: &mut Scan) {
    // `<input type=…>` handling. Literal submit/hidden/button/reset/image
    // controls need no visible label. A spliced `type=(…)` is unresolvable —
    // it could be one of those non-labeling types, so skip rather than misfire
    // on valid markup (per the non-failing-splice convention).
    if name == "input" {
        match el.attr("type") {
            Some(AttrValue::Literal(ty))
                if ["hidden", "submit", "button", "reset", "image"]
                    .contains(&ty.to_ascii_lowercase().as_str()) =>
            {
                return;
            }
            Some(AttrValue::Dynamic) => return,
            _ => {}
        }
    }
    if el.has_aria_name() || el.has_title_name() || ctx.in_named_label {
        return;
    }
    match el.attr("id") {
        // A static id resolves against the collected `<label for>` set. A
        // dynamic `<label for=(…)>` elsewhere (`dynamic_for`) could target it,
        // and a splice that is a DIRECT SIBLING of this control could render a
        // `<label for=..>` fragment for it — either makes the association
        // unresolvable, so skip rather than risk a false positive.
        Some(AttrValue::Literal(id)) => {
            if ctx.fors.contains(id) || ctx.dynamic_for || ctx.sibling_splice {
                return;
            }
        }
        // A spliced id cannot be matched to a label — skip rather than misfire.
        Some(AttrValue::Dynamic) => return,
        _ => {}
    }
    scan.push(Rule::Label, name, el.line, file);
}

/// Whether an element has an accessible name from its own attributes or content:
/// an `aria-label`/`aria-labelledby` on the control itself, or — among the
/// descendants that are not hidden from the accessibility tree — visible text, a
/// dynamic (spliced) body, or a named child image. A dynamic subtree is
/// unresolvable, so it is treated as named (skip rather than misfire).
fn named_content(el: &Element) -> bool {
    el.has_aria_name() || el.has_title_name() || content_provides_name(&el.children)
}

/// Whether an element is hidden from the accessibility tree via a literal
/// `aria-hidden="true"`. Per ARIA such a subtree contributes nothing to an
/// ancestor's accessible name. A dynamic/spliced `aria-hidden=(…)` is
/// unresolvable and treated as *not* hidden (conservative — don't suppress a
/// name on an unresolved splice); `aria-hidden="false"` or an absent attribute
/// are likewise not hidden.
fn is_aria_hidden(el: &Element) -> bool {
    matches!(el.attr("aria-hidden"), Some(AttrValue::Literal(v)) if v.eq_ignore_ascii_case("true"))
}

/// Whether descendant content contributes an accessible name to an enclosing
/// control: a dynamic (spliced) node, non-empty visible text, or a named
/// `<img>`. Subtrees hidden via a literal `aria-hidden="true"` are excluded —
/// per ARIA their text does not count toward the ancestor's accessible name.
fn content_provides_name(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        // Only a genuine `(expr)`/`[..]` splice renders text that could name the
        // control; an `@`-control head (`splice: false`) renders nothing itself
        // (its branch bodies were already parsed out), so it is not a name.
        Node::Dynamic { splice } => *splice,
        Node::Text(t) => !t.trim().is_empty(),
        Node::Element(e) if is_aria_hidden(e) => false,
        Node::Element(e) => {
            // A descendant contributes a name via its own `aria-label`/
            // `aria-labelledby`/`title` (used during the name-from-content
            // traversal), a named child `<img alt>`, or its own descendants.
            e.has_aria_name()
                || e.has_title_name()
                || (e.name.eq_ignore_ascii_case("img") && attr_is_present_name(e.attr("alt")))
                || content_provides_name(&e.children)
        }
    })
}

/// Whether a `<label>` provides an accessible name to a control (whether it
/// wraps the control or associates via `for`). This uses the SAME accessible-
/// name walk as buttons and links (`content_provides_name`): visible text or a
/// dynamic (spliced) body counts, a named child `<img alt>` counts, and an
/// `aria-hidden="true"` subtree does NOT — its text is not an accessible name.
fn label_provides_name(el: &Element) -> bool {
    content_provides_name(&el.children)
}

// ── Output ─────────────────────────────────────────────────────────────────

const PRIMITIVE_NOTE: &str = "Note: code using the typed autumn_web::a11y primitives (Img, Button, \
Link, MenuItem, TextField) is proven accessible at compile time and is \
intentionally not re-scanned. This pass targets raw html! markup that bypasses \
those primitives.";

/// Verify `root`, print a report, and return the process exit code (non-zero
/// when findings meet the failure threshold). The `autumn a11y verify` command
/// calls this with the resolved project path and `std::process::exit`s on it.
#[must_use]
pub fn run_in(root: &Path, opts: A11yVerifyOptions) -> i32 {
    // An unreadable scan root is a hard failure, not an empty (PASS) audit:
    // surface the io error on stderr and exit non-zero so a CI path typo cannot
    // silently disable the whole check.
    let scan = match scan_project(root) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("error: cannot read scan path '{}': {err}", root.display());
            return 1;
        }
    };
    let report = Report::from_scan(scan);
    match opts.format {
        OutputFormat::Json => print_json(&report),
        OutputFormat::Text => print_text(&report, opts.strict),
    }
    report.exit_code(opts.strict)
}

fn print_json(report: &Report) {
    match serde_json::to_string_pretty(report) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("autumn a11y verify: failed to serialize report: {err}"),
    }
}

fn print_text(report: &Report, strict: bool) {
    println!("autumn a11y verify");
    println!(
        "  scanned {} html! block(s) across {} .rs file(s)",
        report.html_blocks, report.files_scanned
    );
    println!("  {PRIMITIVE_NOTE}");

    if report.findings.is_empty() {
        println!("\nResult: PASS — no accessibility violations in raw html! markup.");
        return;
    }

    println!("\n  findings:");
    for f in &report.findings {
        println!(
            "    {}:{}: <{}> [WCAG {}] {} — {}",
            f.file, f.line, f.element, f.wcag, f.severity, f.message
        );
        println!("        hint: {}", f.hint);
    }

    println!(
        "\n  summary: {} Critical, {} Serious, {} Moderate ({} total)",
        report.summary.critical,
        report.summary.serious,
        report.summary.moderate,
        report.summary.total
    );
    if report.exit_code(strict) == 0 {
        println!("Result: PASS with findings — below the failure threshold.");
    } else {
        println!(
            "Result: FAIL — fix the accessibility violations above (or use the typed primitives)."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan a single `html!`-bearing source snippet and return its findings.
    fn findings_for(src: &str) -> Vec<Finding> {
        let mut scan = Scan::default();
        scan_source(src, "view.rs", &mut scan);
        scan.findings
    }

    /// The set of rule ids flagged for a snippet.
    fn rule_ids(src: &str) -> Vec<&'static str> {
        let mut ids: Vec<&'static str> = findings_for(src).iter().map(|f| f.rule_id).collect();
        ids.sort_unstable();
        ids
    }

    fn wrap(markup: &str) -> String {
        format!("fn view() {{ let _ = html! {{ {markup} }}; }}")
    }

    // ── Rule 1: image-alt ──────────────────────────────────────────────────

    #[test]
    fn alt_less_img_is_flagged() {
        let src = wrap(r#"img src="x.png";"#);
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "image-alt");
        assert_eq!(f[0].wcag, "1.1.1");
        assert_eq!(f[0].severity, Severity::Serious);
    }

    #[test]
    fn img_with_alt_is_clean() {
        let src = wrap(r#"img src="x.png" alt="A logo";"#);
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn img_with_empty_alt_is_clean() {
        // An explicit decorative alt="" is a valid accessible marker.
        let src = wrap(r#"img src="divider.png" alt="";"#);
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn img_with_dynamic_alt_is_clean() {
        let src = wrap(r#"img src="x.png" alt=(caption);"#);
        assert!(findings_for(&src).is_empty());
    }

    // ── Rule 2: label ──────────────────────────────────────────────────────

    #[test]
    fn unlabeled_input_is_flagged() {
        let src = wrap(r#"input type="text" id="name";"#);
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "label");
        assert_eq!(f[0].wcag, "1.3.1 / 3.3.2 / 4.1.2");
        assert_eq!(f[0].element, "input");
    }

    #[test]
    fn input_with_matching_label_for_is_clean() {
        let src = wrap(r#"label for="name" { "Name" } input type="text" id="name";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn input_with_aria_label_is_clean() {
        let src = wrap(r#"input type="text" aria-label="Search";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn input_with_empty_aria_label_is_flagged() {
        let src = wrap(r#"input type="text" aria-label="";"#);
        assert_eq!(rule_ids(&src), vec!["label"]);
    }

    #[test]
    fn wrapped_label_with_text_is_clean() {
        let src = wrap(r#"label { "Name" input type="text"; }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn hidden_input_needs_no_label() {
        let src = wrap(r#"input type="hidden" name="_csrf";"#);
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn unlabeled_select_and_textarea_are_flagged() {
        let src = wrap(r#"select id="role" { } textarea id="bio" { }"#);
        assert_eq!(rule_ids(&src), vec!["label", "label"]);
    }

    #[test]
    fn input_with_dynamic_id_is_not_flagged() {
        // A spliced id cannot be matched to a label — skip rather than misfire.
        let src = wrap(r#"input type="text" id=(field_id);"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn empty_label_for_does_not_satisfy_association() {
        // An empty `<label for=..>` provides no accessible name, so the
        // associated control must still be flagged (regression: false negative).
        let src = wrap(r#"label for="email" { } input type="text" id="email";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn labeled_input_via_for_with_text_is_clean() {
        // A `<label for=..>` with real text satisfies the association.
        let src = wrap(r#"label for="email" { "Email" } input type="text" id="email";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn label_for_with_dynamic_body_is_clean() {
        // A spliced label body is unresolvable and counts as providing a name.
        let src = wrap(r#"label for="email" { (title) } input type="text" id="email";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn dynamic_for_label_with_empty_body_does_not_satisfy_association() {
        // A spliced-`for` label with a statically EMPTY body provides no
        // accessible name, so a static-id control must still be flagged. The
        // dynamic-`for` marker must carry the same `label_provides_name` guard as
        // the literal-`for` path (regression: false negative).
        let src = wrap(r#"label for=(id) { } input type="text" id="email";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn dynamic_for_label_with_dynamic_body_is_clean() {
        // A spliced-`for` label with a dynamic (spliced) body counts as providing
        // a name, per the non-failing-splice convention.
        let src = wrap(r#"label for=(id) { (title) } input type="text" id="email";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn dynamic_for_label_with_text_body_is_clean() {
        // A spliced-`for` label with real text provides a name.
        let src = wrap(r#"label for=(id) { "Email" } input type="text" id="email";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn input_with_dynamic_type_is_not_flagged() {
        // A spliced `type=(…)` is unresolvable — it may be a non-labeling type
        // like submit/button, so skip rather than misfire (regression: false
        // positive).
        let src = wrap(r#"input type=(kind) value="Save";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn input_with_literal_text_type_still_flagged() {
        // A literal text-like type with no label is still flagged.
        let src = wrap(r#"input type="text";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn spliced_label_fragment_beside_control_is_not_flagged() {
        // A label composed as a separate fragment and spliced next to the raw
        // control could render a `<label for=..>`; the association is
        // unresolvable, so per the non-failing-splice convention the control is
        // treated as possibly-labeled (regression: false positive).
        let src = wrap(r#"(field_label) input id="email";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn unlabeled_input_with_no_splice_is_still_flagged() {
        // With no spliced fragment anywhere in the block, an unlabeled control
        // must still be flagged — the fragment marker must not suppress unrelated
        // static controls.
        let src = wrap(r#"input id="x";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    // ── Rule 3: button-name ────────────────────────────────────────────────

    #[test]
    fn empty_button_is_flagged() {
        let src = wrap(r"button { }");
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "button-name");
        assert_eq!(f[0].wcag, "4.1.2");
    }

    #[test]
    fn button_with_text_is_clean() {
        let src = wrap(r#"button { "Save" }"#);
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn button_with_aria_label_is_clean() {
        let src = wrap(r#"button aria-label="Close" { }"#);
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn icon_button_with_alt_image_is_clean() {
        let src = wrap(r#"button { img src="trash.svg" alt="Delete"; }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn button_with_dynamic_content_is_not_flagged() {
        let src = wrap(r"button { (label_text) }");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn button_with_only_control_head_is_flagged() {
        // The only child is an `@if` control HEAD (`splice: false`), whose branch
        // bodies were parsed out — it renders no accessible text. A definitively
        // nameless button must still be flagged (regression: false negative — a
        // control head was wrongly counted as a name).
        let src = wrap(r"button { @if disabled { } }");
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "button-name");
    }

    #[test]
    fn button_with_real_splice_content_is_clean() {
        // A genuine `(expr)` splice body could render the button's name, so it is
        // treated as named (unchanged; guards against over-correcting the head fix).
        let src = wrap(r"button { (label) }");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    // ── Rule 4: link-name ──────────────────────────────────────────────────

    #[test]
    fn textless_anchor_is_flagged() {
        let src = wrap(r#"a href="/x" { }"#);
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "link-name");
        assert_eq!(f[0].wcag, "2.4.4 / 4.1.2");
    }

    #[test]
    fn anchor_with_text_is_clean() {
        let src = wrap(r#"a href="/x" { "About us" }"#);
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn anchor_without_href_is_not_flagged() {
        // Rule 4 only fires for anchors that carry an href.
        let src = wrap(r"a { }");
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn icon_anchor_with_aria_label_is_clean() {
        let src = wrap(r#"a href="https://example.com" aria-label="GitHub" { }"#);
        assert!(findings_for(&src).is_empty());
    }

    // ── aria-hidden subtrees don't count toward the accessible name ─────────

    #[test]
    fn icon_button_with_aria_hidden_glyph_is_flagged() {
        // The only content is an `aria-hidden="true"` glyph, which per ARIA does
        // not contribute an accessible name — the button is effectively nameless.
        let src = wrap(r#"button { span aria-hidden="true" { "×" } }"#);
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "button-name");
    }

    #[test]
    fn icon_anchor_with_aria_hidden_glyph_is_flagged() {
        let src = wrap(r#"a href="/x" { span aria-hidden="true" { "🔍" } }"#);
        let f = findings_for(&src);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule_id, "link-name");
    }

    #[test]
    fn button_with_hidden_glyph_and_visible_text_is_clean() {
        // The visible "Close" text still provides an accessible name.
        let src = wrap(r#"button { span aria-hidden="true" { "×" } "Close" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn button_with_hidden_glyph_and_aria_label_is_clean() {
        // The `aria-label` on the control itself provides the name.
        let src = wrap(r#"button aria-label="Close" { span aria-hidden="true" { "×" } }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn button_with_dynamic_aria_hidden_glyph_is_not_flagged() {
        // A spliced `aria-hidden=(…)` is unresolvable, so the subtree is treated
        // as not hidden and its text still counts — conservative, no new false
        // positive on valid dynamic markup.
        let src = wrap(r#"button { span aria-hidden=(flag) { "×" } }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn label_with_only_aria_hidden_text_does_not_satisfy_association() {
        // The `<label>` body is entirely `aria-hidden="true"`, which per ARIA
        // contributes no accessible name — the label check must use the same
        // hidden-excluding name walk as buttons/links, so the field is FLAGGED
        // (regression: false negative — hidden text was wrongly accepted).
        let src = wrap(r#"label for="q" { span aria-hidden="true" { "Search" } } input id="q";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn label_with_only_control_head_does_not_satisfy_association() {
        // The `<label>` body is only an `@if` control HEAD (`splice: false`),
        // which renders no accessible name — so the associated field must still be
        // FLAGGED (regression: false negative — a control head was wrongly counted
        // as a name).
        let src = wrap(r#"label for="q" { @if show { } } input id="q";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn label_with_alt_image_satisfies_association() {
        // A `<label>` whose only content is an `<img alt=..>` has an accessible
        // name via the alt text, so the associated field is CLEAN (regression:
        // false positive — the img alt was previously ignored).
        let src = wrap(r#"label for="q" { img src="s.svg" alt="Search"; } input id="q";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn label_with_plain_text_still_satisfies_association() {
        // Unchanged: a `<label for=..>` with real visible text names the field.
        let src = wrap(r#"label for="e" { "Email" } input id="e";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn empty_label_body_still_does_not_satisfy_association() {
        // Unchanged: an empty `<label for=..>` provides no accessible name, so
        // the associated field is FLAGGED.
        let src = wrap(r#"label for="e" { } input id="e";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn label_in_aria_hidden_container_does_not_satisfy_association() {
        // Regression (false negative): a `<label for=..>` that only exists inside
        // an `aria-hidden="true"` subtree is not in the accessibility tree, so it
        // provides no accessible name. `collect_labels` must stop descending into
        // hidden subtrees (like `walk`), so the visible input is FLAGGED.
        let src = wrap(r#"div aria-hidden="true" { label for="q" { "Search" } } input id="q";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn label_in_visible_container_satisfies_association() {
        // Unchanged: a `<label for=..>` inside a visible container is in the
        // accessibility tree and names the associated input, so it is CLEAN.
        let src = wrap(r#"div { label for="q" { "Search" } } input id="q";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    // ── Escape-hatch / dynamic limits ──────────────────────────────────────

    #[test]
    fn typed_primitive_splice_is_not_flagged() {
        // `(Img::new(..))` is a splice, not an `img` element, so the compile-time
        // guarantee is respected and nothing is re-scanned.
        let src = wrap(r"(autumn_web::a11y::Img::new(src, alt))");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn fully_dynamic_splice_is_not_flagged() {
        let src = wrap(r"(rendered_widget)");
        assert!(findings_for(&src).is_empty());
    }

    #[test]
    fn element_inside_control_block_is_still_scanned() {
        let src = wrap(r#"@if show { img src="x.png"; }"#);
        assert_eq!(rule_ids(&src), vec!["image-alt"]);
    }

    #[test]
    fn match_arm_pattern_named_input_is_not_flagged() {
        // Regression: an `@match` arm whose PATTERN is named `input` must not be
        // read as an `<input>` element. Parsing the arm list as markup produced a
        // false `label` finding on valid views.
        let src = wrap(r#"@match field { input => { "text" } }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn match_arm_body_real_input_is_still_flagged() {
        // The arm PATTERN is skipped, but the arm BODY is still scanned: a real
        // unlabeled `<input>` inside an arm body must still be flagged.
        let src = wrap(r"@match x { A => { input; } }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn match_arm_with_guard_does_not_misparse() {
        // A guard (`Pattern if cond => …`) sits between the pattern and `=>` and
        // must be skipped along with the pattern — no crash, no false finding.
        let src = wrap(r#"@match x { A if ready => { "ok" } B => { "no" } }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn match_multiple_arms_scan_each_body() {
        // Several arms, mixed body shapes: only the real markup in bodies counts.
        // The `input` pattern must not flag; the alt-less `<img>` body must.
        let src = wrap(r#"@match k { input => { "a" } other => { img src="x.png"; } }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["image-alt"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn let_initializer_struct_literal_is_not_parsed_as_markup() {
        // Regression: `@let props = Field { input: true };` binds a Rust
        // expression, so its brace group is struct-literal syntax, not markup.
        // Parsing it as markup misread the Rust field `input` as an `<input>`
        // element and fired a false `label` finding on valid views.
        let src = wrap(r#"@let props = Field { input: true }; div { "ok" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn let_binding_does_not_suppress_real_input_elsewhere() {
        // The `@let` initializer is skipped wholesale, but a real unlabeled
        // `<input>` later in the same markup must still be flagged.
        let src = wrap(r#"@let props = Field { input: true }; input type="text" id="name";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn let_with_plain_initializer_does_not_crash() {
        // A plain (non-struct) `@let` initializer parses without panicking.
        let src = wrap(r#"@let x = foo(); div { "ok" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn let_after_control_reaches_its_skip_handler() {
        // Regression: only `@else` continues a control chain. A `@let` following a
        // parsed control block must be returned to `parse_nodes` so it hits
        // `skip_let` — otherwise its `Field { input: true }` struct literal is
        // misparsed as markup and fires a bogus `<input>` finding (false positive).
        let src = wrap(r#"@if show { "ok" } @let props = Field { input: true };"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn if_else_chain_still_parses_and_flags_real_input() {
        // A genuine `@if … @else …` chain still parses across the `@else`, and a
        // real unlabeled `<input>` in the `@if` body is still flagged.
        let src = wrap(r#"@if c { input; } @else { "x" }"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn while_body_element_is_scanned() {
        // `@while cond { body }`: the body is markup and is scanned, so a real
        // unlabeled `<input>` in a `@while` body is still flagged.
        let src = wrap(r"@while more { input; }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn for_struct_pattern_is_not_parsed_as_markup() {
        // Regression (false positive): a `@for` loop destructuring a struct
        // pattern (`@for Field { input, .. } in fields`) has BRACES in the
        // pattern that are NOT markup. Parsing them misread the Rust field
        // `input` as an `<input>` element.
        let src = wrap(r#"@for Field { input, label } in fields { "row" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn for_body_real_input_is_still_flagged() {
        // The `@for` pattern is skipped, but the body is still scanned.
        let src = wrap(r"@for x in items { input; }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn if_let_struct_pattern_is_not_parsed_as_markup() {
        // Regression (false positive): `@if let Field { input } = props { .. }`
        // has a struct PATTERN brace that is not markup.
        let src = wrap(r#"@if let Field { input } = props { "ok" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn while_let_struct_pattern_is_not_parsed_as_markup() {
        let src = wrap(r#"@while let Field { input } = next() { "ok" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn if_let_body_real_input_is_still_flagged() {
        // The `if let` pattern is skipped, but the body is scanned.
        let src = wrap(r"@if let Some(x) = maybe { input; }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn if_block_expression_condition_is_not_parsed_as_markup() {
        // Regression (false positive): maud parses the condition with
        // `parse_without_eager_brace`, so a block-expression condition
        // (`@if { input } { body }`) is valid — the FIRST brace is the condition,
        // the LAST is the markup body. The condition must NOT be parsed as markup.
        let src = wrap(r#"@if { input } { "ok" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn if_block_expression_condition_body_is_still_scanned() {
        // The block-expression condition is skipped, but the body IS scanned: a
        // real unlabeled `<input>` in the body must still be flagged.
        let src = wrap(r"@if { ready } { input; }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn else_if_block_expression_condition_is_not_parsed_as_markup() {
        // The block-expression handling also applies across an `@else if` chain.
        let src = wrap(r#"@if a { "a" } @else if { input } { "b" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn if_body_then_sibling_block_still_scans_body() {
        // `@if a { body } { sibling }`: the first brace is the body (a condition
        // token `a` precedes it), the second is a sibling markup block. Both are
        // scanned, so a real `<input>` in EITHER is flagged.
        let src = wrap(r"@if a { input; } { button { } }");
        assert_eq!(
            rule_ids(&src),
            vec!["button-name", "label"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn match_block_expression_scrutinee_is_not_parsed_as_markup() {
        // Regression (false positive): a `@match` scrutinee is parsed with
        // `parse_without_eager_brace`, so a block-expression scrutinee
        // (`@match { input } { arms }`) is valid — the first brace is the
        // scrutinee, the second is the arm list. The scrutinee must not be markup.
        let src = wrap(r#"@match { input } { A => { "a" } }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn match_block_expression_scrutinee_arm_body_still_scanned() {
        let src = wrap(r"@match { flag } { A => { input; } }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    // ── maud class/id shorthand (dynamic + toggled) ─────────────────────────

    #[test]
    fn dynamic_class_shorthand_does_not_orphan_body() {
        // Regression (false positive): a dynamic class shorthand `.(expr)` must
        // be consumed, else the element ends early and its `{ … }` body is
        // orphaned — here the button would lose its "Save" text and be flagged.
        let src = wrap(r#"button.(cls) { "Save" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn dynamic_class_shorthand_empty_button_still_flagged() {
        // The shorthand is consumed, but a genuinely empty button is still caught.
        let src = wrap(r"button.(cls) { }");
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn toggled_class_shorthand_does_not_orphan_body() {
        // A toggled class `.name[cond]` must be consumed along with its bracket.
        let src = wrap(r#"a.icon[active] href="/x" { "Home" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn toggled_class_shorthand_empty_link_still_flagged() {
        let src = wrap(r#"a.icon[active] href="/x" { }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["link-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn dynamic_id_shorthand_is_treated_as_dynamic() {
        // A dynamic id `#(expr)` cannot be matched to a label — skip like
        // `id=(expr)` rather than misfire.
        let src = wrap(r#"input#(field_id) type="text";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn static_id_shorthand_matches_label_for() {
        // A static `#id` shorthand records an id that a `<label for>` can match.
        let src = wrap(r#"label for="name" { "Name" } input#name type="text";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn unlabeled_id_shorthand_input_is_flagged() {
        // A static `#id` shorthand with no matching label is still flagged.
        let src = wrap(r#"input#name type="text";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    // ── maud implicit-div leading shorthand (no explicit tag) ───────────────

    #[test]
    fn leading_class_shorthand_is_implicit_div_not_input() {
        // Regression (false positive): `.input { }` is an implicit `<div
        // class="input">`, NOT an `<input>` element — the class token is a VALUE,
        // not a tag. A `<div>` triggers no rule, so nothing is flagged.
        let src = wrap(r".input { }");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn leading_id_shorthand_is_implicit_div_not_button() {
        // `#button { }` is an implicit `<div id="button">`, not a `<button>`.
        let src = wrap(r"#button { }");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn leading_class_shorthand_with_text_is_implicit_div() {
        // `.input { "x" }` is still an implicit `<div>`; its text body is not a
        // control name obligation.
        let src = wrap(r#".input { "x" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn implicit_div_wrapping_real_input_still_flags_inner_input() {
        // The implicit `<div class="card">` triggers no rule itself, but its body
        // is still scanned: a real unlabeled `<input>` inside is STILL flagged.
        let src = wrap(r".card { input; }");
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    // ── maud markup-valued class shorthand (`.@if c { "x" }`) ────────────────

    #[test]
    fn markup_valued_class_shorthand_does_not_truncate_button() {
        // Regression (false positive): `.@if primary { "primary" }` is a
        // MARKUP-valued class value; the button's real body is the FOLLOWING
        // `{ "Save" }`. The class control must be fully consumed so the body is
        // parsed — otherwise the button is analyzed as empty and falsely flagged.
        let src = wrap(r#"button.@if primary { "primary" } { "Save" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn markup_valued_class_shorthand_empty_button_still_flagged() {
        // The markup class value is consumed, but a genuinely empty body is still
        // caught (no over-correction into a false negative).
        let src = wrap(r#"button.@if primary { "primary" } { }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn markup_valued_class_shorthand_before_attr_and_body_is_clean() {
        // A markup class value followed by a real attribute and body: the anchor
        // keeps its `href` and its `{ "Link" }` text, so it is not flagged.
        let src = wrap(r#"a.@if c { "x" } href="/y" { "Link" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn splice_valued_class_shorthand_stays_clean() {
        // A splice-valued class `.(dynamic_class)` was already handled; it must
        // stay clean (the button keeps its "Save" body).
        let src = wrap(r#"button.(dynamic_class) { "Save" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    // ── colon-qualified maud names (x-on:click, data:foo, svg:path) ─────────

    #[test]
    fn colon_qualified_attr_name_does_not_orphan_button_body() {
        // Regression (false positive): maud names allow `:` (and `-`), so an
        // Alpine/Vue-style `x-on:click` attribute must be read as ONE name. If the
        // scanner stopped at the `:`, the leftover token ends the element early and
        // orphans its `{ "Save" }` body — the button is analyzed as empty and
        // falsely flagged.
        let src = wrap(r#"button x-on:click="save" { "Save" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn colon_qualified_attr_name_empty_button_still_flagged() {
        // The colon-qualified name is consumed, but a genuinely empty button is
        // still caught (no over-correction into a false negative).
        let src = wrap(r#"button x-on:click="save" { }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn colon_qualified_attr_on_link_is_clean() {
        // A colon-qualified `data:foo` attribute must not truncate the anchor: it
        // keeps its `href` and its `{ "Link" }` text, so it is not flagged.
        let src = wrap(r#"a href="/x" data:foo="y" { "Link" }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    // ── aria-hidden on the control itself ───────────────────────────────────

    #[test]
    fn aria_hidden_button_needs_no_name() {
        // A `<button aria-hidden="true">` is removed from the accessibility tree,
        // so it needs no accessible name (regression: false positive).
        let src = wrap(r#"button aria-hidden="true" { }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn aria_hidden_link_needs_no_name() {
        let src = wrap(r#"a href="/x" aria-hidden="true" { }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn aria_hidden_img_needs_no_alt() {
        let src = wrap(r#"img src="x.png" aria-hidden="true";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn aria_hidden_subtree_control_is_exempt() {
        // A control nested inside an `aria-hidden="true"` subtree is hidden too.
        let src = wrap(r#"div aria-hidden="true" { input type="text"; }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn visible_sibling_of_aria_hidden_control_still_flagged() {
        // The hidden button is exempt; the visible empty button beside it is not.
        let src = wrap(r#"button aria-hidden="true" { } button { }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn dynamic_aria_hidden_control_is_still_checked() {
        // A spliced `aria-hidden=(…)` is unresolvable → treated as NOT hidden, so
        // a nameless button is still flagged (no new false negative).
        let src = wrap(r"button aria-hidden=(flag) { }");
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    // ── title as a last-resort accessible name ──────────────────────────────

    #[test]
    fn button_with_title_is_clean() {
        // Per the ARIA accessible-name computation `title` is a last-resort name.
        let src = wrap(r#"button title="Close" { }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn button_with_empty_title_is_flagged() {
        // An empty `title` provides no name, so the button is still flagged.
        let src = wrap(r#"button title="" { }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    #[test]
    fn link_with_title_is_clean() {
        let src = wrap(r#"a href="/x" title="Home" { }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn input_with_title_is_clean() {
        let src = wrap(r#"input type="text" title="Full name";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn input_with_empty_title_is_flagged() {
        let src = wrap(r#"input type="text" title="";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    // ── img named via aria/title/role, not just alt ─────────────────────────

    #[test]
    fn img_with_aria_label_is_clean() {
        // An `<img>` named by `aria-label` has an accessible name (regression:
        // false positive — only `alt` was previously accepted).
        let src = wrap(r#"img src="x.png" aria-label="Logo";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn img_with_presentation_role_is_clean() {
        // A decorative `role="presentation"`/`role="none"` image needs no alt.
        let src = wrap(r#"img src="divider.png" role="presentation";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
        let src = wrap(r#"img src="divider.png" role="none";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn img_with_dynamic_role_is_clean() {
        // A spliced `role=(…)` is unresolvable and could be presentational — skip.
        let src = wrap(r"img src=(s) role=(r);");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn img_with_title_is_clean() {
        let src = wrap(r#"img src="x.png" title="Logo";"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn img_with_no_name_source_is_still_flagged() {
        // Baseline: an image with a non-presentational role and no name is caught.
        let src = wrap(r#"img src="x.png" role="img";"#);
        assert_eq!(
            rule_ids(&src),
            vec!["image-alt"],
            "{:?}",
            findings_for(&src)
        );
    }

    // ── name from a descendant's aria-label ──────────────────────────────────

    #[test]
    fn button_named_by_child_aria_label_is_clean() {
        // A descendant's `aria-label` contributes to the name-from-content walk.
        let src = wrap(r#"button { span aria-label="Close" { } }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn button_with_nameless_child_is_still_flagged() {
        // A child with no name of its own leaves the button nameless.
        let src = wrap(r"button { span { } }");
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    // ── sibling-scoped splice suppression (not block-wide) ──────────────────

    #[test]
    fn nested_splice_does_not_suppress_unrelated_control() {
        // Regression (false negative): a splice nested inside another element
        // (`p { (user.name) }`) is unrelated to labeling and must NOT suppress the
        // unlabeled-control rule for a control elsewhere in the block.
        let src = wrap(r#"p { (user.name) } input type="text" id="email";"#);
        assert_eq!(rule_ids(&src), vec!["label"], "{:?}", findings_for(&src));
    }

    #[test]
    fn direct_sibling_splice_still_suppresses_nested_control() {
        // A splice that is a DIRECT SIBLING of the control (even nested one level
        // deep) could render a `<label>` fragment for it — still suppressed.
        let src = wrap(r#"div { (field_label) input type="text" id="email"; }"#);
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn multiple_findings_across_one_block() {
        let src = wrap(r#"img src="a.png"; button { } a href="/x" { }"#);
        assert_eq!(
            rule_ids(&src),
            vec!["button-name", "image-alt", "link-name"]
        );
    }

    #[test]
    fn json_report_is_keyed_to_wcag() {
        let src = wrap(r#"img src="x.png";"#);
        let mut scan = Scan::default();
        scan_source(&src, "view.rs", &mut scan);
        let report = Report::from_scan(scan);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"wcag\":\"1.1.1\""), "{json}");
        assert!(json.contains("\"severity\":\"Serious\""), "{json}");
        assert!(json.contains("\"rule_id\":\"image-alt\""), "{json}");
    }

    // ── non-maud html! (JSX/Yew island) is skipped, not misparsed ───────────

    #[test]
    fn jsx_html_button_is_skipped() {
        // Regression (false positive): a DIFFERENT `html!` (e.g. a Yew/JSX island)
        // uses `<tag>…</tag>` angle-bracket syntax that maud never uses. Parsing it
        // as maud would strip the brackets and misread `<button></button>` as an
        // empty maud `button`. The body is detected as JSX and skipped — no finding.
        let src = wrap(r"<button>{label}</button>");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn jsx_html_div_input_is_skipped() {
        // A JSX island with self-closing tags (`<input/>`) is likewise skipped.
        let src = wrap(r"<div><input/></div>");
        assert!(findings_for(&src).is_empty(), "{:?}", findings_for(&src));
    }

    #[test]
    fn real_maud_button_still_analyzed_after_jsx_skip() {
        // The JSX skip must not silence real maud: an empty maud `button { }` (no
        // angle brackets) is STILL parsed and flagged.
        let src = wrap(r"button { }");
        assert_eq!(
            rule_ids(&src),
            vec!["button-name"],
            "{:?}",
            findings_for(&src)
        );
    }

    // ── End-to-end: red project → fix → green ──────────────────────────────

    #[test]
    fn end_to_end_red_then_green() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("view.rs");

        // Red: an alt-less image in raw html! ships silently until now.
        std::fs::write(&view, wrap(r#"img src="/logo.png";"#)).unwrap();
        let opts = A11yVerifyOptions {
            format: OutputFormat::Text,
            strict: false,
        };
        assert_eq!(run_in(dir.path(), opts), 1, "red project must fail CI");

        // Fix: add the alt attribute (or switch to Img::new).
        std::fs::write(&view, wrap(r#"img src="/logo.png" alt="Logo";"#)).unwrap();
        assert_eq!(run_in(dir.path(), opts), 0, "fixed project must pass CI");
    }

    #[test]
    fn exit_code_zero_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("view.rs"), wrap(r#"button { "Ok" }"#)).unwrap();
        let opts = A11yVerifyOptions {
            format: OutputFormat::Json,
            strict: false,
        };
        assert_eq!(run_in(dir.path(), opts), 0);
    }

    #[test]
    fn nonexistent_scan_root_fails_hard() {
        // A misspelled/unreadable scan root must be a hard non-zero failure, not
        // a zero-findings PASS — otherwise a CI path typo silently disables the
        // whole audit.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let opts = A11yVerifyOptions {
            format: OutputFormat::Text,
            strict: false,
        };
        assert_eq!(
            run_in(&missing, opts),
            1,
            "an unreadable scan root must fail, not report an empty clean audit"
        );
    }

    #[test]
    fn empty_but_readable_root_passes() {
        // A genuinely empty-but-readable directory has no findings and must still
        // pass (exit 0) — distinct from the unreadable-root failure above.
        let dir = tempfile::tempdir().unwrap();
        let opts = A11yVerifyOptions {
            format: OutputFormat::Text,
            strict: false,
        };
        assert_eq!(run_in(dir.path(), opts), 0);
    }
}

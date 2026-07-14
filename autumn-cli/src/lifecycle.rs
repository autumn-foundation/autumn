//! `autumn lifecycle check` / `autumn lifecycle diagram` — a build-time
//! soundness gate and lifecycle-diagram generator for `#[lifecycle]` state
//! machines (issue #1675).
//!
//! The `#[lifecycle]` proc-macro in `autumn-macros` already proves, *at compile
//! time*, that every declared `initial`/`terminal`/transition endpoint is a real
//! enum variant and that only declared edges are callable. What the type system
//! cannot see is the *shape* of the reachability graph the declared edges form.
//! This pass closes that gap with four structural checks over the declared
//! states and transitions of every `#[lifecycle]` enum in a project's source:
//!
//! - **existence** — `initial`, each terminal, and every transition endpoint
//!   must be a declared variant. (Redundant with the macro's by-construction
//!   guarantee for code that compiles, but this scanner parses source directly
//!   and must stand on its own.)
//! - **reachability** — every declared state must be reachable from `initial`
//!   by following transitions (BFS from `initial`).
//! - **dead-end** — every reachable, non-terminal state must be able to reach
//!   *some* terminal state (reverse BFS from the terminals).
//! - **terminal-source** — a declared terminal state must have no outgoing
//!   transition (a "movable terminal" is not actually terminal). This mirrors
//!   the by-construction compile error the macro emits.
//!
//! Like `autumn a11y verify` and `autumn i18n check`, this is a source scanner:
//! it parses each `.rs` file with `syn` and inspects enums carrying a
//! `#[lifecycle(...)]` attribute. It does **not** depend on the proc-macro crate;
//! the attribute grammar is re-parsed here with a standalone `syn` parser that
//! mirrors the macro's grammar exactly (`initial = <Ident>`,
//! `terminal(<Ident>, ...)`, `transitions(<From> -> <To>, ...)`).
//!
//! **Attribute-recognition limits.** The scanner recognizes the `#[lifecycle]`
//! macro under a bare `#[lifecycle(...)]` path, a qualified path whose last
//! segment is `lifecycle` (e.g. `#[autumn_web::lifecycle(...)]`), and a
//! *same-file* alias introduced by `use ...::lifecycle as <alias>;` (matched by
//! its terminal `lifecycle` rename). Like the `autumn a11y verify`
//! escape-hatch scanner, this is a best-effort textual pass, not a resolver: it
//! cannot follow an alias introduced in *another* file, nor one laundered
//! through a glob re-export (`pub use ...::lifecycle;` then `use crate::x::*;`).
//! Such an invocation is skipped by the scanner. This is a soundness gap only
//! for the scanner's reachability report, never for the state machine itself:
//! the `#[lifecycle]` typestate makes every undeclared transition a *compile
//! error*, so the by-construction guarantee holds however the macro is spelled —
//! this pass only adds the reachability-shape checks the type system cannot see.
//!
//! `lifecycle check` prints a human-readable report (or `--format json`) and
//! exits non-zero when any lifecycle is unsound — the CI gate. `lifecycle diagram`
//! emits a lifecycle diagram per lifecycle in Graphviz DOT or Mermaid
//! `stateDiagram-v2` form, highlighting the initial and terminal states.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, Token};

/// Output format for `autumn lifecycle check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable text (default).
    Text,
    /// Machine-readable JSON — also the lifecycle-graph artifact (AC5).
    Json,
}

/// Diagram rendering format for `autumn lifecycle diagram`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramFormat {
    /// Graphviz DOT (`digraph`).
    Dot,
    /// Mermaid `stateDiagram-v2` (default).
    Mermaid,
}

// ── Attribute grammar parser (mirrors autumn-macros/src/lifecycle.rs) ─────────

/// A single `From -> To` transition edge from the `transitions(...)` list.
struct Transition {
    from: Ident,
    to: Ident,
}

impl Parse for Transition {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let from: Ident = input.parse()?;
        input.parse::<Token![->]>()?;
        let to: Ident = input.parse()?;
        Ok(Self { from, to })
    }
}

/// Parsed `#[lifecycle(...)]` arguments. The grammar (and its error messages)
/// intentionally match the proc-macro so the CLI accepts exactly what compiles.
struct LifecycleArgs {
    initial: Ident,
    terminals: Vec<Ident>,
    transitions: Vec<Transition>,
}

impl Parse for LifecycleArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut initial: Option<Ident> = None;
        let mut terminals: Option<Vec<Ident>> = None;
        let mut transitions: Option<Vec<Transition>> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            match key.to_string().as_str() {
                "initial" => {
                    if initial.is_some() {
                        return Err(syn::Error::new_spanned(&key, "duplicate `initial`"));
                    }
                    input.parse::<Token![=]>()?;
                    initial = Some(input.parse()?);
                }
                "terminal" => {
                    if terminals.is_some() {
                        return Err(syn::Error::new_spanned(&key, "duplicate `terminal`"));
                    }
                    let content;
                    syn::parenthesized!(content in input);
                    let idents: Punctuated<Ident, Token![,]> =
                        content.parse_terminated(Ident::parse, Token![,])?;
                    if idents.is_empty() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "`terminal(...)` requires at least one terminal state",
                        ));
                    }
                    terminals = Some(idents.into_iter().collect());
                }
                "transitions" => {
                    if transitions.is_some() {
                        return Err(syn::Error::new_spanned(&key, "duplicate `transitions`"));
                    }
                    let content;
                    syn::parenthesized!(content in input);
                    let edges: Punctuated<Transition, Token![,]> =
                        content.parse_terminated(Transition::parse, Token![,])?;
                    if edges.is_empty() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "`transitions(...)` requires at least one `From -> To` edge",
                        ));
                    }
                    transitions = Some(edges.into_iter().collect());
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "unknown `#[lifecycle]` argument `{other}` — expected `initial`, `terminal`, or `transitions`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let initial = initial.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `initial = <Variant>` in `#[lifecycle(...)]`",
            )
        })?;
        let terminals = terminals.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `terminal(<Variant>, ...)` in `#[lifecycle(...)]`",
            )
        })?;
        let transitions = transitions.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `transitions(<From> -> <To>, ...)` in `#[lifecycle(...)]`",
            )
        })?;

        Ok(Self {
            initial,
            terminals,
            transitions,
        })
    }
}

// ── Lifecycle model ───────────────────────────────────────────────────────────

/// A lifecycle discovered in source: the enum name, its declared states (in enum
/// declaration order), and the parsed `#[lifecycle]` metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleDef {
    /// Enum identifier (e.g. `OrderState`).
    pub name: String,
    /// Declared variants, in enum declaration order.
    pub states: Vec<String>,
    /// The declared initial state.
    pub initial: String,
    /// The declared terminal states.
    pub terminals: Vec<String>,
    /// Declared `(from, to)` edges.
    pub transitions: Vec<(String, String)>,
}

// ── Analysis ─────────────────────────────────────────────────────────────────

/// The kind of soundness violation, keyed to a stable identifier used in text
/// and JSON output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViolationKind {
    /// A referenced state is not a declared variant.
    Existence,
    /// A declared state is not reachable from `initial`.
    Reachability,
    /// A reachable non-terminal state cannot reach any terminal.
    DeadEnd,
    /// A declared terminal state has an outgoing transition (a "movable
    /// terminal"): a terminal state must have no outgoing edges.
    TerminalSource,
}

impl ViolationKind {
    const fn tag(self) -> &'static str {
        match self {
            Self::Existence => "existence",
            Self::Reachability => "reachability",
            Self::DeadEnd => "dead-end",
            Self::TerminalSource => "terminal-source",
        }
    }
}

/// A single soundness violation with the offending state(s) and a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    pub kind: ViolationKind,
    /// The offending state name(s).
    pub states: Vec<String>,
    /// Human-readable description naming the state(s) and lifecycle.
    pub message: String,
}

/// A per-lifecycle analysis result — its declared shape plus any violations.
/// Serializes to the machine-readable lifecycle-graph artifact (AC5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LifecycleReport {
    pub name: String,
    pub states: Vec<String>,
    pub initial: String,
    pub terminals: Vec<String>,
    pub transitions: Vec<(String, String)>,
    pub violations: Vec<Violation>,
}

impl LifecycleReport {
    const fn is_sound(&self) -> bool {
        self.violations.is_empty()
    }
}

/// The full result of a `lifecycle check` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Number of `.rs` files scanned.
    pub files_scanned: usize,
    /// Per-lifecycle analysis results.
    pub lifecycles: Vec<LifecycleReport>,
    /// Malformed-attribute errors (each `file: message`). A non-empty list is a
    /// hard failure — a `#[lifecycle]` we could not parse is not a pass.
    pub errors: Vec<String>,
}

impl Report {
    /// Total violation count across every lifecycle.
    #[must_use]
    pub fn violation_count(&self) -> usize {
        self.lifecycles.iter().map(|w| w.violations.len()).sum()
    }

    /// Process exit code: 0 when every lifecycle is sound and no parse errors
    /// occurred, 1 otherwise.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.errors.is_empty() || self.lifecycles.iter().any(|w| !w.is_sound()))
    }
}

/// Run the three structural checks over one lifecycle, producing its report.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn analyze(wf: &LifecycleDef) -> LifecycleReport {
    let states: BTreeSet<&str> = wf.states.iter().map(String::as_str).collect();
    let mut violations = Vec::new();

    // (c) EXISTENCE — every referenced state must be declared.
    if !states.contains(wf.initial.as_str()) {
        violations.push(Violation {
            kind: ViolationKind::Existence,
            states: vec![wf.initial.clone()],
            message: format!(
                "unknown state '{}' referenced as initial state of lifecycle '{}'",
                wf.initial, wf.name
            ),
        });
    }
    for t in &wf.terminals {
        if !states.contains(t.as_str()) {
            violations.push(Violation {
                kind: ViolationKind::Existence,
                states: vec![t.clone()],
                message: format!(
                    "unknown state '{t}' referenced in terminals of lifecycle '{}'",
                    wf.name
                ),
            });
        }
    }
    let mut reported_unknown: BTreeSet<&str> = BTreeSet::new();
    for (from, to) in &wf.transitions {
        for endpoint in [from, to] {
            if !states.contains(endpoint.as_str()) && reported_unknown.insert(endpoint.as_str()) {
                violations.push(Violation {
                    kind: ViolationKind::Existence,
                    states: vec![endpoint.clone()],
                    message: format!(
                        "unknown state '{endpoint}' referenced in transitions of lifecycle '{}'",
                        wf.name
                    ),
                });
            }
        }
    }

    // (d) TERMINAL-SOURCE — a declared terminal state must have no outgoing
    // transitions, otherwise it is not actually terminal (an unsound "movable
    // terminal"). This mirrors the by-construction compile error the macro
    // emits, keeping the CLI gate honest even on source that would not compile.
    let terminal_set: BTreeSet<&str> = wf.terminals.iter().map(String::as_str).collect();
    for (from, to) in &wf.transitions {
        if terminal_set.contains(from.as_str()) {
            violations.push(Violation {
                kind: ViolationKind::TerminalSource,
                states: vec![from.clone()],
                message: format!("terminal state '{from}' has an outgoing transition to '{to}'"),
            });
        }
    }

    // Forward adjacency (from -> [to]) over declared edges.
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in &wf.transitions {
        forward.entry(from.as_str()).or_default().push(to.as_str());
    }

    // (a) REACHABILITY — BFS from `initial`. Only meaningful when `initial` is a
    // real state (otherwise existence already reported it and there is no anchor
    // to walk from).
    let reachable = if states.contains(wf.initial.as_str()) {
        let reachable = bfs(wf.initial.as_str(), &forward);
        for state in &wf.states {
            if !reachable.contains(state.as_str()) {
                violations.push(Violation {
                    kind: ViolationKind::Reachability,
                    states: vec![state.clone()],
                    message: format!(
                        "state '{state}' is unreachable from initial state '{}'",
                        wf.initial
                    ),
                });
            }
        }
        reachable
    } else {
        BTreeSet::new()
    };

    // (b) DEAD-END — reverse BFS from the terminals gives the set of states that
    // can reach *some* terminal. A reachable, non-terminal state outside that set
    // is a dead-end.
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in &wf.transitions {
        reverse.entry(to.as_str()).or_default().push(from.as_str());
    }
    let mut can_reach_terminal: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    for t in &terminal_set {
        if states.contains(t) && can_reach_terminal.insert(t) {
            queue.push_back(t);
        }
    }
    while let Some(node) = queue.pop_front() {
        if let Some(preds) = reverse.get(node) {
            for &p in preds {
                if can_reach_terminal.insert(p) {
                    queue.push_back(p);
                }
            }
        }
    }
    for state in &wf.states {
        let s = state.as_str();
        if reachable.contains(s) && !terminal_set.contains(s) && !can_reach_terminal.contains(s) {
            violations.push(Violation {
                kind: ViolationKind::DeadEnd,
                states: vec![state.clone()],
                message: format!(
                    "state '{state}' is a non-terminal dead-end (cannot reach any terminal state)"
                ),
            });
        }
    }

    LifecycleReport {
        name: wf.name.clone(),
        states: wf.states.clone(),
        initial: wf.initial.clone(),
        terminals: wf.terminals.clone(),
        transitions: wf.transitions.clone(),
        violations,
    }
}

/// Breadth-first set of states reachable from `start` over `adj`.
fn bfs<'a>(start: &'a str, adj: &HashMap<&'a str, Vec<&'a str>>) -> BTreeSet<&'a str> {
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        if let Some(nexts) = adj.get(node) {
            for &n in nexts {
                if visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }
    }
    visited
}

// ── Source scanning ──────────────────────────────────────────────────────────

/// Accumulator threaded through the source scan.
#[derive(Debug, Default)]
struct Scan {
    lifecycles: Vec<LifecycleDef>,
    errors: Vec<String>,
    files_scanned: usize,
}

/// Walk `root` recursively and collect every `#[lifecycle]` enum in its `.rs`
/// files. A missing/unreadable scan root is a hard error (mirrors `a11y`): a
/// zero-lifecycle run must never be a silent CI pass caused by a path typo.
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
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never follow symlinks — a symlinked directory cycle would hang CI.
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

/// Parse one source file and collect every `#[lifecycle]` enum it declares.
fn scan_source(src: &str, file: &str, scan: &mut Scan) {
    // A file that does not parse as Rust is skipped (best-effort, like `a11y`).
    let Ok(ast) = syn::parse_file(src) else {
        return;
    };
    // Aliases are per-file: collect every `use ...::lifecycle as <alias>;` in the
    // file (at any inline-module depth) up front, then recognize `#[<alias>(...)]`
    // anywhere in that same file.
    let mut aliases = BTreeSet::new();
    collect_lifecycle_aliases(&ast.items, &mut aliases);
    scan_items(&ast.items, file, scan, &aliases);
}

/// Recursively collect same-file aliases of the `#[lifecycle]` attribute macro:
/// for each `use ...::lifecycle as <alias>;` rename found anywhere in the file
/// (including inside inline modules), record `<alias>`. Only a rename whose
/// *original* terminal name is `lifecycle` counts; glob and plain-name imports
/// carry no rename and are ignored (see the module-level attribute-recognition
/// limits).
fn collect_lifecycle_aliases(items: &[syn::Item], out: &mut BTreeSet<String>) {
    for item in items {
        match item {
            syn::Item::Use(item_use) => collect_use_tree_aliases(&item_use.tree, out),
            syn::Item::Mod(item_mod) => {
                if let Some((_, inner)) = &item_mod.content {
                    collect_lifecycle_aliases(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Walk a `use` tree, recording the rename target of any `lifecycle as <alias>`.
fn collect_use_tree_aliases(tree: &syn::UseTree, out: &mut BTreeSet<String>) {
    match tree {
        syn::UseTree::Path(path) => collect_use_tree_aliases(&path.tree, out),
        syn::UseTree::Rename(rename) => {
            if rename.ident == "lifecycle" {
                out.insert(rename.rename.to_string());
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_tree_aliases(item, out);
            }
        }
        syn::UseTree::Name(_) | syn::UseTree::Glob(_) => {}
    }
}

/// Recursively walk a list of items, collecting every `#[lifecycle]` enum —
/// including those nested inside inline modules (`mod orders { ... }`, stored as
/// `Item::Mod` with `content`). Modules without inline content (`mod foo;`) have
/// no items to walk and are ignored. `aliases` are the same-file
/// `lifecycle`-attribute aliases discovered by [`collect_lifecycle_aliases`].
fn scan_items(items: &[syn::Item], file: &str, scan: &mut Scan, aliases: &BTreeSet<String>) {
    for item in items {
        match item {
            syn::Item::Enum(item_enum) => {
                collect_lifecycle_enum(item_enum, file, scan, aliases);
            }
            syn::Item::Mod(item_mod) => {
                if let Some((_, inner)) = &item_mod.content {
                    scan_items(inner, file, scan, aliases);
                }
            }
            _ => {}
        }
    }
}

/// If `item_enum` carries a `#[lifecycle(...)]` attribute, parse it into a
/// [`LifecycleDef`]; a malformed attribute is recorded as an error naming `file`.
///
/// `aliases` holds the same-file `use ...::lifecycle as <alias>;` renames, so an
/// aliased single-segment invocation `#[<alias>(...)]` is recognized too. See the
/// module-level attribute-recognition limits for what this cannot resolve.
fn collect_lifecycle_enum(
    item_enum: &syn::ItemEnum,
    file: &str,
    scan: &mut Scan,
    aliases: &BTreeSet<String>,
) {
    // Recognize the attribute as `#[lifecycle]` when it is:
    //  - a single-segment path `lifecycle`, or a same-file alias of it
    //    (`use ...::lifecycle as lc; #[lc(...)]`); or
    //  - a multi-segment path whose *last* segment is `lifecycle`, so qualified
    //    forms like `#[autumn_web::lifecycle(...)]` (valid Rust) are recognized.
    // Matching only `is_ident("lifecycle")` would miss both the qualified and the
    // aliased invocations and silently pass an unsound lifecycle.
    let Some(attr) = item_enum.attrs.iter().find(|a| {
        let path = a.path();
        path.get_ident().map_or_else(
            || path.segments.last().is_some_and(|s| s.ident == "lifecycle"),
            |ident| ident == "lifecycle" || aliases.contains(&ident.to_string()),
        )
    }) else {
        return;
    };
    let name = item_enum.ident.to_string();
    match attr.parse_args::<LifecycleArgs>() {
        Ok(args) => {
            let states = item_enum
                .variants
                .iter()
                .map(|v| v.ident.to_string())
                .collect();
            scan.lifecycles.push(LifecycleDef {
                name,
                states,
                initial: args.initial.to_string(),
                terminals: args.terminals.iter().map(ToString::to_string).collect(),
                transitions: args
                    .transitions
                    .iter()
                    .map(|t| (t.from.to_string(), t.to.to_string()))
                    .collect(),
            });
        }
        Err(err) => {
            scan.errors.push(format!(
                "{file}: malformed #[lifecycle] on enum '{name}': {err}"
            ));
        }
    }
}

/// Build a [`Report`] from a scan, analyzing each discovered lifecycle.
fn report_from_scan(scan: Scan) -> Report {
    let lifecycles = scan.lifecycles.iter().map(analyze).collect();
    Report {
        files_scanned: scan.files_scanned,
        lifecycles,
        errors: scan.errors,
    }
}

// ── `lifecycle check` entry point ─────────────────────────────────────────────

/// Options for `autumn lifecycle check`.
#[derive(Debug, Clone, Copy)]
pub struct CheckOptions {
    pub format: OutputFormat,
}

/// Scan `root`, analyze every lifecycle, print the report, and return the process
/// exit code (0 = all sound, 1 = any violation or parse error).
#[must_use]
pub fn run_check(root: &Path, opts: CheckOptions) -> i32 {
    let scan = match scan_project(root) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("error: cannot read scan path '{}': {err}", root.display());
            return 1;
        }
    };
    let report = report_from_scan(scan);
    match opts.format {
        OutputFormat::Json => print_check_json(&report),
        OutputFormat::Text => print_check_text(&report),
    }
    report.exit_code()
}

fn print_check_json(report: &Report) {
    match serde_json::to_string_pretty(report) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("autumn lifecycle check: failed to serialize report: {err}"),
    }
}

fn print_check_text(report: &Report) {
    println!("autumn lifecycle check");
    println!(
        "  scanned {} lifecycle(s) across {} .rs file(s)",
        report.lifecycles.len(),
        report.files_scanned
    );

    for err in &report.errors {
        println!("  error: {err}");
    }

    for wf in &report.lifecycles {
        println!("\n  lifecycle '{}' ({} state(s))", wf.name, wf.states.len());
        println!("    initial:   {}", wf.initial);
        println!("    terminals: {}", wf.terminals.join(", "));
        if wf.violations.is_empty() {
            println!("    sound: no violations");
        } else {
            println!("    violations:");
            for v in &wf.violations {
                println!("      [{}] {}", v.kind.tag(), v.message);
            }
        }
    }

    let violations = report.violation_count();
    println!(
        "\n  summary: {} lifecycle(s), {} violation(s), {} parse error(s)",
        report.lifecycles.len(),
        violations,
        report.errors.len()
    );
    if report.exit_code() == 0 {
        println!("Result: PASS — all lifecycles are sound.");
    } else {
        println!("Result: FAIL — fix the lifecycle violations above.");
    }
}

// ── `lifecycle diagram` entry point ───────────────────────────────────────────

/// Options for `autumn lifecycle diagram`.
#[derive(Debug, Clone)]
pub struct DiagramOptions {
    pub format: DiagramFormat,
    pub out: Option<PathBuf>,
}

/// Scan `root`, render a lifecycle diagram per discovered lifecycle, and write
/// the result to stdout or `opts.out`. Returns the process exit code.
#[must_use]
pub fn run_diagram(root: &Path, opts: &DiagramOptions) -> i32 {
    let scan = match scan_project(root) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("error: cannot read scan path '{}': {err}", root.display());
            return 1;
        }
    };
    if !scan.errors.is_empty() {
        for err in &scan.errors {
            eprintln!("error: {err}");
        }
        return 1;
    }
    if scan.lifecycles.is_empty() {
        eprintln!(
            "autumn lifecycle diagram: no #[lifecycle] enums found under '{}'",
            root.display()
        );
        return 1;
    }

    let mut output = String::new();
    for (i, wf) in scan.lifecycles.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        match opts.format {
            DiagramFormat::Dot => output.push_str(&render_dot(wf)),
            DiagramFormat::Mermaid => output.push_str(&render_mermaid(wf)),
        }
    }

    match &opts.out {
        Some(path) => {
            if let Err(err) = std::fs::write(path, &output) {
                eprintln!(
                    "autumn lifecycle diagram: failed to write '{}': {err}",
                    path.display()
                );
                return 1;
            }
            println!(
                "wrote {} diagram(s) to {}",
                scan.lifecycles.len(),
                path.display()
            );
        }
        None => print!("{output}"),
    }
    0
}

/// Render a lifecycle as a Mermaid `stateDiagram-v2` block. The initial state is
/// entered from `[*]`; each terminal state exits to `[*]`, which is how Mermaid
/// renders start/end markers.
#[must_use]
pub fn render_mermaid(wf: &LifecycleDef) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "%% lifecycle: {}", wf.name);
    out.push_str("stateDiagram-v2\n");
    let _ = writeln!(out, "    [*] --> {}", wf.initial);
    for (from, to) in &wf.transitions {
        let _ = writeln!(out, "    {from} --> {to}");
    }
    for t in &wf.terminals {
        let _ = writeln!(out, "    {t} --> [*]");
    }
    out
}

/// Render a lifecycle as a Graphviz DOT `digraph`. The initial state is filled
/// and reached from a start point; terminal states are drawn as double circles.
#[must_use]
pub fn render_dot(wf: &LifecycleDef) -> String {
    let mut out = String::new();
    // The graph name is the enum ident — already a valid bare DOT id.
    let _ = writeln!(out, "digraph {} {{", wf.name);
    out.push_str("    rankdir=LR;\n");
    out.push_str("    node [shape=box, style=rounded];\n");
    out.push_str("    __start [shape=point, width=0.15];\n");
    // Highlight the initial state (filled) and terminal states (double circle).
    let _ = writeln!(
        out,
        "    {} [style=\"rounded,filled\", fillcolor=lightblue];",
        dot_id(&wf.initial)
    );
    for t in &wf.terminals {
        let _ = writeln!(out, "    {} [shape=doublecircle];", dot_id(t));
    }
    let _ = writeln!(out, "    __start -> {};", dot_id(&wf.initial));
    for (from, to) in &wf.transitions {
        let _ = writeln!(out, "    {} -> {};", dot_id(from), dot_id(to));
    }
    out.push_str("}\n");
    out
}

/// Quote a state/enum name as a DOT node id. Rust identifiers are already valid
/// bare DOT ids, but quoting is unconditional so the output stays well-formed
/// regardless of the ident.
fn dot_id(name: &str) -> String {
    format!("\"{name}\"")
}

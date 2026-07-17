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
//! segment is `lifecycle` (e.g. `#[autumn_web::lifecycle(...)]`), and an
//! in-scope alias introduced by `use ...::lifecycle as <alias>;` (matched by its
//! terminal `lifecycle` rename). Such an alias is honored only within the module
//! subtree where the `use` appears — it does not leak to a parent or sibling
//! module — so a sibling's unrelated `#[<alias>(...)]` macro is never misread as
//! a lifecycle. Like the `autumn a11y verify`
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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
    /// Module-qualified identity (e.g. `orders::State`), unique across the scan.
    /// Used to namespace diagram cluster/node ids so two same-named enums in
    /// separate modules never collide. Equals `name` for a top-level enum.
    pub id: String,
    /// Enum identifier (e.g. `OrderState`) — the human-readable label.
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

/// An effect declared on a `#[state_machine(lifecycle = Enum, effects(...))]`
/// binding (issue #1973), correlated to a `#[lifecycle]` enum's diagram by that
/// enum's module-qualified [`LifecycleDef::id`] in a post-scan pass (see
/// [`resolve_effect_bindings`]): the binding's `lifecycle = <Path>` is first
/// resolved relative to the binding's module ([`resolve_lifecycle_id`]) and, if
/// that qualified id is a scanned lifecycle, keyed by it; otherwise it falls back
/// to a scanned lifecycle whose bare enum name is *globally unique*. Qualified
/// matching keeps two same-named lifecycle enums in different modules from
/// cross-contaminating each other's effect labels (their bare name is ambiguous,
/// so the fallback never fires), while the unique-bare-name fallback rescues a
/// model that imports its lifecycle from another module. This rides in a separate
/// structure consulted only by the diagram renderers, so the `check` command, the
/// JSON artifact, and [`LifecycleDef::transitions`] stay byte-identical.
#[derive(Debug)]
struct EffectEdge {
    from: String,
    to: String,
    /// The `on = "<handler>"` sync in-transaction effect, if declared.
    on: Option<String>,
    /// The `on_commit = <Job>` after-commit effect (its `syn::Path`'s last
    /// segment rendered as the job name), if declared.
    on_commit: Option<String>,
}

/// A raw `#[state_machine(lifecycle = <Path>, effects(...))]` binding collected
/// during the scan. Its `lifecycle = <Path>` is resolved to a scanned lifecycle
/// id only in a post-pass ([`resolve_effect_bindings`]), once the *complete* set
/// of scanned lifecycle ids is known — a single-pass resolution could not see a
/// lifecycle declared later in the scan (nor decide whether a bare enum name is
/// globally unique), so the correlation is deferred.
struct RawBinding {
    /// The inline-module context the binding is declared in (same context
    /// [`collect_lifecycle_enum`] uses to build a [`LifecycleDef::id`]).
    module_path: Vec<String>,
    /// The binding's `lifecycle = <Path>`, resolved in the post-pass.
    lifecycle_path: syn::Path,
    /// The binding's declared `effects(...)` edges.
    edges: Vec<EffectEdge>,
}

// `syn::Path` does not implement `Debug` without the `extra-traits` feature, so
// `Debug` is hand-written (rendering the path as its `::`-joined segment idents)
// to keep the `Debug`-derived [`Scan`] compiling without pulling in that feature.
impl std::fmt::Debug for RawBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self
            .lifecycle_path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        f.debug_struct("RawBinding")
            .field("module_path", &self.module_path)
            .field("lifecycle_path", &path)
            .field("edges", &self.edges)
            .finish()
    }
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
    /// Raw `#[state_machine(lifecycle = Enum, effects(...))]` bindings collected
    /// during the scan (issue #1973), resolved into [`Self::effect_edges`] only
    /// in a post-pass once every lifecycle id is known (see
    /// [`resolve_effect_bindings`]).
    raw_bindings: Vec<RawBinding>,
    /// Transition effects keyed by the bound lifecycle enum's module-qualified
    /// [`LifecycleDef::id`], produced by the post-scan [`resolve_effect_bindings`]
    /// pass. A binding is keyed by its qualified id when that id is a scanned
    /// lifecycle, else by a scanned lifecycle whose bare enum name is globally
    /// unique, else dropped (a safe failure — missing labels, never a mismatch).
    /// Consulted only by the diagram renderers.
    effect_edges: HashMap<String, Vec<EffectEdge>>,
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
    // Post-pass: now that the complete set of scanned lifecycle ids is known,
    // correlate every collected effect binding (qualified-match first, then a
    // unique-bare-name fallback for imported lifecycles). Deferring this until
    // the whole scan finishes lets a binding correlate to a lifecycle declared
    // later in scan order, and lets the bare-name fallback see whether a name is
    // globally unique.
    scan.effect_edges =
        resolve_effect_bindings(std::mem::take(&mut scan.raw_bindings), &scan.lifecycles);
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
    // Aliases are scoped lexically to the module subtree in which the
    // `use ...::lifecycle as <alias>;` appears. The scan starts at the file root
    // with an empty inherited set; each module extends it with its own renames
    // (see `scan_items`) and passes that extended set only to its descendants, so
    // a `use` inside a child module never leaks to a parent or sibling module.
    let mut module_path = Vec::new();
    scan_items(&ast.items, file, scan, &BTreeSet::new(), &mut module_path);
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
/// no items to walk and are ignored.
///
/// `inherited_aliases` are the `use ...::lifecycle as <alias>;` renames in scope
/// from enclosing modules. Aliases are scoped lexically: at this module level we
/// compute `local_aliases = inherited_aliases ∪ {this module's own lifecycle-use
/// renames}`, apply it to enums declared directly here, and pass it as the
/// inherited set when descending into child modules. A `use` in a child module is
/// therefore invisible to its parent and siblings — so an alias declared in one
/// module cannot make a sibling module's unrelated `#[alias(...)]` (a different
/// macro that happens to share the name) be misread as a lifecycle.
fn scan_items(
    items: &[syn::Item],
    file: &str,
    scan: &mut Scan,
    inherited_aliases: &BTreeSet<String>,
    module_path: &mut Vec<String>,
) {
    // A `use` is visible throughout its module regardless of textual position, so
    // gather this module's own lifecycle renames before scanning any enum here.
    let mut local_aliases = inherited_aliases.clone();
    for item in items {
        if let syn::Item::Use(item_use) = item {
            collect_use_tree_aliases(&item_use.tree, &mut local_aliases);
        }
    }
    for item in items {
        match item {
            syn::Item::Enum(item_enum) => {
                collect_lifecycle_enum(
                    item_enum,
                    file,
                    scan,
                    &local_aliases,
                    module_path.as_slice(),
                );
            }
            syn::Item::Struct(item_struct) => {
                collect_state_machine_effects(item_struct, scan, module_path.as_slice());
            }
            syn::Item::Mod(item_mod) => {
                if let Some((_, inner)) = &item_mod.content {
                    module_path.push(item_mod.ident.to_string());
                    scan_items(inner, file, scan, &local_aliases, module_path);
                    module_path.pop();
                }
            }
            _ => {}
        }
    }
}

/// If `item_enum` carries a `#[lifecycle(...)]` attribute, parse it into a
/// [`LifecycleDef`]; a malformed attribute is recorded as an error naming `file`.
///
/// `aliases` holds the in-scope `use ...::lifecycle as <alias>;` renames (this
/// module's own plus those inherited from enclosing modules), so an aliased
/// single-segment invocation `#[<alias>(...)]` is recognized too. See the
/// module-level attribute-recognition limits for what this cannot resolve.
fn collect_lifecycle_enum(
    item_enum: &syn::ItemEnum,
    file: &str,
    scan: &mut Scan,
    aliases: &BTreeSet<String>,
    module_path: &[String],
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
            let id = if module_path.is_empty() {
                name.clone()
            } else {
                format!("{}::{}", module_path.join("::"), name)
            };
            let states = item_enum
                .variants
                .iter()
                .map(|v| v.ident.to_string())
                .collect();
            scan.lifecycles.push(LifecycleDef {
                id,
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

/// Collect per-edge transition effects from any `#[state_machine(...)]` field
/// attribute on `item_struct` (issue #1973). Only the
/// `#[state_machine(lifecycle = <Enum>, effects(...))]` form — the one whose
/// edges correlate to an existing `#[lifecycle]` enum diagram — is collected;
/// an inline `#[state_machine(transitions(...))]` machine has no `#[lifecycle]`
/// enum (and thus no diagram surface today) and is skipped. Parsing is tolerant:
/// a miss on one attribute is skipped and never aborts the scan.
///
/// `module_path` is the inline-module context the binding is declared in (the
/// same context [`collect_lifecycle_enum`] uses to build a [`LifecycleDef::id`]).
/// The binding is stashed *raw* (path + module + edges) and correlated to a
/// scanned lifecycle only in the post-scan [`resolve_effect_bindings`] pass, when
/// the complete set of scanned lifecycle ids is known.
fn collect_state_machine_effects(
    item_struct: &syn::ItemStruct,
    scan: &mut Scan,
    module_path: &[String],
) {
    for field in &item_struct.fields {
        for attr in &field.attrs {
            if !is_state_machine_attr(attr) {
                continue;
            }
            // Tolerant: an unrelated `transitions(...)` form → `Ok(None)`, and a
            // parse miss → `Err`; both simply skip this attribute.
            if let Ok(Some((lifecycle_path, edges))) =
                attr.parse_args_with(parse_state_machine_effects)
                && !edges.is_empty()
            {
                scan.raw_bindings.push(RawBinding {
                    module_path: module_path.to_vec(),
                    lifecycle_path,
                    edges,
                });
            }
        }
    }
}

/// Correlate the raw effect bindings collected during the scan to scanned
/// lifecycle ids, producing the `id -> effects` map the diagram renderers consult
/// (issue #1973; Codex P2 on #2029). Runs as a post-pass so it can see the
/// *complete* set of scanned lifecycles. Each binding is resolved by, in order:
///
/// 1. **Qualified match**: resolve the binding's `lifecycle = <Path>` relative to
///    its own module ([`resolve_lifecycle_id`]); if that id names a scanned
///    lifecycle, key the effects by it. This handles co-located and
///    explicitly-pathed lifecycles and disambiguates two same-named enums in
///    different modules (each binding pins to its co-located enum).
/// 2. **Unique-bare-name fallback**: otherwise take the path's bare last segment;
///    if *exactly one* scanned lifecycle has that bare enum name, key the effects
///    by that lifecycle's id. This rescues a model that imports its lifecycle from
///    another module (`use crate::states::OrderState; lifecycle = OrderState`),
///    where Rust resolves the bare name to another module but the binding's own
///    module prefix would mis-qualify it.
/// 3. **Drop**: otherwise (qualified miss *and* the bare name is absent or
///    ambiguous among scanned lifecycles) the binding is left uncorrelated — a
///    safe failure (missing labels), never a guess. Crucially, two same-named
///    enums make the bare name ambiguous, so step 2 never fires for them and the
///    same-module isolation from step 1 is preserved.
fn resolve_effect_bindings(
    bindings: Vec<RawBinding>,
    lifecycles: &[LifecycleDef],
) -> HashMap<String, Vec<EffectEdge>> {
    let id_set: BTreeSet<&str> = lifecycles.iter().map(|l| l.id.as_str()).collect();
    // Bare enum name -> the ids of every scanned lifecycle with that name. A name
    // mapping to a single id is globally unique (the fallback's precondition).
    let mut by_bare_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for l in lifecycles {
        by_bare_name
            .entry(l.name.as_str())
            .or_default()
            .push(l.id.as_str());
    }
    let mut effect_edges: HashMap<String, Vec<EffectEdge>> = HashMap::new();
    for binding in bindings {
        // Step 1: qualified match against the complete scanned-id set.
        let qualified = resolve_lifecycle_id(&binding.module_path, &binding.lifecycle_path)
            .filter(|id| id_set.contains(id.as_str()));
        let key = qualified.or_else(|| {
            // Step 2: unique-bare-name fallback (last path segment).
            let bare = binding.lifecycle_path.segments.last()?.ident.to_string();
            match by_bare_name.get(bare.as_str()) {
                Some(ids) if ids.len() == 1 => Some(ids[0].to_string()),
                _ => None,
            }
        });
        // Step 3: an unresolved binding is dropped (no labels).
        if let Some(key) = key {
            effect_edges.entry(key).or_default().extend(binding.edges);
        }
    }
    effect_edges
}

/// Resolve a `#[state_machine(lifecycle = <Path>)]` binding's enum path to the
/// module-qualified [`LifecycleDef::id`] scheme (`a::b::State`), relative to the
/// binding's inline-module `module_path`, mirroring how [`collect_lifecycle_enum`]
/// roots enum ids (at the crate root, without a crate-name prefix). Returns
/// `None` when the path cannot be confidently resolved (empty, or more `super::`
/// hops than there are enclosing modules), so the binding stays uncorrelated
/// rather than being force-matched.
///
/// - Bare single-segment (`State`) → `<module_path>::State` (same module — the
///   common case).
/// - Leading `crate::` → rooted at the crate root (empty prefix), matching a
///   top-level enum's bare id.
/// - Leading `self::` → the binding's own module.
/// - Leading `super::` (one per hop) → pops one enclosing module per `super`.
/// - Otherwise relative (`a::State`) → nested under the binding's module.
fn resolve_lifecycle_id(module_path: &[String], path: &syn::Path) -> Option<String> {
    let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    if segs.is_empty() {
        return None;
    }
    let (mut prefix, rest): (Vec<String>, &[String]) = match segs[0].as_str() {
        // `crate::…` is rooted at the crate root; the scanner ids are also rooted
        // there with no crate-name segment, so the prefix is empty.
        "crate" => (Vec::new(), &segs[1..]),
        "self" => (module_path.to_vec(), &segs[1..]),
        "super" => {
            let mut prefix = module_path.to_vec();
            let mut i = 0;
            while i < segs.len() && segs[i] == "super" {
                // More `super` hops than enclosing modules → cannot resolve.
                prefix.pop()?;
                i += 1;
            }
            (prefix, &segs[i..])
        }
        // A plain relative path resolves under the binding's own module.
        _ => (module_path.to_vec(), &segs[..]),
    };
    if rest.is_empty() {
        return None;
    }
    prefix.extend(rest.iter().cloned());
    Some(prefix.join("::"))
}

/// Recognize a `#[state_machine(...)]` attribute under a bare `state_machine`
/// path or any qualified path whose last segment is `state_machine`.
fn is_state_machine_attr(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "state_machine")
}

/// Parse the inside of a `#[state_machine(...)]` attribute, returning the bound
/// enum's full `lifecycle = <Path>` (resolved to a module-qualified id by the
/// caller via [`resolve_lifecycle_id`]) and its declared `effects(...)` edges.
/// Returns `Ok(None)` for the inline `transitions(...)` form (out of scope — no
/// `#[lifecycle]` enum, no diagram surface). Mirrors the macro grammar in
/// `autumn-macros/src/model.rs` (`parse_state_machine_source`).
fn parse_state_machine_effects(
    input: ParseStream,
) -> syn::Result<Option<(syn::Path, Vec<EffectEdge>)>> {
    let key: Ident = input.parse()?;
    if key == "transitions" {
        return Ok(None);
    }
    if key != "lifecycle" {
        return Err(syn::Error::new_spanned(
            &key,
            "expected `transitions(...)` or `lifecycle = <Enum>`",
        ));
    }
    input.parse::<Token![=]>()?;
    let path: syn::Path = input.parse()?;
    if path.segments.is_empty() {
        return Err(syn::Error::new_spanned(
            &path,
            "`lifecycle = <Enum>` requires a non-empty path",
        ));
    }
    let edges = if input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        let effects_kw: Ident = input.parse()?;
        if effects_kw != "effects" {
            return Err(syn::Error::new_spanned(
                &effects_kw,
                "expected `effects(...)` after `lifecycle = <Enum>`",
            ));
        }
        let content;
        syn::parenthesized!(content in input);
        parse_effect_edges(&content)?
    } else {
        Vec::new()
    };
    Ok(Some((path, edges)))
}

/// Parse the inner edge list of an `effects(...)` group (surrounding parens
/// already consumed), extracting each edge's `on = "..."` and/or
/// `on_commit = <Job>` effect. Mirrors `parse_transition_list` in the macro
/// (including the `, <ident> =` vs `, <State> ->` comma disambiguation); any
/// `guard = "..."` present is parsed and ignored (effects-only).
fn parse_effect_edges(content: ParseStream) -> syn::Result<Vec<EffectEdge>> {
    let mut edges = Vec::new();
    while !content.is_empty() {
        let from: Ident = content.parse()?;
        content.parse::<Token![->]>()?;
        let to: Ident = content.parse()?;
        let mut on: Option<String> = None;
        let mut on_commit: Option<String> = None;
        if content.peek(Token![:]) {
            content.parse::<Token![:]>()?;
            if content.peek(syn::LitStr) {
                // Legacy bare-string guard shorthand `: "guard"` — ignored.
                let _: syn::LitStr = content.parse()?;
            } else {
                loop {
                    let mkey: Ident = content.parse()?;
                    content.parse::<Token![=]>()?;
                    if mkey == "guard" {
                        let _: syn::LitStr = content.parse()?;
                    } else if mkey == "on_commit" {
                        let job: syn::Path = content.parse()?;
                        on_commit = job.segments.last().map(|seg| seg.ident.to_string());
                    } else if mkey == "on" {
                        let lit: syn::LitStr = content.parse()?;
                        on = Some(lit.value());
                    } else {
                        return Err(syn::Error::new_spanned(
                            &mkey,
                            "expected `guard = \"...\"`, `on = \"...\"`, or `on_commit = <Job>`",
                        ));
                    }
                    // Continue the meta list only on a following `, <ident> =`
                    // (another key for this edge); a `, <State> ->` (or the end)
                    // begins the next edge and its comma is left for the
                    // separator below.
                    if !content.peek(Token![,]) {
                        break;
                    }
                    let ahead = content.fork();
                    ahead.parse::<Token![,]>()?;
                    if ahead.peek(Ident) && ahead.peek2(Token![=]) {
                        content.parse::<Token![,]>()?;
                    } else {
                        break;
                    }
                }
            }
        }
        edges.push(EffectEdge {
            from: from.to_string(),
            to: to.to_string(),
            on,
            on_commit,
        });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(edges)
}

/// Build the diagram edge-label lookup for one lifecycle from its collected
/// effect edges: `(from, to) -> "on: <h>[, on_commit: <J>]"`. Multiple bindings
/// touching the same edge are merged — the `on` / `on_commit` values are unioned
/// and rendered in a stable (sorted) order so the output is deterministic. Edges
/// with no effect are absent from the map (and so render without a label).
fn build_edge_labels(effects: Option<&Vec<EffectEdge>>) -> BTreeMap<(String, String), String> {
    let mut acc: BTreeMap<(String, String), (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    if let Some(effects) = effects {
        for edge in effects {
            let entry = acc.entry((edge.from.clone(), edge.to.clone())).or_default();
            if let Some(on) = &edge.on {
                entry.0.insert(on.clone());
            }
            if let Some(on_commit) = &edge.on_commit {
                entry.1.insert(on_commit.clone());
            }
        }
    }
    acc.into_iter()
        .filter_map(|(edge, (ons, commits))| {
            let mut parts = Vec::new();
            parts.extend(ons.iter().map(|on| format!("on: {on}")));
            parts.extend(
                commits
                    .iter()
                    .map(|on_commit| format!("on_commit: {on_commit}")),
            );
            if parts.is_empty() {
                None
            } else {
                Some((edge, parts.join(", ")))
            }
        })
        .collect()
}

/// Escape a DOT label's `"`/`\` so an effect handler/job name can never break
/// out of the quoted `label="..."` attribute (defensive — names are idents).
fn dot_label_escape(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"")
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

    let output = match opts.format {
        DiagramFormat::Dot => render_dot_document(&scan.lifecycles, &scan.effect_edges),
        DiagramFormat::Mermaid => render_mermaid_document(&scan.lifecycles, &scan.effect_edges),
    };

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

/// Render every discovered lifecycle as a **single** Mermaid `stateDiagram-v2`
/// document. A lone lifecycle renders as a plain top-level diagram (unchanged);
/// multiple lifecycles each become a composite `state "<Name>" as <id> { ... }`
/// inside one diagram, because concatenating several `stateDiagram-v2` headers is
/// not a single valid Mermaid document. State ids are namespaced by lifecycle so
/// a state name shared across lifecycles stays a distinct node, each carrying a
/// label of its bare state name.
#[must_use]
fn render_mermaid_document(
    lifecycles: &[LifecycleDef],
    effects: &HashMap<String, Vec<EffectEdge>>,
) -> String {
    if let [single] = lifecycles {
        return render_mermaid(single, &build_edge_labels(effects.get(&single.id)));
    }
    let mut out = String::new();
    out.push_str("stateDiagram-v2\n");
    for wf in lifecycles {
        let labels = build_edge_labels(effects.get(&wf.id));
        let ns = sanitize_ident(&wf.id);
        let sid = |state: &str| format!("{ns}_{}", sanitize_ident(state));
        let _ = writeln!(out, "    state \"{}\" as {ns} {{", wf.name);
        for state in &wf.states {
            let _ = writeln!(out, "        state \"{state}\" as {}", sid(state));
        }
        let _ = writeln!(out, "        [*] --> {}", sid(&wf.initial));
        for (from, to) in &wf.transitions {
            if let Some(label) = labels.get(&(from.clone(), to.clone())) {
                let _ = writeln!(out, "        {} --> {} : {label}", sid(from), sid(to));
            } else {
                let _ = writeln!(out, "        {} --> {}", sid(from), sid(to));
            }
        }
        for t in &wf.terminals {
            let _ = writeln!(out, "        {} --> [*]", sid(t));
        }
        out.push_str("    }\n");
    }
    out
}

/// Render a lifecycle as a Mermaid `stateDiagram-v2` block. The initial state is
/// entered from `[*]`; each terminal state exits to `[*]`, which is how Mermaid
/// renders start/end markers.
#[must_use]
pub fn render_mermaid(wf: &LifecycleDef, labels: &BTreeMap<(String, String), String>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "%% lifecycle: {}", wf.name);
    out.push_str("stateDiagram-v2\n");
    let _ = writeln!(out, "    [*] --> {}", wf.initial);
    for (from, to) in &wf.transitions {
        if let Some(label) = labels.get(&(from.clone(), to.clone())) {
            let _ = writeln!(out, "    {from} --> {to} : {label}");
        } else {
            let _ = writeln!(out, "    {from} --> {to}");
        }
    }
    for t in &wf.terminals {
        let _ = writeln!(out, "    {t} --> [*]");
    }
    out
}

/// Render every discovered lifecycle as a **single** Graphviz DOT document. A
/// lone lifecycle renders as a plain `digraph <Name>` (unchanged); multiple
/// lifecycles render as one `digraph` containing a `subgraph cluster_<name>` per
/// lifecycle. Emitting several complete `digraph {}` blocks back-to-back is not a
/// single valid DOT document (consumers expect one graph per document), so the
/// multi-lifecycle case must be one graph with clusters.
#[must_use]
fn render_dot_document(
    lifecycles: &[LifecycleDef],
    effects: &HashMap<String, Vec<EffectEdge>>,
) -> String {
    if let [single] = lifecycles {
        return render_dot(single, &build_edge_labels(effects.get(&single.id)));
    }
    let mut out = String::new();
    out.push_str("digraph {\n");
    out.push_str("    rankdir=LR;\n");
    out.push_str("    node [shape=box, style=rounded];\n");
    for wf in lifecycles {
        write_dot_cluster(&mut out, wf, &build_edge_labels(effects.get(&wf.id)));
    }
    out.push_str("}\n");
    out
}

/// Write one lifecycle as a `subgraph cluster_<name> { ... }` block. Node ids are
/// namespaced by the lifecycle name (DOT node ids live in the digraph's single
/// global namespace, so an un-namespaced state shared across lifecycles would
/// collapse into one node), and each carries a `label` of its bare state name.
fn write_dot_cluster(
    out: &mut String,
    wf: &LifecycleDef,
    labels: &BTreeMap<(String, String), String>,
) {
    let ns = sanitize_ident(&wf.id);
    let nid = |state: &str| dot_id(&format!("{ns}.{state}"));
    let start = dot_id(&format!("{ns}.__start"));
    let _ = writeln!(out, "    subgraph cluster_{ns} {{");
    let _ = writeln!(out, "        label={};", dot_id(&wf.name));
    let _ = writeln!(out, "        {start} [shape=point, width=0.15];");
    for state in &wf.states {
        let mut attrs = format!("label={}", dot_id(state));
        if state == &wf.initial {
            attrs.push_str(", style=\"rounded,filled\", fillcolor=lightblue");
        }
        if wf.terminals.contains(state) {
            attrs.push_str(", shape=doublecircle");
        }
        let _ = writeln!(out, "        {} [{attrs}];", nid(state));
    }
    let _ = writeln!(out, "        {start} -> {};", nid(&wf.initial));
    for (from, to) in &wf.transitions {
        if let Some(label) = labels.get(&(from.clone(), to.clone())) {
            let _ = writeln!(
                out,
                "        {} -> {} [label=\"{}\"];",
                nid(from),
                nid(to),
                dot_label_escape(label)
            );
        } else {
            let _ = writeln!(out, "        {} -> {};", nid(from), nid(to));
        }
    }
    out.push_str("    }\n");
}

/// Sanitize an enum/lifecycle name into a bare DOT/Mermaid identifier by
/// replacing every character that is not ASCII-alphanumeric or `_` with `_`. Rust
/// idents already satisfy this, but the pass keeps cluster/composite ids valid
/// regardless of the input.
fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render a lifecycle as a Graphviz DOT `digraph`. The initial state is filled
/// and reached from a start point; terminal states are drawn as double circles.
#[must_use]
pub fn render_dot(wf: &LifecycleDef, labels: &BTreeMap<(String, String), String>) -> String {
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
        if let Some(label) = labels.get(&(from.clone(), to.clone())) {
            let _ = writeln!(
                out,
                "    {} -> {} [label=\"{}\"];",
                dot_id(from),
                dot_id(to),
                dot_label_escape(label)
            );
        } else {
            let _ = writeln!(out, "    {} -> {};", dot_id(from), dot_id(to));
        }
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

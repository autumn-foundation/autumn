//! `#[query_budget(N)]` — a compile-time, per-route database query budget
//! (issue #1667).
//!
//! Autumn owns 100% of the query-issuing surface: every statement reaches the
//! database through a `#[repository]` method, a [`preload`] batch, or a
//! diesel-async executor call handed the request's `Db` handle. That ownership
//! is what makes a *static* per-route query count tractable at all — the
//! handle is always named in the handler's signature, so any construct that
//! can issue a query either names it or is reachable from something that does.
//!
//! This module turns that ownership into a build-time gate. It walks the
//! annotated function's AST and computes a conservative upper bound on the
//! number of queries any statically reachable path can issue:
//!
//! * straight-line statements **sum**,
//! * `if` / `match` arms take the **maximum** (the worst reachable path),
//! * a loop whose body issues a query is **unbounded** unless the iterable has
//!   a literal, compile-time bound — this is the classic N+1,
//! * anything the analysis cannot read (a helper function handed the handle, a
//!   macro body mentioning it, a closure that may run per element) is
//!   **reported**, never silently skipped.
//!
//! Three escape hatches keep legitimately dynamic code compiling:
//! `#[query_budget(unbounded, reason = ...)]` on the handler, and
//! `#[query_cost(N)]` / `#[query_exempt(reason = ...)]` on a statement.
//!
//! See `docs/guide/query-budgets.md` for the user-facing guide.

use std::collections::HashSet;
use std::fmt::Write as _;

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{ToTokens as _, format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{Attribute, Block, Expr, ExprCall, ExprMethodCall, ItemFn, Local, Pat, Stmt, Type};

// ── Recognized framework surface ─────────────────────────────────────

/// diesel / diesel-async executor methods. Calling one *is* the round trip.
const EXECUTORS: &[&str] = &[
    "load",
    "load_stream",
    "first",
    "get_result",
    "get_results",
    "execute",
];

/// Repository/`Db` chain methods that refine *how* a later query runs without
/// issuing one themselves. The value they return is still a handle.
const HANDLE_BUILDERS: &[&str] = &[
    "on_primary",
    "on_replica",
    "primary",
    "replica",
    "from_shard",
    "for_shard",
    "with_shard",
    "shard",
    "scoped",
    "scope",
    "unscoped",
    "across_tenants",
    "for_tenant",
    "with_actor",
    "acting_as",
    "read_only",
    "as_mut",
    "as_ref",
    "reborrow",
    "clone",
    // Query-DSL refinements on a repository's aggregate/finder builders. They
    // are pure builder calls, so splitting a chain across `let` bindings must
    // not change the count.
    "filter",
    "order",
    "order_by",
    "order_by_aggregate_asc",
    "order_by_aggregate_desc",
    "group_by",
    "having",
    "limit",
    "offset",
    "select",
    "page",
    "per_page",
];

/// Closure-taking methods that invoke their closure **at most once** —
/// `Option`/`Result` combinators, not iterator adapters. A query inside one is
/// a fixed cost, not a per-element one.
const AT_MOST_ONCE_CLOSURE_METHODS: &[&str] = &[
    "unwrap_or_else",
    "ok_or_else",
    "get_or_insert_with",
    "unwrap_or_default",
];

/// Macros that are structurally incapable of issuing a query, however they
/// mention the handle (`tracing::debug!(db = ?db, …)`, `format!("{db:?}")`).
const INERT_MACROS: &[&str] = &[
    "format",
    "write",
    "writeln",
    "print",
    "println",
    "eprint",
    "eprintln",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "dbg",
    "vec",
    "matches",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "trace",
    "debug",
    "info",
    "warn",
    "error",
    "event",
    "span",
    "log",
    // Template macros. Sync by construction, so they cannot drive a future;
    // one that *does* contain an `await` is caught by the check above before
    // this list is consulted.
    "html",
    "maud",
];

/// Methods that run their closure **exactly once**, so a query inside is a
/// fixed cost rather than a per-element one. `Db::tx` / `Db::tx_with` are
/// autumn's transaction API (`autumn/src/db.rs`); `transaction` is
/// diesel-async's own.
const TRANSACTION_METHODS: &[&str] = &["tx", "tx_with", "transaction"];

/// Free functions whose closure likewise runs exactly once, and which take the
/// connection as their first argument (`autumn/src/db.rs`).
const TRANSACTION_FREE_FNS: &[&str] = &["scoped_transaction", "savepoint"];

/// Repository methods that walk a whole table through a keyset cursor. Their
/// query count is the table's size divided by the batch size — unbounded at
/// compile time, however small the budget looks.
const UNBOUNDED_METHODS: &[&str] = &["find_in_batches", "find_each"];

/// Free functions that may receive the handle without querying through it.
const SAFE_FREE_FNS: &[&str] = &["drop"];

/// Field and accessor names that conventionally *hold* a database handle
/// rather than query through one: `self.repo`, `state.db`, `app.pool()`. A
/// handle reached this way is tracked like one named in the signature, so a
/// query issued through it is still counted.
const HANDLE_ACCESSORS: &[&str] = &["db", "repo", "repository", "pool", "conn", "connection"];

/// `Result`/`Option`-unwrapping methods that stand in for the `?` operator
/// (`ctx.conn().await.expect("connection")`, the documented shape in
/// `autumn/src/seed.rs`) without themselves issuing a query. Deliberately
/// narrow: only the two spellings actually used for this in the codebase —
/// `.ok()`, `.unwrap_or_else(...)`, and friends are a known, unaddressed gap
/// (see `docs/guide/query-budgets.md` update tracking #2546).
const RESULT_UNWRAP_METHODS: &[&str] = &["expect", "unwrap"];

/// Exact type names that name a database handle.
const HANDLE_TYPES: &[&str] = &[
    "Db",
    "DeferredDb",
    "ShardedDb",
    "ShardedReadDb",
    "TestDb",
    "AsyncPgConnection",
    "AsyncConnection",
    "PgConnection",
    "SqliteConnection",
    "PooledConnection",
];

/// Offered when the fix is to stop issuing a query per row.
const BATCH_HINT: &str = "Batch the per-row lookup into one query with `preload(...)`, or opt the \
                          handler out with `#[query_budget(unbounded, reason = ...)]`. See \
                          docs/guide/query-budgets.md.";

/// Offered for a loop, where the working annotation goes on the loop statement
/// itself rather than on the call inside it.
const LOOP_HINT: &str = "Batch the per-row lookup into one query with `preload(...)`, put \
                         `#[query_cost(N)]` on the loop statement when the iteration count is \
                         bounded by something the analysis cannot see, or opt the handler out \
                         with `#[query_budget(unbounded, reason = ...)]`. See \
                         docs/guide/query-budgets.md.";

/// Offered when the fix is to state a cost the analysis cannot see.
const DECLARE_HINT: &str = "Declare the statement's cost with `#[query_cost(N)]`, or exempt it \
                            with `#[query_exempt(reason = ...)]` once you have checked it issues \
                            nothing. See docs/guide/query-budgets.md.";

/// Statement annotation declaring a call site's query cost.
const ATTR_QUERY_COST: &str = "query_cost";
/// Statement annotation excluding a call site from the ledger.
const ATTR_QUERY_EXEMPT: &str = "query_exempt";

// ── Attribute parsing ────────────────────────────────────────────────

/// The declared budget: a finite ceiling, or an explicit opt-out.
enum Budget {
    /// `#[query_budget(3)]`
    Bounded(u32),
    /// `#[query_budget(unbounded, reason = ...)]`
    Unbounded,
}

struct BudgetAttr {
    budget: Budget,
}

impl Parse for BudgetAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                "`#[query_budget(...)]` needs a query count, e.g. `#[query_budget(3)]`, \
                 or the explicit opt-out `#[query_budget(unbounded, reason = ...)]`",
            ));
        }

        let budget = if input.peek(syn::LitInt) {
            let lit: syn::LitInt = input.parse()?;
            let count = lit.base10_parse::<u32>().map_err(|_| {
                syn::Error::new(
                    lit.span(),
                    "`#[query_budget(...)]` expects a whole, non-negative query count that fits \
                     in a `u32`, e.g. `#[query_budget(3)]`",
                )
            })?;
            Budget::Bounded(count)
        } else if input.peek(syn::Ident) {
            let ident: syn::Ident = input.parse()?;
            if ident == "unbounded" {
                Budget::Unbounded
            } else {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "unknown `#[query_budget(...)]` argument `{ident}`; expected a query \
                         count like `3`, or `unbounded`"
                    ),
                ));
            }
        } else {
            return Err(syn::Error::new(
                input.span(),
                "`#[query_budget(...)]` expects a query count like `#[query_budget(3)]` \
                 or `#[query_budget(unbounded, reason = ...)]`",
            ));
        };

        // Optional trailing `, reason = "..."` — carried for humans and code
        // review, not for the analysis.
        if input.peek(syn::Token![,]) {
            let _: syn::Token![,] = input.parse()?;
            if !input.is_empty() {
                let key: syn::Ident = input.parse()?;
                if key != "reason" {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `#[query_budget(...)]` key `{key}`; the only supported key \
                             is `reason`"
                        ),
                    ));
                }
                let _: syn::Token![=] = input.parse()?;
                let _: syn::LitStr = input.parse()?;
            }
        }

        if !input.is_empty() {
            return Err(input.error("unexpected trailing tokens in `#[query_budget(...)]`"));
        }

        Ok(Self { budget })
    }
}

// ── Cost lattice ─────────────────────────────────────────────────────

/// An upper bound on the queries a construct can issue.
#[derive(Clone)]
enum Cost {
    /// Provably at most this many queries.
    Exact(u32),
    /// Not provable: the count depends on runtime data, or on code the
    /// analysis cannot read.
    Unbounded(Box<Unprovable>),
}

/// Why a construct could not be bounded, where, and what resolves it.
#[derive(Clone)]
struct Unprovable {
    span: Span,
    /// What the analysis found, phrased to complete "cannot be proven: …".
    message: String,
    /// The fix, phrased as its own sentence.
    hint: String,
}

impl Cost {
    const ZERO: Self = Self::Exact(0);

    fn unbounded(span: Span, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::Unbounded(Box::new(Unprovable {
            span,
            message: message.into(),
            hint: hint.into(),
        }))
    }

    const fn is_zero(&self) -> bool {
        matches!(self, Self::Exact(0))
    }

    /// Sequential composition: two statements in a row issue both.
    fn then(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded(u), _) | (Self::Exact(_), Self::Unbounded(u)) => Self::Unbounded(u),
            (Self::Exact(a), Self::Exact(b)) => Self::Exact(a.saturating_add(b)),
        }
    }

    /// Branch composition: only one arm runs, so the bound is the worst arm.
    fn or_worst(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded(u), _) | (Self::Exact(_), Self::Unbounded(u)) => Self::Unbounded(u),
            (Self::Exact(a), Self::Exact(b)) => Self::Exact(a.max(b)),
        }
    }

    /// Repetition by a compile-time-known factor.
    fn repeated(self, times: u32) -> Self {
        match self {
            Self::Unbounded(u) => Self::Unbounded(u),
            Self::Exact(n) => Self::Exact(n.saturating_mul(times)),
        }
    }
}

// ── Analyzer ─────────────────────────────────────────────────────────

/// Walks a function body accumulating [`Cost`] and a human-readable ledger of
/// every counted call site (which the diagnostic prints back).
struct Analyzer {
    /// Identifiers currently bound to a database handle.
    handles: HashSet<String>,
    /// Counted call sites, in source order, for the diagnostic.
    ledger: Vec<String>,
    /// Errors raised by malformed `#[query_cost]` / `#[query_exempt]`.
    errors: Vec<syn::Error>,
}

impl Analyzer {
    const fn new(handles: HashSet<String>) -> Self {
        Self {
            handles,
            ledger: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn count(&mut self, what: &str) -> Cost {
        self.ledger.push(format!("`{what}`"));
        Cost::Exact(1)
    }

    // ── Blocks and statements ────────────────────────────────────────

    fn block(&mut self, block: &Block) -> Cost {
        // A block scopes the names its own `let`s introduce (or shadow away),
        // so `{ let repo = 1; }` must not strip `repo` for the rest of the
        // function. It does *not* scope an assignment to a name declared
        // outside it: `if flag { active = repo; }` is how a conditional
        // initialises an outer binding, and restoring the whole set discarded
        // that (#1667 review, round five). Restore exactly the declared names.
        let outer = self.handles.clone();
        let mut declared = HashSet::new();
        let mut cost = Cost::ZERO;
        for stmt in &block.stmts {
            if let Stmt::Local(local) = stmt {
                collect_pat_idents(&local.pat, &mut declared);
            }
            cost = cost.then(self.stmt(stmt));
        }
        for name in declared {
            if outer.contains(&name) {
                self.handles.insert(name);
            } else {
                self.handles.remove(&name);
            }
        }
        cost
    }

    fn stmt(&mut self, stmt: &Stmt) -> Cost {
        let attrs: &[Attribute] = match stmt {
            Stmt::Local(local) => &local.attrs,
            Stmt::Expr(expr, _) => expr_attrs(expr),
            Stmt::Macro(m) => &m.attrs,
            // A nested `fn`/`struct`/`impl` is a *definition*; nothing runs
            // here. A call to it is analysed at the call site instead.
            Stmt::Item(_) => return Cost::ZERO,
        };

        // An annotation replaces the statement's *cost*, never its effect on
        // handle tracking. `#[query_exempt(...)] let shard = repo.for_shard(id);`
        // still binds `shard` to a handle; forgetting that made a later
        // `shard.find_all()` — including one inside a loop — invisible (#1667
        // review).
        match self.annotation(attrs) {
            Some(Annotation::Cost(n)) => {
                if let Stmt::Local(local) = stmt {
                    self.bind_handles(local);
                }
                return Cost::Exact(n);
            }
            Some(Annotation::Exempt) => {
                if let Stmt::Local(local) = stmt {
                    self.bind_handles(local);
                }
                return Cost::ZERO;
            }
            None => {}
        }

        match stmt {
            Stmt::Local(local) => self.local(local),
            Stmt::Expr(expr, _) => self.expr(expr),
            Stmt::Macro(m) => self.mac(&m.mac),
            Stmt::Item(_) => Cost::ZERO,
        }
    }

    fn local(&mut self, local: &Local) -> Cost {
        let mut cost = Cost::ZERO;
        if let Some(init) = &local.init {
            cost = cost.then(self.expr(&init.expr));
            if let Some((_, diverge)) = &init.diverge {
                cost = cost.then(self.expr(diverge));
            }
        }
        self.bind_handles(local);
        cost
    }

    /// Propagate handle identity from a `let`'s initialiser to its bindings.
    ///
    /// Split out of [`Self::local`] because it must run for an *annotated*
    /// local too: the annotation declares what the statement costs, not
    /// whether its bindings are handles.
    fn bind_handles(&mut self, local: &Local) {
        let Some(init) = &local.init else {
            // `let repo;` with no initialiser still shadows: the name holds
            // nothing yet, so it is not a handle.
            self.rebind(&local.pat, false);
            return;
        };
        // A binding initialised from a handle — or from a chain rooted at one,
        // so a builder chain split across `let`s keeps its identity — is
        // itself a handle from here on.
        if self.expr_is_handle(&init.expr) || self.chain_root_is_handle(&init.expr) {
            self.rebind(&local.pat, true);
        } else if matches!(&local.pat, Pat::Tuple(_)) && matches!(&*init.expr, Expr::Tuple(_)) {
            let (Pat::Tuple(pat), Expr::Tuple(init_tuple)) = (&local.pat, &*init.expr) else {
                unreachable!("guarded by the matches! above")
            };
            // `let (conn, key) = (db, id);` — pair the pattern against the
            // initialiser element-wise so the handle keeps its tracking.
            for (element_pat, element) in pat.elems.iter().zip(init_tuple.elems.iter()) {
                let is_handle = self.expr_is_handle(element) || self.chain_root_is_handle(element);
                self.rebind(element_pat, is_handle);
            }
        } else {
            // Shadowing. `let repo = repo.find_all().await?;` rebinds the name
            // to a `Vec`, and the old identity must go with it — otherwise
            // `repo.len()` is scored as another query and handing the rows to a
            // renderer is reported as a handle escaping (#1667 review, round
            // three). `block` restores the outer set, so this cannot leak past
            // the enclosing scope.
            self.rebind(&local.pat, false);
        }
    }

    /// Bind `pat` for a nested scope, returning what those names meant before.
    ///
    /// Restoring *only* these names matters: a whole-set snapshot would also
    /// undo an assignment the scope made to an **outer** name, and assignments
    /// are not lexically scoped (#1667 review, round five).
    fn enter_binding_scope(&mut self, pat: &Pat, is_handle: bool) -> Vec<(String, bool)> {
        let mut names = HashSet::new();
        collect_pat_idents(pat, &mut names);
        let saved: Vec<(String, bool)> = names
            .iter()
            .map(|n| (n.clone(), self.handles.contains(n)))
            .collect();
        self.rebind(pat, is_handle);
        saved
    }

    /// Undo an [`Self::enter_binding_scope`], name by name.
    fn leave_binding_scope(&mut self, saved: Vec<(String, bool)>) {
        for (name, was_handle) in saved {
            if was_handle {
                self.handles.insert(name);
            } else {
                self.handles.remove(&name);
            }
        }
    }

    /// Bind every name in `pat` to a handle, or clear whatever identity those
    /// names carried. Insertion alone is not enough: a `HashSet` of names has
    /// no notion of shadowing, so a rebinding must actively remove.
    fn rebind(&mut self, pat: &Pat, is_handle: bool) {
        let mut names = HashSet::new();
        collect_pat_idents(pat, &mut names);
        if is_handle {
            self.handles.extend(names);
        } else {
            for name in names {
                self.handles.remove(&name);
            }
        }
    }

    /// Read a `#[query_cost(N)]` / `#[query_exempt(...)]` statement annotation.
    fn annotation(&mut self, attrs: &[Attribute]) -> Option<Annotation> {
        let annotated: Vec<&Attribute> = attrs
            .iter()
            .filter(|a| a.path().is_ident(ATTR_QUERY_COST) || a.path().is_ident(ATTR_QUERY_EXEMPT))
            .collect();
        if annotated.len() > 1 {
            self.errors.push(syn::Error::new_spanned(
                annotated[1],
                "a statement carries more than one query annotation; keep exactly one of \
                 `#[query_cost(N)]` or `#[query_exempt(reason = ...)]`",
            ));
        }
        for attr in attrs {
            if attr.path().is_ident(ATTR_QUERY_COST) {
                let Ok(lit) = attr.parse_args::<syn::LitInt>() else {
                    self.errors.push(syn::Error::new_spanned(
                        attr,
                        "`#[query_cost(...)]` expects a query count, e.g. `#[query_cost(2)]`",
                    ));
                    return Some(Annotation::Cost(0));
                };
                return Some(match lit.base10_parse::<u32>() {
                    Ok(n) => Annotation::Cost(n),
                    Err(err) => {
                        self.errors.push(err);
                        Annotation::Cost(0)
                    }
                });
            }
            if attr.path().is_ident(ATTR_QUERY_EXEMPT) {
                // The hatch's whole value is that a reviewer can see why. A
                // reason-less or typo'd exemption is a silent hole, so it is an
                // error rather than an accepted default.
                if let Err(err) = parse_reason(attr) {
                    self.errors.push(err);
                }
                return Some(Annotation::Exempt);
            }
        }
        None
    }

    // ── Expressions ──────────────────────────────────────────────────

    fn expr(&mut self, expr: &Expr) -> Cost {
        self.expr_in(expr, false)
    }

    /// `awaited` says whether this expression is the base of an enclosing
    /// `.await` — the marker that a chain actually runs.
    #[allow(clippy::too_many_lines)]
    fn expr_in(&mut self, expr: &Expr, awaited: bool) -> Cost {
        match expr {
            Expr::Await(e) => self.expr_in(&e.base, true),
            Expr::Try(e) => self.expr_in(&e.expr, awaited),
            Expr::Paren(e) => self.expr_in(&e.expr, awaited),
            Expr::Group(e) => self.expr_in(&e.expr, awaited),

            Expr::MethodCall(mc) => self.method_chain(mc, awaited),
            // `(|| async move { … })()` — the shape `#[cached]` wraps a handler
            // body in when it expands first. The closure runs exactly once, so
            // look straight through it rather than reporting a closure the user
            // never wrote.
            Expr::Call(call) if immediately_invoked_closure(&call.func).is_some() => {
                let closure = immediately_invoked_closure(&call.func)
                    .expect("guarded by the match arm above");
                let mut cost = Cost::ZERO;
                for arg in &call.args {
                    cost = cost.then(self.expr(arg));
                }
                // Bind each parameter from its argument before walking the
                // body. `(|active| async move { active.find_all().await })(repo)`
                // otherwise left `active` untracked and the finder free (#1667
                // review, round five) — the general closure arm scopes
                // parameters, but this shortcut bypassed it.
                let mut saved = Vec::new();
                for (param, arg) in closure.inputs.iter().zip(call.args.iter()) {
                    let is_handle = self.expr_carries_handle(arg);
                    saved.extend(self.enter_binding_scope(param, is_handle));
                }
                // A parameter with no matching argument still shadows.
                for param in closure.inputs.iter().skip(call.args.len()) {
                    saved.extend(self.enter_binding_scope(param, false));
                }
                let body = self.expr(&closure.body);
                self.leave_binding_scope(saved);
                cost.then(body)
            }
            Expr::Call(call) => self.call(call),
            Expr::Macro(m) => self.mac(&m.mac),
            Expr::Closure(closure) => {
                // A parameter shadows whatever the name meant outside:
                // `rows.iter().map(|repo| repo.len())` binds an element, not
                // the repository. Analysing the body against the outer
                // identity scored `len()` as a query and then blamed the
                // closure for it (#1667 review, round four).
                let mut saved = Vec::new();
                for input in &closure.inputs {
                    saved.extend(self.enter_binding_scope(input, false));
                }
                let body = self.expr(&closure.body);
                self.leave_binding_scope(saved);
                if body.is_zero() {
                    Cost::ZERO
                } else if matches!(body, Cost::Unbounded(_)) {
                    // The body already explains itself (a nested loop, an
                    // opaque helper). Don't overwrite a better diagnostic.
                    body
                } else {
                    Cost::unbounded(
                        closure.span(),
                        format!(
                            "a database query ({}) runs inside a closure, which the analysis \
                             cannot prove runs only once",
                            self.last_counted()
                        ),
                        BATCH_HINT,
                    )
                }
            }

            Expr::ForLoop(f) => {
                let iter = self.expr(&f.expr);
                // The loop variable inherits the iterable's provenance:
                // `for active in [repo]` yields a handle under a new name, and
                // leaving it untracked made the body's finder free (#1667
                // review, round five).
                let yields_handle = self.expr_carries_handle(&f.expr);
                let saved = self.enter_binding_scope(&f.pat, yields_handle);
                let before = self.ledger.len();
                let body = self.block(&f.body);
                self.leave_binding_scope(saved);
                iter.then(self.bound_loop(body, const_bound(&f.expr), f.span(), before))
            }
            Expr::While(w) => {
                // The condition is re-evaluated on every iteration, so a query
                // in it (`while let Some(job) = repo.next_pending().await?`) is
                // loop-resident, not a one-off prologue.
                let before = self.ledger.len();
                let cond = self.expr(&w.cond);
                let body = self.block(&w.body);
                self.bound_loop(cond.then(body), None, w.span(), before)
            }
            Expr::Loop(l) => {
                let before = self.ledger.len();
                let body = self.block(&l.body);
                self.bound_loop(body, None, l.span(), before)
            }

            Expr::If(i) => {
                let cond = self.expr(&i.cond);
                let then = self.block(&i.then_branch);
                let els = i
                    .else_branch
                    .as_ref()
                    .map_or(Cost::ZERO, |(_, e)| self.expr(e));
                cond.then(then.or_worst(els))
            }
            Expr::Match(m) => {
                let mut cost = self.expr(&m.expr);
                // Exactly one *body* runs, so bodies take the worst arm. Guards
                // are different: a failing guard falls through to the next
                // matching arm, so every guard on the path can run. They sum.
                let mut worst_body = Cost::ZERO;
                for arm in &m.arms {
                    if let Some((_, guard)) = &arm.guard {
                        cost = cost.then(self.expr(guard));
                    }
                    let body = match self.annotation(&arm.attrs) {
                        Some(Annotation::Cost(n)) => Cost::Exact(n),
                        Some(Annotation::Exempt) => Cost::ZERO,
                        None => self.expr(&arm.body),
                    };
                    worst_body = worst_body.or_worst(body);
                }
                cost.then(worst_body)
            }

            Expr::Block(syn::ExprBlock { block, .. })
            | Expr::Async(syn::ExprAsync { block, .. })
            | Expr::Unsafe(syn::ExprUnsafe { block, .. })
            | Expr::TryBlock(syn::ExprTryBlock { block, .. }) => self.block(block),

            Expr::Array(a) => self.each(a.elems.iter()),
            Expr::Tuple(t) => self.each(t.elems.iter()),
            Expr::Assign(a) => {
                let cost = self.expr(&a.left).then(self.expr(&a.right));
                // `active = repo;` makes `active` a handle just as a `let`
                // would; without this the queries through it vanish (#1667
                // review, round three). A non-handle RHS clears it, for the
                // same reason shadowing does.
                if let Expr::Path(path) = &*a.left
                    && let Some(ident) = path.path.get_ident()
                {
                    let is_handle =
                        self.expr_is_handle(&a.right) || self.chain_root_is_handle(&a.right);
                    if is_handle {
                        self.handles.insert(ident.to_string());
                    } else {
                        self.handles.remove(&ident.to_string());
                    }
                }
                cost
            }
            Expr::Binary(b) => self.expr(&b.left).then(self.expr(&b.right)),
            Expr::Break(b) => b.expr.as_deref().map_or(Cost::ZERO, |e| self.expr(e)),
            Expr::Cast(c) => self.expr(&c.expr),
            Expr::Field(f) => self.expr(&f.base),
            Expr::Index(i) => self.expr(&i.expr).then(self.expr(&i.index)),
            Expr::Let(l) => self.expr(&l.expr),
            Expr::Range(r) => {
                let start = r.start.as_deref().map_or(Cost::ZERO, |e| self.expr(e));
                let end = r.end.as_deref().map_or(Cost::ZERO, |e| self.expr(e));
                start.then(end)
            }
            Expr::RawAddr(r) => self.expr(&r.expr),
            Expr::Reference(r) => self.expr(&r.expr),
            Expr::Repeat(r) => self.expr(&r.expr).then(self.expr(&r.len)),
            Expr::Return(r) => r.expr.as_deref().map_or(Cost::ZERO, |e| self.expr(e)),
            Expr::Struct(s) => {
                let mut cost = self.each(s.fields.iter().map(|f| &f.expr));
                if let Some(rest) = &s.rest {
                    cost = cost.then(self.expr(rest));
                }
                cost
            }
            Expr::Unary(u) => self.expr(&u.expr),
            Expr::Yield(y) => y.expr.as_deref().map_or(Cost::ZERO, |e| self.expr(e)),

            // Forms that hold no reachable call at all.
            Expr::Lit(_) | Expr::Path(_) | Expr::Infer(_) | Expr::Continue(_) => Cost::ZERO,
            Expr::Const(c) => self.block(&c.block),

            // `Expr::Verbatim` — syntax this `syn` could not parse — and any
            // variant a future `syn` adds. Assuming those are query-free would
            // make the no-false-negative claim depend on the toolchain, so an
            // unreadable form that names the handle is reported instead.
            other => {
                if tokens_mention_any(&other.to_token_stream(), &self.handles) {
                    Cost::unbounded(
                        other.span(),
                        "an expression form the analysis does not recognise names the database \
                         handle",
                        DECLARE_HINT,
                    )
                } else {
                    Cost::ZERO
                }
            }
        }
    }

    fn each<'a>(&mut self, exprs: impl Iterator<Item = &'a Expr>) -> Cost {
        let mut cost = Cost::ZERO;
        for expr in exprs {
            cost = cost.then(self.expr(expr));
        }
        cost
    }

    /// Turn a loop body's cost into the loop's cost.
    fn bound_loop(
        &mut self,
        body: Cost,
        bound: Option<u32>,
        span: Span,
        ledger_before: usize,
    ) -> Cost {
        if body.is_zero() {
            return Cost::ZERO;
        }
        if let Cost::Unbounded(_) = body {
            return body;
        }
        if let Some(times) = bound {
            if times > 1 {
                for entry in &mut self.ledger[ledger_before..] {
                    write!(entry, " ×{times}").expect("writing to a String cannot fail");
                }
            }
            return body.repeated(times);
        }
        let culprit = self.ledger.get(ledger_before).map_or_else(
            || "a declared query cost".to_string(),
            |entry| format!("a database query ({entry})"),
        );
        Cost::unbounded(
            span,
            format!(
                "{culprit} runs inside a loop, so this handler's query count grows with the size \
                 of the collection — the classic N+1"
            ),
            LOOP_HINT,
        )
    }

    /// Analyse a `recv.a().b().c()` chain as one unit: in autumn a chain rooted
    /// at a handle is *one* query, however many builder methods it carries.
    fn method_chain(&mut self, outermost: &ExprMethodCall, awaited: bool) -> Cost {
        // Innermost-first list of the methods in this chain, and the receiver
        // the chain is rooted at.
        let mut methods: Vec<&ExprMethodCall> = Vec::new();
        let mut current = outermost;
        let root = loop {
            methods.push(current);
            match &*current.receiver {
                Expr::MethodCall(inner) => current = inner,
                other => break other,
            }
        };
        methods.reverse();

        let mut cost = self.expr(root);

        // Where the handle enters the chain: the root itself, or the first
        // conventional accessor (`app.db()…`, `ctx.repo()…`). Methods before
        // it are ordinary; methods after it act on a handle.
        let handle_from = if self.expr_is_handle(root) {
            Some(0)
        } else {
            methods
                .iter()
                .position(|m| HANDLE_ACCESSORS.contains(&m.method.to_string().as_str()))
                .map(|i| i + 1)
        };

        // Arguments run regardless of what the chain does with them.
        for method in &methods {
            cost = cost.then(self.method_args(method));
        }
        if matches!(cost, Cost::Unbounded(_)) {
            return cost;
        }

        if let Some(from) = handle_from {
            let on_handle = &methods[from.min(methods.len())..];
            if let Some(walker) = on_handle
                .iter()
                .find(|m| UNBOUNDED_METHODS.contains(&m.method.to_string().as_str()))
            {
                return Cost::unbounded(
                    walker.span(),
                    format!(
                        "`{}` walks the whole table through a keyset cursor, so it issues one \
                         query per batch — a count that depends on the table's size, not on the \
                         code",
                        walker.method
                    ),
                    DECLARE_HINT,
                );
            }
            if let Some(preload) = on_handle.iter().find(|m| m.method == "preload") {
                cost = cost.then(self.preload_cost(preload));
                // A finder ahead of the preload is its own query:
                // `repo.recent_page(1).preload(rows, spec)` is 1 + N, not N.
                if on_handle
                    .iter()
                    .any(|m| m.method != "preload" && !is_handle_builder(&m.method.to_string()))
                {
                    cost = cost.then(self.count("finder ahead of `preload`"));
                }
                return cost;
            }
            // The chain is counted where it is *built*, not where it is
            // awaited: `let fut = repo.find_all();` still costs a query, and
            // collecting such futures to `join_all` later is still an N+1.
            let Some(last) = on_handle.last().map(|m| m.method.to_string()) else {
                return cost;
            };
            // A builder name refines the *next* query rather than issuing one —
            // unless the chain is awaited here, in which case the terminal call
            // really did run (a user finder may share a builder's name).
            if is_handle_builder(&last) && !awaited {
                return cost;
            }
            return cost.then(self.count(&last));
        }

        // Not rooted at a handle: a diesel executor call is the round trip.
        for method in &methods {
            let is_executor = EXECUTORS.contains(&method.method.to_string().as_str());
            let takes_handle = method.args.iter().any(|a| self.expr_carries_handle(a));
            // Provenance, not just the name: this chain is *not* rooted at a
            // handle, so `store.load(id).await` and `client.execute(req).await`
            // land here with executor-shaped names and no database in sight.
            // Counting those spent a route's budget on ordinary async APIs
            // (#1667 review, round three). A real diesel executor is handed the
            // connection — `query.load(&mut conn).await` — so require that.
            if is_executor && takes_handle {
                let name = method.method.to_string();
                cost = cost.then(self.count(&name));
            } else if takes_handle && !is_executor {
                return Cost::unbounded(
                    method.span(),
                    format!(
                        "`{}` is handed the database handle, and what it does with it is another \
                         function's business",
                        method.method
                    ),
                    DECLARE_HINT,
                );
            }
        }
        cost
    }

    /// Analyse one method call's arguments, treating a closure that may run per
    /// element differently from a transaction callback that runs exactly once.
    fn method_args(&mut self, method: &ExprMethodCall) -> Cost {
        let name = method.method.to_string();
        // Both kinds run their closure at most once, so the body is a fixed
        // cost either way — but only a transaction hands its closure a
        // *connection*. Promoting an `Option`/`Result` combinator's parameter
        // to a handle made `result.unwrap_or_else(|error| error.to_string())`
        // count `error.to_string()` as a query (#1667 review, round two).
        let is_transaction = TRANSACTION_METHODS.contains(&name.as_str());
        let runs_once = is_transaction || AT_MOST_ONCE_CLOSURE_METHODS.contains(&name.as_str());
        let mut cost = Cost::ZERO;
        for arg in &method.args {
            if runs_once {
                cost = cost.then(self.callback_arg(arg, is_transaction));
                continue;
            }
            cost = cost.then(self.expr(arg));
        }
        cost
    }

    /// An argument to something that invokes it exactly once: the closure body
    /// is counted as a fixed cost rather than a per-element one.
    ///
    /// `binds_handle` says whether the closure's parameter is a database
    /// connection. It is for a transaction callback (`db.tx(|conn| …)`); it is
    /// **not** for an `Option`/`Result` combinator, whose parameter is the
    /// contained value.
    fn callback_arg(&mut self, arg: &Expr, binds_handle: bool) -> Cost {
        let Expr::Closure(closure) = arg else {
            return self.expr(arg);
        };
        // Same scoping as the general closure arm: a parameter shadows the
        // outer meaning of its name. A transaction's parameter *is* a
        // connection, so it binds; any other parameter clears.
        let mut saved = Vec::new();
        for input in &closure.inputs {
            saved.extend(self.enter_binding_scope(input, binds_handle));
        }
        let cost = self.expr(&closure.body);
        self.leave_binding_scope(saved);
        cost
    }

    /// `.preload(rows, Post::preload().author().tags())` issues one batched
    /// `WHERE ... IN (...)` per association named in the spec.
    fn preload_cost(&mut self, preload: &ExprMethodCall) -> Cost {
        let Some(spec) = preload.args.iter().nth(1) else {
            return Cost::unbounded(
                preload.span(),
                "`preload(...)` was called without an association spec, so its batched queries \
                 cannot be counted",
                DECLARE_HINT,
            );
        };
        let associations = count_associations(spec);
        if associations == 0 {
            return Cost::unbounded(
                spec.span(),
                "the `preload(...)` association spec is not a literal builder chain \
                 (e.g. `Post::preload().author()`), so its batched queries cannot be counted",
                DECLARE_HINT,
            );
        }
        for _ in 0..associations {
            self.ledger.push("`preload` association".to_string());
        }
        Cost::Exact(associations)
    }

    fn call(&mut self, call: &ExprCall) -> Cost {
        let name = call_path_name(call);
        let runs_once = name
            .as_deref()
            .is_some_and(|n| TRANSACTION_FREE_FNS.contains(&n));

        let mut cost = self.expr(&call.func);
        for arg in &call.args {
            if runs_once {
                // `scoped_transaction` / `savepoint` — always a connection.
                cost = cost.then(self.callback_arg(arg, true));
                continue;
            }
            cost = cost.then(self.expr(arg));
        }
        if runs_once {
            // The connection is the callback's, not an escape into opaque code.
            return cost;
        }
        // An unreadable argument already explains itself; don't relabel it.
        if matches!(cost, Cost::Unbounded(_)) {
            return cost;
        }

        if let Some(name) = &name
            && SAFE_FREE_FNS.contains(&name.as_str())
        {
            return cost;
        }

        if call.args.iter().any(|a| self.expr_carries_handle(a)) {
            // `Post::published(&mut db)` / `Todo::page(&page, &mut db)` — a
            // model-level finder. Same framework contract as a repository
            // method: one call, one query, declare it if it is more.
            if is_associated_fn_path(call) {
                let label = name.unwrap_or_else(|| "finder".to_string());
                return cost.then(self.count(&label));
            }
            let label = name.unwrap_or_else(|| "this call".to_string());
            return Cost::unbounded(
                call.span(),
                format!(
                    "`{label}` is handed the database handle, and what it does with it is \
                     another function's business"
                ),
                DECLARE_HINT,
            );
        }
        cost
    }

    /// A macro body is an opaque token soup to `syn`. If it so much as names a
    /// handle, the queries it may hide are reported rather than assumed absent.
    fn mac(&self, mac: &syn::Macro) -> Cost {
        if !tokens_mention_any(&mac.tokens, &self.handles) {
            return Cost::ZERO;
        }
        let name = mac
            .path
            .segments
            .last()
            .map_or_else(|| "macro".to_string(), |s| s.ident.to_string());
        // An `await` anywhere in the body is a future being driven right here,
        // whatever macro wraps it — checked *before* the allowlist so that
        // `html! { div { (fetch_title(&mut db).await?) } }` is still reported.
        if tokens_contain_await(&mac.tokens) {
            return Self::opaque_macro(mac, &name);
        }
        // A logging, formatting or template macro cannot itself issue a query,
        // however it names the handle (`tracing::debug!(db = ?db, …)`,
        // `html! { (render_row(p, &repo)) }`): it does not await, and a sync
        // helper it hands the handle to cannot run an async query.
        if INERT_MACROS.contains(&name.as_str()) {
            return Cost::ZERO;
        }
        // Anything else that names a handle is reported. This is an
        // allowlist, deliberately: a receiver-shaped test ("does the body call
        // `repo.method()`?") looked like it separated `tokio::join!` from
        // `html!`, but it does not — `join!(Post::published(&mut db), …)`
        // passes the handle as an *argument*, exactly as a template passes it
        // to a render helper, and slipped through (#1667 review, round two).
        // Only a name we recognise as inert may be assumed query-free.
        Self::opaque_macro(mac, &name)
    }

    /// Report a macro body the analysis cannot read but which names a handle.
    fn opaque_macro(mac: &syn::Macro, name: &str) -> Cost {
        Cost::unbounded(
            mac.span(),
            format!(
                "the `{name}!` macro body names the database handle, and a macro body is opaque \
                 token soup to the analysis"
            ),
            "Move the query out of the macro and into a statement the analysis can read, declare \
             the statement with `#[query_cost(N)]`, or exempt it with \
             `#[query_exempt(reason = ...)]`. See docs/guide/query-budgets.md.",
        )
    }

    // ── Handle tracking ──────────────────────────────────────────────

    /// Does this expression *evaluate to* a database handle (as opposed to
    /// merely mentioning one)?
    fn expr_is_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.handles.contains(&i.to_string())),
            Expr::Reference(r) => self.expr_is_handle(&r.expr),
            Expr::RawAddr(r) => self.expr_is_handle(&r.expr),
            Expr::Paren(p) => self.expr_is_handle(&p.expr),
            Expr::Group(g) => self.expr_is_handle(&g.expr),
            // `self.conn().await?` is a common shape for a fallible/async
            // handle accessor (a pooled connection getter) — without peeling
            // `.await`/`?`, `conn` in `let mut conn = self.conn().await?;`
            // silently falls through to "not a handle," and every later
            // query issued through `conn` goes uncounted with no diagnostic
            // at all (#2546 review). Deliberately recurses into the
            // *narrower* `awaited_expr_is_fresh_handle`, not `expr_is_handle`
            // itself: peeling through to a bare `Expr::Path` here would
            // re-derive handle-ness from `chain_root_is_handle`'s "deferred
            // future" tracking (`let pending = repo.find_all(); let rows =
            // pending.await?;`) and wrongly mark `rows` — the resolved
            // `Vec<Post>` — as a handle too, miscounting a harmless
            // `rows.len()` as another query (caught by the existing
            // `a_deferred_repository_future_is_counted_once` unit test).
            //
            // Deliberately has NO matching `Expr::Await` arm here (only
            // `Expr::Try`, which peels through its own inner `Await` via
            // `awaited_expr_is_fresh_handle`): a *fallible* accessor's
            // `.await` alone yields `Result<Conn, E>`, not `Conn` — only the
            // `?` actually unwraps to the handle. Promoting a bare
            // `self.conn().await` (no `?`) would treat that `Result` itself
            // as a handle, so a later `result.is_err()` or `.unwrap()` call
            // gets miscounted as a query (#2546 review, round 3).
            Expr::Try(t) => self.awaited_expr_is_fresh_handle(&t.expr),
            Expr::Unary(u) => matches!(u.op, syn::UnOp::Deref(_)) && self.expr_is_handle(&u.expr),
            // A field of a handle is a handle (`db.inner`), and so is a field
            // that conventionally holds one (`self.repo`, `state.db`) — a
            // service method's queries would otherwise be invisible.
            Expr::Field(f) => self.expr_is_handle(&f.base) || member_is_handle_accessor(&f.member),
            Expr::MethodCall(mc) => {
                let method = mc.method.to_string();
                if HANDLE_ACCESSORS.contains(&method.as_str()) {
                    return true;
                }
                // `.expect(...)`/`.unwrap()` stand in for `?` on a fallible
                // accessor (`ctx.conn().await.expect("connection")`, #2546
                // review round 5) — the unwrap call itself issues nothing,
                // so it is never counted, but the value it unwraps can still
                // be a fresh handle. Recurses into the narrower
                // `awaited_expr_is_fresh_handle`, not `expr_is_handle`, for
                // the same reason `Expr::Try` does below: a bare
                // `Expr::Path` here must not re-derive handle-ness from
                // `chain_root_is_handle`'s unrelated deferred-future
                // tracking.
                //
                // KNOWN LIMITATION (#2546 review, round 7): splitting the
                // accessor call and the unwrap across two statements
                // (`let result = ctx.conn().await; let mut db =
                // result.unwrap();`) still isn't caught — the receiver here
                // is `Expr::Path("result")`, which `awaited_expr_is_fresh_handle`
                // deliberately never matches (that is what keeps round
                // three's `result.is_err()` case from being miscounted).
                // Catching the split form soundly would mean tracking a
                // *third* binding state alongside "is a handle" and "is
                // not" — "is a `Result` that becomes a handle once
                // unwrapped" — threaded through every place `handles` is
                // scoped and restored (`block`, `enter_binding_scope`,
                // `rebind`, tuple destructuring). That is a real,
                // structural change to the analyzer's state model, not
                // another naming-list entry, and this round's evidence is
                // a constructed example rather than a concrete occurrence
                // in this codebase (unlike rounds two, five, and six, each
                // pinned to a real file:line). Left as an acknowledged gap
                // rather than taken on speculatively.
                if RESULT_UNWRAP_METHODS.contains(&method.as_str()) {
                    return self.awaited_expr_is_fresh_handle(&mc.receiver);
                }
                HANDLE_BUILDERS.contains(&method.as_str()) && self.expr_is_handle(&mc.receiver)
            }
            // A handle selected through a conditional is still a handle:
            // `let active = if primary { repo } else { replica };`. Every arm
            // is a candidate and *any* of them being a handle is enough — the
            // conservative direction, since guessing "not a handle" here loses
            // every query made through the binding (#1667 review, round four).
            Expr::If(i) => {
                self.block_tail_is_handle(&i.then_branch)
                    || i.else_branch
                        .as_ref()
                        .is_some_and(|(_, e)| self.expr_is_handle(e))
            }
            Expr::Match(m) => m.arms.iter().any(|arm| self.expr_is_handle(&arm.body)),
            Expr::Block(b) => self.block_tail_is_handle(&b.block),
            Expr::Unsafe(u) => self.block_tail_is_handle(&u.block),
            _ => false,
        }
    }

    /// The peeled target of a `.await`/`?`: does *this* expression freshly
    /// produce a handle, as opposed to naming a variable that was already
    /// tracked by `chain_root_is_handle`'s deferred-future provenance?
    /// Deliberately omits `Expr::Path` (and anything that bottoms out in
    /// one) — see the comment on `expr_is_handle`'s `Expr::Await`/`Expr::Try`
    /// arms for why re-deriving handle-ness from a bound name here is wrong.
    fn awaited_expr_is_fresh_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Reference(r) => self.awaited_expr_is_fresh_handle(&r.expr),
            Expr::RawAddr(r) => self.awaited_expr_is_fresh_handle(&r.expr),
            Expr::Paren(p) => self.awaited_expr_is_fresh_handle(&p.expr),
            Expr::Group(g) => self.awaited_expr_is_fresh_handle(&g.expr),
            Expr::Await(a) => self.awaited_expr_is_fresh_handle(&a.base),
            Expr::Try(t) => self.awaited_expr_is_fresh_handle(&t.expr),
            Expr::Field(f) => self.expr_is_handle(&f.base) || member_is_handle_accessor(&f.member),
            // Deliberately checks only `HANDLE_ACCESSORS`, never
            // `HANDLE_BUILDERS`: reaching this arm means the call was
            // *awaited* (peeled through `Expr::Await`/`Expr::Try` above), and
            // the method-chain cost counter's own rule is that an awaited
            // builder-named call "really did run" as the terminal query — "a
            // user finder may share a builder's name." So `let rows =
            // repo.page(1).await?;` is correctly counted as one query by
            // that counter, but `rows` itself is the query's *result*, not a
            // handle; matching `HANDLE_BUILDERS` here as well would promote
            // `rows` too and miscount a later `rows.len()` as a second query
            // (#2546 review, round 4). A `HANDLE_ACCESSORS` name is
            // different: those never issue a query even when awaited — they
            // only ever produce a handle to query with next.
            //
            // KNOWN LIMITATION (#2546 review, round 6): a checkout idiom
            // like `db.pool().get().await?` (deadpool/bb8-style, and the
            // shape `autumn-cli`'s own generated scaffold tests emit at
            // `autumn-cli/src/generate/scaffold.rs:14419`) is *not* caught
            // here — `get` is not a recognized accessor name, so this falls
            // through to `false`. A fix was attempted: recurse into the
            // receiver for any non-accessor terminal name, so `get` would
            // inherit handle-ness from the `pool` accessor beneath it. That
            // did catch the checkout idiom, but it is syntactically
            // indistinguishable from a genuine terminal query made through
            // an accessor-obtained handle
            // (`state.db().find_recipients(...).await?`, the exact shape
            // `query_budget_job_shaped_accessor_batched.rs` already pins as
            // required to compile clean) — both are "some name, chained off
            // an accessor call, then awaited." Recursing into the receiver
            // fixed the former and silently reintroduced round four's exact
            // regression on the latter, verified with the same fixture and
            // reverted before landing. There is no reliable syntactic
            // signal — no type information is available to this proc
            // macro — that tells "a checkout wrapper" apart from "a named
            // query" when both share this shape, so this stays a real,
            // acknowledged boundary of the analysis rather than a bug
            // fixable by another naming heuristic.
            Expr::MethodCall(mc) => HANDLE_ACCESSORS.contains(&mc.method.to_string().as_str()),
            _ => false,
        }
    }

    /// Does a block *evaluate to* a handle — i.e. does its tail expression?
    fn block_tail_is_handle(&self, block: &Block) -> bool {
        match block.stmts.last() {
            Some(Stmt::Expr(expr, None)) => self.expr_is_handle(expr),
            _ => false,
        }
    }

    /// Is this a method chain whose root is a handle (`repo.aggregate().order()`)?
    fn chain_root_is_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::MethodCall(mc) => {
                self.expr_is_handle(&mc.receiver) || self.chain_root_is_handle(&mc.receiver)
            }
            Expr::Paren(p) => self.chain_root_is_handle(&p.expr),
            Expr::Group(g) => self.chain_root_is_handle(&g.expr),
            // Deliberately does NOT peel `Expr::Await`/`Expr::Try` the way
            // `expr_is_handle` does: this function backs `bind_handles`'
            // "future built but not yet awaited" tracking (`let fut =
            // repo.find_all();` — the doc's "a repository future is counted
            // where it is built, not where it is awaited"), where *any*
            // chain rooted at a handle should keep provenance even through a
            // non-accessor terminal call. Peeling here would make an
            // already-awaited, already-resolved query result (`let posts =
            // repo.find_all().await?;`) register as a handle too — `posts`
            // is a `Vec<Post>`, and a bare `.len()` on it was miscounted as
            // a third query before this comment was added (regression
            // caught by the existing `query_budget_over_budget.rs`
            // fixture). `expr_is_handle`'s own `Expr::Await`/`Expr::Try` arms
            // already cover the real gap (an accessor call like
            // `self.conn().await?`) without this broader, unawaited-chain
            // reach.
            _ => false,
        }
    }

    /// Does this expression *carry* a handle into a callee — directly, or
    /// wrapped one level deep in a context struct, tuple, or slice?
    fn expr_carries_handle(&self, expr: &Expr) -> bool {
        if self.expr_is_handle(expr) {
            return true;
        }
        match expr {
            Expr::Struct(s) => s.fields.iter().any(|f| self.expr_carries_handle(&f.expr)),
            Expr::Tuple(t) => t.elems.iter().any(|e| self.expr_carries_handle(e)),
            Expr::Array(a) => a.elems.iter().any(|e| self.expr_carries_handle(e)),
            Expr::Call(c) => c.args.iter().any(|a| self.expr_carries_handle(a)),
            Expr::Reference(r) => self.expr_carries_handle(&r.expr),
            Expr::RawAddr(r) => self.expr_carries_handle(&r.expr),
            Expr::Paren(p) => self.expr_carries_handle(&p.expr),
            Expr::Group(g) => self.expr_carries_handle(&g.expr),
            // No separate `Expr::Await`/`Expr::Try` arms: the `expr_is_handle`
            // check above already covers them via `awaited_expr_is_fresh_handle`,
            // which deliberately does not fall through to a bare `Expr::Path`
            // — recursing into the full `expr_carries_handle` here instead
            // would reach `Expr::Path` through its own `Expr::Await`/`Expr::Try`
            // (if added) and reintroduce the same false-positive this file
            // documents on `expr_is_handle`.
            _ => false,
        }
    }

    fn last_counted(&self) -> String {
        self.ledger
            .last()
            .cloned()
            .unwrap_or_else(|| "a database query".to_string())
    }
}

enum Annotation {
    Cost(u32),
    Exempt,
}

// ── Free helpers ─────────────────────────────────────────────────────
//
// `agent_authority.rs` forked this module's handle tracking (see its module
// doc comment) and most of its similarly-named helpers have since diverged on
// purpose — it carries a richer `Handle` enum where this module only needs a
// flat set of names, and its `INERT_MACROS` deliberately excludes `vec!`/
// `format!` for a reason specific to that analyser (see its `mac()`).
//
// `expr_attrs`, `expr_attrs_mut`, `item_attrs_mut`, `immediately_invoked_closure`,
// `call_path_name`, `tokens_contain_await`, `collect_pat_idents`, the
// `StripAnnotations`/`VisitMut` impl, and `EXECUTORS` (above) *are* still
// byte-for-byte copies — they just enumerate `syn`'s own `Expr`/`Item`
// variants or do generic token-tree plumbing, owing nothing to either
// analyser's rules. Fix a bug in one of those and fix it in the other;
// `shared_helpers_match_query_budget` in `agent_authority.rs`'s test module
// fails the build if they drift.

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(e) => &e.attrs,
        Expr::Assign(e) => &e.attrs,
        Expr::Async(e) => &e.attrs,
        Expr::Await(e) => &e.attrs,
        Expr::Binary(e) => &e.attrs,
        Expr::Block(e) => &e.attrs,
        Expr::Break(e) => &e.attrs,
        Expr::Call(e) => &e.attrs,
        Expr::Cast(e) => &e.attrs,
        Expr::Closure(e) => &e.attrs,
        Expr::Const(e) => &e.attrs,
        Expr::Continue(e) => &e.attrs,
        Expr::Field(e) => &e.attrs,
        Expr::ForLoop(e) => &e.attrs,
        Expr::Group(e) => &e.attrs,
        Expr::If(e) => &e.attrs,
        Expr::Index(e) => &e.attrs,
        Expr::Infer(e) => &e.attrs,
        Expr::Let(e) => &e.attrs,
        Expr::Lit(e) => &e.attrs,
        Expr::Loop(e) => &e.attrs,
        Expr::Macro(e) => &e.attrs,
        Expr::Match(e) => &e.attrs,
        Expr::MethodCall(e) => &e.attrs,
        Expr::Paren(e) => &e.attrs,
        Expr::Path(e) => &e.attrs,
        Expr::Range(e) => &e.attrs,
        Expr::RawAddr(e) => &e.attrs,
        Expr::Reference(e) => &e.attrs,
        Expr::Repeat(e) => &e.attrs,
        Expr::Return(e) => &e.attrs,
        Expr::Struct(e) => &e.attrs,
        Expr::Try(e) => &e.attrs,
        Expr::TryBlock(e) => &e.attrs,
        Expr::Tuple(e) => &e.attrs,
        Expr::Unary(e) => &e.attrs,
        Expr::Unsafe(e) => &e.attrs,
        Expr::While(e) => &e.attrs,
        Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

/// The compile-time iteration count of `for _ in <iter>`, when there is one.
fn const_bound(iter: &Expr) -> Option<u32> {
    match iter {
        Expr::Range(range) => {
            let start = range.start.as_deref().map_or(Some(0u32), int_literal)?;
            let end = int_literal(range.end.as_deref()?)?;
            let span = end.checked_sub(start)?;
            match range.limits {
                syn::RangeLimits::HalfOpen(_) => Some(span),
                syn::RangeLimits::Closed(_) => span.checked_add(1),
            }
        }
        Expr::Array(array) => u32::try_from(array.elems.len()).ok(),
        Expr::Reference(r) => const_bound(&r.expr),
        Expr::Paren(p) => const_bound(&p.expr),
        Expr::Group(g) => const_bound(&g.expr),
        // `[a, b, c].iter()`, `[a, b].into_iter()`, `(0..3).rev()` …
        Expr::MethodCall(mc)
            if matches!(
                mc.method.to_string().as_str(),
                "iter" | "into_iter" | "iter_mut" | "rev"
            ) =>
        {
            const_bound(&mc.receiver)
        }
        _ => None,
    }
}

fn int_literal(expr: &Expr) -> Option<u32> {
    match expr {
        Expr::Lit(lit) => match &lit.lit {
            syn::Lit::Int(int) => int.base10_parse::<u32>().ok(),
            _ => None,
        },
        Expr::Paren(p) => int_literal(&p.expr),
        Expr::Group(g) => int_literal(&g.expr),
        _ => None,
    }
}

/// Count the associations named in a `Post::preload().author().tags()` spec.
/// Each one is a separate batched query.
fn count_associations(spec: &Expr) -> u32 {
    match spec {
        Expr::MethodCall(mc) => {
            let nested: u32 = mc.args.iter().map(count_associations).sum();
            1 + nested + count_associations(&mc.receiver)
        }
        Expr::Closure(closure) => count_associations(&closure.body),
        Expr::Paren(p) => count_associations(&p.expr),
        Expr::Group(g) => count_associations(&g.expr),
        Expr::Reference(r) => count_associations(&r.expr),
        _ => 0,
    }
}

/// The closure of an immediately-invoked `(|| …)()` / `(|| async move …)()`.
fn immediately_invoked_closure(func: &Expr) -> Option<&syn::ExprClosure> {
    match func {
        Expr::Closure(closure) => Some(closure),
        Expr::Paren(p) => immediately_invoked_closure(&p.expr),
        Expr::Group(g) => immediately_invoked_closure(&g.expr),
        _ => None,
    }
}

/// Is this call `Type::assoc_fn(…)` — a model-level finder rather than a free
/// function? The framework's `#[model]` finders take the handle as an argument,
/// so they are counted like a repository method instead of being reported.
fn is_associated_fn_path(call: &ExprCall) -> bool {
    let Expr::Path(path) = &*call.func else {
        return false;
    };
    let segments = &path.path.segments;
    segments.len() >= 2
        && segments
            .iter()
            .nth(segments.len() - 2)
            .is_some_and(|s| s.ident.to_string().starts_with(char::is_uppercase))
}

/// Does this token stream contain an `await` — the marker that something in it
/// actually runs a future, and so could be a query?
fn tokens_contain_await(tokens: &TokenStream) -> bool {
    tokens.clone().into_iter().any(|tt| match tt {
        TokenTree::Ident(ident) => ident == "await",
        TokenTree::Group(group) => tokens_contain_await(&group.stream()),
        _ => false,
    })
}

/// Do these tokens open a function definition? Used to decide whether a parse
/// failure is "you put this on a non-function" or "your function body has a
/// syntax error rustc will explain better than we can".
fn tokens_look_like_fn(tokens: &TokenStream) -> bool {
    tokens.clone().into_iter().any(|tt| match tt {
        TokenTree::Ident(ident) => ident == "fn",
        _ => false,
    })
}

/// Require `#[query_exempt(reason = "…")]` to actually carry a reason.
fn parse_reason(attr: &Attribute) -> syn::Result<String> {
    let reason: syn::MetaNameValue = attr.parse_args().map_err(|_| {
        syn::Error::new_spanned(
            attr,
            "`#[query_exempt(...)]` needs the reason it is safe, e.g. \
             `#[query_exempt(reason = \"reads the warm cache only\")]`",
        )
    })?;
    if !reason.path.is_ident("reason") {
        return Err(syn::Error::new_spanned(
            &reason.path,
            "the only `#[query_exempt(...)]` key is `reason`",
        ));
    }
    match &reason.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(text),
            ..
        }) if !text.value().trim().is_empty() => Ok(text.value()),
        other => Err(syn::Error::new_spanned(
            other,
            "`#[query_exempt(reason = ...)]` takes a non-empty string explaining why the call \
             site issues no query",
        )),
    }
}

/// Is this a chain method that refines a later query rather than issuing one?
fn is_handle_builder(method: &str) -> bool {
    HANDLE_BUILDERS.contains(&method)
}

/// Does this struct field name conventionally hold a database handle?
fn member_is_handle_accessor(member: &syn::Member) -> bool {
    match member {
        syn::Member::Named(ident) => HANDLE_ACCESSORS.contains(&ident.to_string().as_str()),
        syn::Member::Unnamed(_) => false,
    }
}

fn call_path_name(call: &ExprCall) -> Option<String> {
    match &*call.func {
        Expr::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// Does this token stream name any of `idents` (recursing into groups)?
fn tokens_mention_any(tokens: &TokenStream, idents: &HashSet<String>) -> bool {
    tokens.clone().into_iter().any(|tt| match tt {
        TokenTree::Ident(ident) => idents.contains(&ident.to_string()),
        TokenTree::Group(group) => tokens_mention_any(&group.stream(), idents),
        _ => false,
    })
}

fn collect_pat_idents(pat: &Pat, out: &mut HashSet<String>) {
    match pat {
        Pat::Ident(p) => {
            out.insert(p.ident.to_string());
            if let Some((_, sub)) = &p.subpat {
                collect_pat_idents(sub, out);
            }
        }
        Pat::Type(p) => collect_pat_idents(&p.pat, out),
        Pat::Reference(p) => collect_pat_idents(&p.pat, out),
        Pat::Paren(p) => collect_pat_idents(&p.pat, out),
        Pat::Tuple(p) => p.elems.iter().for_each(|e| collect_pat_idents(e, out)),
        Pat::TupleStruct(p) => p.elems.iter().for_each(|e| collect_pat_idents(e, out)),
        Pat::Slice(p) => p.elems.iter().for_each(|e| collect_pat_idents(e, out)),
        Pat::Or(p) => p.cases.iter().for_each(|e| collect_pat_idents(e, out)),
        Pat::Struct(p) => p
            .fields
            .iter()
            .for_each(|f| collect_pat_idents(&f.pat, out)),
        _ => {}
    }
}

/// Does this type name a database handle — directly, behind a reference, or
/// inside an extractor wrapper such as `Extension<Db>`?
fn type_is_handle(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_is_handle(&r.elem),
        Type::Paren(p) => type_is_handle(&p.elem),
        Type::Group(g) => type_is_handle(&g.elem),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            let name = segment.ident.to_string();
            if HANDLE_TYPES.contains(&name.as_str())
                || name.ends_with("Db")
                || name.ends_with("Repository")
            {
                return true;
            }
            // Look inside an extractor wrapper (`Extension<Db>`, `State<Db>`)
            // for an *exact* handle type only. Recursing with the suffix
            // heuristic would make `Form<NewRepo>` a database handle.
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                return args.args.iter().any(|arg| match arg {
                    syn::GenericArgument::Type(inner) => type_is_exact_handle(inner),
                    _ => false,
                });
            }
            false
        }
        _ => false,
    }
}

/// A type named exactly like a framework handle, ignoring the name-suffix
/// heuristic. Used when peering inside extractor generics, where a suffix match
/// would sweep in unrelated application types.
fn type_is_exact_handle(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_is_exact_handle(&r.elem),
        Type::Paren(p) => type_is_exact_handle(&p.elem),
        Type::Group(g) => type_is_exact_handle(&g.elem),
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|s| HANDLE_TYPES.contains(&s.ident.to_string().as_str())),
        _ => false,
    }
}

/// The handle bindings a handler's signature introduces.
fn signature_handles(input_fn: &ItemFn) -> HashSet<String> {
    let mut handles = HashSet::new();
    for arg in &input_fn.sig.inputs {
        if let syn::FnArg::Typed(typed) = arg
            && type_is_handle(&typed.ty)
        {
            collect_pat_idents(&typed.pat, &mut handles);
        }
    }
    handles
}

/// Removes `#[query_cost]` / `#[query_exempt]` from the emitted function: they
/// are this macro's own vocabulary and mean nothing to rustc.
struct StripAnnotations;

impl VisitMut for StripAnnotations {
    fn visit_item_fn_mut(&mut self, item_fn: &mut ItemFn) {
        // Includes the annotated handler itself: a stray `#[query_cost]` on the
        // function has already been diagnosed, and leaving it behind would add
        // rustc's "cannot find attribute" on top.
        retain_foreign(&mut item_fn.attrs);
        syn::visit_mut::visit_item_fn_mut(self, item_fn);
    }

    fn visit_arm_mut(&mut self, arm: &mut syn::Arm) {
        retain_foreign(&mut arm.attrs);
        syn::visit_mut::visit_arm_mut(self, arm);
    }

    fn visit_item_mut(&mut self, item: &mut syn::Item) {
        if let Some(attrs) = item_attrs_mut(item) {
            retain_foreign(attrs);
        }
        syn::visit_mut::visit_item_mut(self, item);
    }

    fn visit_stmt_mut(&mut self, stmt: &mut Stmt) {
        match stmt {
            Stmt::Local(local) => retain_foreign(&mut local.attrs),
            Stmt::Macro(m) => retain_foreign(&mut m.attrs),
            Stmt::Expr(..) | Stmt::Item(_) => {}
        }
        syn::visit_mut::visit_stmt_mut(self, stmt);
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        if let Some(attrs) = expr_attrs_mut(expr) {
            retain_foreign(attrs);
        }
        syn::visit_mut::visit_expr_mut(self, expr);
    }
}

/// The outer attributes of the item kinds that can appear inside a function
/// body. Anything else cannot carry one of our annotations meaningfully.
const fn item_attrs_mut(item: &mut syn::Item) -> Option<&mut Vec<Attribute>> {
    Some(match item {
        syn::Item::Fn(i) => &mut i.attrs,
        syn::Item::Const(i) => &mut i.attrs,
        syn::Item::Static(i) => &mut i.attrs,
        syn::Item::Struct(i) => &mut i.attrs,
        syn::Item::Enum(i) => &mut i.attrs,
        syn::Item::Impl(i) => &mut i.attrs,
        syn::Item::Mod(i) => &mut i.attrs,
        syn::Item::Trait(i) => &mut i.attrs,
        syn::Item::Type(i) => &mut i.attrs,
        syn::Item::Use(i) => &mut i.attrs,
        _ => return None,
    })
}

fn retain_foreign(attrs: &mut Vec<Attribute>) {
    attrs.retain(|attr| {
        !attr.path().is_ident(ATTR_QUERY_COST) && !attr.path().is_ident(ATTR_QUERY_EXEMPT)
    });
}

const fn expr_attrs_mut(expr: &mut Expr) -> Option<&mut Vec<Attribute>> {
    Some(match expr {
        Expr::Array(e) => &mut e.attrs,
        Expr::Assign(e) => &mut e.attrs,
        Expr::Async(e) => &mut e.attrs,
        Expr::Await(e) => &mut e.attrs,
        Expr::Binary(e) => &mut e.attrs,
        Expr::Block(e) => &mut e.attrs,
        Expr::Break(e) => &mut e.attrs,
        Expr::Call(e) => &mut e.attrs,
        Expr::Cast(e) => &mut e.attrs,
        Expr::Closure(e) => &mut e.attrs,
        Expr::Const(e) => &mut e.attrs,
        Expr::Continue(e) => &mut e.attrs,
        Expr::Field(e) => &mut e.attrs,
        Expr::ForLoop(e) => &mut e.attrs,
        Expr::Group(e) => &mut e.attrs,
        Expr::If(e) => &mut e.attrs,
        Expr::Index(e) => &mut e.attrs,
        Expr::Infer(e) => &mut e.attrs,
        Expr::Let(e) => &mut e.attrs,
        Expr::Lit(e) => &mut e.attrs,
        Expr::Loop(e) => &mut e.attrs,
        Expr::Macro(e) => &mut e.attrs,
        Expr::Match(e) => &mut e.attrs,
        Expr::MethodCall(e) => &mut e.attrs,
        Expr::Paren(e) => &mut e.attrs,
        Expr::Path(e) => &mut e.attrs,
        Expr::Range(e) => &mut e.attrs,
        Expr::RawAddr(e) => &mut e.attrs,
        Expr::Reference(e) => &mut e.attrs,
        Expr::Repeat(e) => &mut e.attrs,
        Expr::Return(e) => &mut e.attrs,
        Expr::Struct(e) => &mut e.attrs,
        Expr::Try(e) => &mut e.attrs,
        Expr::TryBlock(e) => &mut e.attrs,
        Expr::Tuple(e) => &mut e.attrs,
        Expr::Unary(e) => &mut e.attrs,
        Expr::Unsafe(e) => &mut e.attrs,
        Expr::While(e) => &mut e.attrs,
        Expr::Yield(e) => &mut e.attrs,
        _ => return None,
    })
}

// ── Macro entry point ────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub fn query_budget_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Keep the original tokens so a parse failure still emits the item — one
    // purpose-written diagnostic beats a cascade of "cannot find" errors.
    let original = item.clone();
    let parsed_fn = syn::parse2::<ItemFn>(item);

    let budget = match syn::parse2::<BudgetAttr>(attr) {
        Ok(parsed) => parsed.budget,
        Err(err) => {
            // Emit the function with our own statement annotations stripped, so
            // a typo in the budget yields one diagnostic instead of that plus
            // an "unknown attribute" per annotation in the body.
            let err = err.to_compile_error();
            return parsed_fn.map_or_else(
                |_| quote! { #original #err },
                |mut input_fn| {
                    StripAnnotations.visit_item_fn_mut(&mut input_fn);
                    quote! { #input_fn #err }
                },
            );
        }
    };

    let mut input_fn = match parsed_fn {
        Ok(parsed) => parsed,
        Err(parse_error) => {
            // A malformed function body is rustc's error to report, not ours;
            // claiming "this is not a function" about a function is worse than
            // useless.
            let err = if tokens_look_like_fn(&original) {
                parse_error.to_compile_error()
            } else {
                syn::Error::new(
                    Span::call_site(),
                    "`#[query_budget(...)]` can only be applied to a function — put it on the \
                     route handler whose queries you want bounded",
                )
                .to_compile_error()
            };
            return quote! { #original #err };
        }
    };

    // Our statement annotations mean nothing on the function itself.
    if let Some(stray) = input_fn
        .attrs
        .iter()
        .find(|a| a.path().is_ident(ATTR_QUERY_COST) || a.path().is_ident(ATTR_QUERY_EXEMPT))
    {
        let err = syn::Error::new_spanned(
            stray,
            "`#[query_cost(...)]` / `#[query_exempt(...)]` annotate a statement inside the \
             handler, not the handler itself; the handler's ceiling is the `#[query_budget(N)]` \
             argument",
        )
        .to_compile_error();
        StripAnnotations.visit_item_fn_mut(&mut input_fn);
        return quote! { #input_fn #err };
    }

    let mut analyzer = Analyzer::new(signature_handles(&input_fn));
    let cost = analyzer.block(&input_fn.block);

    let mut errors: Vec<syn::Error> = std::mem::take(&mut analyzer.errors);
    let proven = match (&budget, &cost) {
        (Budget::Unbounded, Cost::Exact(n)) => Some(*n),
        (Budget::Unbounded, Cost::Unbounded(_)) => None,
        (Budget::Bounded(limit), Cost::Exact(n)) => {
            if n > limit {
                errors.push(syn::Error::new_spanned(
                    &input_fn.sig.ident,
                    over_budget_message(*limit, *n, &analyzer.ledger),
                ));
            }
            Some(*n)
        }
        (Budget::Bounded(limit), Cost::Unbounded(unprovable)) => {
            errors.push(syn::Error::new(
                unprovable.span,
                format!(
                    "`#[query_budget({limit})]` cannot be proven: {}.\n\n{}",
                    unprovable.message, unprovable.hint
                ),
            ));
            None
        }
    };

    StripAnnotations.visit_item_fn_mut(&mut input_fn);

    let fn_name = input_fn.sig.ident.clone();
    let marker = format_ident!("__AUTUMN_QUERY_BUDGET_{}", fn_name);
    let vis = input_fn.vis.clone();
    let handler_name = fn_name.to_string();
    let handler_name = handler_name
        .strip_prefix("r#")
        .unwrap_or(&handler_name)
        .to_string();
    // A method taking `self` may sit in a trait impl, where an associated const
    // the trait never declared is not a legal item. The analysis still runs;
    // only the marker is withheld.
    let takes_self = input_fn
        .sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, syn::FnArg::Receiver(_)));
    let declared = match budget {
        Budget::Bounded(n) => quote! { ::core::option::Option::Some(#n) },
        Budget::Unbounded => quote! { ::core::option::Option::None },
    };
    let proven = proven.map_or_else(
        || quote! { ::core::option::Option::None },
        |n| quote! { ::core::option::Option::Some(#n) },
    );
    let errors = errors.iter().map(syn::Error::to_compile_error);

    // Only plain `cfg` is replayed. A `cfg_attr` applies *some other*
    // attribute conditionally, and that attribute is written for a function:
    // copying `#[cfg_attr(feature = "tracing", tracing::instrument)]` verbatim
    // puts `tracing::instrument` on a `const` and fails to compile once the
    // feature is on (#1667 review). Dropping it is safe — the marker is a
    // standalone const that names nothing from the function, so emitting it in
    // a configuration where the function is absent costs a dead const, which
    // `dead_code` below already allows.
    let cfgs: Vec<&Attribute> = input_fn
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("cfg"))
        .collect();

    let marker_const = if takes_self {
        TokenStream::new()
    } else {
        quote! {
            #(#cfgs)*
            #[doc(hidden)]
            #[allow(non_upper_case_globals, dead_code)]
            #vis const #marker: ::autumn_web::query_budget::StaticQueryBudget =
                ::autumn_web::query_budget::StaticQueryBudget::new(
                    #handler_name,
                    #declared,
                    #proven,
                );
        }
    };

    quote! {
        #input_fn

        #marker_const

        #(#errors)*
    }
}

fn over_budget_message(limit: u32, actual: u32, ledger: &[String]) -> String {
    let plural = if actual == 1 { "query" } else { "queries" };
    let counted = if ledger.is_empty() {
        String::new()
    } else {
        format!("\n\ncounted: {}", ledger.join(", "))
    };
    format!(
        "`#[query_budget({limit})]` is exceeded: a statically reachable path through this handler \
         issues {actual} database {plural}.{counted}\n\nBatch the extra lookups with \
         `preload(...)`, raise the budget, or declare a call site with `#[query_cost(N)]` / \
         `#[query_exempt(reason = ...)]`"
    )
}

#[cfg(test)]
mod tests {
    use quote::ToTokens as _;

    use super::*;

    /// Expand `#[query_budget(attr)]` over `item` and return the generated code
    /// as a string.
    fn expand(attr: &str, item: &str) -> String {
        let attr: TokenStream = attr.parse().expect("attr parses");
        let item: TokenStream = item.parse().expect("item parses");
        query_budget_macro(attr, item).to_string()
    }

    /// The `compile_error!` messages the expansion emitted, concatenated.
    ///
    /// Walks the token stream rather than the stringified output: the
    /// diagnostics themselves contain quoted attribute examples, so substring
    /// scanning for the closing quote is not reliable.
    fn error_of(attr: &str, item: &str) -> Option<String> {
        let attr: TokenStream = attr.parse().expect("attr parses");
        let item: TokenStream = item.parse().expect("item parses");
        let out = query_budget_macro(attr, item);
        let mut messages = Vec::new();
        collect_compile_errors(&out, &mut messages);
        (!messages.is_empty()).then(|| messages.join("\n---\n"))
    }

    /// The generated marker const, sliced out of a full expansion. The
    /// handler's own attributes stay on the handler, so a test that asks what
    /// the *const* carries must not look at the whole stream.
    fn marker_const_of(expansion: &str) -> &str {
        let doc_hidden = expansion
            .find("# [doc (hidden)]")
            .expect("expansion contains a marker const");
        // The const's own attributes (`#[cfg(...)]`) precede `#[doc(hidden)]`,
        // so start just past the handler body's closing brace.
        let start = expansion[..doc_hidden]
            .rfind('}')
            .map_or(doc_hidden, |brace| brace + 1);
        &expansion[start..]
    }

    /// Expand and return the rendered token stream, asserting it carries no
    /// compile error. Used by tests that assert on what expansion *emits*
    /// rather than on whether it rejects.
    fn expand_ok(attr: &str, item: &str) -> String {
        let attr_ts: TokenStream = attr.parse().expect("attr parses");
        let item_ts: TokenStream = item.parse().expect("item parses");
        let out = query_budget_macro(attr_ts, item_ts);
        let mut messages = Vec::new();
        collect_compile_errors(&out, &mut messages);
        assert!(
            messages.is_empty(),
            "expected a clean expansion, got: {}",
            messages.join("\n---\n")
        );
        out.to_string()
    }

    fn collect_compile_errors(tokens: &TokenStream, out: &mut Vec<String>) {
        let mut saw_marker = false;
        for tt in tokens.clone() {
            match tt {
                proc_macro2::TokenTree::Ident(ident) => {
                    saw_marker = ident == "compile_error";
                }
                proc_macro2::TokenTree::Group(group) => {
                    if saw_marker {
                        if let Some(proc_macro2::TokenTree::Literal(lit)) =
                            group.stream().into_iter().next()
                            && let Ok(text) = syn::parse2::<syn::LitStr>(lit.to_token_stream())
                        {
                            out.push(text.value());
                        }
                        saw_marker = false;
                    } else {
                        collect_compile_errors(&group.stream(), out);
                    }
                }
                _ => {}
            }
        }
    }

    /// The emitted item alone — the expansion with any trailing
    /// `compile_error!` diagnostics cut off, since those quote our own
    /// attribute names and would defeat a "did it leak?" substring check.
    fn emitted_item(attr: &str, item: &str) -> String {
        let out = expand(attr, item);
        out.find(":: core :: compile_error")
            .map_or_else(|| out.clone(), |idx| out[..idx].to_string())
    }

    fn assert_clean(attr: &str, item: &str) {
        if let Some(err) = error_of(attr, item) {
            panic!("expected a clean expansion, got compile error: {err}");
        }
    }

    fn assert_error_contains(attr: &str, item: &str, needles: &[&str]) {
        let err = error_of(attr, item)
            .unwrap_or_else(|| panic!("expected a compile error, expansion was clean"));
        for needle in needles {
            assert!(
                err.contains(needle),
                "diagnostic {err:?} does not mention {needle:?}"
            );
        }
    }

    // ── Flat handlers ────────────────────────────────────────────────

    #[test]
    fn flat_handler_under_budget_compiles_clean() {
        assert_clean(
            "3",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.select(Post::as_select()).load(&mut *db).await?;
                let count: i64 = posts::table.count().get_result(&mut *db).await?;
                Ok(render(&posts, count))
            }
            ",
        );
    }

    #[test]
    fn flat_handler_over_budget_is_rejected() {
        assert_error_contains(
            "1",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let a = posts::table.load(&mut *db).await?;
                let b = tags::table.load(&mut *db).await?;
                Ok(render(&a, &b))
            }
            ",
            &["query_budget(1)", "2"],
        );
    }

    #[test]
    fn zero_budget_rejects_any_query() {
        assert_error_contains(
            "0",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let a = posts::table.load(&mut *db).await?;
                Ok(render(&a))
            }
            ",
            &["query_budget(0)"],
        );
    }

    #[test]
    fn handler_with_no_queries_fits_a_zero_budget() {
        assert_clean(
            "0",
            r"
            async fn about() -> Markup {
                render_about()
            }
            ",
        );
    }

    // ── The classic N+1 ──────────────────────────────────────────────

    #[test]
    fn query_in_a_for_loop_over_runtime_rows_is_rejected() {
        assert_error_contains(
            "3",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                for p in &posts {
                    let author = users::table.filter(users::id.eq(p.author_id))
                        .first(&mut *db).await?;
                    render_row(&author);
                }
                Ok(render(&posts))
            }
            ",
            &["loop", "first"],
        );
    }

    #[test]
    fn query_in_an_iterator_closure_is_rejected() {
        assert_error_contains(
            "3",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                let authors = posts.iter().map(|p| {
                    users::table.filter(users::id.eq(p.author_id)).first(&mut *db)
                }).collect::<Vec<_>>();
                Ok(render(&authors))
            }
            ",
            &["closure"],
        );
    }

    #[test]
    fn repository_future_built_in_a_closure_is_rejected_even_without_await() {
        // `join_all(futures)` is the N+1 in functional clothing: the query is
        // committed to where the future is built, not where it is driven.
        assert_error_contains(
            "2",
            r"
            async fn index(repo: PgAuthorRepository, posts: Vec<Post>) -> AutumnResult<usize> {
                let pending: Vec<_> = posts.iter().map(|p| repo.find_by_id(p.author_id)).collect();
                Ok(pending.len())
            }
            ",
            &["closure"],
        );
    }

    #[test]
    fn a_deferred_repository_future_is_counted_once() {
        let handler = r"
            async fn show(repo: PgPostRepository) -> AutumnResult<usize> {
                let pending = repo.find_all();
                let rows = pending.await?;
                Ok(rows.len())
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn query_in_a_while_loop_is_rejected() {
        assert_error_contains(
            "2",
            r"
            async fn drain(mut db: Db) -> AutumnResult<()> {
                while has_more() {
                    let _ = posts::table.load(&mut *db).await?;
                }
                Ok(())
            }
            ",
            &["loop"],
        );
    }

    #[test]
    fn loop_without_a_query_is_free() {
        assert_clean(
            "1",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                let mut titles = Vec::new();
                for p in &posts {
                    titles.push(p.title.clone());
                }
                Ok(render(&titles))
            }
            ",
        );
    }

    #[test]
    fn const_bounded_loop_multiplies_instead_of_going_unbounded() {
        let handler = r"
            async fn warm(mut db: Db) -> AutumnResult<()> {
                for _ in 0..3 {
                    let _ = posts::table.load(&mut *db).await?;
                }
                Ok(())
            }
            ";
        assert_clean("3", handler);
        assert_error_contains("2", handler, &["3"]);
    }

    // ── Branches take the worst path, not the sum ────────────────────

    #[test]
    fn if_else_branches_take_the_maximum() {
        let handler = r"
            async fn show(mut db: Db, flag: bool) -> AutumnResult<Markup> {
                if flag {
                    let a = posts::table.load(&mut *db).await?;
                    Ok(render(&a))
                } else {
                    let b = tags::table.load(&mut *db).await?;
                    Ok(render(&b))
                }
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn match_arms_take_the_maximum() {
        let handler = r"
            async fn show(mut db: Db, kind: Kind) -> AutumnResult<Markup> {
                match kind {
                    Kind::One => {
                        let a = posts::table.load(&mut *db).await?;
                        Ok(render(&a))
                    }
                    Kind::Two => {
                        let a = posts::table.load(&mut *db).await?;
                        let b = tags::table.load(&mut *db).await?;
                        Ok(render2(&a, &b))
                    }
                }
            }
            ";
        assert_clean("2", handler);
        assert_error_contains("1", handler, &["2"]);
    }

    // ── Repository + preload surface ─────────────────────────────────

    #[test]
    fn repository_chain_counts_as_one_query() {
        assert_clean(
            "1",
            r"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let rows = repo.find_all().await?;
                Ok(render(&rows))
            }
            ",
        );
    }

    #[test]
    fn repository_builder_prefix_is_not_a_query() {
        assert_clean(
            "1",
            r"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let rows = repo.on_primary().find_all().await?;
                Ok(render(&rows))
            }
            ",
        );
    }

    #[test]
    fn preload_costs_one_query_per_association() {
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let posts = repo.find_all().await?;
                let posts = repo.preload(posts, Post::preload().author().tags()).await?;
                Ok(render(&posts))
            }
            ";
        assert_clean("3", handler);
        assert_error_contains("2", handler, &["3"]);
    }

    /// The AC's worked example: the N+1 red build becomes green by replacing
    /// the per-row lookup with a `preload`.
    #[test]
    fn preload_turns_the_red_build_green() {
        assert_error_contains(
            "2",
            r"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let posts = repo.find_all().await?;
                let mut authors = Vec::new();
                for p in &posts {
                    authors.push(repo.find_author(p.author_id).await?);
                }
                Ok(render(&posts, &authors))
            }
            ",
            &["loop"],
        );
        assert_clean(
            "2",
            r"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let posts = repo.find_all().await?;
                let posts = repo.preload(posts, Post::preload().author()).await?;
                for p in &posts {
                    let _ = p.author()?;
                }
                Ok(render(&posts))
            }
            ",
        );
    }

    // ── Opaque surfaces are reported, never silently ignored ─────────

    #[test]
    fn free_function_receiving_the_handle_is_reported() {
        assert_error_contains(
            "5",
            r"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                let links = load_links(&mut db, 1).await?;
                Ok(render(&links))
            }
            ",
            &["load_links"],
        );
    }

    #[test]
    fn dropping_the_handle_is_not_a_query() {
        assert_clean(
            "1",
            r"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                drop(db);
                Ok(render(&posts))
            }
            ",
        );
    }

    #[test]
    fn macro_body_carrying_the_handle_is_reported() {
        assert_error_contains(
            "5",
            r"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                Ok(html! { div { (fetch_title(&mut db).await?) } })
            }
            ",
            &["macro"],
        );
    }

    #[test]
    fn macro_body_without_the_handle_is_free() {
        assert_clean(
            "1",
            r#"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                Ok(html! { div { "hello" } })
            }
            "#,
        );
    }

    #[test]
    fn transaction_closure_body_is_counted_once() {
        assert_clean(
            "3",
            r"
            async fn apply(mut db: Db) -> AutumnResult<()> {
                db.transaction(|conn| async move {
                    let _ = posts::table.load(&mut *conn).await?;
                    let _ = tags::table.load(&mut *conn).await?;
                    Ok(())
                }).await?;
                Ok(())
            }
            ",
        );
    }

    // ── Escape hatches ───────────────────────────────────────────────

    #[test]
    fn unbounded_budget_accepts_a_looping_query() {
        assert_clean(
            r#"unbounded, reason = "admin backfill, bounded by an operator-supplied page size""#,
            r"
            async fn backfill(mut db: Db, ids: Vec<i64>) -> AutumnResult<()> {
                for id in ids {
                    let _ = posts::table.filter(posts::id.eq(id)).first(&mut *db).await?;
                }
                Ok(())
            }
            ",
        );
    }

    #[test]
    fn query_cost_annotation_declares_an_opaque_statement() {
        let handler = r"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                #[query_cost(2)]
                let links = load_links(&mut db, 1).await?;
                Ok(render(&links))
            }
            ";
        assert_clean("2", handler);
        assert_error_contains("1", handler, &["2"]);
    }

    #[test]
    fn query_exempt_annotation_drops_a_statement_from_the_ledger() {
        assert_clean(
            "1",
            r#"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                #[query_exempt(reason = "cache-only helper, verified query-free")]
                let extra = helper(&mut db).await?;
                Ok(render(&posts, &extra))
            }
            "#,
        );
    }

    #[test]
    fn inner_annotations_are_stripped_from_the_emitted_function() {
        let out = emitted_item(
            "2",
            r"
            async fn show(mut db: Db) -> AutumnResult<Markup> {
                #[query_cost(2)]
                let links = load_links(&mut db, 1).await?;
                Ok(render(&links))
            }
            ",
        );
        assert!(
            !out.contains("query_cost"),
            "inner annotation leaked into the emitted function: {out}"
        );
    }

    // ── Emitted artefacts ────────────────────────────────────────────

    #[test]
    fn emits_a_static_budget_marker_const() {
        let out = expand(
            "3",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                Ok(render(&posts))
            }
            ",
        );
        assert!(
            out.contains("__AUTUMN_QUERY_BUDGET_list"),
            "no marker const emitted: {out}"
        );
        assert!(
            out.contains("StaticQueryBudget"),
            "marker is untyped: {out}"
        );
    }

    #[test]
    fn a_method_taking_self_gets_no_marker_const() {
        // An associated const the trait never declared is not a legal item in a
        // trait impl, so the marker is withheld there. The analysis still runs.
        let out = expand(
            "1",
            r"
            async fn load(&self, repo: PgPostRepository) -> AutumnResult<usize> {
                Ok(repo.find_all().await?.len())
            }
            ",
        );
        assert!(
            !out.contains("__AUTUMN_QUERY_BUDGET_load"),
            "marker const emitted for a method with a self receiver: {out}"
        );
        assert_error_contains(
            "0",
            r"
            async fn load(&self, repo: PgPostRepository) -> AutumnResult<usize> {
                Ok(repo.find_all().await?.len())
            }
            ",
            &["query_budget(0)"],
        );
    }

    #[test]
    fn a_literal_loop_bound_shows_its_multiplier_in_the_ledger() {
        assert_error_contains(
            "2",
            r"
            async fn warm(repo: PgPostRepository) -> AutumnResult<()> {
                for _ in 0..3 {
                    let _ = repo.find_all().await?;
                }
                Ok(())
            }
            ",
            &["find_all", "3"],
        );
    }

    #[test]
    fn the_original_function_is_always_emitted_even_when_over_budget() {
        let out = expand(
            "0",
            r"
            async fn list(mut db: Db) -> AutumnResult<Markup> {
                let posts = posts::table.load(&mut *db).await?;
                Ok(render(&posts))
            }
            ",
        );
        assert!(out.contains("async fn list"), "handler was dropped: {out}");
    }

    // ── Attribute parsing ────────────────────────────────────────────

    // ── Regressions from the #1667 review sweep ──────────────────────

    #[test]
    fn a_query_in_a_while_condition_is_loop_resident() {
        // The condition re-runs every iteration, so a drain loop whose only
        // query is in the condition is still an N+1.
        assert_error_contains(
            "1",
            r"
            async fn drain(repo: PgJobRepository) -> AutumnResult<()> {
                while let Some(job) = repo.next_pending().await? {
                    handle(job);
                }
                Ok(())
            }
            ",
            &["loop"],
        );
    }

    #[test]
    fn match_guards_sum_because_a_failing_guard_falls_through() {
        assert_error_contains(
            "1",
            r"
            async fn show(repo: PgPostRepository, k: Kind) -> AutumnResult<usize> {
                match k {
                    _ if repo.count_a().await? > 0 => Ok(1),
                    _ if repo.count_b().await? > 0 => Ok(2),
                    _ => Ok(0),
                }
            }
            ",
            &["2"],
        );
    }

    #[test]
    fn a_handle_held_in_a_field_is_still_tracked() {
        // `self.repo` / `ctx.repo` — a service method's queries would otherwise
        // be invisible to the analysis.
        assert_error_contains(
            "1",
            r"
            async fn load(&self, ids: Vec<i64>) -> AutumnResult<()> {
                for id in ids {
                    let _ = self.repo.find_by_id(id).await?;
                }
                Ok(())
            }
            ",
            &["loop"],
        );
    }

    #[test]
    fn a_handle_reached_through_a_conventional_accessor_is_tracked() {
        assert_error_contains(
            "1",
            r"
            async fn index(app: AppState, ids: Vec<i64>) -> AutumnResult<()> {
                for id in ids {
                    let _ = app.db().find_by_id(id).await?;
                }
                Ok(())
            }
            ",
            &["loop"],
        );
    }

    #[test]
    fn a_tuple_binding_keeps_the_handle_tracked() {
        assert_error_contains(
            "0",
            r"
            async fn show(handle: Db, id: i64) -> AutumnResult<usize> {
                let (conn, key) = (handle, id);
                Ok(conn.posts().find_all().await?.len())
            }
            ",
            &["query_budget(0)"],
        );
    }

    #[test]
    fn a_handle_wrapped_in_a_context_struct_is_still_reported() {
        assert_error_contains(
            "0",
            r"
            async fn show(mut db: Db) -> AutumnResult<usize> {
                Ok(load_all(Ctx { db: &mut db }).await?)
            }
            ",
            &["load_all"],
        );
    }

    #[test]
    fn a_terminal_builder_name_that_is_awaited_is_a_query() {
        // A user finder may share a builder's name; awaiting it means it ran.
        assert_error_contains(
            "0",
            r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                Ok(repo.published().scoped().await?.len())
            }
            ",
            &["query_budget(0)"],
        );
    }

    #[test]
    fn a_finder_ahead_of_preload_is_its_own_query() {
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let posts = repo
                    .recent_page(1)
                    .preload(rows, Post::preload().author())
                    .await?;
                Ok(posts.len())
            }
            ";
        assert_clean("2", handler);
        assert_error_contains("1", handler, &["2"]);
    }

    #[test]
    fn batch_walkers_are_unbounded_not_one_query() {
        // `find_in_batches` walks the whole table through a keyset cursor.
        assert_error_contains(
            "50",
            r"
            async fn export(repo: PgPostRepository) -> AutumnResult<()> {
                let mut batches = repo.find_in_batches(1000);
                while let Some(chunk) = batches.next_batch().await? {
                    write_chunk(chunk);
                }
                Ok(())
            }
            ",
            &["find_in_batches"],
        );
    }

    // ── The real framework surface, as the examples actually write it ──

    #[test]
    fn autumn_transaction_api_counts_its_body_once() {
        // `Db::tx` is autumn's transaction API — `db.transaction(...)` does not
        // exist. Getting this wrong made every transactional handler unbuildable.
        assert_clean(
            "3",
            r"
            async fn create(mut db: Db) -> AutumnResult<()> {
                let id = db.tx(move |conn| {
                    async move {
                        let created = diesel::insert_into(collections::table)
                            .values(&new)
                            .get_result(conn)
                            .await?;
                        let _ = diesel::insert_into(links::table).values(&l).execute(conn).await?;
                        Ok(created.id)
                    }
                    .scope_boxed()
                })
                .await?;
                Ok(())
            }
            ",
        );
    }

    #[test]
    fn a_helper_handed_the_transaction_connection_is_reported() {
        // The closure parameter is a handle, so an opaque call inside the
        // transaction body cannot slip through uncounted.
        assert_error_contains(
            "5",
            r"
            async fn create(mut db: Db) -> AutumnResult<()> {
                db.tx(move |conn| async move { write_audit(conn).await?; Ok(()) }.scope_boxed())
                    .await?;
                Ok(())
            }
            ",
            &["write_audit"],
        );
    }

    #[test]
    fn model_static_finders_count_as_one_query() {
        // `Post::published(&mut db)` is the `#[model]` finder idiom the blog and
        // todo-app examples are written in — a framework finder, not opaque code.
        let handler = r"
            async fn index(mut db: Db, page: PageRequest) -> AutumnResult<usize> {
                let posts = Post::published(&mut db).await?;
                let page = Todo::page(&page, &mut db).await?;
                Ok(posts.len() + page.len())
            }
            ";
        assert_clean("2", handler);
        assert_error_contains("1", handler, &["2"]);
    }

    #[test]
    fn a_macro_that_drives_futures_without_an_await_token_is_reported() {
        // `tokio::join!` polls both futures, but its tokens carry no `await`.
        // An await-only test scores this zero and the two queries vanish —
        // a false negative, which the soundness contract forbids (#1667 review).
        let handler = r"
            async fn dashboard(repo: PgPostRepository) -> AutumnResult<usize> {
                let (a, b) = tokio::join!(repo.find_one(), repo.find_two());
                Ok(a? + b?)
            }
            ";
        assert_error_contains("0", handler, &["join"]);
        // And it stays reported however generous the budget: the body is
        // opaque, so no finite ceiling can be proven from it.
        assert_error_contains("5", handler, &["join"]);
    }

    #[test]
    fn a_template_that_passes_a_handle_to_a_helper_still_compiles() {
        // The counterpart to the test above: `&repo` is an *argument* here,
        // never a receiver, and a sync helper cannot issue an async query.
        // Reporting this would make `html!` unusable.
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let posts = repo.find_all().await?;
                Ok(html! { @for p in &posts { (render_row(p, &repo)) } })
            }
            ";
        assert_clean("1", handler);
    }

    #[test]
    fn a_cfg_attr_on_the_handler_is_not_replayed_onto_the_marker_const() {
        // `#[cfg_attr(feature = "x", tracing::instrument)]` names an attribute
        // written for a *function*. Copying it verbatim onto the generated
        // `const` fails to compile once the feature is on (#1667 review), so
        // only plain `cfg` is replayed.
        let handler = r#"
            #[cfg_attr(feature = "tracing", tracing::instrument)]
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let posts = repo.find_all().await?;
                Ok(posts.len())
            }
            "#;
        // Assert on the marker const alone — the function itself keeps its
        // `cfg_attr`, which is correct and would otherwise mask the check.
        let expansion = expand_ok("1", handler);
        let marker = marker_const_of(&expansion);
        assert!(
            !marker.contains("instrument"),
            "marker const replayed a cfg_attr payload: {marker}"
        );

        // A plain `cfg`, by contrast, still gates the const so it cannot
        // outlive the function it describes.
        let gated = r#"
            #[cfg(feature = "db")]
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let posts = repo.find_all().await?;
                Ok(posts.len())
            }
            "#;
        let gated_expansion = expand_ok("1", gated);
        assert!(
            marker_const_of(&gated_expansion).contains("cfg"),
            "plain cfg was dropped from the marker const: {gated_expansion}"
        );
    }

    #[test]
    fn an_annotated_local_still_binds_its_handle() {
        // The annotation declares what the *statement* costs. It must not also
        // erase the fact that `shard` is a handle, or every query through the
        // alias becomes invisible — including one in a loop (#1667 review).
        let handler = r#"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                #[query_exempt(reason = "selects a shard, issues nothing")]
                let shard = repo.for_shard(1);
                let rows = shard.find_all().await?;
                Ok(rows.len())
            }
            "#;
        // The exempt statement costs nothing, but the query through the alias
        // is still counted — so a budget of 0 is rejected and 1 is clean.
        assert_error_contains("0", handler, &["1"]);
        assert_clean("1", handler);

        // And the alias is still a handle inside a loop, so the N+1 is caught.
        let n_plus_one = r#"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                #[query_exempt(reason = "selects a shard, issues nothing")]
                let shard = repo.for_shard(1);
                let posts = shard.find_all().await?;
                let mut n = 0;
                for post in &posts {
                    n += shard.find_by_id(post.author_id).await?;
                }
                Ok(n)
            }
            "#;
        assert_error_contains("5", n_plus_one, &["loop"]);
    }

    #[test]
    fn a_future_driving_macro_is_reported_even_when_the_handle_is_an_argument() {
        // The receiver-shaped test this replaced saw no `db.method()` here and
        // scored it zero, although `join!` polls two model-finder futures
        // (#1667 review, round two).
        let handler = r"
            async fn dashboard(mut db: Db) -> AutumnResult<usize> {
                let (posts, todos) = tokio::join!(Post::published(&mut db), Todo::page(&page, &mut db));
                Ok(posts?.len() + todos?.len())
            }
            ";
        assert_error_contains("0", handler, &["join"]);
        assert_error_contains("9", handler, &["join"]);
    }

    #[test]
    fn an_option_combinator_closure_parameter_is_not_a_handle() {
        // `unwrap_or_else` runs its closure at most once, but its parameter is
        // the contained error — not a connection. Treating it as a handle made
        // `error.to_string()` count as a query (#1667 review, round two).
        let handler = r"
            async fn index(flag: bool) -> AutumnResult<String> {
                let result: Result<String, String> = Err(String::new());
                Ok(result.unwrap_or_else(|error| error.to_string()))
            }
            ";
        assert_clean("0", handler);
    }

    #[test]
    fn a_transaction_callback_parameter_is_still_a_handle() {
        // The counterpart to the test above: `tx` really does hand its closure
        // a connection, so a query through it is still counted.
        let handler = r"
            async fn index(mut db: Db) -> AutumnResult<usize> {
                let n = db.tx(|conn| async move { conn.find_all().await }).await?;
                Ok(n)
            }
            ";
        // 1 for the `tx` call itself, 1 for the query through `conn`.
        assert_error_contains("1", handler, &["2"]);
        assert_clean("2", handler);
    }

    #[test]
    fn an_assignment_propagates_handle_identity() {
        // `active = repo` aliases the handle exactly as a `let` would. Without
        // tracking it the loop below issues one query per id and scores zero
        // (#1667 review, round three).
        let handler = r"
            async fn index(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> {
                let mut active;
                active = repo;
                let mut n = 0;
                for id in &ids {
                    n += active.find_by_id(*id).await?;
                }
                Ok(n)
            }
            ";
        assert_error_contains("9", handler, &["loop"]);
    }

    #[test]
    fn shadowing_a_handle_clears_its_identity() {
        // `let repo = repo.find_all().await?;` rebinds the name to a `Vec`.
        // Keeping the old identity scored `repo.len()` as another query and
        // reported handing the rows to a renderer as a handle escape.
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let repo = repo.find_all().await?;
                Ok(repo.len())
            }
            ";
        assert_clean("1", handler);
        // The one query is still counted — the shadow clears identity, it does
        // not erase what already ran.
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn a_shadow_inside_a_block_does_not_leak_out_of_it() {
        // The guard on the fix above: clearing a name must be lexically scoped,
        // or an inner shadow would stop the *outer* handle being counted —
        // trading a false positive for a false negative.
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                {
                    let repo = 1;
                    let _ = repo;
                }
                let rows = repo.find_all().await?;
                Ok(rows.len())
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn an_executor_name_without_a_handle_is_not_a_query() {
        // `load` / `execute` are diesel executor names, but on a chain with no
        // database in sight they are just ordinary async APIs. Counting them by
        // name alone spent the budget on unrelated calls (#1667 review, round
        // three).
        let handler = r"
            async fn index(store: ObjectStore, client: HttpClient) -> AutumnResult<usize> {
                let blob = store.load(1).await?;
                let resp = client.execute(blob).await?;
                Ok(resp.len())
            }
            ";
        assert_clean("0", handler);
    }

    #[test]
    fn a_diesel_executor_handed_the_connection_is_still_a_query() {
        // The counterpart: provenance is the connection in the call, and when
        // it is there the round trip is still counted.
        let handler = r"
            async fn index(mut db: Db) -> AutumnResult<usize> {
                let rows = posts::table.filter(posts::published.eq(true)).load(&mut db).await?;
                Ok(rows.len())
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn a_conditionally_selected_handle_is_still_a_handle() {
        // Every arm yields a repository, so the loop below is an N+1. Scoring
        // the initialiser "not a handle" loses every query through the binding
        // (#1667 review, round four).
        let handler = r"
            async fn index(repo: PgPostRepository, replica: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> {
                let active = if primary { repo } else { replica };
                let mut n = 0;
                for id in &ids {
                    n += active.find_by_id(*id).await?;
                }
                Ok(n)
            }
            ";
        assert_error_contains("9", handler, &["loop"]);

        // `match` selects a handle the same way.
        let via_match = r"
            async fn index(repo: PgPostRepository, replica: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> {
                let active = match mode { Mode::Primary => repo, Mode::Replica => replica };
                let mut n = 0;
                for id in &ids {
                    n += active.find_by_id(*id).await?;
                }
                Ok(n)
            }
            ";
        assert_error_contains("9", via_match, &["loop"]);
    }

    #[test]
    fn rebinding_a_handle_through_a_conditional_keeps_it_tracked() {
        // The regression guard for the shadowing fix: clearing on a
        // non-handle initialiser must not swallow `let repo = if … { repo }`,
        // where the name genuinely still holds a handle.
        let handler = r"
            async fn index(repo: PgPostRepository, replica: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> {
                let repo = if primary { repo } else { replica };
                let mut n = 0;
                for id in &ids {
                    n += repo.find_by_id(*id).await?;
                }
                Ok(n)
            }
            ";
        assert_error_contains("9", handler, &["loop"]);
    }

    #[test]
    fn a_closure_parameter_shadows_an_outer_handle_name() {
        // `|repo|` binds a row, not the repository. Analysing the body against
        // the outer identity counted `len()` as a query and then reported it as
        // unbounded for sitting in a closure (#1667 review, round four).
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let rows = repo.find_all().await?;
                Ok(rows.iter().map(|repo| repo.len()).sum())
            }
            ";
        assert_clean("1", handler);
    }

    #[test]
    fn a_closure_parameter_shadow_does_not_leak_past_the_closure() {
        // The counterpart: clearing the name inside the closure must not stop
        // the real handle being counted afterwards.
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let rows = repo.find_all().await?;
                let n: usize = rows.iter().map(|repo| repo.len()).sum();
                let more = repo.find_all().await?;
                Ok(n + more.len())
            }
            ";
        assert_clean("2", handler);
        assert_error_contains("1", handler, &["2"]);
    }

    #[test]
    fn an_iife_binds_its_parameters_from_its_arguments() {
        // The `#[cached]` shortcut looked through the closure without binding
        // parameters, so the handle arrived under a new name and vanished
        // (#1667 review, round five).
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let rows = (|active| async move { active.find_all().await })(repo).await?;
                Ok(rows.len())
            }
            ";
        assert_error_contains("0", handler, &["1"]);
        assert_clean("1", handler);
    }

    #[test]
    fn a_for_loop_pattern_inherits_the_iterables_provenance() {
        // `for active in [repo]` yields a handle under a new name; leaving it
        // untracked made the body's finder free.
        //
        // A literal array carries a const bound, so this loop is *bounded* —
        // one iteration, one query. The point is that the query is seen at
        // all, not that it is unbounded.
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                let mut n = 0;
                for active in [repo] {
                    n += active.find_all().await?.len();
                }
                Ok(n)
            }
            ";
        assert_error_contains("0", handler, &["find_all"]);
        assert_clean("1", handler);
    }

    #[test]
    fn a_loop_pattern_does_not_leak_past_the_loop() {
        // Counterpart: the loop variable's identity is scoped to the loop.
        let handler = r"
            async fn index(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> {
                for repo in &ids {
                    let _ = repo;
                }
                let rows = repo.find_all().await?;
                Ok(rows.len())
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn a_handle_assigned_inside_a_branch_survives_the_branch() {
        // A block scopes its own `let`s, not an assignment to a name declared
        // outside it. Restoring the whole handle set discarded the alias and
        // the finder after the conditional went uncounted (#1667 review,
        // round five) — a regression from the block-scoping fix.
        let handler = r"
            async fn index(repo: PgPostRepository, replica: PgPostRepository) -> AutumnResult<usize> {
                let active;
                if flag {
                    active = repo;
                } else {
                    active = replica;
                }
                let rows = active.find_all().await?;
                Ok(rows.len())
            }
            ";
        assert_error_contains("0", handler, &["1"]);
        assert_clean("1", handler);
    }

    #[test]
    fn a_let_inside_a_branch_is_still_scoped_to_it() {
        // The guard on the fix above: a `let` really is block-scoped, so an
        // inner shadow must not strip the outer handle afterwards.
        let handler = r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                if flag {
                    let repo = 1;
                    let _ = repo;
                }
                let rows = repo.find_all().await?;
                Ok(rows.len())
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn a_split_builder_chain_counts_the_same_as_a_joined_one() {
        // Extracting a sub-expression to a `let` changes no SQL, so it must not
        // change the count.
        let joined = r"
            async fn stats(repo: PgBookmarkRepository) -> AutumnResult<usize> {
                let rows = repo.count_grouped_by_tag().order_by_aggregate_desc().limit(5).load().await?;
                Ok(rows.len())
            }
            ";
        let split = r"
            async fn stats(repo: PgBookmarkRepository) -> AutumnResult<usize> {
                let q = repo.count_grouped_by_tag().order_by_aggregate_desc();
                let rows = q.limit(5).load().await?;
                Ok(rows.len())
            }
            ";
        assert_clean("1", joined);
        assert_clean("1", split);
    }

    #[test]
    fn an_immediately_invoked_closure_is_seen_through() {
        // `#[cached]` expanding first wraps the body in `(|| async move {…})()`.
        // Rejecting that would blame a closure the user never wrote.
        assert_clean(
            "1",
            r"
            async fn index(repo: PgPostRepository) -> AutumnResult<usize> {
                (|| async move { Ok(repo.find_all().await?.len()) })().await
            }
            ",
        );
    }

    #[test]
    fn a_template_that_merely_names_the_handle_is_free() {
        // Passing `&repo` to a render helper from inside `html!` is ordinary
        // style; only an awaited macro body can be hiding a query.
        assert_clean(
            "1",
            r#"
            async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
                let posts = repo.find_all().await?;
                tracing::debug!(count = posts.len(), handle = ?repo, "loaded");
                Ok(html! { @for p in &posts { (render_row(p, &repo)) } })
            }
            "#,
        );
    }

    #[test]
    fn an_at_most_once_combinator_closure_is_not_per_element() {
        assert_clean(
            "1",
            r"
            async fn show(repo: PgPostRepository, cached: Option<Vec<Post>>) -> AutumnResult<usize> {
                let rows = cached.unwrap_or_else(|| repo.find_all_blocking());
                Ok(rows.len())
            }
            ",
        );
    }

    #[test]
    fn an_extractor_generic_is_not_a_handle_by_name_suffix() {
        // `Form<NewRepo>` is a form, not a database handle.
        assert_clean(
            "0",
            r"
            async fn create(form: Form<NewRepo>) -> AutumnResult<Markup> {
                Ok(render(&form))
            }
            ",
        );
    }

    #[test]
    fn a_query_cost_on_the_loop_statement_bounds_it() {
        // The documented way to bound a loop the analysis cannot size.
        assert_clean(
            "10",
            r"
            async fn refresh(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<()> {
                #[query_cost(10)]
                for id in ids {
                    let _ = repo.find_by_id(id).await?;
                }
                Ok(())
            }
            ",
        );
    }

    #[test]
    fn a_loop_diagnostic_never_names_the_culprit_twice() {
        let err = error_of(
            "1",
            r"
            async fn index(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<()> {
                for id in ids { let _ = repo.find_by_id(id).await?; }
                Ok(())
            }
            ",
        )
        .expect("over-budget loop is rejected");
        assert!(
            !err.contains("a database query (a database query)"),
            "culprit is named twice: {err}"
        );
        assert!(err.contains("find_by_id"), "culprit is not named: {err}");
    }

    #[test]
    fn diagnostics_point_at_the_guide() {
        let err = error_of(
            "0",
            r"
            async fn index(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<()> {
                for id in ids { let _ = repo.find_by_id(id).await?; }
                Ok(())
            }
            ",
        )
        .expect("over-budget loop is rejected");
        assert!(
            err.contains("docs/guide/query-budgets.md"),
            "diagnostic does not link the guide: {err}"
        );
    }

    // ── Annotation hygiene ───────────────────────────────────────────

    #[test]
    fn match_arm_annotations_are_read_and_stripped() {
        let handler = r"
            async fn show(repo: PgPostRepository, k: Kind) -> AutumnResult<usize> {
                match k {
                    #[query_cost(3)]
                    Kind::A => repo.find_all().await?.len(),
                    Kind::B => repo.count().await? as usize,
                }
            }
            ";
        assert_error_contains("1", handler, &["3"]);
        let out = emitted_item("3", handler);
        assert!(
            !out.contains("query_cost"),
            "match-arm annotation leaked to rustc: {out}"
        );
    }

    #[test]
    fn query_exempt_without_a_reason_is_an_error() {
        assert_error_contains(
            "1",
            r"
            async fn show(mut db: Db) -> AutumnResult<usize> {
                #[query_exempt]
                let extra = helper(&mut db).await?;
                Ok(extra)
            }
            ",
            &["reason"],
        );
    }

    #[test]
    fn contradictory_annotations_on_one_statement_are_an_error() {
        assert_error_contains(
            "5",
            r#"
            async fn show(mut db: Db) -> AutumnResult<usize> {
                #[query_cost(1)]
                #[query_exempt(reason = "also this")]
                let extra = helper(&mut db).await?;
                Ok(extra)
            }
            "#,
            &["more than one query annotation"],
        );
    }

    #[test]
    fn a_stray_annotation_on_the_handler_gets_our_own_diagnostic() {
        let item = r"
            #[query_cost(1)]
            async fn show(mut db: Db) -> AutumnResult<usize> { Ok(0) }
            ";
        assert_error_contains("1", item, &["not the handler itself"]);
        let out = emitted_item("1", item);
        assert!(
            !out.contains("query_cost"),
            "stray annotation leaked to rustc: {out}"
        );
    }

    #[test]
    fn a_bad_budget_argument_still_strips_body_annotations() {
        // Otherwise one typo yields our diagnostic plus an "unknown attribute"
        // error per annotation in the body.
        let out = emitted_item(
            "bogus",
            r#"
            async fn show(mut db: Db) -> AutumnResult<usize> {
                #[query_exempt(reason = "checked")]
                let x = helper(&mut db);
                Ok(0)
            }
            "#,
        );
        assert!(
            !out.contains("query_exempt"),
            "annotations survived a bad budget argument: {out}"
        );
    }

    #[test]
    fn an_out_of_range_budget_names_the_attribute() {
        assert_error_contains("99999999999999999999", "async fn f() {}", &["query_budget"]);
        assert_error_contains("-1", "async fn f() {}", &["query_budget"]);
    }

    #[test]
    fn a_cfg_gated_handler_carries_its_cfg_onto_the_marker() {
        let out = expand(
            "0",
            r#"
            #[cfg(feature = "reports")]
            async fn report() -> usize { 0 }
            "#,
        );
        let marker = out
            .find("__AUTUMN_QUERY_BUDGET_report")
            .expect("marker emitted");
        assert!(
            out[..marker].contains("cfg (feature = \"reports\")"),
            "marker is not cfg-gated with its handler: {out}"
        );
    }

    #[test]
    fn a_raw_identifier_handler_records_its_plain_name() {
        let out = expand("0", "async fn r#type() -> usize { 0 }");
        assert!(
            out.contains(r#""type""#),
            "raw-identifier prefix leaked into the record: {out}"
        );
    }

    #[test]
    fn an_unrecognised_expression_naming_the_handle_is_reported() {
        // The catch-all must not be fail-open: soundness cannot depend on which
        // `syn` version parsed the body.
        assert_error_contains(
            "5",
            r"
            async fn show(mut db: Db) -> AutumnResult<usize> {
                let x = const { helper(&mut db) };
                Ok(0)
            }
            ",
            &["helper"],
        );
    }

    // ── Seeded N+1 corpus (the issue's success metric) ───────────────

    /// Handlers seeded with a known N+1, one per shape the bug takes in real
    /// code. Every one must be flagged at build time; the issue's bar is 95%
    /// with zero false negatives.
    const SEEDED_N_PLUS_ONE: &[(&str, &str)] = &[
        (
            "for over a vec",
            r"async fn h(repo: PgPostRepository) -> R {
                let posts = repo.find_all().await?;
                for p in posts { let _ = repo.find_author(p.author_id).await?; }
                Ok(())
            }",
        ),
        (
            "for over a slice reference",
            r"async fn h(mut db: Db, posts: Vec<Post>) -> R {
                for p in &posts {
                    let _ = users::table.filter(users::id.eq(p.author_id)).first(&mut *db).await?;
                }
                Ok(())
            }",
        ),
        (
            "while loop",
            r"async fn h(mut db: Db) -> R {
                while has_more() { let _ = posts::table.load(&mut *db).await?; }
                Ok(())
            }",
        ),
        (
            "bare loop",
            r"async fn h(repo: PgPostRepository) -> R {
                loop { let _ = repo.find_all().await?; }
            }",
        ),
        (
            "while let over a worklist",
            r"async fn h(repo: PgPostRepository, mut stack: Vec<i64>) -> R {
                while let Some(id) = stack.pop() { let _ = repo.find_by_id(id).await?; }
                Ok(())
            }",
        ),
        (
            "map closure building futures",
            r"async fn h(repo: PgPostRepository, posts: Vec<Post>) -> R {
                let futs: Vec<_> = posts.iter().map(|p| repo.find_by_id(p.author_id)).collect();
                Ok(futs.len())
            }",
        ),
        (
            "join_all over a map closure",
            r"async fn h(repo: PgPostRepository, posts: Vec<Post>) -> R {
                let rows = join_all(posts.iter().map(|p| repo.find_by_id(p.author_id))).await;
                Ok(rows.len())
            }",
        ),
        (
            "for_each closure",
            r"async fn h(mut db: Db, posts: Vec<Post>) -> R {
                posts.iter().for_each(|p| { let _ = posts::table.first(&mut *db); });
                Ok(())
            }",
        ),
        (
            "filter_map closure",
            r"async fn h(repo: PgPostRepository, posts: Vec<Post>) -> R {
                let rows: Vec<_> = posts.iter().filter_map(|p| repo.find_by_id(p.id).ok()).collect();
                Ok(rows.len())
            }",
        ),
        (
            "nested loops",
            r"async fn h(repo: PgPostRepository, groups: Vec<Vec<i64>>) -> R {
                for g in groups { for id in g { let _ = repo.find_by_id(id).await?; } }
                Ok(())
            }",
        ),
        (
            "loop inside a branch",
            r"async fn h(repo: PgPostRepository, flag: bool, ids: Vec<i64>) -> R {
                if flag { for id in ids { let _ = repo.find_by_id(id).await?; } }
                Ok(())
            }",
        ),
        (
            "loop inside a match arm",
            r"async fn h(repo: PgPostRepository, kind: Kind, ids: Vec<i64>) -> R {
                match kind {
                    Kind::One => { for id in ids { let _ = repo.find_by_id(id).await?; } }
                    Kind::Two => {}
                }
                Ok(())
            }",
        ),
        (
            "loop nested inside a closure",
            r"async fn h(repo: PgPostRepository, ids: Vec<i64>) -> R {
                let f = || { for id in ids { let _ = repo.find_by_id(id); } };
                Ok(())
            }",
        ),
        (
            "loop over an enumerate adapter",
            r"async fn h(repo: PgPostRepository, posts: Vec<Post>) -> R {
                for (i, p) in posts.iter().enumerate() { let _ = repo.find_by_id(p.id).await?; }
                Ok(())
            }",
        ),
        (
            "loop over a function result",
            r"async fn h(repo: PgPostRepository) -> R {
                for id in ids_to_refresh() { let _ = repo.find_by_id(id).await?; }
                Ok(())
            }",
        ),
        (
            "opaque helper handed the handle",
            r"async fn h(mut db: Db) -> R {
                let links = load_links(&mut db, 1).await?;
                Ok(links.len())
            }",
        ),
        (
            "opaque helper called in a loop",
            r"async fn h(mut db: Db, ids: Vec<i64>) -> R {
                for id in ids { let _ = load_links(&mut db, id).await?; }
                Ok(())
            }",
        ),
        (
            "query hidden in a macro body",
            r"async fn h(mut db: Db) -> R {
                Ok(html! { div { (fetch_title(&mut db).await?) } })
            }",
        ),
        (
            "query in a loop inside a nested block",
            r"async fn h(repo: PgPostRepository, ids: Vec<i64>) -> R {
                { { for id in ids { let _ = repo.find_by_id(id).await?; } } }
                Ok(())
            }",
        ),
        (
            "try_for_each closure",
            r"async fn h(repo: PgPostRepository, posts: Vec<Post>) -> R {
                posts.iter().try_for_each(|p| repo.find_by_id(p.id))?;
                Ok(())
            }",
        ),
        (
            "query in the iterator expression of an inner loop",
            r"async fn h(repo: PgPostRepository, ids: Vec<i64>) -> R {
                for id in ids {
                    for row in repo.find_children(id).await? { let _ = row; }
                }
                Ok(())
            }",
        ),
        (
            "query in a while-loop condition",
            r"async fn h(repo: PgJobRepository) -> R {
                while let Some(job) = repo.next_pending().await? { handle(job); }
                Ok(())
            }",
        ),
        (
            "query behind a repository field on self",
            r"async fn h(&self, ids: Vec<i64>) -> R {
                for id in ids { let _ = self.repo.find_by_id(id).await?; }
                Ok(())
            }",
        ),
        (
            "query behind a conventional accessor",
            r"async fn h(app: AppState, ids: Vec<i64>) -> R {
                for id in ids { let _ = app.db().find_by_id(id).await?; }
                Ok(())
            }",
        ),
        (
            "keyset batch walker",
            r"async fn h(repo: PgPostRepository) -> R {
                let mut b = repo.find_in_batches(1000);
                while let Some(c) = b.next_batch().await? { write(c); }
                Ok(())
            }",
        ),
        (
            "handle laundered through a tuple binding",
            r"async fn h(handle: Db, ids: Vec<i64>) -> R {
                let (conn, _) = (handle, 1);
                for id in ids { let _ = posts::table.find(id).first(&mut *conn).await?; }
                Ok(())
            }",
        ),
        (
            "handle wrapped in a context struct",
            r"async fn h(mut db: Db) -> R { Ok(load_all(Ctx { db: &mut db }).await?) }",
        ),
        (
            "helper handed the transaction connection",
            r"async fn h(mut db: Db) -> R {
                db.tx(move |conn| async move { write_audit(conn).await?; Ok(()) }.scope_boxed()).await?;
                Ok(())
            }",
        ),
        (
            "model static finder in a loop",
            r"async fn h(mut db: Db, ids: Vec<i64>) -> R {
                for id in ids { let _ = Post::find(id, &mut db).await?; }
                Ok(())
            }",
        ),
        (
            "preload spec passed as an opaque variable",
            r"async fn h(repo: PgPostRepository, spec: Spec) -> R {
                let posts = repo.find_all().await?;
                let posts = repo.preload(posts, spec).await?;
                Ok(posts.len())
            }",
        ),
    ];

    /// Handlers that are genuinely within budget. None may be flagged — a false
    /// positive here is what pushes developers to blanket-`unbounded` the app.
    const CLEAN_CORPUS: &[(&str, &str, &str)] = &[
        (
            "single finder",
            "1",
            r"async fn h(repo: PgPostRepository) -> R { Ok(repo.find_all().await?.len()) }",
        ),
        (
            "finder plus one batched association",
            "2",
            r"async fn h(repo: PgPostRepository) -> R {
                let posts = repo.find_all().await?;
                let posts = repo.preload(posts, Post::preload().author()).await?;
                Ok(posts.len())
            }",
        ),
        (
            "loop that issues nothing",
            "1",
            r"async fn h(repo: PgPostRepository) -> R {
                let posts = repo.find_all().await?;
                let mut n = 0;
                for p in &posts { n += p.title.len(); }
                Ok(n)
            }",
        ),
        (
            "branches take the worst arm",
            "1",
            r"async fn h(repo: PgPostRepository, flag: bool) -> R {
                if flag { Ok(repo.find_all().await?.len()) } else { Ok(repo.count().await? as usize) }
            }",
        ),
        (
            "literal loop bound within budget",
            "3",
            r"async fn h(repo: PgPostRepository) -> R {
                let mut n = 0;
                for _ in 0..3 { n += repo.find_all().await?.len(); }
                Ok(n)
            }",
        ),
        (
            "transaction body counted once",
            "3",
            r"async fn h(mut db: Db) -> R {
                db.transaction(|conn| async move {
                    let _ = posts::table.load(&mut *conn).await?;
                    let _ = tags::table.load(&mut *conn).await?;
                    Ok(())
                }).await?;
                Ok(0)
            }",
        ),
        (
            "closure with no query",
            "1",
            r"async fn h(repo: PgPostRepository) -> R {
                let posts = repo.find_all().await?;
                let titles: Vec<_> = posts.iter().map(|p| p.title.clone()).collect();
                Ok(titles.len())
            }",
        ),
        (
            "builder chain is one query",
            "1",
            r"async fn h(repo: PgPostRepository) -> R {
                Ok(repo.on_primary().scoped().find_all().await?.len())
            }",
        ),
        (
            "raw diesel executor",
            "1",
            r"async fn h(mut db: Db) -> R {
                let posts = posts::table.select(Post::as_select()).load(&mut *db).await?;
                Ok(posts.len())
            }",
        ),
        (
            "no database at all",
            "0",
            r"async fn h() -> R { Ok(render_static()) }",
        ),
        (
            "macro body that never names the handle",
            "1",
            r#"async fn h(mut db: Db) -> R {
                let posts = posts::table.load(&mut *db).await?;
                Ok(html! { div { "hello" } })
            }"#,
        ),
        (
            "dropping the handle mid-handler",
            "1",
            r"async fn h(mut db: Db) -> R {
                let posts = posts::table.load(&mut *db).await?;
                drop(db);
                Ok(posts.len())
            }",
        ),
    ];

    /// Handler shapes taken from the shipped example apps. These are the code
    /// the framework's own docs teach, so a rejection here is the failure mode
    /// that makes teams blanket-`unbounded` an app.
    const EXAMPLE_APP_CORPUS: &[(&str, &str, &str)] = &[
        (
            "wiki: transactional create (db.tx + scope_boxed)",
            "3",
            r"async fn create(mut db: Db, form: Form<NewCollection>) -> R {
                let id = db.tx(move |conn| {
                    async move {
                        let created = diesel::insert_into(collections::table)
                            .values(&form.0)
                            .returning(Collection::as_returning())
                            .get_result(conn)
                            .await?;
                        diesel::insert_into(links::table).values(&rows).execute(conn).await?;
                        Ok(created.id)
                    }
                    .scope_boxed()
                })
                .await?;
                Ok(id)
            }",
        ),
        (
            "blog: model static finders",
            "1",
            r"async fn index(mut db: Db) -> R { Ok(Post::published(&mut db).await?.len()) }",
        ),
        (
            "todo-app: paginated model finder",
            "1",
            r"async fn list(page: PageRequest, mut db: Db) -> R {
                Ok(Todo::page(&page, &mut db).await?.len())
            }",
        ),
        (
            "bookmarks: grouped aggregate builder chain",
            "1",
            r"async fn stats(repo: PgBookmarkRepository) -> R {
                Ok(repo.count_grouped_by_tag().order_by_aggregate_desc().limit(5).load().await?.len())
            }",
        ),
        (
            "reddit: repository preload with two associations",
            "3",
            r"async fn front(repo: PgPostRepository) -> R {
                let hot = repo.hot_posts(20).await?;
                let hot = repo.on_primary().preload(hot, Post::preload().author().subreddit()).await?;
                Ok(hot.len())
            }",
        ),
        (
            "reddit: template naming the repository handle",
            "1",
            r#"async fn front(repo: PgPostRepository) -> R {
                let posts = repo.find_all().await?;
                tracing::debug!(?repo, "rendered");
                Ok(html! { @for p in &posts { (render_row(p, &repo)) } })
            }"#,
        ),
        (
            "any app: drop the handle mid-handler, then render",
            "1",
            r"async fn show(mut db: Db) -> R {
                let posts = posts::table.load(&mut *db).await?;
                drop(db);
                Ok(html! { div { (posts.len()) } })
            }",
        ),
    ];

    #[test]
    fn example_app_handler_shapes_are_not_false_positives() {
        let flagged: Vec<(&str, String)> = EXAMPLE_APP_CORPUS
            .iter()
            .filter_map(|(name, budget, handler)| error_of(budget, handler).map(|err| (*name, err)))
            .collect();

        assert!(
            flagged.is_empty(),
            "handler shapes the example apps actually use were rejected: {flagged:#?}"
        );
    }

    #[test]
    fn seeded_n_plus_one_corpus_is_caught_at_build_time() {
        // Deliberately generous: the budget is high enough that only an
        // *unbounded* path can trip it, so every catch here is the analysis
        // recognising unbounded growth rather than arithmetic overrun.
        let missed: Vec<&str> = SEEDED_N_PLUS_ONE
            .iter()
            .filter(|(_, handler)| error_of("50", handler).is_none())
            .map(|(name, _)| *name)
            .collect();

        assert!(
            missed.is_empty(),
            "{} of {} seeded N+1 handlers compiled clean (false negatives): {missed:?}",
            missed.len(),
            SEEDED_N_PLUS_ONE.len()
        );
    }

    #[test]
    fn every_seeded_diagnostic_names_the_offending_call_site() {
        // AC2 asks for more than a rejection: the developer must be told which
        // call is the problem.
        let anonymous: Vec<&str> = SEEDED_N_PLUS_ONE
            .iter()
            .filter(|(_, handler)| {
                let Some(err) = error_of("50", handler) else {
                    return true;
                };
                // Every diagnostic quotes the offending call, association, or
                // expression form in backticks alongside the fix.
                !err.contains('`') || !err.contains("docs/guide/query-budgets.md")
            })
            .map(|(name, _)| *name)
            .collect();

        assert!(
            anonymous.is_empty(),
            "diagnostics that name no call site or omit the guide link: {anonymous:?}"
        );
    }

    #[test]
    fn clean_corpus_has_no_false_positives() {
        let flagged: Vec<(&str, String)> = CLEAN_CORPUS
            .iter()
            .filter_map(|(name, budget, handler)| error_of(budget, handler).map(|err| (*name, err)))
            .collect();

        assert!(
            flagged.is_empty(),
            "in-budget handlers were rejected: {flagged:#?}"
        );
    }

    #[test]
    fn empty_attribute_is_an_error() {
        assert_error_contains("", "async fn f() {}", &["query_budget"]);
    }

    #[test]
    fn non_numeric_attribute_is_an_error() {
        assert_error_contains("\"three\"", "async fn f() {}", &["query_budget"]);
    }

    #[test]
    fn unknown_keyword_attribute_is_an_error() {
        assert_error_contains("infinite", "async fn f() {}", &["unbounded"]);
    }

    #[test]
    fn attribute_on_a_non_function_is_an_error() {
        assert_error_contains("1", "struct S;", &["function"]);
    }
}

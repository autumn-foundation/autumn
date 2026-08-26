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
use quote::{format_ident, quote};
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
];

/// Methods that run their closure **exactly once**, so a query inside is a
/// fixed cost rather than a per-element one.
const TRANSACTION_METHODS: &[&str] = &[
    "transaction",
    "transaction_with_retry",
    "transaction_with_isolation",
    "immediate_transaction",
    "read_only_transaction",
    "with_transaction",
];

/// Free functions that may receive the handle without querying through it.
const SAFE_FREE_FNS: &[&str] = &["drop"];

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
const BATCH_HINT: &str = "Batch the per-row lookup into one query with `preload(...)`, declare \
                          the call site with `#[query_cost(N)]`, exempt it with \
                          `#[query_exempt(reason = ...)]`, or opt the handler out with \
                          `#[query_budget(unbounded, reason = ...)]`.";

/// Offered when the fix is to state a cost the analysis cannot see.
const DECLARE_HINT: &str = "Declare the statement's cost with `#[query_cost(N)]`, or exempt it \
                            with `#[query_exempt(reason = ...)]` once you have checked it issues \
                            nothing.";

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
            Budget::Bounded(lit.base10_parse::<u32>()?)
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
        let mut cost = Cost::ZERO;
        for stmt in &block.stmts {
            cost = cost.then(self.stmt(stmt));
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

        match self.annotation(attrs) {
            Some(Annotation::Cost(n)) => return Cost::Exact(n),
            Some(Annotation::Exempt) => return Cost::ZERO,
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
            // A binding initialised from a handle (or from a handle-rooted
            // chain) is itself a handle from here on, so passing it into an
            // opaque call is still caught.
            if self.expr_is_handle(&init.expr) {
                collect_pat_idents(&local.pat, &mut self.handles);
            }
        }
        cost
    }

    /// Read a `#[query_cost(N)]` / `#[query_exempt(...)]` statement annotation.
    fn annotation(&mut self, attrs: &[Attribute]) -> Option<Annotation> {
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
            Expr::Call(call) => self.call(call),
            Expr::Macro(m) => self.mac(&m.mac),
            Expr::Closure(closure) => {
                let body = self.expr(&closure.body);
                if body.is_zero() {
                    Cost::ZERO
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
                let before = self.ledger.len();
                let body = self.block(&f.body);
                iter.then(self.bound_loop(body, const_bound(&f.expr), f.span(), before))
            }
            Expr::While(w) => {
                let cond = self.expr(&w.cond);
                let before = self.ledger.len();
                let body = self.block(&w.body);
                cond.then(self.bound_loop(body, None, w.span(), before))
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
                let mut worst = Cost::ZERO;
                for arm in &m.arms {
                    let mut arm_cost = arm
                        .guard
                        .as_ref()
                        .map_or(Cost::ZERO, |(_, guard)| self.expr(guard));
                    arm_cost = arm_cost.then(self.expr(&arm.body));
                    worst = worst.or_worst(arm_cost);
                }
                cost = cost.then(worst);
                cost
            }

            Expr::Block(syn::ExprBlock { block, .. })
            | Expr::Async(syn::ExprAsync { block, .. })
            | Expr::Unsafe(syn::ExprUnsafe { block, .. })
            | Expr::TryBlock(syn::ExprTryBlock { block, .. }) => self.block(block),

            Expr::Array(a) => self.each(a.elems.iter()),
            Expr::Tuple(t) => self.each(t.elems.iter()),
            Expr::Assign(a) => self.expr(&a.left).then(self.expr(&a.right)),
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

            // `Lit`, `Path`, `Const`, `Infer`, `Continue`, `Verbatim`, and any
            // future variant carry no reachable call.
            _ => Cost::ZERO,
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
        let culprit = self
            .ledger
            .get(ledger_before)
            .cloned()
            .unwrap_or_else(|| "a database query".to_string());
        Cost::unbounded(
            span,
            format!(
                "a database query ({culprit}) runs inside a loop, so this handler's query count \
                 grows with the size of the collection — the classic N+1"
            ),
            BATCH_HINT,
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
        let root_is_handle = self.expr_is_handle(root);

        // Arguments run regardless of what the chain does with them.
        for method in &methods {
            cost = cost.then(self.method_args(method));
        }
        if matches!(cost, Cost::Unbounded(_)) {
            return cost;
        }

        if root_is_handle {
            if let Some(preload) = methods.iter().find(|m| m.method == "preload") {
                return cost.then(self.preload_cost(preload));
            }
            // The chain is counted where it is *built*, not where it is
            // awaited: `let fut = repo.find_all();` still costs a query, and
            // collecting such futures to `join_all` later is still an N+1.
            let last = methods
                .last()
                .map_or_else(|| "query".to_string(), |m| m.method.to_string());
            if HANDLE_BUILDERS.contains(&last.as_str()) {
                // `repo.on_primary()` refines the next query; it is not one.
                return cost;
            }
            return cost.then(self.count(&last));
        }

        // Not rooted at a handle: a diesel executor call is the round trip.
        for method in &methods {
            let is_executor = EXECUTORS.contains(&method.method.to_string().as_str());
            let takes_handle = method.args.iter().any(|a| self.expr_is_handle(a));
            if is_executor && (awaited || takes_handle) {
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
        let runs_once = TRANSACTION_METHODS.contains(&method.method.to_string().as_str());
        let mut cost = Cost::ZERO;
        for arg in &method.args {
            if runs_once && let Expr::Closure(closure) = arg {
                // The callback runs exactly once, and its parameter is the
                // transaction's own connection — a handle.
                for input in &closure.inputs {
                    collect_pat_idents(input, &mut self.handles);
                }
                cost = cost.then(self.expr(&closure.body));
                continue;
            }
            cost = cost.then(self.expr(arg));
        }
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

        let mut cost = self.expr(&call.func);
        for arg in &call.args {
            cost = cost.then(self.expr(arg));
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

        if call.args.iter().any(|a| self.expr_is_handle(a)) {
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
        Cost::unbounded(
            mac.span(),
            format!(
                "the `{name}!` macro body names the database handle, and a macro body is opaque \
                 token soup to the analysis"
            ),
            "Move the query out of the macro and into a statement the analysis can read, declare \
             the statement with `#[query_cost(N)]`, or exempt it with \
             `#[query_exempt(reason = ...)]`.",
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
            Expr::Unary(u) => matches!(u.op, syn::UnOp::Deref(_)) && self.expr_is_handle(&u.expr),
            Expr::MethodCall(mc) => {
                HANDLE_BUILDERS.contains(&mc.method.to_string().as_str())
                    && self.expr_is_handle(&mc.receiver)
            }
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
                || name.ends_with("Repo")
            {
                return true;
            }
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                return args.args.iter().any(|arg| match arg {
                    syn::GenericArgument::Type(inner) => type_is_handle(inner),
                    _ => false,
                });
            }
            false
        }
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

pub fn query_budget_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let budget = match syn::parse2::<BudgetAttr>(attr) {
        Ok(parsed) => parsed.budget,
        Err(err) => {
            let err = err.to_compile_error();
            return quote! { #item #err };
        }
    };

    // Keep the original tokens so a parse failure still emits the item — one
    // purpose-written diagnostic beats a cascade of "cannot find" errors.
    let original = item.clone();
    let Ok(mut input_fn) = syn::parse2::<ItemFn>(item) else {
        let err = syn::Error::new(
            Span::call_site(),
            "`#[query_budget(...)]` can only be applied to a function — put it on the route \
             handler whose queries you want bounded",
        )
        .to_compile_error();
        return quote! { #original #err };
    };

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

    let marker_const = if takes_self {
        TokenStream::new()
    } else {
        quote! {
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
    use super::*;

    /// Expand `#[query_budget(attr)]` over `item` and return the generated code
    /// as a string.
    fn expand(attr: &str, item: &str) -> String {
        let attr: TokenStream = attr.parse().expect("attr parses");
        let item: TokenStream = item.parse().expect("item parses");
        query_budget_macro(attr, item).to_string()
    }

    /// The `compile_error!` message the expansion emitted, if any.
    fn error_of(attr: &str, item: &str) -> Option<String> {
        let out = expand(attr, item);
        let idx = out.find("compile_error !")?;
        let rest = &out[idx..];
        let start = rest.find('"')? + 1;
        let end = rest[start..].find("\" }")? + start;
        Some(rest[start..end].to_string())
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
        let out = expand(
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

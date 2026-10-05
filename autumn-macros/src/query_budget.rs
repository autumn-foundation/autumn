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
//! * a path that leaves early (`return`, `break`, `continue`) does **not**
//!   include the cost of the code it skips ([`Flow`]),
//! * a loop whose body issues a query is **unbounded** unless the iterable has
//!   a literal, compile-time bound — this is the classic N+1,
//! * anything the analysis cannot read (a helper or associated function handed
//!   the handle, a macro body mentioning it, a closure that may run per
//!   element) is **reported**, never silently skipped.
//!
//! Three escape hatches keep legitimately dynamic code compiling:
//! `#[query_budget(unbounded, reason = ...)]` on the handler, and
//! `#[query_cost(N)]` / `#[query_exempt(reason = ...)]` on a statement.
//!
//! A scoped [`Env`] follows each handle through every binding, branch, exit
//! and container ([`Kind`]).
//!
//! See `docs/guide/query-budgets.md` for the user-facing guide.

use std::collections::HashMap;
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
    "json",
];

/// Methods that run their closure **exactly once**, so a query inside is a
/// fixed cost rather than a per-element one. `Db::tx` / `Db::tx_with` are
/// autumn's transaction API (`autumn/src/db.rs`); `transaction` is
/// diesel-async's own.
const TRANSACTION_METHODS: &[&str] = &["tx", "tx_with", "tx_immediate", "transaction"];

/// Free functions whose closure likewise runs exactly once, and which take the
/// connection as their first argument (`autumn/src/db.rs`).
const TRANSACTION_FREE_FNS: &[&str] = &[
    "scoped_transaction",
    "scoped_immediate_transaction",
    "maybe_immediate_transaction",
    "savepoint",
];

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

/// Methods that turn a known `LazyDb` into a live `Db` without a query:
/// `LazyDb::checkout` (autumn/src/db.rs, #2264). The call costs nothing and
/// its result is a handle.
///
/// Not in `HANDLE_ACCESSORS`, which matches a name on any receiver:
/// "checkout" is also a domain verb (`autumn-billing`'s
/// `self.checkout(&snapshot)`). It applies only to a receiver known to be a
/// `LazyDb` (`expr_is_lazy_db`).
const HANDLE_TRANSITIONS: &[&str] = &["checkout"];

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
    "LazyDb",
    "ShardedDb",
    "ShardedReadDb",
    "TestDb",
    "AsyncPgConnection",
    "AsyncConnection",
    "PgConnection",
    "SqliteConnection",
    "PooledConnection",
];

/// Wrappers to look inside for a `LazyDb` parameter (`Result<LazyDb, E>`). An
/// allowlist: an unknown wrapper (`Cart<LazyDb>`) may have its own domain
/// `checkout()` (Codex review, PR #2762, round 7).
const LAZY_DB_WRAPPERS: &[&str] = &["Result", "Option", "Arc", "Rc", "Box", "Extension", "State"];

/// Constructors that build a carrier and run no code.
const CONTAINER_CONSTRUCTORS: &[&str] = &["Some", "Ok", "Err"];

/// Methods that call a closure argument. A function given by path in its
/// place is opaque.
const CALLBACK_METHODS: &[&str] = &[
    "map",
    "map_or",
    "map_or_else",
    "and_then",
    "for_each",
    "try_for_each",
    "filter",
    "filter_map",
    "flat_map",
    "find",
    "find_map",
    "fold",
    "try_fold",
    "any",
    "all",
    "inspect",
    "position",
    "scan",
    "take_while",
    "skip_while",
    "then",
    "unwrap_or_else",
    "ok_or_else",
    "get_or_insert_with",
];

/// Smart pointers. They deref to what they hold, so `Arc<PgPostRepository>`
/// is a handle.
const SMART_POINTERS: &[&str] = &["Box", "Arc", "Rc"];

/// Types that hold values without being them. A `Vec` or `Option` of a handle
/// type (`Vec<Db>`, `Vec<PgPostRepository>`) is a carrier: a method on it is
/// not a query, and its parts are handles.
const CARRIER_TYPES: &[&str] = &["Vec", "VecDeque", "Option"];

/// Methods on a carrier that return nothing, a number or a `bool`: not a
/// part of it.
const SCALAR_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "is_some_and",
    "is_none_or",
    "is_ok_and",
    "is_err_and",
    "contains",
    "count",
    "any",
    "all",
    "position",
    "for_each",
    "try_for_each",
    "push",
    "push_back",
    "push_front",
    "extend",
    "append",
    "clear",
    "truncate",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "sort_unstable_by",
    "sort_unstable_by_key",
    "dedup",
    "reverse",
    "retain",
    "swap",
    "resize",
    "reserve",
    "shrink_to_fit",
];

/// Methods on a carrier that return a carrier: a view, an iterator, or an
/// `Option` of a part.
const CARRIER_METHODS: &[&str] = &[
    "iter",
    "iter_mut",
    "into_iter",
    "as_ref",
    "as_mut",
    "as_slice",
    "clone",
    "cloned",
    "copied",
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "and_then",
    "or",
    "or_else",
    "enumerate",
    "rev",
    "skip",
    "take",
    "replace",
    "chain",
    "zip",
    "peekable",
    "collect",
    "by_ref",
    "first",
    "last",
    "get",
    "get_mut",
    "pop",
    "next",
    "nth",
    "find",
    "find_map",
    "inspect",
    "step_by",
    "chunks",
    "windows",
    "skip_while",
    "take_while",
    "ok",
    "ok_or",
    "ok_or_else",
    "as_deref",
    "as_deref_mut",
    "drain",
    "values",
    "values_mut",
];

/// Methods on a carrier that return a part of it, which is a handle.
const ELEMENT_METHODS: &[&str] = &[
    "remove",
    "swap_remove",
    "insert",
    "unwrap",
    "expect",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "unwrap_unchecked",
    "map_or",
    "map_or_else",
    "fold",
    "try_fold",
    "reduce",
    "max",
    "min",
    "max_by",
    "min_by",
    "max_by_key",
    "min_by_key",
];

/// Is `method` one of the known methods on a carrier? Any other method on a
/// carrier may run queries on its parts, so it is reported.
fn is_container_method(method: &str) -> bool {
    SCALAR_METHODS.contains(&method)
        || CARRIER_METHODS.contains(&method)
        || ELEMENT_METHODS.contains(&method)
}

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

/// The worse of two optional costs. `None` is "no path".
fn worst(a: Option<Cost>, b: Option<Cost>) -> Option<Cost> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.or_worst(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The cost of a construct, split by where each path goes next (#2316).
///
/// `None` means that no path goes there. A block folds its statements with
/// [`Flow::then`]: an exit costs the statements before it plus its own cost,
/// and only the paths that fall through pay for the next statement. So
/// `if cached { return repo.a().await; } repo.b().await` costs 1, not 2.
///
/// `?` needs no rule. Its exit costs no more than the path that falls
/// through, so the fall-through path always covers it.
struct Flow {
    /// Paths that reach the next statement.
    fall: Option<Cost>,
    /// Paths that leave by `break` or `continue`.
    brk: Option<Cost>,
    /// Paths that leave by `return`.
    ret: Option<Cost>,
}

impl Flow {
    const ZERO: Self = Self::cost(Cost::ZERO);
    const NEVER: Self = Self {
        fall: None,
        brk: None,
        ret: None,
    };
    const BREAK: Self = Self {
        fall: None,
        brk: Some(Cost::ZERO),
        ret: None,
    };
    const RETURN: Self = Self {
        fall: None,
        brk: None,
        ret: Some(Cost::ZERO),
    };

    const fn cost(cost: Cost) -> Self {
        Self {
            fall: Some(cost),
            brk: None,
            ret: None,
        }
    }

    /// `self`, then `next` on the paths that fall through.
    fn then(self, next: Self) -> Self {
        let Some(fall) = self.fall else {
            return self;
        };
        // A `next` with no path at all (`match never {}`) ends the run here.
        // The cost so far was still paid.
        if next.fall.is_none() && next.brk.is_none() && next.ret.is_none() {
            return Self {
                fall: None,
                brk: self.brk,
                ret: worst(self.ret, Some(fall)),
            };
        }
        let after = |cost: Option<Cost>| cost.map(|c| fall.clone().then(c));
        Self {
            brk: worst(self.brk, after(next.brk)),
            ret: worst(self.ret, after(next.ret)),
            fall: after(next.fall),
        }
    }

    /// Only one of `self` and `other` runs.
    fn or_worst(self, other: Self) -> Self {
        Self {
            fall: worst(self.fall, other.fall),
            brk: worst(self.brk, other.brk),
            ret: worst(self.ret, other.ret),
        }
    }

    /// The same paths, each costing `cost`.
    fn with_cost(self, cost: &Cost) -> Self {
        let set = |path: Option<Cost>| path.map(|_| cost.clone());
        Self {
            fall: set(self.fall),
            brk: set(self.brk),
            ret: set(self.ret),
        }
    }

    /// The worst path, wherever it goes.
    fn total(self) -> Cost {
        worst(worst(self.fall, self.brk), self.ret).unwrap_or(Cost::ZERO)
    }
}

// ── Binding environment ──────────────────────────────────────────────

/// What a name holds. The order is the join: after a branch, a name holds the
/// greatest kind it holds on any path. `LazyDb` is above `Handle` because all
/// paths share one type, so one `LazyDb` path makes the name a `LazyDb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// No database handle.
    Plain,
    /// A value that holds handles without being one: `[repo]`, `Some(repo)`,
    /// `Vec<PgPostRepository>`. A method on it is not a query. Its parts are
    /// handles (see `Analyzer::expr_is_carrier`).
    Carrier,
    /// A database handle.
    Handle,
    /// A `LazyDb`: a handle whose `checkout` is not a query.
    LazyDb,
}

impl Kind {
    /// What one part of a value of this kind holds.
    const fn element(self) -> Self {
        match self {
            Self::Plain => Self::Plain,
            Self::Carrier | Self::Handle => Self::Handle,
            Self::LazyDb => Self::LazyDb,
        }
    }

    const fn is_handle(self) -> bool {
        matches!(self, Self::Handle | Self::LazyDb)
    }
}

/// What a name holds, and, for a struct or tuple literal, what each part
/// holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    kind: Kind,
    /// Each part's name and kind, when the value was a struct or tuple
    /// literal. `None`: every part of a carrier is a handle.
    parts: Option<Vec<(String, Kind)>>,
    /// The value is a user struct, not a standard container, so none of its
    /// methods is a known container method.
    user: bool,
}

impl Binding {
    const fn of(kind: Kind) -> Self {
        Self {
            kind,
            parts: None,
            user: false,
        }
    }

    /// The binding that holds what `self` or `other` holds.
    fn join(&self, other: &Self) -> Self {
        let parts = match (&self.parts, &other.parts) {
            (Some(mine), Some(theirs))
                if mine.len() == theirs.len()
                    && mine.iter().zip(theirs).all(|(m, t)| m.0 == t.0) =>
            {
                Some(
                    mine.iter()
                        .zip(theirs)
                        .map(|(m, t)| (m.0.clone(), m.1.max(t.1)))
                        .collect(),
                )
            }
            (None, Some(theirs)) if self.kind == Kind::Plain => Some(theirs.clone()),
            (Some(mine), None) if other.kind == Kind::Plain => Some(mine.clone()),
            _ => None,
        };
        Self {
            kind: self.kind.max(other.kind),
            parts,
            user: self.user || other.user,
        }
    }
}

/// Lexical scopes, innermost last. Each scope maps a name to its [`Binding`].
#[derive(Clone, PartialEq, Eq)]
struct Env {
    scopes: Vec<HashMap<String, Binding>>,
}

impl Env {
    fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    const fn depth(&self) -> usize {
        self.scopes.len()
    }

    /// The binding of `name`, looking from scope `top` outwards.
    fn binding_from(&self, top: usize, name: &str) -> Binding {
        self.scopes[..=top]
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
            .unwrap_or(Binding::of(Kind::Plain))
    }

    /// The binding of `name` here.
    fn binding(&self, name: &str) -> Binding {
        self.binding_from(self.scopes.len() - 1, name)
    }

    /// What `name` holds here.
    fn get(&self, name: &str) -> Kind {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).map(|b| b.kind))
            .unwrap_or(Kind::Plain)
    }

    /// What part `member` of `name` holds, when the parts are known.
    fn part(&self, name: &str, member: &str) -> Option<Kind> {
        let binding = self.binding(name);
        if binding.kind.is_handle() {
            return None;
        }
        binding
            .parts?
            .into_iter()
            .find(|(m, _)| m == member)
            .map(|(_, k)| k)
    }

    /// Bind `name` in the innermost scope. This shadows an outer binding.
    fn declare(&mut self, name: String, binding: Binding) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, binding);
        }
    }

    /// The index of the scope that declared `name`. An undeclared name
    /// belongs to the root scope, so a write outlives every inner scope.
    fn home(&self, name: &str) -> usize {
        self.scopes
            .iter()
            .rposition(|scope| scope.contains_key(name))
            .unwrap_or(0)
    }

    /// Store into the scope that declared `name`.
    fn assign(&mut self, name: String, binding: Binding) {
        let at = self.home(&name);
        self.scopes[at].insert(name, binding);
    }

    /// Store `kind` into part `member` of `name`. An unknown part makes every
    /// part of the value a handle.
    fn assign_part(&mut self, name: &str, member: &str, kind: Kind) {
        let mut binding = self.binding(name);
        let known = binding
            .parts
            .as_mut()
            .and_then(|parts| parts.iter_mut().find(|(m, _)| m == member));
        match known {
            Some(part) => part.1 = kind,
            None if kind != Kind::Plain => binding.parts = None,
            None => {}
        }
        if kind != Kind::Plain {
            binding.kind = binding.kind.max(Kind::Carrier);
        }
        self.assign(name.to_string(), binding);
    }

    /// Join `other` into `self`: each name holds what it holds in either.
    /// Scopes deeper than the shallower of the two are ignored.
    fn join(&mut self, other: &Self) {
        for depth in 0..self.scopes.len().min(other.scopes.len()) {
            let names: Vec<String> = self.scopes[depth]
                .keys()
                .chain(other.scopes[depth].keys())
                .cloned()
                .collect();
            for name in names {
                let joined = self
                    .binding_from(depth, &name)
                    .join(&other.binding_from(depth, &name));
                self.scopes[depth].insert(name, joined);
            }
        }
    }

    /// Does `name` hold a handle or a carrier here?
    fn is_tracked(&self, name: &str) -> bool {
        self.get(name) != Kind::Plain
    }
}

/// What kind of exit lands in an [`ExitFrame`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A function, closure or async body. `return` and `?` land here, and
    /// `break` does not cross it.
    Body,
    /// A loop or a labeled block. `break` and `continue` land here.
    Break,
}

/// Where an exit lands, and the bindings at the exits that land there.
struct ExitFrame {
    target: Target,
    /// The scope depth when the frame opened.
    depth: usize,
    /// The join of the bindings at every exit that landed here.
    env: Option<Env>,
}

impl ExitFrame {
    fn record(&mut self, env: &Env) {
        let env = Env {
            scopes: env.scopes[..self.depth].to_vec(),
        };
        match &mut self.env {
            Some(joined) => joined.join(&env),
            None => self.env = Some(env),
        }
    }
}

// ── Analyzer ─────────────────────────────────────────────────────────

/// Walks a function body. It returns a [`Flow`], keeps an [`Env`] of what
/// each name holds, and keeps a ledger of every counted call site for the
/// diagnostic.
///
/// Every binding ends in [`Env::declare`] (`let`, a pattern, a parameter) or
/// [`Env::assign`] (`=`). Most scopes open and close in
/// [`Analyzer::scoped`]; an `if` opens its own, because its `if let` scope
/// covers only the condition and the then-branch. Every branch joins its
/// bindings, and every exit records its bindings where it lands.
struct Analyzer {
    env: Env,
    exits: Vec<ExitFrame>,
    /// Counted call sites, in source order, for the diagnostic.
    ledger: Vec<String>,
    /// Errors raised by malformed `#[query_cost]` / `#[query_exempt]`.
    errors: Vec<syn::Error>,
}

impl Analyzer {
    /// An analyzer with the handler's parameters bound.
    fn new(input_fn: &ItemFn) -> Self {
        let mut analyzer = Self {
            env: Env::new(),
            exits: Vec::new(),
            ledger: Vec::new(),
            errors: Vec::new(),
        };
        for arg in &input_fn.sig.inputs {
            if let syn::FnArg::Typed(typed) = arg {
                analyzer.bind_pat(&typed.pat, type_kind(&typed.ty));
            }
        }
        analyzer
    }

    /// The cost of the handler's body.
    fn function_body(&mut self, block: &Block) -> Cost {
        self.framed(Target::Body, |s| s.block(block).total())
    }

    fn count(&mut self, what: &str) -> Cost {
        self.ledger.push(format!("`{what}`"));
        Cost::Exact(1)
    }

    // ── Bindings ─────────────────────────────────────────────────────

    /// Bind the names in `pat` to the parts of a value that holds `kind`.
    fn bind_pat(&mut self, pat: &Pat, kind: Kind) {
        match pat {
            Pat::Ident(p) => {
                self.env.declare(p.ident.to_string(), Binding::of(kind));
                if let Some((_, sub)) = &p.subpat {
                    self.bind_pat(sub, kind);
                }
            }
            // rustc checks the annotation. A type made only of standard and
            // primitive types cannot hold a handle.
            Pat::Type(p) if type_is_plain_std(&p.ty) => self.bind_pat(&p.pat, Kind::Plain),
            Pat::Type(p) => self.bind_pat(&p.pat, kind.max(type_kind(&p.ty))),
            Pat::Reference(p) => self.bind_pat(&p.pat, kind),
            Pat::Paren(p) => self.bind_pat(&p.pat, kind),
            Pat::Guard(p) => self.bind_pat(&p.pat, kind),
            Pat::Or(p) => {
                for case in &p.cases {
                    self.bind_pat(case, kind);
                }
            }
            Pat::Tuple(p) => {
                for elem in &p.elems {
                    self.bind_pat(elem, kind.element());
                }
            }
            Pat::Slice(p) => {
                for elem in &p.elems {
                    self.bind_pat(elem, kind.element());
                }
            }
            Pat::TupleStruct(p) => {
                // A `Result` is a handle only when its `Ok` side is one, so
                // `Err(e)` on it binds the error. On a carrier, such as a
                // `Result<(), Db>`, `Err(e)` may be the handle.
                let is_err = p.path.segments.last().is_some_and(|s| s.ident == "Err");
                let inner = if is_err && kind.is_handle() {
                    Kind::Plain
                } else {
                    kind.element()
                };
                for elem in &p.elems {
                    self.bind_pat(elem, inner);
                }
            }
            Pat::Struct(p) => {
                for field in &p.fields {
                    self.bind_pat(&field.pat, kind.element());
                }
            }
            _ => {}
        }
    }

    /// Bind `pat` to `init`. A tuple or slice pattern over a literal binds
    /// part by part: `let (conn, key) = (db, id);`. A name bound to a struct
    /// or tuple literal records what each part holds.
    fn bind_init(&mut self, pat: &Pat, init: &Expr) {
        let pairs = match (pat, init) {
            (Pat::Tuple(p), Expr::Tuple(t)) => pair_parts(&p.elems, &t.elems),
            (Pat::Slice(p), Expr::Array(a)) => pair_parts(&p.elems, &a.elems),
            _ => None,
        };
        if let Some(pairs) = pairs {
            for (part, value) in pairs {
                self.bind_init(part, value);
            }
            return;
        }
        match (pat, init) {
            (Pat::Paren(p), _) => self.bind_init(&p.pat, init),
            (_, Expr::Paren(e)) => self.bind_init(pat, &e.expr),
            (Pat::Ident(p), _) if p.subpat.is_none() => {
                let binding = self.binding_of(init);
                self.env.declare(p.ident.to_string(), binding);
            }
            _ => {
                let kind = self.value_of(init);
                self.bind_pat(pat, kind);
            }
        }
    }

    /// What `init` holds, with its parts when it is a struct or tuple literal.
    fn binding_of(&self, init: &Expr) -> Binding {
        let kind = self.value_of(init);
        let parts = match init {
            Expr::Struct(st) if st.rest.is_none() => Some(
                st.fields
                    .iter()
                    .map(|f| (member_name(&f.member), self.value_of(&f.expr)))
                    .collect(),
            ),
            Expr::Tuple(t) => Some(
                t.elems
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (i.to_string(), self.value_of(e)))
                    .collect(),
            ),
            _ => None,
        };
        let user = kind == Kind::Carrier && matches!(init, Expr::Struct(_));
        Binding { kind, parts, user }
    }

    /// `place = value`. A tuple or array place over a literal of the same
    /// shape assigns part by part.
    fn assign(&mut self, place: &Expr, value: &Expr) {
        match (place, value) {
            (Expr::Tuple(p), Expr::Tuple(v)) if p.elems.len() == v.elems.len() => {
                for (part, value) in p.elems.iter().zip(&v.elems) {
                    self.assign(part, value);
                }
            }
            (Expr::Array(p), Expr::Array(v)) if p.elems.len() == v.elems.len() => {
                for (part, value) in p.elems.iter().zip(&v.elems) {
                    self.assign(part, value);
                }
            }
            (Expr::Paren(p), _) => self.assign(&p.expr, value),
            (Expr::Path(p), _) if p.path.get_ident().is_some() => {
                let binding = self.binding_of(value);
                if let Some(ident) = p.path.get_ident() {
                    self.env.assign(ident.to_string(), binding);
                }
            }
            _ => {
                let kind = self.value_of(value);
                self.assign_kind(place, kind);
            }
        }
    }

    fn assign_kind(&mut self, place: &Expr, kind: Kind) {
        match place {
            Expr::Path(p) => {
                if let Some(ident) = p.path.get_ident() {
                    self.env.assign(ident.to_string(), Binding::of(kind));
                }
            }
            Expr::Paren(p) => self.assign_kind(&p.expr, kind),
            Expr::Tuple(t) => {
                for part in &t.elems {
                    self.assign_kind(part, kind.element());
                }
            }
            Expr::Array(a) => {
                for part in &a.elems {
                    self.assign_kind(part, kind.element());
                }
            }
            // `deps.0 = repo`: store into that part.
            Expr::Field(f) if path_ident(&f.base).is_some() => {
                if let Some(base) = path_ident(&f.base) {
                    self.env.assign_part(&base, &member_name(&f.member), kind);
                }
            }
            // `a.b.c = repo`, `repos[0] = repo`, `*slot = repo`: the name at
            // the root now holds a handle somewhere inside it.
            _ => {
                if kind == Kind::Plain {
                    return;
                }
                let deref = matches!(place, Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)));
                if let Some(root) = place_root(place) {
                    let held = if deref { kind } else { Kind::Carrier };
                    let kind = self.env.get(&root).max(held);
                    self.env.assign(root, Binding::of(kind));
                }
            }
        }
    }

    // ── Scopes, branches and exits ───────────────────────────────────

    /// Run `f` in a new innermost scope.
    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.env.push();
        let out = f(self);
        self.env.pop();
        out
    }

    /// Run `f` in an exit frame, then join the bindings of every exit that
    /// landed in it.
    fn framed<T>(&mut self, target: Target, f: impl FnOnce(&mut Self) -> T) -> T {
        self.exits.push(ExitFrame {
            target,
            depth: self.env.depth(),
            env: None,
        });
        let out = f(self);
        if let Some(frame) = self.exits.pop()
            && let Some(env) = frame.env
        {
            self.env.join(&env);
        }
        out
    }

    /// Record the bindings at a `break` or `continue`. Labels are not
    /// resolved, so every loop and labeled block up to the enclosing body
    /// gets them.
    fn exit_loop(&mut self) {
        for frame in self.exits.iter_mut().rev() {
            if frame.target == Target::Body {
                break;
            }
            frame.record(&self.env);
        }
    }

    /// Record the bindings at a `return` or `?`.
    fn exit_body(&mut self) {
        if let Some(frame) = self
            .exits
            .iter_mut()
            .rev()
            .find(|f| f.target == Target::Body)
        {
            frame.record(&self.env);
        }
    }

    /// Run `f` as code that runs zero or one times.
    fn optional<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let entry = self.env.clone();
        let out = f(self);
        self.env.join(&entry);
        out
    }

    /// Run `f` as code that runs zero or more times: a loop body or a
    /// closure body. Repeat until the bindings stop changing, so a binding
    /// made late in one pass reaches the next. Each repeat drops the ledger
    /// lines and errors of the pass before it.
    fn repeated<T>(&mut self, mut f: impl FnMut(&mut Self) -> T) -> T {
        loop {
            let entry = self.env.clone();
            let (ledger, errors) = (self.ledger.len(), self.errors.len());
            let out = f(self);
            self.env.join(&entry);
            if self.env == entry {
                return out;
            }
            self.ledger.truncate(ledger);
            self.errors.truncate(errors);
        }
    }

    /// Run `f` unless `attrs` carry a statement annotation. An annotation
    /// replaces the cost, not the bindings or the exits: `f` still runs, what
    /// it counts and the errors it raises are dropped, and each of its paths
    /// costs the declared amount.
    fn annotated(&mut self, attrs: &[Attribute], f: impl FnOnce(&mut Self) -> Flow) -> Flow {
        let Some(annotation) = self.annotation(attrs) else {
            return f(self);
        };
        let (ledger, errors) = (self.ledger.len(), self.errors.len());
        let flow = f(self);
        self.ledger.truncate(ledger);
        self.errors.truncate(errors);
        flow.with_cost(&annotation.cost())
    }

    // ── Blocks and statements ────────────────────────────────────────

    fn block(&mut self, block: &Block) -> Flow {
        self.scoped(|s| {
            let mut flow = Flow::ZERO;
            for stmt in &block.stmts {
                let next = s.stmt(stmt);
                flow = flow.then(next);
            }
            flow
        })
    }

    fn stmt(&mut self, stmt: &Stmt) -> Flow {
        let attrs: &[Attribute] = match stmt {
            Stmt::Local(local) => &local.attrs,
            Stmt::Expr(expr, _) => stmt_expr_attrs(expr),
            Stmt::Macro(m) => &m.attrs,
            // A nested `fn`/`struct`/`impl` is a *definition*; nothing runs
            // here. A call to it is analysed at the call site instead.
            Stmt::Item(_) => return Flow::ZERO,
        };
        self.annotated(attrs, |s| s.stmt_unannotated(stmt))
    }

    fn stmt_unannotated(&mut self, stmt: &Stmt) -> Flow {
        match stmt {
            Stmt::Local(local) => self.local(local),
            Stmt::Expr(expr, _) => self.expr(expr),
            Stmt::Macro(m) => Flow::cost(self.mac(&m.mac)),
            Stmt::Item(_) => Flow::ZERO,
        }
    }

    fn local(&mut self, local: &Local) -> Flow {
        let Some(init) = &local.init else {
            // `let repo;` holds nothing yet.
            self.bind_pat(&local.pat, Kind::Plain);
            return Flow::ZERO;
        };
        let mut flow = self.expr(&init.expr);
        if let Some((_, diverge)) = &init.diverge {
            // The `else` block always leaves, so its bindings do not reach
            // the next statement. Its exits record their own.
            let entry = self.env.clone();
            let diverge = self.expr(diverge);
            self.env = entry;
            flow = flow.then(Flow::ZERO.or_worst(diverge));
        }
        self.bind_init(&local.pat, &init.expr);
        flow
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

    fn expr(&mut self, expr: &Expr) -> Flow {
        self.expr_in(expr, false)
    }

    /// The cost of every path through `expr`. Used for operands, where an
    /// exit is rare: taking it as a fall-through can only over-count.
    fn cost_of(&mut self, expr: &Expr) -> Cost {
        self.expr(expr).total()
    }

    /// `awaited` says whether this expression is the base of an enclosing
    /// `.await` — the marker that a chain actually runs.
    #[allow(clippy::too_many_lines)]
    fn expr_in(&mut self, expr: &Expr, awaited: bool) -> Flow {
        match expr {
            Expr::Await(e) => self.expr_in(&e.base, true),
            Expr::Try(e) => {
                let flow = self.expr_in(&e.expr, awaited);
                self.exit_body();
                flow
            }
            Expr::Paren(e) => self.expr_in(&e.expr, awaited),
            Expr::Group(e) => self.expr_in(&e.expr, awaited),

            Expr::MethodCall(mc) => Flow::cost(self.method_chain(mc, awaited)),
            // `(|| async move { … })()` — the shape `#[cached]` wraps a handler
            // body in. It runs exactly once, so it is seen through rather than
            // reported as a closure the user never wrote.
            Expr::Call(call) if immediately_invoked_closure(&call.func).is_some() => {
                let closure = immediately_invoked_closure(&call.func)
                    .expect("guarded by the match arm above");
                let mut cost = Cost::ZERO;
                for arg in &call.args {
                    cost = cost.then(self.cost_of(arg));
                }
                let params: Vec<Kind> = call.args.iter().map(|a| self.value_of(a)).collect();
                Flow::cost(cost.then(self.closure_body(closure, &params, Kind::Plain)))
            }
            Expr::Call(call) => Flow::cost(self.call(call)),
            Expr::Macro(m) => Flow::cost(self.mac(&m.mac)),
            Expr::Closure(_) => Flow::cost(self.closure_arg(expr, Kind::Plain, false)),

            Expr::ForLoop(f) => {
                let iter = self.cost_of(&f.expr);
                let element = self.value_of(&f.expr).element();
                let body = self.loop_cost(const_bound(&f.expr), f.span(), |s| {
                    s.bind_pat(&f.pat, element);
                    s.block(&f.body).total()
                });
                Flow::cost(iter.then(body))
            }
            // The condition runs on every pass, so a query in it
            // (`while let Some(job) = repo.next_pending().await?`) is
            // loop-resident.
            Expr::While(w) => Flow::cost(self.loop_cost(None, w.span(), |s| {
                let cond = s.cost_of(&w.cond);
                cond.then(s.block(&w.body).total())
            })),
            Expr::Loop(l) => {
                Flow::cost(self.loop_cost(None, l.span(), |s| s.block(&l.body).total()))
            }

            Expr::If(i) => {
                // `if let` binds in a scope that covers the condition and the
                // then-branch only.
                self.env.push();
                let cond = self.expr(&i.cond);
                let mut else_env = self.env.clone();
                else_env.pop();
                let then = self.block(&i.then_branch);
                self.env.pop();
                let then_env = std::mem::replace(&mut self.env, else_env);
                let els = i
                    .else_branch
                    .as_ref()
                    .map_or(Flow::ZERO, |(_, e)| self.expr(e));
                // Only a branch that falls through brings its bindings to
                // the next statement. Its exits recorded their own.
                match (then.fall.is_some(), els.fall.is_some()) {
                    (true, true) => self.env.join(&then_env),
                    (true, false) => self.env = then_env,
                    (false, _) => {}
                }
                cond.then(then.or_worst(els))
            }
            Expr::Match(m) => self.match_expr(m),

            Expr::Block(b) if b.label.is_some() => {
                // `break 'label` lands after the block.
                let flow = self.framed(Target::Break, |s| s.block(&b.block));
                Flow {
                    fall: worst(flow.fall, flow.brk),
                    brk: None,
                    ret: flow.ret,
                }
            }
            Expr::Block(syn::ExprBlock { block, .. })
            | Expr::Unsafe(syn::ExprUnsafe { block, .. })
            | Expr::TryBlock(syn::ExprTryBlock { block, .. }) => self.block(block),
            // An async block may never be polled. A `return` or `?` in it
            // leaves the block only.
            Expr::Async(a) => {
                Flow::cost(self.optional(|s| s.framed(Target::Body, |s| s.block(&a.block).total())))
            }
            Expr::Const(c) => Flow::cost(self.block(&c.block).total()),

            Expr::Array(a) => self.each(a.elems.iter()),
            Expr::Tuple(t) => self.each(t.elems.iter()),
            Expr::Assign(a) => {
                let flow = self.expr(&a.left).then(self.expr(&a.right));
                self.assign(&a.left, &a.right);
                flow
            }
            // The right side of `&&` / `||` may not run.
            Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
                let left = self.expr(&b.left);
                left.then(self.optional(|s| s.expr(&b.right)))
            }
            Expr::Binary(b) => self.expr(&b.left).then(self.expr(&b.right)),
            Expr::Return(r) => {
                let value = r.expr.as_deref().map_or(Flow::ZERO, |e| self.expr(e));
                self.exit_body();
                value.then(Flow::RETURN)
            }
            Expr::Break(b) => {
                let value = b.expr.as_deref().map_or(Flow::ZERO, |e| self.expr(e));
                self.exit_loop();
                value.then(Flow::BREAK)
            }
            Expr::Continue(_) => {
                self.exit_loop();
                Flow::BREAK
            }
            Expr::Cast(c) => self.expr(&c.expr),
            Expr::Field(f) => self.expr(&f.base),
            Expr::Index(i) => self.expr(&i.expr).then(self.expr(&i.index)),
            Expr::Let(l) => {
                let flow = self.expr(&l.expr);
                self.bind_init(&l.pat, &l.expr);
                flow
            }
            Expr::Range(r) => {
                let start = r.start.as_deref().map_or(Flow::ZERO, |e| self.expr(e));
                let end = r.end.as_deref().map_or(Flow::ZERO, |e| self.expr(e));
                start.then(end)
            }
            Expr::RawAddr(r) => self.expr(&r.expr),
            Expr::Reference(r) => self.expr(&r.expr),
            Expr::Repeat(r) => self.expr(&r.expr).then(self.expr(&r.len)),
            Expr::Struct(s) => {
                let mut flow = self.each(s.fields.iter().map(|f| &f.expr));
                if let Some(rest) = &s.rest {
                    flow = flow.then(self.expr(rest));
                }
                flow
            }
            Expr::Unary(u) => self.expr(&u.expr),
            Expr::Yield(y) => y.expr.as_deref().map_or(Flow::ZERO, |e| self.expr(e)),

            // Forms that hold no reachable call at all.
            Expr::Lit(_) | Expr::Path(_) | Expr::Infer(_) => Flow::ZERO,

            // `Expr::Verbatim` — syntax this `syn` could not parse — and any
            // variant a future `syn` adds. Assuming those are query-free would
            // make the no-false-negative claim depend on the toolchain, so an
            // unreadable form that names the handle is reported instead.
            other => {
                if tokens_mention_any(&other.to_token_stream(), &|name| self.env.is_tracked(name)) {
                    Flow::cost(Cost::unbounded(
                        other.span(),
                        "an expression form the analysis does not recognise names the database \
                         handle",
                        DECLARE_HINT,
                    ))
                } else {
                    Flow::ZERO
                }
            }
        }
    }

    /// A `match`. Each arm starts from the bindings after the scrutinee, and
    /// the bindings of the arms that fall through join afterwards. Exactly one body runs, so bodies
    /// take the worst arm. A failing guard falls through to the next arm, so
    /// every guard on the path can run: guards sum.
    fn match_expr(&mut self, m: &syn::ExprMatch) -> Flow {
        let scrutinee = self.expr(&m.expr);
        let mut entry = self.env.clone();
        let mut guards = Cost::ZERO;
        let mut bodies = Flow::NEVER;
        let mut joined: Option<Env> = None;
        for arm in &m.arms {
            self.env = entry.clone();
            let (pat, guard) = crate::parse::arm_pat_and_guard(arm);
            let depth = self.env.depth();
            let (guard, after_guard, body) = self.scoped(|s| {
                s.bind_init(pat, &m.expr);
                let guard = guard.map_or(Cost::ZERO, |g| s.cost_of(g));
                let after_guard = s.env.clone();
                (
                    guard,
                    after_guard,
                    s.annotated(&arm.attrs, |s| s.expr(&arm.body)),
                )
            });
            guards = guards.then(guard);
            // A failing guard falls through to the next arm with its bindings.
            let mut after_guard = after_guard;
            after_guard.scopes.truncate(depth);
            entry.join(&after_guard);
            let falls = body.fall.is_some();
            bodies = bodies.or_worst(body);
            if !falls {
                // The arm always leaves; its exits recorded their bindings.
                continue;
            }
            let arm_env = std::mem::replace(&mut self.env, Env::new());
            match &mut joined {
                Some(env) => env.join(&arm_env),
                None => joined = Some(arm_env),
            }
        }
        self.env = joined.unwrap_or(entry);
        scrutinee.then(Flow::cost(guards)).then(bodies)
    }

    fn each<'a>(&mut self, exprs: impl Iterator<Item = &'a Expr>) -> Flow {
        let mut flow = Flow::ZERO;
        for expr in exprs {
            let next = self.expr(expr);
            flow = flow.then(next);
        }
        flow
    }

    /// A closure's body, with parameter `i` bound to `params[i]`, or to
    /// `rest` past the end. A `return` inside leaves the closure only.
    fn closure_body(&mut self, closure: &syn::ExprClosure, params: &[Kind], rest: Kind) -> Cost {
        self.framed(Target::Body, |s| {
            s.scoped(|s| {
                for (i, input) in closure.inputs.iter().enumerate() {
                    s.bind_pat(input, params.get(i).copied().unwrap_or(rest));
                }
                s.cost_of(&closure.body)
            })
        })
    }

    /// An argument that may be a closure run any number of times. A query in
    /// it is unbounded.
    fn closure_arg(&mut self, arg: &Expr, param: Kind, takes_callback: bool) -> Cost {
        let Expr::Closure(closure) = arg else {
            return self.non_closure_arg(arg, takes_callback);
        };
        let body = self.repeated(|s| s.closure_body(closure, &[], param));
        if body.is_zero() || matches!(body, Cost::Unbounded(_)) {
            // An unbounded body already explains itself (a nested loop, an
            // opaque helper). Do not overwrite a better diagnostic.
            return body;
        }
        Cost::unbounded(
            closure.span(),
            format!(
                "a database query ({}) runs inside a closure, which the analysis cannot prove \
                 runs only once",
                self.last_counted()
            ),
            BATCH_HINT,
        )
    }

    /// An argument that is not a closure literal. In callback position, a
    /// plain path (`PgPostRepository::find_all`, `do_work`) is a function
    /// whose body the analysis cannot read.
    fn non_closure_arg(&mut self, arg: &Expr, takes_callback: bool) -> Cost {
        let cost = self.cost_of(arg);
        if takes_callback && matches!(arg, Expr::Path(_)) && self.value_of(arg) == Kind::Plain {
            return Cost::unbounded(
                arg.span(),
                "a function is passed by name where a closure runs, and its body is another \
                 function's business",
                DECLARE_HINT,
            );
        }
        cost
    }

    /// An argument that runs at most once, such as a transaction callback.
    /// The closure body is a fixed cost.
    fn callback_arg(&mut self, arg: &Expr, param: Kind, takes_callback: bool) -> Cost {
        let Expr::Closure(closure) = arg else {
            return self.non_closure_arg(arg, takes_callback);
        };
        self.optional(|s| s.closure_body(closure, &[], param))
    }

    /// A loop's cost. `body` runs in its own scope and break frame, zero or
    /// more times. A `return` in it counts as one more pass: the loop's
    /// fall-through cost covers it.
    fn loop_cost(
        &mut self,
        bound: Option<u32>,
        span: Span,
        mut body: impl FnMut(&mut Self) -> Cost,
    ) -> Cost {
        let before = self.ledger.len();
        let cost = self.repeated(|s| s.framed(Target::Break, |s| s.scoped(&mut body)));
        self.bound_loop(cost, bound, span, before)
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

        let mut cost = self.cost_of(root);

        // Where the handle enters the chain: the root itself, or the first
        // method that yields one (`app.db()…`, `slot.unwrap()…`). Methods
        // before it are ordinary; methods after it act on a handle.
        let handle_from = if self.expr_is_handle(root) {
            Some(0)
        } else {
            methods
                .iter()
                .position(|m| self.method_is_handle(m))
                .map(|i| i + 1)
        };

        // Arguments run regardless of what the chain does with them.
        for method in &methods {
            cost = cost.then(self.method_args(method));
        }
        if matches!(cost, Cost::Unbounded(_)) {
            return cost;
        }
        if let Some(opaque) = self.opaque_container_method(&methods) {
            return opaque;
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
            // `LazyDb::checkout` hands over a connection and costs nothing,
            // awaited or not. Only on a known `LazyDb`: a repository's own
            // `checkout` may be a real query (Codex review, PR #2762).
            if HANDLE_TRANSITIONS.contains(&last.as_str()) && self.expr_is_lazy_db(root) {
                return cost;
            }
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
            // `repos.push(repo)`: a known container method stores the handle.
            if self.expr_is_carrier(&method.receiver)
                && is_container_method(&method.method.to_string())
            {
                continue;
            }
            let is_executor = EXECUTORS.contains(&method.method.to_string().as_str());
            let takes_handle = method.args.iter().any(|a| self.expr_carries_handle(a));
            // A diesel executor is handed the connection
            // (`query.load(&mut conn)`). Without it, `store.load(id)` is an
            // ordinary async API (#1667 review, round three).
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

    /// An unknown method on a container of handles (an extension-trait
    /// `repos.refresh_all()`), or any method on a user struct that holds one,
    /// may query through them.
    fn opaque_container_method(&self, methods: &[&ExprMethodCall]) -> Option<Cost> {
        let unknown = methods.iter().find(|m| {
            self.expr_is_carrier(&m.receiver)
                && (self.expr_is_user_carrier(&m.receiver)
                    || !is_container_method(&m.method.to_string()))
        })?;
        Some(Cost::unbounded(
            unknown.span(),
            format!(
                "`{}` is called on a container of database handles, and what it does with them \
                 is another function's business",
                unknown.method
            ),
            DECLARE_HINT,
        ))
    }

    /// One method call's arguments. A transaction runs its closure once and
    /// hands it a connection. An `Option`/`Result` combinator runs its
    /// closure at most once. Any other closure may run once per element.
    fn method_args(&mut self, method: &ExprMethodCall) -> Cost {
        let name = method.method.to_string();
        let is_transaction = TRANSACTION_METHODS.contains(&name.as_str());
        let runs_once = is_transaction || AT_MOST_ONCE_CLOSURE_METHODS.contains(&name.as_str());
        // A closure handed to a method on a carrier takes its elements:
        // `repos.iter().for_each(|r| …)`.
        let param = if is_transaction || self.expr_is_carrier(&method.receiver) {
            Kind::Handle
        } else {
            Kind::Plain
        };
        // A function given by path where a closure would run is opaque.
        let takes_callback = is_transaction
            || (CALLBACK_METHODS.contains(&name.as_str())
                && self.value_of(&method.receiver) != Kind::Plain);
        let mut cost = Cost::ZERO;
        for arg in &method.args {
            let next = if runs_once {
                self.callback_arg(arg, param, takes_callback)
            } else {
                self.closure_arg(arg, param, takes_callback)
            };
            cost = cost.then(next);
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
        // `scoped_transaction` / `savepoint` run their closure once and hand
        // it a connection.
        let runs_once = name
            .as_deref()
            .is_some_and(|n| TRANSACTION_FREE_FNS.contains(&n));

        let mut cost = self.cost_of(&call.func);
        for arg in &call.args {
            let next = if runs_once {
                self.callback_arg(arg, Kind::Handle, true)
            } else {
                self.cost_of(arg)
            };
            cost = cost.then(next);
        }
        // The connection is the callback's, not an escape into opaque code.
        // An unreadable argument already explains itself.
        if runs_once || matches!(cost, Cost::Unbounded(_)) {
            return cost;
        }
        if name.as_deref().is_some_and(|n| SAFE_FREE_FNS.contains(&n)) {
            return cost;
        }
        // `Some(repo)`, `Ok(db)`, `Arc::new(repo)`, `PgPostRepository(pool)`:
        // these build a value that holds the handle and run no query. Any
        // other uppercase callee may be a user type that runs queries.
        if is_container_constructor(call)
            || is_smart_pointer_new(call)
            || is_handle_constructor(call)
        {
            return cost;
        }
        // Any other callee is opaque, `Post::published(&mut db)` included:
        // nothing at the call site tells a one-query finder from a helper
        // that loops (#2316).
        if call.args.iter().any(|a| self.expr_carries_handle(a)) {
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
    fn mac(&mut self, mac: &syn::Macro) -> Cost {
        // `vec![a, b]` and `vec![a; n]` are read like an array.
        if let Some(elems) = vec_elems(mac) {
            let mut cost = Cost::ZERO;
            for elem in &elems {
                cost = cost.then(self.cost_of(elem));
            }
            return cost;
        }
        if !tokens_mention_any(&mac.tokens, &|name| self.env.is_tracked(name)) {
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
        // however it names the handle: it does not await, and a sync helper it
        // hands the handle to cannot run an async query. Anything else that
        // names a handle is reported (#1667 review, round two).
        if INERT_MACROS.contains(&name.as_str()) {
            return Cost::ZERO;
        }
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

    // ── What an expression holds ─────────────────────────────────────

    /// What `expr` evaluates to.
    fn value_of(&self, expr: &Expr) -> Kind {
        if self.expr_is_lazy_db(expr) {
            Kind::LazyDb
        } else if self.expr_is_handle(expr) || self.chain_root_is_handle(expr) {
            Kind::Handle
        } else if self.expr_is_carrier(expr) {
            Kind::Carrier
        } else {
            Kind::Plain
        }
    }

    /// Does `e`, as a part of a container, hold a handle? A query future
    /// does not count: it was paid for where it was built, and a future runs
    /// at most one query.
    fn holds(&self, e: &Expr) -> bool {
        !self.is_counted_query_future(e) && self.value_of(e) != Kind::Plain
    }

    /// Is `e` a query future that `method_chain` counts where it is built:
    /// `repo.recent(10)`?
    fn is_counted_query_future(&self, e: &Expr) -> bool {
        let Expr::MethodCall(mc) = e else {
            return false;
        };
        let last = mc.method.to_string();
        self.chain_root_is_handle(e)
            && !self.method_is_handle(mc)
            && !is_handle_builder(&last)
            && !HANDLE_TRANSITIONS.contains(&last.as_str())
    }

    /// Is `e` a user struct that holds a handle (`Ctx { repo }`)? Its
    /// methods are the user's, so none of them is a known container method.
    fn expr_is_user_carrier(&self, e: &Expr) -> bool {
        match e {
            Expr::Struct(_) => self.expr_is_carrier(e),
            Expr::Path(_) => path_ident(e).is_some_and(|name| {
                let binding = self.env.binding(&name);
                binding.kind == Kind::Carrier && binding.user
            }),
            Expr::Reference(r) => self.expr_is_user_carrier(&r.expr),
            Expr::Paren(p) => self.expr_is_user_carrier(&p.expr),
            Expr::Group(g) => self.expr_is_user_carrier(&g.expr),
            _ => false,
        }
    }

    /// What `f` holds, when its base is a name bound to a struct or tuple
    /// literal.
    fn part_kind(&self, f: &syn::ExprField) -> Option<Kind> {
        self.env
            .part(&path_ident(&f.base)?, &member_name(&f.member))
    }

    /// Does this expression *evaluate to* a database handle (as opposed to
    /// merely mentioning one)?
    fn expr_is_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.env.get(&i.to_string()).is_handle()),
            Expr::Reference(r) => self.expr_is_handle(&r.expr),
            Expr::RawAddr(r) => self.expr_is_handle(&r.expr),
            Expr::Paren(p) => self.expr_is_handle(&p.expr),
            Expr::Group(g) => self.expr_is_handle(&g.expr),
            // `self.conn().await?`. Only `?` unwraps to the handle: a bare
            // `.await` on a fallible accessor is a `Result` (#2546 review,
            // round 3). Recurses into the narrower check so that an awaited
            // deferred future (`pending.await?`) is not a handle (#2546).
            // `?` on a carrier takes its part (`maybe?`, `maybe.ok_or(e)?`),
            // and `?` on a name that holds a `Result<Db, E>` gives the `Db`.
            Expr::Try(t) => {
                self.awaited_expr_is_fresh_handle(&t.expr)
                    || self.expr_is_carrier(&t.expr)
                    || (path_ident(&t.expr).is_some() && self.expr_is_handle(&t.expr))
            }
            Expr::Unary(u) => matches!(u.op, syn::UnOp::Deref(_)) && self.expr_is_handle(&u.expr),
            // `Arc::new(repo)`: a smart pointer derefs to the handle.
            // `PgPostRepository(pool)`: a value of a handle type is a handle.
            Expr::Call(c) => {
                (is_smart_pointer_new(c) && c.args.first().is_some_and(|a| self.expr_is_handle(a)))
                    || is_handle_constructor(c)
            }
            Expr::Struct(st) => st
                .path
                .segments
                .last()
                .is_some_and(|s| name_is_handle_part(&s.ident.to_string())),
            // A field of a handle (`db.inner`), a field that conventionally
            // holds one (`self.repo`), or a field of a carrier (`pair.0`).
            Expr::Field(f) => {
                if let Some(kind) = self.part_kind(f) {
                    return kind.is_handle() || member_is_handle_accessor(&f.member);
                }
                self.expr_is_handle(&f.base)
                    || member_is_handle_accessor(&f.member)
                    || self.expr_is_carrier(&f.base)
            }
            // An element of a carrier: `repos[0]`.
            Expr::Index(i) => self.expr_is_carrier(&i.expr) || self.expr_is_handle(&i.expr),
            Expr::MethodCall(mc) => self.method_is_handle(mc),
            // A handle selected through a conditional is still a handle. Any
            // arm is enough (#1667 review, round four).
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

    /// Does this method call evaluate to a handle?
    fn method_is_handle(&self, mc: &ExprMethodCall) -> bool {
        let method = mc.method.to_string();
        if HANDLE_ACCESSORS.contains(&method.as_str()) {
            return true;
        }
        // A method on a carrier that returns a part: `repos.remove(0)`.
        if self.expr_is_carrier(&mc.receiver) {
            return ELEMENT_METHODS.contains(&method.as_str());
        }
        // `.expect(...)`/`.unwrap()` stand in for `?`
        // (`ctx.conn().await.expect("connection")`, #2546 review round 5).
        //
        // KNOWN LIMITATION (#2546 review, round 7): `let result =
        // ctx.conn().await; let db = result.unwrap();` is not caught. `result`
        // would need a new `Kind`: "a `Result` that unwraps to a handle".
        if RESULT_UNWRAP_METHODS.contains(&method.as_str()) {
            return self.awaited_expr_is_fresh_handle(&mc.receiver);
        }
        HANDLE_BUILDERS.contains(&method.as_str()) && self.expr_is_handle(&mc.receiver)
    }

    /// The peeled target of a `.await`/`?`: does *this* expression produce a
    /// fresh handle? A bare name never does here, so the result of an awaited
    /// deferred future is not a handle.
    fn awaited_expr_is_fresh_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Reference(r) => self.awaited_expr_is_fresh_handle(&r.expr),
            Expr::RawAddr(r) => self.awaited_expr_is_fresh_handle(&r.expr),
            Expr::Paren(p) => self.awaited_expr_is_fresh_handle(&p.expr),
            Expr::Group(g) => self.awaited_expr_is_fresh_handle(&g.expr),
            Expr::Await(a) => self.awaited_expr_is_fresh_handle(&a.base),
            Expr::Try(t) => self.awaited_expr_is_fresh_handle(&t.expr),
            Expr::Field(f) => self.expr_is_handle(&f.base) || member_is_handle_accessor(&f.member),
            // Accessors only, never `HANDLE_BUILDERS`: an awaited builder name
            // is counted as the query, so its result is rows, not a handle
            // (#2546 review, round 4).
            //
            // KNOWN LIMITATION (#2546 review, round 6): `db.pool().get().await?`
            // is not caught; it has the shape of a query through an accessor.
            // `checkout` counts only on a known `LazyDb` (PR #2762, round 3).
            Expr::MethodCall(mc) => {
                let method = mc.method.to_string();
                HANDLE_ACCESSORS.contains(&method.as_str())
                    || (HANDLE_TRANSITIONS.contains(&method.as_str())
                        && self.expr_is_lazy_db(&mc.receiver))
            }
            _ => false,
        }
    }

    /// Is this expression known to be a `LazyDb`, not only some handle?
    /// Narrow on purpose: a name convention (`HANDLE_ACCESSORS`) says "some
    /// handle", never "a `LazyDb`". A missed `LazyDb` only makes its
    /// `checkout` count as a query.
    fn expr_is_lazy_db(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.env.get(&i.to_string()) == Kind::LazyDb),
            Expr::Reference(r) => self.expr_is_lazy_db(&r.expr),
            Expr::RawAddr(r) => self.expr_is_lazy_db(&r.expr),
            Expr::Paren(p) => self.expr_is_lazy_db(&p.expr),
            Expr::Group(g) => self.expr_is_lazy_db(&g.expr),
            Expr::Unary(u) => matches!(u.op, syn::UnOp::Deref(_)) && self.expr_is_lazy_db(&u.expr),
            // `lazy?` on a `Result<LazyDb, E>`.
            Expr::Try(t) => path_ident(&t.expr).is_some() && self.expr_is_lazy_db(&t.expr),
            // `lazy_db.expect(...)` on a `Result<LazyDb, E>` (Codex review,
            // PR #2762, round 6).
            Expr::MethodCall(mc) => {
                RESULT_UNWRAP_METHODS.contains(&mc.method.to_string().as_str())
                    && self.expr_is_lazy_db(&mc.receiver)
            }
            // Every arm of a value-producing `if`/`match` has one type, so
            // one `LazyDb` arm makes them all `LazyDb` (PR #2762, round 5).
            Expr::If(i) => {
                self.block_tail_is_lazy_db(&i.then_branch)
                    || i.else_branch
                        .as_ref()
                        .is_some_and(|(_, e)| self.expr_is_lazy_db(e))
            }
            Expr::Match(m) => m.arms.iter().any(|arm| self.expr_is_lazy_db(&arm.body)),
            Expr::Block(b) => self.block_tail_is_lazy_db(&b.block),
            Expr::Unsafe(u) => self.block_tail_is_lazy_db(&u.block),
            _ => false,
        }
    }

    /// Does this expression evaluate to a value that holds handles without
    /// being one? The container rule (#2316):
    ///
    /// * an array, tuple, `vec!`, struct literal or constructor call
    ///   (`Some(repo)`) that holds a handle is a carrier;
    /// * on a carrier, a method in `CARRIER_METHODS` gives a carrier
    ///   (`repos.iter()`), one in `ELEMENT_METHODS` gives a handle
    ///   (`repos.remove(0)`), one in `SCALAR_METHODS` gives a plain value
    ///   (`repos.len()`), and any other method is reported;
    /// * an index, a field, a pattern or `?` on a carrier gives a handle (see
    ///   [`Self::expr_is_handle`]).
    fn expr_is_carrier(&self, expr: &Expr) -> bool {
        let holds = |e: &Expr| self.holds(e);
        match expr {
            Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.env.get(&i.to_string()) == Kind::Carrier),
            Expr::Reference(r) => self.expr_is_carrier(&r.expr),
            Expr::RawAddr(r) => self.expr_is_carrier(&r.expr),
            Expr::Paren(p) => self.expr_is_carrier(&p.expr),
            Expr::Group(g) => self.expr_is_carrier(&g.expr),
            Expr::Array(a) => a.elems.iter().any(holds),
            Expr::Tuple(t) => t.elems.iter().any(holds),
            Expr::Repeat(r) => holds(&r.expr),
            Expr::Struct(s) => {
                !self.expr_is_handle(expr)
                    && (s.fields.iter().any(|f| holds(&f.expr))
                        || s.rest.as_deref().is_some_and(holds))
            }
            Expr::Call(c) => {
                (is_container_constructor(c) || is_smart_pointer_new(c)) && c.args.iter().any(holds)
            }
            Expr::Field(f) => self.part_kind(f) == Some(Kind::Carrier),
            Expr::Macro(m) => vec_elems(&m.mac).is_some_and(|elems| elems.iter().any(holds)),
            Expr::MethodCall(mc) => {
                CARRIER_METHODS.contains(&mc.method.to_string().as_str())
                    && self.expr_is_carrier(&mc.receiver)
            }
            Expr::If(i) => {
                block_tail(&i.then_branch).is_some_and(|e| self.expr_is_carrier(e))
                    || i.else_branch
                        .as_ref()
                        .is_some_and(|(_, e)| self.expr_is_carrier(e))
            }
            Expr::Match(m) => m.arms.iter().any(|arm| self.expr_is_carrier(&arm.body)),
            Expr::Block(b) => block_tail(&b.block).is_some_and(|e| self.expr_is_carrier(e)),
            Expr::Unsafe(u) => block_tail(&u.block).is_some_and(|e| self.expr_is_carrier(e)),
            _ => false,
        }
    }

    /// Does a block *evaluate to* a handle — i.e. does its tail expression?
    fn block_tail_is_handle(&self, block: &Block) -> bool {
        block_tail(block).is_some_and(|e| self.expr_is_handle(e))
    }

    /// [`Self::block_tail_is_handle`] for a `LazyDb`.
    fn block_tail_is_lazy_db(&self, block: &Block) -> bool {
        block_tail(block).is_some_and(|e| self.expr_is_lazy_db(e))
    }

    /// Is this a method chain whose root is a handle (`repo.aggregate().order()`)?
    /// This keeps a deferred future or a split builder chain a handle. It does
    /// not peel `.await`/`?`: an awaited query's result is rows, not a handle.
    fn chain_root_is_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::MethodCall(mc) => {
                self.expr_is_handle(&mc.receiver) || self.chain_root_is_handle(&mc.receiver)
            }
            Expr::Paren(p) => self.chain_root_is_handle(&p.expr),
            Expr::Group(g) => self.chain_root_is_handle(&g.expr),
            _ => false,
        }
    }

    /// Does this expression *carry* a handle into a callee — directly, as a
    /// carrier, or wrapped in a context struct, tuple or slice?
    fn expr_carries_handle(&self, expr: &Expr) -> bool {
        if self.expr_is_handle(expr) || self.expr_is_carrier(expr) {
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
            // No `Await`/`Try` arms: `expr_is_handle` covers a fresh handle,
            // and a bare name under them is a resolved result.
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

impl Annotation {
    /// The cost the annotation declares for its statement.
    const fn cost(&self) -> Cost {
        match self {
            Self::Cost(n) => Cost::Exact(*n),
            Self::Exempt => Cost::ZERO,
        }
    }
}

// ── Free helpers ─────────────────────────────────────────────────────
//
// `agent_authority.rs` forked this module's handle tracking (see its module
// doc comment) and most of its similarly-named helpers have since diverged on
// purpose — it carries its own `Handle` enum where this module keeps a
// scoped `Env` of `Kind`s, and its `INERT_MACROS` deliberately excludes
// `vec!`/`format!` for a reason specific to that analyser (see its `mac()`).
//
// `expr_attrs`, `expr_attrs_mut`, `item_attrs_mut`, `immediately_invoked_closure`,
// `call_path_name`, `tokens_contain_await`, the
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

/// The attributes written before an expression statement. `syn` puts them
/// on the left operand of `=`, `+=`, a binary operator or `as`.
fn stmt_expr_attrs(expr: &Expr) -> &[Attribute] {
    let own = expr_attrs(expr);
    if !own.is_empty() {
        return own;
    }
    match expr {
        Expr::Assign(a) => stmt_expr_attrs(&a.left),
        Expr::Binary(b) => stmt_expr_attrs(&b.left),
        Expr::Cast(c) => stmt_expr_attrs(&c.expr),
        _ => &[],
    }
}

/// The tail expression of a block, when the block has one.
fn block_tail(block: &Block) -> Option<&Expr> {
    match block.stmts.last() {
        Some(Stmt::Expr(expr, None)) => Some(expr),
        _ => None,
    }
}

/// Pair each pattern with its element of a literal. `(a, .., z)` over
/// `(x, y, w)` pairs `a` with `x` and `z` with `w`. `None` when the shapes
/// do not match.
fn pair_parts<'a, P, E>(
    pats: &'a syn::punctuated::Punctuated<Pat, P>,
    exprs: &'a syn::punctuated::Punctuated<Expr, E>,
) -> Option<Vec<(&'a Pat, &'a Expr)>> {
    let exprs: Vec<&Expr> = exprs.iter().collect();
    let rests = pats.iter().filter(|p| matches!(p, Pat::Rest(_))).count();
    let Some(at) = pats.iter().position(|p| matches!(p, Pat::Rest(_))) else {
        return (pats.len() == exprs.len()).then(|| pats.iter().zip(exprs).collect());
    };
    let tail = pats.len() - at - 1;
    if rests > 1 || at + tail > exprs.len() {
        return None;
    }
    let mut pairs: Vec<(&Pat, &Expr)> = pats.iter().take(at).zip(exprs.iter().copied()).collect();
    pairs.extend(
        pats.iter()
            .skip(at + 1)
            .zip(exprs[exprs.len() - tail..].iter().copied()),
    );
    Some(pairs)
}

/// The name of a struct field or tuple index: `repo`, `0`.
fn member_name(member: &syn::Member) -> String {
    match member {
        syn::Member::Named(ident) => ident.to_string(),
        syn::Member::Unnamed(index) => index.index.to_string(),
    }
}

/// The name, when `expr` is a bare name.
fn path_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(p) => p.path.get_ident().map(ToString::to_string),
        Expr::Paren(p) => path_ident(&p.expr),
        _ => None,
    }
}

/// The name at the root of a place: `a` in `a.b.c`, `repos[0]` or `*a`.
fn place_root(place: &Expr) -> Option<String> {
    match place {
        Expr::Field(f) => place_root(&f.base),
        Expr::Index(i) => place_root(&i.expr),
        Expr::Unary(u) => place_root(&u.expr),
        Expr::Paren(p) => place_root(&p.expr),
        other => path_ident(other),
    }
}

/// Is this call `Some(x)`, `Ok(x)` or `Err(x)`?
fn is_container_constructor(call: &ExprCall) -> bool {
    call_path_name(call).is_some_and(|name| CONTAINER_CONSTRUCTORS.contains(&name.as_str()))
}

/// Does this call build a value of a handle type: `PgPostRepository(pool)`
/// or `PgPostRepository::new(pool)`?
fn is_handle_constructor(call: &ExprCall) -> bool {
    let Expr::Path(path) = &*call.func else {
        return false;
    };
    let mut segments = path.path.segments.iter().rev();
    match segments.next() {
        Some(last) if last.ident == "new" => segments
            .next()
            .is_some_and(|s| name_is_handle_part(&s.ident.to_string())),
        Some(last) => name_is_handle_part(&last.ident.to_string()),
        None => false,
    }
}

/// An exact handle name or a `*Repository`. The bare `*Db` suffix is left
/// out, because row structs (`PostDb`) use it too.
fn name_is_handle_part(name: &str) -> bool {
    HANDLE_TYPES.contains(&name) || name.ends_with("Repository")
}

/// Does this path name a handle type: `PgPostRepository`, `Db`?
fn names_handle_type(path: &syn::Path) -> bool {
    path.segments
        .last()
        .is_some_and(|s| segment_names_handle(&s.ident.to_string()))
}

/// Is `name` a handle type name, by the exact list or the name suffix?
fn segment_names_handle(name: &str) -> bool {
    HANDLE_TYPES.contains(&name) || name.ends_with("Db") || name.ends_with("Repository")
}

/// The elements of a `vec![a, b]` or `vec![a; n]` body.
fn vec_elems(mac: &syn::Macro) -> Option<Vec<Expr>> {
    if mac.path.segments.last().is_none_or(|s| s.ident != "vec") {
        return None;
    }
    let list =
        mac.parse_body_with(syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated);
    if let Ok(list) = list {
        return Some(list.into_iter().collect());
    }
    mac.parse_body_with(|input: ParseStream| {
        let elem: Expr = input.parse()?;
        let _: syn::Token![;] = input.parse()?;
        let len: Expr = input.parse()?;
        Ok(vec![elem, len])
    })
    .ok()
}

/// Is this call `Box::new(x)`, `Arc::new(x)` or `Rc::new(x)`?
fn is_smart_pointer_new(call: &ExprCall) -> bool {
    let Expr::Path(path) = &*call.func else {
        return false;
    };
    let mut segments = path.path.segments.iter().rev();
    segments.next().is_some_and(|s| s.ident == "new")
        && segments
            .next()
            .is_some_and(|s| SMART_POINTERS.contains(&s.ident.to_string().as_str()))
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

/// Does this token stream name a tracked name (recursing into groups)?
fn tokens_mention_any(tokens: &TokenStream, tracked: &impl Fn(&str) -> bool) -> bool {
    tokens.clone().into_iter().any(|tt| match tt {
        TokenTree::Ident(ident) => tracked(&ident.to_string()),
        TokenTree::Group(group) => tokens_mention_any(&group.stream(), tracked),
        _ => false,
    })
}

/// What a parameter or annotated binding of type `ty` holds.
fn type_kind(ty: &Type) -> Kind {
    // A carrier first: `Vec<Db>` and `Vec<PgPostRepository>` agree.
    if type_is_lazy_db(ty) {
        Kind::LazyDb
    } else if type_is_carrier(ty) {
        Kind::Carrier
    } else if type_is_handle(ty) {
        Kind::Handle
    } else {
        Kind::Plain
    }
}

/// Does this type hold handles without being one: a collection, `Option`,
/// tuple, array or slice of a handle type (`Vec<PgPostRepository>`)?
fn type_is_carrier(ty: &Type) -> bool {
    let holds = |inner: &Type| type_is_handle_part(inner) || type_is_carrier(inner);
    match ty {
        Type::Reference(r) => type_is_carrier(&r.elem),
        Type::Paren(p) => type_is_carrier(&p.elem),
        Type::Group(g) => type_is_carrier(&g.elem),
        Type::Array(a) => holds(&a.elem),
        Type::Slice(s) => holds(&s.elem),
        Type::Tuple(t) => t.elems.iter().any(holds),
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let name = segment.ident.to_string();
            if name == "Result" {
                // A container on the `Ok` side (`Result<Vec<Repo>, E>`), or a
                // handle on the `Err` side (`Result<(), Db>`). A handle on the
                // `Ok` side makes the `Result` a handle instead.
                let mut sides = generic_types(segment);
                return sides.next().is_some_and(type_is_carrier)
                    || sides.next().is_some_and(holds);
            }
            CARRIER_TYPES.contains(&name.as_str()) && generic_types(segment).any(holds)
        }),
        _ => false,
    }
}

/// Standard and primitive type names that cannot hold a handle unless a
/// type argument does.
const PLAIN_STD_TYPES: &[&str] = &[
    "bool", "char", "str", "String", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16",
    "u32", "u64", "u128", "usize", "f32", "f64", "Vec", "VecDeque", "Option", "Result", "HashMap",
    "HashSet", "BTreeMap", "BTreeSet", "Box", "Arc", "Rc",
];

/// Is `ty` made only of [`PLAIN_STD_TYPES`], with no `_` left to infer?
fn type_is_plain_std(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_is_plain_std(&r.elem),
        Type::Paren(p) => type_is_plain_std(&p.elem),
        Type::Group(g) => type_is_plain_std(&g.elem),
        Type::Array(a) => type_is_plain_std(&a.elem),
        Type::Slice(s) => type_is_plain_std(&s.elem),
        Type::Tuple(t) => t.elems.iter().all(type_is_plain_std),
        Type::Path(path) if path.qself.is_none() => {
            path.path.segments.last().is_some_and(|segment| {
                PLAIN_STD_TYPES.contains(&segment.ident.to_string().as_str())
                    && match &segment.arguments {
                        syn::PathArguments::None => true,
                        syn::PathArguments::AngleBracketed(args) => {
                            args.args.iter().all(|arg| match arg {
                                syn::GenericArgument::Type(inner) => type_is_plain_std(inner),
                                syn::GenericArgument::Lifetime(_) => true,
                                _ => false,
                            })
                        }
                        syn::PathArguments::Parenthesized(_) => false,
                    }
            })
        }
        _ => false,
    }
}

/// The type arguments of a path segment: `T` in `Vec<T>`.
fn generic_types(segment: &syn::PathSegment) -> impl Iterator<Item = &Type> {
    let args = match &segment.arguments {
        syn::PathArguments::AngleBracketed(args) => Some(args.args.iter()),
        _ => None,
    };
    args.into_iter().flatten().filter_map(|arg| match arg {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
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
            // A smart pointer derefs to what it holds: `Arc<PgPostRepository>`.
            // Any other wrapper (`Extension<Db>`, `State<Db>`) counts for an
            // *exact* handle type only, so `Form<NewRepo>` is not a handle.
            // A `Result` is a handle only through its `Ok` side.
            let smart_pointer = SMART_POINTERS.contains(&name.as_str());
            let take = if name == "Result" { 1 } else { usize::MAX };
            generic_types(segment).take(take).any(|inner| {
                type_is_exact_handle(inner) || (smart_pointer && type_is_handle_part(inner))
            })
        }
        // `dyn PostRepository`, `impl PostRepository`.
        Type::TraitObject(t) => bounds_name_handle(&t.bounds),
        Type::ImplTrait(t) => bounds_name_handle(&t.bounds),
        _ => false,
    }
}

/// A handle type inside a container or a smart pointer: a
/// [`name_is_handle_part`] name, or a trait object of a handle type.
fn type_is_handle_part(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_is_handle_part(&r.elem),
        Type::Paren(p) => type_is_handle_part(&p.elem),
        Type::Group(g) => type_is_handle_part(&g.elem),
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let name = segment.ident.to_string();
            name_is_handle_part(&name)
                || (SMART_POINTERS.contains(&name.as_str())
                    && generic_types(segment).any(type_is_handle_part))
        }),
        Type::TraitObject(t) => bounds_name_handle(&t.bounds),
        Type::ImplTrait(t) => bounds_name_handle(&t.bounds),
        _ => false,
    }
}

/// Does one of these trait bounds name a handle type?
fn bounds_name_handle<P>(bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, P>) -> bool {
    bounds.iter().any(|bound| match bound {
        syn::TypeParamBound::Trait(t) => names_handle_type(&t.path),
        _ => false,
    })
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

/// Does this type name `LazyDb`, directly or inside a `LAZY_DB_WRAPPERS`
/// wrapper? No name-suffix rule: a `*Repository`'s own `checkout()` may be a
/// query (Codex review, PR #2762).
fn type_is_lazy_db(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_is_lazy_db(&r.elem),
        Type::Paren(p) => type_is_lazy_db(&p.elem),
        Type::Group(g) => type_is_lazy_db(&g.elem),
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let take = if segment.ident == "Result" {
                1
            } else {
                usize::MAX
            };
            segment.ident == "LazyDb"
                || (LAZY_DB_WRAPPERS.contains(&segment.ident.to_string().as_str())
                    && generic_types(segment).take(take).any(type_is_lazy_db))
        }),
        _ => false,
    }
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

    let mut analyzer = Analyzer::new(&input_fn);
    let cost = analyzer.function_body(&input_fn.block);

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

    // ── `LazyDb::checkout` (#2264, Codex review round 3) ───────────────

    #[test]
    fn lazy_db_checkout_is_free_and_the_checked_out_db_is_tracked() {
        // The documented idiom: `checkout()` itself costs nothing, and the
        // `Db` it returns is still tracked, so the one real query after it
        // is exactly the handler's whole cost.
        assert_clean(
            "1",
            r#"
            async fn post_comment(lazy_db: LazyDb, form: Form<CommentForm>) -> AutumnResult<&'static str> {
                let mut db = lazy_db.checkout().await?;
                posts::table.load(&mut *db).await?;
                Ok("posted")
            }
            "#,
        );
    }

    #[test]
    fn a_repositorys_own_checkout_method_is_still_counted() {
        // A `*Repository` is a tracked handle by the same suffix rule as
        // `PgPostRepository` elsewhere in this file, and "checkout" is a
        // real domain verb some repository might expose (a Stripe
        // checkout-completed reconciliation, a cart checkout, ...) —
        // unrelated to `LazyDb::checkout`'s connection handoff. Zeroing its
        // cost just because the method name matches would let a real query
        // dodge the budget (Codex review, PR #2762, round 3).
        assert_error_contains(
            "0",
            r"
            async fn checkout(repo: CartRepository) -> AutumnResult<Receipt> {
                let receipt = repo.checkout().await?;
                Ok(receipt)
            }
            ",
            &["1"],
        );
    }

    #[test]
    fn shadowing_a_lazy_db_with_a_repository_still_counts_its_checkout() {
        // A shadow replaces the `LazyDb` kind. The new value is a plain
        // handle, so its `checkout` is a query.
        assert_error_contains(
            "0",
            r"
            async fn h(lazy_db: LazyDb, repo: CartRepository) -> AutumnResult<Receipt> {
                let lazy_db = repo;
                let receipt = lazy_db.checkout().await?;
                Ok(receipt)
            }
            ",
            &["1"],
        );
    }

    #[test]
    fn a_lazy_db_assigned_through_deferred_initialisation_stays_tracked() {
        // An assignment stores the `LazyDb` kind like a `let` does (Codex
        // review, PR #2762, round 4).
        assert_clean(
            "1",
            r"
            async fn h(lazy_db: LazyDb) -> AutumnResult<Markup> {
                let selected;
                selected = lazy_db;
                let mut db = selected.checkout().await?;
                let rows = posts::table.load(&mut *db).await?;
                Ok(render(&rows))
            }
            ",
        );
    }

    #[test]
    fn a_lazy_db_selected_through_a_conditional_stays_tracked() {
        // `expr_is_lazy_db` must mirror `expr_is_handle`'s `Expr::If`/
        // `Expr::Match`/`Expr::Block` arms, not just `Expr::Path`/`Assign`:
        // `let selected = if flag { first } else { second };` is sound to
        // treat as `LazyDb` on *any* arm being one, the same way
        // `expr_is_handle` already does for a generic handle — real Rust
        // requires every arm of a value-producing `if` to share one type, so
        // if either arm is `LazyDb`, both are (Codex review, PR #2762,
        // round 5).
        assert_clean(
            "1",
            r"
            async fn h(first: LazyDb, second: LazyDb, flag: bool) -> AutumnResult<Markup> {
                let selected = if flag { first } else { second };
                let mut db = selected.checkout().await?;
                let rows = posts::table.load(&mut *db).await?;
                Ok(render(&rows))
            }
            ",
        );
    }

    #[test]
    fn a_lazy_db_unwrapped_from_a_result_still_tracks_its_checked_out_db() {
        // A handler that catches extraction failure itself —
        // `lazy_db: Result<LazyDb, AutumnError>` — is a signature shape
        // `type_is_lazy_db` already recognizes (mirroring how `type_is_handle`
        // treats `Result<Db, E>`), so `method_chain`'s root-based cost
        // exemption already fired for `lazy_db.expect(...).checkout()`
        // before this fix. But `expr_is_lazy_db` had no `Expr::MethodCall`
        // arm at all, so `awaited_expr_is_fresh_handle`'s `HANDLE_TRANSITIONS`
        // check — looking at `checkout`'s *immediate* receiver,
        // `lazy_db.expect(...)`, not the outer chain's root — never
        // recognized it as `LazyDb`, so `db` was never tracked as a handle
        // and every query through it, including this N+1 loop, went
        // uncounted (Codex review, PR #2762, round 6). Asserts the loop is
        // now caught rather than that the whole handler is clean, so this
        // fails loudly if the fix regresses instead of silently compiling.
        assert_error_contains(
            "1",
            r#"
            async fn h(lazy_db: Result<LazyDb, AutumnError>, ids: Vec<i64>) -> AutumnResult<Markup> {
                let mut db = lazy_db.expect("lazy db").checkout().await?;
                for id in &ids {
                    posts::table.find(*id).first(&mut *db).await?;
                }
                Ok(render(&()))
            }
            "#,
            &["loop", "first"],
        );
    }

    #[test]
    fn a_lazy_db_nested_in_an_arbitrary_wrapper_is_not_exempt() {
        // `type_is_lazy_db`'s generic-argument peering is a *whitelist*
        // (`LAZY_DB_WRAPPERS`), unlike `type_is_handle`'s own unrestricted
        // version: `Cart<LazyDb>` still makes `cart` a generic handle (that
        // broader, pre-existing behaviour is untouched), but must not make
        // it specifically `LazyDb` — `Cart` may define its own domain
        // `checkout()`, unrelated to a connection handoff (Codex review,
        // PR #2762, round 7).
        assert_error_contains(
            "0",
            r"
            async fn h(cart: Cart<LazyDb>) -> AutumnResult<Receipt> {
                let receipt = cart.checkout().await?;
                Ok(receipt)
            }
            ",
            &["1"],
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
    fn an_associated_fn_handed_the_handle_is_reported() {
        // `Post::published(&mut db)` and `ReportBuilder::build(&mut db)` have the
        // same shape. Nothing local tells a one-query finder from a helper that
        // loops, so both are opaque (#2316).
        for call in ["Post::published(&mut db)", "ReportBuilder::build(&mut db)"] {
            let handler = format!(
                "async fn index(mut db: Db) -> AutumnResult<usize> {{
                    let rows = {call}.await?;
                    Ok(rows.len())
                }}"
            );
            let name = call.split('(').next().unwrap_or_default();
            let name = name.rsplit("::").next().unwrap_or_default();
            assert_error_contains("50", &handler, &[name]);
        }
    }

    #[test]
    fn an_associated_fn_with_a_declared_cost_is_counted_as_declared() {
        // The migration path for a model finder: declare its cost.
        let handler = r"
            async fn index(mut db: Db, page: PageRequest) -> AutumnResult<usize> {
                #[query_cost(1)]
                let posts = Post::published(&mut db).await?;
                #[query_cost(1)]
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
        // outside it, so the alias lasts after the branch (#1667 review,
        // round five).
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

    // ── Binding environment matrix (#2316) ──────────────────────────

    /// The cost a test expects the analysis to prove.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Expect {
        Exact(u32),
        Unbounded,
    }

    impl Expect {
        /// Add `extra` to an exact cost.
        const fn plus(self, extra: u32) -> Self {
            match self {
                Self::Exact(n) => Self::Exact(n + extra),
                Self::Unbounded => Self::Unbounded,
            }
        }
    }

    /// How many times a wrapped body runs.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Runs {
        /// Exactly once.
        Once,
        /// Zero or one times.
        AtMostOnce,
        /// Zero or more times: a query in it is unbounded.
        Many,
    }

    /// One way to wrap a body. `extra` is the cost the wrapper adds.
    struct Construct {
        name: &'static str,
        wrap: fn(&str) -> String,
        extra: u32,
        runs: Runs,
    }

    const CONSTRUCTS: &[Construct] = &[
        Construct {
            name: "function body",
            wrap: |b| b.to_string(),
            extra: 0,
            runs: Runs::Once,
        },
        Construct {
            name: "block",
            wrap: |b| format!("{{ {b} }}"),
            extra: 0,
            runs: Runs::Once,
        },
        Construct {
            name: "labeled block",
            wrap: |b| format!("'outer: {{ {b} }}"),
            extra: 0,
            runs: Runs::Once,
        },
        Construct {
            name: "if arm",
            wrap: |b| format!("if flag {{ {b} }}"),
            extra: 0,
            runs: Runs::AtMostOnce,
        },
        Construct {
            name: "match arm",
            wrap: |b| format!("match mode {{ Mode::A => {{ {b} }} _ => {{}} }}"),
            extra: 0,
            runs: Runs::AtMostOnce,
        },
        Construct {
            name: "loop body",
            wrap: |b| format!("for _id in &ids {{ {b} }}"),
            extra: 0,
            runs: Runs::Many,
        },
        Construct {
            name: "closure body",
            wrap: |b| format!("let _f = || {{ {b} }};"),
            extra: 0,
            runs: Runs::Many,
        },
        Construct {
            name: "IIFE",
            wrap: |b| format!("(|| {{ {b} }})();"),
            extra: 0,
            runs: Runs::Once,
        },
        Construct {
            name: "async block",
            wrap: |b| format!("let _fut = async {{ {b} }};"),
            extra: 0,
            runs: Runs::AtMostOnce,
        },
        Construct {
            name: "tx callback",
            wrap: |b| format!("let _ = db.tx(|_conn| async move {{ {b} }});"),
            extra: 1,
            runs: Runs::AtMostOnce,
        },
    ];

    /// One query built through `active`.
    const USE_ACTIVE: &str = "let _ = active.find_all();";

    /// Each form binds `active` to the handle `repo`, then runs `{Q}` in the
    /// binding's scope. The cost is the form's own cost with one query.
    const BINDING_FORMS: &[(&str, &str, Expect)] = &[
        ("let", "let active = repo; {Q}", Expect::Exact(1)),
        (
            "typed let",
            "let active: PgPostRepository = make(); {Q}",
            Expect::Exact(1),
        ),
        (
            "annotated let",
            r#"#[query_exempt(reason = "alias")] let active = repo; {Q}"#,
            Expect::Exact(1),
        ),
        (
            "assignment",
            "let active; active = repo; {Q}",
            Expect::Exact(1),
        ),
        (
            "annotated assignment",
            r#"let active; #[query_exempt(reason = "alias")] active = repo; {Q}"#,
            Expect::Exact(1),
        ),
        (
            "tuple let",
            "let (active, _k) = (repo, 1); {Q}",
            Expect::Exact(1),
        ),
        (
            "nested tuple let",
            "let ((_k, active), _j) = ((1, repo), 2); {Q}",
            Expect::Exact(1),
        ),
        (
            "let-else",
            "let Some(active) = Some(repo) else { return Ok(0); }; {Q}",
            Expect::Exact(1),
        ),
        (
            "if let",
            "if let Some(active) = Some(repo) { {Q} }",
            Expect::Exact(1),
        ),
        (
            "while let",
            "let mut slot = Some(repo); while let Some(active) = slot.take() { {Q} }",
            Expect::Unbounded,
        ),
        (
            "match arm",
            "match repo { active => { {Q} } }",
            Expect::Exact(1),
        ),
        (
            "match arm with guard",
            "match Some(repo) { Some(active) if flag => { {Q} } _ => {} }",
            Expect::Exact(1),
        ),
        (
            "for pattern",
            "for active in [repo] { {Q} }",
            Expect::Exact(1),
        ),
        (
            "closure param",
            "[repo].iter().for_each(|active| { {Q} });",
            Expect::Unbounded,
        ),
        (
            "typed closure param",
            "let _g = |active: &PgPostRepository| { {Q} };",
            Expect::Unbounded,
        ),
        ("IIFE param", "(|active| { {Q} })(repo);", Expect::Exact(1)),
        (
            "tx callback param",
            "let _ = db.tx(|active| async move { {Q} });",
            Expect::Exact(2),
        ),
    ];

    /// A handler around `body`, with every name the matrix uses in scope.
    fn matrix_handler(body: &str) -> String {
        format!(
            "async fn h(repo: PgPostRepository, mut db: Db, ids: Vec<i64>, flag: bool, \
             mode: Mode) -> AutumnResult<usize> {{ {body} Ok(0) }}"
        )
    }

    /// `None` when the analysis proves `expect`, else a line saying why not.
    fn check(handler: &str, expect: Expect) -> Option<String> {
        match expect {
            Expect::Exact(n) => {
                if let Some(err) = error_of(&n.to_string(), handler) {
                    return Some(format!("expected {n}, got an error: {err}"));
                }
                if n > 0 && error_of(&(n - 1).to_string(), handler).is_none() {
                    return Some(format!("expected {n}, but budget {} passes", n - 1));
                }
                None
            }
            Expect::Unbounded => error_of("50", handler)
                .is_none()
                .then(|| "expected unbounded, but budget 50 passes".to_string()),
        }
    }

    /// Check each `(name, body, expect)` in the matrix handler.
    fn check_cases(cases: &[(&str, &str, Expect)]) {
        let mut failures = Vec::new();
        for (name, body, expect) in cases {
            let handler = matrix_handler(body);
            if let Some(why) = check(&handler, *expect) {
                failures.push(format!("{name}: {why}\n  {handler}"));
            }
        }
        assert_matrix(&failures);
    }

    fn assert_matrix(failures: &[String]) {
        assert!(
            failures.is_empty(),
            "{} matrix cell(s) failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn every_binding_form_tracks_the_handle_in_every_construct() {
        let mut failures = Vec::new();
        for (form, template, form_cost) in BINDING_FORMS {
            let body = template.replace("{Q}", USE_ACTIVE);
            for c in CONSTRUCTS {
                let expect = if c.runs == Runs::Many {
                    Expect::Unbounded
                } else {
                    form_cost.plus(c.extra)
                };
                let handler = matrix_handler(&(c.wrap)(&body));
                if let Some(why) = check(&handler, expect) {
                    failures.push(format!("{form} in {}: {why}\n  {handler}", c.name));
                }
            }
        }
        assert_matrix(&failures);
    }

    /// Each form rebinds `repo` to a plain value inside its own scope, then
    /// runs `{S}` there. `{S}` must be free.
    const SHADOW_FORMS: &[(&str, &str)] = &[
        ("let", "let repo = 1; {S}"),
        (
            "let-else",
            "let Some(repo) = Some(1) else { return Ok(0); }; {S}",
        ),
        ("if let", "if let Some(repo) = Some(1) { {S} }"),
        ("while let", "while let Some(repo) = stack.pop() { {S} }"),
        ("match arm", "match 1 { repo => { {S} } }"),
        ("for pattern", "for repo in &ids { {S} }"),
        ("closure param", "ids.iter().for_each(|repo| { {S} });"),
        ("IIFE param", "(|repo| { {S} })(1);"),
    ];

    /// Free when `repo` is plain; a query when `repo` is still the handle.
    const USE_SHADOW: &str = "let _ = repo.len();";

    #[test]
    fn an_inner_shadow_never_strips_the_outer_handle() {
        // Inside the scope, `repo.len()` is free. After it, the real handle is
        // still a handle. The "function body" construct is left out: there, a
        // `let` shadow really does last to the end of the function.
        let mut failures = Vec::new();
        for (form, template) in SHADOW_FORMS {
            let body = template.replace("{S}", USE_SHADOW);
            for c in CONSTRUCTS.iter().filter(|c| c.name != "function body") {
                let handler = matrix_handler(&format!(
                    "let mut stack = vec![1]; {} let _ = repo.find_all();",
                    (c.wrap)(&body)
                ));
                if let Some(why) = check(&handler, Expect::Exact(1 + c.extra)) {
                    failures.push(format!("{form} in {}: {why}\n  {handler}", c.name));
                }
            }
        }
        assert_matrix(&failures);
    }

    #[test]
    fn a_scope_never_undoes_an_assignment_made_through_it() {
        // `active` is declared outside; the scope assigns it. The handle must
        // survive the scope.
        let mut failures = Vec::new();
        for c in CONSTRUCTS {
            let handler = matrix_handler(&format!(
                "let active; {} {USE_ACTIVE}",
                (c.wrap)("active = repo;")
            ));
            if let Some(why) = check(&handler, Expect::Exact(1 + c.extra)) {
                failures.push(format!("{}: {why}\n  {handler}", c.name));
            }
        }
        assert_matrix(&failures);
    }

    #[test]
    fn a_clearing_assignment_strips_the_handle_only_on_paths_that_run_it() {
        // Where the scope always runs, the handle is gone after it. Where it
        // may not run, the other path keeps the handle, so the query counts.
        let mut failures = Vec::new();
        for c in CONSTRUCTS {
            let handler = matrix_handler(&format!(
                "let mut active = repo; {} {USE_ACTIVE}",
                (c.wrap)("active = Vec::new();")
            ));
            let expect = Expect::Exact(u32::from(c.runs != Runs::Once) + c.extra);
            if let Some(why) = check(&handler, expect) {
                failures.push(format!("{}: {why}\n  {handler}", c.name));
            }
        }
        assert_matrix(&failures);
    }

    #[test]
    fn a_handle_assigned_late_in_a_loop_reaches_the_next_iteration() {
        // First pass: `cur` is plain. Second pass: it is the handle. The
        // analysis must see the second pass.
        let handler = matrix_handler(
            "let mut cur = Vec::new(); for _id in &ids { let _ = cur.find_all(); cur = repo; }",
        );
        assert_error_contains("50", &handler, &["loop"]);
    }

    #[test]
    fn exit_edges_carry_their_bindings_to_where_they_land() {
        // Each exit skips a clearing assignment. The state at the exit must
        // reach the code after the construct.
        let cases = [
            (
                "continue",
                "let mut active = Vec::new(); \
                 for _id in &ids { active = repo; if flag { continue; } active = Vec::new(); }",
            ),
            (
                "break",
                "let mut active = Vec::new(); \
                 for _id in &ids { active = repo; if flag { break; } active = Vec::new(); }",
            ),
            (
                "labeled block break",
                "let mut active = repo; 'a: { if flag { break 'a; } active = Vec::new(); }",
            ),
            (
                "return from a closure",
                "let mut active = Vec::new(); \
                 let _f = || { active = repo; if flag { return; } active = Vec::new(); };",
            ),
            (
                "? in an async block",
                "let mut active = Vec::new(); \
                 let _fut = async { active = repo; check()?; active = Vec::new(); Ok(()) };",
            ),
        ];
        let mut failures = Vec::new();
        for (name, body) in cases {
            let handler = matrix_handler(&format!("{body} {USE_ACTIVE}"));
            if let Some(why) = check(&handler, Expect::Exact(1)) {
                failures.push(format!("{name}: {why}\n  {handler}"));
            }
        }
        assert_matrix(&failures);
    }

    #[test]
    fn bindings_follow_only_the_paths_that_run() {
        let cases: &[(&str, &str, Expect)] = &[
            // The right side of `||` may not run, so its clearing assignment
            // must not strip the handle on the other path.
            (
                "short-circuit right side",
                "let mut slot = Some(repo); let _ = flag || { slot = None; true }; \
                 if let Some(r) = slot { let _ = r.find_all(); }",
                Expect::Exact(1),
            ),
            // A branch that always leaves does not reach the next statement,
            // so its bindings do not apply there.
            (
                "diverging if branch",
                "let mut slot = None; if flag { slot = Some(repo); return Ok(0); } \
                 if let Some(r) = slot { let _ = r.find_all(); }",
                Expect::Exact(0),
            ),
            (
                "diverging match arm",
                "let mut slot = None; \
                 match mode { Mode::A => { slot = Some(repo); return Ok(0); } _ => {} } \
                 if let Some(r) = slot { let _ = r.find_all(); }",
                Expect::Exact(0),
            ),
        ];
        check_cases(cases);
    }

    // ── Early exits (#2316) ──────────────────────────────────────────

    #[test]
    fn early_exits_count_each_path_on_its_own() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "early return",
                "if flag { return Ok(repo.find_cached().await?.len()); }
                 let _ = repo.find_fresh().await?;",
                Expect::Exact(1),
            ),
            (
                "early return in a nested block",
                "{ if flag { return Ok(repo.find_cached().await?.len()); } }
                 let _ = repo.find_fresh().await?;",
                Expect::Exact(1),
            ),
            (
                "both arms return",
                "if flag { return Ok(repo.a().await?.len()); } else { return Ok(repo.b().await?.len()); }",
                Expect::Exact(1),
            ),
            (
                "match arm returns",
                "let row = match repo.find(1).await? { Some(r) => r, None => return Ok(0) };
                 let _ = repo.more(row).await?;",
                Expect::Exact(2),
            ),
            (
                "let-else returns",
                "let Some(row) = repo.find(1).await? else { return Ok(repo.fallback().await?.len()); };
                 let _ = repo.more(row).await?;",
                Expect::Exact(2),
            ),
            (
                "return in a tx callback leaves the callback only",
                "let _ = db.tx(|c| async move { if flag { return c.a().await; } c.b().await }).await?;
                 let _ = repo.after().await?;",
                Expect::Exact(3),
            ),
            (
                "return in a bounded loop",
                "for _ in 0..2 { if flag { return Ok(repo.a().await?.len()); } }",
                Expect::Exact(2),
            ),
            (
                "annotated return keeps its exit",
                "if flag { #[query_cost(1)] return Ok(load(&mut db).await?); }
                 let _ = repo.a().await?;",
                Expect::Exact(1),
            ),
            (
                "question mark sums",
                "let _ = repo.a().await?; let _ = repo.b().await?;",
                Expect::Exact(2),
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn a_break_never_hides_the_cost_after_its_target() {
        // Soundness guards. The number is the real maximum. The analysis must
        // reject any budget below it.
        let cases: &[(&str, &str, u32)] = &[
            (
                "labeled block break",
                "'a: { if flag { let _ = repo.x().await?; break 'a; } }
                 let _ = repo.y().await?;",
                2,
            ),
            (
                "break out of a bounded loop",
                "for _ in 0..2 { if flag { let _ = repo.a().await?; break; } }
                 let _ = repo.b().await?;",
                2,
            ),
            (
                "labeled break out of two loops",
                "'o: for _ in 0..2 { for _ in 0..2 { let _ = repo.a().await?; break 'o; } }
                 let _ = repo.b().await?;",
                2,
            ),
        ];
        let mut failures = Vec::new();
        for (name, body, real_max) in cases {
            let handler = matrix_handler(body);
            if error_of(&(real_max - 1).to_string(), &handler).is_none() {
                failures.push(format!(
                    "{name}: budget {} passes\n  {handler}",
                    real_max - 1
                ));
            }
        }
        assert_matrix(&failures);
    }

    // ── Containers (#2316) ───────────────────────────────────────────

    #[test]
    fn a_container_of_handles_yields_handles() {
        // Rule: a value built from a handle carries it. Iterating, indexing,
        // destructuring, unwrapping or reading a field of it gives a handle.
        let cases: &[(&str, &str, Expect)] = &[
            (
                "array local, then for",
                "let repos = [repo]; for active in repos { let _ = active.find_all(); }",
                Expect::Unbounded,
            ),
            (
                "vec! local, then for",
                "let repos = vec![repo]; for active in &repos { let _ = active.find_all(); }",
                Expect::Unbounded,
            ),
            (
                "iterator adapter closure",
                "let repos = vec![repo]; repos.iter().for_each(|r| { let _ = r.find_all(); });",
                Expect::Unbounded,
            ),
            (
                "index",
                "let repos = [repo]; let _ = repos[0].find_all();",
                Expect::Exact(1),
            ),
            (
                "tuple field",
                "let pair = (repo, 1); let _ = pair.0.find_all();",
                Expect::Exact(1),
            ),
            (
                "Option, then if let",
                "let maybe = Some(repo); if let Some(r) = maybe { let _ = r.find_all(); }",
                Expect::Exact(1),
            ),
            (
                "Option, then unwrap",
                "let maybe = Some(repo); let _ = maybe.unwrap().find_all();",
                Expect::Exact(1),
            ),
            (
                "qualified vec! path",
                "let repos = std::vec![repo]; let _ = repos[0].find_all();",
                Expect::Exact(1),
            ),
            (
                "question mark on an Option local",
                "let maybe = Some(repo); let r = maybe?; let _ = r.find_all();",
                Expect::Exact(1),
            ),
            (
                "question mark after ok_or",
                "let maybe = Some(repo); let _ = maybe.ok_or(1)?.find_all();",
                Expect::Exact(1),
            ),
            (
                "element taken by remove",
                "let mut repos = vec![repo]; let _ = repos.remove(0).find_all();",
                Expect::Exact(1),
            ),
            (
                "element taken by unwrap_or_default",
                "let maybe = Some(repo); let _ = maybe.unwrap_or_default().find_all();",
                Expect::Exact(1),
            ),
            (
                "first, then if let",
                "let repos = vec![repo]; if let Some(r) = repos.first() { let _ = r.find_all(); }",
                Expect::Exact(1),
            ),
            (
                "smart pointer around a handle",
                "let shared = std::sync::Arc::new(repo); let _ = shared.find_all();",
                Expect::Exact(1),
            ),
            (
                "method on the container is free",
                "let repos = vec![repo]; let _ = repos.len();",
                Expect::Exact(0),
            ),
            (
                "helper handed the container",
                "let repos = vec![repo]; let _ = audit(&repos);",
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn a_container_rule_keeps_ordinary_shapes_precise() {
        let cases: &[(&str, &str, Expect)] = &[
            // Each future is counted where it is built. The array of futures
            // carries no further query.
            (
                "join_all over an array of query futures",
                "let _ = futures::future::join_all([repo.recent(10), repo.popular(10)]).await;",
                Expect::Exact(2),
            ),
            // A struct or tuple literal records what each part holds.
            (
                "plain sibling field of a context struct",
                "let ctx = PageCtx { repo: &repo, ids: &ids }; \
                 let _ = ctx.repo.recent(10); let _ = render(ctx.ids);",
                Expect::Exact(1),
            ),
            (
                "plain sibling field of a tuple",
                "let deps = (repo, ids); let _ = render(&deps.1);",
                Expect::Exact(0),
            ),
            (
                "a handle assigned into a field",
                "let mut deps = (Vec::new(), 1); deps.0 = repo; let _ = deps.0.find_all();",
                Expect::Exact(1),
            ),
            (
                "a handle assigned into a nested field",
                "let mut st = State::default(); st.inner.repo = repo; \
                 let _ = st.inner.repo.find_all();",
                Expect::Exact(1),
            ),
            (
                "json! naming a handle without await",
                r#"let _ = serde_json::json!({ "name": repo.name });"#,
                Expect::Exact(0),
            ),
            (
                "row struct named like a handle inside a Vec",
                "let rows: Vec<PostDb> = load_rows(); let _ = render(&rows);",
                Expect::Exact(0),
            ),
            (
                "rest pattern over a tuple literal",
                "match (&repo, ids) { (.., list) => { let _ = render(list); } }",
                Expect::Exact(0),
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn the_soundness_review_shapes_are_counted() {
        let cases: &[(&str, &str, Expect)] = &[
            // Only `Some`/`Ok`/`Err` are free constructors. A user wrapper
            // handed the handle is opaque.
            (
                "newtype wrapper handed the handle",
                "let _ = PostQueries(&mut db).recent();",
                Expect::Unbounded,
            ),
            (
                "uppercase local closure handed the handle",
                "let Fetch = |c| load(c); let _ = Fetch(&mut db);",
                Expect::Unbounded,
            ),
            // A value of a handle type built in place is a handle.
            (
                "tuple struct of a handle type",
                "let r = PgPostRepository(make_pool()); let _ = r.find_all();",
                Expect::Exact(1),
            ),
            (
                "struct literal of a handle type",
                "let r = PgPostRepository { pool: make_pool() }; let _ = r.find_all();",
                Expect::Exact(1),
            ),
            // A path that ends with no exit still paid for what ran before.
            (
                "empty match after a query",
                "let never: Infallible = repo.find_all().await?; match never {}",
                Expect::Exact(1),
            ),
            // A failing guard falls through to the next arm with its bindings.
            (
                "binding made in a failing guard",
                "let mut r = None; \
                 match ids.len() { _ if { r = Some(&repo); false } => {} \
                 _ => { let _ = r.unwrap().find_all(); } }",
                Expect::Exact(1),
            ),
            // A callback given by path is opaque.
            (
                "fn path handed to an adapter on a container",
                "let repos = vec![repo]; let _ = repos.iter().map(PgPostRepository::find_all);",
                Expect::Unbounded,
            ),
            (
                "fn path handed to a transaction",
                "let _ = db.tx(do_work);",
                Expect::Unbounded,
            ),
            (
                "tx_immediate counts its callback",
                "let _ = db.tx_immediate(|c| async move { let _ = c.a(); let _ = c.b(); });",
                Expect::Exact(3),
            ),
            // `vec!` is read like an array.
            (
                "futures built inside vec!",
                "for f in vec![repo.find_all(), repo.find_recent()] { let _ = f; }",
                Expect::Exact(2),
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn question_mark_on_a_result_handle_gives_the_handle() {
        let handler = r"
            async fn h(conn: Result<Db, DbError>) -> AutumnResult<usize> {
                let mut db = conn?;
                Ok(posts::table.load(&mut *db).await?.len())
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn an_err_payload_keeps_a_handle_on_the_error_side() {
        // `Result<(), Db>` holds its handle in `Err`.
        let handler = r"
            async fn h(result: Result<(), Db>) -> AutumnResult<usize> {
                if let Err(mut db) = result {
                    let _ = posts::table.load(&mut *db).await?;
                }
                Ok(0)
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn a_method_on_a_user_struct_holding_a_handle_is_reported() {
        // `clear` here is the user's method, not `Vec::clear`.
        let handler = matrix_handler("let ctx = Ctx { repo }; ctx.clear().await;");
        assert_error_contains("50", &handler, &["clear"]);
    }

    #[test]
    fn a_std_type_annotation_marks_a_binding_plain() {
        // `Vec<i64>` is checked by rustc and cannot hold a handle.
        let handler = matrix_handler(
            "let repos = vec![repo]; \
             let ids: Vec<i64> = repos.iter().map(|r| r.id).collect(); \
             let _ = render(ids);",
        );
        assert_clean("0", &handler);
        // A `_` leaves the type open, so the container rule still applies.
        let open = matrix_handler(
            "let repos = vec![repo]; \
             let ids: Vec<_> = repos.iter().map(|r| r.id).collect(); \
             let _ = render(ids);",
        );
        assert_error_contains("50", &open, &["render"]);
    }

    #[test]
    fn a_result_of_a_container_of_handles_is_a_container() {
        let handler = r"
            async fn h(result: Result<Vec<PgPostRepository>, Error>) -> AutumnResult<usize> {
                let repos = result?;
                for repo in repos { let _ = repo.find_all().await?; }
                Ok(0)
            }
            ";
        assert_error_contains("50", handler, &["loop"]);
    }

    #[test]
    fn an_unknown_method_on_a_container_of_handles_is_reported() {
        // An extension-trait method may run a query per element.
        let handler = r"
            async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> {
                repos.refresh_all().await?;
                Ok(0)
            }
            ";
        assert_error_contains("50", handler, &["refresh_all"]);
        // Known container methods stay free.
        let known = r"
            async fn h(mut repos: Vec<PgPostRepository>, extra: PgPostRepository) -> AutumnResult<usize> {
                repos.push(extra);
                repos.sort_by_key(|r| r.id);
                repos.iter().for_each(|r| drop(r));
                Ok(repos.len())
            }
            ";
        assert_clean("0", known);
    }

    #[test]
    fn a_trait_object_repository_is_a_handle() {
        let handler = r"
            async fn h(repo: Arc<dyn PostRepository>, other: impl PostRepository) -> AutumnResult<usize> {
                Ok(repo.find_all().await?.len() + other.find_all().await?.len())
            }
            ";
        assert_clean("2", handler);
        assert_error_contains("1", handler, &["2"]);
    }

    #[test]
    fn a_signature_collection_of_handles_is_a_container() {
        let handler = r"
            async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> {
                for r in &repos { let _ = r.find_all().await?; }
                Ok(repos.len())
            }
            ";
        assert_error_contains("50", handler, &["loop"]);
        // The rule does not depend on how the handle type is named.
        let only_len = r"
            async fn h(
                repos: Vec<PgPostRepository>,
                slots: Option<PgPostRepository>,
                dbs: Vec<Db>,
                maybe_db: Option<Db>,
                arc_repo: std::sync::Arc<PgPostRepository>,
            ) -> AutumnResult<usize> {
                Ok(repos.len() + usize::from(slots.is_some()) + dbs.len()
                    + usize::from(maybe_db.is_some()))
            }
            ";
        assert_clean("0", only_len);
        let through_arc = r"
            async fn h(arc_repo: std::sync::Arc<PgPostRepository>) -> AutumnResult<usize> {
                Ok(arc_repo.find_all().await?.len())
            }
            ";
        assert_clean("1", through_arc);
        assert_error_contains("0", through_arc, &["1"]);
    }

    #[test]
    fn an_err_pattern_binds_the_error_not_the_handle() {
        // `Result<Db, E>` is a handle. `Ok(db)` binds the handle; `Err(e)` binds
        // the error, so `e.to_string()` is not a query.
        let handler = r"
            async fn h(conn: Result<Db, DbError>) -> AutumnResult<usize> {
                match conn {
                    Ok(db) => { let _ = db.find_all(); }
                    Err(e) => { let _ = e.to_string(); }
                }
                Ok(0)
            }
            ";
        assert_clean("1", handler);
        assert_error_contains("0", handler, &["1"]);
    }

    #[test]
    fn an_annotation_on_an_assignment_statement_is_read() {
        // `syn` hangs a statement attribute on the left operand of `=`, `+=`
        // or `as`. The annotation must still replace the statement's cost.
        for stmt in [
            "links = load_links(&mut db).await?;",
            "total += load_links(&mut db).await?.len();",
        ] {
            let handler = format!(
                "async fn h(mut db: Db) -> AutumnResult<usize> {{
                    let mut links = Vec::new();
                    let mut total = 0;
                    #[query_cost(2)]
                    {stmt}
                    Ok(links.len() + total)
                }}"
            );
            assert_clean("2", &handler);
            assert_error_contains("1", &handler, &["2"]);
        }
    }

    // ── Round-six threads on #2315 ───────────────────────────────────

    #[test]
    fn round_six_annotated_assignment_keeps_the_alias() {
        let handler = r#"
            async fn h(repo: PgPostRepository) -> AutumnResult<usize> {
                let active;
                #[query_exempt(reason = "alias only")]
                active = repo;
                Ok(active.find_all().await?.len())
            }
            "#;
        assert_error_contains("0", handler, &["1"]);
        assert_clean("1", handler);
    }

    #[test]
    fn round_six_match_arm_pattern_binds_the_handle() {
        let handler = r"
            async fn h(repo: PgPostRepository) -> AutumnResult<usize> {
                Ok(match repo { active => active.find_all().await?.len() })
            }
            ";
        assert_error_contains("0", handler, &["1"]);
        assert_clean("1", handler);
    }

    #[test]
    fn round_six_collection_local_keeps_provenance() {
        let handler = r"
            async fn h(repo: PgPostRepository) -> AutumnResult<usize> {
                let repos = [repo];
                for active in repos { active.find_all().await?; }
                Ok(0)
            }
            ";
        assert_error_contains("50", handler, &["loop"]);
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
            "blog: model static finder, cost declared",
            "1",
            r"async fn index(mut db: Db) -> R {
                #[query_cost(1)]
                let posts = Post::published(&mut db).await?;
                Ok(posts.len())
            }",
        ),
        (
            "todo-app: paginated model finder, cost declared",
            "1",
            r"async fn list(page: PageRequest, mut db: Db) -> R {
                #[query_cost(1)]
                let page = Todo::page(&page, &mut db).await?;
                Ok(page.len())
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

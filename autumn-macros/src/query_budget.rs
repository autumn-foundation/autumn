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

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{ToTokens as _, format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::visit::Visit;
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

/// `Option`/`Result` methods that call their closure **at most once**. A
/// query inside one is a fixed cost. An iterator has some of these names
/// too, so the receiver must be known to be an `Option` or a `Result`.
const AT_MOST_ONCE_CLOSURE_METHODS: &[&str] = &[
    "unwrap_or_else",
    "ok_or_else",
    "get_or_insert_with",
    "unwrap_or_default",
    "map",
    "map_err",
    "map_or",
    "map_or_else",
    "and_then",
    "or_else",
    "filter",
    "inspect",
    "inspect_err",
    "is_some_and",
    "is_none_or",
    "is_ok_and",
    "is_err_and",
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

/// The accessors in [`HANDLE_ACCESSORS`] that give a connection, not a
/// repository. A transaction runs its callback once only on a connection.
const DB_ACCESSORS: &[&str] = &["db", "pool", "conn", "connection"];

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

/// Methods whose result is a container of what their callback returns:
/// `ids.iter().map(|_| &repo)`, `flag.then(|| &repo)`.
const WRAPPING_CALLBACKS: &[&str] = &[
    "map",
    "map_err",
    "then",
    "then_some",
    "ok_or",
    "ok_or_else",
    "reduce",
];

/// Iterator adapters whose items are the parts of what their callback
/// returns: `filter_map(|_| Some(&repo))` yields handles.
const FLATTENING_CALLBACKS: &[&str] = &["filter_map", "flat_map", "map_while", "scan"];

/// Methods whose result is what their callback returns, or what their other
/// arguments hold: `fold(init, f)`, `unwrap_or_else(f)`, `find_map(f)`.
const DIRECT_CALLBACKS: &[&str] = &[
    "and_then",
    "or",
    "xor",
    "unwrap_or",
    "or_else",
    "find_map",
    "fold",
    "try_fold",
    // Its `Try` result holds the callback's residual: `Err(&repo)`.
    "try_for_each",
    "unwrap_or_else",
    "map_or",
    "map_or_else",
];

/// Element methods whose result is only what their seed and callback give:
/// `map_or(0, |_| 1)` is an `i32`, not an element.
const OUTPUT_CALLBACKS: &[&str] = &["map_or", "map_or_else", "fold", "try_fold"];

/// Methods whose result has the receiver's type.
const SAME_TYPE_METHODS: &[&str] = &["clone", "to_owned"];

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
    "or_else",
    "map_err",
    "is_some_and",
    "is_none_or",
    "is_ok_and",
    "is_err_and",
    "reduce",
    "max_by",
    "min_by",
    "max_by_key",
    "min_by_key",
    "retain",
    "sort_by",
    "sort_by_key",
    "sort_unstable_by",
    "sort_unstable_by_key",
];

/// Smart pointers. They deref to what they hold, so `Arc<PgPostRepository>`
/// is a handle.
const SMART_POINTERS: &[&str] = &["Box", "Arc", "Rc"];

/// Standard collection types. One of a handle type (`Vec<Db>`,
/// `HashMap<i64, PgPostRepository>`) is a carrier: a known container method
/// on it is not a query, and its parts are handles.
const CARRIER_TYPES: &[&str] = &[
    "Vec",
    "VecDeque",
    "Option",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    "BinaryHeap",
    "LinkedList",
    "IndexMap",
    "IndexSet",
];

/// Methods on a carrier that return nothing, a number or a `bool`: not a
/// part of it.
const SCALAR_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains_key",
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
    "as_mut_slice",
    "xor",
    "err",
    "map_err",
    "then",
    "then_some",
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
    "inspect_err",
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
    "into_values",
    "keys",
    "into_keys",
    "pop_back",
    "pop_front",
    "to_vec",
    "flatten",
    // An iterator's extrema are an `Option` of an element.
    "max",
    "min",
    "max_by",
    "min_by",
    "max_by_key",
    "min_by_key",
    "reduce",
];

/// Methods on a carrier that return a part of it, which is a handle.
const ELEMENT_METHODS: &[&str] = &[
    "unwrap_err",
    "expect_err",
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
    "get_or_insert_with",
];

/// The methods of each standard container [`Shape`]. Each list holds only
/// the methods that container type has, and that no extension trait can
/// take over (#2316):
///
/// * not a slice method reached through `Deref` (`Vec::first` is `[T]::first`):
///   a trait method on `Vec<T>` is found first;
/// * not `clone`: `Clone` for a container needs `T: Clone`, and when it does
///   not hold, a trait method of that name runs.
/// * not `extend`: it comes from the `Extend` trait and takes `&mut self`, so
///   a user trait method `extend(&self)` is found first.
const BOOL_METHODS: &[&str] = &["then", "then_some"];

const VEC_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "into_iter",
    "as_slice",
    "as_mut_slice",
    "as_ref",
    "as_mut",
    "pop",
    "push",
    "insert",
    "remove",
    "swap_remove",
    "append",
    "clear",
    "truncate",
    "dedup",
    "retain",
    "resize",
    "reserve",
    "shrink_to_fit",
    "drain",
];

/// An array's own methods. Its slice methods come through unsizing, after
/// any trait method on the array.
const ARRAY_METHODS: &[&str] = &["as_slice", "as_mut_slice", "into_iter", "as_ref", "as_mut"];

/// A slice.
const SLICE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains",
    "iter",
    "iter_mut",
    "into_iter",
    "as_ref",
    "as_mut",
    "first",
    "last",
    "get",
    "get_mut",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "sort_unstable_by",
    "sort_unstable_by_key",
    "reverse",
    "swap",
    "chunks",
    "windows",
    "to_vec",
];

const DEQUE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains",
    "iter",
    "iter_mut",
    "into_iter",
    "get",
    "get_mut",
    "push_back",
    "push_front",
    "pop_back",
    "pop_front",
    "insert",
    "remove",
    "append",
    "clear",
    "truncate",
    "retain",
    "swap",
    "resize",
    "reserve",
    "shrink_to_fit",
    "drain",
];

const LIST_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains",
    "iter",
    "iter_mut",
    "into_iter",
    "push_back",
    "push_front",
    "pop_back",
    "pop_front",
    "append",
    "clear",
];

const HEAP_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "iter",
    "into_iter",
    "push",
    "pop",
    "append",
    "clear",
    "retain",
    "reserve",
    "shrink_to_fit",
    "drain",
];

const ITER_METHODS: &[&str] = &[
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "flatten",
    "enumerate",
    "rev",
    "skip",
    "take",
    "chain",
    "zip",
    "peekable",
    "collect",
    "by_ref",
    "next",
    "nth",
    "find",
    "find_map",
    "inspect",
    "step_by",
    "skip_while",
    "take_while",
    "count",
    "any",
    "all",
    "position",
    "for_each",
    "try_for_each",
    "fold",
    "try_fold",
    "reduce",
    "max",
    "min",
    "max_by",
    "min_by",
    "max_by_key",
    "min_by_key",
    "last",
    "cloned",
    "copied",
];

const OPTION_METHODS: &[&str] = &[
    "is_some",
    "is_none",
    "is_some_and",
    "is_none_or",
    "unwrap",
    "expect",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "map",
    "map_or",
    "map_or_else",
    "and_then",
    "or",
    "xor",
    "or_else",
    "filter",
    "take",
    "replace",
    "insert",
    "get_or_insert_with",
    "ok_or",
    "ok_or_else",
    "as_ref",
    "as_mut",
    "iter",
    "into_iter",
    "inspect",
];

/// An `Option` of a reference also has `cloned`, `copied` and `as_deref`. On
/// an `Option` of a value, an extension trait may give those names.
const OPTION_REF_METHODS: &[&str] = &[
    "is_some",
    "is_none",
    "is_some_and",
    "is_none_or",
    "unwrap",
    "expect",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "map",
    "map_or",
    "map_or_else",
    "and_then",
    "or",
    "xor",
    "or_else",
    "filter",
    "take",
    "replace",
    "insert",
    "get_or_insert_with",
    "ok_or",
    "ok_or_else",
    "as_ref",
    "as_mut",
    "iter",
    "into_iter",
    "cloned",
    "copied",
    "as_deref",
    "inspect",
];

const RESULT_METHODS: &[&str] = &[
    "is_ok",
    "is_err",
    "is_ok_and",
    "is_err_and",
    "ok",
    "err",
    "map_err",
    "unwrap_err",
    "expect_err",
    "unwrap",
    "expect",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "map",
    "map_or",
    "map_or_else",
    "and_then",
    "or_else",
    "as_ref",
    "as_mut",
    "iter",
    "into_iter",
    "inspect",
    "inspect_err",
];

const MAP_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains_key",
    "get",
    "get_mut",
    "insert",
    "remove",
    "keys",
    "values",
    "values_mut",
    "into_keys",
    "into_values",
    "iter",
    "iter_mut",
    "into_iter",
    "clear",
    "retain",
    "drain",
    "reserve",
];

/// `BTreeMap`: no `drain` and no `reserve`.
const SORTED_MAP_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains_key",
    "get",
    "get_mut",
    "insert",
    "remove",
    "keys",
    "values",
    "values_mut",
    "into_keys",
    "into_values",
    "iter",
    "iter_mut",
    "into_iter",
    "clear",
    "retain",
];

const SET_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains",
    "insert",
    "remove",
    "iter",
    "into_iter",
    "clear",
    "retain",
    "drain",
];

/// `BTreeSet`: no `drain`.
const SORTED_SET_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "contains",
    "insert",
    "remove",
    "iter",
    "into_iter",
    "clear",
    "retain",
];

const TUPLE_METHODS: &[&str] = &[];

/// The kind of standard container a carrier is. A method is known only if
/// this container has it: an extension trait may add a method with a standard
/// name (`ok()` on a `Vec`) that queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// A `bool`: `then` gives an `Option`.
    Bool,
    /// A database connection (`Db`, `PgConnection`): a transaction on it
    /// runs its callback once.
    Db,
    Vec,
    Array,
    Slice,
    /// `VecDeque`.
    Deque,
    /// `LinkedList`.
    List,
    /// `BinaryHeap`.
    Heap,
    /// An iterator over parts.
    Iter,
    /// An iterator over references to parts: `repos.iter()`.
    IterRef,
    Opt,
    /// An `Option` of a reference: `repos.first()`.
    OptRef,
    Res,
    /// `HashMap` or `IndexMap`.
    Map,
    /// `BTreeMap`.
    SortedMap,
    /// `HashSet` or `IndexSet`.
    Set,
    /// `BTreeSet`.
    SortedSet,
    Tuple,
}

impl Shape {
    /// The methods this container has.
    const fn methods(self) -> &'static [&'static str] {
        match self {
            Self::Bool => BOOL_METHODS,
            Self::Db => &[],
            Self::Vec => VEC_METHODS,
            Self::Array => ARRAY_METHODS,
            Self::Slice => SLICE_METHODS,
            Self::Deque => DEQUE_METHODS,
            Self::List => LIST_METHODS,
            Self::Heap => HEAP_METHODS,
            Self::Iter | Self::IterRef => ITER_METHODS,
            Self::Opt => OPTION_METHODS,
            Self::OptRef => OPTION_REF_METHODS,
            Self::Res => RESULT_METHODS,
            Self::Map => MAP_METHODS,
            Self::SortedMap => SORTED_MAP_METHODS,
            Self::Set => SET_METHODS,
            Self::SortedSet => SORTED_SET_METHODS,
            Self::Tuple => TUPLE_METHODS,
        }
    }

    fn has(self, method: &str) -> bool {
        self.methods().contains(&method)
    }

    /// Does `method` give a plain value here, where on an `Option` it gives
    /// the part? `Vec::insert` gives `()`, `HashSet::remove` gives `bool`.
    fn scalar_result(self, method: &str) -> bool {
        match method {
            "insert" => !matches!(self, Self::Opt | Self::OptRef | Self::Map | Self::SortedMap),
            "remove" => matches!(self, Self::Set | Self::SortedSet),
            _ => false,
        }
    }

    /// Does `method` give an `Option` of a part here, where on a `Vec` it
    /// gives the part? `VecDeque::remove`, `HashMap::insert`.
    fn option_of_part(self, method: &str) -> bool {
        match method {
            "remove" => matches!(self, Self::Deque | Self::Map | Self::SortedMap),
            "insert" => matches!(self, Self::Map | Self::SortedMap),
            _ => false,
        }
    }

    /// The shape of what `method` returns, when it returns a carrier.
    fn after(self, method: &str) -> Option<Self> {
        if self == Self::Bool {
            return matches!(method, "then" | "then_some").then_some(Self::Opt);
        }
        // What gives references, and what gives values.
        let by_ref = matches!(self, Self::IterRef | Self::OptRef)
            || !matches!(self, Self::Iter | Self::Opt | Self::Res);
        match method {
            "iter" | "iter_mut" | "keys" | "values" | "values_mut" | "chunks" | "windows" => {
                Some(Self::IterRef)
            }
            "into_iter" if self == Self::IterRef => Some(Self::IterRef),
            "into_iter" | "drain" | "into_keys" | "into_values" => Some(Self::Iter),
            // An `Option` of a reference: an element in place.
            "first" | "last" | "get" | "get_mut" | "next" | "nth" | "find" | "max" | "min"
            | "max_by" | "min_by" | "max_by_key" | "min_by_key" | "reduce"
                if by_ref && self != Self::OptRef =>
            {
                Some(Self::OptRef)
            }
            "as_ref" | "as_mut" if matches!(self, Self::Opt | Self::OptRef) => Some(Self::OptRef),
            "first" | "last" | "get" | "get_mut" | "pop" | "pop_back" | "pop_front" | "next"
            | "nth" | "find" | "find_map" | "ok" | "err" | "max" | "min" | "max_by" | "min_by"
            | "max_by_key" | "min_by_key" | "reduce" => Some(Self::Opt),
            // The items are no longer references.
            "map" | "cloned" | "copied" | "enumerate" | "zip" | "filter_map" | "flat_map" => {
                match self {
                    Self::IterRef => Some(Self::Iter),
                    Self::OptRef => Some(Self::Opt),
                    other => Some(other),
                }
            }
            "ok_or" | "ok_or_else" => Some(Self::Res),
            _ if self.option_of_part(method) => Some(Self::Opt),
            // A part has its own shape: `slot.insert(repo)`, `maybe.unwrap()`.
            _ if ELEMENT_METHODS.contains(&method) => None,
            "to_vec" => Some(Self::Vec),
            "as_slice" | "as_mut_slice" => Some(Self::Slice),
            "collect" => None,
            _ => Some(self),
        }
    }
}

/// Does `ty` leave a part to inference (`_`)?
fn type_infers(ty: &Type) -> bool {
    struct Infers(bool);
    impl<'a> Visit<'a> for Infers {
        fn visit_type_infer(&mut self, _: &'a syn::TypeInfer) {
            self.0 = true;
        }
    }
    let mut infers = Infers(false);
    infers.visit_type(ty);
    infers.0
}

/// What each side of a `Result` type holds, as the parts `Ok` and `Err`.
/// `None` when a side is left to inference: `Result<_, ()>`.
fn result_sides(ty: &Type) -> Option<Vec<(String, Kind)>> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    // `custom::Result<T, E>` may define its own variants. A `Result<T>`
    // alias hides its error type.
    if segment.ident != "Result"
        || path.qself.is_some()
        || !std_prefix(&path.path)
        || generic_types(segment).count() != 2
        || generic_types(segment).any(type_infers)
    {
        return None;
    }
    let side = |t: Option<&Type>| match t {
        Some(t) if type_is_handle_part(t) => Kind::Handle,
        Some(t) => type_kind(t),
        None => Kind::Plain,
    };
    let mut args = generic_types(segment);
    let ok = side(args.next());
    let err = side(args.next());
    Some(vec![("Ok".to_string(), ok), ("Err".to_string(), err)])
}

/// The side of a `Result` that callback `arg` of `method` takes: `map_err`
/// takes the error, `map` the value. `None` when not known.
fn callback_side(method: &str, arg: usize, args: usize) -> Option<&'static str> {
    match method {
        "map_err" | "or_else" | "unwrap_or_else" | "is_err_and" | "inspect_err" => Some("Err"),
        "map" | "and_then" | "is_ok_and" | "inspect" | "map_or" => Some("Ok"),
        // `map_or_else(default, f)`: the default takes the error.
        "map_or_else" => Some(if arg + 1 == args { "Ok" } else { "Err" }),
        _ => None,
    }
}

/// The shape of the standard container type `name`.
fn shape_named(name: &str) -> Option<Shape> {
    match name {
        "bool" => Some(Shape::Bool),
        "Vec" => Some(Shape::Vec),
        "VecDeque" => Some(Shape::Deque),
        "LinkedList" => Some(Shape::List),
        "BinaryHeap" => Some(Shape::Heap),
        "Option" => Some(Shape::Opt),
        "Result" => Some(Shape::Res),
        "HashMap" | "IndexMap" => Some(Shape::Map),
        "BTreeMap" => Some(Shape::SortedMap),
        "HashSet" | "IndexSet" => Some(Shape::Set),
        "BTreeSet" => Some(Shape::SortedSet),
        _ => None,
    }
}

/// The container shape a type names, when it names one.
fn type_shape(ty: &Type) -> Option<Shape> {
    match ty {
        Type::Reference(r) => type_shape(&r.elem),
        Type::Paren(p) => type_shape(&p.elem),
        Type::Group(g) => type_shape(&g.elem),
        Type::Array(_) => Some(Shape::Array),
        Type::Slice(_) => Some(Shape::Slice),
        Type::Tuple(_) => Some(Shape::Tuple),
        Type::Path(path) => {
            let segment = path.path.segments.last()?;
            let std = path.qself.is_none() && std_prefix(&path.path);
            match segment.ident.to_string().as_str() {
                // A connection: not the name suffix, which a user type may have.
                name if HANDLE_TYPES.contains(&name) && name != "LazyDb" => Some(Shape::Db),
                // `custom::Vec<T>` is a user type with its own methods.
                _ if !std => None,
                name if SMART_POINTERS.contains(&name) => {
                    generic_types(segment).next().and_then(type_shape)
                }
                // `Option<&T>`.
                "Option" if matches!(generic_types(segment).next(), Some(Type::Reference(_))) => {
                    Some(Shape::OptRef)
                }
                name => shape_named(name),
            }
        }
        _ => None,
    }
}

/// The shape of a part of the wrapper type `ty`: `Db` for `Option<Db>`.
fn type_inner_shape(ty: &Type) -> Option<Shape> {
    match ty {
        Type::Reference(r) => type_inner_shape(&r.elem),
        Type::Paren(p) => type_inner_shape(&p.elem),
        Type::Group(g) => type_inner_shape(&g.elem),
        Type::Path(path) => {
            let segment = path.path.segments.last()?;
            let first = generic_types(segment).next()?;
            if SMART_POINTERS.contains(&segment.ident.to_string().as_str()) {
                type_inner_shape(first)
            } else {
                type_shape(first)
            }
        }
        _ => None,
    }
}

/// Methods on an `Option` or a `Result` that give its part.
const UNWRAP_METHODS: &[&str] = &[
    "unwrap",
    "expect",
    "unwrap_unchecked",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
];

/// Methods that store an argument in their receiver as a part.
const STORE_METHODS: &[&str] = &[
    "push",
    "push_back",
    "push_front",
    "insert",
    "extend",
    "append",
    "resize",
    "resize_with",
    "replace",
    "get_or_insert",
    "get_or_insert_with",
    "or_insert",
    "or_insert_with",
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
/// A `?` that leaves the function needs no rule. Its exit costs no more than
/// the path that falls through, so the fall-through path always covers it. A
/// `?` in a `try` block lands after the block, so it is an exit.
struct Flow {
    /// Paths that reach the next statement.
    fall: Option<Cost>,
    /// Paths that leave by `break` or `continue`, by target.
    exits: Vec<(Exit, Cost)>,
    /// Paths that leave by `return`.
    ret: Option<Cost>,
}

/// The exit label of a failed `?` in a `try` block. No real label can
/// have it: `try` is a keyword.
const TRY_EXIT: &str = "try";

/// Where a `break` or `continue` goes.
#[derive(Clone, PartialEq, Eq)]
struct Exit {
    /// The target label. `None`: the nearest loop.
    label: Option<String>,
    /// `break` ends the target. `continue` starts its next pass.
    breaks: bool,
}

impl Flow {
    const ZERO: Self = Self::cost(Cost::ZERO);
    const NEVER: Self = Self {
        fall: None,
        exits: Vec::new(),
        ret: None,
    };
    const RETURN: Self = Self {
        fall: None,
        exits: Vec::new(),
        ret: Some(Cost::ZERO),
    };

    const fn cost(cost: Cost) -> Self {
        Self {
            fall: Some(cost),
            exits: Vec::new(),
            ret: None,
        }
    }

    /// A `break` or `continue` to the nearest loop or to `label`.
    fn exit_to(label: Option<&syn::Lifetime>, breaks: bool) -> Self {
        let exit = Exit {
            label: label.map(|l| l.ident.to_string()),
            breaks,
        };
        Self {
            exits: vec![(exit, Cost::ZERO)],
            ..Self::NEVER
        }
    }

    /// A failed `?` to the nearest `try` block.
    fn exit_to_try() -> Self {
        let exit = Exit {
            label: Some(TRY_EXIT.to_string()),
            breaks: true,
        };
        Self {
            exits: vec![(exit, Cost::ZERO)],
            ..Self::NEVER
        }
    }

    const fn has_path(&self) -> bool {
        self.fall.is_some() || self.ret.is_some() || !self.exits.is_empty()
    }

    /// `self`, then `next` on the paths that fall through.
    fn then(self, next: Self) -> Self {
        let Some(fall) = self.fall else {
            return self;
        };
        // A `next` with no path at all (`match never {}`) ends the run here.
        // The cost so far was still paid.
        if !next.has_path() {
            return Self {
                fall: None,
                ret: worst(self.ret, Some(fall)),
                ..self
            };
        }
        let after = |cost: Option<Cost>| cost.map(|c| fall.clone().then(c));
        let exits = next
            .exits
            .into_iter()
            .map(|(exit, cost)| (exit, fall.clone().then(cost)))
            .collect();
        Self {
            exits: merge_exits(self.exits, exits),
            ret: worst(self.ret, after(next.ret)),
            fall: after(next.fall),
        }
    }

    /// Only one of `self` and `other` runs.
    fn or_worst(self, other: Self) -> Self {
        Self {
            fall: worst(self.fall, other.fall),
            exits: merge_exits(self.exits, other.exits),
            ret: worst(self.ret, other.ret),
        }
    }

    /// The same paths, each costing `cost`.
    fn with_cost(self, cost: &Cost) -> Self {
        let set = |path: Option<Cost>| path.map(|_| cost.clone());
        Self {
            fall: set(self.fall),
            exits: self
                .exits
                .into_iter()
                .map(|(exit, _)| (exit, cost.clone()))
                .collect(),
            ret: set(self.ret),
        }
    }

    /// Remove and return the exits that land on a `try` block.
    fn take_try_exits(&mut self) -> Vec<(Exit, Cost)> {
        let (taken, kept) = std::mem::take(&mut self.exits)
            .into_iter()
            .partition(|(exit, _)| exit.label.as_deref() == Some(TRY_EXIT));
        self.exits = kept;
        taken
    }

    /// Remove and return the exits that land on a target: a loop takes its
    /// unlabeled exits and its own label; a labeled block takes its label.
    fn take_exits(&mut self, label: Option<&syn::Label>, unlabeled: bool) -> Vec<(Exit, Cost)> {
        let own = label.map(|l| l.name.ident.to_string());
        let (taken, kept) = std::mem::take(&mut self.exits)
            .into_iter()
            .partition(|(exit, _)| {
                exit.label
                    .as_ref()
                    .map_or(unlabeled, |name| own.as_ref() == Some(name))
            });
        self.exits = kept;
        taken
    }

    /// The worst path, wherever it goes.
    fn total(self) -> Cost {
        let exits = worst_of(self.exits);
        worst(worst(self.fall, self.ret), exits).unwrap_or(Cost::ZERO)
    }
}

/// The worst cost of a list of exits.
fn worst_of(exits: Vec<(Exit, Cost)>) -> Option<Cost> {
    exits
        .into_iter()
        .fold(None, |acc, (_, cost)| worst(acc, Some(cost)))
}

/// Join two lists of exits, taking the worse cost per target.
fn merge_exits(mut into: Vec<(Exit, Cost)>, from: Vec<(Exit, Cost)>) -> Vec<(Exit, Cost)> {
    for (exit, cost) in from {
        match into.iter_mut().find(|(e, _)| *e == exit) {
            Some(slot) => slot.1 = slot.1.clone().or_worst(cost),
            None => into.push((exit, cost)),
        }
    }
    into
}

// ── Binding environment ──────────────────────────────────────────────

/// What a name holds. The order is the join: after a branch, a name holds the
/// greatest kind it holds on any path. `LazyDb` is above `Handle` because all
/// paths share one type, so one `LazyDb` path makes the name a `LazyDb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// No database handle.
    Plain,
    /// A standard container that holds handles: `[repo]`, `Some(repo)`,
    /// `Vec<PgPostRepository>`. A known container method on it is not a
    /// query. Its parts are handles (see `Analyzer::expr_is_carrier`).
    Carrier,
    /// A user value that holds a handle: `Ctx { repo }`. Every method on it
    /// is reported. A part is what its literal recorded, or else `Nested`.
    Holder,
    /// A database handle.
    Handle,
    /// A `LazyDb`: a handle whose `checkout` is not a query.
    LazyDb,
    /// A value that holds handles at an unknown depth: a container of
    /// containers (`Vec<Vec<Repo>>`) or of user values (`[ctx]`). Every
    /// method on it is reported, and its parts are `Nested` too. It is the
    /// top of the order, so a join with anything stays this careful.
    Nested,
}

impl Kind {
    /// What one part of a value of this kind holds.
    const fn element(self) -> Self {
        match self {
            Self::Plain => Self::Plain,
            Self::Carrier | Self::Handle => Self::Handle,
            Self::LazyDb => Self::LazyDb,
            // A part of a user value may itself be a container.
            Self::Holder | Self::Nested => Self::Nested,
        }
    }

    const fn is_handle(self) -> bool {
        matches!(self, Self::Handle | Self::LazyDb)
    }
}

/// What a name holds, and, for a struct or tuple literal, what each part
/// holds.
// Each flag is a separate fact about the value, and each joins its own way.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    kind: Kind,
    /// Each part's name and kind, when the value was a struct or tuple
    /// literal. `None`: every part of a carrier is a handle.
    parts: Option<Vec<(String, Kind)>>,
    /// The container shape, for a carrier, when it is known.
    shape: Option<Shape>,
    /// The names a `&mut` binding may point to, sorted: `slot` in `let slot
    /// = &mut repos;` points to `repos`. After a branch it may point to more
    /// than one.
    referents: Vec<String>,
    /// The plain result of a known container method on handles. No std
    /// method of this kind gives a future, so `.await` on it is reported.
    container_result: bool,
    /// Each referent is borrowed whole (`&mut slot`, not `&mut slot.0`),
    /// so a write through the binding replaces it.
    whole: bool,
    /// A known future: `.await` on it runs what it names. A name bound to
    /// anything else that holds a handle (`PgPostRepository::new(&mut db)`,
    /// an `async fn new`) is reported when it is awaited.
    future: bool,
    /// What `.await` on an `async` block gives: `let pending = async {
    /// &repo };`.
    output: Option<Kind>,
    /// A builder chain on a handle, not awaited yet: `let pending =
    /// repo.scoped();`. A finder may share the builder's name, so
    /// `pending.await` counts one query.
    pending_query: bool,
    /// The shape of a part, when known: a part of an `Option<Db>` is a `Db`.
    inner: Option<Shape>,
    /// What the declared type gives: `let mut repo: PgPostRepository = …;`.
    /// An assignment to the name keeps it.
    declared: Option<Box<Self>>,
}

impl Binding {
    const fn of(kind: Kind) -> Self {
        Self {
            kind,
            parts: None,
            shape: None,
            referents: Vec::new(),
            container_result: false,
            whole: false,
            future: false,
            output: None,
            pending_query: false,
            inner: None,
            declared: None,
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
        let shape = match (self.shape, other.shape) {
            (a, b) if a == b => a,
            // The shape with fewer known methods: both sides have it.
            (Some(Shape::Opt | Shape::OptRef), Some(Shape::Opt | Shape::OptRef)) => {
                Some(Shape::Opt)
            }
            (Some(Shape::Iter | Shape::IterRef), Some(Shape::Iter | Shape::IterRef)) => {
                Some(Shape::Iter)
            }
            (None, b) if self.kind == Kind::Plain => b,
            (a, None) if other.kind == Kind::Plain => a,
            _ => None,
        };
        Self {
            kind: self.kind.max(other.kind),
            parts,
            shape,
            referents: {
                let mut all = self.referents.clone();
                all.extend(other.referents.iter().cloned());
                all.sort();
                all.dedup();
                all
            },
            container_result: self.container_result || other.container_result,
            whole: self.whole && other.whole,
            future: self.future && other.future,
            output: self.output.max(other.output),
            pending_query: self.pending_query || other.pending_query,
            inner: if self.inner == other.inner {
                self.inner
            } else {
                None
            },
            declared: self.declared.clone().or_else(|| other.declared.clone()),
        }
    }
}

/// Lexical scopes, innermost last. Each scope maps a name to its [`Binding`].
#[derive(Clone)]
struct Env {
    scopes: Vec<HashMap<String, Binding>>,
    /// The bindings of the scopes that closed during each statement being
    /// read, outermost first. A value is often read after its scope closes:
    /// `let alias = { let moved = repo; moved };` reads `moved` after the
    /// block. A lookup joins these bindings in. A shadowed name may then
    /// hold more than it does, never less. Every copy shares them, so a
    /// branch that restores a copy keeps them.
    closed: Rc<RefCell<Vec<HashMap<String, Binding>>>>,
}

impl PartialEq for Env {
    fn eq(&self, other: &Self) -> bool {
        self.scopes == other.scopes
    }
}

impl Eq for Env {}

impl Env {
    fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
            closed: Rc::default(),
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Close the innermost scope, and keep its bindings for the statement.
    fn pop(&mut self) {
        let Some(scope) = self.scopes.pop() else {
            return;
        };
        if let Some(record) = self.closed.borrow_mut().last_mut() {
            for (name, binding) in scope {
                let joined = record
                    .get(&name)
                    .map_or_else(|| binding.clone(), |b| b.join(&binding));
                record.insert(name, joined);
            }
        }
    }

    /// Start or end a statement's record of closed scopes.
    fn open_statement(&self) {
        self.closed.borrow_mut().push(HashMap::new());
    }

    fn close_statement(&self) {
        self.closed.borrow_mut().pop();
    }

    const fn depth(&self) -> usize {
        self.scopes.len()
    }

    /// The binding of `name`, looking from scope `top` outwards, joined
    /// with its bindings in the closed scopes.
    fn binding_from(&self, top: usize, name: &str) -> Binding {
        self.closed
            .borrow()
            .iter()
            .filter_map(|record| record.get(name))
            .fold(self.open_binding_from(top, name), |b, c| b.join(c))
    }

    /// [`Self::binding_from`] in the open scopes only.
    fn open_binding_from(&self, top: usize, name: &str) -> Binding {
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
        self.binding(name).kind
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

    /// Record the container shape of `name`, as a type annotation gives it.
    fn set_shape(&mut self, name: &str, shape: Option<Shape>) {
        let at = self.home(name);
        if let Some(binding) = self.scopes[at].get_mut(name) {
            binding.shape = shape;
        }
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
        // With every part known, the value holds what its parts hold.
        if let Some(parts) = &binding.parts
            && matches!(binding.kind, Kind::Plain | Kind::Carrier | Kind::Holder)
            && parts.iter().all(|(_, k)| *k == Kind::Plain)
        {
            binding.kind = Kind::Plain;
            self.assign(name.to_string(), binding);
            return;
        }
        if kind != Kind::Plain {
            // A tuple stays a standard container; anything else may be a
            // user value.
            let held = if binding.kind == Kind::Carrier {
                Kind::Carrier
            } else {
                Kind::Holder
            };
            binding.kind = binding.kind.max(held);
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
                    .open_binding_from(depth, &name)
                    .join(&other.open_binding_from(depth, &name));
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
    /// A loop. An unlabeled `break` or `continue`, or one with its label,
    /// lands here.
    Loop,
    /// A labeled block. A `break` with its label lands here.
    Block,
    /// A `try` block. A `?` lands here, and `return` crosses it.
    Try,
}

/// The cost of one loop pass, split by where it goes next. `None`: no path.
struct Pass {
    /// Paths that start the next pass.
    again: Option<Cost>,
    /// Paths that leave the loop.
    leave: Option<Cost>,
}

/// The cost of the paths that end a loop and reach the next statement: every
/// pass goes on until the loop ends, or a pass `break`s out of it. A
/// `return` or an exit to an outer label does not reach the next statement.
fn loop_fall(
    again: Option<Cost>,
    brk: Option<Cost>,
    exhausts: bool,
    bound: Option<u32>,
    total: &Cost,
) -> Cost {
    if matches!(total, Cost::Unbounded(_)) {
        return total.clone();
    }
    let again = again.unwrap_or(Cost::ZERO);
    // With no bound, `again` is zero here: else `total` is unbounded.
    let times = bound.unwrap_or(1);
    let ended = exhausts.then(|| again.clone().repeated(times));
    let broke = brk.map(|b| again.clone().repeated(times.saturating_sub(1)).then(b));
    worst(ended, broke).unwrap_or(Cost::ZERO)
}

/// What a loop is, for [`Analyzer::loop_flow`].
struct LoopShape<'a> {
    /// The compile-time number of passes, when there is one.
    bound: Option<u32>,
    span: Span,
    label: Option<&'a syn::Label>,
    /// A `for` or `while` can end without an exit; a `loop` cannot.
    ends: bool,
}

/// Where an exit lands, and the bindings at the exits that land there.
struct ExitFrame {
    target: Target,
    /// The frame's label, without the `'`.
    label: Option<String>,
    /// The scope depth when the frame opened.
    depth: usize,
    /// The join of the bindings at every exit that landed here.
    env: Option<Env>,
    /// The same for a `continue`: it starts the next pass, not the code
    /// after the loop.
    continues: Option<Env>,
}

impl ExitFrame {
    fn record(&mut self, env: &Env, continues: bool) {
        let env = Env {
            scopes: env.scopes[..self.depth].to_vec(),
            closed: Rc::clone(&env.closed),
        };
        let slot = if continues {
            &mut self.continues
        } else {
            &mut self.env
        };
        match slot {
            Some(joined) => joined.join(&env),
            None => *slot = Some(env),
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
    /// What every `return` read so far gives. A closure probe reads it.
    returned: Kind,
    /// The next closure body's parameters are connections: it is a
    /// transaction callback.
    connection_params: bool,
    /// What the next closure body's parameters borrow into: the places the
    /// method's receiver borrows.
    param_referents: Vec<String>,
    /// The same, one list per parameter: an invoked closure's parameter
    /// borrows what its own argument borrows.
    param_referents_each: Vec<Vec<String>>,
    /// Names the handler body defines or imports (`macro_rules! vec`, `fn
    /// drop`, `use x::format`). A std name among them is not trusted.
    shadowed: Rc<Vec<String>>,
}

impl Analyzer {
    /// An analyzer with the handler's parameters bound.
    fn new(input_fn: &ItemFn) -> Self {
        let mut analyzer = Self {
            env: Env::new(),
            exits: Vec::new(),
            ledger: Vec::new(),
            errors: Vec::new(),
            returned: Kind::Plain,
            connection_params: false,
            param_referents: Vec::new(),
            param_referents_each: Vec::new(),
            shadowed: Rc::new(Vec::new()),
        };
        // The signature reads names from outside the body, so the body's
        // items do not shadow its types.
        for arg in &input_fn.sig.inputs {
            if let syn::FnArg::Typed(typed) = arg {
                analyzer.bind_typed(&typed.pat, &typed.ty, Kind::Plain);
            }
        }
        analyzer.shadowed = Rc::new(local_names(&input_fn.block));
        analyzer
    }

    /// The cost of the handler's body.
    fn function_body(&mut self, block: &Block) -> Cost {
        self.framed(Target::Body, None, |s| s.block(block).total())
    }

    fn count(&mut self, what: &str) -> Cost {
        self.ledger.push(format!("`{what}`"));
        Cost::Exact(1)
    }

    // ── Bindings ─────────────────────────────────────────────────────

    /// Record what the declared type `ty` of `name` tells: its shape, the
    /// shape of its part (`Db` in `Option<Db>`), and each side of a `Result`
    /// (`Err(e)` on a `Result<Repo, Error>` is not a handle).
    fn record_type(&mut self, name: String, ty: &Type) {
        // A type that names an item of the body may not be the std type:
        // only its kind, which only raises, is kept.
        let trusted = !self.type_is_shadowed(ty);
        let shape = if trusted { type_shape(ty) } else { None };
        self.env.set_shape(&name, shape);
        let mut binding = self.env.binding(&name);
        binding.inner = if trusted { type_inner_shape(ty) } else { None };
        if let Some(sides) = result_sides(ty).filter(|_| trusted) {
            binding.parts = Some(sides);
        }
        binding.declared = Some(Box::new(Binding {
            kind: type_kind(ty),
            parts: binding.parts.clone(),
            shape,
            inner: binding.inner,
            ..Binding::of(Kind::Plain)
        }));
        self.env.declare(name, binding);
    }

    /// Bind `pat`, declared as `ty`, to a value that holds `kind`. A tuple
    /// pattern pairs each part with its own type: in `let (db, n): (Db,
    /// i64)`, `n` is plain.
    fn bind_typed(&mut self, pat: &Pat, ty: &Type, kind: Kind) {
        if let (Pat::Tuple(p), Type::Tuple(t)) = (pat, ty)
            && p.elems.len() == t.elems.len()
            && !p.elems.iter().any(|e| matches!(e, Pat::Rest(_)))
        {
            for (part, part_ty) in p.elems.iter().zip(&t.elems) {
                self.bind_typed(part, part_ty, kind.element());
            }
            return;
        }
        // rustc checks the annotation. A type made only of standard and
        // primitive types cannot hold a handle.
        if type_is_plain_std(ty) && !self.type_is_shadowed(ty) {
            self.bind_pat(pat, Kind::Plain);
            if let Pat::Ident(id) = pat {
                self.env.set_shape(&id.ident.to_string(), type_shape(ty));
            }
            return;
        }
        self.bind_pat(pat, kind.max(type_kind(ty)));
        if let Pat::Ident(id) = pat {
            self.record_type(id.ident.to_string(), ty);
        }
    }

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
            Pat::Type(p) => self.bind_typed(&p.pat, &p.ty, kind),
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
                    // `tail @ ..` binds a subslice, not one element.
                    let rest = matches!(elem, Pat::Ident(id)
                        if matches!(id.subpat.as_ref().map(|(_, p)| &**p), Some(Pat::Rest(_))));
                    self.bind_pat(elem, if rest { kind } else { kind.element() });
                }
            }
            Pat::TupleStruct(p) => {
                // A `Result` is a handle only when its `Ok` side is one, so
                // `Err(e)` on it binds the error. On a carrier, such as a
                // `Result<(), Db>`, `Err(e)` may be the handle. Only the std
                // `Err`: a handle type may have its own `Err` variant.
                let is_err = pattern_variant(pat, &self.shadowed)
                    .is_some_and(|(_, variant)| variant == "Err");
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
        self.bind_init_parts(pat, init);
        // `let target: &mut Vec<Db> = &mut list;` is an alias, as without
        // the type.
        if let Pat::Type(typed) = pat
            && let Pat::Ident(id) = &*typed.pat
        {
            let name = id.ident.to_string();
            let targets = self.referents_of(init);
            self.add_referents(name.clone(), &targets);
            if !targets.is_empty() && self.borrows_whole(init) {
                let mut binding = self.env.binding(&name);
                binding.whole = true;
                self.env.declare(name, binding);
            }
        }
        // `let (left, _) = slots.split_at_mut(1);`: each name may borrow
        // into what the value borrows.
        let single = match pat {
            Pat::Type(t) => matches!(&*t.pat, Pat::Ident(_)),
            other => matches!(other, Pat::Ident(_)),
        };
        if !single {
            let targets = self.referents_of(init);
            if !targets.is_empty() {
                for name in bound_names(pat) {
                    self.add_referents(name, &targets);
                }
            }
        }
        // `let Holder(ref mut bucket) = holder;`: a store into `bucket` is a
        // store into `holder`.
        let names = ref_mut_names(pat);
        if names.is_empty() {
            return;
        }
        let borrow: Expr = syn::parse_quote!(&mut #init);
        let targets = self.referents_of(&borrow);
        for name in names {
            self.add_referents(name, &targets);
        }
    }

    /// Record that `name` may point to each of `targets`.
    fn add_referents(&mut self, name: String, targets: &[String]) {
        if targets.is_empty() {
            return;
        }
        let mut binding = self.env.binding(&name);
        for target in targets {
            if !binding.referents.contains(target) {
                binding.referents.push(target.clone());
            }
        }
        binding.referents.sort();
        self.env.declare(name, binding);
    }

    fn bind_init_parts(&mut self, pat: &Pat, init: &Expr) {
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
            // `let Ctx { repos } = Ctx { repos: vec![repo] };`: field by field.
            (Pat::Struct(p), Expr::Struct(e)) => {
                let rest = self.value_of(init).element();
                for field in &p.fields {
                    let name = member_name(&field.member);
                    match e.fields.iter().find(|f| member_name(&f.member) == name) {
                        Some(value) => self.bind_init(&field.pat, &value.expr),
                        None => self.bind_pat(&field.pat, rest),
                    }
                }
            }
            // A struct pattern over a name with recorded parts.
            (Pat::Struct(p), _) if path_ident(init).is_some() => {
                let name = path_ident(init).unwrap_or_default();
                let rest = self.value_of(init).element();
                for field in &p.fields {
                    let kind = self
                        .env
                        .part(&name, &member_name(&field.member))
                        .unwrap_or(rest);
                    self.bind_pat(&field.pat, kind);
                }
            }
            // `Ok(x)` / `Err(e)` over a `Result` with recorded sides:
            // `result`, `&result`, `result.as_ref()`.
            (Pat::TupleStruct(p), _)
                if p.path
                    .segments
                    .last()
                    .is_some_and(|s| self.side_kind(init, &s.ident.to_string()).is_some()) =>
            {
                let side = p
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();
                let kind = self.side_kind(init, &side).unwrap_or(Kind::Nested);
                for elem in &p.elems {
                    self.bind_pat(elem, kind);
                }
            }
            // A tuple pattern over a name with recorded parts, with no `..`.
            (Pat::Tuple(p), _)
                if path_ident(init).is_some()
                    && !p.elems.iter().any(|e| matches!(e, Pat::Rest(_))) =>
            {
                let name = path_ident(init).unwrap_or_default();
                let rest = self.value_of(init).element();
                for (i, elem) in p.elems.iter().enumerate() {
                    let kind = self.env.part(&name, &i.to_string()).unwrap_or(rest);
                    self.bind_pat(elem, kind);
                }
            }
            (Pat::Paren(p), _) => self.bind_init(&p.pat, init),
            (_, Expr::Paren(e)) => self.bind_init(pat, &e.expr),
            // `let ref mut alias = repos;` is `let alias = &mut repos;`.
            (Pat::Ident(p), _)
                if p.subpat.is_none() && p.by_ref.is_some() && p.mutability.is_some() =>
            {
                let borrow: Expr = syn::parse_quote!(&mut #init);
                let binding = self.binding_of(&borrow);
                self.env.declare(p.ident.to_string(), binding);
            }
            // `Ok::<(), PgPostRepository>(())` is typed like a `let` with
            // `Result<(), PgPostRepository>`, and `Vec::<Db>::new()` like one
            // with `Vec<Db>`.
            (Pat::Ident(p), _) if p.subpat.is_none() && constructor_type(init).is_some() => {
                if let Some(ty) = constructor_type(init) {
                    let kind = self.value_of(init);
                    self.bind_typed(pat, &ty, kind);
                }
            }
            (Pat::Ident(p), _) if p.subpat.is_none() => {
                let binding = self.binding_of(init);
                self.env.declare(p.ident.to_string(), binding);
            }
            _ => {
                let kind = self.value_of(init);
                self.bind_pat(pat, kind);
                // `let h: Holder = Holder { inner: &repo };` keeps the parts
                // of the literal, unless the type proves it plain.
                if let Pat::Type(t) = pat
                    && let Pat::Ident(id) = &*t.pat
                    && id.subpat.is_none()
                {
                    let name = id.ident.to_string();
                    let mut binding = self.env.binding(&name);
                    if binding.kind != Kind::Plain && binding.parts.is_none() {
                        binding.parts = self.binding_of(init).parts;
                        self.env.declare(name, binding);
                    }
                }
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
            // `let alias = result;` or `result.as_ref()` keeps the recorded
            // parts.
            other => match self.sides_of(other) {
                Some((ok, err)) => Some(vec![("Ok".to_string(), ok), ("Err".to_string(), err)]),
                None => wrapper_root(other).and_then(|name| self.env.binding(&name).parts),
            },
        };
        Binding {
            kind,
            parts,
            shape: self.shape_of(init),
            referents: self.referents_of(init),
            container_result: self.is_container_result(init),
            whole: self.borrows_whole(init),
            future: self.is_known_future(init),
            pending_query: self.is_pending_query(init),
            output: self.output_of(init),
            inner: wrapper_root(init).and_then(|name| self.env.binding(&name).inner),
            declared: None,
        }
    }

    /// Is `e` a known future: a query or accessor on a handle
    /// (`repo.find_all()`, `ctx.conn()`), a call that is not a handle
    /// constructor, an `async` block, a macro, or a name bound to one?
    fn is_known_future(&self, e: &Expr) -> bool {
        let e = peel_parens(e);
        match e {
            Expr::MethodCall(mc) => {
                let method = mc.method.to_string();
                self.chain_root_is_handle(e)
                    || HANDLE_ACCESSORS.contains(&method.as_str())
                    || HANDLE_TRANSITIONS.contains(&method.as_str())
            }
            // A constructor is not a future (`Some(&repo)`, `Arc::new(&repo)`).
            // A free function (`drop`) is not one either.
            Expr::Call(call) => {
                !(is_handle_constructor(call)
                    || self.is_container_constructor(call)
                    || self.is_smart_pointer_new(call)
                    || self.is_std_free_fn(call))
            }
            // A path that diverges gives no value.
            Expr::Async(_)
            | Expr::Macro(_)
            | Expr::Return(_)
            | Expr::Break(_)
            | Expr::Continue(_) => true,
            // A loop or labeled block gives its `break` values too. Unlike an
            // `if`, it cannot charge a builder on its own path, so a mix of
            // builders and other futures is not known.
            Expr::Loop(_)
            | Expr::Unsafe(_)
            | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                let tails = value_tails(e);
                let pending = tails.iter().filter(|t| self.is_pending_query(t)).count();
                (pending == 0 || pending == tails.len())
                    && tails.iter().all(|t| self.is_known_future(t))
            }
            Expr::If(_) | Expr::Match(_) | Expr::Block(_) => {
                branch_tails(e).is_some_and(|tails| tails.iter().all(|t| self.is_known_future(t)))
            }
            other => path_ident(other).is_some_and(|name| self.env.binding(&name).future),
        }
    }

    /// Is `e` a builder chain on a handle (`repo.scoped()`), or a name bound
    /// to one?
    fn is_pending_query(&self, e: &Expr) -> bool {
        let e = peel_parens(e);
        match e {
            Expr::MethodCall(mc) => {
                is_handle_builder(&mc.method.to_string()) && self.chain_root_is_handle(e)
            }
            Expr::Loop(_)
            | Expr::Unsafe(_)
            | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                let tails = value_tails(e);
                !tails.is_empty() && tails.iter().all(|t| self.is_pending_query(t))
            }
            Expr::If(_) | Expr::Match(_) | Expr::Block(_) => {
                branch_tails(e).is_some_and(|tails| tails.iter().all(|t| self.is_pending_query(t)))
            }
            other => path_ident(other).is_some_and(|name| self.env.binding(&name).pending_query),
        }
    }

    /// What `.await` on `async { block }` gives.
    fn async_output(&self, block: &Block) -> Kind {
        let mut probe = self.probe();
        let tail = probe.block_value(block);
        tail.max(probe.returned)
    }

    /// The names that `init` may point to when it is a `&mut` place or a
    /// `&mut` binding: `&mut repos`, `&mut *slot`, `slot`.
    fn referents_of(&self, init: &Expr) -> Vec<String> {
        match init {
            Expr::Paren(p) => self.referents_of(&p.expr),
            Expr::Group(g) => self.referents_of(&g.expr),
            Expr::Reference(r) if r.mutability.is_some() => self.place_referents(&r.expr),
            // `addr_of_mut!(slot)`, read like `&raw mut slot`. Any macro of
            // that name: a borrow only adds to what is reported.
            Expr::Macro(m)
                if m.mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "addr_of_mut") =>
            {
                m.mac
                    .parse_body::<Expr>()
                    .map(|place| self.place_referents(&place))
                    .unwrap_or_default()
            }
            // `async { &mut slot }.await`: what the block gives, by its tail
            // or a `return`, or any borrow made in it (a local may pass it
            // on).
            Expr::Async(a) => self.block_borrows(&a.block),
            // `move || alias`: what the body names or borrows.
            Expr::Closure(c) => self.closure_borrows(c),
            // `vec![&raw mut slot]`, or any macro whose body reads as
            // expressions: a borrow in it only adds to what is reported.
            Expr::Macro(m) => {
                let mut all: Vec<String> = macro_exprs(&m.mac)
                    .iter()
                    .flat_map(|e| self.referents_of(e))
                    .collect();
                all.sort();
                all.dedup();
                all
            }
            // `&raw mut slot`: a write through the pointer reaches `slot`.
            Expr::RawAddr(r) if matches!(r.mutability, syn::PointerMutability::Mut(_)) => {
                self.place_referents(&r.expr)
            }
            Expr::Path(_) => path_ident(init)
                .map(|name| self.env.binding(&name).referents)
                .unwrap_or_default(),
            Expr::Cast(c) => self.referents_of(&c.expr),
            // `*boxed` on a `Box<&mut slot>` moves out the borrow.
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => self.referents_of(&u.expr),
            // `pick(&mut slots)?`, `pick(&mut slots).await`.
            Expr::Try(t) => self.referents_of(&t.expr),
            Expr::Await(a) => self.referents_of(&a.base),
            // `refs.0`, `refs[i]`: a part may be any borrow of the whole.
            Expr::Field(f) => self.referents_of(&f.base),
            Expr::Index(i) => self.referents_of(&i.expr),
            // `(&mut left, 1)`, `[&mut left, &mut right]`, a range or a
            // repeat: a part may borrow.
            Expr::Tuple(_)
            | Expr::Array(_)
            | Expr::Struct(_)
            | Expr::Range(_)
            | Expr::Repeat(_) => {
                let parts: Vec<&Expr> = match init {
                    Expr::Tuple(t) => t.elems.iter().collect(),
                    Expr::Array(a) => a.elems.iter().collect(),
                    // `(&mut a)..(&mut b)`, `[&raw mut a; 2]`.
                    Expr::Range(r) => [&r.start, &r.end]
                        .into_iter()
                        .flatten()
                        .map(|e| &**e)
                        .collect(),
                    Expr::Repeat(r) => vec![&*r.expr],
                    // `Ctx { n: 1, ..base }` keeps what `base` borrows.
                    Expr::Struct(st) => st
                        .fields
                        .iter()
                        .map(|f| &f.expr)
                        .chain(st.rest.as_deref())
                        .collect(),
                    _ => Vec::new(),
                };
                let mut all: Vec<String> = parts
                    .into_iter()
                    .flat_map(|p| self.referents_of(p))
                    .collect();
                all.sort();
                all.dedup();
                all
            }
            // `pick(&mut left)`, `(|x| x)(&mut left)`, `Some(&mut left)`: the
            // result may borrow any place a `&mut` argument points to.
            Expr::Call(c) => {
                // `make()` on a closure that captures a borrow may give it.
                let mut all: Vec<String> = c
                    .args
                    .iter()
                    .chain(std::iter::once(&*c.func))
                    .flat_map(|a| self.referents_of(a))
                    .collect();
                all.sort();
                all.dedup();
                all
            }
            // `left.get_mut(0).unwrap()`, `left.as_mut()`: a part of the
            // receiver.
            Expr::MethodCall(mc) => {
                // `picker.pick(&mut left)`: like a call, the result may borrow
                // any place a `&mut` argument points to.
                let mut all: Vec<String> =
                    mc.args.iter().flat_map(|a| self.referents_of(a)).collect();
                all.extend(self.receiver_referents(mc));
                all.sort();
                all.dedup();
                all
            }
            // `if flag { &mut left } else { &mut right }`: every tail, and
            // every `break` value of a loop or labeled block.
            Expr::If(_) | Expr::Match(_) | Expr::Block(_) | Expr::Loop(_) | Expr::Unsafe(_) => {
                let mut all: Vec<String> = value_tails(init)
                    .into_iter()
                    .flat_map(|tail| self.referents_of(tail))
                    .collect();
                all.sort();
                all.dedup();
                all
            }
            _ => Vec::new(),
        }
    }

    /// Every name that a closure body may point to: through a name it
    /// captures, or a borrow it makes.
    fn closure_borrows(&self, c: &syn::ExprClosure) -> Vec<String> {
        struct Parts<'a>(Vec<&'a Expr>);
        impl<'a> Visit<'a> for Parts<'a> {
            fn visit_expr(&mut self, e: &'a Expr) {
                if matches!(
                    e,
                    Expr::Path(_)
                        | Expr::Reference(_)
                        | Expr::RawAddr(_)
                        | Expr::MethodCall(_)
                        | Expr::Macro(_)
                ) {
                    self.0.push(e);
                }
                syn::visit::visit_expr(self, e);
            }
        }
        let mut parts = Parts(Vec::new());
        parts.visit_expr(&c.body);
        let mut all: Vec<String> = parts
            .0
            .into_iter()
            .flat_map(|e| self.referents_of(e))
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// Every name that a `&mut`, a `&raw mut` or a `_mut` method in `block`
    /// may point to, with what its tail points to.
    fn block_borrows(&self, block: &Block) -> Vec<String> {
        struct Borrows<'a>(Vec<&'a Expr>);
        impl<'a> Visit<'a> for Borrows<'a> {
            fn visit_expr(&mut self, e: &'a Expr) {
                if matches!(
                    e,
                    Expr::Reference(_) | Expr::RawAddr(_) | Expr::MethodCall(_) | Expr::Macro(_)
                ) {
                    self.0.push(e);
                }
                syn::visit::visit_expr(self, e);
            }
        }
        let mut borrows = Borrows(Vec::new());
        borrows.visit_block(block);
        let mut all: Vec<String> = borrows
            .0
            .into_iter()
            .chain(block_tail(block))
            .flat_map(|e| self.referents_of(e))
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// The names that a `&mut` or `&raw mut` of `place` points to.
    fn place_referents(&self, place: &Expr) -> Vec<String> {
        // `&mut *slots.get_mut(0).unwrap()`: a reborrow of a temporary
        // borrows what the temporary borrows.
        let Some(root) = place_root(place) else {
            return self.referents_of(place_base(place));
        };
        let through = self.env.binding(&root).referents;
        if through.is_empty() {
            vec![root]
        } else {
            through
        }
    }

    /// What the result of `mc` may borrow through its receiver.
    fn receiver_referents(&self, mc: &ExprMethodCall) -> Vec<String> {
        let method = mc.method.to_string();
        if matches!(method.as_str(), "unwrap" | "expect") {
            self.referents_of(&mc.receiver)
        } else if method == "as_mut"
            || method.ends_with("_mut")
            || BORROW_MUT_METHODS.contains(&method.as_str())
        {
            // `groups.entry(k).or_default()`: through the chain.
            if matches!(peel_parens(&mc.receiver), Expr::MethodCall(_)) {
                return self.referents_of(&mc.receiver);
            }
            place_root(&mc.receiver)
                .map(|root| {
                    let through = self.env.binding(&root).referents;
                    if through.is_empty() {
                        vec![root]
                    } else {
                        through
                    }
                })
                .unwrap_or_default()
        } else if matches!(
            peel_parens(&mc.receiver),
            Expr::MethodCall(_) | Expr::Path(_)
        ) {
            // `slots.iter_mut().next()`, `it.next()`: a method on a borrow
            // may give a borrow into it.
            self.referents_of(&mc.receiver)
        } else {
            Vec::new()
        }
    }

    /// Does `init` borrow a whole name (`&mut slot`), or name a binding that
    /// does?
    fn borrows_whole(&self, init: &Expr) -> bool {
        match peel_parens(init) {
            Expr::Reference(r) if r.mutability.is_some() => path_ident(peel_parens(&r.expr))
                .is_some_and(|name| {
                    let binding = self.env.binding(&name);
                    binding.referents.is_empty() || binding.whole
                }),
            // `if flag { &mut left } else { &mut right }`: every tail does.
            other @ (Expr::If(_)
            | Expr::Match(_)
            | Expr::Block(_)
            | Expr::Unsafe(_)
            | Expr::Loop(_)) => {
                let tails = value_tails(other);
                !tails.is_empty() && tails.iter().all(|t| self.borrows_whole(t))
            }
            other => path_ident(other).is_some_and(|name| self.env.binding(&name).whole),
        }
    }

    /// `root` and every name it may point to through `&mut` bindings.
    fn alias_targets(&self, root: String) -> Vec<String> {
        let mut targets = vec![root];
        let mut i = 0;
        while i < targets.len() && targets.len() < 32 {
            for next in self.env.binding(&targets[i]).referents {
                if !targets.contains(&next) {
                    targets.push(next);
                }
            }
            i += 1;
        }
        targets
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
            // `Ctx { repo: slot } = Ctx { repo: Some(repo) }`: field by field.
            // Against any other value, each place takes that field of it.
            (Expr::Struct(p), _) => {
                let literal = match peel_parens(value) {
                    Expr::Struct(v) if v.rest.is_none() && same_tokens(&p.path, &v.path) => Some(v),
                    _ => None,
                };
                for field in &p.fields {
                    let given = literal
                        .and_then(|v| v.fields.iter().find(|f| f.member == field.member))
                        .map(|f| f.expr.clone());
                    let part = given.unwrap_or_else(|| field_of(value, field.member.clone()));
                    self.assign(&field.expr, &part);
                }
            }
            // `Wrapper(slot, _) = w`: each place takes that field.
            (Expr::Call(p), _) => {
                let literal = match peel_parens(value) {
                    Expr::Call(v)
                        if v.args.len() == p.args.len() && same_tokens(&p.func, &v.func) =>
                    {
                        Some(v)
                    }
                    _ => None,
                };
                for (i, place) in p.args.iter().enumerate() {
                    let part = literal.map_or_else(
                        || field_of(value, syn::Member::Unnamed(syn::Index::from(i))),
                        |v| v.args[i].clone(),
                    );
                    self.assign(place, &part);
                }
            }
            // `*target = None;` through an alias of one whole place writes
            // that place. With more places, only one is written, so none
            // clears. An alias of a part (`&mut slot.0`) writes only the part.
            (Expr::Unary(u), _)
                if matches!(u.op, syn::UnOp::Deref(_))
                    && !self.is_opaque_value(value)
                    && path_ident(&u.expr).is_some_and(|alias| {
                        let binding = self.env.binding(&alias);
                        binding.whole && binding.referents.len() == 1
                    }) =>
            {
                let alias = path_ident(&u.expr).unwrap_or_default();
                let target = self.env.binding(&alias).referents.remove(0);
                let binding = self.binding_of(value);
                self.assign_name(target, binding, Some(value));
            }
            (Expr::Path(p), _) if p.path.get_ident().is_some() => {
                let binding = self.binding_of(value);
                if let Some(ident) = p.path.get_ident() {
                    self.assign_name(ident.to_string(), binding, Some(value));
                }
            }
            _ => {
                let kind = self.value_of(value);
                self.assign_kind(place, kind);
            }
        }
    }

    /// `name = …`. A declared type stays a floor: `repo = make();` on a
    /// `PgPostRepository` binding is still a handle.
    fn assign_name(&mut self, name: String, mut binding: Binding, value: Option<&Expr>) {
        let old = self.env.binding(&name);
        // An assignment keeps the name's type. When the new value is opaque
        // (`alias = make_repository();`), it may hold what the old one held.
        // A literal (`active = Vec::new();`) is known, so it clears.
        if binding.kind < old.kind && value.is_none_or(|v| self.is_opaque_value(v)) {
            binding.kind = old.kind;
            binding.parts = None;
            binding.shape = binding.shape.or(old.shape);
        }
        if let Some(declared) = old.declared {
            // A value that shows what it holds (`slot = None;`) clears. An
            // opaque one may hold anything its declared type holds.
            if value.is_none_or(|v| self.is_opaque_value(v)) {
                binding.kind = binding.kind.max(declared.kind);
                if declared.parts.is_some() {
                    binding.parts.clone_from(&declared.parts);
                }
            }
            binding.shape = declared.shape.or(binding.shape);
            binding.inner = declared.inner.or(binding.inner);
            binding.declared = Some(declared);
        }
        self.env.assign(name, binding);
    }

    /// Is `e` a value whose contents the analysis cannot see? Only a value
    /// whose syntax shows what it holds is known: a literal, `None`, a
    /// known constructor, `vec!`, or a tuple, array or struct literal, each
    /// of known values, or a closure. Anything else (a call, an operator, a
    /// name) has the place's type, so it may hold what the old value held.
    fn is_opaque_value(&self, e: &Expr) -> bool {
        let known = |e: &Expr| !self.is_opaque_value(e);
        match peel_parens(e) {
            Expr::Lit(_) | Expr::Closure(_) => false,
            // `None`, `std::option::Option::None`.
            Expr::Path(p) => {
                !(p.qself.is_none()
                    && p.path.segments.last().is_some_and(|s| s.ident == "None")
                    && std_prefix(&p.path))
            }
            Expr::Call(c) => {
                !((self.is_container_constructor(c)
                    || self.is_smart_pointer_new(c)
                    || is_handle_constructor(c)
                    || self.shape_of(e).is_some())
                    && c.args.iter().all(known))
            }
            Expr::Macro(m) => self
                .std_vec(&m.mac)
                .is_none_or(|elems| !elems.iter().all(known)),
            Expr::Tuple(t) => !t.elems.iter().all(known),
            Expr::Array(a) => !a.elems.iter().all(known),
            Expr::Struct(st) => st.rest.is_some() || !st.fields.iter().all(|f| known(&f.expr)),
            // A labeled block's `break` values count too (`'a: { break 'a
            // make(); }`).
            Expr::Loop(_) | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                !value_tails(e).iter().all(|t| known(t))
            }
            Expr::If(_) | Expr::Match(_) | Expr::Block(_) => {
                branch_tails(e).is_none_or(|tails| !tails.iter().all(|t| known(t)))
            }
            _ => true,
        }
    }

    fn assign_kind(&mut self, place: &Expr, kind: Kind) {
        match place {
            Expr::Path(p) => {
                if let Some(ident) = p.path.get_ident() {
                    self.assign_name(ident.to_string(), Binding::of(kind), None);
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
            // `deps.0 = repo`: store into that part. Through `slot = &mut
            // saved`, `slot.value = repo` stores into `saved.value` too.
            Expr::Field(f) if path_ident(&f.base).is_some() => {
                let member = member_name(&f.member);
                if let Some(base) = path_ident(&f.base) {
                    for name in self.alias_targets(base) {
                        self.env.assign_part(&name, &member, kind);
                    }
                }
            }
            // `a.b.c = repo`, `repos[0] = repo`, `*slot = repo`: the name at
            // the root now holds a handle somewhere inside it.
            _ => {
                if kind == Kind::Plain {
                    return;
                }
                // `*target = repo`: `target` holds it. Each place `target`
                // borrows whole holds it too. A place it borrows a part of
                // (`&mut repos[0]`) holds it inside.
                let alias = match place {
                    Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => {
                        path_ident(peel_parens(&u.expr))
                    }
                    _ => None,
                };
                if let Some(alias) = alias {
                    let owner = if self.env.binding(&alias).whole {
                        kind
                    } else {
                        Kind::Holder
                    };
                    for (i, name) in self.alias_targets(alias).into_iter().enumerate() {
                        self.raise_one(name, if i == 0 { kind } else { owner });
                    }
                } else if let Some(root) = place_root(place) {
                    self.raise(root, Kind::Holder);
                } else {
                    // `*slots.get_mut(0).unwrap() = …`,
                    // `ctxs.last_mut().unwrap().repo = …`: a place inside a
                    // temporary borrow. Each place it borrows holds the value
                    // one layer down.
                    for name in self.referents_of(place_base(place)) {
                        self.raise_one(name, Kind::Nested);
                    }
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
    fn framed<T>(
        &mut self,
        target: Target,
        label: Option<&syn::Label>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let (out, frame) = self.in_frame(target, label, f);
        for env in [frame.env, frame.continues].into_iter().flatten() {
            self.env.join(&env);
        }
        out
    }

    /// Run `f` in an exit frame, and give back the frame.
    fn in_frame<T>(
        &mut self,
        target: Target,
        label: Option<&syn::Label>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> (T, ExitFrame) {
        let depth = self.env.depth();
        self.exits.push(ExitFrame {
            target,
            label: label.map(|l| l.name.ident.to_string()),
            depth,
            env: None,
            continues: None,
        });
        let out = f(self);
        let frame = self.exits.pop().unwrap_or(ExitFrame {
            target,
            label: None,
            depth,
            env: None,
            continues: None,
        });
        (out, frame)
    }

    /// Record the bindings at a `break` or `continue` in the frame it
    /// targets: the nearest loop, or the frame with its label.
    fn exit_loop(&mut self, label: Option<&syn::Lifetime>, continues: bool) {
        let label = label.map(|l| l.ident.to_string());
        for frame in self.exits.iter_mut().rev() {
            if frame.target == Target::Body {
                break;
            }
            let hit = match &label {
                None => frame.target == Target::Loop,
                Some(name) => frame.label.as_ref() == Some(name),
            };
            if hit {
                frame.record(&self.env, continues);
                return;
            }
        }
    }

    /// Record the bindings at a `return`.
    fn exit_body(&mut self) {
        if let Some(frame) = self
            .exits
            .iter_mut()
            .rev()
            .find(|f| f.target == Target::Body)
        {
            frame.record(&self.env, false);
        }
    }

    /// Record the bindings at a `?`, in the nearest `try` block or body.
    /// Is that frame a `try` block?
    fn exit_question(&mut self) -> bool {
        let Some(frame) = self
            .exits
            .iter_mut()
            .rev()
            .find(|f| matches!(f.target, Target::Body | Target::Try))
        else {
            return false;
        };
        frame.record(&self.env, false);
        frame.target == Target::Try
    }

    /// Join in the bindings at earlier reads. A later part of an expression
    /// may change a binding that an earlier part already read:
    /// `f(source.take(), { source = None; })`.
    fn keep_reads(&mut self, reads: Vec<Env>) {
        for read in reads {
            if read != self.env {
                self.env.join(&read);
            }
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
                if flow.fall.is_none() {
                    // No path reaches this statement. Read it for its
                    // diagnostics only.
                    s.unreachable(|s| s.stmt(stmt));
                    continue;
                }
                let next = s.stmt(stmt);
                flow = flow.then(next);
            }
            flow
        })
    }

    /// Run `f` on code that never runs: keep its errors, and drop its
    /// bindings and its ledger entries.
    fn unreachable<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let (env, ledger) = (self.env.clone(), self.ledger.len());
        let out = f(self);
        self.env = env;
        self.ledger.truncate(ledger);
        out
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
        self.env.open_statement();
        let flow = self.annotated(attrs, |s| s.stmt_unannotated(stmt));
        self.env.close_statement();
        flow
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
            Expr::Await(e) => {
                let flow = self.expr_in(&e.base, true);
                // `pending.await` on `let pending = repo.scoped();`: the
                // terminal call runs here.
                // A chain awaited in place already counted its query.
                if !matches!(peel_parens(&e.base), Expr::MethodCall(_))
                    && self.is_pending_query(&e.base)
                {
                    let query = self.count("deferred builder awaited");
                    return flow.then(Flow::cost(query));
                }
                // No std container method gives a future. `repos.push(repo)
                // .await` is an application method of that name, which may
                // query through the handles.
                if !matches!(flow.fall, Some(Cost::Unbounded(_)))
                    && self.is_container_result(&e.base)
                {
                    return flow.then(Flow::cost(Cost::unbounded(
                        e.span(),
                        "this awaits a method with a standard container name on handles, so it \
                         is an application method, and what it runs is another function's \
                         business",
                        DECLARE_HINT,
                    )));
                }
                // `.await` on a value that holds a handle and is not a known
                // future: `PgPostRepository::new(&mut db)` stored, then
                // awaited. A base that is already reported keeps its reason.
                if matches!(flow.fall, Some(Cost::Unbounded(_)))
                    || self.is_known_future(&e.base)
                    || self.value_of(&e.base) == Kind::Plain
                {
                    return flow;
                }
                flow.then(Flow::cost(Cost::unbounded(
                    e.span(),
                    "this awaits a value that holds the database handle, such as an `async` \
                     constructor, and what it runs is another function's business",
                    DECLARE_HINT,
                )))
            }
            Expr::Try(e) => {
                let flow = self.expr_in(&e.expr, awaited);
                if self.exit_question() {
                    // A failed `?` lands after its `try` block.
                    return flow.then(Flow {
                        fall: Some(Cost::ZERO),
                        ..Flow::exit_to_try()
                    });
                }
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
                // Each parameter takes its argument as it was when it ran:
                // a later argument may change what an earlier one read.
                // `(|slot| slot.push(repo))(&mut list)`: each parameter
                // borrows what its argument borrows.
                let mut cost = Cost::ZERO;
                let mut params = Vec::new();
                let mut borrows = Vec::new();
                for arg in &call.args {
                    cost = cost.then(self.cost_of(arg));
                    params.push(self.value_of(arg));
                    borrows.push(self.referents_of(arg));
                }
                self.param_referents_each = borrows;
                Flow::cost(cost.then(self.closure_body(closure, &params, Kind::Plain)))
            }
            Expr::Call(call) => Flow::cost(self.call(call, awaited)),
            Expr::Macro(m) => Flow::cost(self.mac(&m.mac)),
            Expr::Closure(_) => Flow::cost(self.closure_arg(expr, Kind::Plain, false)),

            Expr::ForLoop(f) => {
                // The iterable runs first: what its value holds is known after.
                let iter = self.expr(&f.expr);
                let element = self.value_of(&f.expr).element();
                let shape = LoopShape {
                    // An iterable that never falls through leaves before the body.
                    bound: if iter.fall.is_none() {
                        Some(0)
                    } else {
                        const_bound(&f.expr)
                    },
                    span: f.span(),
                    label: f.label.as_ref(),
                    ends: true,
                };
                // `for target in slots.iter_mut()`, `for target in &mut slots`:
                // each element borrows into the container.
                let targets = self.referents_of(&f.expr);
                let body = self.loop_flow(&shape, |s| {
                    s.bind_pat(&f.pat, element);
                    for name in bound_names(&f.pat) {
                        s.add_referents(name, &targets);
                    }
                    s.block(&f.body)
                });
                iter.then(body)
            }
            // The condition runs on every pass, so a query in it
            // (`while let Some(job) = repo.next_pending().await?`) is
            // loop-resident.
            // A `while` ends when its condition is false: that path leaves
            // like a `break`, and pays for the condition.
            Expr::While(w) => {
                let shape = LoopShape {
                    bound: None,
                    span: w.span(),
                    label: w.label.as_ref(),
                    ends: false,
                };
                self.loop_flow(&shape, |s| {
                    // A condition that never falls through (`while { return
                    // …; }`) leaves before the body.
                    let cond = s.expr(&w.cond);
                    if cond.fall.is_none() {
                        s.unreachable(|s| s.block(&w.body));
                        return cond;
                    }
                    s.exit_loop(None, false);
                    let stop = Flow {
                        fall: None,
                        exits: vec![(
                            Exit {
                                label: None,
                                breaks: true,
                            },
                            Cost::ZERO,
                        )],
                        ret: None,
                    };
                    cond.then(stop.or_worst(s.block(&w.body)))
                })
            }
            // A `loop` ends only by an exit.
            Expr::Loop(l) => {
                let shape = LoopShape {
                    bound: None,
                    span: l.span(),
                    label: l.label.as_ref(),
                    ends: false,
                };
                self.loop_flow(&shape, |s| s.block(&l.body))
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
                let (then, els) = if self.picks_mixed_deferred(expr) {
                    let els_value = i.else_branch.as_ref().map(|(_, e)| &**e);
                    (
                        self.charge_deferred(then, block_tail(&i.then_branch)),
                        self.charge_deferred(els, els_value),
                    )
                } else {
                    (then, els)
                };
                cond.then(then.or_worst(els))
            }
            Expr::Match(m) => self.match_expr(m, expr),

            Expr::Block(b) if b.label.is_some() => {
                // `break 'label` lands after the block.
                let mut flow = self.framed(Target::Block, b.label.as_ref(), |s| s.block(&b.block));
                let own = worst_of(flow.take_exits(b.label.as_ref(), false));
                Flow {
                    fall: worst(flow.fall, own),
                    ..flow
                }
            }
            Expr::TryBlock(t) => {
                let mut flow = self.framed(Target::Try, None, |s| s.block(&t.block));
                let own = worst_of(flow.take_try_exits());
                Flow {
                    fall: worst(flow.fall, own),
                    ..flow
                }
            }
            Expr::Block(syn::ExprBlock { block, .. })
            | Expr::Unsafe(syn::ExprUnsafe { block, .. }) => self.block(block),
            // An async block may never be polled. A `return` or `?` in it
            // leaves the block only.
            Expr::Async(a) => Flow::cost(
                self.optional(|s| s.framed(Target::Body, None, |s| s.block(&a.block).total())),
            ),
            Expr::Const(c) => Flow::cost(self.block(&c.block).total()),

            Expr::Array(a) => self.each(a.elems.iter()),
            Expr::Tuple(t) => self.each(t.elems.iter()),
            // The value runs before the place.
            Expr::Assign(a) => {
                let value = self.expr(&a.right);
                let read = self.env.clone();
                let flow = value.then(self.expr(&a.left));
                self.keep_reads(vec![read]);
                self.assign(&a.left, &a.right);
                flow
            }
            // The right side of `&&` / `||` may not run. Its bindings go on
            // only when it falls through.
            Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
                let left = self.expr(&b.left);
                let entry = self.env.clone();
                let right = self.expr(&b.right);
                if right.fall.is_some() {
                    self.env.join(&entry);
                } else {
                    self.env = entry;
                }
                left.then(Flow::ZERO.or_worst(right))
            }
            // `ctx += repo`: an overloaded compound assignment may store the
            // right side in the left.
            Expr::Binary(b) if is_compound_assign(&b.op) => {
                // A std number type runs the value first. An overloaded
                // operator runs the place first. Take both orders.
                let entry = self.env.clone();
                let (ledger, errors) = (self.ledger.len(), self.errors.len());
                let value = self.expr(&b.right);
                let read = self.env.clone();
                let value_first = value.then(self.expr(&b.left));
                self.keep_reads(vec![read]);
                let after = std::mem::replace(&mut self.env, entry);
                self.ledger.truncate(ledger);
                self.errors.truncate(errors);
                let place_first = self.expr(&b.left).then(self.expr(&b.right));
                self.env.join(&after);
                self.store_into(&b.left, "", &[&b.right]);
                value_first.or_worst(place_first)
            }
            Expr::Binary(b) => self.expr(&b.left).then(self.expr(&b.right)),
            Expr::Return(r) => {
                let value = r.expr.as_deref().map_or(Flow::ZERO, |e| self.expr(e));
                if let Some(e) = r.expr.as_deref() {
                    self.returned = self.returned.max(self.result_value(e));
                }
                self.exit_body();
                value.then(Flow::RETURN)
            }
            Expr::Break(b) => {
                let value = b.expr.as_deref().map_or(Flow::ZERO, |e| self.expr(e));
                self.exit_loop(b.label.as_ref(), false);
                value.then(Flow::exit_to(b.label.as_ref(), true))
            }
            Expr::Continue(c) => {
                self.exit_loop(c.label.as_ref(), true);
                Flow::exit_to(c.label.as_ref(), false)
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
            Expr::Struct(s) => self.each(s.fields.iter().map(|f| &f.expr).chain(s.rest.as_deref())),
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
    fn match_expr(&mut self, m: &syn::ExprMatch, whole: &Expr) -> Flow {
        let scrutinee = self.expr(&m.expr);
        let entry = self.env.clone();
        // The guards tried so far, with their patterns and the bindings
        // after them: an arm runs after every guard before it whose pattern
        // can match its value.
        let mut tried: Vec<(&Pat, Cost, Env)> = Vec::new();
        let mut bodies = Vec::new();
        let mut joined: Option<Env> = None;
        for arm in &m.arms {
            let (pat, guard) = crate::parse::arm_pat_and_guard(arm);
            self.env = entry.clone();
            for (earlier, _, after) in &tried {
                if !patterns_disjoint(earlier, pat, &self.shadowed) {
                    self.env.join(after);
                }
            }
            let depth = self.env.depth();
            let (guard, after_guard, body) = self.scoped(|s| {
                s.bind_init(pat, &m.expr);
                let guard = guard.map_or(Flow::ZERO, |g| s.expr(g));
                let after_guard = s.env.clone();
                // A guard that never falls through never runs the body.
                let body = if guard.fall.is_some() {
                    s.annotated(&arm.attrs, |s| s.expr(&arm.body))
                } else {
                    s.unreachable(|s| s.annotated(&arm.attrs, |s| s.expr(&arm.body)))
                };
                (guard, after_guard, body)
            });
            let guard_falls = guard.fall.clone();
            let prefix = guard_prefix(
                tried
                    .iter()
                    .filter(|(earlier, _, _)| !patterns_disjoint(earlier, pat, &self.shadowed))
                    .map(|(earlier, cost, _)| (*earlier, cost)),
                &self.shadowed,
            );
            let body = Flow::cost(prefix).then(guard).then(body);
            // A later arm runs after this guard falls through, or after
            // this pattern fails (and the guard does not run).
            if let Some(cost) = guard_falls.clone() {
                // A failing guard falls through to the next arm with its
                // bindings.
                let mut after_guard = after_guard;
                after_guard.scopes.truncate(depth);
                tried.push((pat, cost, after_guard));
            }
            // `_ if { return …; } => …`: no later arm can run.
            let ends = guard_falls.is_none() && pattern_always_matches(pat);
            let falls = body.fall.is_some();
            bodies.push((body, &*arm.body));
            if falls {
                let arm_env = std::mem::replace(&mut self.env, Env::new());
                match &mut joined {
                    Some(env) => env.join(&arm_env),
                    None => joined = Some(arm_env),
                }
            }
            // Otherwise the arm always leaves; its exits recorded their
            // bindings.
            if ends {
                break;
            }
        }
        self.env = joined.unwrap_or_else(|| {
            let mut all = entry;
            for (_, _, after) in &tried {
                all.join(after);
            }
            all
        });
        let mixed = self.picks_mixed_deferred(whole);
        let mut flow = Flow::NEVER;
        for (body, value) in bodies {
            let body = if mixed {
                self.charge_deferred(body, Some(value))
            } else {
                body
            };
            flow = flow.or_worst(body);
        }
        scrutinee.then(flow)
    }

    /// Does the `if` or `match` `e` pick between known futures, some of
    /// them builder chains and some not? An `.await` cannot tell which
    /// one it runs, so each builder chain pays its query where it is built.
    fn picks_mixed_deferred(&self, e: &Expr) -> bool {
        branch_tails(e).is_some_and(|tails| {
            let pending = tails.iter().filter(|t| self.is_pending_query(t)).count();
            pending > 0 && pending < tails.len() && tails.iter().all(|t| self.is_known_future(t))
        })
    }

    /// `flow`, plus the query of a builder chain that `value` gives.
    fn charge_deferred(&mut self, flow: Flow, value: Option<&Expr>) -> Flow {
        if value.is_some_and(|v| self.is_pending_query(v)) {
            let query = self.count("deferred builder awaited");
            flow.then(Flow::cost(query))
        } else {
            flow
        }
    }

    fn each<'a>(&mut self, exprs: impl Iterator<Item = &'a Expr>) -> Flow {
        // A later part may change what an earlier part read:
        // `(source.take(), { source = None; })`.
        let mut flow = Flow::ZERO;
        let mut reads = Vec::new();
        for (i, expr) in exprs.enumerate() {
            if i > 0 {
                reads.push(self.env.clone());
            }
            let next = self.expr(expr);
            flow = flow.then(next);
        }
        self.keep_reads(reads);
        flow
    }

    /// A closure's body, with parameter `i` bound to `params[i]`, or to
    /// `rest` past the end. A `return` inside leaves the closure only.
    fn closure_body(&mut self, closure: &syn::ExprClosure, params: &[Kind], rest: Kind) -> Cost {
        let connection = std::mem::take(&mut self.connection_params);
        let borrows = std::mem::take(&mut self.param_referents);
        let each = std::mem::take(&mut self.param_referents_each);
        self.framed(Target::Body, None, |s| {
            s.scoped(|s| {
                for (i, input) in closure.inputs.iter().enumerate() {
                    s.bind_pat(input, params.get(i).copied().unwrap_or(rest));
                    for name in bound_names(input) {
                        s.add_referents(name.clone(), &borrows);
                        if let Some(own) = each.get(i) {
                            s.add_referents(name, own);
                        }
                    }
                    // A transaction hands its callback a connection.
                    if connection && let Pat::Ident(id) = input {
                        s.env.set_shape(&id.ident.to_string(), Some(Shape::Db));
                    }
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
        let cost = match arg {
            Expr::Closure(closure) => self.optional(|s| s.closure_body(closure, &[], param)),
            _ => self.non_closure_arg(arg, takes_callback),
        };
        // Only this argument's closure gets a connection: a named callback
        // never consumes the flag, so it must not reach a later closure.
        self.connection_params = false;
        cost
    }

    /// A loop's flow. `body` runs in its own scope and break frame, zero or
    /// more times. Every path out of the loop costs at most every pass: a
    /// `break` or `continue` to this loop ends here, and an exit to an outer
    /// label or a `return` leaves with that cost.
    fn loop_flow(
        &mut self,
        shape: &LoopShape<'_>,
        mut body: impl FnMut(&mut Self) -> Flow,
    ) -> Flow {
        if shape.bound == Some(0) {
            // The body never runs.
            self.unreachable(|s| s.framed(Target::Loop, shape.label, |s| s.scoped(&mut body)));
            return Flow::ZERO;
        }
        let before = self.ledger.len();
        // A pass that never falls through brings no bindings to the next
        // pass or past the loop. A `continue` brings its bindings to the next
        // pass. A `break` brings them past the loop only.
        let mut breaks: Option<Env> = None;
        let mut flow = self.repeated(|s| {
            let (flow, frame) = s.in_frame(Target::Loop, shape.label, |s| {
                let start = s.env.clone();
                let flow = s.scoped(&mut body);
                if flow.fall.is_none() {
                    s.env = start;
                }
                flow
            });
            if let Some(env) = frame.continues {
                s.env.join(&env);
            }
            if let Some(env) = frame.env {
                match &mut breaks {
                    Some(joined) => joined.join(&env),
                    None => breaks = Some(env),
                }
            }
            flow
        });
        if let Some(env) = breaks {
            self.env.join(&env);
        }
        let (breaks, continues): (Vec<_>, Vec<_>) = flow
            .take_exits(shape.label, true)
            .into_iter()
            .partition(|(exit, _)| exit.breaks);
        let brk = worst_of(breaks);
        let outer = std::mem::take(&mut flow.exits);
        let pass = Pass {
            again: worst(flow.fall, worst_of(continues)),
            leave: worst(worst(flow.ret, brk.clone()), worst_of(outer.clone())),
        };
        // Only a `break` to this loop ends a `loop`; a `continue` does not.
        // A loop known to run, whose every pass leaves, never ends by itself.
        let exhausts = shape.ends && !(shape.bound.is_some_and(|n| n > 0) && pass.again.is_none());
        let again = pass.again.clone();
        let total = self.bound_loop(pass, shape.bound, shape.span, before);
        let fall = (exhausts || brk.is_some())
            .then(|| loop_fall(again, brk, exhausts, shape.bound, &total));
        Flow {
            fall,
            exits: outer
                .into_iter()
                .map(|(exit, _)| (exit, total.clone()))
                .collect(),
            // A `?` or a panic may also leave the loop.
            ret: Some(total),
        }
    }

    /// Turn a loop pass's cost into the loop's cost. Each pass but the last
    /// goes `again`; the last pass may also `leave`. A path that leaves is
    /// paid once, not once per pass.
    fn bound_loop(
        &mut self,
        pass: Pass,
        bound: Option<u32>,
        span: Span,
        ledger_before: usize,
    ) -> Cost {
        let again = pass.again.unwrap_or(Cost::ZERO);
        let last = again.clone().or_worst(pass.leave.unwrap_or(Cost::ZERO));
        if again.is_zero() || matches!(last, Cost::Unbounded(_)) {
            return last;
        }
        if let Some(times) = bound {
            if times > 1 {
                for entry in &mut self.ledger[ledger_before..] {
                    write!(entry, " ×{times}").expect("writing to a String cannot fail");
                }
            }
            return again.repeated(times.saturating_sub(1)).then(last);
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

        let mut cost = self.cost_of(root).then(self.chain_args(&methods));
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
            // Only `checkout` called on the `LazyDb` itself: `lazy.wrap()` may
            // give a user type with its own `checkout`.
            let receiver = on_handle.last().map(|m| &*m.receiver);
            if HANDLE_TRANSITIONS.contains(&last.as_str())
                && receiver.is_some_and(|r| self.expr_is_lazy_db(r))
            {
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
            if self.known_container_method(
                &method.receiver,
                &method.method.to_string(),
                method.args.len(),
            ) {
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
        // A handle in an `Option` or a `Result` (`Result<LazyDb, E>`) has only
        // their methods: any other comes from an extension trait.
        let unknown = methods.iter().find(|m| {
            (self.expr_is_carrier(&m.receiver)
                || (self.expr_is_handle(&m.receiver)
                    && matches!(
                        self.shape_of(&m.receiver),
                        Some(Shape::Opt | Shape::OptRef | Shape::Res)
                    )))
                && !self.known_container_method(&m.receiver, &m.method.to_string(), m.args.len())
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
    /// Run the arguments of each method in a chain. They run regardless of
    /// what the chain does with them. Each store reads its own arguments
    /// right after they ran, before a later method's arguments run:
    /// `dest.push({ source.push(repo); … })`.
    fn chain_args(&mut self, methods: &[&ExprMethodCall]) -> Cost {
        let mut cost = Cost::ZERO;
        let mut reads = Vec::new();
        for method in methods {
            if !method.args.is_empty() {
                reads.push(self.env.clone());
            }
            cost = cost.then(self.method_args(method));
            let args: Vec<&Expr> = method.args.iter().collect();
            self.store_into(&method.receiver, &method.method.to_string(), &args);
        }
        self.keep_reads(reads);
        cost
    }

    fn method_args(&mut self, method: &ExprMethodCall) -> Cost {
        let name = method.method.to_string();
        // A user type may have a method with a transaction name that calls
        // its closure many times, so the receiver must be a handle.
        let is_transaction =
            TRANSACTION_METHODS.contains(&name.as_str()) && self.is_connection(&method.receiver);
        // Only an `Option` or a `Result` is known to call its closure at most
        // once; a user type's `unwrap_or_else` may call it many times.
        let runs_once = is_transaction
            || (AT_MOST_ONCE_CLOSURE_METHODS.contains(&name.as_str())
                && matches!(
                    self.shape_of(&method.receiver),
                    Some(Shape::Opt | Shape::OptRef | Shape::Res)
                ))
            // `bool::then` runs its closure at most once.
            || (name == "then" && self.shape_of(&method.receiver) == Some(Shape::Bool));
        // A closure handed to a method on a carrier takes its elements:
        // `repos.iter().for_each(|r| …)`.
        let param = if is_transaction {
            Kind::Handle
        } else if self.expr_is_nested(&method.receiver) || self.expr_is_holder(&method.receiver) {
            Kind::Nested
        } else if self.expr_is_carrier(&method.receiver) {
            Kind::Handle
        } else if TRANSACTION_METHODS.contains(&name.as_str())
            && self.expr_is_handle(&method.receiver)
        {
            // `repo.tx(|conn| …)` on a repository may hand its callback a
            // connection, maybe many times.
            Kind::Handle
        } else {
            Kind::Plain
        };
        // A fold's closure also gets its accumulator: the seed, then what the
        // closure returns.
        let param = if matches!(
            name.as_str(),
            "fold" | "try_fold" | "rfold" | "try_rfold" | "scan"
        ) {
            let seed = method
                .args
                .iter()
                .filter(|a| !matches!(a, Expr::Closure(_)))
                .map(|a| self.value_of(a))
                .fold(param, Kind::max);
            let acc = method
                .args
                .last()
                .map_or(Kind::Plain, |f| self.closure_output(f, seed));
            seed.max(acc)
        } else {
            param
        };
        // A function given by path where a closure would run is opaque.
        // An at-most-once method calls its argument too: `inspect_err(f)`.
        let takes_callback = is_transaction
            || ((CALLBACK_METHODS.contains(&name.as_str())
                || AT_MOST_ONCE_CLOSURE_METHODS.contains(&name.as_str()))
                && self.value_of(&method.receiver) != Kind::Plain);
        // The callback is the last argument: `db.tx_with(opts, |conn| …)`,
        // `opt.map_or(default, f)`. Every argument of `map_or_else` is one.
        let last = method.args.len().saturating_sub(1);
        let every = name == "map_or_else";
        let mut cost = Cost::ZERO;
        // `slots.iter_mut().for_each(|t| …)`: a parameter borrows into the
        // place the receiver borrows.
        let borrows = self.referents_of(&method.receiver);
        let mut reads = Vec::new();
        for (i, arg) in method.args.iter().enumerate() {
            if i > 0 {
                reads.push(self.env.clone());
            }
            let callback = takes_callback && (every || i == last);
            let param = self.side_param(method, i).unwrap_or(param);
            self.param_referents.clone_from(&borrows);
            let next = if runs_once {
                self.connection_params = is_transaction;
                self.callback_arg(arg, param, callback)
            } else {
                self.closure_arg(arg, param, callback)
            };
            // Only this argument's closure borrows: a named callback never
            // takes the field, so it must not reach a later closure.
            self.param_referents.clear();
            cost = cost.then(next);
        }
        self.keep_reads(reads);
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

    fn call(&mut self, call: &ExprCall, awaited: bool) -> Cost {
        let name = call_path_name(call);
        // `scoped_transaction` / `savepoint` run their closure once and hand
        // it a connection.
        // Only with the connection as its first argument: a user function of
        // the same name may call its closure many times.
        let runs_once = name
            .as_deref()
            .is_some_and(|n| TRANSACTION_FREE_FNS.contains(&n))
            && call.args.first().is_some_and(|a| self.is_connection(a));

        let mut cost = self.cost_of(&call.func);
        let last = call.args.len().saturating_sub(1);
        let mut reads = Vec::new();
        for (i, arg) in call.args.iter().enumerate() {
            reads.push(self.env.clone());
            let next = if runs_once {
                self.connection_params = true;
                self.callback_arg(arg, Kind::Handle, i == last)
            } else {
                self.cost_of(arg)
            };
            cost = cost.then(next);
        }
        self.keep_reads(reads);
        // `fill(&mut repos, &repo)` or `fill(slot, &repo)`: a `&mut`
        // argument may receive a handle from another argument, or from a
        // stored closure that captures one. The arguments ran first, so what
        // they hold is known.
        for (i, arg) in call.args.iter().enumerate() {
            let place = match arg {
                Expr::Reference(r) if r.mutability.is_some() => &*r.expr,
                _ if !self.referents_of(arg).is_empty() => arg,
                _ => continue,
            };
            let others: Vec<&Expr> = call
                .args
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| a)
                .chain(std::iter::once(&*call.func))
                .collect();
            self.store_into(place, "", &others);
        }
        // The connection is the callback's, not an escape into opaque code.
        // An unreadable argument already explains itself.
        if runs_once || matches!(cost, Cost::Unbounded(_)) {
            return cost;
        }
        // An awaited `drop` is a user `async fn` of that name.
        if !awaited && self.is_std_free_fn(call) {
            return cost;
        }
        // `Some(repo)`, `Ok(db)`, `Arc::new(repo)`, `PgPostRepository(pool)`:
        // these build a value that holds the handle and run no query. Any
        // other uppercase callee may be a user type that runs queries. No
        // std constructor gives a future, so an awaited one is a user
        // `async fn`, which can run queries.
        if !awaited
            && (self.is_container_constructor(call)
                || self.is_smart_pointer_new(call)
                || is_handle_constructor(call))
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
        if let Some(elems) = self.std_vec(mac) {
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
        // A name the handler body defines, or a `vec!` that is not std, is
        // the user's macro.
        let users = self.is_shadowed(&name) || (name == "vec" && !std_prefix(&mac.path));
        if INERT_MACROS.contains(&name.as_str()) && !users {
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
        // Std `drop` gives `()`. A value from a `drop` that took a handle is
        // a user function's, and may hold it (`let pending = drop(repo);`).
        if let Expr::Call(c) = peel_parens(expr)
            && self.is_std_free_fn(c)
            && c.args.iter().any(|a| self.expr_carries_handle(a))
        {
            return Kind::Holder;
        }
        if self.expr_is_nested(expr) {
            Kind::Nested
        } else if self.expr_is_lazy_db(expr) {
            Kind::LazyDb
        } else if self.expr_is_handle(expr) || self.chain_root_is_handle(expr) {
            Kind::Handle
        } else if self.expr_is_holder(expr) {
            Kind::Holder
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

    /// The shape of an `if`, `match` or block whose tails all have it.
    fn tails_shape(&self, e: &Expr) -> Option<Shape> {
        let tails = branch_tails(e)?;
        let first = self.shape_of(tails.first()?)?;
        tails
            .iter()
            .all(|t| self.shape_of(t) == Some(first))
            .then_some(first)
    }

    /// The container shape of `e`, when it is known.
    fn shape_of(&self, e: &Expr) -> Option<Shape> {
        match e {
            Expr::Path(p)
                if p.path.is_ident("None") && self.env.binding("None").shape.is_none() =>
            {
                Some(Shape::Opt)
            }
            Expr::Path(_) => path_ident(e).and_then(|name| self.env.binding(&name).shape),
            // `self.db`, `state.conn`: a connection accessor.
            Expr::Field(f) if member_is_db_accessor(&f.member) => Some(Shape::Db),
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => self.shape_of(&u.expr),
            // `res?` on a `Result<Db, E>` gives a `Db`.
            Expr::Try(t) => self
                .shape_of(&t.expr)
                .filter(|s| *s == Shape::Db)
                .or_else(|| self.inner_shape(&t.expr)),
            Expr::Await(a) => self.shape_of(&a.base).filter(|s| *s == Shape::Db),
            Expr::Reference(r) => self.shape_of(&r.expr),
            Expr::Paren(p) => self.shape_of(&p.expr),
            Expr::Group(g) => self.shape_of(&g.expr),
            Expr::Array(_) | Expr::Repeat(_) => Some(Shape::Array),
            Expr::Tuple(_) => Some(Shape::Tuple),
            Expr::If(_) | Expr::Match(_) | Expr::Block(_) => self.tails_shape(e),
            // A comparison, `&&`, `||`, `!` or a `bool` literal.
            Expr::Binary(b)
                if matches!(
                    b.op,
                    syn::BinOp::Eq(_)
                        | syn::BinOp::Ne(_)
                        | syn::BinOp::Lt(_)
                        | syn::BinOp::Le(_)
                        | syn::BinOp::Gt(_)
                        | syn::BinOp::Ge(_)
                        | syn::BinOp::And(_)
                        | syn::BinOp::Or(_)
                ) =>
            {
                Some(Shape::Bool)
            }
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Not(_)) => Some(Shape::Bool),
            Expr::Lit(l) if matches!(l.lit, syn::Lit::Bool(_)) => Some(Shape::Bool),
            Expr::Macro(m) => self.std_vec(&m.mac).map(|_| Shape::Vec),
            Expr::Call(c) if self.is_smart_pointer_new(c) => {
                c.args.first().and_then(|a| self.shape_of(a))
            }
            // `use custom::Item as Some;`: an application item has no std
            // shape.
            Expr::Call(c) if self.call_head_shadowed(c) => None,
            Expr::Call(c) => match call_path_name(c).as_deref() {
                Some("Some") if matches!(c.args.first(), Some(Expr::Reference(_))) => {
                    Some(Shape::OptRef)
                }
                Some("Some") => Some(Shape::Opt),
                Some("Ok" | "Err") => Some(Shape::Res),
                // `Vec::new()`, `HashMap::with_capacity(n)`. Not
                // `custom::Vec::new()`.
                Some("new" | "with_capacity" | "default") => match &*c.func {
                    Expr::Path(p) if std_owner(&p.path) => p
                        .path
                        .segments
                        .iter()
                        .rev()
                        .nth(1)
                        .and_then(|s| shape_named(&s.ident.to_string())),
                    _ => None,
                },
                _ => None,
            },
            Expr::MethodCall(mc) => {
                let method = mc.method.to_string();
                // `state.db()`, `ctx.conn().await?`, `lazy.checkout().await?`: a
                // connection.
                if DB_ACCESSORS.contains(&method.as_str())
                    || (method == "checkout" && self.expr_is_lazy_db(&mc.receiver))
                {
                    return Some(Shape::Db);
                }
                // `try_for_each(|_| Err(&repo))` gives what its callback gives.
                if matches!(method.as_str(), "try_for_each" | "try_fold") {
                    let tail = match mc.args.last()? {
                        Expr::Closure(c) => match &*c.body {
                            Expr::Block(b) => block_tail(&b.block)?,
                            body => body,
                        },
                        _ => return None,
                    };
                    return self.shape_of(tail);
                }
                // `maybe.unwrap()` on an `Option<Db>` gives a `Db`.
                if UNWRAP_METHODS.contains(&method.as_str())
                    && matches!(
                        self.shape_of(&mc.receiver),
                        Some(Shape::Opt | Shape::OptRef | Shape::Res)
                    )
                {
                    return self.inner_shape(&mc.receiver);
                }
                if method == "collect" {
                    // `collect::<Vec<_>>()` names its shape.
                    return mc.turbofish.as_ref().and_then(|t| {
                        t.args.iter().find_map(|arg| match arg {
                            syn::GenericArgument::Type(ty) if !self.type_is_shadowed(ty) => {
                                type_shape(ty)
                            }
                            _ => None,
                        })
                    });
                }
                self.shape_of(&mc.receiver)?.after(&method)
            }
            _ => None,
        }
    }

    /// The shape of a part of the named wrapper `e`, when it is known.
    fn inner_shape(&self, e: &Expr) -> Option<Shape> {
        self.env.binding(&wrapper_root(e)?).inner
    }

    /// Is `method`, called with `args` arguments, a known method of the
    /// standard container `receiver`? Not on a user value or a nested
    /// container: their methods are the user's. Not with an argument count
    /// the standard method does not take: `repos.push()` is a trait method.
    /// Is `e` the plain result of a known container method on a receiver
    /// that holds handles (`repos.push(repo)`, `repos.len()`), or a name
    /// bound to one? A std method of this kind gives no future.
    fn is_container_result(&self, e: &Expr) -> bool {
        match peel_parens(e) {
            Expr::MethodCall(mc) => {
                self.known_container_method(&mc.receiver, &mc.method.to_string(), mc.args.len())
                    && self.value_of(&mc.receiver) != Kind::Plain
                    && self.value_of(e) == Kind::Plain
            }
            other => path_ident(other).is_some_and(|name| self.env.binding(&name).container_result),
        }
    }

    fn known_container_method(&self, receiver: &Expr, method: &str, args: usize) -> bool {
        !self.expr_is_holder(receiver)
            && !self.expr_is_nested(receiver)
            && self.shape_of(receiver).is_some_and(|shape| {
                shape.has(method) && std_arities(shape, method).contains(&args)
            })
    }

    /// `receiver.method(arg)` may store `arg` in `receiver`. When an argument
    /// holds a handle, the name at the root of `receiver` now holds it too.
    fn store_into(&mut self, receiver: &Expr, method: &str, args: &[&Expr]) {
        // `repos.clear()` on a name that owns its value empties it. What it
        // held stays a floor: a later opaque store has the element type.
        if method == "clear" && args.is_empty() && self.known_container_method(receiver, method, 0)
        {
            if let Some(name) = path_ident(peel_parens(receiver)) {
                let mut binding = self.env.binding(&name);
                if binding.referents.is_empty() && binding.kind != Kind::Plain {
                    let floor = binding.declared.take().unwrap_or_else(|| {
                        Box::new(Binding {
                            kind: binding.kind,
                            shape: binding.shape,
                            inner: binding.inner,
                            ..Binding::of(Kind::Plain)
                        })
                    });
                    binding.kind = Kind::Plain;
                    binding.parts = None;
                    binding.declared = Some(floor);
                    self.env.assign(name, binding);
                }
            }
            return;
        }
        // An opaque value stored into a container with a floor has the
        // element type: `repos.push(make_repo())` after `repos.clear()`.
        if STORE_METHODS.contains(&method)
            && args.iter().any(|a| self.is_opaque_value(a))
            && let Some(root) = place_root(receiver)
            && let Some(floor) = self.env.binding(&root).declared
            && floor.kind != Kind::Plain
        {
            self.raise(root, floor.kind);
        }
        // An executor uses the connection for one query and gives it back.
        // A known container method stores only if it is a store method.
        if EXECUTORS.contains(&method)
            || (self.known_container_method(receiver, method, args.len())
                && !STORE_METHODS.contains(&method))
        {
            return;
        }
        // A callback stores what it returns: `slot.get_or_insert_with(|| …)`.
        let held = args
            .iter()
            .map(|a| match a {
                // A std callback method gives its callback's result back.
                Expr::Closure(_)
                    if CALLBACK_METHODS.contains(&method) && !STORE_METHODS.contains(&method) =>
                {
                    Kind::Plain
                }
                Expr::Closure(_) => self.closure_output(a, Kind::Plain),
                // A query future was paid for where it was built.
                _ if self.is_counted_query_future(a) => Kind::Plain,
                _ => self.value_of(a),
            })
            .max()
            .unwrap_or(Kind::Plain);
        if held == Kind::Plain {
            return;
        }
        let Some(root) = place_root(receiver) else {
            return;
        };
        // `append` and `extend` add the parts of a sequence, an `Option` or a
        // set, not the container. A map's parts are tuples.
        let flat = matches!(method, "append" | "extend")
            && args.iter().all(|a| {
                self.value_of(a) != Kind::Carrier
                    || !matches!(
                        self.shape_of(a),
                        None | Some(Shape::Map | Shape::SortedMap | Shape::Tuple)
                    )
            });
        let kind = match held {
            Kind::Handle | Kind::LazyDb if STORE_METHODS.contains(&method) => Kind::Carrier,
            Kind::Carrier if flat => Kind::Carrier,
            Kind::Handle | Kind::LazyDb => Kind::Holder,
            _ => Kind::Nested,
        };
        // A store into a part (`lists[0].push(repo)`) puts the handle one
        // layer down: the root holds it at an unknown depth.
        let kind = if matches!(peel_refs(receiver), Expr::Path(_)) {
            kind
        } else {
            Kind::Nested
        };
        self.raise(root, kind);
    }

    /// `root` now holds at least `kind`. A store through `slot = &mut repos`
    /// is a store into `repos` too.
    fn raise(&mut self, root: String, kind: Kind) {
        // A place an alias borrows only a part of (`&mut slots[0]`) holds
        // the value one layer down.
        let whole = self.env.binding(&root).whole;
        for (i, name) in self.alias_targets(root).into_iter().enumerate() {
            let held = if i == 0 || whole { kind } else { Kind::Nested };
            self.raise_one(name, held);
        }
    }

    /// Raise `name` alone to hold at least `kind`.
    fn raise_one(&mut self, name: String, kind: Kind) {
        let mut binding = self.env.binding(&name);
        if kind > binding.kind {
            binding.kind = kind;
            binding.parts = None;
            if kind != Kind::Carrier {
                binding.shape = None;
            }
            self.env.assign(name, binding);
        }
    }

    /// Does `e` hold handles at an unknown depth ([`Kind::Nested`])? A
    /// container of containers or of user values, or any part of one.
    fn expr_is_nested(&self, e: &Expr) -> bool {
        if let Some(kind) = self.produced(e) {
            return kind == Kind::Nested;
        }
        let container = |e: &Expr| {
            !self.is_counted_query_future(e)
                && matches!(
                    self.value_of(e),
                    Kind::Carrier | Kind::Holder | Kind::Nested
                )
        };
        match e {
            // `try { repo }` wraps its tail in a type the macro cannot see:
            // a value that holds anything is read as nested.
            Expr::TryBlock(t) => {
                block_tail(&t.block).is_some_and(|tail| self.value_of(tail) != Kind::Plain)
            }
            // `repo..`: a range holds its ends.
            Expr::Range(r) => [&r.start, &r.end]
                .into_iter()
                .flatten()
                .any(|end| self.value_of(end) != Kind::Plain),
            Expr::Path(_) => path_ident(e).is_some_and(|name| self.env.get(&name) == Kind::Nested),
            Expr::Reference(r) => self.expr_is_nested(&r.expr),
            Expr::RawAddr(r) => self.expr_is_nested(&r.expr),
            Expr::Cast(c) => self.expr_is_nested(&c.expr),
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => self.expr_is_nested(&u.expr),
            Expr::Paren(p) => self.expr_is_nested(&p.expr),
            Expr::Group(g) => self.expr_is_nested(&g.expr),
            // A part of a user value with no recorded parts may itself be a
            // container.
            Expr::Field(f) => self.part_kind(f).map_or_else(
                || self.expr_is_nested(&f.base) || self.expr_is_holder(&f.base),
                |k| k == Kind::Nested,
            ),
            Expr::Index(i) => self.expr_is_nested(&i.expr) || self.expr_is_holder(&i.expr),
            Expr::Try(t) => self.expr_is_nested(&t.expr) || self.expr_is_holder(&t.expr),
            // Any part of a nested value, the result of a user method on a
            // holder, or a mapping whose closure gives containers or user
            // values (`map(|r| Ctx { repo: r })`): its shape is not known.
            Expr::MethodCall(mc) => {
                let method = mc.method.to_string();
                // A scalar name gives a plain value only on a std container:
                // a user's `ctx.clear()` may return anything.
                !(SCALAR_METHODS.contains(&method.as_str())
                    && self.known_container_method(&mc.receiver, &method, mc.args.len()))
                    && ((self.expr_is_nested(&mc.receiver) && !self.maps_away(mc))
                        // `repos.chunks(2)` yields slices of handles.
                        || (matches!(method.as_str(), "chunks" | "windows")
                            && self.expr_is_carrier(&mc.receiver))
                        // `zip` yields tuples, with handles on either side.
                        || (method == "zip"
                            && (self.holds(&mc.receiver) || mc.args.iter().any(|a| self.holds(a))))
                        || (method == "chain" && mc.args.iter().any(|a| self.expr_is_nested(a)))
                        // `enumerate` and a map's iterators yield tuples.
                        || (method == "enumerate" && self.holds(&mc.receiver))
                        || (matches!(method.as_str(), "iter" | "iter_mut" | "into_iter" | "drain")
                            && matches!(
                                self.shape_of(&mc.receiver),
                                Some(Shape::Map | Shape::SortedMap)
                            )
                            && self.holds(&mc.receiver))
                        || (self.expr_is_holder(&mc.receiver)
                            && !SAME_TYPE_METHODS.contains(&method.as_str())
                            && !HANDLE_ACCESSORS.contains(&method.as_str()))
                        || self.callback_result(mc) == Kind::Nested)
            }
            Expr::Array(a) => a.elems.iter().any(container),
            Expr::Tuple(t) => t.elems.iter().any(container),
            Expr::Repeat(r) => container(&r.expr),
            Expr::Call(c) => {
                ((self.is_container_constructor(c) || self.is_smart_pointer_new(c))
                    && c.args.iter().any(container))
                    // `make()`, where `make` is a closure that holds a handle.
                    || path_ident(&c.func).is_some_and(|name| self.env.get(&name) != Kind::Plain)
            }
            Expr::Macro(m) => self
                .std_vec(&m.mac)
                .is_some_and(|elems| elems.iter().any(container)),
            Expr::If(i) => {
                block_tail(&i.then_branch).is_some_and(|e| self.expr_is_nested(e))
                    || i.else_branch
                        .as_ref()
                        .is_some_and(|(_, e)| self.expr_is_nested(e))
            }
            Expr::Match(m) => m.arms.iter().any(|arm| self.expr_is_nested(&arm.body)),
            Expr::Loop(_) | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                break_results(e).into_iter().any(|v| self.expr_is_nested(v))
            }
            Expr::Block(b) => block_tail(&b.block).is_some_and(|e| self.expr_is_nested(e)),
            Expr::Unsafe(u) => block_tail(&u.block).is_some_and(|e| self.expr_is_nested(e)),
            _ => false,
        }
    }

    /// What a callback method's result holds, from what its callback
    /// returns and its other arguments hold. Plain for any other method.
    fn callback_result(&self, mc: &ExprMethodCall) -> Kind {
        let method = mc.method.to_string();
        let flattens = FLATTENING_CALLBACKS.contains(&method.as_str());
        let wraps = flattens || WRAPPING_CALLBACKS.contains(&method.as_str());
        if !wraps && !DIRECT_CALLBACKS.contains(&method.as_str()) {
            return Kind::Plain;
        }
        let param = match self.value_of(&mc.receiver) {
            Kind::Plain => Kind::Plain,
            kind => kind.element(),
        };
        let out = mc
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| match a {
                Expr::Closure(_) => self.closure_output(a, self.side_param(mc, i).unwrap_or(param)),
                _ => self.value_of(a),
            })
            .max()
            .unwrap_or(Kind::Plain);
        // A flattening adapter yields the parts of each output.
        let out = if flattens { out.element() } else { out };
        match out {
            Kind::Handle | Kind::LazyDb if wraps => Kind::Carrier,
            Kind::Carrier | Kind::Holder if wraps => Kind::Nested,
            kind => kind,
        }
    }

    /// What the closure `f` returns when its parameters hold `param`. It is
    /// read in a copy of the bindings. Plain when `f` is not a closure.
    fn closure_output(&self, f: &Expr, param: Kind) -> Kind {
        let Expr::Closure(closure) = f else {
            return Kind::Plain;
        };
        self.closure_value(closure, &[], param, false)
    }

    /// A copy of the analysis that reads a body. Its own record of closed
    /// scopes keeps the body's handle parameters out of the real one.
    /// Is `name` defined or imported by the handler body?
    fn is_shadowed(&self, name: &str) -> bool {
        shadows(&self.shadowed, name)
    }

    /// Does the handler body define or import the first name of `call`'s
    /// path: `use custom::Item as Some;`, `use custom::Arc;`?
    fn call_head_shadowed(&self, call: &ExprCall) -> bool {
        let Expr::Path(path) = &*call.func else {
            return false;
        };
        path.path
            .segments
            .first()
            .is_some_and(|s| self.is_shadowed(&s.ident.to_string()))
    }

    /// A std `Some`, `Ok` or `Err` call that the body does not shadow.
    fn is_container_constructor(&self, call: &ExprCall) -> bool {
        is_container_constructor_path(call) && !self.call_head_shadowed(call)
    }

    /// A std smart pointer's `new` that the body does not shadow.
    fn is_smart_pointer_new(&self, call: &ExprCall) -> bool {
        is_smart_pointer_new_path(call) && !self.call_head_shadowed(call)
    }

    /// Does `ty` name an item that the handler body defines or imports?
    fn type_is_shadowed(&self, ty: &Type) -> bool {
        struct Names<'s>(&'s [String], bool);
        impl<'a> Visit<'a> for Names<'_> {
            fn visit_path_segment(&mut self, s: &'a syn::PathSegment) {
                self.1 |= shadows(self.0, &s.ident.to_string());
                syn::visit::visit_path_segment(self, s);
            }
        }
        if self.shadowed.is_empty() {
            return false;
        }
        let mut names = Names(&self.shadowed, false);
        names.visit_type(ty);
        names.1
    }

    /// The elements of a std `vec!`: bare or under a std path, and not
    /// defined by the handler body.
    fn std_vec(&self, mac: &syn::Macro) -> Option<Vec<Expr>> {
        (std_prefix(&mac.path) && !self.is_shadowed("vec"))
            .then(|| vec_elems(mac))
            .flatten()
    }

    /// Is `call` the std `drop` (`drop`, `std::mem::drop`), not one the
    /// handler body defines?
    fn is_std_free_fn(&self, call: &ExprCall) -> bool {
        let Expr::Path(path) = &*call.func else {
            return false;
        };
        call_path_name(call)
            .is_some_and(|n| SAFE_FREE_FNS.contains(&n.as_str()) && !self.is_shadowed(&n))
            && std_prefix(&path.path)
    }

    fn probe(&self) -> Self {
        let mut env = self.env.clone();
        env.closed = Rc::default();
        Self {
            env,
            exits: Vec::new(),
            ledger: Vec::new(),
            errors: Vec::new(),
            returned: Kind::Plain,
            connection_params: false,
            param_referents: Vec::new(),
            param_referents_each: Vec::new(),
            shadowed: Rc::clone(&self.shadowed),
        }
    }

    /// What an expression that runs a body at once gives: `(|x| x)(repo)`,
    /// `async { &repo }.await`, `(|| async { … })().await`. `None` for any
    /// other expression.
    fn produced(&self, e: &Expr) -> Option<Kind> {
        match e {
            Expr::Paren(p) => self.produced(&p.expr),
            Expr::Group(g) => self.produced(&g.expr),
            // `result?` on a `Result` with known sides: the `Ok` side.
            Expr::Try(t) if self.sides_of(&t.expr).is_some() => {
                self.sides_of(&t.expr).map(|(ok, _)| ok)
            }
            Expr::Call(call) => self.invoked(call, false),
            // `result.unwrap_err()` on a `Result` with known sides: that side.
            // `result.unwrap_or(0)`: the `Ok` side or what the fallback gives.
            Expr::MethodCall(mc)
                if matches!(
                    mc.method.to_string().as_str(),
                    "unwrap"
                        | "expect"
                        | "unwrap_err"
                        | "expect_err"
                        | "unwrap_or"
                        | "unwrap_or_default"
                        | "unwrap_or_else"
                ) && self.shape_of(&mc.receiver) == Some(Shape::Res) =>
            {
                let method = mc.method.to_string();
                if method.ends_with("_err") {
                    return self.side_kind(&mc.receiver, "Err");
                }
                let ok = self.side_kind(&mc.receiver, "Ok")?;
                let error = self.side_kind(&mc.receiver, "Err").unwrap_or(Kind::Nested);
                let fallback = mc
                    .args
                    .iter()
                    .filter(|_| method.starts_with("unwrap_or"))
                    .map(|a| match a {
                        Expr::Closure(_) => self.closure_output(a, error),
                        _ => self.value_of(a),
                    })
                    .max()
                    .unwrap_or(Kind::Plain);
                Some(ok.max(fallback))
            }
            // `result.err()` on a `Result` with known sides: an `Option` of
            // that side.
            Expr::MethodCall(mc)
                if matches!(mc.method.to_string().as_str(), "ok" | "err")
                    && self.shape_of(&mc.receiver) == Some(Shape::Res) =>
            {
                let side = if mc.method == "ok" { "Ok" } else { "Err" };
                let kind = self.side_kind(&mc.receiver, side)?;
                Some(match kind {
                    Kind::Plain => Kind::Plain,
                    Kind::Handle | Kind::LazyDb => Kind::Carrier,
                    Kind::Carrier | Kind::Holder | Kind::Nested => Kind::Nested,
                })
            }
            Expr::Await(a) => {
                let base = peel_parens(&a.base);
                let output = self.output_of(base);
                // `.await` on a value that holds a handle and is not a known
                // future (`PgPostRepository::new(&mut db).await`) gives what
                // that value holds.
                output.or_else(|| {
                    Some(self.value_of(base))
                        .filter(|kind| *kind != Kind::Plain && !self.is_known_future(base))
                })
            }
            _ => None,
        }
    }

    /// What `.await` on `e` gives, when `e` is an `async` block, an invoked
    /// closure, a name bound to one, or an `if`, `match` or block whose
    /// tails are: `{ async move { repo } }` gives the handle.
    fn output_of(&self, e: &Expr) -> Option<Kind> {
        match peel_parens(e) {
            Expr::Async(a) => Some(self.async_output(&a.block)),
            // `(|| async { &repo })()`: what its `async` block gives. A stored
            // callee (`let make = move || async move { repo }; make()`) gives
            // at most what it holds.
            Expr::Call(call) => self.invoked(call, true).or_else(|| {
                path_ident(&call.func)
                    .map(|name| self.env.binding(&name).kind)
                    .filter(|kind| *kind != Kind::Plain)
            }),
            Expr::Path(_) => path_ident(e).and_then(|name| self.env.binding(&name).output),
            other @ (Expr::If(_)
            | Expr::Match(_)
            | Expr::Block(_)
            | Expr::Unsafe(_)
            | Expr::Loop(_)) => value_tails(other)
                .into_iter()
                .filter_map(|tail| self.output_of(tail))
                .max(),
            _ => None,
        }
    }

    /// What an immediately invoked closure gives. Each parameter holds what
    /// its argument holds.
    fn invoked(&self, call: &ExprCall, awaited: bool) -> Option<Kind> {
        let closure = immediately_invoked_closure(&call.func)?;
        let params: Vec<Kind> = call.args.iter().map(|a| self.value_of(a)).collect();
        Some(self.closure_value(closure, &params, Kind::Plain, awaited))
    }

    /// What `closure` returns when parameter `i` holds `params[i]`, or
    /// `rest` past the end. When it is `awaited`, a body of `async { … }`
    /// gives what its block gives.
    fn closure_value(
        &self,
        closure: &syn::ExprClosure,
        params: &[Kind],
        rest: Kind,
        awaited: bool,
    ) -> Kind {
        let mut probe = self.probe();
        probe.env.push();
        for (i, input) in closure.inputs.iter().enumerate() {
            probe.bind_pat(input, params.get(i).copied().unwrap_or(rest));
        }
        let tail = match &*closure.body {
            Expr::Block(b) => probe.block_value(&b.block),
            Expr::Async(a) if awaited => probe.block_value(&a.block),
            body => {
                // Run the body for its `return`s, then read its value.
                let _ = probe.expr(body);
                probe.result_value(body)
            }
        };
        tail.max(probe.returned)
    }

    /// Run a block's statements, then give what its tail expression holds.
    fn block_value(&mut self, block: &Block) -> Kind {
        self.scoped(|s| {
            let mut kind = Kind::Plain;
            for (i, stmt) in block.stmts.iter().enumerate() {
                match stmt {
                    Stmt::Expr(tail, None) if i + 1 == block.stmts.len() => {
                        // Run the tail for its `return`s, then read its value.
                        let _ = s.expr(tail);
                        kind = s.result_value(tail);
                    }
                    _ => {
                        s.stmt(stmt);
                    }
                }
            }
            kind
        })
    }

    /// What a callback gives back in `e`. A query built there is counted
    /// there, so its result is plain, not a handle.
    fn result_value(&self, e: &Expr) -> Kind {
        if self.is_counted_query_future(e) {
            Kind::Plain
        } else {
            self.value_of(e)
        }
    }

    /// Does the closure `c` capture a handle: a tracked name, or a handle
    /// accessor (`state.repo`, `self.db()`) on a value it did not get as a
    /// parameter? Its own parameters are not captures (`|repo| repo.len()`).
    fn closure_captures_handle(&self, c: &syn::ExprClosure) -> bool {
        /// Finds an accessor whose root is not a closure parameter.
        struct Accessors<'p> {
            params: &'p [String],
            found: bool,
        }
        impl Accessors<'_> {
            fn outside(&self, base: &Expr) -> bool {
                place_root(base).is_none_or(|root| !self.params.contains(&root))
            }
        }
        impl<'a> Visit<'a> for Accessors<'_> {
            fn visit_expr_field(&mut self, f: &'a syn::ExprField) {
                self.found |= member_is_handle_accessor(&f.member) && self.outside(&f.base);
                syn::visit::visit_expr_field(self, f);
            }
            fn visit_expr_method_call(&mut self, m: &'a ExprMethodCall) {
                self.found |= HANDLE_ACCESSORS.contains(&m.method.to_string().as_str())
                    && self.outside(&m.receiver);
                syn::visit::visit_expr_method_call(self, m);
            }
        }
        let params = pattern_names(c.inputs.iter());
        let mut free = FreeNames {
            scopes: vec![c.inputs.iter().flat_map(bound_names).collect()],
            tracked: &|name| self.env.is_tracked(name),
            found: false,
        };
        free.visit_expr(&c.body);
        if free.found {
            return true;
        }
        let mut accessors = Accessors {
            params: &params,
            found: false,
        };
        accessors.visit_expr(&c.body);
        accessors.found
    }

    /// Is `e` a user value that holds a handle (`Ctx { repo }`)? Its methods
    /// are the user's, so none of them is a known container method.
    fn expr_is_holder(&self, e: &Expr) -> bool {
        if let Some(kind) = self.produced(e) {
            return kind == Kind::Holder;
        }
        match e {
            Expr::Struct(st) => {
                !self.expr_is_handle(e)
                    && (st.fields.iter().any(|f| self.holds(&f.expr))
                        || st.rest.as_deref().is_some_and(|r| self.holds(r)))
            }
            Expr::Path(_) => path_ident(e).is_some_and(|name| self.env.get(&name) == Kind::Holder),
            Expr::Field(f) => self.part_kind(f) == Some(Kind::Holder),
            // `ctx.clone()` has the type of `ctx`.
            Expr::MethodCall(mc) => {
                (SAME_TYPE_METHODS.contains(&mc.method.to_string().as_str())
                    && self.expr_is_holder(&mc.receiver))
                    || self.callback_result(mc) == Kind::Holder
            }
            // `Ctx(repo)`: a user tuple struct that holds the handle.
            Expr::Call(c) => {
                call_path_name(c).is_some_and(|n| n.starts_with(char::is_uppercase))
                    && !self.is_container_constructor(c)
                    && !is_handle_constructor(c)
                    && c.args.iter().any(|a| self.holds(a))
            }
            Expr::Loop(_) | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                break_results(e).into_iter().any(|v| self.expr_is_holder(v))
            }
            // `{ move || repo }`, `if flag { Ctx { repo } } else { … }`.
            Expr::If(_) | Expr::Match(_) | Expr::Block(_) | Expr::Unsafe(_) => {
                value_tails(e).into_iter().any(|v| self.expr_is_holder(v))
            }
            // A closure that captures a handle holds it, like a user value.
            Expr::Closure(c) => self.closure_captures_handle(c),
            // A future that gives a handle holds it: `async { &repo }`.
            Expr::Async(a) => {
                tokens_mention_any(&a.block.to_token_stream(), &|name| {
                    self.env.is_tracked(name)
                }) && self.async_output(&a.block) != Kind::Plain
            }
            Expr::Reference(r) => self.expr_is_holder(&r.expr),
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => self.expr_is_holder(&u.expr),
            Expr::Paren(p) => self.expr_is_holder(&p.expr),
            Expr::Group(g) => self.expr_is_holder(&g.expr),
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
        if self.expr_is_nested(expr) {
            return false;
        }
        if let Some(kind) = self.produced(expr) {
            return kind.is_handle();
        }
        match expr {
            Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.env.get(&i.to_string()).is_handle()),
            Expr::Reference(r) => self.expr_is_handle(&r.expr),
            Expr::RawAddr(r) => self.expr_is_handle(&r.expr),
            Expr::Cast(c) => self.expr_is_handle(&c.expr),
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
                (self.is_smart_pointer_new(c)
                    && c.args.first().is_some_and(|a| self.expr_is_handle(a)))
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
            Expr::Loop(_) | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                break_results(expr)
                    .into_iter()
                    .any(|v| self.expr_is_handle(v))
            }
            Expr::Block(b) => self.block_tail_is_handle(&b.block),
            Expr::Unsafe(u) => self.block_tail_is_handle(&u.block),
            _ => false,
        }
    }

    /// Does this `map` (or an `Option`'s `and_then`) replace every part of an
    /// `Option` or an iterator with its closure's output? Then the result
    /// holds only what the closure returns (`Some(repo).map(|_| 1)` is
    /// plain). A `Result`'s `map` and `and_then` keep its `Err` side.
    fn maps_away(&self, mc: &ExprMethodCall) -> bool {
        let shape = self.shape_of(&mc.receiver);
        matches!(mc.args.last(), Some(Expr::Closure(_)))
            && match mc.method.to_string().as_str() {
                "map" => matches!(
                    shape,
                    Some(Shape::Opt | Shape::OptRef | Shape::Iter | Shape::IterRef)
                ),
                // `Option::and_then` gives what its closure returns.
                "and_then" => matches!(shape, Some(Shape::Opt | Shape::OptRef)),
                _ => false,
            }
            || self.keeps_plain_side(mc)
    }

    /// Does `mc` map one side of a named `Result` and keep a side that is
    /// plain? `result.map(|_| 1)` keeps the `Err` side.
    fn keeps_plain_side(&self, mc: &ExprMethodCall) -> bool {
        if self.shape_of(&mc.receiver) != Some(Shape::Res) {
            return false;
        }
        let kept = match mc.method.to_string().as_str() {
            "map" | "and_then" => "Err",
            "map_err" | "or_else" => "Ok",
            _ => return false,
        };
        self.side_kind(&mc.receiver, kept) == Some(Kind::Plain)
    }

    /// What callback `arg` of `mc` takes when the receiver is a named
    /// `Result` with known sides: `result.map_err(|e| …)` takes the error.
    fn side_param(&self, mc: &ExprMethodCall, arg: usize) -> Option<Kind> {
        if self.shape_of(&mc.receiver) != Some(Shape::Res) {
            return None;
        }
        let side = callback_side(&mc.method.to_string(), arg, mc.args.len())?;
        self.side_kind(&mc.receiver, side)
    }

    /// What side `side` (`Ok` or `Err`) of a named `Result` with known sides
    /// holds: `result`, `result.as_ref()`, `result.as_mut()`.
    fn side_kind(&self, e: &Expr, side: &str) -> Option<Kind> {
        match (side, self.sides_of(e)) {
            ("Ok", Some((ok, _))) => Some(ok),
            ("Err", Some((_, err))) => Some(err),
            _ => self.env.part(&wrapper_root(e)?, side),
        }
    }

    /// What the `Ok` and `Err` sides of a `Result` hold, when known: a named
    /// `Result`, through `as_ref`, `as_mut`, `inspect`, `inspect_err` and
    /// `clone`, and after `map` or `map_err`.
    fn sides_of(&self, e: &Expr) -> Option<(Kind, Kind)> {
        let mc = match peel_refs(e) {
            Expr::MethodCall(mc) => mc,
            // `Ok::<T, E>(x)`: the other side holds nothing.
            Expr::Call(c) if self.is_container_constructor(c) => {
                let held = c.args.first().map_or(Kind::Plain, |a| self.value_of(a));
                return match call_path_name(c).as_deref() {
                    Some("Ok") => Some((held, Kind::Plain)),
                    Some("Err") => Some((Kind::Plain, held)),
                    _ => None,
                };
            }
            other => {
                let name = path_ident(other)?;
                return Some((self.env.part(&name, "Ok")?, self.env.part(&name, "Err")?));
            }
        };
        let method = mc.method.to_string();
        let callback = |side: Kind| match mc.args.last() {
            Some(f @ Expr::Closure(_)) => Some(self.closure_output(f, side)),
            _ => None,
        };
        match method.as_str() {
            "as_ref" | "as_mut" | "inspect" | "inspect_err" | "clone" => {
                self.sides_of(&mc.receiver)
            }
            "map" => {
                let (ok, err) = self.sides_of(&mc.receiver)?;
                Some((callback(ok)?, err))
            }
            "map_err" => {
                let (ok, err) = self.sides_of(&mc.receiver)?;
                Some((ok, callback(err)?))
            }
            // The callback gives a `Result`: `and_then` keeps the old error
            // side too, `or_else` the old value side.
            "and_then" => {
                let (ok, err) = self.sides_of(&mc.receiver)?;
                let (new_ok, new_err) = self.callback_sides(mc.args.last()?, ok)?;
                Some((new_ok, err.max(new_err)))
            }
            "or_else" => {
                let (ok, err) = self.sides_of(&mc.receiver)?;
                let (new_ok, new_err) = self.callback_sides(mc.args.last()?, err)?;
                Some((ok.max(new_ok), new_err))
            }
            _ => None,
        }
    }

    /// The sides of the `Result` a callback gives when its parameter holds
    /// `param`: its tail is `Ok(x)`, `Err(x)` or a `Result` with known
    /// sides. `None` for a body with statements or a `return`.
    fn callback_sides(&self, f: &Expr, param: Kind) -> Option<(Kind, Kind)> {
        let Expr::Closure(closure) = f else {
            return None;
        };
        let tail = match &*closure.body {
            Expr::Block(b) if b.block.stmts.len() == 1 => block_tail(&b.block)?,
            Expr::Block(_) => return None,
            body => body,
        };
        let mut probe = self.probe();
        probe.env.push();
        for input in &closure.inputs {
            probe.bind_pat(input, param);
        }
        let _ = probe.expr(tail);
        if probe.returned != Kind::Plain {
            return None;
        }
        probe.sides_of(peel_parens(tail))
    }

    /// Is `e` a database connection: a handle with the `Db` shape? A
    /// repository is not one, so a `RunnerRepository::tx` may run its
    /// callback many times. A `LazyDb` is not one either: it has only
    /// `checkout`, so its `tx` is a user's.
    fn is_connection(&self, e: &Expr) -> bool {
        self.value_of(e) == Kind::Handle && self.shape_of(e) == Some(Shape::Db)
    }

    /// Does this call give an `Option` of a part (`deque.remove(0)`)?
    fn gives_option_of_part(&self, mc: &ExprMethodCall) -> bool {
        let method = mc.method.to_string();
        self.shape_of(&mc.receiver)
            .is_some_and(|shape| shape.option_of_part(&method))
    }

    /// Does this method call evaluate to a handle?
    fn method_is_handle(&self, mc: &ExprMethodCall) -> bool {
        let method = mc.method.to_string();
        if HANDLE_ACCESSORS.contains(&method.as_str()) || self.callback_result(mc) == Kind::Handle {
            return true;
        }
        // A method on a carrier that returns a part: `repos.remove(0)`.
        if self.expr_is_carrier(&mc.receiver) {
            return ELEMENT_METHODS.contains(&method.as_str())
                && !OUTPUT_CALLBACKS.contains(&method.as_str())
                && !self.gives_option_of_part(mc)
                && !self
                    .shape_of(&mc.receiver)
                    .is_some_and(|shape| shape.scalar_result(&method));
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
            Expr::Cast(c) => self.awaited_expr_is_fresh_handle(&c.expr),
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
                // `checkout` on any handle gives a handle: a `LazyDb` taken
                // from a container (`[lazy].into_iter().next()`) is a plain
                // handle here, and its connection must stay tracked.
                HANDLE_ACCESSORS.contains(&method.as_str())
                    || (HANDLE_TRANSITIONS.contains(&method.as_str())
                        && (self.expr_is_lazy_db(&mc.receiver)
                            || self.expr_is_handle(&mc.receiver)))
            }
            _ => false,
        }
    }

    /// Is this expression known to be a `LazyDb`, not only some handle?
    /// Narrow on purpose: a name convention (`HANDLE_ACCESSORS`) says "some
    /// handle", never "a `LazyDb`". A missed `LazyDb` only makes its
    /// `checkout` count as a query.
    fn expr_is_lazy_db(&self, expr: &Expr) -> bool {
        if let Some(kind) = self.produced(expr) {
            return kind == Kind::LazyDb;
        }
        match expr {
            Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.env.get(&i.to_string()) == Kind::LazyDb),
            Expr::Reference(r) => self.expr_is_lazy_db(&r.expr),
            Expr::RawAddr(r) => self.expr_is_lazy_db(&r.expr),
            Expr::Cast(c) => self.expr_is_lazy_db(&c.expr),
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
            Expr::Loop(_) | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                break_results(expr)
                    .into_iter()
                    .any(|v| self.expr_is_lazy_db(v))
            }
            Expr::Block(b) => self.block_tail_is_lazy_db(&b.block),
            Expr::Unsafe(u) => self.block_tail_is_lazy_db(&u.block),
            _ => false,
        }
    }

    /// Does this expression evaluate to a value that holds handles without
    /// being one? The container rule (#2316):
    ///
    /// * an array, tuple, `vec!` or `Some`/`Ok`/`Err` that holds a handle is a
    ///   carrier; a user struct literal that holds one is a holder
    ///   ([`Self::expr_is_holder`]), which this also accepts;
    /// * on a carrier, a method in `CARRIER_METHODS` gives a carrier
    ///   (`repos.iter()`), one in `ELEMENT_METHODS` gives a handle
    ///   (`repos.remove(0)`), one in `SCALAR_METHODS` gives a plain value
    ///   (`repos.len()`), and any other method is reported;
    /// * an index, a field, a pattern or `?` on a carrier gives a handle (see
    ///   [`Self::expr_is_handle`]).
    fn expr_is_carrier(&self, expr: &Expr) -> bool {
        if self.expr_is_nested(expr) {
            return true;
        }
        if let Some(kind) = self.produced(expr) {
            return matches!(kind, Kind::Carrier | Kind::Holder);
        }
        let holds = |e: &Expr| self.holds(e);
        match expr {
            Expr::Path(p) => p.path.get_ident().is_some_and(|i| {
                matches!(self.env.get(&i.to_string()), Kind::Carrier | Kind::Holder)
            }),
            Expr::Reference(r) => self.expr_is_carrier(&r.expr),
            Expr::RawAddr(r) => self.expr_is_carrier(&r.expr),
            Expr::Cast(c) => self.expr_is_carrier(&c.expr),
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => self.expr_is_carrier(&u.expr),
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
                (self.is_container_constructor(c) || self.is_smart_pointer_new(c))
                    && c.args.iter().any(holds)
            }
            Expr::Field(f) => matches!(self.part_kind(f), Some(Kind::Carrier | Kind::Holder)),
            Expr::Macro(m) => self
                .std_vec(&m.mac)
                .is_some_and(|elems| elems.iter().any(holds)),
            Expr::MethodCall(mc) => {
                ((CARRIER_METHODS.contains(&mc.method.to_string().as_str())
                    || self.gives_option_of_part(mc))
                    && !self.maps_away(mc)
                    && self.expr_is_carrier(&mc.receiver))
                    // `ids.iter().map(|_| &repo)` gives handles.
                    || self.callback_result(mc) == Kind::Carrier
                    // `chain` yields the parts of either side.
                    || (mc.method == "chain" && mc.args.iter().any(|a| self.expr_is_carrier(a)))
            }
            Expr::If(i) => {
                block_tail(&i.then_branch).is_some_and(|e| self.expr_is_carrier(e))
                    || i.else_branch
                        .as_ref()
                        .is_some_and(|(_, e)| self.expr_is_carrier(e))
            }
            Expr::Match(m) => m.arms.iter().any(|arm| self.expr_is_carrier(&arm.body)),
            Expr::Loop(_) | Expr::Block(syn::ExprBlock { label: Some(_), .. }) => {
                break_results(expr)
                    .into_iter()
                    .any(|v| self.expr_is_carrier(v))
            }
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
        // `expr_is_carrier` also covers a nested value. A holder includes a
        // closure that captures a handle (`drive(|| &repo)`).
        if self.expr_is_handle(expr) || self.expr_is_carrier(expr) || self.expr_is_holder(expr) {
            return true;
        }
        match expr {
            Expr::Struct(s) => s.fields.iter().any(|f| self.expr_carries_handle(&f.expr)),
            Expr::Tuple(t) => t.elems.iter().any(|e| self.expr_carries_handle(e)),
            Expr::Array(a) => a.elems.iter().any(|e| self.expr_carries_handle(e)),
            Expr::Call(c) => c.args.iter().any(|a| self.expr_carries_handle(a)),
            Expr::Reference(r) => self.expr_carries_handle(&r.expr),
            Expr::RawAddr(r) => self.expr_carries_handle(&r.expr),
            Expr::Cast(c) => self.expr_carries_handle(&c.expr),
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => {
                self.expr_carries_handle(&u.expr)
            }
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

/// What a `loop` or a labeled block gives: the block's tail, and the value
/// of each `break` that lands on it. Empty for any other expression.
fn break_results(expr: &Expr) -> Vec<&Expr> {
    /// Collects the `break` values that land on one target.
    struct Breaks<'a> {
        label: Option<String>,
        /// An unlabeled `break` lands here: the target is a loop, and no
        /// inner loop is open.
        unlabeled: bool,
        values: Vec<&'a Expr>,
    }
    impl<'a> Visit<'a> for Breaks<'a> {
        fn visit_expr_break(&mut self, b: &'a syn::ExprBreak) {
            let lands = b.label.as_ref().map_or(self.unlabeled, |l| {
                self.label.as_deref() == Some(l.ident.to_string().as_str())
            });
            if lands && let Some(value) = &b.expr {
                self.values.push(value);
            }
            syn::visit::visit_expr_break(self, b);
        }
        fn visit_expr_loop(&mut self, l: &'a syn::ExprLoop) {
            let unlabeled = std::mem::replace(&mut self.unlabeled, false);
            syn::visit::visit_expr_loop(self, l);
            self.unlabeled = unlabeled;
        }
        fn visit_expr_while(&mut self, w: &'a syn::ExprWhile) {
            let unlabeled = std::mem::replace(&mut self.unlabeled, false);
            syn::visit::visit_expr_while(self, w);
            self.unlabeled = unlabeled;
        }
        fn visit_expr_for_loop(&mut self, f: &'a syn::ExprForLoop) {
            let unlabeled = std::mem::replace(&mut self.unlabeled, false);
            syn::visit::visit_expr_for_loop(self, f);
            self.unlabeled = unlabeled;
        }
        // A `break` cannot leave a closure, an async block or an item.
        fn visit_expr_closure(&mut self, _: &'a syn::ExprClosure) {}
        fn visit_expr_async(&mut self, _: &'a syn::ExprAsync) {}
        fn visit_item(&mut self, _: &'a syn::Item) {}
    }
    let (block, label, unlabeled) = match expr {
        Expr::Loop(l) => (&l.body, &l.label, true),
        Expr::Block(b) if b.label.is_some() => (&b.block, &b.label, false),
        _ => return Vec::new(),
    };
    let mut breaks = Breaks {
        label: label.as_ref().map(|l| l.name.ident.to_string()),
        unlabeled,
        values: Vec::new(),
    };
    breaks.visit_block(block);
    if !unlabeled {
        breaks.values.extend(block_tail(block));
    }
    breaks.values
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
fn is_container_constructor_path(call: &ExprCall) -> bool {
    let Expr::Path(path) = &*call.func else {
        return false;
    };
    let mut segments = path.path.segments.iter().rev();
    // `Some`, `Option::Some`, `std::result::Result::Ok`; not `Factory::Some`.
    segments
        .next()
        .is_some_and(|s| CONTAINER_CONSTRUCTORS.contains(&s.ident.to_string().as_str()))
        && segments.all(|s| {
            matches!(
                s.ident.to_string().as_str(),
                "std" | "core" | "option" | "result" | "Option" | "Result"
            )
        })
}

/// The argument counts of the method `method` of the standard container
/// `shape`. A call with another count is a trait method of that name:
/// `repos.push()`, or `repos.insert(id)` on a `Vec`.
fn std_arities(shape: Shape, method: &str) -> &'static [usize] {
    let keyed = matches!(shape, Shape::Map | Shape::SortedMap);
    let sequence = matches!(
        shape,
        Shape::Vec | Shape::Deque | Shape::Slice | Shape::Array
    );
    match method {
        // `Vec::insert(i, x)`, `HashMap::insert(k, v)`; `HashSet::insert(x)`.
        "insert" if sequence || keyed => &[2],
        // `Vec::drain(range)`; `HashMap::drain()`.
        "drain" if sequence => &[1],
        // `Iterator::take(n)`; `Option::take()`.
        "take" if matches!(shape, Shape::Iter | Shape::IterRef) => &[1],
        "fold" | "try_fold" | "map_or" | "map_or_else" | "swap" | "resize" => &[2],
        "insert"
        | "all"
        | "any"
        | "and_then"
        | "append"
        | "chain"
        | "chunks"
        | "contains"
        | "contains_key"
        | "expect"
        | "expect_err"
        | "filter"
        | "filter_map"
        | "find"
        | "find_map"
        | "flat_map"
        | "for_each"
        | "get"
        | "get_mut"
        | "get_or_insert_with"
        | "inspect"
        | "inspect_err"
        | "is_err_and"
        | "is_none_or"
        | "is_ok_and"
        | "is_some_and"
        | "map"
        | "map_err"
        | "max_by"
        | "max_by_key"
        | "min_by"
        | "min_by_key"
        | "nth"
        | "ok_or"
        | "ok_or_else"
        | "or"
        | "or_else"
        | "position"
        | "push"
        | "push_back"
        | "push_front"
        | "reduce"
        | "remove"
        | "replace"
        | "reserve"
        | "retain"
        | "skip"
        | "skip_while"
        | "sort_by"
        | "sort_by_key"
        | "sort_unstable_by"
        | "sort_unstable_by_key"
        | "step_by"
        | "swap_remove"
        | "take_while"
        | "then"
        | "then_some"
        | "truncate"
        | "try_for_each"
        | "unwrap_or"
        | "unwrap_or_else"
        | "windows"
        | "xor"
        | "zip" => &[1],
        _ => &[0],
    }
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

/// The expressions of a macro body that reads as a comma list or as
/// `elem; len`. Empty when it does not parse.
fn macro_exprs(mac: &syn::Macro) -> Vec<Expr> {
    let list =
        mac.parse_body_with(syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated);
    if let Ok(list) = list {
        return list.into_iter().collect();
    }
    mac.parse_body_with(|input: ParseStream| {
        let elem: Expr = input.parse()?;
        let _: syn::Token![;] = input.parse()?;
        let _: Expr = input.parse()?;
        Ok(vec![elem])
    })
    .unwrap_or_default()
}

/// Is this call `Box::new(x)`, `Arc::new(x)` or `Rc::new(x)`?
fn is_smart_pointer_new_path(call: &ExprCall) -> bool {
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
fn member_is_db_accessor(member: &syn::Member) -> bool {
    match member {
        syn::Member::Named(ident) => DB_ACCESSORS.contains(&ident.to_string().as_str()),
        syn::Member::Unnamed(_) => false,
    }
}

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

/// Is `op` a compound assignment: `+=`, `-=`, `|=`, …?
const fn is_compound_assign(op: &syn::BinOp) -> bool {
    matches!(
        op,
        syn::BinOp::AddAssign(_)
            | syn::BinOp::SubAssign(_)
            | syn::BinOp::MulAssign(_)
            | syn::BinOp::DivAssign(_)
            | syn::BinOp::RemAssign(_)
            | syn::BinOp::BitXorAssign(_)
            | syn::BinOp::BitAndAssign(_)
            | syn::BinOp::BitOrAssign(_)
            | syn::BinOp::ShlAssign(_)
            | syn::BinOp::ShrAssign(_)
    )
}

/// Does `pat` match every value: `_`, or a lowercase binding?
fn pattern_always_matches(pat: &Pat) -> bool {
    match pat {
        Pat::Wild(_) => true,
        // A bare name may be a constant (`const foo: u8 = 1;`). Only `ref`
        // or `mut` makes it a binding for certain.
        Pat::Ident(id) => id.subpat.is_none() && (id.by_ref.is_some() || id.mutability.is_some()),
        Pat::Paren(p) => pattern_always_matches(&p.pat),
        Pat::Type(p) => pattern_always_matches(&p.pat),
        _ => false,
    }
}

/// The name under a wrapper value: `result`, `&result`, `result.as_ref()`,
/// `maybe.as_mut()`.
fn wrapper_root(e: &Expr) -> Option<String> {
    match peel_refs(e) {
        Expr::MethodCall(mc) if matches!(mc.method.to_string().as_str(), "as_ref" | "as_mut") => {
            wrapper_root(&mc.receiver)
        }
        other => path_ident(other),
    }
}

/// `e` without the parentheses and groups around it.
fn peel_parens(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(p) => peel_parens(&p.expr),
        Expr::Group(g) => peel_parens(&g.expr),
        other => other,
    }
}

/// `e` without the `&`, `&mut` and parentheses around it.
fn peel_refs(e: &Expr) -> &Expr {
    match e {
        Expr::Reference(r) => peel_refs(&r.expr),
        Expr::Paren(p) => peel_refs(&p.expr),
        Expr::Group(g) => peel_refs(&g.expr),
        other => other,
    }
}

/// The names that the patterns `pats` bind.
fn pattern_names<'a>(pats: impl Iterator<Item = &'a Pat>) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'a> Visit<'a> for Names {
        fn visit_pat_ident(&mut self, p: &'a syn::PatIdent) {
            self.0.push(p.ident.to_string());
            syn::visit::visit_pat_ident(self, p);
        }
    }
    let mut names = Names(Vec::new());
    for pat in pats {
        names.visit_pat(pat);
    }
    names.0
}

/// Finds a tracked name that a closure body reads from outside: a local
/// `let`, a match arm, a `for` pattern or a nested closure parameter
/// shadows the outer name. An `if let` binding does not shadow, so the
/// result can over-count. A macro is read as tokens.
struct FreeNames<'t> {
    scopes: Vec<Vec<String>>,
    tracked: &'t dyn Fn(&str) -> bool,
    found: bool,
}

impl FreeNames<'_> {
    fn is_free(&self, name: &str) -> bool {
        !self
            .scopes
            .iter()
            .any(|scope| scope.iter().any(|n| n == name))
            && (self.tracked)(name)
    }

    fn scoped(&mut self, names: Vec<String>, body: impl FnOnce(&mut Self)) {
        self.scopes.push(names);
        body(self);
        self.scopes.pop();
    }
}

impl<'a> Visit<'a> for FreeNames<'_> {
    fn visit_expr_path(&mut self, p: &'a syn::ExprPath) {
        if p.qself.is_none()
            && let Some(name) = p.path.get_ident()
        {
            self.found |= self.is_free(&name.to_string());
        }
    }
    fn visit_macro(&mut self, m: &'a syn::Macro) {
        self.found |= tokens_mention_any(&m.tokens, &|name| self.is_free(name));
    }
    fn visit_block(&mut self, b: &'a Block) {
        self.scoped(Vec::new(), |s| syn::visit::visit_block(s, b));
    }
    // The initializer reads the outer name. The pattern shadows it after.
    fn visit_local(&mut self, l: &'a syn::Local) {
        if let Some(init) = &l.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
        let names = bound_names(&l.pat);
        if let Some(scope) = self.scopes.last_mut() {
            scope.extend(names);
        }
    }
    // A guard is part of the arm pattern, and it sees the arm's names.
    fn visit_arm(&mut self, a: &'a syn::Arm) {
        self.scoped(bound_names(&a.pat), |s| {
            s.visit_pat(&a.pat);
            s.visit_expr(&a.body);
        });
    }
    fn visit_expr_for_loop(&mut self, f: &'a syn::ExprForLoop) {
        self.visit_expr(&f.expr);
        self.scoped(bound_names(&f.pat), |s| s.visit_block(&f.body));
    }
    fn visit_expr_closure(&mut self, c: &'a syn::ExprClosure) {
        let names = c.inputs.iter().flat_map(bound_names).collect();
        self.scoped(names, |s| s.visit_expr(&c.body));
    }
}

/// The most that the guards one value can run may cost. A value runs the
/// guard of every earlier arm whose pattern it matches. A guard with no
/// variant key (`_`, a binding, a constant, a user variant) may always
/// run. Of the guards on the std variants (`Some(_)`, `None`) or on
/// literals, a value runs those of one variant or value only.
fn guard_prefix<'a>(
    guards: impl Iterator<Item = (&'a Pat, &'a Cost)>,
    shadowed: &[String],
) -> Cost {
    let mut keyless = Cost::ZERO;
    let mut variants: Vec<((String, String), Cost)> = Vec::new();
    for (pat, cost) in guards {
        match pattern_key(pat, shadowed) {
            None => keyless = keyless.then(cost.clone()),
            Some(key) => match variants.iter_mut().find(|(k, _)| *k == key) {
                Some((_, sum)) => *sum = sum.clone().then(cost.clone()),
                None => variants.push((key, cost.clone())),
            },
        }
    }
    let mut owners: Vec<(String, Cost)> = Vec::new();
    for ((owner, _), cost) in variants {
        match owners.iter_mut().find(|(o, _)| *o == owner) {
            Some((_, worst)) => *worst = worst.clone().or_worst(cost),
            None => owners.push((owner, cost)),
        }
    }
    owners
        .into_iter()
        .fold(keyless, |sum, (_, cost)| sum.then(cost))
}

/// The `(owner, variant)` a pattern names, for grouping guards: a variant
/// pattern, or a literal under the owner `literal`. `None` for any other
/// pattern, which may match any value.
fn pattern_key(pat: &Pat, shadowed: &[String]) -> Option<(String, String)> {
    match pat {
        Pat::Paren(p) => pattern_key(&p.pat, shadowed),
        Pat::Lit(l) => literal_value(&l.lit).map(|v| ("literal".to_string(), v)),
        other => rooted_variant(other, shadowed),
    }
}

/// Can no value match both `a` and `b`? Only when they name different std
/// variants (`Some(_)` and `None`) or are different literals. A constant
/// (`A`, `Foo::MAX`) or a user variant path (`m::A`) may name the same
/// value as another, so it is never disjoint.
fn patterns_disjoint(a: &Pat, b: &Pat, shadowed: &[String]) -> bool {
    match (a, b) {
        (Pat::Paren(p), _) => patterns_disjoint(&p.pat, b, shadowed),
        (_, Pat::Paren(p)) => patterns_disjoint(a, &p.pat, shadowed),
        (Pat::Or(o), _) => o
            .cases
            .iter()
            .all(|case| patterns_disjoint(case, b, shadowed)),
        (_, Pat::Or(o)) => o
            .cases
            .iter()
            .all(|case| patterns_disjoint(a, case, shadowed)),
        (Pat::Lit(x), Pat::Lit(y)) => {
            matches!((literal_value(&x.lit), literal_value(&y.lit)), (Some(x), Some(y)) if x != y)
        }
        // Two owner paths may name one re-exported enum, so only variants
        // of one written owner are disjoint.
        _ => matches!(
            (rooted_variant(a, shadowed), rooted_variant(b, shadowed)),
            (Some((owner_a, a)), Some((owner_b, b))) if owner_a == owner_b && a != b
        ),
    }
}

/// The value of a literal pattern, tagged with its kind. `0x1` and `1`
/// agree. `None` for a float.
fn literal_value(lit: &syn::Lit) -> Option<String> {
    match lit {
        syn::Lit::Int(i) => Some(format!("int {}", i.base10_digits())),
        syn::Lit::Str(v) => Some(format!("str {}", v.value())),
        syn::Lit::Char(v) => Some(format!("char {}", v.value())),
        syn::Lit::Byte(v) => Some(format!("byte {}", v.value())),
        syn::Lit::Bool(v) => Some(format!("bool {}", v.value)),
        _ => None,
    }
}

/// The std variant a pattern names under a `std` or `core` path:
/// `std::option::Option::Some(_)`. A bare `Some` or `Option::Some` may be
/// a module-level alias of one user variant (`use E::V as Some;`), which
/// the macro cannot see.
fn rooted_variant(pat: &Pat, shadowed: &[String]) -> Option<(String, String)> {
    let path = match pat {
        Pat::TupleStruct(p) => &p.path,
        Pat::Struct(p) => &p.path,
        Pat::Path(p) => &p.path,
        _ => return None,
    };
    let rooted = path
        .segments
        .first()
        .is_some_and(|s| s.ident == "std" || s.ident == "core");
    if rooted {
        pattern_variant(pat, shadowed)
    } else {
        None
    }
}

/// The variant a pattern names, as `(owner, variant)`: a std `Some`,
/// `None`, `Ok` or `Err`. Any other path may be a constant or a re-export
/// of another variant.
fn pattern_variant(pat: &Pat, shadowed: &[String]) -> Option<(String, String)> {
    const STD: &[&str] = &["Some", "None", "Ok", "Err"];
    let path = match pat {
        Pat::TupleStruct(p) => &p.path,
        Pat::Struct(p) => &p.path,
        Pat::Path(p) => &p.path,
        Pat::Ident(p)
            if p.ident == "None"
                && p.subpat.is_none()
                && p.by_ref.is_none()
                && !shadows(shadowed, "None") =>
        {
            return Some(("std".to_string(), "None".to_string()));
        }
        _ => return None,
    };
    let mut segments = path.segments.iter().rev();
    let name = segments.next()?.ident.to_string();
    let owner: Vec<String> = segments.rev().map(|s| s.ident.to_string()).collect();
    let std_owner = owner.iter().all(|s| {
        matches!(
            s.as_str(),
            "std" | "core" | "option" | "result" | "Option" | "Result"
        )
    });
    // An item in the body may give a std spelling to a user variant:
    // `use E::V as Some;`, `enum Option { .. }`.
    if shadows(shadowed, owner.first().unwrap_or(&name)) {
        return None;
    }
    // A module may re-export one variant under two names (`m::A`, `m::B`),
    // so only the std variants are known to differ.
    (STD.contains(&name.as_str()) && std_owner).then(|| ("std".to_string(), name))
}

/// Methods that give a `&mut` into their receiver without the `_mut`
/// suffix: `map.entry(k).or_default()`, `slot.get_or_insert_with(f)`.
const BORROW_MUT_METHODS: &[&str] = &[
    "entry",
    "or_default",
    "or_insert",
    "or_insert_with",
    "or_insert_with_key",
    "get_or_insert",
    "get_or_insert_with",
    "insert_entry",
];

/// Module names on a path to a std item: `std::collections::HashMap`,
/// `std::mem::drop`, `alloc::vec!`.
const STD_PATH: &[&str] = &[
    "std",
    "core",
    "alloc",
    "collections",
    "vec",
    "vec_deque",
    "hash_map",
    "hash_set",
    "btree_map",
    "btree_set",
    "binary_heap",
    "linked_list",
    "option",
    "result",
    "Option",
    "Result",
    "sync",
    "rc",
    "boxed",
    "mem",
    "indexmap",
    "map",
    "set",
];

/// Is every segment of `path` before the last a std module? A bare name
/// is std unless the handler body defines it.
fn std_prefix(path: &syn::Path) -> bool {
    let n = path.segments.len().saturating_sub(1);
    path.segments
        .iter()
        .take(n)
        .all(|s| STD_PATH.contains(&s.ident.to_string().as_str()))
}

/// Is every segment of `path` before its last two a std module? The
/// owner of `std::collections::HashMap::new` is std; that of
/// `custom::Vec::new` is not.
fn std_owner(path: &syn::Path) -> bool {
    let n = path.segments.len().saturating_sub(2);
    path.segments
        .iter()
        .take(n)
        .all(|s| STD_PATH.contains(&s.ident.to_string().as_str()))
}

/// The names that items in `block` define or import: `macro_rules! vec`,
/// `fn drop`, `struct Vec`, `use x::format as fmt` (`fmt`).
/// A glob import gives `*`.
fn local_names(block: &Block) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'a> Visit<'a> for Names {
        fn visit_item_macro(&mut self, m: &'a syn::ItemMacro) {
            if let Some(ident) = &m.ident {
                self.0.push(ident.to_string());
            }
        }
        fn visit_item_fn(&mut self, f: &'a ItemFn) {
            self.0.push(f.sig.ident.to_string());
        }
        fn visit_item_struct(&mut self, s: &'a syn::ItemStruct) {
            self.0.push(s.ident.to_string());
        }
        fn visit_item_enum(&mut self, e: &'a syn::ItemEnum) {
            self.0.push(e.ident.to_string());
        }
        fn visit_item_union(&mut self, u: &'a syn::ItemUnion) {
            self.0.push(u.ident.to_string());
        }
        fn visit_item_trait(&mut self, t: &'a syn::ItemTrait) {
            self.0.push(t.ident.to_string());
        }
        fn visit_item_const(&mut self, c: &'a syn::ItemConst) {
            self.0.push(c.ident.to_string());
        }
        fn visit_item_static(&mut self, st: &'a syn::ItemStatic) {
            self.0.push(st.ident.to_string());
        }
        fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
            self.0.push(m.ident.to_string());
        }
        fn visit_item_extern_crate(&mut self, c: &'a syn::ItemExternCrate) {
            let name = c.rename.as_ref().map_or(&c.ident, |(_, rename)| rename);
            self.0.push(name.to_string());
        }
        fn visit_item_type(&mut self, t: &'a syn::ItemType) {
            self.0.push(t.ident.to_string());
        }
        fn visit_use_name(&mut self, u: &'a syn::UseName) {
            self.0.push(u.ident.to_string());
        }
        fn visit_use_rename(&mut self, u: &'a syn::UseRename) {
            self.0.push(u.rename.to_string());
        }
        // A glob import may define any name.
        fn visit_use_glob(&mut self, _: &'a syn::UseGlob) {
            self.0.push("*".to_string());
        }
    }
    let mut names = Names(Vec::new());
    names.visit_block(block);
    names.0
}

/// Does an item in the handler body define or import `name`?
fn shadows(shadowed: &[String], name: &str) -> bool {
    shadowed.iter().any(|n| n == name || n == "*")
}

/// The value a place is inside: `x` for `*x`, `x.field`, `x[i]`.
fn place_base(place: &Expr) -> &Expr {
    match place {
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => place_base(&u.expr),
        Expr::Field(f) => place_base(&f.base),
        Expr::Index(i) => place_base(&i.expr),
        Expr::Paren(p) => place_base(&p.expr),
        other => other,
    }
}

/// `value.member`, built as a node so that no operator in `value` binds
/// the member instead.
fn field_of(value: &Expr, member: syn::Member) -> Expr {
    Expr::Field(syn::ExprField {
        attrs: Vec::new(),
        base: Box::new(value.clone()),
        dot_token: syn::token::Dot::default(),
        member,
    })
}

/// Do `a` and `b` have the same tokens?
fn same_tokens(a: &impl quote::ToTokens, b: &impl quote::ToTokens) -> bool {
    a.to_token_stream().to_string() == b.to_token_stream().to_string()
}

/// The names a pattern binds by `ref mut`.
fn ref_mut_names(pat: &Pat) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'a> Visit<'a> for Names {
        fn visit_pat_ident(&mut self, p: &'a syn::PatIdent) {
            if p.by_ref.is_some() && p.mutability.is_some() {
                self.0.push(p.ident.to_string());
            }
            syn::visit::visit_pat_ident(self, p);
        }
        fn visit_expr(&mut self, _: &'a Expr) {}
    }
    let mut names = Names(Vec::new());
    names.visit_pat(pat);
    names.0
}

/// The type a turbofish gives a std constructor: `Result<T, E>` for
/// `Ok::<T, E>(x)`, `Option<T>` for `Some::<T>(x)` and `None::<T>`, and
/// `Vec<T>` for `Vec::<T>::new()`. `None` when a type is left to inference
/// (`_`).
fn constructor_type(e: &Expr) -> Option<Type> {
    let (path, called) = match peel_parens(e) {
        Expr::Call(call) => match &*call.func {
            Expr::Path(p) => (&p.path, true),
            _ => return None,
        },
        Expr::Path(p) => (&p.path, false),
        _ => return None,
    };
    let mut segments = path.segments.iter().rev();
    let last = segments.next()?;
    let owner = segments.next();
    let args = [Some(last), owner]
        .into_iter()
        .flatten()
        .find_map(|s| match &s.arguments {
            syn::PathArguments::AngleBracketed(a) => Some(a),
            _ => None,
        })?;
    let types: Vec<&Type> = args
        .args
        .iter()
        .filter_map(|a| match a {
            syn::GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .collect();
    let infers = types.iter().any(|ty| type_infers(ty));
    if infers {
        return None;
    }
    let name = last.ident.to_string();
    match (name.as_str(), called, types.as_slice()) {
        ("Some", true, [t]) | ("None", false, [t]) => Some(syn::parse_quote!(Option<#t>)),
        ("Ok" | "Err", true, [t, e]) => Some(syn::parse_quote!(Result<#t, #e>)),
        ("new" | "with_capacity" | "default" | "from" | "from_iter", true, [_, ..]) => {
            let owner = owner?;
            // `custom::Vec::<T>::new()` is a user type.
            if !std_owner(path) {
                return None;
            }
            let collection = &owner.ident;
            CARRIER_TYPES
                .contains(&collection.to_string().as_str())
                .then(|| syn::parse_quote!(#collection<#(#types),*>))
        }
        _ => None,
    }
}

/// The values an `if`, `match`, block or loop can give: [`branch_tails`],
/// and every `break` value that leaves a loop or a labeled block.
fn value_tails(e: &Expr) -> Vec<&Expr> {
    match peel_parens(e) {
        Expr::Unsafe(u) => block_tail(&u.block).into_iter().collect(),
        Expr::Loop(l) => break_values(&l.body, l.label.as_ref(), true),
        Expr::Block(b) => {
            let mut tails: Vec<&Expr> = block_tail(&b.block).into_iter().collect();
            if b.label.is_some() {
                tails.extend(break_values(&b.block, b.label.as_ref(), false));
            }
            tails
        }
        other => branch_tails(other).unwrap_or_default(),
    }
}

/// The values of the `break`s in `body` that leave its loop (`unlabeled`)
/// or the frame with `label`. A `break` in a closure or `async` block
/// cannot leave it.
fn break_values<'a>(body: &'a Block, label: Option<&syn::Label>, unlabeled: bool) -> Vec<&'a Expr> {
    struct Breaks<'a> {
        label: Option<String>,
        unlabeled: bool,
        nested: usize,
        found: Vec<&'a Expr>,
    }
    impl Breaks<'_> {
        fn nested_loop(&mut self, f: impl FnOnce(&mut Self)) {
            self.nested += 1;
            f(self);
            self.nested -= 1;
        }
    }
    impl<'a> Visit<'a> for Breaks<'a> {
        fn visit_expr_break(&mut self, b: &'a syn::ExprBreak) {
            let hits = match &b.label {
                Some(l) => self.label.as_deref() == Some(l.ident.to_string().as_str()),
                None => self.unlabeled && self.nested == 0,
            };
            if hits && let Some(value) = &b.expr {
                self.found.push(value);
            }
            syn::visit::visit_expr_break(self, b);
        }
        fn visit_expr_loop(&mut self, l: &'a syn::ExprLoop) {
            self.nested_loop(|s| syn::visit::visit_expr_loop(s, l));
        }
        fn visit_expr_while(&mut self, w: &'a syn::ExprWhile) {
            self.nested_loop(|s| syn::visit::visit_expr_while(s, w));
        }
        fn visit_expr_for_loop(&mut self, f: &'a syn::ExprForLoop) {
            self.nested_loop(|s| syn::visit::visit_expr_for_loop(s, f));
        }
        fn visit_expr_closure(&mut self, _: &'a syn::ExprClosure) {}
        fn visit_expr_async(&mut self, _: &'a syn::ExprAsync) {}
        fn visit_item(&mut self, _: &'a syn::Item) {}
    }
    let mut breaks = Breaks {
        label: label.map(|l| l.name.ident.to_string()),
        unlabeled,
        nested: 0,
        found: Vec::new(),
    };
    breaks.visit_block(body);
    breaks.found
}

/// The values an `if`, `match` or block can give: each branch tail. `None`
/// when one has no tail (an `if` with no `else`, a block that ends in a
/// statement).
fn branch_tails(e: &Expr) -> Option<Vec<&Expr>> {
    match peel_parens(e) {
        Expr::If(i) => {
            let (_, els) = i.else_branch.as_ref()?;
            let mut tails = vec![block_tail(&i.then_branch)?];
            tails.extend(branch_tails(els)?);
            Some(tails)
        }
        Expr::Match(m) => Some(m.arms.iter().map(|arm| &*arm.body).collect()),
        Expr::Block(b) => Some(vec![block_tail(&b.block)?]),
        _ => None,
    }
}

/// The names a pattern binds. Unlike `pattern_names`, it does not read
/// the expressions in a pattern (a guard), so a closure parameter in a
/// guard does not shadow an outer name.
fn bound_names(pat: &Pat) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'a> Visit<'a> for Names {
        fn visit_pat_ident(&mut self, p: &'a syn::PatIdent) {
            self.0.push(p.ident.to_string());
            syn::visit::visit_pat_ident(self, p);
        }
        fn visit_expr(&mut self, _: &'a Expr) {}
    }
    let mut names = Names(Vec::new());
    names.visit_pat(pat);
    names.0
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
    // A container first: `Vec<Db>` and `Vec<PgPostRepository>` agree.
    match type_depth(ty) {
        _ if type_is_lazy_db(ty) => Kind::LazyDb,
        0 if type_is_handle(ty) => Kind::Handle,
        0 => Kind::Plain,
        1 => Kind::Carrier,
        _ => Kind::Nested,
    }
}

/// How deep handles sit in a container type: 1 for `Vec<PgPostRepository>`,
/// 2 or more for `Vec<Vec<PgPostRepository>>`, 0 for no container of
/// handles. A smart pointer adds no depth.
fn type_depth(ty: &Type) -> u8 {
    let part = |inner: &Type| {
        if type_is_handle_part(inner) {
            1
        } else {
            match type_depth(inner) {
                0 => 0,
                d => d.saturating_add(1),
            }
        }
    };
    match ty {
        Type::Reference(r) => type_depth(&r.elem),
        Type::Ptr(p) => type_depth(&p.elem),
        Type::Paren(p) => type_depth(&p.elem),
        Type::Group(g) => type_depth(&g.elem),
        Type::Array(a) => part(&a.elem),
        Type::Slice(s) => part(&s.elem),
        Type::Tuple(t) => t.elems.iter().map(part).max().unwrap_or(0),
        Type::Path(path) => path.path.segments.last().map_or(0, |segment| {
            let name = segment.ident.to_string();
            let mut args = generic_types(segment);
            if SMART_POINTERS.contains(&name.as_str()) {
                return args.next().map_or(0, type_depth);
            }
            if name == "Result" {
                let ok = args.next();
                let err = args.next().map_or(0, part);
                // A `Result` of a handle is a container of it, like an
                // `Option`.
                if ok.is_some_and(type_is_handle_part) {
                    return err.max(1);
                }
                return ok.map_or(0, part).max(err);
            }
            if CARRIER_TYPES.contains(&name.as_str()) {
                return args.map(part).max().unwrap_or(0);
            }
            0
        }),
        _ => 0,
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
        Type::Ptr(p) => type_is_plain_std(&p.elem),
        Type::Paren(p) => type_is_plain_std(&p.elem),
        Type::Group(g) => type_is_plain_std(&g.elem),
        Type::Array(a) => type_is_plain_std(&a.elem),
        Type::Slice(s) => type_is_plain_std(&s.elem),
        Type::Tuple(t) => t.elems.iter().all(type_is_plain_std),
        // `custom::Vec<i64>` may hold anything.
        Type::Path(path) if path.qself.is_none() && std_prefix(&path.path) => {
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
        Type::Ptr(p) => type_is_handle(&p.elem),
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
        Type::Ptr(p) => type_is_handle_part(&p.elem),
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
        Type::Ptr(p) => type_is_exact_handle(&p.elem),
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
        Type::Ptr(p) => type_is_lazy_db(&p.elem),
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

/// Writes a closure behind `&`, `&mut`, parentheses or a group as the
/// closure itself: `map(&mut |r| …)` reads as `map(|r| …)`.
struct PeelClosureRefs;

impl VisitMut for PeelClosureRefs {
    fn visit_expr_mut(&mut self, e: &mut Expr) {
        syn::visit_mut::visit_expr_mut(self, e);
        if matches!(e, Expr::Reference(_) | Expr::Paren(_) | Expr::Group(_))
            && let Some(closure) = wrapped_closure(e)
        {
            *e = Expr::Closure(closure.clone());
        }
    }
}

/// The closure under `&`, `&mut`, parentheses and groups.
fn wrapped_closure(e: &Expr) -> Option<&syn::ExprClosure> {
    match e {
        Expr::Closure(c) => Some(c),
        Expr::Reference(r) => wrapped_closure(&r.expr),
        Expr::Paren(p) => wrapped_closure(&p.expr),
        Expr::Group(g) => wrapped_closure(&g.expr),
        _ => None,
    }
}

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

    // The analysis reads a copy with `&mut |r| …` written as `|r| …`.
    let mut normalized = input_fn.clone();
    PeelClosureRefs.visit_item_fn_mut(&mut normalized);
    let mut analyzer = Analyzer::new(&normalized);
    let cost = analyzer.function_body(&normalized.block);

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
                // The `return` leaves on the pass that takes it.
                "return in a bounded loop",
                "for _ in 0..2 { if flag { return Ok(repo.a().await?.len()); } }",
                Expect::Exact(1),
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
                "let repos = vec![repo]; if let Some(r) = repos.as_slice().first() { let _ = r.find_all(); }",
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
            // The parts of `st` are not known, so the part is opaque: the
            // query is reported rather than counted.
            (
                "a handle assigned into a nested field",
                "let mut st = State::default(); st.inner.repo = repo; \
                 let _ = st.inner.repo.find_all();",
                Expect::Unbounded,
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
    fn a_user_struct_stays_opaque_through_any_binding() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "alias",
                "let ctx = Ctx { repo }; let alias = ctx; let _ = alias.clear();",
                Expect::Unbounded,
            ),
            (
                "IIFE parameter",
                "(|c| { let _ = c.clear(); })(Ctx { repo });",
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn a_nested_container_keeps_its_shape() {
        // An element of `Vec<Vec<Repo>>` is a `Vec`, so a method on it may run
        // a query per repository. The shape is not tracked past one level,
        // so every method on a nested container is reported.
        for body in [
            "let repos = groups.remove(0); repos.refresh_all().await?;",
            "groups[0].refresh_all().await?;",
        ] {
            let handler = format!(
                "async fn h(mut groups: Vec<Vec<PgPostRepository>>) -> AutumnResult<usize> {{
                    {body}
                    Ok(0)
                }}"
            );
            // `remove` or `refresh_all`: every method on it is reported.
            assert_error_contains("50", &handler, &["is called on a container"]);
        }
        // An array of user structs: every method on an element is reported.
        let holders = matrix_handler("let cs = [Ctx { repo }]; let _ = cs[0].clear();");
        assert_error_contains("50", &holders, &["clear"]);
    }

    #[test]
    fn a_smart_pointer_around_a_container_is_a_container() {
        let handler = r"
            async fn h(repos: Arc<Vec<PgPostRepository>>) -> AutumnResult<usize> {
                for repo in repos.as_slice().iter() { let _ = repo.find_all().await?; }
                Ok(0)
            }
            ";
        assert_error_contains("50", handler, &["loop"]);
    }

    #[test]
    fn a_user_value_stays_opaque_through_its_own_methods() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "exempted clone",
                r#"let ctx = Ctx { repo };
                   #[query_exempt(reason = "clone is pure")]
                   let alias = ctx.clone();
                   let _ = alias.clear();"#,
                Expect::Unbounded,
            ),
            (
                "exempted tuple-struct wrapper",
                r#"#[query_exempt(reason = "wraps only")]
                   let ctx = Ctx(repo);
                   let _ = ctx.clear();"#,
                Expect::Unbounded,
            ),
            (
                "clearing the last tracked part",
                "let mut ctx = Ctx { slot: Some(repo) }; ctx.slot = None; let _ = render(&ctx);",
                Expect::Exact(0),
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn storing_a_handle_makes_the_receiver_hold_it() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "exempted push into a plain Vec",
                r#"let mut repos = Vec::new();
                   #[query_exempt(reason = "pure container operation")]
                   repos.push(repo);
                   for r in repos { let _ = r.find_all(); }"#,
                Expect::Unbounded,
            ),
            (
                "exempted helper filling a &mut argument",
                r#"let mut repos = Vec::new();
                   #[query_exempt(reason = "fills only")]
                   fill(&mut repos, &repo);
                   for r in repos { let _ = r.find_all(); }"#,
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn a_container_method_must_exist_on_that_container() {
        // `ok` is a `Result` method, not a `Vec` one: here it is an extension
        // trait method, which may query.
        let handler = r"
            async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> {
                repos.ok().await;
                Ok(0)
            }
            ";
        assert_error_contains("50", handler, &["ok"]);
    }

    #[test]
    fn a_value_from_a_closed_scope_keeps_its_handle() {
        // Each initializer gives the handle through a name that its own
        // scope declares, or through a `break` value.
        let cases: &[(&str, &str, Expect)] = &[
            (
                "block local",
                "let alias = { let moved = repo; moved }; let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "unsafe block local",
                "let alias = unsafe { let moved = &repo; moved }; let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "if arm local",
                "let alias = if flag { let m = &repo; m } else { let n = &repo; n }; \
                 let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "match arm binding",
                "let alias = match Some(&repo) { Some(r) => r, None => return Ok(0) }; \
                 let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "assignment from a block local",
                "let other = 1; let mut alias = &other; alias = { let m = &repo; m }; \
                 let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "nested block locals",
                "let alias = { let b = { let m = &repo; m }; b }; let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "labeled block break value",
                "let alias = 'pick: { let m = &repo; if flag { break 'pick m; } m }; \
                 let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "loop break value",
                "let alias = loop { let m = &repo; break m; }; let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "loop break value of a container",
                "let list = loop { break vec![&repo]; }; let _ = list[0].find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "labeled block whose break alone gives the handle",
                "let other = 1; let alias = 'pick: { if flag { break 'pick &repo; } &other }; \
                 let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            (
                "labeled break out of an inner loop",
                "let alias = 'outer: loop { for _id in &ids { break 'outer &repo; } }; \
                 let _ = alias.find_all().await?;",
                Expect::Exact(1),
            ),
            // A shadowed name joins both bindings: the safe side.
            (
                "plain block value",
                "let n = { let m = 1; m }; let _ = n + 1;",
                Expect::Exact(0),
            ),
        ];
        check_cases(cases);
    }

    /// The `#[query_budget]` functions in a file, with the attribute
    /// arguments taken off.
    fn budgeted_fns(path: &std::path::Path) -> Vec<(TokenStream, ItemFn)> {
        let src = std::fs::read_to_string(path).expect("fixture reads");
        let file: syn::File = syn::parse_str(&src).expect("fixture parses");
        file.items
            .into_iter()
            .filter_map(|item| {
                let syn::Item::Fn(mut func) = item else {
                    return None;
                };
                let at = func
                    .attrs
                    .iter()
                    .position(|a| a.path().is_ident("query_budget"))?;
                let args = match func.attrs.remove(at).meta {
                    syn::Meta::List(list) => list.tokens,
                    _ => TokenStream::new(),
                };
                Some((args, func))
            })
            .collect()
    }

    /// A fast copy of the trybuild check for the budget fixtures: a
    /// compile-pass fixture and the example expand clean, and each
    /// compile-fail diagnostic is in its `.stderr` file. Trybuild takes about
    /// twenty minutes, so this finds a regression first.
    #[test]
    fn every_budget_fixture_expands_as_trybuild_expects() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let tests = root.join("autumn/tests");
        if !tests.is_dir() {
            // A packaged crate has no workspace.
            return;
        }
        let fixtures = |dir: &str| -> Vec<std::path::PathBuf> {
            let mut paths: Vec<_> = std::fs::read_dir(tests.join(dir))
                .expect("fixture dir reads")
                .map(|e| e.expect("entry reads").path())
                .filter(|p| {
                    p.extension().is_some_and(|x| x == "rs")
                        && p.to_string_lossy().contains("query_budget")
                })
                .collect();
            paths.sort();
            paths
        };
        let mut pass = fixtures("compile-pass");
        pass.push(root.join("examples/bookmarks/src/routes/bookmarks.rs"));
        let fail = fixtures("compile-fail");
        let mut failures = Vec::new();
        let mut checked = 0;
        for (path, must_pass) in pass
            .iter()
            .map(|p| (p, true))
            .chain(fail.iter().map(|p| (p, false)))
        {
            let stderr = std::fs::read_to_string(path.with_extension("stderr")).unwrap_or_default();
            for (args, func) in budgeted_fns(path) {
                checked += 1;
                let mut messages = Vec::new();
                collect_compile_errors(
                    &query_budget_macro(args, func.to_token_stream()),
                    &mut messages,
                );
                let name = format!("{}::{}", path.display(), func.sig.ident);
                if must_pass && !messages.is_empty() {
                    failures.push(format!("{name}: expected clean, got {messages:?}"));
                }
                let missing: Vec<&str> = messages
                    .iter()
                    .flat_map(|m| m.lines())
                    .filter(|line| !must_pass && !stderr.contains(line))
                    .collect();
                if !missing.is_empty() {
                    failures.push(format!("{name}: not in .stderr: {missing:?}"));
                }
            }
        }
        assert!(checked > 20, "only {checked} budget fixtures found");
        assert_matrix(&failures);
    }

    #[test]
    fn an_executor_call_does_not_store_its_connection() {
        // `execute` uses the connection and gives it back. It is one query,
        // and `Query` does not hold the connection after it.
        let handler = "async fn h(lazy_db: LazyDb) -> Result<(), ()> { \
                       let mut db = lazy_db.checkout().await?; \
                       Query.execute(&mut db).await?; \
                       let q = Query; q.execute(&mut db).await?; q.execute(&mut db).await }";
        assert_eq!(check(handler, Expect::Exact(3)), None);
    }

    /// Check each `(name, handler, expect)`.
    fn check_handlers(cases: &[(&str, &str, Expect)]) {
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|(name, handler, expect)| {
                check(handler, *expect).map(|why| format!("{name}: {why}\n  {handler}"))
            })
            .collect();
        assert_matrix(&failures);
    }

    #[test]
    fn a_container_layer_is_kept_through_results_and_slices() {
        check_handlers(&[
            (
                "Err side of a Result with a handle on both sides",
                "async fn h(result: Result<PgPostRepository, PgPostRepository>) \
                 -> AutumnResult<usize> { \
                 if let Err(repo) = result { let _ = repo.find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "Ok side of a Result with a handle on both sides",
                "async fn h(result: Result<PgPostRepository, PgPostRepository>) \
                 -> AutumnResult<usize> { let repo = result?; let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a chunk of a container of handles is a container",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let chunk = repos.chunks(repos.len()).next().unwrap(); \
                 chunk.refresh_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a window of a container of handles is a container",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 for w in repos.windows(2) { w.refresh_all().await?; break; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "both map_or_else arguments are callbacks",
                "async fn h(result: Result<(), PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.map_or_else(query_error, |_| 0); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "append of a Vec of handles keeps a flat Vec",
                "async fn h(mut left: Vec<PgPostRepository>, mut right: Vec<PgPostRepository>) \
                 -> AutumnResult<usize> { left.append(&mut right); left.append(&mut right); \
                 let _ = left[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            // `extend` is a trait method that a user trait can take over: handed
            // a handle, it is opaque.
            (
                "extend handed an Option of a handle is opaque",
                "async fn h(extra: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); left.extend(extra); \
                 let _ = left[0].find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "extend with a map of handles gives tuples",
                "async fn h(mut left: Vec<PgPostRepository>, extra: HashMap<i64, PgPostRepository>) \
                 -> AutumnResult<usize> { left.extend(extra); left[0].refresh_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn code_that_never_runs_binds_nothing() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "a loop with a zero literal bound",
                "let mut slot = None; for _ in 0..0 { slot = Some(&repo); } render(slot);",
                Expect::Exact(0),
            ),
            (
                "a statement after an unconditional break",
                "let mut slot = None; for _id in &ids { break; slot = Some(&repo); } render(slot);",
                Expect::Exact(0),
            ),
            (
                "a statement after a return",
                "let mut slot = None; if flag { return Ok(0); slot = Some(&repo); } render(slot);",
                Expect::Exact(0),
            ),
            // Guards: code that may run still binds.
            (
                "a statement before a conditional break",
                "let mut slot = None; for _id in &ids { slot = Some(&repo); if flag { break; } } \
                 render(slot);",
                Expect::Unbounded,
            ),
            (
                "a loop with a literal bound of one",
                "let mut slot = None; for _ in 0..1 { slot = Some(&repo); } render(slot);",
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn option_and_result_combinators_run_their_callback_once() {
        check_handlers(&[
            (
                "Option::map runs its closure once",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = maybe.map(|r| r.find_all()); Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "Result::and_then runs its closure once",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.ok().and_then(|r| r.find_all()); Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a function passed to or_else is reported",
                "async fn h(result: Result<(), PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.or_else(recover); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a function passed to is_some_and is reported",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = maybe.is_some_and(check); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "unwrap_or does not store its argument",
                "async fn h(maybe: Option<PgPostRepository>, fallback: PgPostRepository) \
                 -> AutumnResult<usize> { let repo = maybe.unwrap_or(fallback); \
                 let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "resize stores its argument",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut list = vec![]; list.resize(2, &repo); render(list); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "replace stores its argument",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(1); slot.replace(&repo); render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
            // Guard: an iterator's `map` still runs per element.
            (
                "Iterator::map runs its closure per element",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = repos.iter().map(|r| r.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn a_callback_result_keeps_its_handle() {
        check_handlers(&[
            (
                "map over plain ids that builds holders",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let ctxs = ids.into_iter().map(|_| Ctx { repo: repo.clone() }) \
                 .collect::<Vec<_>>(); ctxs[0].clear().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map over plain ids that gives handles",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let repos: Vec<_> = ids.iter().map(|_| &repo).collect(); \
                 for r in &repos { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "get_or_insert_with stores the callback result",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; slot.get_or_insert_with(|| Ctx { repo }); \
                 slot.unwrap().clear().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "resize_with stores the callback result",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut list = vec![]; list.resize_with(2, || &repo); render(list); Ok(0) }",
                Expect::Unbounded,
            ),
            // Guards: a plain mapping stays plain.
            (
                "map over plain ids that gives plain values",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let next: Vec<i64> = ids.iter().map(|id| id + 1).collect(); \
                 let wrapped = ids.iter().map(|id| Some(*id)).collect::<Vec<_>>(); \
                 render(wrapped); let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn a_bounded_loop_whose_every_pass_leaves_does_not_fall_through() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "every pass returns",
                "for _ in 0..1 { let _ = repo.a().await?; return Ok(0); } let _ = repo.b().await?;",
                Expect::Exact(1),
            ),
            // Guard: a pass that may go on still reaches the code after.
            (
                "a pass that may go on",
                "for _ in 0..1 { let _ = repo.a().await?; if flag { return Ok(0); } } \
                 let _ = repo.b().await?;",
                Expect::Exact(2),
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn every_callback_output_keeps_its_handle() {
        let over = |body: &str| {
            format!(
                "async fn h(repo: PgPostRepository, ids: Vec<i64>, flag: bool) \
                 -> AutumnResult<usize> {{ {body} Ok(0) }}"
            )
        };
        let each = "for r in &repos { let _ = r.find_all().await?; }";
        let cases = [
            (
                "map_while",
                format!(
                    "let repos: Vec<_> = ids.iter().map_while(|_| Some(&repo)).collect(); {each}"
                ),
                Expect::Unbounded,
            ),
            (
                "find_map",
                "let found = ids.iter().find_map(|_| Some(&repo)); \
                 let _ = found.unwrap().find_all().await?;"
                    .to_string(),
                Expect::Exact(1),
            ),
            (
                "fold",
                format!(
                    "let repos = ids.iter().fold(Vec::new(), |mut v, _| {{ v.push(&repo); v }}); {each}"
                ),
                Expect::Unbounded,
            ),
            (
                "bool::then",
                "let maybe = flag.then(|| &repo); let _ = maybe.unwrap().find_all().await?;"
                    .to_string(),
                Expect::Exact(1),
            ),
            (
                "unwrap_or_else on a plain Option",
                "let r = None.unwrap_or_else(|| &repo); let _ = r.find_all().await?;".to_string(),
                Expect::Exact(1),
            ),
            (
                "explicit return in a map closure",
                format!(
                    "let repos: Vec<_> = ids.iter().map(|_| {{ if flag {{ return &repo; }} \
                     panic!() }}).collect(); {each}"
                ),
                Expect::Unbounded,
            ),
            (
                "explicit return from an inner scope",
                format!(
                    "let repos: Vec<_> = ids.iter().map(|_| {{ if flag {{ let r = &repo; return r; }} \
                     panic!() }}).collect(); {each}"
                ),
                Expect::Unbounded,
            ),
            // Guard: a plain fold stays plain.
            (
                "plain fold",
                "let n = ids.iter().fold(0, |acc, id| acc + id); render(n);".to_string(),
                Expect::Exact(0),
            ),
        ];
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|(name, body, expect)| {
                let handler = over(body);
                check(&handler, *expect).map(|why| format!("{name}: {why}\n  {handler}"))
            })
            .collect();
        assert_matrix(&failures);
    }

    #[test]
    fn a_return_in_a_loop_does_not_reach_the_code_after_it() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "return in a loop over rows",
                "for _id in &ids { if flag { return Ok(repo.a().await?.len()); } } \
                 let _ = repo.b().await?;",
                Expect::Exact(1),
            ),
            (
                "return in a bounded loop",
                "for _ in 0..2 { if flag { let _ = repo.a().await?; return Ok(0); } } \
                 let _ = repo.b().await?;",
                Expect::Exact(1),
            ),
            // Guards: a `break` and every pass still reach the code after.
            (
                "break in a bounded loop",
                "for _ in 0..3 { if flag { let _ = repo.a().await?; break; } } \
                 let _ = repo.b().await?;",
                Expect::Exact(2),
            ),
            (
                "every pass of a bounded loop",
                "for _ in 0..3 { let _ = repo.a().await?; } let _ = repo.b().await?;",
                Expect::Exact(4),
            ),
            (
                "passes before a break",
                "for _ in 0..3 { let _ = repo.a().await?; if flag { break; } } \
                 let _ = repo.b().await?;",
                Expect::Exact(4),
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn zip_and_chain_keep_the_handles_of_either_side() {
        check_handlers(&[
            (
                "a zip item is a tuple",
                "async fn h(repos: Vec<PgPostRepository>, others: Vec<PgPostRepository>) \
                 -> AutumnResult<usize> { \
                 let pair = repos.into_iter().zip(others).next().unwrap(); \
                 pair.refresh_both().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "zip with handles on the argument side",
                "async fn h(ids: Vec<i64>, repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let pair = ids.iter().zip(&repos).next().unwrap(); \
                 pair.refresh_both().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "chain with handles on the argument side",
                "async fn h(ids: Vec<i64>, repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let all: Vec<_> = Vec::new().iter().chain(repos.as_slice().iter()).collect(); \
                 let _ = all[0].find_all().await?; render(ids); Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "chain of plain values stays plain",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let all: Vec<i64> = ids.iter().chain(ids.iter()).copied().collect(); \
                 render(all); let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn closures_by_name_user_methods_and_bool_then_keep_their_kind() {
        check_handlers(&[
            (
                "a closure bound to a name and mapped",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let make = |_| repo.clone(); let repos = ids.into_iter().map(make) \
                 .collect::<Vec<_>>(); for r in &repos { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a closure bound to a name and called",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let make = || repo.clone(); let r = make(); r.refresh().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a user method with a std scalar name on a holder",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let ctx = Ctx { repo }; \
                 #[query_exempt(reason = \"pure builder\")] let alias = ctx.clear(); \
                 alias.refresh().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "bool::then on a bool parameter",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let maybe = flag.then(|| &repo); let _ = maybe.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "bool::then on a comparison",
                "async fn h(repo: PgPostRepository, n: i64) -> AutumnResult<usize> { \
                 let maybe = (n > 3).then(|| &repo); let _ = maybe.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            // Guard: `then` on an unknown receiver gives no `Option` shape.
            (
                "then on an unknown receiver",
                "async fn h(repo: PgPostRepository, ids: Stream) -> AutumnResult<usize> { \
                 let out = ids.then(|_| &repo); let _ = out.map(|r| r.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn transactions_need_a_handle_and_insert_results_follow_the_type() {
        check_handlers(&[
            (
                "a transaction name on a user value",
                "async fn h(repo: PgPostRepository, runner: Runner) -> AutumnResult<usize> { \
                 runner.tx_immediate(|| repo.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                // The transaction is 1, and its callback runs once.
                "a transaction on a handle still runs once",
                "async fn h(mut db: Db) -> AutumnResult<usize> { \
                 db.tx_immediate(|c| async move { let _ = c.find_all().await; }).await; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a transaction function with no connection",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 maybe_immediate_transaction(|| repo.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "Vec::insert gives a unit",
                "async fn h(mut repos: Vec<PgPostRepository>, repo: PgPostRepository) \
                 -> AutumnResult<usize> { let done = repos.insert(0, repo); render(done); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "HashSet::insert and remove give a bool",
                "async fn h(mut repos: HashSet<PgPostRepository>, repo: PgPostRepository) \
                 -> AutumnResult<usize> { let a = repos.insert(repo); \
                 let b = repos.remove(&repo); render(a); render(b); Ok(0) }",
                Expect::Exact(0),
            ),
            // Guard: `Option::insert` gives the part.
            (
                "Option::insert gives the part",
                "async fn h(mut slot: Option<PgPostRepository>, repo: PgPostRepository) \
                 -> AutumnResult<usize> { let r = slot.insert(repo); \
                 let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn closures_errors_and_tuple_items_keep_their_handles() {
        check_handlers(&[
            (
                "a closure that captures a handle, handed to a helper",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 drive(|| &repo).await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_err gives a handle on the error side",
                "async fn h(repo: PgPostRepository, result: Result<i64, Error>) \
                 -> AutumnResult<usize> { let mapped = result.map_err(|_| &repo); \
                 let r = mapped.unwrap_err(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "an enumerate item is a tuple",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let pair = repos.into_iter().enumerate().next().unwrap(); \
                 pair.refresh().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a map item is a tuple",
                "async fn h(repos: HashMap<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let pair = repos.into_iter().next().unwrap(); pair.refresh().await; Ok(0) }",
                Expect::Unbounded,
            ),
            // Guard: a map's values are handles.
            (
                "a map value is a handle",
                "async fn h(repos: HashMap<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let r = repos.values().next().unwrap(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn rest_slices_and_accessor_captures_keep_their_handles() {
        check_handlers(&[
            (
                "a rest binding is a subslice",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let [tail @ ..] = repos.as_slice() else { return Ok(0) }; \
                 tail.refresh_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a closure that captures a handle through a field",
                "async fn h(state: AppState) -> AutumnResult<usize> { \
                 drive(|| state.repo.clone()).await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a closure that captures a handle through an accessor call",
                "async fn h(state: AppState) -> AutumnResult<usize> { \
                 drive(|| state.db()).await?; Ok(0) }",
                Expect::Unbounded,
            ),
            // Guards: one element of a slice is a handle, and a closure
            // parameter's field is not a capture.
            (
                "a slice element is a handle",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let [first, ..] = repos.as_slice() else { return Ok(0) }; \
                 let _ = first.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a closure parameter's field",
                "async fn h(rows: Vec<Row>) -> AutumnResult<usize> { \
                 let ids: Vec<i64> = rows.iter().map(|row| row.id).collect(); render(ids); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn while_conditions_tuple_parts_and_stored_futures() {
        check_handlers(&[
            (
                "the condition-false path of a while",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 while repo.ready().await? { return Ok(0); } let _ = repo.load().await?; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a tuple bound to a name keeps its parts",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pair = (repo, 7); let (_, id) = pair; render(id); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a stored query future is not a handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut futures = Vec::new(); futures.push(repo.find_all()); \
                 join_all(futures).await; Ok(0) }",
                Expect::Exact(1),
            ),
            // Guards: the tuple's handle part is still a handle, and a
            // while whose body goes on is unbounded.
            (
                "the handle part of a tuple bound to a name",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pair = (repo, 7); let (r, _) = pair; let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a while whose body goes on",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 while repo.ready().await? { let _ = repo.load().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn conditional_std_methods_and_mapped_options() {
        check_handlers(&[
            (
                "Option::cloned does not exist on an Option of a value",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 maybe.cloned().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "Option::as_deref needs a Deref value",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 maybe.as_deref().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "Result::copied does not exist on a Result of a value",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 result.copied().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "an Option mapped to a plain value",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let status = Some(repo).map(|_| 1); render(status); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "an iterator mapped to a plain value",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let ids: Vec<_> = repos.as_slice().iter().map(|_| 1).collect(); render(ids); Ok(0) }",
                Expect::Exact(0),
            ),
            // Guards: `cloned` on an `Option` of a reference is known, and a
            // `Result` mapped on its `Ok` side keeps its `Err` side.
            (
                "first().cloned() on a Vec of handles",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let r = repos.as_slice().first().cloned().unwrap(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "iter().find().cloned() on a Vec of handles",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let r = repos.as_slice().iter().find(|_| true).cloned().unwrap(); \
                 let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "as_ref().cloned() on an Option of a handle",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 let r = maybe.as_ref().cloned().unwrap(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "an iterator mapped to its own handles",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let all: Vec<_> = repos.iter().map(|r| r).collect(); \
                 for r in &all { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "an iterator of containers mapped to clones",
                "async fn h(groups: Vec<Vec<PgPostRepository>>) -> AutumnResult<usize> { \
                 let all: Vec<_> = groups.iter().map(|g| g.clone()).collect(); \
                 all[0].refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a Result mapped on its Ok side keeps its Err side",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let mapped = result.map(|_| 1); render(mapped); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn fold_accumulators_result_sides_and_ok_or() {
        check_handlers(&[
            (
                "a fold accumulator seeded with a handle",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let _ = ids.iter().fold(repo, |repo, _| { drive(&repo); repo }); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a fold accumulator that gains a handle in its callback",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let _ = ids.iter().fold(Vec::new(), |mut v, _| { drive(&v); v.push(&repo); v }); \
                 Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "Err of a Result with a handle on the Ok side only, in if let",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 if let Err(e) = result { render(e); } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "Err of a Result with a handle on the Ok side only, in match",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 match result { Ok(r) => { let _ = r.find_all().await?; } Err(e) => render(e) } \
                 Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "ok_or puts a handle on the Err side",
                "async fn h(repo: PgPostRepository, maybe: Option<i64>) -> AutumnResult<usize> { \
                 let result = maybe.ok_or(&repo); let r = result.unwrap_err(); \
                 let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "ok_or_else puts its callback output on the Err side",
                "async fn h(repo: PgPostRepository, maybe: Option<i64>) -> AutumnResult<usize> { \
                 let result = maybe.ok_or_else(|| &repo); let r = result.unwrap_err(); \
                 let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            // Guard: `Ok(r)` on such a Result is still the handle.
            (
                "Ok of a Result with a handle on the Ok side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 if let Ok(r) = result { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn iterator_adapters_keep_a_layer_and_and_then_replaces() {
        check_handlers(&[
            (
                "filter_map gives an iterator, not a handle",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mapped = ids.iter().filter_map(|_| Some(&repo)); \
                 mapped.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "flat_map gives an iterator, not a handle",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mapped = ids.iter().flat_map(|_| vec![&repo]); \
                 mapped.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "filter_map over a closure that returns the handle's result",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mapped = ids.iter().filter_map(|_| repo.cached_one()); \
                 mapped.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_while gives an iterator, not a handle",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mapped = ids.iter().map_while(|_| Some(&repo)); \
                 mapped.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "Option::and_then to a plain value",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let status = Some(repo).and_then(|_| Some(1)); render(status); Ok(0) }",
                Expect::Exact(0),
            ),
            // Guards: the items are still handles, and `and_then` keeps
            // what its closure returns.
            (
                "a filter_map item is a handle",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 for r in ids.iter().filter_map(|_| Some(&repo)) { let _ = r.find_all().await?; } \
                 Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "Option::and_then to the handle",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let r = Some(1).and_then(|_| Some(&repo)).unwrap(); \
                 let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn option_fallback_arguments_keep_their_handles() {
        check_handlers(&[
            (
                "None.or(Some(repo))",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let slot = None.or(Some(repo)); let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "None.unwrap_or(repo)",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let r = None.unwrap_or(repo); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "None.xor(Some(repo))",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let slot = None.xor(Some(repo)); let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            // Guard: a plain fallback stays plain.
            (
                "None.unwrap_or(0)",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let n = None.unwrap_or(0); render(n); let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn transactions_need_a_connection_and_shadowable_methods_are_reported() {
        check_handlers(&[
            (
                "a transaction name on a repository-named user type",
                "async fn h(repo: PgPostRepository, runner: RunnerRepository) \
                 -> AutumnResult<usize> { runner.tx_immediate(|| repo.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a transaction on an accessor connection runs once",
                "async fn h(state: AppState) -> AutumnResult<usize> { \
                 state.db().tx(|c| async move { let _ = c.find_all().await; }).await; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a nested transaction on the callback connection runs once",
                "async fn h(mut db: Db) -> AutumnResult<usize> { \
                 db.tx(|c| async move { c.tx(|d| async move { let _ = d.find_all().await; }).await; }) \
                 .await; Ok(0) }",
                Expect::Exact(3),
            ),
            (
                "a transaction name on a LazyDb comes from a user trait",
                "async fn h(repo: PgPostRepository, lazy: LazyDb) -> AutumnResult<usize> { \
                 lazy.tx(|| repo.find_all()).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a transaction on a checked-out connection runs once",
                "async fn h(lazy: LazyDb) -> AutumnResult<usize> { \
                 let mut db = lazy.checkout().await?; \
                 db.tx(|c| async move { let _ = c.find_all().await; }).await; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a slice method reached through a Vec may be shadowed",
                "async fn h(mut repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 repos.sort().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "clone depends on the element being Clone",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 repos.clone().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a slice method reached through an array may be shadowed",
                "async fn h(repos: [PgPostRepository; 2]) -> AutumnResult<usize> { \
                 repos.first().await; Ok(0) }",
                Expect::Unbounded,
            ),
            // Guards: a Vec's own methods and a slice's methods stay known.
            (
                "Vec::remove is the Vec's own method",
                "async fn h(mut repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let r = repos.remove(0); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a slice's first is the slice's own method",
                "async fn h(repos: &[PgPostRepository]) -> AutumnResult<usize> { \
                 let r = repos.first().unwrap(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "as_slice then first",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let r = repos.as_slice().first().unwrap(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn stores_through_entries_and_bool_then_runs_once() {
        check_handlers(&[
            (
                "a store through a map entry",
                "async fn h(repo: PgPostRepository, mut map: HashMap<i64, PgPostRepository>) \
                 -> AutumnResult<usize> { let mut fresh = HashMap::new(); \
                 fresh.entry(1).or_insert(repo); \
                 for r in fresh.into_values() { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a store through a map entry, with no loop",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut fresh = HashMap::new(); fresh.entry(1).or_insert(repo); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a store into an element of a container nests it",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut lists = vec![Vec::new()]; lists[0].push(repo); \
                 lists[0].refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "bool::then runs its closure at most once",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let _ = flag.then(|| repo.find_all()); Ok(0) }",
                Expect::Exact(1),
            ),
            // Guard: `then` on an unknown receiver may run many times.
            (
                "then on an unknown receiver",
                "async fn h(repo: PgPostRepository, s: Stream) -> AutumnResult<usize> { \
                 let _ = s.then(|_| repo.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn result_callbacks_take_their_side_and_map_or_gives_its_output() {
        check_handlers(&[
            (
                "map_err takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.map_err(|e| { render(&e); e }); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "is_err_and takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.is_err_and(|e| render(&e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "map_or_else's default takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.map_or_else(|e| render(&e), |_| 0); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "map_or gives what its default and callback give",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let status = Some(repo).map_or(0, |_| 1); render(status); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "fold gives its accumulator",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let n = repos.into_iter().fold(0, |n, _| n + 1); render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            // Guards: the side that holds the handle is still a handle.
            (
                "map takes the value side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.map(|r| render(&r)); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_err on a Result whose error is a handle",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.map_err(|e| render(&e)); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_or_else's callback takes the value side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.map_or_else(|_| 0, |r| render(&r)); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_or that gives the element",
                "async fn h(repo: PgPostRepository, other: PgPostRepository) -> AutumnResult<usize> { \
                 let r = Some(repo).map_or(other, |r| r); render(&r); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "fold that gives the element",
                "async fn h(repos: Vec<PgPostRepository>, first: PgPostRepository) -> AutumnResult<usize> { \
                 let r = repos.into_iter().fold(first, |_, r| r); render(&r); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn iterator_extrema_give_an_option() {
        check_handlers(&[
            (
                "max_by_key gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.into_iter().max_by_key(|_| 0); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "min_by_key gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.into_iter().min_by_key(|_| 0); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "max_by gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.into_iter().max_by(|_, _| Ordering::Equal); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "min_by gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.into_iter().min_by(|_, _| Ordering::Equal); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "reduce gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.into_iter().reduce(|a, _| a); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "max gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.into_iter().max(); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "max by reference gives an Option",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let selected = repos.iter().max_by_key(|_| 0); selected.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: the Some side of max_by_key is a handle",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 if let Some(r) = repos.into_iter().max_by_key(|_| 0) { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: max_by_key then unwrap is a handle",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = repos.into_iter().max_by_key(|_| 0).unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: reduce then unwrap is a handle",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = repos.into_iter().reduce(|a, _| a).unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn constructors_iifes_async_blocks_and_mut_aliases_keep_handles() {
        check_handlers(&[
            (
                "an awaited handle constructor is opaque",
                "async fn h(db: Db) -> AutumnResult<usize> { \
                 let repo = PgPostRepository::new(&mut db).await; let _ = repo; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "an immediately invoked closure gives its output",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let r = (|x| x)(repo); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "an awaited async block gives its output",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let r = async { &repo }.await; let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a store through a mutable alias",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::new(); let slot = &mut repos; slot.push(repo); for r in repos { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a store through a chain of mutable aliases",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::new(); let a = &mut repos; let b = a; b.push(repo); for r in repos { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a mutable alias handed to a function",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::new(); let slot = &mut repos; fill(slot, &repo); for r in repos { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "an awaited closure that gives an async block",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let r = (|| async { &repo })().await; let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            // As with `saved.value = Some(repo);`: the field's shape is not known,
            // so `unwrap` on it is reported.
            (
                "a field assignment through a mutable alias",
                "async fn h(repo: PgPostRepository, mut saved: Saved) -> AutumnResult<usize> { \
                 let slot = &mut saved; slot.value = Some(repo); let _ = saved.value.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a field assignment through a chain of mutable aliases",
                "async fn h(repo: PgPostRepository, mut saved: Saved) -> AutumnResult<usize> { \
                 let a = &mut saved; let b = a; b.value = Some(repo); let _ = saved.value.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a store through an alias that may point to either of two places",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut a = Vec::new(); let mut b = Vec::new(); let mut slot = &mut a; if flag { slot = &mut b; } slot.push(repo); for r in b { let _ = r.find_all().await?; } let _ = a; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a plain store through a mutable alias is free",
                "async fn h(ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mut out = Vec::new(); let slot = &mut out; slot.push(1); for i in out { let _ = i; } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a deferred constructor future is opaque when awaited",
                "async fn h(db: Db) -> AutumnResult<usize> { \
                 let pending = PgPostRepository::new(&mut db); let repo = pending.await; let _ = repo.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a deferred accessor future stays free",
                "async fn h(ctx: Ctx) -> AutumnResult<usize> { \
                 let pending = ctx.conn(); let db = pending.await?; let _ = db; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a constructor that is not awaited is free",
                "async fn h(db: Db) -> AutumnResult<usize> { \
                 let repo = PgPostRepository::new(db); let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn wrapped_connections_and_result_sides() {
        check_handlers(&[
            (
                "checkout on a Result of LazyDb is opaque",
                "async fn h(lazy: Result<LazyDb, Error>) -> AutumnResult<usize> { \
                 let c = lazy.checkout().await; let _ = c; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "checkout on an Option of LazyDb is opaque",
                "async fn h(lazy: Option<LazyDb>) -> AutumnResult<usize> { \
                 let c = lazy.checkout().await; let _ = c; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: checkout after expect is free",
                "async fn h(lazy: Result<LazyDb, Error>) -> AutumnResult<usize> { \
                 let db = lazy.expect(\"db\").checkout().await?; let _ = db; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a Db unwrapped from an Option is a connection",
                "async fn h(maybe: Option<Db>) -> AutumnResult<usize> { \
                 let db = maybe.unwrap(); let _ = db.tx(|conn| conn.find_all()).await; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a Db taken from a Result with ? is a connection",
                "async fn h(res: Result<Db, Error>) -> AutumnResult<usize> { \
                 let db = res?; let _ = db.tx(|conn| conn.find_all()).await; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "tx on a repository may hand its callback a connection",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _ = repo.tx(|conn| conn.find_all()).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "err takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let e = result.err().unwrap(); render(&e); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: ok takes the value side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.ok().unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn awaits_deref_stores_result_maps_and_inspect() {
        check_handlers(&[
            (
                "a stored async block gives its output when awaited",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = async { &repo }; let r = pending.await; let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "an async block in a container is opaque when awaited",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let wrapped = Some(async { &repo }); let r = wrapped.unwrap().await; let _ = r; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a deferred constructor in a container is opaque when awaited",
                "async fn h(db: Db) -> AutumnResult<usize> { \
                 let pending = PgPostRepository::new(&mut db); let wrapped = Some(pending); let repo = wrapped.unwrap().await; let _ = repo; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a deferred query future is counted once",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = repo.find_all(); let _ = pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a deferred accessor future in a let chain stays free",
                "async fn h(ctx: Ctx) -> AutumnResult<usize> { \
                 let pending = ctx.conn(); let again = pending; let db = again.await?; let _ = db; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a dereference assignment reaches the referent",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut saved = None; let slot = &mut saved; *slot = Some(repo); let _ = saved.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "map on a Result drops the handle side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let status = result.map(|_| 1); render(status); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "map_err on a Result keeps the handle side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let kept = result.map_err(|_| 1); render(kept); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_err on a Result drops a plain value side",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let status = result.map_err(|_| 1); render(status); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "inspect on an Option is a known method",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 let kept = maybe.inspect(|_| {}); let _ = kept.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "inspect_err on a Result is a known method",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let kept = result.inspect_err(|_| {}); let _ = kept.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn once_callbacks_take_named_functions_and_unwrap_err_takes_its_side() {
        check_handlers(&[
            (
                "inspect_err with a named function is opaque",
                "async fn h(result: Result<(), PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.inspect_err(refresh_repository); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "map_err with a named function is opaque",
                "async fn h(result: Result<(), PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.map_err(refresh_repository); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "or_else with a named function is opaque",
                "async fn h(result: Result<(), PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.or_else(refresh_repository); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "is_err_and with a named function is opaque",
                "async fn h(result: Result<(), PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.is_err_and(refresh_repository); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "unwrap_err takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let e = result.unwrap_err(); render(&e); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "expect_err takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let e = result.expect_err(\"no\"); render(&e); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: unwrap_err on a handle error side is a handle",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = result.unwrap_err().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: unwrap takes the value side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn extend_is_a_trait_method_and_opaque() {
        check_handlers(&[
            (
                "extend on a Vec of handles is opaque",
                "async fn h(mut repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 repos.extend().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "extend on a map of handles is opaque",
                "async fn h(mut repos: HashMap<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 repos.extend().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: extend on a plain Vec is free",
                "async fn h(ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mut all = Vec::new(); all.extend(ids); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn awaited_values_keep_their_handles() {
        check_handlers(&[
            (
                "a LazyDb taken from a container still gives a tracked connection",
                "async fn h(lazy: LazyDb) -> AutumnResult<usize> { \
                 let lazy = [lazy].into_iter().next().unwrap(); let mut db = lazy.checkout().await?; let _ = db.find_all().await?; let _ = db.find_all().await?; Ok(0) }",
                Expect::Exact(3),
            ),
            (
                "an annotated awaited constructor gives a handle",
                "async fn h(db: Db) -> AutumnResult<usize> { \
                 #[query_cost(1)] let repo = PgPostRepository::new(&mut db).await; let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "an annotated awaited deferred constructor gives a handle",
                "async fn h(db: Db) -> AutumnResult<usize> { \
                 let pending = PgPostRepository::new(&mut db); #[query_cost(1)] let repo = pending.await; let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: a LazyDb checkout stays free",
                "async fn h(lazy: LazyDb) -> AutumnResult<usize> { \
                 let mut db = lazy.checkout().await?; let _ = db.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn result_sides_hold_through_adapters_and_fallbacks() {
        check_handlers(&[
            (
                "unwrap_or on a Result takes the value side",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let n = result.unwrap_or(0); render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "unwrap_or_default on a Result takes the value side",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let n = result.unwrap_or_default(); render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "unwrap_or_else on a Result takes the value side and its callback output",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let n = result.unwrap_or_else(|_| 0); render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "map_err after as_ref takes the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.as_ref().map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: unwrap_or_else that gives the error handle",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let n = result.unwrap_or_else(|r| r); render(n); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: map after as_ref takes the handle side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let _ = result.as_ref().map(|r| render(r)); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn checkout_needs_a_direct_lazy_db_and_aliases_keep_sides() {
        check_handlers(&[
            (
                "checkout through another method is not free",
                "async fn h(lazy: LazyDb) -> AutumnResult<usize> { \
                 let _ = lazy.wrap().checkout().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: checkout directly on a LazyDb is free",
                "async fn h(lazy: LazyDb) -> AutumnResult<usize> { \
                 let _ = lazy.checkout().await?; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "an alias of a Result keeps its sides",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let alias = result; let _ = alias.map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: an alias of a Result keeps its handle side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let alias = result; let _ = alias.map(|r| render(r)); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn patterns_and_typed_locals_keep_result_sides() {
        check_handlers(&[
            (
                "a match over as_ref keeps the error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 match result.as_ref() { Ok(_) => {} Err(e) => render(e) } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a match over as_ref keeps the handle side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 match result.as_ref() { Ok(r) => render(r), Err(_) => {} } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a typed local Result records its sides",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let result: Result<PgPostRepository, Error> = make(x); let error = result.unwrap_err(); render(error); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a typed local Option of Db records its inner shape",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let maybe: Option<Db> = make(x); let db = maybe.unwrap(); let _ = db.tx(|conn| conn.find_all()).await; Ok(0) }",
                Expect::Exact(2),
            ),
        ]);
    }

    #[test]
    fn stored_closure_futures_and_wrapper_adapters_keep_metadata() {
        check_handlers(&[
            (
                "a stored closure call that gives an async block keeps its output",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = (|| async { &repo })(); let r = pending.await; let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a Db unwrapped after as_ref is a connection",
                "async fn h(maybe: Option<Db>) -> AutumnResult<usize> { \
                 let db = maybe.as_ref().unwrap(); let _ = db.tx(|conn| conn.find_all()).await; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a Db unwrapped through a reference alias is a connection",
                "async fn h(maybe: Option<Db>) -> AutumnResult<usize> { \
                 let alias = &maybe; let db = alias.as_ref().unwrap(); let _ = db.tx(|conn| conn.find_all()).await; Ok(0) }",
                Expect::Exact(2),
            ),
        ]);
    }

    #[test]
    fn short_circuit_exits_and_adapter_aliases() {
        check_handlers(&[
            (
                "a short-circuit side that returns binds nothing after it",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; let _ = flag || { slot = Some(&repo); return Ok(0); }; render(slot); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a short-circuit side that falls through binds after it",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; let _ = flag || { slot = Some(&repo); true }; render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "an alias of an adapter keeps the Result sides",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let alias = result.as_ref(); let _ = alias.map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn try_for_each_gives_its_residual() {
        check_handlers(&[
            (
                "try_for_each gives its callback's residual",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let failed = ids.into_iter().try_for_each(|_| Err::<(), _>(&repo)); let r = failed.unwrap_err(); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: try_for_each with a plain callback is plain",
                "async fn h(ids: Vec<i64>) -> AutumnResult<usize> { \
                 let done = ids.into_iter().try_for_each(|_| Ok::<(), i64>(())); render(done); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn std_methods_need_their_arity_and_constructors_their_path() {
        check_handlers(&[
            (
                "a container method with the wrong arity is opaque",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 repos.push().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a len with an argument is opaque",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let _ = repos.len(1); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: push with one argument is a known method",
                "async fn h(repos: Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut all = repos; all.push(repo); let _ = all.len(); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a Vec insert with one argument is opaque",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 repos.insert(1).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "an Option take with an argument is opaque",
                "async fn h(maybe: Option<PgPostRepository>) -> AutumnResult<usize> { \
                 maybe.take(1).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a set insert with one argument is known",
                "async fn h(ids: HashSet<i64>, repos: HashSet<PgPostRepository>) -> AutumnResult<usize> { \
                 let mut all = repos; let _ = all.insert(1); let _ = ids; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a Vec insert with two arguments is known",
                "async fn h(repos: Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut all = repos; all.insert(0, repo); let _ = all.len(); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a qualified Some is not Option::Some",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _ = Factory::Some(&repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: Option::Some is a constructor",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let maybe = Option::Some(&repo); let _ = maybe.is_some(); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn deferred_builders_and_named_transaction_callbacks() {
        check_handlers(&[
            (
                "a deferred builder awaited later costs one",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = repo.scoped(); let _ = pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a deferred builder copied, then awaited, costs one",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = repo.scoped(); let again = pending; let _ = again.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a builder refined then run is one query",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let q = repo.scoped(); let _ = q.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a named free-function transaction callback does not leak the connection flag",
                "async fn h(db: Db, repo: PgPostRepository) -> AutumnResult<usize> { \
                 #[query_cost(1)] let _ = scoped_transaction(&mut db, run); \
                 let _ = (|r| r.tx(|c| c.find_all()))(repo); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a named transaction callback does not leak the connection flag",
                "async fn h(db: Db, repo: PgPostRepository) -> AutumnResult<usize> { \
                 #[query_cost(1)] let _ = db.tx(run); let _ = (|r| r.tx(|c| c.find_all()))(repo); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn closure_references_and_declared_types() {
        check_handlers(&[
            (
                "a callback closure behind &mut is still a closure",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _ = Some(&repo).map(&mut |r| r.find_all()).unwrap().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a loop callback behind & is still a closure",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 repos.into_iter().for_each(&|r: PgPostRepository| { let _ = r.find_all(); }); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a typed handle keeps its type when reassigned",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let mut repo: PgPostRepository = make(x); repo = make(x); let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "an inferred handle keeps its kind when reassigned",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut alias = repo; alias = make_repository(); let _ = alias.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "an inferred container keeps its kind when reassigned",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut all = vec![repo]; all = make_repositories(); for r in all { let _ = r.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a typed Result keeps its sides when reassigned",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let mut result: Result<PgPostRepository, Error> = make(x); result = make(x); let e = result.unwrap_err(); render(e); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn transformed_results_keep_their_sides() {
        check_handlers(&[
            (
                "a mapped Result keeps its error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let mapped = result.map(|r| r); let _ = mapped.map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a map_err Result keeps its value side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let mapped = result.map_err(|e| e); let _ = mapped.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a mapped Result keeps its handle side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let mapped = result.map(|r| r); let _ = mapped.map(|r| render(r)); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a Result mapped to plain drops the handle",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let ids = result.map(|_| 1); let _ = ids.map(|n| render(n)); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn while_conditions_guards_and_closure_parameters() {
        check_handlers(&[
            (
                "a while condition that returns never runs the body",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 while { return Ok(repo.a().await?); } { let _ = repo.b().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a while condition that falls through runs the body",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 while flag { let _ = repo.b().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "each match body pays only the guards tried before it",
                "async fn h(repo: PgPostRepository, x: i64) -> AutumnResult<usize> { \
                 let _ = match x { _ if repo.a().await? => repo.b().await?, _ if repo.c().await? => plain(), _ => plain() }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: a later arm pays every guard before it",
                "async fn h(repo: PgPostRepository, x: i64) -> AutumnResult<usize> { \
                 let _ = match x { _ if repo.a().await? => plain(), _ if repo.c().await? => plain(), _ => repo.b().await? }; Ok(0) }",
                Expect::Exact(3),
            ),
            (
                "a called closure binds each parameter to its own argument",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let n = (|_repo, n| n)(repo, 1); render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a called closure gives the handle argument back",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let r = (|r, _n| r)(repo, 1); let _ = r.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn diverging_guards_and_compound_assignments() {
        check_handlers(&[
            (
                "a match guard that returns ends the match",
                "async fn h(repo: PgPostRepository, x: i64) -> AutumnResult<usize> { \
                 let _ = match x { _ if { return Ok(repo.a().await?); } => repo.b().await?, _ => repo.c().await? }; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a refutable arm with a returning guard leaves later arms reachable",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { Some(_) if { return Ok(repo.a().await?); } => 0, None => repo.c().await? }; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a compound assignment stores a handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut ctx = Holder::default(); ctx += repo; let _ = ctx.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a plain compound assignment is free",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let mut n = 0; n += x; render(n); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn typed_tuple_patterns_and_result_chains() {
        check_handlers(&[
            (
                "a typed tuple pattern gives each part its own type",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let (db, n): (Db, i64) = make(x); render(n); let _ = db; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a typed tuple pattern keeps the handle part",
                "async fn h(x: i64) -> AutumnResult<usize> { \
                 let (db, n): (Db, i64) = make(x); let _ = n; let _ = db.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "and_then keeps a plain error side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let mapped = result.and_then(|r| Ok::<_, Error>(r)); let _ = mapped.map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "or_else keeps the value side",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let mapped = result.or_else(|e| Err::<PgPostRepository, _>(e)); let _ = mapped.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: and_then that returns the handle as an error",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let mapped = result.and_then(|r| Err::<i64, _>(r)); let _ = mapped.map_err(|e| render(e)); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn result_constructors_give_their_sides() {
        check_handlers(&[
            (
                "an Ok constructor gives a plain error side",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let result = Ok::<PgPostRepository, Error>(repo); let _ = result.map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "an Err constructor gives a plain value side",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let result = Err::<i64, PgPostRepository>(repo); let _ = result.map(|n| render(n)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: an Ok constructor keeps the handle on its value side",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let result = Ok::<PgPostRepository, Error>(repo); let _ = result.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a user function named Ok is not a constructor",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let result = factory::Ok(repo); let _ = result.map_err(|e| e.run()); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn typed_tuple_parameters_pair_parts_with_types() {
        check_handlers(&[
            (
                "a typed tuple parameter gives each part its own type",
                "async fn h((repo, n): (PgPostRepository, i64)) -> AutumnResult<usize> { \
                 render(n); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a typed tuple parameter keeps the handle part",
                "async fn h((repo, n): (PgPostRepository, i64)) -> AutumnResult<usize> { \
                 let _ = n; let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn ref_mut_aliases_shadowed_captures_and_awaited_constructors() {
        check_handlers(&[
            (
                "a ref mut binding is a mutable alias",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::new(); { let ref mut alias = repos; alias.push(repo); } \
                 let _ = repos[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a ref mut match arm is a mutable alias",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::new(); match repos { ref mut alias => alias.push(repo) } \
                 let _ = repos[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a closure local shadows an outer handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let callback = || { let repo = 1; repo }; render(callback); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a nested closure parameter shadows an outer handle",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let callback = || ids.iter().map(|repo| repo + 1).count(); render(callback); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a closure that reads the handle before it shadows it",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let callback = || { let n = repo.len(); let repo = 1; n + repo }; render(callback); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a let initializer reads the outer handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let callback = || { let repo = repo; repo }; render(callback); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a macro in a closure names the handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let callback = || vec![repo.clone()]; render(callback); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an awaited smart-pointer new is a user function",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _ = Arc::new(&repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an awaited Some is a user function",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _ = Some(&repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a smart-pointer new that is not awaited stays free",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let shared = Arc::new(repo); let _ = shared; Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn branch_futures_returning_loops_and_overloaded_assignments() {
        check_handlers(&[
            (
                "an if that picks a query future is a known future",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = if flag { repo.find(1) } else { repo.find(2) }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a match that picks a query future is a known future",
                "async fn h(repo: PgPostRepository, n: i64) -> AutumnResult<usize> { \
                 let pending = match n { 0 => repo.find(1), _ => repo.find(2) }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an if that picks a builder chain costs 1 when awaited",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = if flag { repo.scoped() } else { repo.scoped() }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an if that picks an opaque value is not a known future",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = if flag { repo.find(1) } else { Wrapper::of(&repo) }; pending.await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a loop pass that returns does not bind after the loop",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mut slot = None; for _ in &ids { slot = Some(&repo); return Ok(0); } render(slot); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a loop pass that breaks binds after the loop",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mut slot = None; for _ in &ids { slot = Some(&repo); break; } render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a loop pass that continues binds after the loop",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mut slot = None; for _ in &ids { slot = Some(&repo); continue; } render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an overloaded operator keeps the handle kind",
                "async fn h(repo: PgPostRepository, factory: Factory, key: i64) -> AutumnResult<usize> { \
                 let mut alias = repo; alias = factory + key; let _ = alias.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a name of unknown kind keeps the handle kind",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut alias = repo; let other = make(); alias = other; let _ = alias.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a constructor of an opaque value keeps the handle kind",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(repo); slot = Some(make()); let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a literal still clears",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(&repo); slot = None; render(slot); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn nested_ref_mut_turbofish_sides_and_mixed_deferred_queries() {
        check_handlers(&[
            (
                "guard: a nested ref mut binding aliases its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut holder = Holder::default(); let Holder(ref mut bucket) = holder; \
                 *bucket += repo; let _ = holder.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a nested ref mut in an if let aliases its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(Holder::default()); if let Some(ref mut inner) = slot { *inner += repo; } \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a turbofish constructor keeps its side types",
                "async fn h() -> AutumnResult<usize> { \
                 let mut r = Ok::<(), PgPostRepository>(()); r = make_result(); \
                 let repo = r.unwrap_err(); let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a turbofish constructor keeps a plain side plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let r = Ok::<PgPostRepository, Error>(repo); let _ = r.map_err(|e| render(e)); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a mixed builder and query pick costs one",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = if flag { repo.scoped() } else { repo.find(1) }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a mixed match pick costs one",
                "async fn h(repo: PgPostRepository, n: i64) -> AutumnResult<usize> { \
                 let pending = match n { 0 => repo.scoped(), _ => repo.find(1) }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a directly awaited if of builder chains costs one",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let _ = (if flag { repo.scoped() } else { repo.scoped() }).await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a directly awaited mixed if costs one",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let _ = (if flag { repo.scoped() } else { repo.find(1) }).await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn deferred_constructors_and_break_bindings() {
        check_handlers(&[
            (
                "guard: a stored smart-pointer new is not a known future",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = Arc::new(&repo); pending.await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a stored Some is not a known future",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = Some(&repo); pending.await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a break binding does not reach the next pass",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; for _ in &ids { if flag { slot = Some(&repo); break; } render(slot); } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a break binding reaches the code after the loop",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; for _ in &ids { if flag { slot = Some(&repo); break; } } render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a continue binding reaches the next pass",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; for _ in &ids { if flag { slot = Some(&repo); continue; } render(slot); } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a labeled break binds after the outer loop",
                "async fn h(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<usize> { \
                 let mut slot = None; 'outer: for _ in &ids { for _ in &ids { slot = Some(&repo); break 'outer; } } \
                 render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a while that ends by its condition binds after the loop",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; while { slot = Some(&repo); flag } {} render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn turbofish_container_constructors_keep_their_type() {
        check_handlers(&[
            (
                "guard: a turbofish Vec keeps its element type",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::<PgPostRepository>::new(); repos = repo + (); \
                 let _ = repos[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a turbofish map keeps its value type",
                "async fn h() -> AutumnResult<usize> { \
                 let mut repos = HashMap::<i64, PgPostRepository>::with_capacity(4); repos = make(); \
                 let _ = repos[&1].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a turbofish None keeps its type",
                "async fn h() -> AutumnResult<usize> { \
                 let mut slot = None::<PgPostRepository>; slot = make(); \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a turbofish Vec of plain values stays plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut ids = Vec::<i64>::new(); ids = make(); render(ids); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn conditional_aliases_declared_clears_and_disjoint_guards() {
        check_handlers(&[
            (
                "guard: a typed alias picked by an if aliases both places",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); let mut right = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = if flag { &mut left } else { &mut right }; target.push(repo); } \
                 let _ = left[0].find_all().await?; let _ = right; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an alias picked by a match aliases every arm",
                "async fn h(repo: PgPostRepository, n: i64) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); let mut right = Vec::new(); \
                 { let target = match n { 0 => &mut left, _ => &mut right }; target.push(repo); } \
                 let _ = right[0].find_all().await?; let _ = left; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a typed alias of one place aliases it",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = &mut left; target.push(repo); } \
                 let _ = left[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "None clears a declared Option",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot: Option<PgPostRepository> = Some(repo); slot = None; render(slot); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: an opaque value keeps a declared Option",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot: Option<PgPostRepository> = None; slot = make(); \
                 let _ = slot.unwrap().find_all().await?; let _ = repo; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a guard on a disjoint pattern is not charged",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { std::option::Option::Some(_) if repo.a().await? => plain(), std::option::Option::None => repo.b().await?, Some(_) => plain() }; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a guard on an overlapping pattern is charged",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { Some(_) if repo.a().await? => plain(), _ => repo.b().await? }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: constant patterns may overlap",
                "async fn h(repo: PgPostRepository, x: i64) -> AutumnResult<usize> { \
                 let _ = match x { A if repo.a().await? => plain(), B => repo.b().await?, _ => plain() }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: equal literals overlap",
                "async fn h(repo: PgPostRepository, x: i64) -> AutumnResult<usize> { \
                 let _ = match x { 1 if repo.a().await? => plain(), 0x1 => repo.b().await?, _ => plain() }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "different literals are disjoint",
                "async fn h(repo: PgPostRepository, x: i64) -> AutumnResult<usize> { \
                 let _ = match x { 1 if repo.a().await? => plain(), 2 => repo.b().await?, _ => plain() }; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn constant_patterns_awaited_drop_and_diverging_iterables() {
        check_handlers(&[
            (
                "guard: a lowercase name pattern may be a constant",
                "async fn h(repo: PgPostRepository, x: u8) -> AutumnResult<usize> { \
                 match x { foo if { return Ok(0); } => (), _ => { let _ = repo.find_all().await?; } } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a mut binding pattern always matches",
                "async fn h(repo: PgPostRepository, x: u8) -> AutumnResult<usize> { \
                 match x { mut y if { return Ok(0); } => { y += 1; } _ => { let _ = repo.find_all().await?; } } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: an awaited drop is a user function",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 drop(repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a stored drop that is awaited is a user function",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = drop(repo); pending.await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "drop that is not awaited stays free",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 drop(repo); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a for iterable that returns skips the body",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 for _ in { return repo.a().await; } { repo.b().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn loop_break_aliases_and_qualified_variants() {
        check_handlers(&[
            (
                "guard: an alias a loop breaks with aliases its place",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = loop { break &mut left; }; target.push(repo); } \
                 let _ = left[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an alias a labeled block breaks with aliases its place",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); let mut right = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = 'pick: { if flag { break 'pick &mut left; } &mut right }; \
                 target.push(repo); } \
                 let _ = left[0].find_all().await?; let _ = right; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: differently qualified variants may be the same",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 let _ = match x { a::E::V(_) if repo.a().await? => plain(), b::E::V(_) => repo.b().await?, _ => plain() }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: user variants of one written owner may alias",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 let _ = match x { E::V(_) if repo.a().await? => plain(), E::W(_) => repo.b().await?, _ => plain() }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: re-exported variant names may alias in guard groups",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 let _ = match x { m::A(_) if repo.a().await? => (), m::B(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
        ]);
    }

    #[test]
    fn disjoint_guard_bindings_and_clears_through_aliases() {
        check_handlers(&[
            (
                "a guard binding does not reach a disjoint arm",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let mut slot = None; \
                 match x { std::option::Option::Some(_) if { slot = Some(&repo); false } => (), std::option::Option::None => render(slot), Some(_) => () } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a guard binding reaches an overlapping arm",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let mut slot = None; \
                 match x { Some(_) if { slot = Some(&repo); false } => (), _ => render(slot) } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a known value clears through a single alias",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(repo); { let target = &mut slot; *target = None; } render(slot); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: an alias of two places does not clear either",
                "async fn h(repo: PgPostRepository, other: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut left = Some(repo); let mut right = Some(other); \
                 { let target = if flag { &mut left } else { &mut right }; *target = None; } render(left); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an alias of a part does not clear the whole",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut pair = (Some(repo), None); { let target = &mut pair.1; *target = None; } render(pair); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an opaque value through an alias keeps the handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(repo); { let target = &mut slot; *target = make(); } render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn part_alias_writes_unsafe_and_method_aliases() {
        check_handlers(&[
            (
                "guard: a write through a part alias keeps the owner a container",
                "async fn h(first: PgPostRepository, second: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = vec![first]; { let target = &mut repos[0]; *target = second; } \
                 repos.refresh_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a write through a whole alias stores the value",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; { let target = &mut slot; *target = Some(repo); } \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an alias from an unsafe block aliases its place",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = unsafe { &mut left }; target.push(repo); } \
                 let _ = left[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an alias from a mut method aliases its receiver",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut left = vec![make()]; { let target = left.get_mut(0).unwrap(); *target = repo; } \
                 let _ = left[0].find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn only_std_names_are_trusted() {
        check_handlers(&[
            (
                "guard: a user type named Vec gets no std methods",
                "async fn h(repos: custom::Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 repos.push(repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a std path to Vec keeps its methods",
                "async fn h(mut repos: std::vec::Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 repos.push(repo); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a user constructor path named Vec is not std",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = custom::Vec::<PgPostRepository>::new(); repos.push(repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a local macro named vec is opaque",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 macro_rules! vec { ($r:expr) => { $r.find_all().await? }; } let _ = vec![repo]; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a qualified user vec macro is opaque",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _ = custom::vec![repo]; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a std path to vec stays a vector",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let repos = std::vec![repo]; let _ = repos; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a local macro named format is opaque",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 macro_rules! format { ($r:expr) => { $r.find_all().await? }; } let _ = format!(repo); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a user function named drop is opaque",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 custom::drop(repo); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a local function named drop is opaque",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 fn drop(r: PgPostRepository) { let _ = r; } drop(repo); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "std::mem::drop stays free",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 std::mem::drop(repo); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn awaited_container_methods_are_application_methods() {
        check_handlers(&[
            (
                "guard: an awaited container method is an application method",
                "async fn h(mut repos: Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 repos.push(repo).await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a stored container method result that is awaited",
                "async fn h(mut repos: Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = repos.push(repo); pending.await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an awaited len on handles is an application method",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let n = repos.len().await; Ok(n) }",
                Expect::Unbounded,
            ),
            (
                "a container method that is not awaited stays free",
                "async fn h(mut repos: Vec<PgPostRepository>, repo: PgPostRepository) -> AutumnResult<usize> { \
                 repos.push(repo); let n = repos.len(); render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a future taken out of an Option is still a future",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(repo.find_all()); let _ = slot.take().unwrap().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn question_mark_sides_and_call_borrows() {
        check_handlers(&[
            (
                "? on a Result takes its Ok side",
                "async fn h(result: Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 let n = result?; render(n); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: ? on a Result of a handle gives the handle",
                "async fn h(result: Result<PgPostRepository, Error>) -> AutumnResult<usize> { \
                 let repo = result?; let _ = repo.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an alias an invoked closure gives back aliases its place",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = (|x| x)(&mut left); target.push(repo); } \
                 let _ = left[0].find_all().await?; Ok(0) }",
                // A call may give back a part, so `left` nests the handle.
                Expect::Unbounded,
            ),
            (
                "guard: a function given a mut borrow may give it back",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = pick(&mut left); target.push(repo); } \
                 let _ = left[0].find_all().await?; Ok(0) }",
                // A call may give back a part, so `left` nests the handle.
                Expect::Unbounded,
            ),
            (
                "guard: an entry value aliases its map",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut groups = HashMap::new(); \
                 { let list: &mut Vec<PgPostRepository> = groups.entry(1).or_default(); list.push(repo); } \
                 render(groups); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn wrapped_async_outputs_and_iterator_borrows() {
        check_handlers(&[
            (
                "guard: a block-wrapped async block keeps its output",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = { async move { repo } }; let alias = pending.await; \
                 let _ = alias.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an if of async blocks keeps their output",
                "async fn h(repo: PgPostRepository, other: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = if flag { async move { repo } } else { async move { other } }; \
                 let alias = pending.await; let _ = alias.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an iterator element borrows its container",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; { let target = slots.iter_mut().next().unwrap(); *target = Some(repo); } \
                 render(slots); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a for element borrows its container",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; for target in slots.iter_mut() { *target = Some(repo.clone()); } \
                 render(slots); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a callback element borrows its container",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; slots.iter_mut().for_each(|t| *t = Some(repo.clone())); \
                 render(slots); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a for element over a mut borrow borrows its container",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; for target in &mut slots { *target = Some(repo.clone()); } \
                 render(slots); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn stored_async_closures_keep_their_output() {
        check_handlers(&[
            (
                "guard: a stored async closure keeps its output",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let make = move || async move { repo }; let alias = make().await; \
                 let _ = alias.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a stored closure of plain values stays plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let make = || async { 1 }; let n = make().await; render(n); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn loop_builders_labeled_breaks_and_clear() {
        check_handlers(&[
            (
                "a loop that breaks with a builder costs one when awaited",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = loop { break repo.scoped(); }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a loop that breaks with a builder and another future is not known",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = loop { if flag { break repo.scoped(); } break repo.find(1); }; pending.await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a labeled break with an opaque value does not clear",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = Some(repo); slot = 'pick: { if flag { break 'pick make(); } None }; \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a labeled break with a builder is pending",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let pending = 'pick: { if flag { break 'pick repo.scoped(); } repo.scoped() }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "clear empties a container",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = vec![repo]; repos.clear(); render(repos); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: an opaque store after clear has the element type",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = vec![repo]; repos.clear(); repos.push(make_repo()); \
                 let _ = repos[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: clear through an alias does not clear the owner",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut left = vec![repo]; let mut right = Vec::new(); \
                 { let target = if flag { &mut left } else { &mut right }; target.clear(); } render(left); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn part_alias_stores_nest_and_qualified_none_clears() {
        check_handlers(&[
            (
                "guard: a store through a part alias nests the owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; \
                 { let target: &mut Option<PgPostRepository> = &mut slots[0]; target.insert(repo); } \
                 slots[0].refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a store through a typed whole alias keeps the owner a carrier",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut repos = Vec::new(); \
                 { let slot: &mut Vec<PgPostRepository> = &mut repos; slot.push(repo); } \
                 let _ = repos[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a qualified None clears a declared Option",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot: Option<PgPostRepository> = Some(repo); slot = std::option::Option::None; \
                 render(slot); Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a user None path stays opaque",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot: Option<PgPostRepository> = Some(repo); slot = custom::None; \
                 render(slot); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn only_std_result_has_sides_and_unsafe_builders() {
        check_handlers(&[
            (
                "guard: a user Result type has no known sides",
                "async fn h(result: custom::Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 if let custom::Result::Ok(repo) = result { let _ = repo.find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a std Result keeps its sides",
                "async fn h(result: std::result::Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 if let Ok(n) = result { render(n); } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "an unsafe block of a builder costs one when awaited",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let pending = unsafe { repo.scoped() }; pending.await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn block_wrapped_holders_and_exclusive_guards() {
        check_handlers(&[
            (
                "guard: a block-wrapped closure that captures a handle holds it",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let make = { move || repo }; let alias = make(); let _ = alias.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an if of holders holds",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let ctx = if flag { Ctx { repo } } else { Ctx::empty() }; render(ctx); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an unsafe block of nested values is nested",
                "async fn h(lists: Vec<Vec<PgPostRepository>>) -> AutumnResult<usize> { \
                 let inner = unsafe { lists }; inner.refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guards on exclusive std variants are not summed",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { std::option::Option::Some(_) if repo.a().await? => (), std::option::Option::None if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: guards on bare variant names add up",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { Some(_) if repo.a().await? => (), None if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: guards on overlapping patterns are summed",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { Some(_) if repo.a().await? => (), Some(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: a keyless guard adds to a variant guard",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { y if repo.a().await? => { let _ = y; } Some(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: variants of different owners add up",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 let _ = match x { a::E::V(_) if repo.a().await? => (), b::E::W(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
        ]);
    }

    #[test]
    fn destructuring_assignments_bind_fields() {
        check_handlers(&[
            (
                "guard: a struct destructuring assignment binds its fields",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; Ctx { repo: slot } = Ctx { repo: Some(repo) }; \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a tuple-struct destructuring assignment binds its fields",
                "async fn h(w: (Option<PgPostRepository>, i64)) -> AutumnResult<usize> { \
                 let mut slot = None; Wrapper(slot, _) = w; \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a destructuring assignment from a name binds its fields",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let ctx = Ctx { repo: Some(repo), n: 1 }; let mut slot = None; Ctx { repo: slot, .. } = ctx; \
                 let _ = slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a plain field of a destructuring assignment stays plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut n = 0; Ctx { n, .. } = Ctx { repo: Some(repo), n: 1 }; render(n); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn evaluation_order_and_method_argument_borrows() {
        check_handlers(&[
            (
                "guard: a for element is read after its iterable runs",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut rows = Vec::new(); for row in { rows.push(repo); rows } { row.find_all().await?; } Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a method result borrows its mut arguments",
                "async fn h(repo: PgPostRepository, picker: Picker) -> AutumnResult<usize> { \
                 let mut left = Vec::new(); \
                 { let target: &mut Vec<PgPostRepository> = picker.pick(&mut left); target.push(repo); } \
                 render(left); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a store reads its argument after the argument runs",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Vec::new(); let mut dest = Vec::new(); \
                 dest.push({ source.push(repo); source.pop().unwrap() }); render(dest); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a call stores into a mut argument after the arguments run",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Vec::new(); let mut dest = Vec::new(); \
                 fill(&mut dest, { source.push(repo); source.pop().unwrap() }); render(dest); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn writes_through_temporary_borrows() {
        check_handlers(&[
            (
                "guard: a write through a temporary borrow reaches its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; *slots.get_mut(0).unwrap() = Some(repo); render(slots); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a store through a temporary borrow reaches its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut lists = vec![Vec::new()]; lists.get_mut(0).unwrap().push(repo); render(lists); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a field write through a temporary borrow reaches its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut ctxs = vec![Ctx::default()]; ctxs.last_mut().unwrap().repo = Some(repo); render(ctxs); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn destructured_borrows_closure_params_and_chain_store_order() {
        check_handlers(&[
            (
                "guard: a destructured borrow keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None, None]; { let (left, _) = slots.split_at_mut(1); left[0] = Some(repo); } \
                 render(slots); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an invoked closure parameter borrows its mut argument",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut list = Vec::new(); (|slot: &mut Vec<PgPostRepository>| slot.push(repo))(&mut list); \
                 render(list); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: each store in a chain follows its own arguments",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); let mut dest = Vec::new(); \
                 dest.insert(0, source.take().unwrap()).clone_from(&{ source = None; 1 }); render(dest); Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn imported_variant_spellings_are_not_std() {
        check_handlers(&[
            (
                "guard: renamed imports of one variant overlap",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 use E::V as Some; use E::V as Ok; \
                 let _ = match x { Some(_) if repo.a().await? => (), Ok(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: an imported None spelling is not std",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 use E::V as None; \
                 let _ = match x { None if repo.a().await? => (), Some(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: a local Option enum is not std",
                "async fn h(repo: PgPostRepository, x: Option) -> AutumnResult<usize> { \
                 enum Option { Some(i64), None } \
                 let _ = match x { Option::Some(_) if repo.a().await? => (), Option::None if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: a glob import may give a std spelling",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 use E::*; \
                 let _ = match x { Some(_) if repo.a().await? => (), None if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "guard: an imported Some is not a std constructor",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 use custom::Many::Item as Some; \
                 let many = Some(repo); let _ = many.map(|r| r.find_all()); Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an imported Arc is not a std smart pointer",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 use custom::Arc; \
                 let shared = Arc::new(repo); shared.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: bare variant names may be module aliases",
                "async fn h(repo: PgPostRepository, x: E) -> AutumnResult<usize> { \
                 let _ = match x { Some(_) if repo.a().await? => (), Ok(_) if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "std variants without imports stay exclusive",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 let _ = match x { core::option::Option::Some(_) if repo.a().await? => (), core::option::Option::None if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn stored_closures_store_their_captures() {
        check_handlers(&[
            (
                "guard: a capturing closure fills a mutable argument",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut list = Vec::new(); \
                 let mut fill = |slot: &mut Vec<PgPostRepository>| slot.push(repo.clone()); \
                 fill(&mut list); list[0].find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a capturing closure fills a borrowed argument",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut list = Vec::new(); let target = &mut list; \
                 let mut fill = |slot: &mut Vec<PgPostRepository>| slot.push(repo.clone()); \
                 fill(target); list[0].find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a closure without a handle leaves its argument plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut list: Vec<i64> = Vec::new(); \
                 let mut fill = |slot: &mut Vec<i64>| slot.push(1); \
                 fill(&mut list); render(list); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn try_blocks_and_aggregate_borrows() {
        check_handlers(&[
            (
                "guard: a question mark leaves the try block only",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; \
                 let _: Result<(), ()> = try { slot = Some(repo); Err(())?; slot = None; }; \
                 slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: code after a try block runs after its question mark",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let _: Result<(), ()> = try { Err(())?; loop {} }; \
                 repo.a().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a return in a try block leaves the function",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let _: Result<(), ()> = try { if flag { return Ok(0); } }; \
                 repo.a().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a tuple keeps the borrow of its part",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let refs = (&mut slot,); *refs.0 = Some(repo); drop(refs); \
                 slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an array keeps the borrows of its elements",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut a = None; let mut b = None; let refs = [&mut a, &mut b]; \
                 *refs[1] = Some(repo); drop(refs); b.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a field taken from a tuple keeps its borrow",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let refs = (&mut slot,); let target = refs.0; \
                 *target = Some(repo); drop(target); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a field taken from a struct keeps its borrow",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let refs = Refs { a: &mut slot }; let target = refs.a; \
                 *target = Some(repo); drop(target); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a raw pointer keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let target = &raw mut slot; \
                 unsafe { *target = Some(repo); } slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: addr_of_mut keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let target = std::ptr::addr_of_mut!(slot); \
                 unsafe { *target = Some(repo); } slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a bare addr_of_mut keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let target = addr_of_mut!(slot); \
                 unsafe { *target = Some(repo); } slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a struct update keeps the borrows of its base",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let base = Ctx { slot: &mut slot, n: 0 }; \
                 let refs = Ctx { n: 1, ..base }; *refs.slot = Some(repo); drop(refs); \
                 slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a tuple write does not clear its target",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = Some(repo); let refs = (&mut slot, 1); *refs.0 = None; drop(refs); \
                 slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
        ]);
    }

    #[test]
    fn later_parts_do_not_erase_earlier_reads() {
        check_handlers(&[
            (
                "guard: a later argument does not erase an earlier one",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); \
                 helper(source.take().unwrap(), { source = None; }).await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a later method argument does not erase an earlier one",
                "async fn h(repo: PgPostRepository, svc: Service) -> AutumnResult<usize> { \
                 let mut source = Some(repo); \
                 svc.helper(source.take().unwrap(), { source = None; }).await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a method argument does not erase its receiver",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); \
                 source.as_ref().unwrap().find_all_by({ source = None; 1 }).await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an assignment runs its value before its place",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); let mut rows = vec![Vec::new()]; \
                 rows[{ source = None; 0 }] = source.as_ref().unwrap().find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an assignment reads its value before its place runs",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); let mut slots = vec![None]; \
                 slots[{ source = None; 0 }] = source; \
                 slots[0].as_ref().unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a later method argument does not erase what an earlier one gained",
                "async fn h(repo: PgPostRepository, sink: Sink) -> AutumnResult<usize> { \
                 let mut source = Vec::new(); \
                 sink.consume({ source.push(repo); source.pop().unwrap() }, { source.clear() }).await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an invoked closure takes each argument as it ran",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Vec::new(); \
                 (|r, _| async move { r.find_all().await })\
                 ({ source.push(repo); source.pop().unwrap() }, { source.clear() }).await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an explicit reborrow points to its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; \
                 { let target = &mut *slots.get_mut(0).unwrap(); *target = Some(repo); } \
                 slots[0].as_ref().unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a borrow keeps its owner through a question mark",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; let target = pick(&mut slots)?; \
                 *target = Some(repo); slots[0].as_ref().unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a borrow keeps its owner through an await",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slots = vec![None]; let target = pick(&mut slots).await; \
                 *target = Some(repo); slots[0].as_ref().unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "a clear in one statement still holds in the next",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); source = None; render(source); Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn only_the_std_err_binds_a_plain_error() {
        check_handlers(&[
            (
                "guard: a handle type's own Err variant binds the handle",
                "async fn h(repo: TaggedRepository) -> AutumnResult<usize> { \
                 match repo { TaggedRepository::Err(r) => { r.find_all().await?; } _ => {} } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an imported Err variant binds the handle",
                "async fn h(repo: TaggedRepository) -> AutumnResult<usize> { \
                 use TaggedRepository::Err; \
                 match repo { Err(r) => { r.find_all().await?; } _ => {} } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "the std Err of a handle result binds the error",
                "async fn h(res: Result<PgPostRepository, AppError>) -> AutumnResult<usize> { \
                 match res { Err(e) => render(e), Ok(_) => () } Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn annotations_with_local_names_are_not_trusted() {
        check_handlers(&[
            (
                "guard: an imported Result has no std sides",
                "async fn h(other: custom::Result<i64, PgPostRepository>) -> AutumnResult<usize> { \
                 use custom::Result; \
                 let r: Result<i64, PgPostRepository> = other; \
                 match r { Result::Ok(x) => { x.find_all().await?; } _ => {} } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: an imported Vec alias is not a plain std type",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 use custom::Vec; \
                 let v: Vec<i64> = vec![repo]; v[0].find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a glob import untrusts annotations",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 use custom::*; \
                 let v: Vec<i64> = vec![repo]; v[0].find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a local union with a std name is not plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 union Vec { inner: *const PgPostRepository } \
                 let hidden: Vec = Vec { inner: &repo as *const _ }; \
                 unsafe { (&*hidden.inner).find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a local const with a std variant name is not std",
                "async fn h(repo: PgPostRepository, x: Option<i64>) -> AutumnResult<usize> { \
                 const None: Option<i64> = Some(1); \
                 let _ = match x { std::option::Option::Some(_) if repo.a().await? => (), \
                 None if repo.b().await? => (), _ => () }; Ok(0) }",
                Expect::Exact(2),
            ),
            (
                "a parameter type is read outside the body's imports",
                "async fn h(res: Result<PgPostRepository, AppError>) -> AutumnResult<usize> { \
                 use custom::Result; \
                 match res { Err(e) => render(e), Ok(_) => () } Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "guard: a qualified type with a std name is not plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let hidden: custom::Vec<i64> = custom::Vec { inner: repo, value: 0 }; \
                 hidden.inner.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "a std path annotation stays plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let v: std::vec::Vec<i64> = vec![1]; render(v); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
            (
                "a std annotation stays plain",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let v: Vec<i64> = vec![1]; render(v); let _ = repo; Ok(0) }",
                Expect::Exact(0),
            ),
        ]);
    }

    #[test]
    fn try_block_values_keep_handles() {
        check_handlers(&[
            (
                "guard: a try block's value keeps its handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let wrapped: Result<_, ()> = try { repo }; let alias = wrapped?; \
                 alias.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an unannotated try block's value keeps its handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let wrapped = try { repo }; let alias = wrapped?; alias.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn compound_assignments_casts_and_ranges() {
        check_handlers(&[
            (
                "guard: a compound assignment may run its value first",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); let mut totals = vec![0]; \
                 totals[{ source = None; 0 }] += source.as_ref().unwrap().count().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a compound assignment may run its place first",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = None; let mut totals = vec![Total::default()]; \
                 totals[{ source = Some(repo); 0 }] += source.unwrap().count().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a pointer cast keeps the handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let ptr = &repo as *const PgPostRepository; \
                 unsafe { (&*ptr).find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a dereferenced holder keeps its handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let ctx = Ctx { inner: repo }; let borrowed = &ctx; \
                 (*borrowed).inner.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a dereferenced carrier keeps its handles",
                "async fn h(repos: Vec<PgPostRepository>) -> AutumnResult<usize> { \
                 let borrowed = &repos; (*borrowed).refresh_all().await; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a range keeps the handle",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let wrapped = repo..; wrapped.start.find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn async_blocks_keep_their_borrows() {
        check_handlers(&[
            (
                "guard: an awaited async block keeps its borrow",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let target = async { &mut slot }.await; \
                 *target = Some(repo); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an async block's returned borrow is kept",
                "async fn h(repo: PgPostRepository, flag: bool) -> AutumnResult<usize> { \
                 let mut slot = None; let mut other = None; \
                 let target = async { if flag { return &mut slot; } &mut other }.await; \
                 *target = Some(repo); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: an async block's local borrow is kept",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let target = async { let r = &mut slot; r }.await; \
                 *target = Some(repo); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn aggregate_reads_and_moved_borrows() {
        check_handlers(&[
            (
                "guard: a later tuple element does not erase an earlier one",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); \
                 let wrapped = (source.take().unwrap(), { source = None; 0 }); \
                 wrapped.0.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a later struct field does not erase an earlier one",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut source = Some(repo); \
                 let wrapped = Ctx { db: source.take().unwrap(), n: { source = None; 0 } }; \
                 wrapped.db.find_all().await?; Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a raw pointer parameter is a handle",
                "async fn h(ptr: *const PgPostRepository) -> AutumnResult<usize> { \
                 unsafe { (&*ptr).find_all().await?; } Ok(0) }",
                Expect::Exact(1),
            ),
            (
                "guard: a closure returning a captured borrow keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let alias = &mut slot; let make = move || alias; \
                 let target = make(); *target = Some(repo); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a vec of raw pointers keeps their owners",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let refs = vec![&raw mut slot]; let target = refs[0]; \
                 unsafe { *target = Some(repo); } slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a range of borrows keeps their owners",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let mut other = None; let refs = (&mut slot)..(&mut other); \
                 let target = refs.start; *target = Some(repo); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a repeated raw pointer keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let refs = [&raw mut slot; 2]; let target = refs[1]; \
                 unsafe { *target = Some(repo); } slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
            (
                "guard: a borrow moved out by a dereference keeps its owner",
                "async fn h(repo: PgPostRepository) -> AutumnResult<usize> { \
                 let mut slot = None; let boxed = Box::new(&mut slot); let target = *boxed; \
                 *target = Some(repo); slot.unwrap().find_all().await?; Ok(0) }",
                Expect::Unbounded,
            ),
        ]);
    }

    #[test]
    fn each_container_type_has_its_own_methods() {
        // `(parameter type, call)`: the type has no such method, so an
        // extension trait gives it, and it may query.
        let missing = [
            ("VecDeque<PgPostRepository>", "repos.sort()"),
            ("LinkedList<PgPostRepository>", "repos.get(0)"),
            ("BinaryHeap<PgPostRepository>", "repos.insert(0)"),
            ("Vec<PgPostRepository>", "repos.push_back()"),
            ("[PgPostRepository; 2]", "repos.push()"),
            ("&[PgPostRepository]", "repos.clear()"),
            ("BTreeMap<i64, PgPostRepository>", "repos.reserve(1)"),
            ("BTreeSet<PgPostRepository>", "repos.drain()"),
        ];
        // The type has the method, so it issues nothing.
        let present = [
            ("Vec<PgPostRepository>", "repos.as_mut_slice().sort()"),
            ("VecDeque<PgPostRepository>", "repos.push_back(other)"),
            ("[PgPostRepository; 2]", "repos.as_mut_slice().reverse()"),
            (
                "BTreeMap<i64, PgPostRepository>",
                "repos.retain(|_, _| true)",
            ),
            ("HashSet<PgPostRepository>", "repos.drain()"),
        ];
        let handler = |ty: &str, call: &str| {
            format!(
                "async fn h(mut repos: {ty}, other: PgPostRepository) -> AutumnResult<usize> \
                 {{ let _ = {call}; Ok(0) }}"
            )
        };
        // `VecDeque::remove` gives an `Option` of the handle.
        let deque = "async fn h(mut repos: VecDeque<PgPostRepository>) -> AutumnResult<usize> { \
                     let repo = repos.pop_front().unwrap(); let _ = repo.find_all().await?; \
                     let other = repos.remove(0).unwrap(); let _ = other.find_all().await?; \
                     Ok(0) }";
        let mut failures = Vec::new();
        for (ty, call) in missing {
            if check(&handler(ty, call), Expect::Unbounded).is_some() {
                failures.push(format!("{ty}: `{call}` is not reported"));
            }
        }
        for (ty, call) in present {
            if let Some(why) = check(&handler(ty, call), Expect::Exact(0)) {
                failures.push(format!("{ty}: `{call}`: {why}"));
            }
        }
        if let Some(why) = check(deque, Expect::Exact(2)) {
            failures.push(format!("VecDeque element: {why}"));
        }
        let map = "async fn h(mut repos: HashMap<i64, PgPostRepository>) -> AutumnResult<usize> \
                   { let repo = repos.remove(&1).unwrap(); let _ = repo.find_all().await?; Ok(0) }";
        if let Some(why) = check(map, Expect::Exact(1)) {
            failures.push(format!("HashMap element: {why}"));
        }
        // `refresh_all` is not an `Option` method.
        let unknown = "async fn h(mut repos: VecDeque<PgPostRepository>) -> AutumnResult<usize> \
                       { let _ = repos.remove(0).refresh_all().await; Ok(0) }";
        if check(unknown, Expect::Unbounded).is_some() {
            failures.push("VecDeque: `remove(0).refresh_all()` is not reported".to_string());
        }
        assert_matrix(&failures);
    }

    #[test]
    fn every_known_container_method_has_a_result_class() {
        // A known method whose result is not classed would give a plain
        // value, and a part taken through it would be lost.
        let shapes = [
            Shape::Bool,
            Shape::Db,
            Shape::Vec,
            Shape::Array,
            Shape::Slice,
            Shape::Deque,
            Shape::List,
            Shape::Heap,
            Shape::Iter,
            Shape::IterRef,
            Shape::Opt,
            Shape::OptRef,
            Shape::Res,
            Shape::Map,
            Shape::SortedMap,
            Shape::Set,
            Shape::SortedSet,
            Shape::Tuple,
        ];
        let unclassed: Vec<&str> = shapes
            .iter()
            .flat_map(|shape| shape.methods().iter().copied())
            .filter(|m| {
                !SCALAR_METHODS.contains(m)
                    && !CARRIER_METHODS.contains(m)
                    && !ELEMENT_METHODS.contains(m)
            })
            .collect();
        assert!(
            unclassed.is_empty(),
            "unclassed container methods: {unclassed:?}"
        );
    }

    #[test]
    fn a_short_circuit_and_a_labeled_exit_keep_their_paths() {
        let cases: &[(&str, &str, Expect)] = &[
            // When `flag` is true the right side is skipped and the query runs.
            (
                "short-circuit right side that returns",
                "let _ = flag || { return Ok(0); }; let _ = repo.find_all();",
                Expect::Exact(1),
            ),
            // `continue 'outer` lands at the outer loop's head, not after the
            // inner loop, so `slot` there is still plain.
            (
                "labeled continue skips the code after the inner loop",
                "'outer: for _id in &ids { \
                     let mut slot = None; \
                     loop { if flag { slot = Some(&repo); continue 'outer; } break; } \
                     let _ = render(slot); \
                 }",
                Expect::Exact(0),
            ),
            // The closure turns each handle into a user value.
            (
                "map closure producing user values",
                "let contexts = vec![repo].into_iter().map(|r| Ctx { repo: r }).collect::<Vec<_>>(); \
                 let _ = contexts[0].clear();",
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn nested_values_survive_blocks_patterns_and_callbacks() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "block-bodied map closure",
                "let contexts = vec![repo].into_iter() \
                     .map(|r| { let ctx = Ctx { repo: r }; ctx }).collect::<Vec<_>>(); \
                 let _ = contexts[0].clear();",
                Expect::Unbounded,
            ),
            (
                "struct pattern over a struct literal",
                "let Ctx { repos } = Ctx { repos: vec![repo] }; let _ = repos.refresh_all();",
                Expect::Unbounded,
            ),
            (
                "tuple-struct pattern over a user value",
                r#"#[query_exempt(reason = "wraps only")]
                   let ctx = Ctx(vec![repo]);
                   let Ctx(repos) = ctx;
                   let _ = repos.refresh_all();"#,
                Expect::Unbounded,
            ),
            // `runner` is not known to be an `Option` or a `Result`, so its
            // `unwrap_or_else` may call the closure many times.
            (
                "at-most-once name on an unknown receiver",
                "let _ = runner.unwrap_or_else(|| repo.find_all());",
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn a_continue_does_not_end_a_loop() {
        let cases: &[(&str, &str, Expect)] = &[(
            "code after loop { continue; }",
            "loop { continue; } let _ = repo.find_all();",
            Expect::Exact(0),
        )];
        check_cases(cases);
    }

    #[test]
    fn a_pass_that_leaves_the_loop_is_paid_once() {
        let cases: &[(&str, &str, Expect)] = &[
            (
                "bounded loop that breaks on its first pass",
                "for _ in 0..3 { let _ = repo.find_all().await?; break; }",
                Expect::Exact(1),
            ),
            (
                "bounded loop that returns on its first pass",
                "for _ in 0..3 { let _ = repo.find_all().await?; return Ok(1); }",
                Expect::Exact(1),
            ),
            (
                "loop over rows that breaks after the query",
                "for _id in &ids { let _ = repo.find_all().await?; break; }",
                Expect::Exact(1),
            ),
            (
                "loop over rows with a query only on the exit path",
                "for _id in &ids { if flag { let _ = repo.find_all().await?; break; } }",
                Expect::Exact(1),
            ),
            (
                "bounded loop with a query before a conditional break",
                "for _ in 0..3 { let _ = repo.find_all().await?; if flag { break; } }",
                Expect::Exact(3),
            ),
            (
                "bounded loop with a query after a conditional continue",
                "for _ in 0..3 { if flag { continue; } let _ = repo.find_all().await?; }",
                Expect::Exact(3),
            ),
            (
                "loop over rows with a query before a conditional break",
                "for _id in &ids { let _ = repo.find_all().await?; if flag { break; } }",
                Expect::Unbounded,
            ),
            (
                "loop over rows with a query on the continue path",
                "for _id in &ids { if flag { let _ = repo.find_all().await?; continue; } break; }",
                Expect::Unbounded,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn a_map_of_handles_is_a_container() {
        let handler = r"
            async fn h(repos: HashMap<i64, PgPostRepository>) -> AutumnResult<usize> {
                for repo in repos.values() { let _ = repo.find_all().await?; }
                Ok(0)
            }
            ";
        assert_error_contains("50", handler, &["loop"]);
    }

    #[test]
    fn only_the_last_argument_is_a_callback() {
        let cases: &[(&str, &str, Expect)] = &[(
            "tx_with options",
            "let opts = TxOptions::default(); \
             let _ = db.tx_with(opts, |c| async move { let _ = c.a(); });",
            Expect::Exact(2),
        )];
        check_cases(cases);
    }

    #[test]
    fn a_labeled_break_lands_on_its_own_loop() {
        let cases: &[(&str, &str, Expect)] = &[(
            "code after an inner loop that always breaks out",
            "'outer: loop { loop { break 'outer; } let _ = repo.find_all(); }",
            Expect::Exact(0),
        )];
        check_cases(cases);
    }

    #[test]
    fn a_std_type_annotation_marks_a_binding_plain() {
        // `Vec<i64>` is checked by rustc and cannot hold a handle.
        let handler = matrix_handler(
            "let repos = vec![repo]; \
             let ids: Vec<i64> = repos.as_slice().iter().map(|r| r.id).collect(); \
             let _ = render(ids);",
        );
        assert_clean("0", &handler);
        // A `_` leaves the type open, so the container rule still applies.
        let open = matrix_handler(
            "let repos = vec![repo]; \
             let ids: Vec<_> = repos.as_slice().iter().map(|r| r.id).collect(); \
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
        // The `Result` wraps a container, so its parts keep the nested shape
        // and the method on each one is reported.
        assert_error_contains("50", handler, &["find_all"]);
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
                repos.as_mut_slice().sort_by_key(|r| r.id);
                repos.as_slice().iter().for_each(|r| drop(r));
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

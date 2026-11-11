//! `#[contract_checked]` — hold a caller's call sites to the callee's contract.
//!
//! The attribute reads the function it is on. For every call through a
//! generated client it collects two sets:
//!
//! * the **read-set** — every response field the caller names, whether off a
//!   binding (`item.name`), a destructuring `let`, or the call expression
//!   itself (`…await?.name`);
//! * the **write-set** — every request field an inline struct literal sets.
//!
//! A field inside a field is a dotted path: `item.details.color` reads
//! `details` and `details.color`, and `Details { color }` inside the request
//! literal writes `details.color`. The descriptor holds the paths. The const
//! assertions use the first segment only: one build shares the nested type,
//! so rustc itself rejects a nested field that is gone.
//!
//! Each set becomes a const assertion against the callee's own const field
//! table, so the check is a cross-crate compile-time fact rather than a
//! snapshot: rustc rebuilds the caller whenever the callee's table changes.
//! Const-eval messages must be literals, so the macro writes them itself —
//! which is why each names the call site, the endpoint and the field.
//!
//! When the endpoint's JSON descriptor is on disk the macro also runs the full
//! check itself, which lets it name a *missing required* field the const
//! assertion can only count. The const assertions stay either way: a missing
//! descriptor must degrade the diagnostic, never the guarantee.

use std::collections::BTreeMap;

use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::visit::{self, Visit};
use syn::{Expr, Ident, ItemFn, LitStr, Path, Stmt, Token};

use crate::wire::check::{self, CallSite, ViolationKind};
use crate::wire::client::endpoint_table_ident;
use crate::wire::ir::{CallerCall, CallerDescriptor, stable_key};
use crate::wire::store;

/// Parsed `#[contract_checked(...)]` arguments.
struct Args {
    clients: Vec<Path>,
}

impl syn::parse::Parse for Args {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut clients = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "client" {
                clients.push(input.parse::<Path>()?);
            } else {
                return Err(syn::Error::new_spanned(
                    &key,
                    format!(
                        "unknown #[contract_checked] argument `{key}`; expected `client = \
                         <ClientType>`"
                    ),
                ));
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        if clients.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                "#[contract_checked] needs a client: #[contract_checked(client = CatalogClient)]",
            ));
        }
        Ok(Self { clients })
    }
}

pub fn contract_checked_macro(attr: TokenStream, item: &TokenStream) -> TokenStream {
    match expand(attr, item) {
        Ok(ts) => ts,
        Err(err) => {
            let err = err.to_compile_error();
            quote! { #err #item }
        }
    }
}

fn expand(attr: TokenStream, item: &TokenStream) -> Result<TokenStream, syn::Error> {
    let args: Args = syn::parse2(attr)?;
    let func: ItemFn = syn::parse2(item.clone())?;
    let caller = func.sig.ident.to_string();

    let calls = Analyzer::new(&args.clients).run(&func);

    // A declared client with no call in this function means the attribute is
    // aimed at the wrong place, or the client is reached through a shape this
    // analysis cannot see (a struct field, an index). Either way, the one
    // outcome that must not happen is passing while checking nothing.
    for (index, client) in args.clients.iter().enumerate() {
        if !calls.iter().any(|c| c.client == index) {
            let name = last_ident(client);
            return Err(syn::Error::new_spanned(
                client,
                format!(
                    "#[contract_checked] found no `{name}` call in `{caller}` to check. The \
                     client has to arrive as a parameter (`catalog: {name}`), a typed `let`, or \
                     `{name}::new(…)`, and be called through that name — one reached through a \
                     struct field or an index is not visible here."
                ),
            ));
        }
    }

    // Emitted beside the function, not spliced into its body: a `const` item
    // inside a function body resolves its value paths against that function's
    // own scope first, and the endpoint table is a module item.
    let assertions: Vec<TokenStream> = calls
        .iter()
        .flat_map(|call| assertions_for(call, &args.clients[call.client], &caller))
        .collect();
    let written = store::contract_dir().map_or_else(TokenStream::new, |dir| {
        let krate = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
        let descriptor = caller_descriptor(&krate, &caller, &calls, &args.clients);
        store::write_outcome(store::write_caller(
            &dir,
            &descriptor,
            &stable_key(&item.to_string()),
        ))
    });
    // The tracker goes in the body: a `const _` item is not allowed in an
    // `impl` block, and the body works for a free function and a method.
    let mut func = func;
    let track = store::track_contract_dir();
    func.block.stmts.insert(0, syn::parse_quote! { #track });
    Ok(quote! { #written #(#assertions)* #func })
}

/// The JSON descriptor for one checked function: what each call reads and
/// writes. `autumn contracts export` uses it as the caller's read-set.
fn caller_descriptor(
    krate: &str,
    caller: &str,
    calls: &[Call],
    clients: &[Path],
) -> CallerDescriptor {
    CallerDescriptor {
        krate: krate.to_owned(),
        function: caller.to_owned(),
        calls: calls
            .iter()
            .map(|call| CallerCall {
                client: path_text(&clients[call.client]),
                method: call.method.to_string(),
                reads: call.reads.clone(),
                writes: call.writes.clone().map(|mut writes| {
                    writes.sort();
                    writes.dedup();
                    writes
                }),
            })
            .collect(),
    }
}

/// A path as `a::b::C`, without generic arguments or a leading `::`.
fn path_text(path: &Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The last segment of a path — a client's or marker's own name.
fn last_ident(path: &Path) -> &Ident {
    &path
        .segments
        .last()
        .expect("a parsed path has at least one segment")
        .ident
}

/// One client call found in the annotated function.
#[derive(Debug)]
struct Call {
    /// The syntax node this call was found at, used as its identity.
    node: *const syn::ExprMethodCall,
    /// The client this call goes through.
    client: usize,
    /// The generated method's name — also the endpoint's name.
    method: Ident,
    /// Where the call is written.
    span: Span,
    /// Response fields the caller names, as dotted paths.
    reads: Vec<String>,
    /// The span of the first read of each field.
    read_spans: BTreeMap<String, Span>,
    /// In payload mode: whether the walk bound the payload anywhere.
    payload_seen: bool,
    /// Request fields the caller sets, as dotted paths, or `None` when the
    /// request is not an inline struct literal.
    writes: Option<Vec<String>>,
}

/// What a name in scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    /// One of the declared clients.
    Client(usize),
    /// A value inside the response of a call.
    Response(Target),
    /// Anything else. Recorded rather than omitted, so a rebinding shadows the
    /// name instead of leaving the old meaning in force.
    Other,
}

/// A value inside one call's response: the response itself, or a field of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    /// The index of the call in `Analyzer::calls`.
    call: usize,
    /// The field path from the response, as an index into `Analyzer::paths`.
    /// [`ROOT`] is the response itself.
    path: usize,
}

/// The `Analyzer::paths` index of the empty path: the response itself.
const ROOT: usize = 0;

impl Target {
    /// The response of call `call`.
    const fn root(call: usize) -> Self {
        Self { call, path: ROOT }
    }
}

/// Methods the generated client owns that are not endpoints.
const CLIENT_OWN_METHODS: [&str; 6] = ["new", "base_url", "clone", "to_owned", "eq", "fmt"];

/// Wrapper types a client is commonly injected through.
const CLIENT_WRAPPERS: [&str; 5] = ["Arc", "Rc", "Box", "State", "Extension"];

/// `Option` and `Result` methods whose last argument is a callback that gets
/// the success value.
const VALUE_CALLBACKS: [&str; 7] = [
    "map",
    "and_then",
    "inspect",
    "is_some_and",
    "is_ok_and",
    "map_or",
    "map_or_else",
];

/// Methods that hand back the same value, or the value it wraps:
/// `.unwrap()` on a call's result is still that call's result, and
/// `catalog.clone()` is still that client. An adapter that changes only the
/// error, only wraps the value, or only borrows it (`.map_err(f)`,
/// `.context(…)`, `.ok()`, `.as_ref()`), keeps it too.
const VALUE_ADAPTERS: [&str; 16] = [
    "unwrap",
    "expect",
    "unwrap_or_default",
    "clone",
    "to_owned",
    "map_err",
    "inspect_err",
    "context",
    "with_context",
    "ok",
    "ok_or",
    "ok_or_else",
    "as_ref",
    "as_mut",
    "as_deref",
    "as_deref_mut",
];

/// Macros whose tokens are never evaluated, so a field named inside one is not
/// a read. `matches!` is not one: it evaluates its first argument, and its
/// pattern cannot hold a `binding.field` read.
const UNEVALUATED_MACROS: [&str; 2] = ["stringify", "quote"];

/// One ordered walk of the function, tracking what each name in scope holds.
///
/// Ordered and scoped on purpose. A name-keyed map filled by one pass and read
/// by another cannot tell `let item = catalog.get_item(…)` from a later `for
/// item in rows`, so it would attribute the loop's field reads to the call —
/// failing a build over code that is wire-compatible, and checking the real
/// call against nothing.
struct Analyzer<'a> {
    /// The clients `#[contract_checked]` was told about.
    clients: &'a [Path],
    /// Innermost scope last.
    scopes: Vec<BTreeMap<String, Bound>>,
    /// Calls in the order they were found.
    calls: Vec<Call>,
    /// In payload mode, the event type whose bindings are `Response(0)`.
    payload: Option<&'a Path>,
    /// Whether a name was bound to the payload.
    payload_found: std::cell::Cell<bool>,
    /// Each field path a [`Target`] names. Index [`ROOT`] is the empty path.
    paths: std::cell::RefCell<Vec<String>>,
}

/// How a consumer function holds its event payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadBinding {
    /// The parameter at this index is the payload, whatever its type says.
    Param(usize),
    /// Every binding whose type names the event is the payload.
    Typed,
}

/// The fields a consumer reads off its event payload.
#[derive(Debug, Clone, Default)]
pub struct PayloadReads {
    /// Each field read, by Rust name, sorted, with the span of its first read.
    /// A field inside a field is a dotted path: `address.city`.
    pub reads: Vec<(String, Span)>,
    /// Whether the function binds the payload at all.
    pub found: bool,
}

/// The payload fields that `func` reads.
///
/// The same scoped walk as `#[contract_checked]`: the payload is one
/// "response", so field access, destructuring, macro bodies and shadowing all
/// follow the same rules.
#[must_use]
pub fn payload_reads(func: &ItemFn, event: &Path, binding: PayloadBinding) -> PayloadReads {
    let mut analyzer = Analyzer::new(&[]);
    analyzer.payload = Some(event);
    analyzer.calls.push(Call {
        node: std::ptr::null(),
        client: usize::MAX,
        method: Ident::new("payload", Span::call_site()),
        span: Span::call_site(),
        reads: Vec::new(),
        read_spans: BTreeMap::new(),
        payload_seen: false,
        writes: None,
    });
    let param = match binding {
        PayloadBinding::Param(index) => Some(index),
        PayloadBinding::Typed => None,
    };
    let found_param = param.is_some_and(|index| func.sig.inputs.len() > index);
    let mut calls = analyzer.run_with(func, param);
    let call = calls.swap_remove(0);
    PayloadReads {
        found: found_param || call.payload_seen,
        reads: call
            .reads
            .iter()
            .map(|f| {
                (
                    f.clone(),
                    call.read_spans.get(f).copied().unwrap_or(call.span),
                )
            })
            .collect(),
    }
}

impl<'a> Analyzer<'a> {
    fn new(clients: &'a [Path]) -> Self {
        Self {
            clients,
            scopes: vec![BTreeMap::new()],
            calls: Vec::new(),
            payload: None,
            payload_found: std::cell::Cell::new(false),
            paths: std::cell::RefCell::new(vec![String::new()]),
        }
    }

    /// The path of `field` inside `target`, for example `details.color`.
    fn path_of(&self, target: Target, field: &str) -> String {
        join_path(&self.paths.borrow()[target.path], field)
    }

    /// The target of `field` inside `target`.
    fn child(&self, target: Target, field: &str) -> Target {
        let path = self.path_of(target, field);
        let mut paths = self.paths.borrow_mut();
        let index = paths.iter().position(|p| *p == path).unwrap_or_else(|| {
            paths.push(path);
            paths.len() - 1
        });
        Target {
            call: target.call,
            path: index,
        }
    }

    /// Record that `field` inside `target` is read at `span`.
    fn read(&mut self, target: Target, field: &str, span: Span) {
        let path = self.path_of(target, field);
        let call = &mut self.calls[target.call];
        call.read_spans.entry(path.clone()).or_insert(span);
        call.reads.push(path);
    }

    /// Whether `ty` names the payload's event type.
    fn is_payload(&self, ty: &syn::Type) -> bool {
        match strip_refs(ty) {
            syn::Type::Path(tp) if tp.qself.is_none() => self.is_payload_path(&tp.path),
            _ => false,
        }
    }

    /// Whether `path` names the payload's event type.
    ///
    /// The written path must be a suffix of the declared one, after a
    /// leading `crate`, `self` or `super`. So `OrderPlaced` matches a
    /// declared `orders::OrderPlaced`, but `legacy::OrderPlaced` does not
    /// match a declared `OrderPlaced`: that is another type.
    fn is_payload_path(&self, path: &Path) -> bool {
        let Some(payload) = self.payload else {
            return false;
        };
        let mut written: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        while written
            .first()
            .is_some_and(|s| matches!(s.as_str(), "crate" | "self" | "super"))
        {
            written.remove(0);
        }
        let declared: Vec<String> = payload
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        !written.is_empty() && is_suffix(&declared, &written)
    }

    /// Whether `expr` decodes the payload through a turbofish:
    /// `m.payload_as::<E>()` or `serde_json::from_value::<E>(v)`.
    fn is_payload_turbofish(&self, expr: &Expr) -> bool {
        if self.payload.is_none() {
            return false;
        }
        let args = match peel(expr) {
            Expr::MethodCall(call) => call.turbofish.as_ref().map(|t| &t.args),
            Expr::Call(call) => match call.func.as_ref() {
                Expr::Path(path) => path.path.segments.last().and_then(|s| match &s.arguments {
                    syn::PathArguments::AngleBracketed(a) => Some(&a.args),
                    _ => None,
                }),
                _ => None,
            },
            _ => None,
        };
        let found = args.is_some_and(|args| {
            args.iter().any(|arg| match arg {
                syn::GenericArgument::Type(ty) => self.is_payload(ty),
                _ => false,
            })
        });
        if found {
            self.payload_found.set(true);
        }
        found
    }

    /// Walk a function: its parameters bind in the body's scope.
    fn run(self, func: &ItemFn) -> Vec<Call> {
        self.run_with(func, None)
    }

    /// Walk a function. The parameter at `payload_param`, if any, is the
    /// payload, whatever its type says.
    fn run_with(mut self, func: &ItemFn, payload_param: Option<usize>) -> Vec<Call> {
        for (index, arg) in func.sig.inputs.iter().enumerate() {
            if let syn::FnArg::Typed(pat) = arg {
                let bound = if payload_param == Some(index) || self.is_payload(&pat.ty) {
                    Bound::Response(Target::root(0))
                } else {
                    self.client_index(&pat.ty)
                        .map_or(Bound::Other, Bound::Client)
                };
                self.bind_pattern(&pat.pat, bound);
            }
        }
        self.visit_block(&func.block);
        let found = self.payload_found.get();
        for call in &mut self.calls {
            with_parents(call);
            call.payload_seen = found;
        }
        self.calls
    }

    fn lookup(&self, name: &str) -> Option<Bound> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn bind(&mut self, name: String, bound: Bound) {
        if self.payload.is_some() && matches!(bound, Bound::Response(t) if t.call == 0) {
            self.payload_found.set(true);
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, bound);
        }
    }

    /// Bind every name a pattern introduces.
    ///
    /// Only a plain `x` carries the incoming meaning. A struct pattern over a
    /// response binds each field to its path: after `let Item { details, .. }
    /// = item;`, `details.color` reads `details.color`. Other destructuring
    /// patterns bind [`Bound::Other`] and stop attribution.
    fn bind_pattern(&mut self, pat: &syn::Pat, bound: Bound) {
        match pat {
            syn::Pat::Ident(ident) => {
                self.bind(field_name(&ident.ident), bound);
                // `x @ Item { .. }`: the sub-pattern sees the same value.
                if let Some((_, sub)) = &ident.subpat {
                    let sub_bound = match bound {
                        Bound::Response(_) => bound,
                        _ => Bound::Other,
                    };
                    self.bind_pattern(sub, sub_bound);
                }
            }
            syn::Pat::Type(p) => {
                let bound = if self.is_payload(&p.ty) {
                    Bound::Response(Target::root(0))
                } else {
                    self.client_index(&p.ty).map_or(bound, Bound::Client)
                };
                self.bind_pattern(&p.pat, bound);
            }
            syn::Pat::Reference(p) => self.bind_pattern(&p.pat, bound),
            syn::Pat::Paren(p) => self.bind_pattern(&p.pat, bound),
            syn::Pat::Struct(p) => {
                // `Item { id, name, .. }` over a response names the fields
                // it reads. A pattern that names the payload type is the
                // payload, whatever its value comes from.
                let bound = if self.is_payload_path(&p.path) {
                    Bound::Response(Target::root(0))
                } else {
                    bound
                };
                if let Bound::Response(target) = bound {
                    if self.payload.is_some() && target.call == 0 {
                        self.payload_found.set(true);
                    }
                    for field in &p.fields {
                        if let syn::Member::Named(ident) = &field.member {
                            self.read(target, &field_name(ident), ident.span());
                        }
                    }
                }
                for field in &p.fields {
                    let inner = match (bound, &field.member) {
                        (Bound::Response(target), syn::Member::Named(ident)) => {
                            Bound::Response(self.child(target, &field_name(ident)))
                        }
                        _ => Bound::Other,
                    };
                    self.bind_pattern(&field.pat, inner);
                }
            }
            syn::Pat::TupleStruct(p) => {
                // `Ok(x)` or `Some(x)` over a response holds the response:
                // `match m.payload_as::<E>() { Ok(event) => … }`.
                let wraps = p.elems.len() == 1
                    && p.path
                        .segments
                        .last()
                        .is_some_and(|s| s.ident == "Ok" || s.ident == "Some");
                let inner = match bound {
                    Bound::Response(_) if wraps => bound,
                    _ => Bound::Other,
                };
                for elem in &p.elems {
                    self.bind_pattern(elem, inner);
                }
            }
            syn::Pat::Tuple(p) => {
                for elem in &p.elems {
                    self.bind_pattern(elem, Bound::Other);
                }
            }
            syn::Pat::Slice(p) => {
                for elem in &p.elems {
                    self.bind_pattern(elem, Bound::Other);
                }
            }
            syn::Pat::Or(p) => {
                for case in &p.cases {
                    self.bind_pattern(case, Bound::Other);
                }
            }
            syn::Pat::Guard(p) => self.bind_pattern(&p.pat, Bound::Other),
            _ => {}
        }
    }

    /// Which declared client a type names, if exactly one.
    ///
    /// Matched by path suffix rather than by last segment alone, so
    /// `client = a::Client` and `client = b::Client` stay distinct. A common
    /// injection wrapper — `Arc<…>`, `State<…>` — is looked through once.
    fn client_index(&self, ty: &syn::Type) -> Option<usize> {
        let ty = strip_refs(ty);
        let syn::Type::Path(tp) = ty else { return None };
        let last = tp.path.segments.last()?;
        if CLIENT_WRAPPERS.contains(&last.ident.to_string().as_str())
            && let syn::PathArguments::AngleBracketed(args) = &last.arguments
            && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
        {
            return self.client_index(inner);
        }
        let segments: Vec<String> = tp
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        let mut matches = self.clients.iter().enumerate().filter(|(_, client)| {
            let declared: Vec<String> = client
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            is_suffix(&segments, &declared) || is_suffix(&declared, &segments)
        });
        let first = matches.next()?;
        // Two declared clients a binding could equally be: refuse to guess.
        // The caller's "no calls to check" error then names the problem.
        matches.next().is_none().then_some(first.0)
    }

    /// The client index when `expr` calls one of a client's own associated
    /// functions — `let catalog = CatalogClient::new(…)`, where the
    /// constructor names the type even though the binding does not.
    fn constructor_client(&self, expr: &Expr) -> Option<usize> {
        let Expr::Call(call) = peel(expr) else {
            return None;
        };
        let Expr::Path(path) = call.func.as_ref() else {
            return None;
        };
        let segments = &path.path.segments;
        if segments.len() < 2 {
            return None;
        }
        // Drop the associated function to leave the type's own path.
        let mut owner = path.path.clone();
        owner.segments.pop();
        owner.segments.pop_punct();
        self.client_index(&syn::Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path: owner,
        }))
    }

    /// What an expression evaluates to, when that is knowable.
    fn bound_of(&self, expr: &Expr) -> Option<Bound> {
        if self.is_payload_turbofish(expr) {
            return Some(Bound::Response(Target::root(0)));
        }
        let recorded = |call: &syn::ExprMethodCall| {
            let node = std::ptr::from_ref(call);
            self.calls.iter().any(|c| c.node == node)
        };
        match peel_until(expr, &recorded) {
            Expr::Path(path) => self.lookup(&field_name(path.path.get_ident()?)),
            Expr::MethodCall(call) => {
                let node = std::ptr::from_ref(call);
                self.calls
                    .iter()
                    .position(|c| c.node == node)
                    .map(|call| Bound::Response(Target::root(call)))
            }
            // `item.details`: a field of a response is a value inside it.
            Expr::Field(field) => match (self.bound_of(&field.base)?, &field.member) {
                (Bound::Response(target), syn::Member::Named(ident)) => {
                    Some(Bound::Response(self.child(target, &field_name(ident))))
                }
                _ => None,
            },
            Expr::Match(m) => self.match_passes_through(m).map(Bound::Response),
            // `{ m.payload_as::<E>()? }`: a block that is only its tail. A
            // block with more statements can rebind a name the tail uses, so
            // it is not looked into.
            Expr::Block(b) => match b.block.stmts.as_slice() {
                [Stmt::Expr(tail, None)] => self.bound_of(tail),
                _ => None,
            },
            _ => None,
        }
    }

    /// The response a `match` hands back unchanged, if any.
    ///
    /// `match m.payload_as::<E>() { Ok(e) => e, Err(err) => return … }` is
    /// the payload: each arm either gives back the value its pattern binds
    /// (`x`, `Ok(x)`, `Some(x)`), or does not finish (`return`, `break`,
    /// `continue`, `panic!`, `unreachable!`, `todo!`).
    fn match_passes_through(&self, m: &syn::ExprMatch) -> Option<Target> {
        let target = self.response_of(&m.expr)?;
        let mut passes = false;
        for arm in &m.arms {
            if diverges(&arm.body) {
                continue;
            }
            let bound = bound_name(&arm.pat)?;
            let Expr::Path(body) = peel(&arm.body) else {
                return None;
            };
            if body.path.get_ident().map(field_name) != Some(bound) {
                return None;
            }
            passes = true;
        }
        passes.then_some(target)
    }

    /// The value inside a response that an expression holds.
    fn response_of(&self, expr: &Expr) -> Option<Target> {
        match self.bound_of(expr) {
            Some(Bound::Response(target)) => Some(target),
            _ => None,
        }
    }

    /// Record a method call on a client binding as an endpoint call.
    fn record_call(&mut self, call: &syn::ExprMethodCall) {
        let Some(Bound::Client(client)) = self.bound_of(&call.receiver) else {
            return;
        };
        if CLIENT_OWN_METHODS.contains(&call.method.to_string().as_str()) {
            return;
        }
        self.calls.push(Call {
            node: std::ptr::from_ref(call),
            client,
            method: call.method.clone(),
            // The method name, not the whole expression: a multi-line call
            // chain's own span points at the receiver, which says nothing
            // about which call is at fault.
            span: call.method.span(),
            reads: Vec::new(),
            read_spans: BTreeMap::new(),
            payload_seen: false,
            writes: request_write_set(call.args.last()),
        });
    }

    /// A `let`: the initializer is evaluated first, then the name it binds
    /// takes effect — so a rebinding cannot reach backwards.
    fn local(&mut self, local: &syn::Local) {
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
        let bound = local.init.as_ref().map_or(Bound::Other, |init| {
            self.bound_of(&init.expr)
                .or_else(|| self.constructor_client(&init.expr).map(Bound::Client))
                .unwrap_or(Bound::Other)
        });
        // `let Item { id, name, .. } = …` names the fields it reads: the
        // pattern records them as it binds.
        self.bind_pattern(&local.pat, bound);
    }

    /// Walk a macro's tokens for `binding.field`.
    ///
    /// In an Autumn app most response fields are read inside one
    /// (`html! { (item.name) }`), and the body is an unexpanded token stream
    /// that no AST walk reaches.
    fn scan_macro(&mut self, mac: &syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|s| UNEVALUATED_MACROS.contains(&s.ident.to_string().as_str()))
        {
            return;
        }
        // `matches!(value, Pattern { field, .. })` is an `if let`: the
        // pattern reads the fields it names.
        if mac
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "matches")
            && let Ok((value, pat, guard)) = mac.parse_body_with(parse_matches)
        {
            self.visit_expr(&value);
            self.scopes.push(BTreeMap::new());
            let bound = self.response_bound(&value);
            self.bind_pattern(&pat, bound);
            if let Some(guard) = guard {
                self.visit_expr(&guard);
            }
            self.scopes.pop();
            return;
        }
        let mut reads = Vec::new();
        scan_tokens(
            &mac.tokens,
            &MacroScope {
                resolve: &|name| match self.lookup(name) {
                    Some(Bound::Response(target)) => Some(target),
                    _ => None,
                },
                child: &|target, field| self.child(target, field),
            },
            &BTreeMap::new(),
            &mut reads,
        );
        for (target, field, span) in reads {
            self.read(target, &field, span);
        }
    }
}

impl Visit<'_> for Analyzer<'_> {
    fn visit_block(&mut self, block: &syn::Block) {
        self.scopes.push(BTreeMap::new());
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
        }
        self.scopes.pop();
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            // A nested `fn`, `struct` or `impl` has its own scope; none of
            // this function's bindings are visible inside it.
            Stmt::Item(_) => {}
            Stmt::Local(local) => self.local(local),
            other => visit::visit_stmt(self, other),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        // A typed decode is the payload even when nothing binds it:
        // `record(m.payload_as::<E>()?)`. The check marks it found.
        if matches!(expr, Expr::MethodCall(_) | Expr::Call(_)) {
            self.is_payload_turbofish(expr);
        }
        match expr {
            Expr::MethodCall(call) => {
                self.visit_expr(&call.receiver);
                // `m.payload_as::<E>().map(|event| event.email)`: the
                // callback gets the success value, so its first parameter
                // holds the response.
                let callback = VALUE_CALLBACKS
                    .contains(&call.method.to_string().as_str())
                    .then(|| self.response_bound(&call.receiver))
                    .filter(|bound| matches!(bound, Bound::Response(_)));
                let last = call.args.len().saturating_sub(1);
                for (i, arg) in call.args.iter().enumerate() {
                    match (callback, arg) {
                        (Some(bound), Expr::Closure(closure)) if i == last => {
                            self.scopes.push(BTreeMap::new());
                            for (n, input) in closure.inputs.iter().enumerate() {
                                self.bind_pattern(input, if n == 0 { bound } else { Bound::Other });
                            }
                            self.visit_expr(&closure.body);
                            self.scopes.pop();
                        }
                        _ => self.visit_expr(arg),
                    }
                }
                self.record_call(call);
            }
            Expr::Field(field) => {
                self.visit_expr(&field.base);
                if let (Some(target), syn::Member::Named(name)) =
                    (self.response_of(&field.base), &field.member)
                {
                    self.read(target, &field_name(name), name.span());
                }
            }
            Expr::Closure(closure) => {
                self.scopes.push(BTreeMap::new());
                for input in &closure.inputs {
                    self.bind_pattern(input, Bound::Other);
                }
                self.visit_expr(&closure.body);
                self.scopes.pop();
            }
            Expr::ForLoop(for_loop) => {
                self.visit_expr(&for_loop.expr);
                self.scopes.push(BTreeMap::new());
                self.bind_pattern(&for_loop.pat, Bound::Other);
                self.visit_block(&for_loop.body);
                self.scopes.pop();
            }
            Expr::Let(let_expr) => {
                self.visit_expr(&let_expr.expr);
                let bound = self.response_bound(&let_expr.expr);
                self.bind_pattern(&let_expr.pat, bound);
            }
            // An `if let` / `while let` binding lives in the condition and the
            // body only: not in the `else` branch, and not after the loop.
            Expr::If(if_expr) => {
                self.scopes.push(BTreeMap::new());
                self.visit_expr(&if_expr.cond);
                self.visit_block(&if_expr.then_branch);
                self.scopes.pop();
                if let Some((_, else_branch)) = &if_expr.else_branch {
                    self.visit_expr(else_branch);
                }
            }
            Expr::While(while_expr) => {
                self.scopes.push(BTreeMap::new());
                self.visit_expr(&while_expr.cond);
                self.visit_block(&while_expr.body);
                self.scopes.pop();
            }
            Expr::Match(m) => {
                self.visit_expr(&m.expr);
                let bound = self.response_bound(&m.expr);
                for arm in &m.arms {
                    self.arm(arm, bound);
                }
            }
            Expr::Macro(mac) => self.scan_macro(&mac.mac),
            other => visit::visit_expr(self, other),
        }
    }

    fn visit_arm(&mut self, arm: &syn::Arm) {
        self.arm(arm, Bound::Other);
    }

    fn visit_stmt_macro(&mut self, stmt: &syn::StmtMacro) {
        self.scan_macro(&stmt.mac);
    }
}

impl Analyzer<'_> {
    /// The bound a scrutinee passes to its pattern: a response stays a
    /// response, anything else is `Other`.
    fn response_bound(&self, expr: &Expr) -> Bound {
        self.response_of(expr).map_or(Bound::Other, Bound::Response)
    }

    /// A match arm: the pattern binds `bound`, then the guard and the body run
    /// in the arm's own scope.
    fn arm(&mut self, arm: &syn::Arm, bound: Bound) {
        self.scopes.push(BTreeMap::new());
        let (pat, guard) = crate::parse::arm_pat_and_guard(arm);
        self.bind_pattern(pat, bound);
        if let Some(guard) = guard {
            self.visit_expr(guard);
        }
        self.visit_expr(&arm.body);
        self.scopes.pop();
    }
}

/// Whether `needle` is a suffix of `haystack`.
fn is_suffix(haystack: &[String], needle: &[String]) -> bool {
    needle.len() <= haystack.len() && haystack[haystack.len() - needle.len()..] == *needle
}

/// A field or binding name as the descriptor records it — `r#type` is `type`,
/// which is both what serde writes and what the shape tables carry.
fn field_name(ident: &Ident) -> String {
    let raw = ident.to_string();
    raw.strip_prefix("r#").unwrap_or(&raw).to_owned()
}

/// The arguments of `matches!(value, pattern if guard)`.
fn parse_matches(
    input: syn::parse::ParseStream<'_>,
) -> syn::Result<(Expr, syn::Pat, Option<Expr>)> {
    let value: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    let pat = syn::Pat::parse_multi_with_leading_vert(input)?;
    let guard = if input.peek(Token![if]) {
        input.parse::<Token![if]>()?;
        Some(input.parse::<Expr>()?)
    } else {
        None
    };
    if input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
    }
    Ok((value, pat, guard))
}

/// The one name a pattern binds to the whole value: `x`, `Ok(x)`, `Some(x)`.
fn bound_name(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Ident(p) if p.subpat.is_none() => Some(field_name(&p.ident)),
        syn::Pat::TupleStruct(p)
            if p.elems.len() == 1
                && p.path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "Ok" || s.ident == "Some") =>
        {
            bound_name(&p.elems[0])
        }
        _ => None,
    }
}

/// Whether an arm body never finishes with a value.
fn diverges(expr: &Expr) -> bool {
    match expr {
        Expr::Return(_) | Expr::Break(_) | Expr::Continue(_) => true,
        Expr::Macro(m) => m.mac.path.segments.last().is_some_and(|s| {
            matches!(
                s.ident.to_string().as_str(),
                "panic" | "unreachable" | "todo" | "unimplemented" | "bail"
            )
        }),
        Expr::Block(b) => b.block.stmts.last().is_some_and(|s| match s {
            Stmt::Expr(e, _) => diverges(e),
            Stmt::Macro(m) => diverges(&Expr::Macro(syn::ExprMacro {
                attrs: Vec::new(),
                mac: m.mac.clone(),
            })),
            _ => false,
        }),
        _ => false,
    }
}

/// Peel the wrappers that sit between a call and the value it produces.
fn peel(expr: &Expr) -> &Expr {
    peel_until(expr, &|_| false)
}

/// [`peel`], but stop at a method call `stop` accepts. A recorded endpoint
/// call named like an adapter (`catalog.context(…)`) is a call, not an
/// adapter.
fn peel_until<'e>(expr: &'e Expr, stop: &dyn Fn(&syn::ExprMethodCall) -> bool) -> &'e Expr {
    let peel = |e: &'e Expr| peel_until(e, stop);
    match expr {
        Expr::MethodCall(e) if stop(e) => expr,
        Expr::Await(e) => peel(&e.base),
        Expr::Try(e) => peel(&e.expr),
        Expr::Paren(e) => peel(&e.expr),
        Expr::Group(e) => peel(&e.expr),
        Expr::Reference(e) => peel(&e.expr),
        Expr::Unary(e) if matches!(e.op, syn::UnOp::Deref(_)) => peel(&e.expr),
        // Methods that hand back the same value: `.unwrap()` on a call's
        // result is still that call's result, and `catalog.clone()` is still
        // that client. An adapter that changes only the error, only wraps the
        // value, or only borrows it (`.map_err(f)`, `.context(…)`, `.ok()`,
        // `.as_ref()`), keeps it too.
        Expr::MethodCall(e) if VALUE_ADAPTERS.contains(&e.method.to_string().as_str()) => {
            peel(&e.receiver)
        }
        other => other,
    }
}

/// Strip `&`/`&mut` from a type.
fn strip_refs(ty: &syn::Type) -> &syn::Type {
    match ty {
        syn::Type::Reference(r) => strip_refs(&r.elem),
        syn::Type::Paren(p) => strip_refs(&p.elem),
        other => other,
    }
}

/// The write-set of a request argument, sorted.
///
/// `None` when the request is not an inline struct literal, because then the
/// fields the call site sets are not visible here. A field whose value is a
/// struct literal adds the fields it sets as dotted paths: `details` and
/// `details.color`.
fn request_write_set(arg: Option<&Expr>) -> Option<Vec<String>> {
    let Some(Expr::Struct(lit)) = arg.map(peel) else {
        return None;
    };
    let mut writes = Vec::new();
    literal_writes(lit, "", &mut writes);
    writes.sort();
    writes.dedup();
    Some(writes)
}

/// Push each named field that `lit` sets, under `prefix`, and the fields of
/// each nested literal.
fn literal_writes(lit: &syn::ExprStruct, prefix: &str, out: &mut Vec<String>) {
    for field in &lit.fields {
        let syn::Member::Named(ident) = &field.member else {
            continue;
        };
        let path = join_path(prefix, &field_name(ident));
        if let Some(inner) = nested_literal(&field.expr) {
            literal_writes(inner, &path, out);
        }
        out.push(path);
    }
}

/// The struct literal a field value holds: `Details { … }`, also in
/// `Some(…)` or `Box::new(…)`.
fn nested_literal(expr: &Expr) -> Option<&syn::ExprStruct> {
    match peel(expr) {
        Expr::Struct(lit) => Some(lit),
        Expr::Call(call) if call.args.len() == 1 => {
            let Expr::Path(func) = call.func.as_ref() else {
                return None;
            };
            let names: Vec<String> = func
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            let wraps = matches!(names.last().map(String::as_str), Some("Some"))
                || names.ends_with(&["Box".to_owned(), "new".to_owned()]);
            if wraps {
                nested_literal(&call.args[0])
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `field` under `prefix`: `prefix.field`, or `field` for an empty prefix.
fn join_path(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_owned()
    } else {
        format!("{prefix}.{field}")
    }
}

/// The first segment of a dotted path: `details` of `details.color`.
#[must_use]
pub fn top_field(path: &str) -> &str {
    path.split_once('.').map_or(path, |(top, _)| top)
}

/// The first segment of each path, sorted and without duplicates.
fn top_fields(paths: &[String]) -> Vec<String> {
    let set: std::collections::BTreeSet<&str> = paths.iter().map(|p| top_field(p)).collect();
    set.into_iter().map(str::to_owned).collect()
}

/// Sort the reads of `call`, drop duplicates, and add each parent path:
/// `details.color` also reads `details`. A parent gets the span of its first
/// read when it has none of its own.
///
/// The parent keeps the top-level read in the descriptor. An older CLI does
/// not find a dotted path, but it still finds `details`.
fn with_parents(call: &mut Call) {
    let mut all = Vec::new();
    for read in &call.reads {
        let span = call.read_spans.get(read).copied();
        for (at, _) in read.match_indices('.') {
            let parent = &read[..at];
            if let Some(span) = span {
                call.read_spans.entry(parent.to_owned()).or_insert(span);
            }
            all.push(parent.to_owned());
        }
        all.push(read.clone());
    }
    all.sort();
    all.dedup();
    call.reads = all;
}

/// A name a macro body binds for itself, and the token range it covers.
///
/// A closure's parameters (`|item| …`) cover its body: the tokens up to the
/// next `,` at the same level, or the end of the group. A `for item in … { }`
/// binding covers its `{ }` block. An `if let` or `while let` covers its
/// `{ }` block, and a plain `let … ;` covers the tokens after the `;`. The
/// names a `match` arm binds cover the `{ }` block of the `match`. Outside
/// that range the name keeps its outer meaning, so a read of the payload
/// there is still a read.
struct MacroBinding {
    names: Vec<String>,
    from: usize,
    to: usize,
    /// For a `let` that binds one name to the whole value (`d`, `Some(d)`):
    /// that name, and the token range of the value.
    value: Option<(String, std::ops::Range<usize>)>,
}

/// Whether `tt` is the punctuation `ch`.
fn is_punct(tt: Option<&proc_macro2::TokenTree>, ch: char) -> bool {
    matches!(tt, Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == ch)
}

/// Whether `tt` is a `{ }` group.
fn is_brace(tt: &proc_macro2::TokenTree) -> bool {
    matches!(tt, proc_macro2::TokenTree::Group(g) if g.delimiter() == proc_macro2::Delimiter::Brace)
}

/// Every identifier in `tts`, also inside groups.
fn idents_in(tts: &[proc_macro2::TokenTree], out: &mut Vec<String>) {
    for tt in tts {
        match tt {
            proc_macro2::TokenTree::Ident(id) => out.push(field_name(id)),
            proc_macro2::TokenTree::Group(g) => {
                let inner: Vec<_> = g.stream().into_iter().collect();
                idents_in(&inner, out);
            }
            _ => {}
        }
    }
}

/// The one name a pattern binds to the whole value: `d`, `mut d`, `Some(d)`
/// or `Ok(d)`.
fn whole_value_name(pat: &[proc_macro2::TokenTree]) -> Option<String> {
    match pat {
        [proc_macro2::TokenTree::Ident(name)] => Some(field_name(name)),
        [
            proc_macro2::TokenTree::Ident(m),
            proc_macro2::TokenTree::Ident(name),
        ] if m == "mut" => Some(field_name(name)),
        [
            proc_macro2::TokenTree::Ident(wrap),
            proc_macro2::TokenTree::Group(g),
        ] if (wrap == "Some" || wrap == "Ok")
            && g.delimiter() == proc_macro2::Delimiter::Parenthesis =>
        {
            let inner: Vec<_> = g.stream().into_iter().collect();
            whole_value_name(&inner)
        }
        _ => None,
    }
}

/// The `let` binding whose keyword is at `i`, if its `=` is found.
fn let_binding(tts: &[proc_macro2::TokenTree], i: usize) -> Option<MacroBinding> {
    // The `=` of the binding: not part of `==`, `=>`, `<=` or `>=`.
    let eq = (i + 1..tts.len()).find(|&k| match &tts[k] {
        proc_macro2::TokenTree::Punct(p) => {
            p.as_char() == '='
                && p.spacing() == proc_macro2::Spacing::Alone
                && !matches!(&tts[k - 1], proc_macro2::TokenTree::Punct(q)
                    if q.spacing() == proc_macro2::Spacing::Joint)
        }
        _ => false,
    })?;
    let pat = &tts[i + 1..eq];
    let mut names = Vec::new();
    idents_in(pat, &mut names);
    let conditional = matches!(
        i.checked_sub(1).and_then(|k| tts.get(k)),
        Some(proc_macro2::TokenTree::Ident(kw)) if kw == "if" || kw == "while"
    );
    let (value_end, from, to) = if conditional {
        let body = (eq + 1..tts.len()).find(|&k| is_brace(&tts[k]))?;
        (body, body, body + 1)
    } else {
        let semi = (eq + 1..tts.len())
            .find(|&k| is_punct(tts.get(k), ';'))
            .unwrap_or(tts.len());
        (semi, semi + 1, tts.len())
    };
    let value = whole_value_name(pat).map(|name| (name, eq + 1..value_end));
    Some(MacroBinding {
        names,
        from,
        to,
        value,
    })
}

/// The names the arms of a `match` body bind: each arm's pattern, up to its
/// `if` guard or its `=>`.
fn match_arm_names(body: &[proc_macro2::TokenTree]) -> Vec<String> {
    let mut names = Vec::new();
    let mut start = 0;
    let mut k = 0;
    while k < body.len() {
        let arrow = is_punct(body.get(k), '=') && is_punct(body.get(k + 1), '>');
        if arrow {
            let pat_end = (start..k)
                .find(|&j| matches!(&body[j], proc_macro2::TokenTree::Ident(id) if id == "if"))
                .unwrap_or(k);
            idents_in(&body[start..pat_end], &mut names);
            k += 2;
            // The arm body: one `{ }` group, or the tokens up to a `,`.
            if body.get(k).is_some_and(is_brace) {
                k += 1;
                if is_punct(body.get(k), ',') {
                    k += 1;
                }
            } else {
                while k < body.len() && !is_punct(body.get(k), ',') {
                    k += 1;
                }
                k += 1;
            }
            start = k;
            continue;
        }
        k += 1;
    }
    names
}

/// The bindings one level of a macro body makes. Nested groups are scanned
/// on their own.
fn macro_bindings(tts: &[proc_macro2::TokenTree]) -> Vec<MacroBinding> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tts.len() {
        match &tts[i] {
            proc_macro2::TokenTree::Punct(p) if p.as_char() == '|' && opens_closure(tts, i) => {
                let mut names = Vec::new();
                let mut j = i + 1;
                while j < tts.len() && !is_punct(tts.get(j), '|') {
                    if let proc_macro2::TokenTree::Ident(id) = &tts[j] {
                        names.push(id.to_string());
                    }
                    j += 1;
                }
                let to = (j + 1..tts.len())
                    .find(|&k| is_punct(tts.get(k), ','))
                    .unwrap_or(tts.len());
                out.push(MacroBinding {
                    names,
                    from: j + 1,
                    to,
                    value: None,
                });
                i = j + 1;
            }
            proc_macro2::TokenTree::Ident(id) if id == "for" => {
                if let Some(proc_macro2::TokenTree::Ident(name)) = tts.get(i + 1)
                    && let Some(body) = (i + 2..tts.len()).find(|&k| is_brace(&tts[k]))
                {
                    out.push(MacroBinding {
                        names: vec![name.to_string()],
                        from: body,
                        to: body + 1,
                        value: None,
                    });
                }
                i += 1;
            }
            proc_macro2::TokenTree::Ident(id) if id == "let" => {
                out.extend(let_binding(tts, i));
                i += 1;
            }
            proc_macro2::TokenTree::Ident(id) if id == "match" => {
                if let Some(body) = (i + 1..tts.len()).find(|&k| is_brace(&tts[k]))
                    && let proc_macro2::TokenTree::Group(g) = &tts[body]
                {
                    let inner: Vec<_> = g.stream().into_iter().collect();
                    out.push(MacroBinding {
                        names: match_arm_names(&inner),
                        from: body,
                        to: body + 1,
                        value: None,
                    });
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether the `|` at `i` opens a closure's parameters.
///
/// A `|` after an operand is `a | b`, and the second `|` of `a || b` follows
/// the first. `move |x|` and a `|` at the start of an expression open one.
fn opens_closure(tts: &[proc_macro2::TokenTree], i: usize) -> bool {
    let prev = i.checked_sub(1).and_then(|p| tts.get(p));
    let after_operand = match prev {
        Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '|' => {
            is_operand(i.checked_sub(2).and_then(|p| tts.get(p)))
        }
        other => is_operand(other),
    };
    !after_operand
}

/// Whether `token` ends an operand: an identifier (not `move`), a literal or
/// a group.
fn is_operand(token: Option<&proc_macro2::TokenTree>) -> bool {
    match token {
        Some(proc_macro2::TokenTree::Ident(id)) => id != "move",
        Some(proc_macro2::TokenTree::Literal(_) | proc_macro2::TokenTree::Group(_)) => true,
        _ => false,
    }
}

/// How a macro body's names resolve: the outer scope, and how to step into
/// a field of a value.
struct MacroScope<'s, T> {
    /// What an outer name holds.
    resolve: &'s dyn Fn(&str) -> Option<T>,
    /// The value of a field inside a value.
    child: &'s dyn Fn(T, &str) -> T,
}

/// The names a macro body binds over one token: `None` hides the outer
/// meaning, `Some` is a value inside a response.
type MacroNames<T> = BTreeMap<String, Option<T>>;

/// Record every `binding.field` in a token stream, resolving the binding
/// through `scope`. `binding.a.b` records the path `a.b`.
///
/// Token-level, because a macro body is not parsed Rust. Four shapes that look
/// the same are excluded: `binding.method(…)`, `binding.field::<T>()`,
/// `binding.mac!(…)`, and `other.binding.field` — where `binding` is itself a
/// field of something else. A method, a turbofish or a macro after a later
/// segment ends the path before that segment. A `?` between segments does
/// not.
fn scan_tokens<T: Copy>(
    tokens: &proc_macro2::TokenStream,
    scope: &MacroScope<'_, T>,
    outer: &MacroNames<T>,
    reads: &mut Vec<(T, String, Span)>,
) {
    let tts: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    let bindings = macro_bindings(&tts);
    // The names in force at token `i`: the outer ones, then each binding of
    // this level that covers `i`.
    let names_at = |i: usize, values: &[Option<T>]| {
        let mut here = outer.clone();
        for (b, value) in bindings.iter().zip(values) {
            if !(b.from..b.to).contains(&i) {
                continue;
            }
            for name in &b.names {
                here.insert(name.clone(), None);
            }
            if let (Some((name, _)), Some(target)) = (&b.value, value) {
                here.insert(name.clone(), Some(*target));
            }
        }
        here
    };
    // A `let` over a response value binds its name to that value. The value
    // is read in the names in force where it is written.
    let mut values: Vec<Option<T>> = Vec::with_capacity(bindings.len());
    for b in &bindings {
        let value = b.value.as_ref().and_then(|(_, range)| {
            let here = names_at(range.start, &values);
            value_of(&tts[range.clone()], scope, &here)
        });
        values.push(value);
    }
    for (i, tt) in tts.iter().enumerate() {
        let here = names_at(i, &values);
        if let proc_macro2::TokenTree::Group(group) = tt {
            scan_tokens(&group.stream(), scope, &here, reads);
            continue;
        }
        let proc_macro2::TokenTree::Ident(base) = tt else {
            continue;
        };
        if is_punct(i.checked_sub(1).and_then(|k| tts.get(k)), '.') {
            continue;
        }
        let Some(target) = lookup_name(&base.to_string(), scope, &here) else {
            continue;
        };
        let mut path: Vec<String> = Vec::new();
        let mut span = None;
        let mut at = skip_try(&tts, i + 1);
        while let Some(field) = field_after(&tts, at) {
            path.push(field_name(field));
            span.get_or_insert_with(|| field.span());
            at = skip_try(&tts, at + 2);
        }
        if let Some(span) = span {
            reads.push((target, path.join("."), span));
        }
    }
}

/// What `name` holds in a macro body: a binding of the body, else the outer
/// meaning.
fn lookup_name<T: Copy>(name: &str, scope: &MacroScope<'_, T>, here: &MacroNames<T>) -> Option<T> {
    here.get(name)
        .map_or_else(|| (scope.resolve)(name), |bound| *bound)
}

/// The value inside a response that `tts` holds: `&item.details`,
/// `item.details.as_ref()`. `None` for any other expression.
fn value_of<T: Copy>(
    tts: &[proc_macro2::TokenTree],
    scope: &MacroScope<'_, T>,
    here: &MacroNames<T>,
) -> Option<T> {
    let mut k = 0;
    while is_punct(tts.get(k), '&')
        || matches!(tts.get(k), Some(proc_macro2::TokenTree::Ident(m)) if m == "mut")
    {
        k += 1;
    }
    let Some(proc_macro2::TokenTree::Ident(base)) = tts.get(k) else {
        return None;
    };
    let mut target = lookup_name(&base.to_string(), scope, here)?;
    k = skip_try(tts, k + 1);
    loop {
        if let Some(field) = field_after(tts, k) {
            target = (scope.child)(target, &field_name(field));
            k = skip_try(tts, k + 2);
        } else if is_punct(tts.get(k), '.')
            && matches!(tts.get(k + 1), Some(proc_macro2::TokenTree::Ident(m))
                if VALUE_ADAPTERS.contains(&m.to_string().as_str()))
            && matches!(tts.get(k + 2), Some(proc_macro2::TokenTree::Group(g))
                if g.delimiter() == proc_macro2::Delimiter::Parenthesis)
        {
            k = skip_try(tts, k + 3);
        } else {
            break;
        }
    }
    (k == tts.len()).then_some(target)
}

/// `at`, or the index after it when `tts[at]` is a `?`.
fn skip_try(tts: &[proc_macro2::TokenTree], at: usize) -> usize {
    if is_punct(tts.get(at), '?') {
        at + 1
    } else {
        at
    }
}

/// The field that `.field` at `tts[at]` reads, if it is a field read.
///
/// `.await`, a call, a turbofish and a macro are not field reads. The `!` of
/// `!=` is joined to its `=`: that is a comparison, so a read.
fn field_after(tts: &[proc_macro2::TokenTree], at: usize) -> Option<&Ident> {
    if !matches!(tts.get(at), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '.') {
        return None;
    }
    let Some(proc_macro2::TokenTree::Ident(field)) = tts.get(at + 1) else {
        return None;
    };
    if field == "await" {
        return None;
    }
    let next = tts.get(at + 2);
    let not_equal = matches!(next, Some(proc_macro2::TokenTree::Punct(p))
        if p.as_char() == '!' && p.spacing() == proc_macro2::Spacing::Joint)
        && matches!(tts.get(at + 3), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '=');
    let call = matches!(
        next,
        Some(proc_macro2::TokenTree::Group(g))
            if g.delimiter() == proc_macro2::Delimiter::Parenthesis
    ) || matches!(next, Some(proc_macro2::TokenTree::Punct(p)) if matches!(p.as_char(), ':' | '!'));
    (not_equal || !call).then_some(field)
}

/// The const assertions for one call site.
///
/// Every assertion is always emitted: the const tables are the authority. The
/// on-disk descriptor, when one resolves, is used only to write a better
/// message — naming the field a coverage failure is about, which a const-eval
/// message (a literal) cannot compute for itself. A stale descriptor can
/// therefore mislabel a failure, never cause or hide one.
///
/// Every assertion goes through the client's endpoint table BY METHOD NAME, so
/// a call to a method the client does not declare as an endpoint — one from
/// someone's own extension trait — is vacuously true rather than a reference
/// to a type that does not exist.
fn assertions_for(call: &Call, client: &Path, caller: &str) -> Vec<TokenStream> {
    let span = call.span;
    let method = &call.method;
    // The table sits beside the client, so the user's own path to the client
    // also reaches it — including `crate::api::CatalogClient`.
    let table = {
        let mut path = client.clone();
        let ident = endpoint_table_ident(last_ident(client));
        let last = path
            .segments
            .last_mut()
            .expect("a parsed path has at least one segment");
        last.ident = ident;
        last.arguments = syn::PathArguments::None;
        quote_spanned! { span => #path }
    };
    let method_lit = LitStr::new(&method.to_string(), span);
    let snippet = snippet_of(method);
    // One build shares each nested type, so rustc checks a nested field. The
    // assertions check the first segment of each path.
    let reads = top_fields(&call.reads);
    let writes = call.writes.as_deref().map(top_fields);
    let site = CallSite {
        caller: caller.to_owned(),
        snippet: snippet.clone(),
        method: method.to_string(),
        reads: reads.clone(),
        writes: writes.clone(),
    };
    let named = named_violations(&site);
    let enriched = |kind: &ViolationKind, fallback: String| -> String {
        named
            .iter()
            .find(|(k, _)| k == kind)
            .map_or(fallback, |(_, message)| message.clone())
    };

    let mut out = Vec::new();
    for read in &reads {
        let message = enriched(
            &ViolationKind::ResponseFieldMissing(read.clone()),
            format!(
                "wire contract broken in `{caller}` at `{snippet}`: reads response field \
                 `{read}` from endpoint `{method}`, which it no longer produces"
            ),
        );
        let field = LitStr::new(read, span);
        let message = LitStr::new(&escape_for_assert(&message), span);
        out.push(quote_spanned! { span =>
            const _: () = ::core::assert!(
                ::autumn_web::wire::client_produces(#table, #method_lit, #field),
                #message
            );
        });
    }

    let Some(writes) = &writes else {
        return out;
    };
    for write in writes {
        let message = enriched(
            &ViolationKind::RequestFieldUnknown(write.clone()),
            format!(
                "wire contract broken in `{caller}` at `{snippet}`: sets request field \
                 `{write}` on endpoint `{method}`, which does not accept it"
            ),
        );
        let field = LitStr::new(write, span);
        let message = LitStr::new(&escape_for_assert(&message), span);
        out.push(quote_spanned! { span =>
            const _: () = ::core::assert!(
                ::autumn_web::wire::client_accepts(#table, #method_lit, #field),
                #message
            );
        });
    }

    // Both ends share the request type, so serialization emits every field —
    // a `..rest` initializer sends a default VALUE, not nothing. The one shape
    // that can still drop a field the callee demands is one the request type
    // may keep off the wire: `skip_serializing_if`, or `skip_serializing`.
    let missing = named
        .iter()
        .find(|(k, _)| matches!(k, ViolationKind::RequestFieldMissing(_)))
        .map(|(_, message)| message.clone());
    let supplied: Vec<LitStr> = writes.iter().map(|w| LitStr::new(w, span)).collect();
    let message = LitStr::new(
        &escape_for_assert(&missing.unwrap_or_else(|| {
            format!(
                "wire contract broken in `{caller}` at `{snippet}`: endpoint `{method}` requires \
                 a request field that the request type does not always put on the wire, and the \
                 call site sets only [{}]",
                writes.join(", ")
            )
        })),
        span,
    );
    out.push(quote_spanned! { span =>
        const _: () = ::core::assert!(
            ::autumn_web::wire::client_request_covered(#table, #method_lit, &[#(#supplied),*]),
            #message
        );
    });
    out
}

/// Violations the on-disk descriptor can name, if exactly one endpoint matches.
///
/// Only an endpoint from *another* crate is consulted. Cargo builds a
/// dependency before its dependant, so a cross-crate descriptor is always the
/// one that was just written; a descriptor for the crate being compiled right
/// now is being written by this same run and may be half-written or left over.
/// Ambiguity (two crates with the same marker name) and absence both yield
/// nothing, leaving the const assertions to hold the contract on their own.
fn named_violations(site: &CallSite) -> Vec<(ViolationKind, String)> {
    let Some(dir) = store::contract_dir() else {
        return Vec::new();
    };
    let this_crate = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let found: Vec<_> = store::find_by_ident(&dir, &format!("{}_endpoint", site.method))
        .into_iter()
        .filter(|e| e.endpoint.krate != this_crate)
        .collect();
    let [endpoint] = found.as_slice() else {
        return Vec::new();
    };
    // An unresolved shape describes nothing, so it can only produce noise.
    if endpoint.response.serialized.is_empty() && endpoint.request.deserialized.is_empty() {
        return Vec::new();
    }
    check::check(site, endpoint)
        .into_iter()
        .map(|v| (v.kind.clone(), v.message()))
        .collect()
}

/// Escape a diagnostic for use as an `assert!` message.
///
/// `assert!(cond, "…")` hands its message to `panic!`, which reads it as a
/// FORMAT STRING — so a route like `/items/{id}` in the text becomes an
/// implicit capture of a variable named `id`, and the build fails with
/// "cannot find value `id`" instead of the contract error. Const panics cannot
/// take runtime arguments, so the braces are doubled instead.
pub fn escape_for_assert(message: &str) -> String {
    message.replace('{', "{{").replace('}', "}}")
}

/// How a diagnostic names the offending call. The compiler adds the file and
/// line from the span each assertion carries, so this only has to say which
/// call in the function is at fault.
fn snippet_of(method: &Ident) -> String {
    format!("{method}(…)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(attr: &str, item: &str) -> String {
        let attr: TokenStream = attr.parse().expect("attr parses");
        let item: TokenStream = item.parse().expect("item parses");
        contract_checked_macro(attr, &item).to_string()
    }

    fn calls_of(item: &str) -> Vec<Call> {
        let func: ItemFn = syn::parse_str(item).expect("fixture parses");
        let clients: Vec<Path> = vec![syn::parse_str("CatalogClient").expect("client path")];
        Analyzer::new(&clients).run(&func)
    }

    fn reads_of(item: &str) -> Vec<(String, Vec<String>)> {
        calls_of(item)
            .iter()
            .map(|c| (c.method.to_string(), c.reads.clone()))
            .collect()
    }

    #[test]
    fn a_field_read_off_the_response_binding_is_in_the_read_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn an_endpoint_named_like_an_adapter_is_still_a_call() {
        // `context` and `ok` are adapter names to `peel`, but here they are
        // the client's own endpoints.
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) -> Result<()> { \
             let response = catalog.context(NoBody).await?; let _ = response.email; \
             let other = catalog.ok(NoBody).await.unwrap(); let _ = other.name; Ok(()) }",
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].reads, ["email"]);
        assert_eq!(calls[1].reads, ["name"]);
    }

    #[test]
    fn a_field_read_straight_off_the_call_is_in_the_read_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) -> Result<()> { let _ = catalog.get_item(&id, NoBody).await?.name; Ok(()) }",
        );
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_destructuring_let_names_the_fields_it_reads() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let Item { id, name, .. } = catalog.get_item(&x, NoBody).await.unwrap(); }",
        );
        assert_eq!(calls[0].reads, ["id", "name"]);
    }

    #[test]
    fn a_client_built_in_the_body_is_recognised() {
        let calls = calls_of(
            "async fn page(http: Client) { let catalog = CatalogClient::new(url, http); let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_typed_let_is_recognised() {
        let calls = calls_of(
            "async fn page() { let catalog: CatalogClient = build(); let _ = catalog.get_item(&id, NoBody); }",
        );
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn a_client_behind_an_injection_wrapper_is_recognised() {
        for ty in [
            "Arc<CatalogClient>",
            "State<CatalogClient>",
            "&CatalogClient",
        ] {
            let calls = calls_of(&format!(
                "async fn page(catalog: {ty}) {{ let _ = catalog.get_item(&id, NoBody); }}"
            ));
            assert_eq!(calls.len(), 1, "`{ty}` must be recognised");
        }
    }

    #[test]
    fn a_clone_of_the_client_is_still_the_client() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.clone().get_item(&id, NoBody); }",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method.to_string(), "get_item");
    }

    // ── Scoping ──────────────────────────────────────────────────────────
    // A name-keyed map filled by one pass and read by another attributes every
    // `item.field` in the function to whichever call bound `item` last. Each
    // of these is code that is wire-compatible and must not be rejected.

    #[test]
    fn a_rebound_name_does_not_reach_backwards() {
        let reads = reads_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); let _ = item.name; \
             let item = catalog.list_items(NoBody).await.unwrap(); let _ = item.total; }",
        );
        assert!(
            reads.contains(&("get_item".to_owned(), vec!["name".to_owned()])),
            "{reads:?}"
        );
        assert!(
            reads.contains(&("list_items".to_owned(), vec!["total".to_owned()])),
            "{reads:?}"
        );
    }

    #[test]
    fn a_shadow_that_is_not_a_call_stops_attribution() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); let _ = item.name; \
             let item = Widget { colour: 1 }; let _ = item.colour; }",
        );
        assert_eq!(calls[0].reads, ["name"], "`colour` belongs to the Widget");
    }

    #[test]
    fn a_closure_parameter_shadows_the_response() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); \
             rows.iter().for_each(|item| { let _ = item.width; }); }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_loop_variable_shadows_the_response() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); \
             for item in rows { let _ = item.width; } }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_match_arm_binding_shadows_the_response() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); \
             match other { Thing { item } => { let _ = item.zap; } } }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_nested_functions_own_parameter_is_not_the_response() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); \
             fn helper(item: Other) -> u8 { item.bogus } }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_read_inside_a_nested_block_still_attributes_to_the_outer_binding() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&x, NoBody).await.unwrap(); \
             if flag { let _ = item.name; } }",
        );
        assert_eq!(calls[0].reads, ["name"]);
    }

    // ── Requests ─────────────────────────────────────────────────────────

    #[test]
    fn an_inline_request_literal_gives_the_write_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name, price_cents }); }",
        );
        assert_eq!(
            calls[0].writes.as_deref(),
            Some(["name".to_owned(), "price_cents".to_owned()].as_slice())
        );
    }

    #[test]
    fn a_rest_initializer_supplies_only_the_named_fields() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name, ..Default::default() }); }",
        );
        assert_eq!(
            calls[0].writes.as_deref(),
            Some(["name".to_owned()].as_slice())
        );
    }

    #[test]
    fn a_request_that_is_not_a_literal_has_an_unknown_write_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(body); }",
        );
        assert!(calls[0].writes.is_none());
    }

    // ── Macro bodies ─────────────────────────────────────────────────────

    #[test]
    fn a_field_read_inside_a_macro_body_is_in_the_read_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { h1 { (item.name) } p { (item.price_cents) } } }",
        );
        assert_eq!(calls[0].reads, ["name", "price_cents"]);
    }

    #[test]
    fn a_method_call_inside_a_macro_body_is_not_a_field_read() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { (item.clone()) (item.name) (item.await) (item.parse::<u32>()) } }",
        );
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_field_of_a_field_inside_a_macro_body_is_not_read_as_the_binding() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { (other.item.name) } }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_name_the_macro_binds_itself_is_not_the_response() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { @for item in &rows { (item.width) } } }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_macro_that_does_not_evaluate_its_tokens_contributes_no_reads() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = stringify!(item.not_read); }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    // ── Nested paths ─────────────────────────────────────────────────────

    /// The read-set of the one `get_item` call in `body`.
    fn nested_reads(body: &str) -> Vec<String> {
        let calls = calls_of(&format!(
            "async fn page(catalog: CatalogClient) {{ \
             let item = catalog.get_item(&id, NoBody).await.unwrap(); {body} }}"
        ));
        calls[0].reads.clone()
    }

    #[test]
    fn a_field_of_a_field_is_a_dotted_read() {
        assert_eq!(
            nested_reads("let _ = item.details.color;"),
            ["details", "details.color"]
        );
        assert_eq!(
            nested_reads("let _ = item.a.b.c;"),
            ["a", "a.b", "a.b.c"],
            "every level is in the read-set"
        );
    }

    #[test]
    fn a_dotted_read_keeps_each_parent_path() {
        assert_eq!(
            nested_reads("let _ = &item.details; let _ = item.details.color;"),
            ["details", "details.color"]
        );
        assert_eq!(
            nested_reads("let _ = item.details_x; let _ = item.details.color;"),
            ["details", "details.color", "details_x"],
            "a name that only starts the same is another field"
        );
    }

    #[test]
    fn a_method_call_ends_the_dotted_path() {
        assert_eq!(
            nested_reads("let _ = item.details.color.len();"),
            ["details", "details.color"]
        );
        assert_eq!(nested_reads("let _ = item.details.label();"), ["details"]);
    }

    #[test]
    fn a_try_between_segments_keeps_the_dotted_path() {
        assert_eq!(
            nested_reads("html! { (item.details?.color) }"),
            ["details", "details.color"]
        );
    }

    #[test]
    fn unwrap_or_default_keeps_the_dotted_path() {
        assert_eq!(
            nested_reads("let _ = item.details.unwrap_or_default().color;"),
            ["details", "details.color"]
        );
    }

    #[test]
    fn a_let_inside_a_macro_body_binds_a_nested_field() {
        for body in [
            "html! { @let d = &item.details; (d.color) }",
            "html! { @if let Some(d) = &item.details { (d.color) } }",
            "html! { @if let Some(d) = item.details.as_ref() { (d.color) } }",
        ] {
            assert_eq!(nested_reads(body), ["details", "details.color"], "{body}");
        }
    }

    #[test]
    fn a_let_inside_a_macro_body_shadows_the_outer_name() {
        for body in [
            "html! { @if let Some(item) = other { (item.foo) } }",
            "html! { @let item = other(); (item.foo) }",
            "html! { @match other { Some(item) => (item.foo), None => {} } }",
            "html! { @while let Some(item) = it.next() { (item.foo) } }",
        ] {
            assert!(nested_reads(body).is_empty(), "{body}");
        }
        assert_eq!(
            nested_reads("html! { @if let Some(item) = item.next { (item.foo) } (item.bar) }"),
            ["bar", "next", "next.foo"],
            "the right side and the code after the body see the outer name"
        );
    }

    #[test]
    fn a_let_inside_a_macro_body_over_a_sub_binding_extends_its_path() {
        assert_eq!(
            nested_reads(
                "let Item { details, .. } = item; \
                 html! { @if let Some(details) = details.extra { (details.color) } }"
            ),
            ["details", "details.extra", "details.extra.color"]
        );
    }

    #[test]
    fn a_tuple_index_ends_the_dotted_path() {
        assert_eq!(nested_reads("let _ = item.pair.0.color;"), ["pair"]);
    }

    #[test]
    fn a_dotted_read_inside_a_macro_body_is_in_the_read_set() {
        assert_eq!(
            nested_reads("html! { (item.details.color) }"),
            ["details", "details.color"]
        );
        assert_eq!(
            nested_reads("html! { (item.details.size.max(1)) }"),
            ["details", "details.size"]
        );
        assert_eq!(
            nested_reads("html! { (item.details.color != x) }"),
            ["details", "details.color"]
        );
        for call in ["label()", "parse::<u32>()", "mac!()"] {
            assert_eq!(
                nested_reads(&format!("html! {{ (item.details.{call}) }}")),
                ["details"],
                "{call}"
            );
        }
    }

    #[test]
    fn a_binding_of_a_nested_field_keeps_its_path() {
        assert_eq!(
            nested_reads("let details = &item.details; let _ = details.color;"),
            ["details", "details.color"]
        );
        assert_eq!(
            nested_reads("let d = item.details.clone(); html! { (d.size) }"),
            ["details", "details.size"]
        );
    }

    #[test]
    fn a_rebound_sub_binding_stops_attribution() {
        assert_eq!(
            nested_reads("let d = &item.details; let d = other(); let _ = d.color;"),
            ["details"]
        );
    }

    #[test]
    fn a_destructured_nested_field_keeps_its_path() {
        assert_eq!(
            nested_reads("let Item { details, .. } = item; let _ = details.color;"),
            ["details", "details.color"]
        );
        assert_eq!(
            nested_reads("let Item { details: Details { color, .. }, id } = item;"),
            ["details", "details.color", "id"]
        );
    }

    #[test]
    fn an_unwrapped_optional_nested_field_keeps_its_path() {
        assert_eq!(
            nested_reads("let _ = item.details.as_ref().unwrap().color;"),
            ["details", "details.color"]
        );
        assert_eq!(
            nested_reads("let _ = item.details.map(|d| d.color);"),
            ["details", "details.color"]
        );
    }

    #[test]
    fn a_nested_request_literal_gives_dotted_writes() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { \
             name, details: Details { color: Some(c), ..Default::default() } }); }",
        );
        assert_eq!(
            calls[0].writes.as_deref(),
            Some(
                [
                    "details".to_owned(),
                    "details.color".to_owned(),
                    "name".to_owned()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn a_nested_literal_in_some_or_a_box_gives_dotted_writes() {
        for wrapped in ["Some(Details { color })", "Box::new(Details { color })"] {
            let calls = calls_of(&format!(
                "async fn page(catalog: CatalogClient) {{ \
                 let _ = catalog.create_item(NewItem {{ details: {wrapped} }}); }}"
            ));
            assert_eq!(
                calls[0].writes.as_deref(),
                Some(["details".to_owned(), "details.color".to_owned()].as_slice()),
                "{wrapped}"
            );
        }
    }

    #[test]
    fn a_nested_value_that_is_not_a_literal_has_no_dotted_writes() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { \
             let _ = catalog.create_item(NewItem { details: d, name: Some(n) }); }",
        );
        assert_eq!(
            calls[0].writes.as_deref(),
            Some(["details".to_owned(), "name".to_owned()].as_slice())
        );
    }

    #[test]
    fn the_assertions_use_the_first_segment_of_a_nested_path() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { \
             let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.details.color; \
             catalog.create_item(NewItem { details: Details { color: c } }).await.unwrap(); }",
        );
        assert!(out.contains("reads response field `details`"), "{out}");
        assert!(out.contains("sets request field `details`"), "{out}");
        assert!(!out.contains("\"details.color\""), "{out}");
        assert!(!out.contains("`details.color`"), "{out}");
    }

    #[test]
    fn the_caller_descriptor_holds_dotted_paths() {
        let func: ItemFn = syn::parse_str(
            "async fn page(catalog: CatalogClient) { \
               let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.details.color; \
               catalog.create_item(NewItem { details: Details { color: c } }).await.unwrap(); }",
        )
        .expect("fixture parses");
        let clients: Vec<Path> = vec![syn::parse_str("CatalogClient").expect("client path")];
        let calls = Analyzer::new(&clients).run(&func);
        let got = caller_descriptor("storefront", "page", &calls, &clients);
        assert_eq!(got.calls[0].reads, ["details", "details.color"]);
        assert_eq!(
            got.calls[1].writes.as_deref(),
            Some(["details".to_owned(), "details.color".to_owned()].as_slice())
        );
    }

    // ── Other ────────────────────────────────────────────────────────────

    #[test]
    fn a_call_on_something_that_is_not_a_client_is_ignored() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient, other: Vec<u8>) { let _ = catalog.get_item(&x, NoBody); let _ = other.len(); }",
        );
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn a_raw_identifier_field_is_recorded_without_its_prefix() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.r#type; }",
        );
        assert_eq!(calls[0].reads, ["type"], "the shape table records `type`");
    }

    #[test]
    fn the_expansion_routes_every_assertion_through_the_clients_endpoint_table() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert!(
            out.contains("__AUTUMN_WIRE_ENDPOINTS_CatalogClient"),
            "{out}"
        );
        assert!(out.contains("client_produces"), "{out}");
        assert!(out.contains("reads response field `name`"), "{out}");
    }

    #[test]
    fn the_expansion_always_emits_the_request_coverage_assertion() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name, ..Default::default() }); }",
        );
        assert!(out.contains("client_request_covered"), "{out}");
    }

    #[test]
    fn a_client_with_no_call_in_the_function_is_refused() {
        let out = expand_str("client = CatalogClient", "async fn page() { let _ = 1; }");
        assert!(out.contains("found no `CatalogClient` call"), "{out}");
        // Having a value but never calling it is the same vacuous outcome.
        let unused = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let _ = 1; }",
        );
        assert!(unused.contains("found no `CatalogClient` call"), "{unused}");
    }

    #[test]
    fn a_missing_client_argument_is_refused() {
        let out = expand_str("", "async fn page() {}");
        assert!(out.contains("needs a client"), "{out}");
    }

    #[test]
    fn an_unknown_argument_is_refused_rather_than_ignored() {
        let out = expand_str("clietn = CatalogClient", "async fn page() {}");
        assert!(
            out.contains("unknown #[contract_checked] argument"),
            "{out}"
        );
    }

    #[test]
    fn a_qualified_client_path_keeps_its_module_prefix() {
        let out = expand_str(
            "client = crate::api::CatalogClient",
            "async fn page(catalog: crate::api::CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert!(
            out.contains("crate :: api :: __AUTUMN_WIRE_ENDPOINTS_CatalogClient"),
            "{out}"
        );
    }

    /// Two clients whose paths differ only in their module must stay distinct.
    #[test]
    fn same_named_clients_from_different_modules_are_told_apart() {
        let func: ItemFn = syn::parse_str(
            "async fn page(a: first::Client, b: second::Client) { \
             let _ = a.get_item(&x, NoBody); let _ = b.create_item(NewItem { name }); }",
        )
        .expect("fixture parses");
        let clients: Vec<Path> = vec![
            syn::parse_str("first::Client").expect("path"),
            syn::parse_str("second::Client").expect("path"),
        ];
        let calls = Analyzer::new(&clients).run(&func);
        assert_eq!(calls.len(), 2);
        let first = calls
            .iter()
            .find(|c| c.method == "get_item")
            .expect("first");
        let second = calls
            .iter()
            .find(|c| c.method == "create_item")
            .expect("second");
        assert_ne!(first.client, second.client);
    }

    /// `assert!` hands its message to `panic!`, which reads it as a format
    /// string. A route like `/items/{id}` in the text would become an implicit
    /// capture and fail the build with "cannot find value `id`" instead of the
    /// contract error.
    #[test]
    fn an_assert_message_never_carries_an_unescaped_brace() {
        assert_eq!(
            escape_for_assert("endpoint `catalog.get_item` (GET /items/{id})"),
            "endpoint `catalog.get_item` (GET /items/{{id}})"
        );
        assert_eq!(escape_for_assert("no braces here"), "no braces here");

        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        for message in out.split('"').skip(1).step_by(2) {
            let single_open = message.replace("{{", "").contains('{');
            let single_close = message.replace("}}", "").contains('}');
            assert!(
                !single_open && !single_close,
                "an assert message must not carry a lone brace: {message}"
            );
        }
    }

    #[test]
    fn the_original_body_is_preserved() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); item.name }",
        );
        assert!(out.contains("item . name"), "{out}");
    }

    #[test]
    fn the_caller_descriptor_holds_each_calls_read_and_write_sets() {
        let func: ItemFn = syn::parse_str(
            "async fn page(catalog: CatalogClient) { \
               let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = (item.price, item.name); \
               catalog.create_item(NewItem { price: 1, name: x, ..Default::default() }).await.unwrap(); }",
        )
        .expect("fixture parses");
        let clients: Vec<Path> = vec![syn::parse_str("crate::CatalogClient").expect("client path")];
        let calls = Analyzer::new(&clients).run(&func);
        let got = caller_descriptor("storefront", "page", &calls, &clients);
        assert_eq!(got.function, "page");
        assert_eq!(got.calls.len(), 2);
        assert_eq!(got.calls[0].client, "crate::CatalogClient");
        assert_eq!(got.calls[0].method, "get_item");
        assert_eq!(got.calls[0].reads, ["name", "price"]);
        assert_eq!(got.calls[0].writes, None);
        assert_eq!(got.calls[1].method, "create_item");
        assert_eq!(
            got.calls[1].writes.as_deref(),
            Some(["name".to_owned(), "price".to_owned()].as_slice())
        );
    }

    #[test]
    fn the_expansion_tracks_the_contract_dir_variable() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let _ = catalog.get_item(&id, NoBody).await.unwrap().name; }",
        );
        assert!(out.contains("option_env"), "{out}");
        assert!(out.contains("AUTUMN_CONTRACT_DIR"), "{out}");
    }

    #[test]
    fn a_method_with_no_assertions_still_expands_to_one_impl_item() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(&self, catalog: CatalogClient) { catalog.list(self.query()).await.unwrap(); }",
        );
        let method: syn::ImplItemFn = syn::parse_str(&out).expect("one method, no free items");
        assert!(
            quote::quote!(#method).to_string().contains("option_env"),
            "{out}"
        );
    }
}

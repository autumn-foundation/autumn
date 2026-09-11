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
    Ok(quote! { #(#assertions)* #func })
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
    /// Response fields the caller names.
    reads: Vec<String>,
    /// Request fields the caller sets, or `None` when the request is not an
    /// inline struct literal.
    writes: Option<Vec<String>>,
}

/// What a name in scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    /// One of the declared clients.
    Client(usize),
    /// The response of call `n`.
    Response(usize),
    /// Anything else. Recorded rather than omitted, so a rebinding shadows the
    /// name instead of leaving the old meaning in force.
    Other,
}

/// Methods the generated client owns that are not endpoints.
const CLIENT_OWN_METHODS: [&str; 6] = ["new", "base_url", "clone", "to_owned", "eq", "fmt"];

/// Wrapper types a client is commonly injected through.
const CLIENT_WRAPPERS: [&str; 5] = ["Arc", "Rc", "Box", "State", "Extension"];

/// Macros whose tokens are never evaluated, so a field named inside one is not
/// a read.
const UNEVALUATED_MACROS: [&str; 3] = ["stringify", "quote", "matches"];

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
}

impl<'a> Analyzer<'a> {
    fn new(clients: &'a [Path]) -> Self {
        Self {
            clients,
            scopes: vec![BTreeMap::new()],
            calls: Vec::new(),
        }
    }

    /// Walk a function: its parameters bind in the body's scope.
    fn run(mut self, func: &ItemFn) -> Vec<Call> {
        for arg in &func.sig.inputs {
            if let syn::FnArg::Typed(pat) = arg {
                let bound = self
                    .client_index(&pat.ty)
                    .map_or(Bound::Other, Bound::Client);
                self.bind_pattern(&pat.pat, bound);
            }
        }
        self.visit_block(&func.block);
        for call in &mut self.calls {
            call.reads.sort();
            call.reads.dedup();
        }
        self.calls
    }

    fn lookup(&self, name: &str) -> Option<Bound> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn bind(&mut self, name: String, bound: Bound) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, bound);
        }
    }

    /// Bind every name a pattern introduces.
    ///
    /// Only a plain `x` carries the incoming meaning; a destructuring pattern
    /// binds pieces of the value, not the value, so its names bind to
    /// [`Bound::Other`] and stop attribution rather than inheriting it.
    fn bind_pattern(&mut self, pat: &syn::Pat, bound: Bound) {
        match pat {
            syn::Pat::Ident(ident) => {
                self.bind(field_name(&ident.ident), bound);
                if let Some((_, sub)) = &ident.subpat {
                    self.bind_pattern(sub, Bound::Other);
                }
            }
            syn::Pat::Type(p) => {
                let bound = self.client_index(&p.ty).map_or(bound, Bound::Client);
                self.bind_pattern(&p.pat, bound);
            }
            syn::Pat::Reference(p) => self.bind_pattern(&p.pat, bound),
            syn::Pat::Paren(p) => self.bind_pattern(&p.pat, bound),
            syn::Pat::Struct(p) => {
                for field in &p.fields {
                    self.bind_pattern(&field.pat, Bound::Other);
                }
            }
            syn::Pat::TupleStruct(p) => {
                for elem in &p.elems {
                    self.bind_pattern(elem, Bound::Other);
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
        if let Some(pair) = owner.segments.pop() {
            owner.segments.push(pair.into_value());
        }
        self.client_index(&syn::Type::Path(syn::TypePath {
            qself: None,
            path: owner,
        }))
    }

    /// What an expression evaluates to, when that is knowable.
    fn bound_of(&self, expr: &Expr) -> Option<Bound> {
        match peel(expr) {
            Expr::Path(path) => self.lookup(&field_name(path.path.get_ident()?)),
            Expr::MethodCall(call) => {
                let node = std::ptr::from_ref(call);
                self.calls
                    .iter()
                    .position(|c| c.node == node)
                    .map(Bound::Response)
            }
            _ => None,
        }
    }

    /// The call index an expression's response came from.
    fn response_of(&self, expr: &Expr) -> Option<usize> {
        match self.bound_of(expr) {
            Some(Bound::Response(index)) => Some(index),
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
            writes: request_write_set(call.args.last()),
        });
    }

    /// A `let`: the initializer is evaluated first, then the name it binds
    /// takes effect — so a rebinding cannot reach backwards.
    fn local(&mut self, local: &syn::Local) {
        let mut bound = Bound::Other;
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
            bound = self
                .bound_of(&init.expr)
                .or_else(|| self.constructor_client(&init.expr).map(Bound::Client))
                .unwrap_or(Bound::Other);
        }
        // `let Item { id, name, .. } = …` names the fields it reads.
        if let (Bound::Response(index), syn::Pat::Struct(pat)) = (bound, strip_pat_type(&local.pat))
        {
            let fields: Vec<String> = pat
                .fields
                .iter()
                .filter_map(|f| match &f.member {
                    syn::Member::Named(ident) => Some(field_name(ident)),
                    syn::Member::Unnamed(_) => None,
                })
                .collect();
            self.calls[index].reads.extend(fields);
        }
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
        let mut reads = Vec::new();
        let shadowed = macro_bound_idents(&mac.tokens);
        scan_tokens(
            &mac.tokens,
            &|name| {
                if shadowed.contains(name) {
                    return None;
                }
                match self.lookup(name) {
                    Some(Bound::Response(index)) => Some(index),
                    _ => None,
                }
            },
            &mut reads,
        );
        for (index, field) in reads {
            self.calls[index].reads.push(field);
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
        match expr {
            Expr::MethodCall(call) => {
                self.visit_expr(&call.receiver);
                for arg in &call.args {
                    self.visit_expr(arg);
                }
                self.record_call(call);
            }
            Expr::Field(field) => {
                self.visit_expr(&field.base);
                if let (Some(index), syn::Member::Named(name)) =
                    (self.response_of(&field.base), &field.member)
                {
                    self.calls[index].reads.push(field_name(name));
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
                self.bind_pattern(&let_expr.pat, Bound::Other);
            }
            Expr::Macro(mac) => self.scan_macro(&mac.mac),
            other => visit::visit_expr(self, other),
        }
    }

    fn visit_arm(&mut self, arm: &syn::Arm) {
        self.scopes.push(BTreeMap::new());
        self.bind_pattern(&arm.pat, Bound::Other);
        if let Some((_, guard)) = &arm.guard {
            self.visit_expr(guard);
        }
        self.visit_expr(&arm.body);
        self.scopes.pop();
    }

    fn visit_stmt_macro(&mut self, stmt: &syn::StmtMacro) {
        self.scan_macro(&stmt.mac);
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

/// Peel the wrappers that sit between a call and the value it produces.
fn peel(expr: &Expr) -> &Expr {
    match expr {
        Expr::Await(e) => peel(&e.base),
        Expr::Try(e) => peel(&e.expr),
        Expr::Paren(e) => peel(&e.expr),
        Expr::Group(e) => peel(&e.expr),
        Expr::Reference(e) => peel(&e.expr),
        Expr::Unary(e) if matches!(e.op, syn::UnOp::Deref(_)) => peel(&e.expr),
        // Methods that hand back the same value: `.unwrap()` on a call's
        // result is still that call's result, and `catalog.clone()` is still
        // that client.
        Expr::MethodCall(e)
            if matches!(
                e.method.to_string().as_str(),
                "unwrap" | "expect" | "clone" | "to_owned"
            ) =>
        {
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

/// Look through a `let x: T` pattern to the name it binds.
fn strip_pat_type(pat: &syn::Pat) -> &syn::Pat {
    match pat {
        syn::Pat::Type(p) => strip_pat_type(&p.pat),
        other => other,
    }
}

/// The write-set of a request argument.
///
/// `None` when the request is not an inline struct literal, because then the
/// fields the call site sets are not visible here.
fn request_write_set(arg: Option<&Expr>) -> Option<Vec<String>> {
    let Some(Expr::Struct(lit)) = arg.map(peel) else {
        return None;
    };
    Some(
        lit.fields
            .iter()
            .filter_map(|f| match &f.member {
                syn::Member::Named(ident) => Some(field_name(ident)),
                syn::Member::Unnamed(_) => None,
            })
            .collect(),
    )
}

/// Names a macro body binds for itself — `@for item in …`, `|item|`.
///
/// A macro can introduce names the surrounding scope knows nothing about, and
/// reading a field off one of those is not a read of a response that happens to
/// share the name.
fn macro_bound_idents(tokens: &proc_macro2::TokenStream) -> std::collections::BTreeSet<String> {
    let mut bound = std::collections::BTreeSet::new();
    collect_macro_bound(tokens, &mut bound);
    bound
}

fn collect_macro_bound(
    tokens: &proc_macro2::TokenStream,
    bound: &mut std::collections::BTreeSet<String>,
) {
    let tts: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    let mut in_closure_params = false;
    for (i, tt) in tts.iter().enumerate() {
        match tt {
            proc_macro2::TokenTree::Group(group) => collect_macro_bound(&group.stream(), bound),
            proc_macro2::TokenTree::Punct(punct) if punct.as_char() == '|' => {
                in_closure_params = !in_closure_params;
            }
            proc_macro2::TokenTree::Ident(ident) => {
                if in_closure_params {
                    bound.insert(ident.to_string());
                } else if ident == "for"
                    && let Some(proc_macro2::TokenTree::Ident(name)) = tts.get(i + 1)
                {
                    bound.insert(name.to_string());
                }
            }
            proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => {}
        }
    }
}

/// Record every `binding.field` in a token stream, resolving the binding
/// through `resolve`.
///
/// Token-level, because a macro body is not parsed Rust. Four shapes that look
/// the same are excluded: `binding.method(…)`, `binding.field::<T>()`,
/// `binding.mac!(…)`, and `other.binding.field` — where `binding` is itself a
/// field of something else.
fn scan_tokens(
    tokens: &proc_macro2::TokenStream,
    resolve: &dyn Fn(&str) -> Option<usize>,
    reads: &mut Vec<(usize, String)>,
) {
    let tts: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    for (i, tt) in tts.iter().enumerate() {
        if let proc_macro2::TokenTree::Group(group) = tt {
            scan_tokens(&group.stream(), resolve, reads);
            continue;
        }
        let proc_macro2::TokenTree::Ident(base) = tt else {
            continue;
        };
        if matches!(tts.get(i.wrapping_sub(1)), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '.')
        {
            continue;
        }
        let Some(index) = resolve(&base.to_string()) else {
            continue;
        };
        if !matches!(tts.get(i + 1), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '.') {
            continue;
        }
        let Some(proc_macro2::TokenTree::Ident(field)) = tts.get(i + 2) else {
            continue;
        };
        if field == "await" {
            continue;
        }
        // A call, a turbofish, or a macro — none of them a field read.
        let next = tts.get(i + 3);
        if matches!(
            next,
            Some(proc_macro2::TokenTree::Group(g))
                if g.delimiter() == proc_macro2::Delimiter::Parenthesis
        ) || matches!(next, Some(proc_macro2::TokenTree::Punct(p)) if matches!(p.as_char(), ':' | '!'))
        {
            continue;
        }
        reads.push((index, field_name(field)));
    }
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
    let site = CallSite {
        caller: caller.to_owned(),
        snippet: snippet.clone(),
        method: method.to_string(),
        reads: call.reads.clone(),
        writes: call.writes.clone(),
    };
    let named = named_violations(&site);
    let enriched = |kind: &ViolationKind, fallback: String| -> String {
        named
            .iter()
            .find(|(k, _)| k == kind)
            .map_or(fallback, |(_, message)| message.clone())
    };

    let mut out = Vec::new();
    for read in &call.reads {
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

    let Some(writes) = &call.writes else {
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
fn escape_for_assert(message: &str) -> String {
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
}

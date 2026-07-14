//! `#[lifecycle]` attribute macro implementation.
//!
//! Applied to an enum, `#[lifecycle]` turns a plain state enum into a
//! statically-verified lifecycle. Given a declared `initial` state, one or more
//! `terminal` states, and a set of `transitions`, it generates two things:
//!
//! 1. **Metadata consts + `can_transition_to`** on the enum itself, referencing
//!    the enum's own variants — so a declared `initial`/`terminal`/transition
//!    endpoint that is not a real variant is a compile error *by construction*.
//! 2. **A typestate transition module** (named after the enum in `snake_case`)
//!    with one zero-size marker type per state and a `Machine<S>` whose
//!    consuming `to_<target>` methods exist *only* for declared edges — firing
//!    an undeclared transition simply does not compile.
//!
//! ```ignore
//! use autumn_web::lifecycle;
//!
//! #[lifecycle(
//!     initial = Draft,
//!     terminal(Archived),
//!     transitions(
//!         Draft -> Published,
//!         Published -> Archived,
//!         Published -> Draft,
//!     )
//! )]
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! pub enum ArticleState { Draft, Published, Archived }
//! ```

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, ItemEnum, Token};

/// A single `From -> To` transition edge parsed from the `transitions(...)`
/// list.
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

/// Parsed `#[lifecycle(...)]` attribute arguments.
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
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `initial` — declare exactly one initial state",
                        ));
                    }
                    input.parse::<Token![=]>()?;
                    initial = Some(input.parse()?);
                }
                "terminal" => {
                    if terminals.is_some() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `terminal` — declare all terminal states in one `terminal(...)`",
                        ));
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
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `transitions` — declare all edges in one `transitions(...)`",
                        ));
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

            // Allow (and consume) a separating comma between top-level args.
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

/// Convert a `PascalCase` variant ident (raw-ident-aware) into `snake_case`.
///
/// Raw identifiers (`r#type`) have their `r#` prefix stripped before casing so
/// the derived method/module name is a plain identifier. A digit following a
/// letter starts a new word boundary as well (`Step2` -> `step2`, `V2State` ->
/// `v2_state`).
fn snake_case(ident: &Ident) -> String {
    let name = ident.to_string();
    let name = name.strip_prefix("r#").unwrap_or(&name);
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower_or_digit = false;
    for ch in name.chars() {
        if ch.is_uppercase() {
            if prev_lower_or_digit {
                out.push('_');
            }
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            prev_lower_or_digit = false;
        } else {
            out.push(ch);
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out
}

/// Core implementation invoked by the thin `#[proc_macro_attribute]` wrapper.
pub fn lifecycle_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_enum: ItemEnum = match syn::parse2(item) {
        Ok(e) => e,
        Err(_) => {
            return syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[lifecycle] may only be applied to an enum",
            )
            .to_compile_error();
        }
    };

    let args: LifecycleArgs = match syn::parse2(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    match expand(&item_enum, &args) {
        Ok(generated) => quote! {
            #item_enum
            #generated
        },
        Err(err) => {
            // Still emit the original enum so downstream references to it don't
            // cascade into a flood of "cannot find type" errors.
            let compile_err = err.to_compile_error();
            quote! {
                #item_enum
                #compile_err
            }
        }
    }
}

fn expand(item_enum: &ItemEnum, args: &LifecycleArgs) -> syn::Result<TokenStream> {
    let enum_ident = &item_enum.ident;

    // Collect the declared variant idents (as strings) for validation.
    let variants: Vec<&Ident> = item_enum.variants.iter().map(|v| &v.ident).collect();
    let is_variant = |ident: &Ident| variants.contains(&ident);

    // Validate `initial`.
    if !is_variant(&args.initial) {
        return Err(syn::Error::new_spanned(
            &args.initial,
            format!(
                "`initial` state `{}` is not a variant of enum `{}`",
                args.initial, enum_ident
            ),
        ));
    }

    // Validate `terminal`s.
    for t in &args.terminals {
        if !is_variant(t) {
            return Err(syn::Error::new_spanned(
                t,
                format!("`terminal` state `{t}` is not a variant of enum `{enum_ident}`"),
            ));
        }
    }

    // Validate transition endpoints and reject duplicate edges.
    let mut seen_edges: Vec<(String, String)> = Vec::new();
    for tr in &args.transitions {
        if !is_variant(&tr.from) {
            return Err(syn::Error::new_spanned(
                &tr.from,
                format!(
                    "transition source `{}` is not a variant of enum `{}`",
                    tr.from, enum_ident
                ),
            ));
        }
        if !is_variant(&tr.to) {
            return Err(syn::Error::new_spanned(
                &tr.to,
                format!(
                    "transition target `{}` is not a variant of enum `{}`",
                    tr.to, enum_ident
                ),
            ));
        }
        // Reject transitions *out of* a declared terminal state: a terminal
        // state must have no outgoing edges, otherwise it is not actually
        // terminal (an unsound "movable terminal").
        if args.terminals.contains(&tr.from) {
            return Err(syn::Error::new_spanned(
                &tr.from,
                format!(
                    "terminal state `{}` cannot have an outgoing transition to `{}`",
                    tr.from, tr.to
                ),
            ));
        }
        let key = (tr.from.to_string(), tr.to.to_string());
        if seen_edges.contains(&key) {
            return Err(syn::Error::new_spanned(
                &tr.to,
                format!("duplicate transition `{} -> {}`", tr.from, tr.to),
            ));
        }
        seen_edges.push(key);
    }

    let metadata = build_metadata(item_enum, args, &variants);
    let module = build_module(item_enum, args);

    Ok(quote! {
        #metadata
        #module
    })
}

/// Build the metadata `impl` block on the enum: the four consts plus
/// `can_transition_to`.
fn build_metadata(item_enum: &ItemEnum, args: &LifecycleArgs, variants: &[&Ident]) -> TokenStream {
    let enum_ident = &item_enum.ident;
    let initial = &args.initial;

    let terminal_paths = args.terminals.iter().map(|t| quote!(#enum_ident::#t));
    let state_paths = variants.iter().map(|v| quote!(#enum_ident::#v));
    let transition_pairs = args
        .transitions
        .iter()
        .map(|tr| {
            let from = &tr.from;
            let to = &tr.to;
            quote!((#enum_ident::#from, #enum_ident::#to))
        })
        .collect::<Vec<_>>();

    let match_arms = args.transitions.iter().map(|tr| {
        let from = &tr.from;
        let to = &tr.to;
        quote!((#enum_ident::#from, #enum_ident::#to) => true)
    });

    quote! {
        impl #enum_ident {
            /// The declared initial state of this lifecycle.
            pub const LIFECYCLE_INITIAL: #enum_ident = #enum_ident::#initial;
            /// The declared terminal states of this lifecycle.
            pub const LIFECYCLE_TERMINALS: &'static [#enum_ident] = &[#(#terminal_paths),*];
            /// All states of this lifecycle, in declaration order.
            pub const LIFECYCLE_STATES: &'static [#enum_ident] = &[#(#state_paths),*];
            /// All declared `(from, to)` transition edges of this lifecycle.
            pub const LIFECYCLE_TRANSITIONS: &'static [(#enum_ident, #enum_ident)] = &[
                #(#transition_pairs),*
            ];

            /// Runtime check: is `to` a declared transition from `self`?
            pub fn can_transition_to(&self, to: &#enum_ident) -> bool {
                match (self, to) {
                    #(#match_arms,)*
                    _ => false,
                }
            }
        }
    }
}

/// Build the typestate transition module named after the enum in `snake_case`.
fn build_module(item_enum: &ItemEnum, args: &LifecycleArgs) -> TokenStream {
    let enum_ident = &item_enum.ident;
    let mod_ident = format_ident!("{}", snake_case(enum_ident));
    let initial = &args.initial;

    let variants: Vec<&Ident> = item_enum.variants.iter().map(|v| &v.ident).collect();

    // One zero-size marker struct per state.
    let marker_structs = variants.iter().map(|v| {
        quote! {
            #[allow(non_snake_case)]
            pub struct #v;
        }
    });

    // Sealed + State impls for each marker.
    let state_impls = variants.iter().map(|v| {
        let name = v.to_string();
        let name = name.strip_prefix("r#").unwrap_or(&name).to_string();
        quote! {
            impl sealed::Sealed for #v {}
            impl State for #v {
                const NAME: &'static str = #name;
                const VALUE: super::#enum_ident = super::#enum_ident::#v;
            }
        }
    });

    // `start()` only on the initial state.
    let start_impl = quote! {
        impl Machine<#initial> {
            /// Begin the lifecycle at its declared initial state.
            pub fn start() -> Machine<#initial> {
                Machine { _state: ::core::marker::PhantomData }
            }
        }
    };

    // One consuming `to_<target>` method per declared edge, grouped by source
    // state so each source gets a single `impl Machine<Source>` block.
    let mut sources: Vec<&Ident> = Vec::new();
    for tr in &args.transitions {
        if !sources.contains(&&tr.from) {
            sources.push(&tr.from);
        }
    }
    let edge_impls = sources.iter().map(|source| {
        let methods = args
            .transitions
            .iter()
            .filter(|tr| &tr.from == *source)
            .map(|tr| {
                let target = &tr.to;
                let method = format_ident!("to_{}", snake_case(target));
                quote! {
                    /// Consume this machine and transition to the declared target state.
                    pub fn #method(self) -> Machine<#target> {
                        Machine { _state: ::core::marker::PhantomData }
                    }
                }
            });
        quote! {
            impl Machine<#source> {
                #(#methods)*
            }
        }
    });

    quote! {
        /// Typestate transition module for the lifecycle: firing an undeclared
        /// transition is a compile error because the method does not exist.
        pub mod #mod_ident {
            use core::marker::PhantomData;

            #(#marker_structs)*

            mod sealed {
                pub trait Sealed {}
            }

            /// A lifecycle state marker.
            pub trait State: sealed::Sealed {
                /// The variant name as a string.
                const NAME: &'static str;
                /// The corresponding enum value.
                const VALUE: super::#enum_ident;
            }

            #(#state_impls)*

            /// A lifecycle machine parameterized by its current state marker.
            pub struct Machine<S: State> {
                _state: PhantomData<S>,
            }

            #start_impl

            impl<S: State> Machine<S> {
                /// The current state as the runtime enum value.
                pub fn current(&self) -> super::#enum_ident {
                    S::VALUE
                }
            }

            #(#edge_impls)*
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn expand_ok(attr: TokenStream, item: TokenStream) -> String {
        let item_enum: ItemEnum = syn::parse2(item).expect("enum should parse");
        let args: LifecycleArgs = syn::parse2(attr).expect("args should parse");
        expand(&item_enum, &args)
            .expect("expansion should succeed")
            .to_string()
    }

    fn sample_enum() -> TokenStream {
        quote! {
            pub enum ArticleState {
                Draft,
                Published,
                Archived,
            }
        }
    }

    fn sample_attr() -> TokenStream {
        quote! {
            initial = Draft,
            terminal(Archived),
            transitions(
                Draft -> Published,
                Published -> Archived,
                Published -> Draft,
            )
        }
    }

    #[test]
    fn emits_metadata_consts_with_expected_names_and_entries() {
        let out = expand_ok(sample_attr(), sample_enum());
        assert!(out.contains("LIFECYCLE_INITIAL"), "{out}");
        assert!(out.contains("LIFECYCLE_TERMINALS"), "{out}");
        assert!(out.contains("LIFECYCLE_STATES"), "{out}");
        assert!(out.contains("LIFECYCLE_TRANSITIONS"), "{out}");
        // Initial value.
        assert!(
            out.contains("LIFECYCLE_INITIAL : ArticleState = ArticleState :: Draft"),
            "{out}"
        );
        // Terminal list references the enum variant.
        assert!(out.contains("& [ArticleState :: Archived]"), "{out}");
        // States in declaration order.
        assert!(
            out.contains(
                "& [ArticleState :: Draft , ArticleState :: Published , ArticleState :: Archived]"
            ),
            "{out}"
        );
    }

    #[test]
    fn can_transition_to_matches_declared_edges() {
        let out = expand_ok(sample_attr(), sample_enum());
        assert!(out.contains("fn can_transition_to"), "{out}");
        // Declared edge arm present.
        assert!(
            out.contains("(ArticleState :: Draft , ArticleState :: Published) => true"),
            "{out}"
        );
        // Fallthrough for undeclared edges.
        assert!(out.contains("_ => false"), "{out}");
    }

    #[test]
    fn transition_method_exists_on_source_state() {
        let out = expand_ok(sample_attr(), sample_enum());
        // Module named after enum in snake_case.
        assert!(out.contains("pub mod article_state"), "{out}");
        // `to_published` on the Draft source.
        assert!(out.contains("fn to_published"), "{out}");
        assert!(out.contains("fn to_archived"), "{out}");
        assert!(out.contains("fn to_draft"), "{out}");
    }

    #[test]
    fn terminal_state_has_no_outgoing_methods() {
        let out = expand_ok(sample_attr(), sample_enum());
        // No method transitions *into* an impl on Archived: Archived is a
        // source of no edges, so no `impl Machine < Archived >` block exists.
        assert!(
            !out.contains("impl Machine < Archived >"),
            "Archived is terminal and must have no outgoing method impl:\n{out}"
        );
    }

    #[test]
    fn start_only_on_initial_state() {
        let out = expand_ok(sample_attr(), sample_enum());
        // `start` lives inside `impl Machine < Draft >`.
        assert!(out.contains("impl Machine < Draft >"), "{out}");
        assert!(out.contains("fn start"), "{out}");
        // Ensure there is exactly one `fn start`.
        assert_eq!(out.matches("fn start").count(), 1, "{out}");
    }

    #[test]
    fn undeclared_initial_is_error() {
        let item_enum: ItemEnum = syn::parse2(sample_enum()).unwrap();
        let args: LifecycleArgs = syn::parse2(quote! {
            initial = Nonexistent,
            terminal(Archived),
            transitions(Draft -> Published)
        })
        .unwrap();
        let err = expand(&item_enum, &args).unwrap_err();
        assert!(
            err.to_string().contains("Nonexistent"),
            "error should name the offending ident: {err}"
        );
    }

    #[test]
    fn undeclared_terminal_is_error() {
        let item_enum: ItemEnum = syn::parse2(sample_enum()).unwrap();
        let args: LifecycleArgs = syn::parse2(quote! {
            initial = Draft,
            terminal(Ghost),
            transitions(Draft -> Published)
        })
        .unwrap();
        let err = expand(&item_enum, &args).unwrap_err();
        assert!(err.to_string().contains("Ghost"), "{err}");
    }

    #[test]
    fn undeclared_transition_endpoint_is_error() {
        let item_enum: ItemEnum = syn::parse2(sample_enum()).unwrap();
        let args: LifecycleArgs = syn::parse2(quote! {
            initial = Draft,
            terminal(Archived),
            transitions(Draft -> Nowhere)
        })
        .unwrap();
        let err = expand(&item_enum, &args).unwrap_err();
        assert!(err.to_string().contains("Nowhere"), "{err}");
    }

    #[test]
    fn terminal_source_transition_is_error() {
        let item_enum: ItemEnum = syn::parse2(sample_enum()).unwrap();
        let args: LifecycleArgs = syn::parse2(quote! {
            initial = Draft,
            terminal(Archived),
            transitions(
                Draft -> Archived,
                Archived -> Draft,
            )
        })
        .unwrap();
        let err = expand(&item_enum, &args).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("terminal state `Archived`"),
            "error should name the offending terminal: {msg}"
        );
        assert!(
            msg.contains("Draft"),
            "error should name the transition target: {msg}"
        );
    }

    #[test]
    fn duplicate_transition_edge_is_error() {
        let item_enum: ItemEnum = syn::parse2(sample_enum()).unwrap();
        let args: LifecycleArgs = syn::parse2(quote! {
            initial = Draft,
            terminal(Archived),
            transitions(
                Draft -> Published,
                Draft -> Published,
            )
        })
        .unwrap();
        let err = expand(&item_enum, &args).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    fn parse_args_err(attr: TokenStream) -> String {
        let res: syn::Result<LifecycleArgs> = syn::parse2(attr);
        match res {
            Ok(_) => panic!("expected a parse error"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn missing_initial_is_error() {
        let err = parse_args_err(quote! {
            terminal(Archived),
            transitions(Draft -> Published)
        });
        assert!(err.contains("initial"), "{err}");
    }

    #[test]
    fn missing_terminal_is_error() {
        let err = parse_args_err(quote! {
            initial = Draft,
            transitions(Draft -> Published)
        });
        assert!(err.contains("terminal"), "{err}");
    }

    #[test]
    fn missing_transitions_is_error() {
        let err = parse_args_err(quote! {
            initial = Draft,
            terminal(Archived)
        });
        assert!(err.contains("transitions"), "{err}");
    }

    #[test]
    fn snake_case_naming_for_multi_word_variants() {
        let item = quote! {
            pub enum ReviewState {
                Draft,
                InReview,
                Approved,
            }
        };
        let attr = quote! {
            initial = Draft,
            terminal(Approved),
            transitions(
                Draft -> InReview,
                InReview -> Approved,
            )
        };
        let out = expand_ok(attr, item);
        // Module: ReviewState -> review_state.
        assert!(out.contains("pub mod review_state"), "{out}");
        // Method: InReview -> to_in_review.
        assert!(out.contains("fn to_in_review"), "{out}");
    }

    #[test]
    fn snake_case_helper_cases() {
        assert_eq!(snake_case(&format_ident!("Draft")), "draft");
        assert_eq!(snake_case(&format_ident!("InReview")), "in_review");
        assert_eq!(snake_case(&format_ident!("ArticleState")), "article_state");
        // A word boundary is only inserted before an uppercase that follows a
        // lowercase or digit, so runs of capitals stay together.
        assert_eq!(snake_case(&format_ident!("HTTPState")), "httpstate");
        assert_eq!(snake_case(&format_ident!("Step2Review")), "step2_review");
    }
}

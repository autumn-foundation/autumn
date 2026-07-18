//! `#[listener]` proc macro implementation.
//!
//! Declares an event listener: an async function reacting to a typed `#[event]`.
//! Mirrors `#[job]` — it emits a `__autumn_listener_info_{fn}()` companion that
//! `listeners![]` collects into a `Vec<ListenerInfo>` for `AppBuilder::listeners`.
//!
//! A listener runs **synchronously** (in-request, before the response) by
//! default, or **durably** (`durable`) — enqueued onto the existing `#[job]`
//! queue so it survives restarts and inherits retry + DLQ semantics.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream, Parser as _};
use syn::{FnArg, ItemFn, LitInt, PatType, Token, Type};

struct ListenerAttrs {
    event_type: Type,
    durable: bool,
    max_attempts: Option<u32>,
    backoff_ms: Option<u64>,
}

impl Parse for ListenerAttrs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let event_type: Type = input.parse().map_err(|_| {
            input.error("#[listener] requires an event type, e.g. #[listener(UserSignedUp)]")
        })?;

        let mut durable = false;
        let mut max_attempts = None;
        let mut backoff_ms = None;

        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let ident: syn::Ident = input.parse()?;
            if ident == "durable" {
                durable = true;
            } else if ident == "max_attempts" {
                input.parse::<Token![=]>()?;
                let value: LitInt = input.parse()?;
                max_attempts = Some(value.base10_parse::<u32>()?);
            } else if ident == "backoff_ms" {
                input.parse::<Token![=]>()?;
                let value: LitInt = input.parse()?;
                backoff_ms = Some(value.base10_parse::<u64>()?);
            } else {
                return Err(syn::Error::new(
                    ident.span(),
                    "unsupported attribute: expected `durable`, `max_attempts`, or `backoff_ms`",
                ));
            }
        }

        if !durable && (max_attempts.is_some() || backoff_ms.is_some()) {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`max_attempts`/`backoff_ms` only apply to `durable` listeners",
            ));
        }

        Ok(Self {
            event_type,
            durable,
            max_attempts,
            backoff_ms,
        })
    }
}

pub fn listener_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match ListenerAttrs::parse.parse2(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "#[listener] functions must be async",
        )
        .to_compile_error();
    }

    if input_fn.sig.inputs.len() != 2 {
        return syn::Error::new_spanned(
            &input_fn.sig.ident,
            "#[listener] function must have signature async fn(AppState, Event)",
        )
        .to_compile_error();
    }

    // Validate the second argument is a typed event struct (mirrors #[job]).
    let mut inputs = input_fn.sig.inputs.iter();
    let _state_arg = inputs.next();
    if !matches!(inputs.next(), Some(FnArg::Typed(PatType { .. }))) {
        return syn::Error::new_spanned(
            &input_fn.sig.ident,
            "#[listener] second argument must be a typed event struct",
        )
        .to_compile_error();
    }

    let event_type = &attrs.event_type;
    let fn_name = &input_fn.sig.ident;
    let companion_name = format_ident!("__autumn_listener_info_{fn_name}");
    let fn_name_str = fn_name.to_string();
    let max_attempts = attrs.max_attempts.unwrap_or(0);
    let backoff_ms = attrs.backoff_ms.unwrap_or(0);

    let (mode, job_name_expr) = if attrs.durable {
        (
            quote! { ::autumn_web::events::DispatchMode::Durable },
            quote! { ::std::option::Option::Some(::std::format!("__event_listener::{listener_name}")) },
        )
    } else {
        (
            quote! { ::autumn_web::events::DispatchMode::Sync },
            quote! { ::std::option::Option::<::std::string::String>::None },
        )
    };

    quote! {
        #input_fn

        #[doc(hidden)]
        pub fn #companion_name() -> ::autumn_web::events::ListenerInfo {
            // Fully-qualified, per-listener identity. `module_path!` expands at
            // the listener's definition site, so two listeners can never collide.
            let listener_name = ::std::format!("{}::{}", ::std::module_path!(), #fn_name_str);
            // Compute `job_name` before moving `listener_name` into the struct.
            let job_name = #job_name_expr;
            ::autumn_web::events::ListenerInfo {
                event_name: <#event_type as ::autumn_web::events::Event>::NAME,
                listener_name,
                mode: #mode,
                job_name,
                max_attempts: #max_attempts,
                initial_backoff_ms: #backoff_ms,
                handler: |state: ::autumn_web::AppState, payload: ::autumn_web::reexports::serde_json::Value| {
                    ::std::boxed::Box::pin(async move {
                        let event: #event_type = ::autumn_web::reexports::serde_json::from_value(payload)
                            .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("event deserialization failed: {e}"))))?;
                        #fn_name(state, event).await
                    })
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn expand(attr: TokenStream, item: TokenStream) -> String {
        listener_macro(attr, item).to_string()
    }

    #[test]
    fn sync_listener_emits_companion_with_sync_mode() {
        let expanded = expand(
            quote! { UserSignedUp },
            quote! {
                async fn send_welcome(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
                    Ok(())
                }
            },
        );
        assert!(
            expanded.contains("pub fn __autumn_listener_info_send_welcome"),
            "{expanded}"
        );
        assert!(expanded.contains("DispatchMode :: Sync"), "{expanded}");
        assert!(
            expanded.contains("string :: String > :: None"),
            "{expanded}"
        );
        assert!(
            expanded.contains("< UserSignedUp as :: autumn_web :: events :: Event > :: NAME"),
            "{expanded}"
        );
    }

    #[test]
    fn durable_listener_sets_job_name_and_durable_mode() {
        let expanded = expand(
            quote! { UserSignedUp, durable, max_attempts = 5, backoff_ms = 1000 },
            quote! {
                async fn seed_workspace(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
                    Ok(())
                }
            },
        );
        assert!(expanded.contains("DispatchMode :: Durable"), "{expanded}");
        assert!(expanded.contains("__event_listener::"), "{expanded}");
        assert!(expanded.contains("max_attempts : 5u32"), "{expanded}");
        assert!(
            expanded.contains("initial_backoff_ms : 1000u64"),
            "{expanded}"
        );
    }

    #[test]
    fn rejects_non_async() {
        let expanded = expand(
            quote! { UserSignedUp },
            quote! {
                fn handle(state: AppState, event: UserSignedUp) -> AutumnResult<()> { Ok(()) }
            },
        );
        assert!(expanded.contains("must be async"), "{expanded}");
    }

    #[test]
    fn rejects_wrong_arity() {
        let expanded = expand(
            quote! { UserSignedUp },
            quote! {
                async fn handle(state: AppState) -> AutumnResult<()> { Ok(()) }
            },
        );
        assert!(expanded.contains("async fn(AppState, Event)"), "{expanded}");
    }

    #[test]
    fn rejects_retry_attrs_on_sync_listener() {
        let expanded = expand(
            quote! { UserSignedUp, max_attempts = 3 },
            quote! {
                async fn handle(state: AppState, event: UserSignedUp) -> AutumnResult<()> { Ok(()) }
            },
        );
        assert!(expanded.contains("only apply to `durable`"), "{expanded}");
    }
}

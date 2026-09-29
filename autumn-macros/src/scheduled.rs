//! `#[scheduled]` proc macro implementation.
//!
//! Generates a companion `__autumn_task_info_{name}()` function that returns
//! a `TaskInfo` struct for the scheduler.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{ItemFn, LitStr};

struct ScheduledAttrs {
    every: Option<String>,
    cron: Option<String>,
    name: Option<String>,
    tz: Option<String>,
    coordination: Option<String>,
}

fn parse_scheduled_args(attr: TokenStream) -> syn::Result<ScheduledAttrs> {
    let mut result = ScheduledAttrs {
        every: None,
        cron: None,
        name: None,
        tz: None,
        coordination: None,
    };

    syn::meta::parser(|meta| {
        if meta.path.is_ident("every") {
            let value: LitStr = meta.value()?.parse()?;
            result.every = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("cron") {
            let value: LitStr = meta.value()?.parse()?;
            result.cron = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("name") {
            let value: LitStr = meta.value()?.parse()?;
            result.name = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("tz") {
            let value: LitStr = meta.value()?.parse()?;
            result.tz = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("coordination") {
            let value: LitStr = meta.value()?.parse()?;
            result.coordination = Some(value.value());
            Ok(())
        } else {
            Err(meta
                .error("unsupported attribute: expected every, cron, name, tz, or coordination"))
        }
    })
    .parse2(attr)?;

    if result.every.is_none() && result.cron.is_none() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[scheduled] requires either `every` or `cron` parameter",
        ));
    }

    if result.every.is_some() && result.cron.is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[scheduled] cannot have both `every` and `cron`",
        ));
    }

    Ok(result)
}

pub fn scheduled_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_scheduled_args(attr) {
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
            "#[scheduled] functions must be async",
        )
        .to_compile_error();
    }

    let fn_name = &input_fn.sig.ident;
    let task_name = attrs.name.unwrap_or_else(|| fn_name.to_string());
    let companion_name = format_ident!("__autumn_task_info_{fn_name}");

    // Build the schedule expression
    let schedule_expr = if let Some(every) = &attrs.every {
        // Validate the duration at compile time
        let every_str = every.clone();
        quote! {
            ::autumn_web::task::Schedule::FixedDelay(
                ::autumn_web::task::parse_duration(#every_str)
                    .expect(concat!("invalid duration in #[scheduled(every = \"", #every_str, "\")]"))
            )
        }
    } else if let Some(cron) = &attrs.cron {
        let tz = attrs.tz.as_deref();
        let tz_expr = tz.map_or_else(|| quote! { None }, |tz| quote! { Some(#tz.to_string()) });
        quote! {
            ::autumn_web::task::Schedule::Cron {
                expression: #cron.to_string(),
                timezone: #tz_expr,
            }
        }
    } else {
        unreachable!()
    };

    let task_name_str = task_name;
    let coordination_expr = match attrs.coordination.as_deref() {
        None | Some("fleet") => quote! { ::autumn_web::task::TaskCoordination::Fleet },
        Some("per_replica") => quote! { ::autumn_web::task::TaskCoordination::PerReplica },
        Some(other) => {
            return syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "invalid #[scheduled(coordination = {other:?})]; expected \"fleet\" or \"per_replica\""
                ),
            )
            .to_compile_error();
        }
    };

    // ── Architecture-graph node (#1747) ─────────────────────────
    // The schedule is carried as the literal the author wrote (`every = "15m"`
    // or the cron expression), not the parsed `Schedule`: the graph reports the
    // declaration, and a parsed value would need the runtime type to render.
    let graph_schedule = attrs
        .every
        .clone()
        .or_else(|| attrs.cron.clone())
        .unwrap_or_default();
    let graph_descriptor = crate::graph::emit_job_descriptor(
        &input_fn,
        &quote! { #task_name_str },
        "Scheduled",
        &quote! { #graph_schedule },
    );

    quote! {
        #input_fn

        #graph_descriptor

        #[doc(hidden)]
        pub fn #companion_name() -> ::autumn_web::task::TaskInfo {
            ::autumn_web::task::TaskInfo {
                name: #task_name_str.to_string(),
                schedule: #schedule_expr,
                coordination: #coordination_expr,
                handler: |state: ::autumn_web::AppState| {
                    Box::pin(async move {
                        #fn_name(state).await
                    })
                },
            }
        }
    }
}

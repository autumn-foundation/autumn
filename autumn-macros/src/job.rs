//! `#[job]` proc macro implementation.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{FnArg, ItemFn, LitBool, LitInt, LitStr, PatType, Type};

struct JobAttrs {
    name: Option<String>,
    max_attempts: Option<u32>,
    backoff_ms: Option<u64>,
    /// Named queue this job is routed to; `None` defaults to `"default"`.
    queue: Option<String>,
    /// `None` = unconfigured; `Some(false)` = explicit opt-out, which is an
    /// error when combined with other uniqueness attributes.
    unique: Option<bool>,
    unique_by: Option<Vec<String>>,
    unique_window: Option<String>,
    unique_for_ms: Option<u64>,
    concurrency: Option<u32>,
    concurrency_key: Option<String>,
}

fn parse_basic_arg(
    meta: &syn::meta::ParseNestedMeta<'_>,
    result: &mut JobAttrs,
) -> syn::Result<bool> {
    if meta.path.is_ident("name") {
        let value: LitStr = meta.value()?.parse()?;
        result.name = Some(value.value());
    } else if meta.path.is_ident("max_attempts") {
        let value: LitInt = meta.value()?.parse()?;
        result.max_attempts = Some(value.base10_parse::<u32>()?);
    } else if meta.path.is_ident("backoff_ms") {
        let value: LitInt = meta.value()?.parse()?;
        result.backoff_ms = Some(value.base10_parse::<u64>()?);
    } else if meta.path.is_ident("queue") {
        let value: LitStr = meta.value()?.parse()?;
        let queue = value.value().trim().to_string();
        if queue.is_empty()
            || !queue
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(meta.error(
                "queue name must be non-empty and contain only lowercase letters, digits, \
                 underscores, or hyphens (e.g. \"critical\", \"email-low\")",
            ));
        }
        result.queue = Some(queue);
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn parse_uniqueness_arg(
    meta: &syn::meta::ParseNestedMeta<'_>,
    result: &mut JobAttrs,
) -> syn::Result<bool> {
    if meta.path.is_ident("unique") {
        if meta.input.peek(syn::Token![=]) {
            let value: LitBool = meta.value()?.parse()?;
            result.unique = Some(value.value());
        } else {
            result.unique = Some(true);
        }
    } else if meta.path.is_ident("unique_by") {
        let value: LitStr = meta.value()?.parse()?;
        // Sort and deduplicate so the derived key is canonical regardless of
        // declaration order — two app versions listing the same fields in a
        // different order still coalesce each other's jobs.
        let mut fields: Vec<String> = value
            .value()
            .split(',')
            .map(|field| field.trim().to_string())
            .filter(|field| !field.is_empty())
            .collect();
        fields.sort();
        fields.dedup();
        if fields.is_empty() {
            return Err(
                meta.error("unique_by must list at least one args field, e.g. \"account_id\"")
            );
        }
        result.unique_by = Some(fields);
    } else if meta.path.is_ident("unique_window") {
        let value: LitStr = meta.value()?.parse()?;
        let window = value.value();
        if window != "pending" && window != "running" {
            return Err(meta.error("unique_window must be \"pending\" or \"running\""));
        }
        result.unique_window = Some(window);
    } else if meta.path.is_ident("unique_for_ms") {
        let value: LitInt = meta.value()?.parse()?;
        let ms = value.base10_parse::<u64>()?;
        if ms == 0 {
            return Err(meta.error("unique_for_ms must be greater than zero"));
        }
        result.unique_for_ms = Some(ms);
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn parse_concurrency_arg(
    meta: &syn::meta::ParseNestedMeta<'_>,
    result: &mut JobAttrs,
) -> syn::Result<bool> {
    if meta.path.is_ident("concurrency") {
        let value: LitInt = meta.value()?.parse()?;
        let limit = value.base10_parse::<u32>()?;
        if limit == 0 {
            return Err(meta.error("concurrency must be greater than zero"));
        }
        result.concurrency = Some(limit);
    } else if meta.path.is_ident("concurrency_key") {
        let value: LitStr = meta.value()?.parse()?;
        let key = value.value().trim().to_string();
        if key.is_empty() {
            return Err(meta.error("concurrency_key must name an args field"));
        }
        result.concurrency_key = Some(key);
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn validate_job_attrs(result: &mut JobAttrs) -> syn::Result<()> {
    if result.unique_window.is_some() && result.unique_for_ms.is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "unique_window and unique_for_ms are mutually exclusive",
        ));
    }
    if result.concurrency_key.is_some() && result.concurrency.is_none() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "concurrency_key requires concurrency = N",
        ));
    }
    let uniqueness_configured = result.unique_by.is_some()
        || result.unique_window.is_some()
        || result.unique_for_ms.is_some();
    if uniqueness_configured {
        if result.unique == Some(false) {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "explicit `unique = false` is incompatible with other uniqueness attributes \
                 (`unique_by`, `unique_window`, `unique_for_ms`)",
            ));
        }
        result.unique = Some(true);
    }
    Ok(())
}

fn parse_job_args(attr: TokenStream) -> syn::Result<JobAttrs> {
    let mut result = JobAttrs {
        name: None,
        max_attempts: None,
        backoff_ms: None,
        queue: None,
        unique: None,
        unique_by: None,
        unique_window: None,
        unique_for_ms: None,
        concurrency: None,
        concurrency_key: None,
    };

    syn::meta::parser(|meta| {
        if parse_basic_arg(&meta, &mut result)?
            || parse_uniqueness_arg(&meta, &mut result)?
            || parse_concurrency_arg(&meta, &mut result)?
        {
            Ok(())
        } else {
            Err(meta.error(
                "unsupported attribute: expected name, max_attempts, backoff_ms, queue, unique, \
                 unique_by, unique_window, unique_for_ms, concurrency, or concurrency_key",
            ))
        }
    })
    .parse2(attr)?;

    validate_job_attrs(&mut result)?;
    Ok(result)
}

fn pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let mut chars = seg.chars();
            chars.next().map_or_else(String::new, |c| {
                format!("{}{}", c.to_ascii_uppercase(), chars.as_str())
            })
        })
        .collect::<String>()
}

fn uniqueness_tokens(attrs: &JobAttrs) -> TokenStream {
    if !attrs.unique.unwrap_or(false) {
        return quote! { ::std::option::Option::None };
    }
    let by = attrs.unique_by.clone().unwrap_or_default();
    let window = attrs.unique_for_ms.map_or_else(
        || {
            if attrs.unique_window.as_deref() == Some("pending") {
                quote! { ::autumn_web::job::JobUniquenessWindow::Pending }
            } else {
                quote! { ::autumn_web::job::JobUniquenessWindow::Running }
            }
        },
        |ms| quote! { ::autumn_web::job::JobUniquenessWindow::TtlMs(#ms) },
    );
    quote! {
        ::std::option::Option::Some(::autumn_web::job::JobUniqueness {
            by: ::std::vec![#(#by.to_string()),*],
            window: #window,
        })
    }
}

fn concurrency_tokens(attrs: &JobAttrs) -> TokenStream {
    attrs.concurrency.map_or_else(
        || quote! { ::std::option::Option::None },
        |limit| {
            let key = attrs.concurrency_key.as_ref().map_or_else(
                || quote! { ::std::option::Option::None },
                |key| quote! { ::std::option::Option::Some(#key.to_string()) },
            );
            quote! {
                ::std::option::Option::Some(::autumn_web::job::JobConcurrency {
                    limit: #limit,
                    key: #key,
                })
            }
        },
    )
}

pub fn job_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_job_args(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "#[job] functions must be async")
            .to_compile_error();
    }

    if input_fn.sig.inputs.len() != 2 && input_fn.sig.inputs.len() != 3 {
        return syn::Error::new_spanned(
            &input_fn.sig.ident,
            "#[job] function must have signature async fn(AppState, Args) or \
             async fn(AppState, Args, JobContext)",
        )
        .to_compile_error();
    }
    let takes_context = input_fn.sig.inputs.len() == 3;

    let mut inputs = input_fn.sig.inputs.iter();
    let _state_arg = inputs.next();
    let args_type: Type = match inputs.next() {
        Some(FnArg::Typed(PatType { ty, .. })) => (**ty).clone(),
        _ => {
            return syn::Error::new_spanned(
                &input_fn.sig.ident,
                "#[job] second argument must be a typed args struct",
            )
            .to_compile_error();
        }
    };
    if takes_context && !matches!(inputs.next(), Some(FnArg::Typed(_))) {
        return syn::Error::new_spanned(
            &input_fn.sig.ident,
            "#[job] third argument must be a typed JobContext",
        )
        .to_compile_error();
    }

    let fn_name = &input_fn.sig.ident;
    let companion_name = format_ident!("__autumn_job_info_{fn_name}");
    let api_name = format_ident!("{}Job", pascal_case(&fn_name.to_string()));
    let job_name = attrs.name.clone().unwrap_or_else(|| fn_name.to_string());
    let max_attempts = attrs.max_attempts.unwrap_or(0);
    let backoff_ms = attrs.backoff_ms.unwrap_or(0);
    let queue = attrs.queue.clone().unwrap_or_else(|| "default".to_string());
    let uniqueness = uniqueness_tokens(&attrs);
    let concurrency = concurrency_tokens(&attrs);

    let handler_call = if takes_context {
        quote! { #fn_name(state, args, ::autumn_web::job::JobContext::current()).await }
    } else {
        quote! { #fn_name(state, args).await }
    };

    quote! {
        #input_fn

        pub struct #api_name;

        impl #api_name {
            /// The registered name of this job, as passed to `AppBuilder::jobs`.
            pub const NAME: &'static str = #job_name;

            pub async fn enqueue(args: #args_type) -> ::autumn_web::AutumnResult<()> {
                let payload = ::autumn_web::reexports::serde_json::to_value(&args)
                    .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("job args serialization failed: {e}"))))?;
                ::autumn_web::job::enqueue(#job_name, payload).await
            }

            /// Enqueue this job to run once after `delay` elapses.
            pub async fn enqueue_in(args: #args_type, delay: ::core::time::Duration) -> ::autumn_web::AutumnResult<()> {
                let payload = ::autumn_web::reexports::serde_json::to_value(&args)
                    .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("job args serialization failed: {e}"))))?;
                ::autumn_web::job::enqueue_in(#job_name, payload, delay).await
            }

            /// Enqueue this job to run once at the absolute instant `when`.
            pub async fn enqueue_at(
                args: #args_type,
                when: ::autumn_web::reexports::chrono::DateTime<::autumn_web::reexports::chrono::Utc>,
            ) -> ::autumn_web::AutumnResult<()> {
                let payload = ::autumn_web::reexports::serde_json::to_value(&args)
                    .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("job args serialization failed: {e}"))))?;
                ::autumn_web::job::enqueue_at(#job_name, payload, when).await
            }

            /// Enqueue this job and return a handle carrying a public,
            /// unguessable token that grants anonymous (capability-only)
            /// access to its tracked status via the built-in status route.
            pub async fn enqueue_tracked(args: #args_type) -> ::autumn_web::AutumnResult<::autumn_web::job::TrackedJobHandle> {
                let payload = ::autumn_web::reexports::serde_json::to_value(&args)
                    .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("job args serialization failed: {e}"))))?;
                ::autumn_web::job::enqueue_tracked(#job_name, payload).await
            }

            /// Like [`Self::enqueue_tracked`], binding the tracked status to
            /// `owner` so only a request matching that session/user may poll
            /// it.
            pub async fn enqueue_tracked_for(
                args: #args_type,
                owner: ::autumn_web::job::TrackedJobOwner,
            ) -> ::autumn_web::AutumnResult<::autumn_web::job::TrackedJobHandle> {
                let payload = ::autumn_web::reexports::serde_json::to_value(&args)
                    .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("job args serialization failed: {e}"))))?;
                ::autumn_web::job::enqueue_tracked_for(#job_name, payload, owner).await
            }
        }

        #[doc(hidden)]
        pub fn #companion_name() -> ::autumn_web::job::JobInfo {
            ::autumn_web::job::JobInfo {
                name: #job_name.to_string(),
                max_attempts: #max_attempts,
                initial_backoff_ms: #backoff_ms,
                queue: #queue.to_string(),
                uniqueness: #uniqueness,
                concurrency: #concurrency,
                handler: |state: ::autumn_web::AppState, payload: ::autumn_web::reexports::serde_json::Value| {
                    Box::pin(async move {
                        let args: #args_type = ::autumn_web::reexports::serde_json::from_value(payload)
                            .map_err(|e| ::autumn_web::AutumnError::internal_server_error(::std::io::Error::other(format!("job args deserialization failed: {e}"))))?;
                        #handler_call
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

    fn parse(tokens: TokenStream) -> syn::Result<JobAttrs> {
        parse_job_args(tokens)
    }

    #[test]
    fn parses_existing_args_without_uniqueness_or_concurrency() {
        let attrs = parse(quote! { name = "send_email", max_attempts = 3, backoff_ms = 10 })
            .expect("parse");
        assert_eq!(attrs.name.as_deref(), Some("send_email"));
        assert_eq!(attrs.max_attempts, Some(3));
        assert_eq!(attrs.backoff_ms, Some(10));
        assert_eq!(attrs.unique, None);
        assert!(attrs.concurrency.is_none());
    }

    #[test]
    fn parses_bare_unique_flag() {
        let attrs = parse(quote! { unique }).expect("parse");
        assert_eq!(attrs.unique, Some(true));
        assert!(attrs.unique_by.is_none());
        assert!(attrs.unique_for_ms.is_none());
    }

    #[test]
    fn parses_unique_with_explicit_bool() {
        let attrs = parse(quote! { unique = true }).expect("parse");
        assert_eq!(attrs.unique, Some(true));
        let attrs = parse(quote! { unique = false }).expect("parse");
        assert_eq!(attrs.unique, Some(false));
    }

    #[test]
    fn unique_by_lists_trimmed_fields_and_implies_unique() {
        let attrs = parse(quote! { unique_by = "account_id, region" }).expect("parse");
        assert_eq!(attrs.unique, Some(true));
        assert_eq!(
            attrs.unique_by.as_deref(),
            Some(&["account_id".to_string(), "region".to_string()][..])
        );
    }

    #[test]
    fn unique_by_fields_are_sorted_and_deduplicated() {
        let attrs = parse(quote! { unique_by = "region, account_id, region" }).expect("parse");
        assert_eq!(
            attrs.unique_by.as_deref(),
            Some(&["account_id".to_string(), "region".to_string()][..])
        );
    }

    #[test]
    fn explicit_unique_false_conflicts_with_other_uniqueness_attributes() {
        assert!(parse(quote! { unique = false, unique_by = "account_id" }).is_err());
        assert!(parse(quote! { unique = false, unique_window = "pending" }).is_err());
        assert!(parse(quote! { unique = false, unique_for_ms = 1000 }).is_err());
        // A lone opt-out stays a no-op.
        let attrs = parse(quote! { unique = false }).expect("parse");
        assert_eq!(attrs.unique, Some(false));
    }

    #[test]
    fn unique_window_accepts_pending_and_running_only() {
        assert!(parse(quote! { unique, unique_window = "pending" }).is_ok());
        assert!(parse(quote! { unique, unique_window = "running" }).is_ok());
        assert!(parse(quote! { unique, unique_window = "forever" }).is_err());
    }

    #[test]
    fn unique_for_ms_implies_unique_and_rejects_zero() {
        let attrs = parse(quote! { unique_for_ms = 60000 }).expect("parse");
        assert_eq!(attrs.unique, Some(true));
        assert_eq!(attrs.unique_for_ms, Some(60_000));
        assert!(parse(quote! { unique_for_ms = 0 }).is_err());
    }

    #[test]
    fn unique_window_and_unique_for_ms_are_mutually_exclusive() {
        assert!(parse(quote! { unique_window = "pending", unique_for_ms = 1000 }).is_err());
    }

    #[test]
    fn concurrency_rejects_zero_limit() {
        assert!(parse(quote! { concurrency = 0 }).is_err());
        let attrs = parse(quote! { concurrency = 2 }).expect("parse");
        assert_eq!(attrs.concurrency, Some(2));
    }

    #[test]
    fn concurrency_key_requires_concurrency_limit() {
        assert!(parse(quote! { concurrency_key = "account_id" }).is_err());
        let attrs =
            parse(quote! { concurrency = 1, concurrency_key = "account_id" }).expect("parse");
        assert_eq!(attrs.concurrency_key.as_deref(), Some("account_id"));
    }

    #[test]
    fn unknown_attribute_is_rejected() {
        assert!(parse(quote! { uniqueness = true }).is_err());
    }

    #[test]
    fn parses_queue_arg() {
        let attrs = parse(quote! { name = "send_email", queue = "critical" }).expect("parse");
        assert_eq!(attrs.queue.as_deref(), Some("critical"));
    }

    #[test]
    fn queue_defaults_to_none_when_unset() {
        let attrs = parse(quote! { name = "send_email" }).expect("parse");
        assert!(attrs.queue.is_none());
    }

    #[test]
    fn rejects_empty_queue() {
        assert!(parse(quote! { queue = "" }).is_err());
        assert!(parse(quote! { queue = "   " }).is_err());
    }

    #[test]
    fn expansion_carries_queue_into_job_info() {
        let expanded = job_macro(
            quote! { queue = "critical" },
            quote! {
                async fn reset_password(state: AppState, args: ResetArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(expanded.contains("queue : \"critical\""), "{expanded}");
    }

    #[test]
    fn expansion_defaults_queue_to_default() {
        let expanded = job_macro(
            quote! {},
            quote! {
                async fn plain(state: AppState, args: PlainArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(expanded.contains("queue : \"default\""), "{expanded}");
    }

    #[test]
    fn expansion_carries_uniqueness_and_concurrency_into_job_info() {
        let expanded = job_macro(
            quote! { unique_by = "account_id", concurrency = 2, concurrency_key = "account_id" },
            quote! {
                async fn sync_account(state: AppState, args: SyncArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(expanded.contains("JobUniqueness"), "{expanded}");
        assert!(
            expanded.contains("JobUniquenessWindow :: Running"),
            "{expanded}"
        );
        assert!(expanded.contains("JobConcurrency"), "{expanded}");
        assert!(expanded.contains("limit : 2u32"), "{expanded}");
    }

    #[test]
    fn expansion_generates_delayed_enqueue_helpers() {
        let expanded = job_macro(
            quote! { name = "reminder" },
            quote! {
                async fn reminder(state: AppState, args: ReminderArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        // The per-job helper struct gains enqueue_in / enqueue_at alongside enqueue.
        assert!(
            expanded.contains("async fn enqueue_in"),
            "missing enqueue_in: {expanded}"
        );
        assert!(
            expanded.contains("async fn enqueue_at"),
            "missing enqueue_at: {expanded}"
        );
        // They delegate to the module-level free functions with the job name.
        assert!(
            expanded.contains(":: autumn_web :: job :: enqueue_in"),
            "{expanded}"
        );
        assert!(
            expanded.contains(":: autumn_web :: job :: enqueue_at"),
            "{expanded}"
        );
        // enqueue_at takes a chrono UTC instant.
        assert!(
            expanded.contains("chrono :: DateTime"),
            "enqueue_at should take a DateTime: {expanded}"
        );
    }

    #[test]
    fn expansion_defaults_to_no_uniqueness_or_concurrency() {
        let expanded = job_macro(
            quote! {},
            quote! {
                async fn plain(state: AppState, args: PlainArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains("uniqueness : :: std :: option :: Option :: None"),
            "{expanded}"
        );
        assert!(
            expanded.contains("concurrency : :: std :: option :: Option :: None"),
            "{expanded}"
        );
    }

    #[test]
    fn job_macro_generates_enqueue_tracked_companions() {
        let expanded = job_macro(
            quote! { name = "export_orders" },
            quote! {
                async fn export_orders(state: AppState, args: ExportArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains("async fn enqueue_tracked"),
            "missing enqueue_tracked: {expanded}"
        );
        assert!(
            expanded.contains("async fn enqueue_tracked_for"),
            "missing enqueue_tracked_for: {expanded}"
        );
        assert!(
            expanded.contains(":: autumn_web :: job :: enqueue_tracked (\"export_orders\" , payload)"),
            "enqueue_tracked companion should delegate to the module-level free function: {expanded}"
        );
        assert!(
            expanded.contains(":: autumn_web :: job :: enqueue_tracked_for"),
            "enqueue_tracked_for companion should delegate to the module-level free function: {expanded}"
        );
        assert!(
            expanded.contains("TrackedJobHandle"),
            "companions should return TrackedJobHandle: {expanded}"
        );
        assert!(
            expanded.contains("TrackedJobOwner"),
            "enqueue_tracked_for should accept a TrackedJobOwner: {expanded}"
        );
    }

    #[test]
    fn job_macro_accepts_three_arg_signature_and_passes_context_current() {
        let expanded = job_macro(
            quote! {},
            quote! {
                async fn export_orders(state: AppState, args: ExportArgs, ctx: JobContext) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains(":: autumn_web :: job :: JobContext :: current ()"),
            "three-arg handler should be invoked with JobContext::current(): {expanded}"
        );
        assert!(
            expanded.contains("export_orders (state , args , :: autumn_web :: job :: JobContext :: current ())"),
            "generated handler closure should call the user fn with the context: {expanded}"
        );
    }

    #[test]
    fn job_macro_two_arg_expansion_does_not_reference_job_context() {
        let expanded = job_macro(
            quote! {},
            quote! {
                async fn plain(state: AppState, args: PlainArgs) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(
            !expanded.contains("JobContext"),
            "two-arg expansion must stay unchanged: {expanded}"
        );
        assert!(expanded.contains("plain (state , args) . await"), "{expanded}");
    }

    #[test]
    fn job_macro_rejects_one_arg_signature() {
        let expanded = job_macro(
            quote! {},
            quote! {
                async fn plain(state: AppState) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains("must have signature"),
            "expected a compile error about the signature: {expanded}"
        );
    }

    #[test]
    fn job_macro_rejects_four_arg_signature() {
        let expanded = job_macro(
            quote! {},
            quote! {
                async fn plain(state: AppState, args: PlainArgs, ctx: JobContext, extra: u8) -> AutumnResult<()> {
                    Ok(())
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains("must have signature"),
            "expected a compile error about the signature: {expanded}"
        );
    }
}

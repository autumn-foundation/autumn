//! `#[task]` proc macro implementation for one-off operational scripts.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{FnArg, ItemFn, LitStr};

struct TaskAttrs {
    name: Option<String>,
}

fn parse_task_args(attr: TokenStream) -> syn::Result<TaskAttrs> {
    let mut result = TaskAttrs { name: None };

    syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            let value: LitStr = meta.value()?.parse()?;
            result.name = Some(value.value());
            Ok(())
        } else {
            Err(meta.error("unsupported attribute: expected name"))
        }
    })
    .parse2(attr)?;

    Ok(result)
}

fn first_doc_line(input_fn: &ItemFn) -> String {
    input_fn
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .find_map(|attr| {
            let syn::Meta::NameValue(name_value) = &attr.meta else {
                return None;
            };
            let syn::Expr::Lit(expr_lit) = &name_value.value else {
                return None;
            };
            let syn::Lit::Str(lit) = &expr_lit.lit else {
                return None;
            };
            let line = lit.value().trim().to_owned();
            (!line.is_empty()).then_some(line)
        })
        .unwrap_or_default()
}

pub fn task_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_task_args(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "#[task] functions must be async")
            .to_compile_error();
    }

    let fn_name = &input_fn.sig.ident;
    let companion_name = format_ident!("__autumn_one_off_task_info_{fn_name}");
    let task_name = attrs.name.unwrap_or_else(|| fn_name.to_string());
    let description = first_doc_line(&input_fn);

    let mut extractors = Vec::new();
    let mut call_args = Vec::new();
    for (idx, input) in input_fn.sig.inputs.iter().enumerate() {
        let FnArg::Typed(pat_type) = input else {
            return syn::Error::new_spanned(input, "#[task] functions cannot take self receivers")
                .to_compile_error();
        };
        let ty = &pat_type.ty;
        let arg_ident = format_ident!("__autumn_task_arg_{idx}");
        extractors.push(quote! {
            let #arg_ident: #ty =
                <#ty as ::autumn_web::task::TaskExtractor>::from_task_parts(
                    &mut parts,
                    &state,
                )
                .await?;
        });
        call_args.push(arg_ident);
    }

    // ── Architecture-graph node (#1747) ─────────────────────────
    let graph_descriptor =
        crate::graph::emit_job_descriptor(&input_fn, &quote! { #task_name }, "Task", &quote! { "" });

    quote! {
        #input_fn

        #graph_descriptor

        #[doc(hidden)]
        pub fn #companion_name() -> ::autumn_web::task::OneOffTaskInfo {
            ::autumn_web::task::OneOffTaskInfo {
                name: #task_name.to_string(),
                description: #description.to_string(),
                handler: |state: ::autumn_web::AppState, args: ::std::vec::Vec<::std::string::String>| {
                    Box::pin(async move {
                        let mut parts = ::autumn_web::task::request_parts_for_task_args(&args)?;
                        #(#extractors)*
                        #fn_name(#(#call_args),*).await
                    })
                },
            }
        }
    }
}

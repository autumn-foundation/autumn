//! `#[autumn_web::main]` macro implementation.
//!
//! Generates a synchronous `main()` that builds a tokio runtime and
//! blocks on the user's async body. We generate the runtime manually
//! instead of delegating to `#[tokio::main]` because `tokio::main`
//! emits code with `::tokio::` paths, which don't resolve when the
//! user only depends on `autumn-web`.
//!
//! ## Optional runtime arguments
//!
//! Building the runtime by hand also means the attribute owns the
//! `tokio::runtime::Builder` call, so it can expose the knobs an app
//! otherwise has to drop the macro entirely to reach:
//!
//! ```ignore
//! #[autumn_web::main(
//!     flavor = "multi_thread",
//!     worker_threads = 4,
//!     max_blocking_threads = 64,
//!     thread_name = "autumn-worker",
//!     thread_stack_size = 3 * 1024 * 1024,
//!     thread_keep_alive = "30s",
//!     configure = tune_runtime,
//! )]
//! async fn main() { /* ... */ }
//! ```
//!
//! Every argument is optional; with none of them the expansion is exactly
//! what it was before they existed (`new_multi_thread().enable_all()`).
//!
//! `configure` is the escape hatch for everything this list does not name
//! (`on_thread_start`, `on_thread_stop`, `global_queue_interval`, …): it
//! names a `fn(&mut tokio::runtime::Builder)` that runs last, after the
//! declarative arguments, so it can also override them.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser as _;
use syn::spanned::Spanned as _;
use syn::{Expr, ItemFn, Lit, LitStr, Path};

/// The runtime flavor an app can ask for, mirroring `#[tokio::main]`'s
/// `flavor` argument.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    CurrentThread,
    MultiThread,
}

/// Parsed `#[autumn_web::main(...)]` arguments.
///
/// Each numeric knob is stored as the user's `Expr` rather than a parsed
/// integer: tokio's setters take ordinary runtime values, so
/// `worker_threads = std::thread::available_parallelism().map_or(4, |n| n.get())`
/// is as valid as `worker_threads = 4`. A literal `0` is still rejected at
/// expansion time, because tokio would only panic on it at startup.
#[derive(Default)]
struct MainArgs {
    /// `flavor = "multi_thread" | "current_thread"`. `None` means
    /// `multi_thread`, kept distinct from an explicit `"multi_thread"` only
    /// so the `worker_threads` conflict can point at the `flavor` the user
    /// actually wrote.
    flavor: Option<(Flavor, LitStr)>,
    /// `worker_threads = <usize expr>` — multi-thread flavor only.
    worker_threads: Option<Expr>,
    /// `max_blocking_threads = <usize expr>`.
    max_blocking_threads: Option<Expr>,
    /// `thread_name = <Into<String> expr>`.
    thread_name: Option<Expr>,
    /// `thread_stack_size = <usize expr>`.
    thread_stack_size: Option<Expr>,
    /// `thread_keep_alive = "30s"`, stored as whole seconds.
    thread_keep_alive: Option<(u64, LitStr)>,
    /// `configure = <path to fn(&mut Builder)>`.
    configure: Option<Path>,
}

/// Arguments accepted inside `#[autumn_web::main(...)]`, in the order they are
/// documented. Used only to build the "unsupported argument" diagnostic.
const SUPPORTED_ARGS: &[&str] = &[
    "flavor",
    "worker_threads",
    "max_blocking_threads",
    "thread_name",
    "thread_stack_size",
    "thread_keep_alive",
    "configure",
];

/// Parse a duration string like `"30s"`, `"2m"`, `"1h"` into whole seconds.
///
/// Deliberately the same shape accepted by `#[throttle(per = "1m")]` so an
/// app only has to learn one duration spelling. Sub-second keep-alives are
/// not expressible, which is fine: tokio's default is 10 seconds and the knob
/// exists to lengthen it.
fn parse_keep_alive(s: &str) -> Result<u64, String> {
    let mut total_secs: u64 = 0;
    let mut current = String::new();
    let mut saw_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if ch.is_ascii_alphabetic() {
            let num: u64 = current
                .parse()
                .map_err(|_| format!("invalid duration: '{s}' (expected e.g. \"30s\")"))?;
            current.clear();
            saw_unit = true;
            let scale = match ch {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                _ => return Err(format!("invalid duration: '{s}' (unit must be s/m/h/d)")),
            };
            total_secs = num
                .checked_mul(scale)
                .and_then(|scaled| total_secs.checked_add(scaled))
                .ok_or_else(|| format!("invalid duration: '{s}' (overflow)"))?;
        } else if ch != ' ' {
            return Err(format!("invalid duration: '{s}'"));
        }
    }
    if !current.is_empty() {
        return Err(format!(
            "invalid duration: '{s}' (trailing number without unit)"
        ));
    }
    if !saw_unit || total_secs == 0 {
        return Err(format!(
            "invalid duration: '{s}' (must be greater than zero)"
        ));
    }
    Ok(total_secs)
}

/// Reject a literal `0` for a knob tokio would panic on at startup.
///
/// Only literals are checked — a computed expression is the user's to get
/// right, exactly as it would be if they built the runtime themselves.
fn reject_literal_zero(expr: &Expr, key: &str) -> syn::Result<()> {
    if let Expr::Lit(lit) = expr
        && let Lit::Int(int) = &lit.lit
        && int.base10_parse::<u128>().is_ok_and(|n| n == 0)
    {
        return Err(syn::Error::new_spanned(
            expr,
            format!("`{key}` must be greater than zero"),
        ));
    }
    Ok(())
}

/// Reject a second occurrence of an argument rather than silently keeping the
/// last one: a repeated `worker_threads` is an unfinished edit, not intent.
fn reject_duplicate(already_set: bool, key: &str, span: proc_macro2::Span) -> syn::Result<()> {
    if already_set {
        return Err(syn::Error::new(
            span,
            format!("duplicate `{key}` argument. Declare each argument once."),
        ));
    }
    Ok(())
}

fn parse_main_args(attr: TokenStream) -> syn::Result<MainArgs> {
    let mut args = MainArgs::default();
    if attr.is_empty() {
        return Ok(args);
    }

    syn::meta::parser(|meta| {
        let span = meta.path.span();
        if meta.path.is_ident("flavor") {
            reject_duplicate(args.flavor.is_some(), "flavor", span)?;
            let lit: LitStr = meta.value()?.parse().map_err(|_| {
                meta.error(
                    "`flavor` expects a string literal: \"multi_thread\" or \"current_thread\"",
                )
            })?;
            let flavor = match lit.value().as_str() {
                "multi_thread" => Flavor::MultiThread,
                "current_thread" => Flavor::CurrentThread,
                other => {
                    return Err(syn::Error::new_spanned(
                        &lit,
                        format!(
                            "unknown `flavor` \"{other}\". Expected \"multi_thread\" or \
                             \"current_thread\"."
                        ),
                    ));
                }
            };
            args.flavor = Some((flavor, lit));
            Ok(())
        } else if meta.path.is_ident("worker_threads") {
            reject_duplicate(args.worker_threads.is_some(), "worker_threads", span)?;
            let expr: Expr = meta.value()?.parse()?;
            reject_literal_zero(&expr, "worker_threads")?;
            args.worker_threads = Some(expr);
            Ok(())
        } else if meta.path.is_ident("max_blocking_threads") {
            reject_duplicate(
                args.max_blocking_threads.is_some(),
                "max_blocking_threads",
                span,
            )?;
            let expr: Expr = meta.value()?.parse()?;
            reject_literal_zero(&expr, "max_blocking_threads")?;
            args.max_blocking_threads = Some(expr);
            Ok(())
        } else if meta.path.is_ident("thread_name") {
            reject_duplicate(args.thread_name.is_some(), "thread_name", span)?;
            args.thread_name = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("thread_stack_size") {
            reject_duplicate(args.thread_stack_size.is_some(), "thread_stack_size", span)?;
            let expr: Expr = meta.value()?.parse()?;
            reject_literal_zero(&expr, "thread_stack_size")?;
            args.thread_stack_size = Some(expr);
            Ok(())
        } else if meta.path.is_ident("thread_keep_alive") {
            reject_duplicate(args.thread_keep_alive.is_some(), "thread_keep_alive", span)?;
            let lit: LitStr = meta.value()?.parse().map_err(|_| {
                meta.error("`thread_keep_alive` expects a duration string literal, e.g. \"30s\"")
            })?;
            let secs =
                parse_keep_alive(&lit.value()).map_err(|msg| syn::Error::new_spanned(&lit, msg))?;
            args.thread_keep_alive = Some((secs, lit));
            Ok(())
        } else if meta.path.is_ident("configure") {
            reject_duplicate(args.configure.is_some(), "configure", span)?;
            let path: Path = meta.value()?.parse().map_err(|_| {
                meta.error(
                    "`configure` expects the path of a function taking \
                     `&mut tokio::runtime::Builder`, e.g. `configure = tune_runtime`",
                )
            })?;
            args.configure = Some(path);
            Ok(())
        } else {
            Err(meta.error(format!(
                "unsupported `#[autumn_web::main]` argument. Supported arguments: `{}`.",
                SUPPORTED_ARGS.join("`, `")
            )))
        }
    })
    .parse2(attr)?;

    // tokio's current-thread runtime has no worker pool to size, so accepting
    // `worker_threads` there would silently do nothing.
    if let (Some(expr), Some((Flavor::CurrentThread, flavor_lit))) =
        (&args.worker_threads, &args.flavor)
    {
        let mut err = syn::Error::new_spanned(
            expr,
            "`worker_threads` has no effect with `flavor = \"current_thread\"`. \
             Drop it, or use the default `flavor = \"multi_thread\"`.",
        );
        err.combine(syn::Error::new_spanned(
            flavor_lit,
            "`flavor = \"current_thread\"` declared here",
        ));
        return Err(err);
    }

    Ok(args)
}

/// Emit the statements that construct and configure the runtime builder,
/// leaving it bound as `__autumn_runtime_builder` for the caller to `.build()`.
fn builder_tokens(args: &MainArgs) -> TokenStream {
    let flavor = args
        .flavor
        .as_ref()
        .map_or(Flavor::MultiThread, |(f, _)| *f);
    let constructor = match flavor {
        Flavor::CurrentThread => quote! { new_current_thread },
        Flavor::MultiThread => quote! { new_multi_thread },
    };

    // Each setter binds the user's expression to the type tokio's setter
    // takes, so a wrong type is reported against the argument they wrote
    // rather than somewhere inside the generated chain.
    let mut setters = TokenStream::new();
    if let Some(expr) = &args.worker_threads {
        setters.extend(quote! {
            .worker_threads({ let __autumn_worker_threads: usize = #expr; __autumn_worker_threads })
        });
    }
    if let Some(expr) = &args.max_blocking_threads {
        setters.extend(quote! {
            .max_blocking_threads({
                let __autumn_max_blocking_threads: usize = #expr;
                __autumn_max_blocking_threads
            })
        });
    }
    if let Some(expr) = &args.thread_name {
        setters.extend(quote! {
            .thread_name({ let __autumn_thread_name: ::std::string::String = (#expr).into(); __autumn_thread_name })
        });
    }
    if let Some(expr) = &args.thread_stack_size {
        setters.extend(quote! {
            .thread_stack_size({
                let __autumn_thread_stack_size: usize = #expr;
                __autumn_thread_stack_size
            })
        });
    }
    if let Some((secs, _)) = &args.thread_keep_alive {
        setters.extend(quote! {
            .thread_keep_alive(::std::time::Duration::from_secs(#secs))
        });
    }

    // The escape hatch runs last so it can override anything above it. The
    // `fn` binding is what turns a wrong signature into an error pointing at
    // the user's `configure = ...` instead of at the expansion.
    let configure = args.configure.as_ref().map(|path| {
        quote! {
            let __autumn_configure: fn(&mut ::autumn_web::reexports::tokio::runtime::Builder) = #path;
            __autumn_configure(&mut __autumn_runtime_builder);
        }
    });

    // Statements, not a block expression: they are spliced straight into the
    // generated `main`, because `{ ... }.build()` in statement position parses
    // the braces as a block and then chokes on the `.`.
    quote! {
        let mut __autumn_runtime_builder =
            ::autumn_web::reexports::tokio::runtime::Builder::#constructor();
        __autumn_runtime_builder
            .enable_all()
            #setters;
        #configure
    }
}

pub fn main_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "the main function must be async")
            .to_compile_error();
    }

    let args = match parse_main_args(attr) {
        Ok(args) => args,
        Err(err) => return err.to_compile_error(),
    };
    let runtime = builder_tokens(&args);

    let body = &input_fn.block;
    let attrs = &input_fn.attrs;

    quote! {
        #(#attrs)*
        fn main() {
            // Tell the framework where autumn.toml lives (the app's crate root),
            // and whether the *user's* crate was built in debug mode.
            // cfg!(debug_assertions) evaluates here — in the user's crate context —
            // so it reflects their build mode, not autumn-web's library build mode.
            ::autumn_web::config::__set_macro_context(
                env!("CARGO_MANIFEST_DIR").to_string(),
                cfg!(debug_assertions),
            );

            // Bake the app's compile-time build + git provenance into the
            // process so `/actuator/info` can report exactly which commit/build
            // is running. `env!`/`option_env!` are evaluated *here*, in the
            // app crate's compile context, so CARGO_PKG_* reflect the app (not
            // autumn-web) and the AUTUMN_BUILD_* vars come from the app's
            // build.rs. All are optional — a build with no git checkout or no
            // build.rs stanza degrades gracefully. See issue #1242.
            ::autumn_web::build_info::__set_build_context(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                option_env!("AUTUMN_BUILD_GIT_SHA"),
                option_env!("AUTUMN_BUILD_GIT_SHA_SHORT"),
                option_env!("AUTUMN_BUILD_GIT_BRANCH"),
                option_env!("AUTUMN_BUILD_GIT_DIRTY"),
                option_env!("AUTUMN_BUILD_TIMESTAMP"),
            );

            #runtime

            __autumn_runtime_builder
                .build()
                .expect("failed to build tokio runtime")
                .block_on(async move #body);
        }
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::main_macro;

    /// A minimal async `main` body, reused by every case below.
    fn expand(attr: proc_macro2::TokenStream) -> String {
        main_macro(attr, quote! { async fn main() { let _ = 1; } }).to_string()
    }

    #[test]
    fn no_arguments_keeps_the_previous_expansion() {
        let generated = expand(quote! {});
        assert!(
            generated.contains("new_multi_thread ()"),
            "the default flavor must stay multi-thread: {generated}"
        );
        assert!(
            generated.contains("enable_all ()"),
            "all drivers must stay enabled: {generated}"
        );
        for absent in [
            "worker_threads",
            "max_blocking_threads",
            "thread_name",
            "thread_stack_size",
            "thread_keep_alive",
        ] {
            assert!(
                !generated.contains(absent),
                "no argument means no `{absent}` setter: {generated}"
            );
        }
    }

    #[test]
    fn current_thread_flavor_switches_the_constructor() {
        let generated = expand(quote! { flavor = "current_thread" });
        assert!(
            generated.contains("new_current_thread ()"),
            "`flavor = \"current_thread\"` must build a current-thread runtime: {generated}"
        );
        assert!(
            !generated.contains("new_multi_thread"),
            "the multi-thread constructor must not also be emitted: {generated}"
        );
    }

    #[test]
    fn every_knob_reaches_its_builder_setter() {
        let generated = expand(quote! {
            flavor = "multi_thread",
            worker_threads = 4,
            max_blocking_threads = 64,
            thread_name = "autumn-worker",
            thread_stack_size = 3 * 1024 * 1024,
            thread_keep_alive = "2m",
        });
        assert!(generated.contains("worker_threads"), "{generated}");
        assert!(generated.contains("max_blocking_threads"), "{generated}");
        assert!(generated.contains("thread_name"), "{generated}");
        assert!(generated.contains("thread_stack_size"), "{generated}");
        assert!(
            generated
                .contains("thread_keep_alive (:: std :: time :: Duration :: from_secs (120u64))"),
            "`2m` must expand to 120 seconds: {generated}"
        );
    }

    #[test]
    fn numeric_arguments_accept_expressions() {
        let generated = expand(quote! {
            worker_threads = std::thread::available_parallelism().map_or(4, |n| n.get())
        });
        assert!(
            generated.contains("available_parallelism"),
            "a computed worker count must be passed through verbatim: {generated}"
        );
    }

    #[test]
    fn configure_runs_after_the_declarative_arguments() {
        let generated = expand(quote! { worker_threads = 2, configure = tune });
        let setter = generated
            .find("worker_threads")
            .expect("the worker_threads setter must be emitted");
        let hook = generated
            .find("__autumn_configure (")
            .expect("the configure hook must be called");
        assert!(
            setter < hook,
            "`configure` must run last so it can override the arguments: {generated}"
        );
        assert!(
            generated
                .contains("fn (& mut :: autumn_web :: reexports :: tokio :: runtime :: Builder)"),
            "the hook must be bound to a typed fn pointer for a readable error: {generated}"
        );
    }

    #[test]
    fn a_non_async_main_is_still_rejected() {
        let generated = main_macro(quote! {}, quote! { fn main() {} }).to_string();
        assert!(
            generated.contains("the main function must be async"),
            "{generated}"
        );
    }

    #[test]
    fn unknown_arguments_are_rejected_with_the_supported_list() {
        let generated = expand(quote! { worker_thread = 4 });
        assert!(
            generated.contains("unsupported"),
            "a typo'd argument must not be silently ignored: {generated}"
        );
        assert!(
            generated.contains("worker_threads"),
            "the diagnostic must list the supported arguments: {generated}"
        );
    }

    #[test]
    fn worker_threads_conflicts_with_the_current_thread_flavor() {
        let generated = expand(quote! { flavor = "current_thread", worker_threads = 4 });
        assert!(
            generated.contains("has no effect with"),
            "a worker count the runtime would ignore must be a compile error: {generated}"
        );
    }

    #[test]
    fn a_literal_zero_is_rejected() {
        for attr in [
            quote! { worker_threads = 0 },
            quote! { max_blocking_threads = 0 },
            quote! { thread_stack_size = 0 },
        ] {
            let generated = expand(attr);
            assert!(
                generated.contains("must be greater than zero"),
                "a zero tokio would panic on must be caught at expansion: {generated}"
            );
        }
    }

    #[test]
    fn duplicate_arguments_are_rejected() {
        let generated = expand(quote! { worker_threads = 2, worker_threads = 4 });
        assert!(generated.contains("duplicate"), "{generated}");
    }

    #[test]
    fn an_unknown_flavor_is_rejected() {
        let generated = expand(quote! { flavor = "single_thread" });
        assert!(generated.contains("unknown `flavor`"), "{generated}");
    }

    #[test]
    fn a_malformed_keep_alive_is_rejected() {
        for attr in [
            quote! { thread_keep_alive = "30" },
            quote! { thread_keep_alive = "0s" },
            quote! { thread_keep_alive = "soon" },
        ] {
            let generated = expand(attr);
            assert!(generated.contains("invalid duration"), "{generated}");
        }
    }
}

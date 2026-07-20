//! `#[sim_test]` macro implementation (sim-testing W1, issue #1797).
//!
//! Mirrors the [`crate::main_macro`] pattern: we emit a *synchronous*
//! `#[test]` function that builds a tokio runtime by hand (rather than
//! delegating to `#[tokio::test]`), because the generated code uses
//! `::autumn_web::` paths so it resolves for a user who only depends on
//! `autumn-web`.
//!
//! The user writes:
//!
//! ```ignore
//! #[sim_test]
//! async fn my_test(mut sim: Sim) { /* ... */ }
//! ```
//!
//! and it expands (in spirit) to:
//!
//! ```ignore
//! #[test]
//! fn my_test() {
//!     let seed = ::autumn_web::sim::__seed_from_env();
//!     let runtime = ::autumn_web::reexports::tokio::runtime::Builder::new_current_thread()
//!         .enable_all()
//!         .start_paused(true)
//!         .build()
//!         .expect("failed to build paused sim runtime");
//!     let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
//!         let mut sim = ::autumn_web::sim::Sim::from_seed(seed);
//!         runtime.block_on(async move { /* body */ })
//!     }));
//!     if let ::std::result::Result::Err(payload) = result {
//!         ::std::eprintln!(
//!             "{}",
//!             ::autumn_web::sim::__replay_line(seed, env!("CARGO_PKG_NAME"), "my_test")
//!         );
//!         ::std::panic::resume_unwind(payload);
//!     }
//! }
//! ```
//!
//! The paused runtime (`start_paused(true)`) freezes the tokio virtual clock at
//! start so W1 tests run deterministically; W2 will drive that clock through
//! [`Sim`](../../autumn_web/sim/struct.Sim.html)'s `SimClock` handle.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{FnArg, ItemFn};

/// Expand `#[sim_test] async fn t(sim: Sim) { .. }` into a deterministic,
/// seed-driven `#[test]` function.
pub fn sim_test_macro(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    let sig = &input_fn.sig;

    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(sig.fn_token, "sim_test functions must be async")
            .to_compile_error();
    }

    // Exactly one argument, and it must be a plain typed argument (not `self`).
    let mut typed_args = sig.inputs.iter().filter_map(|arg| match arg {
        FnArg::Typed(pat_type) => Some(pat_type),
        FnArg::Receiver(_) => None,
    });
    let Some(sim_arg) = typed_args.next() else {
        return syn::Error::new_spanned(
            &sig.inputs,
            "sim_test functions must take exactly one argument, the `Sim` handle \
             (e.g. `async fn my_test(sim: Sim)`)",
        )
        .to_compile_error();
    };
    if typed_args.next().is_some() {
        return syn::Error::new_spanned(
            &sig.inputs,
            "sim_test functions must take exactly one argument, the `Sim` handle \
             (e.g. `async fn my_test(sim: Sim)`)",
        )
        .to_compile_error();
    }

    let sim_pat = &sim_arg.pat;
    // Bind the constructed Sim with the user's declared type annotation, so the
    // type they name (e.g. an imported `Sim`) is actually used — otherwise the
    // import warns as unused — and the constructed value is checked against it.
    let sim_ty = &sim_arg.ty;
    let fn_name = &sig.ident;
    let fn_name_str = fn_name.to_string();
    let attrs = &input_fn.attrs;
    let vis = &input_fn.vis;
    let body = &input_fn.block;

    quote! {
        #(#attrs)*
        #[test]
        #vis fn #fn_name() {
            // Deterministic seed: `AUTUMN_SIM_SEED` (hex `0x..` or decimal),
            // defaulting to 0. Parsing lives in autumn-web so it is unit-tested
            // and the macro stays tiny.
            let __autumn_sim_seed: u64 = ::autumn_web::sim::__seed_from_env();

            // A current-thread runtime with the virtual clock paused at start,
            // so time never advances on its own — the executor is deterministic.
            // `start_paused(true)` requires tokio's `test-util` feature.
            let __autumn_sim_runtime =
                ::autumn_web::reexports::tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .start_paused(true)
                    .build()
                    .expect("failed to build paused sim runtime");

            // Run the user body with the fn argument bound to the constructed
            // Sim, catching a panic so we can print the deterministic replay
            // line before re-propagating (the test must still FAIL).
            let __autumn_sim_result = ::std::panic::catch_unwind(
                ::std::panic::AssertUnwindSafe(|| {
                    let #sim_pat: #sim_ty = ::autumn_web::sim::Sim::from_seed(__autumn_sim_seed);
                    __autumn_sim_runtime.block_on(async move #body)
                }),
            );

            if let ::std::result::Result::Err(__autumn_sim_payload) = __autumn_sim_result {
                ::std::eprintln!(
                    "{}",
                    ::autumn_web::sim::__replay_line(
                        __autumn_sim_seed,
                        env!("CARGO_PKG_NAME"),
                        #fn_name_str,
                    )
                );
                ::std::panic::resume_unwind(__autumn_sim_payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sim_test_macro;
    use quote::quote;

    // The expansion is a synchronous `#[test]` fn that builds a paused
    // current-thread runtime, constructs a Sim from the seed, and wires the
    // replay-on-panic path.
    #[test]
    fn expands_to_paused_test_runtime_with_replay() {
        let out = sim_test_macro(quote! {}, quote! { async fn t(sim: Sim) {} });
        let rendered = out.to_string();
        assert!(
            rendered.contains("# [test]"),
            "expansion must be a #[test] fn: {rendered}"
        );
        assert!(
            rendered.contains("new_current_thread"),
            "expansion must build a current-thread runtime: {rendered}"
        );
        assert!(
            rendered.contains("start_paused"),
            "expansion must pause the runtime clock: {rendered}"
        );
        assert!(
            rendered.contains("from_seed"),
            "expansion must construct the Sim from the seed: {rendered}"
        );
        assert!(
            rendered.contains("__replay_line"),
            "expansion must wire the replay line: {rendered}"
        );
        assert!(
            rendered.contains("catch_unwind"),
            "expansion must catch the panic to print the replay line: {rendered}"
        );
        assert!(
            rendered.contains("__seed_from_env"),
            "expansion must read the seed from the environment: {rendered}"
        );
    }

    // The user's argument pattern (e.g. `mut sim`) is preserved as the binding
    // for the constructed Sim.
    #[test]
    fn preserves_the_argument_binding_pattern() {
        let out = sim_test_macro(quote! {}, quote! { async fn t(mut sim: Sim) {} });
        let rendered = out.to_string();
        assert!(
            rendered.contains("let mut sim :"),
            "expansion must bind the Sim to the user's `mut sim` pattern with its \
             declared type: {rendered}"
        );
    }

    // Non-async input is a targeted compile error mentioning `async`.
    #[test]
    fn rejects_non_async() {
        let out = sim_test_macro(quote! {}, quote! { fn t(sim: Sim) {} });
        let rendered = out.to_string();
        assert!(
            rendered.contains("compile_error"),
            "non-async input must produce a compile error: {rendered}"
        );
        assert!(
            rendered.contains("async"),
            "the compile error must mention async: {rendered}"
        );
    }

    // Zero arguments is a targeted compile error about the argument count.
    #[test]
    fn rejects_zero_args() {
        let out = sim_test_macro(quote! {}, quote! { async fn t() {} });
        let rendered = out.to_string();
        assert!(
            rendered.contains("compile_error"),
            "zero-arg input must produce a compile error: {rendered}"
        );
        assert!(
            rendered.contains("exactly one argument"),
            "the compile error must mention the argument count: {rendered}"
        );
    }

    // Two arguments is a targeted compile error about the argument count.
    #[test]
    fn rejects_two_args() {
        let out = sim_test_macro(quote! {}, quote! { async fn t(sim: Sim, extra: u8) {} });
        let rendered = out.to_string();
        assert!(
            rendered.contains("compile_error"),
            "two-arg input must produce a compile error: {rendered}"
        );
        assert!(
            rendered.contains("exactly one argument"),
            "the compile error must mention the argument count: {rendered}"
        );
    }
}

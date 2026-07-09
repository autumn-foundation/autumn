//! `#[autumn_web::main]` macro implementation.
//!
//! Generates a synchronous `main()` that builds a tokio runtime and
//! blocks on the user's async body. We generate the runtime manually
//! instead of delegating to `#[tokio::main]` because `tokio::main`
//! emits code with `::tokio::` paths, which don't resolve when the
//! user only depends on `autumn-web`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemFn;

pub fn main_macro(item: TokenStream) -> TokenStream {
    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "the main function must be async")
            .to_compile_error();
    }

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

            ::autumn_web::reexports::tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(async move #body);
        }
    }
}

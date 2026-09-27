use proc_macro2::TokenStream;

use crate::route;

pub fn oauth2_callback_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    // This macro is a GET alias, so `route_macro` would happily hand the
    // callback to the edge lane. An OAuth2 callback completes an authentication
    // flow against origin session state, so refuse it here (#1790).
    if let Some(err) = crate::edge::reject_if_edge(
        &item,
        "`#[edge]` cannot be combined with `#[oauth2_callback]`: the callback completes an \
         authentication flow against origin session state, which the edge capsule does not \
         have. Serve this route from the origin.",
    ) {
        return err;
    }
    route::route_macro("GET", "get", attr, item)
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::oauth2_callback_macro;

    #[test]
    fn rejects_a_live_edge_attribute() {
        let generated = oauth2_callback_macro(
            quote! { "/auth/github/callback" },
            quote! {
                #[edge]
                async fn callback() -> &'static str { "ok" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] on an OAuth2 callback must be rejected: {generated}"
        );
        assert!(
            generated.contains("oauth2_callback"),
            "the error must name the attribute it conflicts with: {generated}"
        );
    }

    #[test]
    fn rejects_an_expanded_edge_marker() {
        let edged = crate::edge::edge_macro(
            quote! {},
            quote! { async fn callback() -> &'static str { "ok" } },
        );
        let generated =
            oauth2_callback_macro(quote! { "/auth/github/callback" }, edged).to_string();

        assert!(
            generated.contains("compile_error"),
            "an already-expanded #[edge] must be rejected here too: {generated}"
        );
    }

    #[test]
    fn plain_callback_still_expands_to_a_get_route() {
        let generated = oauth2_callback_macro(
            quote! { "/auth/github/callback" },
            quote! { async fn callback() -> &'static str { "ok" } },
        )
        .to_string();

        assert!(
            !generated.contains("compile_error"),
            "an ordinary callback must still compile: {generated}"
        );
        assert!(
            generated.contains("__autumn_route_info_callback"),
            "an ordinary callback must still emit its route companion: {generated}"
        );
    }
}

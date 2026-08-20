//! `edge_routes![]` collection macro (#1790).
//!
//! Collects `#[edge]`-annotated handlers into a `Vec<EdgeRoute>`, parallel to
//! the `routes![]` and `static_routes![]` macros. Registration stays
//! author-explicit: marking a handler `#[edge]` makes it *eligible*, listing it
//! here is what puts it in the capsule.

use proc_macro2::TokenStream;

pub fn edge_routes_macro(input: TokenStream) -> TokenStream {
    crate::collect::collect_companions(input, "__autumn_edge_route_")
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::edge_routes_macro;

    #[test]
    fn calls_the_edge_companion_for_each_handler() {
        let generated = edge_routes_macro(quote! { greet, note }).to_string();
        assert!(
            generated.contains("__autumn_edge_route_greet ()"),
            "each handler must resolve to its edge companion: {generated}"
        );
        assert!(
            generated.contains("__autumn_edge_route_note ()"),
            "each handler must resolve to its edge companion: {generated}"
        );
    }

    #[test]
    fn qualifies_module_paths() {
        let generated = edge_routes_macro(quote! { handlers::greet }).to_string();
        assert!(
            generated.contains("handlers :: __autumn_edge_route_greet ()"),
            "a module-qualified handler must keep its module: {generated}"
        );
    }

    #[test]
    fn an_empty_list_is_an_empty_vec() {
        let generated = edge_routes_macro(quote! {}).to_string();
        assert_eq!(generated, quote! { ::std::vec::Vec::new() }.to_string());
    }
}

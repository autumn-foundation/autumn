//! `listeners![]` collection macro.

use proc_macro2::TokenStream;

pub fn listeners_macro(input: TokenStream) -> TokenStream {
    crate::collect::collect_companions(input, "__autumn_listener_info_")
}

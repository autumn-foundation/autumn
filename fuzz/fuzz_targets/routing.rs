#![no_main]
//! Fuzz target: routing / path-parameter extraction.
//!
//! Feeds UTF-8-lossy path/host strings through the router's parsing helpers.

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

/// Largest index `<= i` that lies on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let i = i.min(s.len());
    let mut b = i;
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);

    let _ = __fuzz::extract_path_params(&s);
    let _ = __fuzz::extract_host_without_port(&s);

    // Prefix matching against both a fixed route shape and the arbitrary input
    // (so pathological prefixes are exercised too).
    let _ = __fuzz::path_matches_route_prefix(&s, "/api/:id/posts");
    let _ = __fuzz::path_matches_route_prefix(&s, &s);

    // Nested-path join over two halves split on a char boundary.
    let split = floor_char_boundary(&s, s.len() / 2);
    let (a, b) = s.split_at(split);
    let _ = __fuzz::join_nested_path(a, b);
});

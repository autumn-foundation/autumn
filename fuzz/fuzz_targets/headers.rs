#![no_main]
//! Fuzz target: trusted-proxy / `X-Forwarded-*` header parsing.
//!
//! Drives `TrustedProxy::parse`, `ProxyResolver::parse_forwarded_ip`, and the
//! full resolver over arbitrary header values.

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Split the input into up to five newline-delimited fields, one per header;
    // missing fields fall back to the whole input so single-line seeds still
    // exercise every parser.
    let text = String::from_utf8_lossy(data);
    let mut fields = text.split('\n');
    let f0 = fields.next().unwrap_or(&text);
    let f1 = fields.next().unwrap_or(f0);
    let f2 = fields.next().unwrap_or(f0);
    let f3 = fields.next().unwrap_or(f0);
    let f4 = fields.next().unwrap_or(f0);

    let _ = __fuzz::parse_trusted_proxy(f0);
    let _ = __fuzz::parse_forwarded_ip(f0);

    // Full resolver: X-Forwarded-For, X-Real-IP, X-Forwarded-Proto,
    // X-Forwarded-Host, Host.
    __fuzz::resolve_forwarded(f0, f1, f2, f3, f4);
});

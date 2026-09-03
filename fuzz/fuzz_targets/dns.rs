#![no_main]
//! Fuzz target: DNS wire-format parsing for the ACME DNS-01 propagation probe
//! (issue #1620).
//!
//! `parse_response` decodes a datagram read straight off a UDP socket — length-
//! prefixed labels, compression pointers, per-type RDATA — so its input is
//! attacker-shapeable by anyone on-path, and by any resolver an operator points
//! `[server.tls.acme.dns] resolvers` at. The classic failure modes are all
//! reachable from here: an out-of-bounds slice on a lying `RDLENGTH`, and a
//! decompression loop from a pointer cycle.
//!
//! The first two bytes are the transaction id the parser is told to expect, so
//! the corpus can exercise both the id-mismatch rejection and the full parse.
//! The remainder is the raw message.

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (id, msg) = match data {
        [hi, lo, rest @ ..] => (u16::from_be_bytes([*hi, *lo]), rest),
        _ => return,
    };
    // Any outcome is fine; the target asserts only that it terminates without
    // panicking, slicing out of bounds, or looping.
    let _ = __fuzz::parse_dns_response(id, "_acme-challenge.myapp.test", msg);
});

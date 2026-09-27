#![no_main]
//! Fuzz target: the sim demo scenario (issue #2967).
//!
//! Drives the same `Op` vocabulary as the `sim-sweep` proptest sweep, so
//! coverage-guided search can find op sequences that random seeds miss.
//!
//! Layout of the input bytes:
//!   [seed:8 little-endian][op:3]...
//! Each op is a tag byte (even: deposit, odd: withdraw) and a little-endian
//! `u16` amount. See `autumn::sim::scenario::ops_from_bytes`.
//!
//! A crash is an `always!` invariant break. Encode a shrunk sweep failure with
//! `autumn::sim::scenario::ops_to_bytes` to replay it here.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    autumn::sim::scenario::run_fuzz_input(data);
});

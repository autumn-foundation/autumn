#![no_main]
//! Fuzz target: the sandboxed-plugin decoders (#1609).
//!
//! Every byte these three functions see came out of an artifact the operator
//! explicitly did not audit — that is the entire premise of the lane — so the
//! decoders are the first thing a hostile plugin author gets to aim at. None of
//! them may panic, and none may be driven into an unbounded allocation by a
//! length field it was handed.
//!
//! The input is split on a NUL byte so one corpus entry can carry a binary
//! container and a text frame; a single-field entry drives all three, which
//! keeps a raw byte seed useful.

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut fields = data.splitn(2, |byte| *byte == 0);
    let container = fields.next().unwrap_or(data);
    let text = fields.next().unwrap_or(container);
    let text = String::from_utf8_lossy(text);

    // The container: magic, framing, a guest-chosen manifest length, the
    // manifest itself, and the digest binding it to the module.
    let _ = __fuzz::read_sandbox_artifact(container);
    // The manifest on its own, so a text seed reaches the validator directly.
    let _ = __fuzz::parse_sandbox_manifest(&text);
    // One NDJSON frame, as a guest writes it to stdout.
    let _ = __fuzz::parse_sandbox_guest_frame(&text);
});

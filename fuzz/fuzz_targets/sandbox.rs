#![no_main]
//! Fuzz target: the sandboxed-plugin decoders (#1609).
//!
//! Every byte these three functions see came out of an artifact the operator
//! explicitly did not audit — that is the entire premise of the lane — so the
//! decoders are the first thing a hostile plugin author gets to aim at. None of
//! them may panic, and none may be driven into an unbounded allocation by a
//! length field it was handed.
//!
//! The container reader gets the input whole; the text decoders get whatever
//! follows the first NUL, so one corpus entry can still carry both and an entry
//! without a NUL drives all three from the same bytes.
//!
//! The container arm deliberately does *not* take the split half. Every v1
//! header carries the format version as `\x01\0\0\0` at offset 8, so the first
//! NUL in a well-formed container falls at offset 9 — splitting first handed
//! the reader `AUTUMNPL\x01` and nothing else, and this target could only ever
//! exercise the truncated-header path. The manifest length, the manifest and
//! the digest it exists to fuzz were unreachable.

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut fields = data.splitn(2, |byte| *byte == 0);
    let head = fields.next().unwrap_or(data);
    let text = String::from_utf8_lossy(fields.next().unwrap_or(head));

    // The container: magic, framing, a guest-chosen manifest length, the
    // manifest itself, and the digest binding it to the module. Handed the
    // whole input, because a container that reaches any of those necessarily
    // contains a NUL.
    let _ = __fuzz::read_sandbox_artifact(data);
    // The manifest on its own, so a text seed reaches the validator directly.
    let _ = __fuzz::parse_sandbox_manifest(&text);
    // One NDJSON frame, as a guest writes it to stdout.
    let _ = __fuzz::parse_sandbox_guest_frame(&text);
});

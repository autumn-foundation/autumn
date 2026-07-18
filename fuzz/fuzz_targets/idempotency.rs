#![no_main]
//! Fuzz target: idempotency key/TTL handling.
//!
//! Interprets the leading bytes as a (seconds, nanos) TTL and the remainder as
//! key/body bytes, then drives `MemoryIdempotencyStore::{set,get,try_lock}`.
//! This deliberately exercises the deadline arithmetic (`saturating_deadline`),
//! so reintroducing an unchecked `Instant::now() + ttl` makes this target crash
//! on a boundary-value TTL (issue #1637, AC2 falsifiable check).

use autumn::idempotency::{IdempotencyRecord, IdempotencyStore, MemoryIdempotencyStore};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

fn take_u64(data: &[u8]) -> (u64, &[u8]) {
    if let Some(head) = data.get(..8) {
        (
            u64::from_le_bytes(head.try_into().expect("8 bytes")),
            &data[8..],
        )
    } else {
        // When input is starved, bias toward the overflow boundary so the
        // deadline path is always exercised even for tiny corpus entries.
        (u64::MAX, data)
    }
}

fn take_u32(data: &[u8]) -> (u32, &[u8]) {
    if let Some(head) = data.get(..4) {
        (
            u32::from_le_bytes(head.try_into().expect("4 bytes")),
            &data[4..],
        )
    } else {
        (0, data)
    }
}

fuzz_target!(|data: &[u8]| {
    let (secs, rest) = take_u64(data);
    let (nanos, rest) = take_u32(rest);
    // `Duration::new` carries nanos into secs; clamp nanos below 1e9 so the
    // carry is zero and `Duration::new` itself cannot panic — we want to probe
    // the *store's* arithmetic, not `Duration::new`'s.
    let ttl = Duration::new(secs, nanos % 1_000_000_000);

    let mid = rest.len() / 2;
    let (key_bytes, body) = rest.split_at(mid);
    let key = String::from_utf8_lossy(key_bytes);

    let store = MemoryIdempotencyStore::new(Duration::from_secs(60));
    let record = IdempotencyRecord {
        status: 200,
        headers: Vec::new(),
        body: body.to_vec(),
        metadata: Vec::new(),
    };
    // set() computes an expiry deadline from `ttl` — the overflow site.
    store.set(&key, record, body.to_vec(), ttl);
    let _ = store.get(&key);
    // The in-flight lock path computes a deadline from the same TTL.
    let _ = store.try_lock(&key, ttl);
});

#![no_main]
//! Fuzz target: a Constela document in, bounded memory out (#2626).
//!
//! A document comes from a language model, so treat it as hostile. Parse,
//! validate, render and dispatch must not panic. The peak live bytes must stay
//! under a bound set by the input size, not by what the document asks for.
//!
//! The first two bytes set the size of a string that the host puts in each
//! declared string state field, as app-owned state would be. The rest is the
//! document.

use autumn::constela::{Document, Limits, RenderContext};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

/// Fixed headroom for one render: its allocation budget, its output, buffer
/// growth slack, and escape copies. Same as `autumn/tests/constela_memory.rs`.
const FIXED: u64 = 40 * 1024 * 1024;

/// Bytes the pipeline may use for each byte of input.
const PER_INPUT_BYTE: u64 = 24;

/// Fail if one measured step went over the bound.
fn check(step: &str, info: &allocation_counter::AllocationInfo, bound: u64) {
    assert!(
        info.bytes_max <= bound,
        "{step}: peak {} bytes over the {bound}-byte bound",
        info.bytes_max
    );
}

fuzz_target!(|data: &[u8]| {
    let Some((size, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let Ok(source) = std::str::from_utf8(rest) else {
        return;
    };
    // Up to 1 MiB of app state per string field.
    let blob = usize::from(u16::from_le_bytes(*size)) * 16;

    let mut parsed = None;
    let info = allocation_counter::measure(|| {
        parsed = Document::parse(source, &Limits::default()).ok();
    });
    check("parse", &info, FIXED + PER_INPUT_BYTE * source.len() as u64);
    let Some(document) = parsed else {
        return;
    };

    // The host state is built outside the measured steps. Each step gets its
    // own budget, so each step is measured alone.
    let mut state: Map<String, Value> = document.initial_state();
    let mut state_bytes = 0u64;
    for value in state.values_mut() {
        if value.is_string() {
            *value = Value::String("x".repeat(blob));
            state_bytes += blob as u64;
        }
    }
    let bound = FIXED + PER_INPUT_BYTE * (source.len() as u64 + state_bytes);
    let ctx = RenderContext {
        state,
        ..RenderContext::default()
    };
    let info = allocation_counter::measure(|| drop(document.render(&ctx)));
    check("render", &info, bound);

    let names: Vec<String> = document
        .action_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    for name in names {
        let mut state = ctx.state.clone();
        let info = allocation_counter::measure(|| {
            drop(document.dispatch(&name, &mut state, &Map::new()));
        });
        check(&name, &info, bound);
    }
});

//! Isolated test: a Constela document in, bounded memory out (#2626).
//!
//! Each case builds a hostile document from a known amplification shape, then
//! measures the peak live bytes of parse, validate, render and dispatch. The
//! peak must stay under [`bound`]. Example tests cover the shapes we know;
//! this test varies their sizes so that a path that keeps bytes without a
//! charge fails here.
//!
//! Own binary: `allocation-counter` installs a global allocator. See
//! CLAUDE.md, "Isolated Integration Tests". Counting is per thread.

use autumn_web::constela::{Document, Limits, RenderContext};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

/// Fixed headroom: one render may use its 4 MiB allocation budget, its 4 MiB
/// output, the growth slack of the output buffer, and escape copies.
const FIXED: u64 = 40 * 1024 * 1024;

/// Bytes the pipeline may use for each byte of input. Parse keeps a JSON tree
/// and a typed tree of the source.
const PER_INPUT_BYTE: u64 = 24;

/// The peak live bytes a case may reach.
fn bound(source: &str, state_bytes: usize) -> u64 {
    FIXED + PER_INPUT_BYTE * (source.len() as u64 + state_bytes as u64)
}

/// One hostile document, and the app state the host would give it.
#[derive(Debug)]
struct Case {
    source: String,
    state: Map<String, Value>,
    state_bytes: usize,
}

fn blob_state(bytes: usize) -> (Map<String, Value>, usize) {
    let mut state = Map::new();
    state.insert("blob".into(), Value::String("x".repeat(bytes)));
    (state, bytes)
}

/// Finding 1: `set t_i = state(blob)`, `n` times.
fn read_fan_out(n: usize, blob: usize) -> Case {
    let fields: Vec<String> = (0..n)
        .map(|i| format!(r#""t{i}":{{"type":"string","initial":""}}"#))
        .collect();
    let steps: Vec<String> = (0..n)
        .map(|i| {
            format!(r#"{{"do":"set","target":"t{i}","value":{{"expr":"state","name":"blob"}}}}"#)
        })
        .collect();
    let source = format!(
        r#"{{"version":"1.0",
            "state":{{"blob":{{"type":"string","initial":""}},{}}},
            "actions":[{{"name":"go","steps":[{}]}}],
            "view":{{"kind":"element","tag":"div"}}}}"#,
        fields.join(","),
        steps.join(",")
    );
    let (state, state_bytes) = blob_state(blob);
    Case {
        source,
        state,
        state_bytes,
    }
}

/// Finding 2: an `each` of `n` empty portals, each with a long target.
fn portal_targets(n: usize, target: usize) -> Case {
    let source = format!(
        r#"{{"version":"1.0",
            "state":{{"rows":{{"type":"list","initial":[]}}}},
            "view":{{"kind":"each","items":{{"expr":"state","name":"rows"}},"as":"r",
              "body":{{"kind":"portal","target":"{}","children":[]}}}}}}"#,
        "t".repeat(target)
    );
    let mut state = Map::new();
    state.insert("rows".into(), Value::Array(vec![json!(1); n]));
    Case {
        source,
        state,
        state_bytes: n * 8,
    }
}

/// Finding 3: one call that omits `n` required props of a long-named
/// component.
fn missing_props(n: usize, name: usize) -> Case {
    let name = "C".repeat(name);
    let params: Vec<String> = (0..n)
        .map(|i| format!(r#""p{i}":{{"type":"string"}}"#))
        .collect();
    let source = format!(
        r#"{{"version":"1.0",
            "components":{{"{name}":{{"params":{{{}}},"view":{{"kind":"element","tag":"div"}}}}}},
            "view":{{"kind":"component","name":"{name}"}}}}"#,
        params.join(",")
    );
    Case {
        source,
        state: Map::new(),
        state_bytes: 0,
    }
}

/// Nested loops over one large row. Each level copied every outer binding.
fn nested_loops(depth: usize, blob: usize) -> Case {
    let mut view = r#"{"kind":"text","value":{"expr":"lit","value":"."}}"#.to_string();
    for level in 0..depth {
        view = format!(
            r#"{{"kind":"each","items":{{"expr":"state","name":"rows"}},"as":"v{level}","body":{view}}}"#
        );
    }
    let source = format!(
        r#"{{"version":"1.0","state":{{"rows":{{"type":"list","initial":[]}}}},"view":{view}}}"#
    );
    let mut state = Map::new();
    state.insert(
        "rows".into(),
        Value::Array(vec![Value::String("x".repeat(blob))]),
    );
    Case {
        source,
        state,
        state_bytes: blob,
    }
}

/// A component that passes a large param to itself through a chain of
/// components, with a loop in each.
fn param_chain(depth: usize, blob: usize) -> Case {
    let mut components = Vec::new();
    for level in 0..depth {
        let inner = if level + 1 == depth {
            r#"{"kind":"text","value":{"expr":"lit","value":"."}}"#.to_string()
        } else {
            format!(
                r#"{{"kind":"component","name":"C{}","props":{{"p":{{"expr":"param","name":"p"}}}}}}"#,
                level + 1
            )
        };
        components.push(format!(
            r#""C{level}":{{"params":{{"p":{{"type":"json"}}}},"view":{{"kind":"each",
               "items":{{"expr":"lit","value":[1,2,3]}},"as":"r","body":{inner}}}}}"#
        ));
    }
    let source = format!(
        r#"{{"version":"1.0","state":{{"blob":{{"type":"string","initial":""}}}},
            "components":{{{}}},
            "view":{{"kind":"component","name":"C0","props":{{"p":{{"expr":"state","name":"blob"}}}}}}}}"#,
        components.join(",")
    );
    let (state, state_bytes) = blob_state(blob);
    Case {
        source,
        state,
        state_bytes,
    }
}

/// Parse, render, and dispatch every action. Errors are expected: a hostile
/// document should fail, and fail cheaply.
fn run(case: &Case) {
    let Ok(document) = Document::parse(&case.source, &Limits::unbounded()) else {
        return;
    };
    let mut ctx = RenderContext {
        state: document.initial_state(),
        ..RenderContext::default()
    };
    ctx.state
        .extend(case.state.iter().map(|(k, v)| (k.clone(), v.clone())));
    let _ = std::hint::black_box(document.render(&ctx));
    let names: Vec<String> = document
        .action_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    for name in names {
        let _ = std::hint::black_box(document.dispatch(&name, &mut ctx.state, &Map::new()));
    }
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let info = allocation_counter::measure(|| run(case));
    let limit = bound(&case.source, case.state_bytes);
    prop_assert!(
        info.bytes_max <= limit,
        "peak {} bytes, bound {} bytes, source {} bytes",
        info.bytes_max,
        limit,
        case.source.len()
    );
    Ok(())
}

fn cases() -> impl Strategy<Value = Case> {
    prop_oneof![
        (500usize..=2_000, 16usize..=64).prop_map(|(n, kib)| read_fan_out(n, kib * 1024)),
        (1_000usize..=5_000, 1usize..=64).prop_map(|(n, kib)| portal_targets(n, kib * 1024)),
        (500usize..=2_000, 8usize..=50).prop_map(|(n, kib)| missing_props(n, kib * 1024)),
        (8usize..=20, 256usize..=1_024).prop_map(|(d, kib)| nested_loops(d, kib * 1024)),
        (8usize..=40, 256usize..=1_024).prop_map(|(d, kib)| param_chain(d, kib * 1024)),
    ]
}

proptest! {
    // Each case moves megabytes. A few cases per shape is enough to vary the
    // sizes; the shapes are what the test is about.
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn a_hostile_document_uses_bounded_memory(case in cases()) {
        check(&case)?;
    }
}

/// Each shape at its largest size, so every shape runs on every test run.
#[test]
fn each_shape_at_its_largest_size_is_bounded() {
    let cases = [
        ("read fan-out", read_fan_out(2_000, 64 * 1024)),
        ("portal targets", portal_targets(5_000, 64 * 1024)),
        ("missing props", missing_props(2_000, 50 * 1024)),
        ("nested loops", nested_loops(20, 1024 * 1024)),
        ("param chain", param_chain(40, 1024 * 1024)),
    ];
    // Check every shape before failing, so one run names each leak.
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(name, case)| check(case).err().map(|err| format!("{name}: {err}")))
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A legitimate document stays well under the bound. Without this, a bound
/// set too high would pass every hostile case too.
#[test]
fn an_ordinary_document_is_far_under_the_bound() {
    let case = Case {
        source: r#"{"version":"1.0",
            "state":{"rows":{"type":"list","initial":["a","b","c"]}},
            "view":{"kind":"each","items":{"expr":"state","name":"rows"},"as":"r",
              "body":{"kind":"element","tag":"p","children":[
                {"kind":"text","value":{"expr":"var","name":"r"}}]}}}"#
            .to_string(),
        state: Map::new(),
        state_bytes: 0,
    };
    let info = allocation_counter::measure(|| run(&case));
    assert!(
        info.bytes_max < 1024 * 1024,
        "peak {} bytes",
        info.bytes_max
    );
}

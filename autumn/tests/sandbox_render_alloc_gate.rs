//! Isolated integration test: a render tag or attribute name the allow-list is
//! going to refuse must not be copied on its way to being refused.
//!
//! `write_node` and `write_attribute` used to `to_ascii_lowercase()` the
//! guest's string and *then* look it up, so a tag or attribute name the size of
//! the stdout frame was duplicated in full before anything decided it was not a
//! name at all — per concurrent render, on the host side, outside the Wasm
//! memory limiter that bounds everything else the guest allocates.
//!
//! This has to be measured rather than asserted structurally, for the same
//! reason as the request-header gate beside it: the *result* is identical
//! either way — the fragment is refused — and only the transient allocation
//! differs. A test that inspects the returned `Err` passes against the defect,
//! which is exactly what happened when this fix was first written.
//!
//! Its own test binary because `allocation-counter` installs a counting
//! `#[global_allocator]`, a process-wide side effect per CLAUDE.md's
//! isolated-test rules.

#![cfg(feature = "plugin-sandbox")]

use autumn_web::plugin_sandbox::capability::{FragmentNode, render::render};

/// Big enough that one copy cannot hide in the noise of building the node.
const NAME_BYTES: usize = 512 * 1024;

/// The whole fragment budget, so nothing is refused for size rather than shape.
const BUDGET: usize = 1 << 20;

fn element(tag: String, attributes: Vec<(String, String)>) -> Vec<FragmentNode> {
    vec![FragmentNode::Element {
        tag,
        attributes,
        children: Vec::new(),
    }]
}

#[test]
fn refusing_an_unknown_tag_does_not_copy_its_name() {
    // Both fragments are built outside the measured windows, the long `String`
    // included: what is measured is what the renderer does with a name it is
    // handed, not the cost of handing it one.
    let short = element("nope".to_owned(), Vec::new());
    let long = element("n".repeat(NAME_BYTES), Vec::new());

    // Warm-up outside the windows, so neither is charged for first-run setup.
    drop(render(&short, BUDGET));

    let without = allocation_counter::measure(|| {
        let out = render(&short, BUDGET);
        std::hint::black_box(&out);
    });
    let with = allocation_counter::measure(|| {
        let out = render(&long, BUDGET);
        std::hint::black_box(&out);
    });

    let extra = with.bytes_total.saturating_sub(without.bytes_total);
    assert!(
        extra < NAME_BYTES as u64,
        "refusing a {NAME_BYTES}-byte tag allocated {extra} bytes more than refusing a \
         short one — the name was copied on its way to being refused",
    );
}

#[test]
fn refusing_an_unknown_attribute_does_not_copy_its_name() {
    // The tag is legal here, so the refusal happens at the attribute and not
    // before it.
    let short = element("p".to_owned(), vec![("nope".to_owned(), "1".to_owned())]);
    let long = element(
        "p".to_owned(),
        vec![("n".repeat(NAME_BYTES), "1".to_owned())],
    );

    drop(render(&short, BUDGET));

    let without = allocation_counter::measure(|| {
        let out = render(&short, BUDGET);
        std::hint::black_box(&out);
    });
    let with = allocation_counter::measure(|| {
        let out = render(&long, BUDGET);
        std::hint::black_box(&out);
    });

    let extra = with.bytes_total.saturating_sub(without.bytes_total);
    assert!(
        extra < NAME_BYTES as u64,
        "refusing a {NAME_BYTES}-byte attribute name allocated {extra} bytes more than \
         refusing a short one — the name was copied on its way to being refused",
    );
}

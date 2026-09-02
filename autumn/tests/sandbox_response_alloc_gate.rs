//! Isolated integration test: deciding a response value is unacceptable must
//! not cost a copy of it.
//!
//! Two questions the wire asks about guest-chosen strings, both answerable on
//! the borrowed value and both answered by lower-casing it first:
//!
//! - is a `Location` segment a double-dot segment, in any of its four
//!   spellings; and
//! - is a declared content type on the allowlist.
//!
//! A guest chooses those lengths — a header value is bounded only by the
//! stdout-line ceiling, roughly twice the response ceiling — and `run` is
//! holding the parsed response and its clone while the question is asked. So a
//! copy made to answer it is a third live copy of the same bytes, against a
//! footprint term that budgets fewer.
//!
//! Measured rather than asserted structurally, for the same reason as the
//! request-header gate beside it: the *answer* is identical either way. A test
//! that inspects the returned verdict passes against the defect. Allocation is
//! the observable.
//!
//! Its own binary because `allocation-counter` installs a counting
//! `#[global_allocator]`, a process-wide side effect per CLAUDE.md's
//! isolated-test rules.

#![cfg(feature = "plugin-sandbox")]

use autumn_web::plugin_sandbox::{OwnedRoutes, SandboxResponse};

/// Big enough that one copy cannot hide in the noise, and far below any
/// ceiling so the value is examined rather than refused for its size first.
const VALUE_BYTES: usize = 512 * 1024;

/// A plugin that serves everything beneath its prefix.
///
/// The redirect fixture below has to stay *permitted* — the whole point is that
/// the value survives the check, so nothing about the outcome reveals what
/// answering cost. A narrower route set would have it refused, and the gate
/// would be measuring the refusal path instead.
fn owns_the_prefix() -> OwnedRoutes {
    OwnedRoutes::from_paths(["/hello", "/hello/{*rest}"])
}

const fn response(headers: Vec<(String, String)>) -> SandboxResponse {
    SandboxResponse {
        status: 200,
        headers,
        body: Vec::new(),
    }
}

#[test]
fn deciding_a_redirect_stays_in_the_prefix_does_not_copy_the_path() {
    // One enormous path segment, so `is_double_dot_segment` is asked about the
    // whole thing at once. It is a *permitted* redirect — inside the prefix,
    // no climbing — which is the point: the value survives, so nothing about
    // the outcome reveals that answering cost a copy of it.
    let long = format!("/hello/{}", "a".repeat(VALUE_BYTES));
    let small = "/hello/ok".to_owned();

    let long_response = response(vec![("location".to_owned(), long)]);
    let small_response = response(vec![("location".to_owned(), small)]);

    // Warm-up outside the windows: whatever the first call sets up, neither
    // measurement should be charged for.
    drop(
        small_response
            .clone()
            .sanitize("/hello", &owns_the_prefix()),
    );

    let baseline = allocation_counter::measure(|| {
        let out = small_response
            .clone()
            .sanitize("/hello", &owns_the_prefix());
        std::hint::black_box(&out);
    });
    let measured = allocation_counter::measure(|| {
        let out = long_response.clone().sanitize("/hello", &owns_the_prefix());
        std::hint::black_box(&out);
    });

    // `clone()` inside the window copies the value once by construction; the
    // defect is the *second* copy, made only to compare four spellings.
    let extra = measured
        .bytes_total
        .saturating_sub(baseline.bytes_total)
        .saturating_sub(VALUE_BYTES as u64);
    assert!(
        extra < (VALUE_BYTES / 2) as u64,
        "comparing a {VALUE_BYTES}-byte redirect segment allocated {extra} bytes \
         beyond the response's own copy — the segment was lower-cased to be read",
    );
}

#[test]
fn deciding_a_content_type_is_refused_does_not_copy_it() {
    // No semicolon, so the essence is the whole value, and it is not on the
    // allowlist — so it is refused. What may not happen is lower-casing all of
    // it to find that out; the denial needs a name, not the megabyte.
    let long = format!("text/{}", "x".repeat(VALUE_BYTES));
    let long_response = response(vec![("content-type".to_owned(), long)]);
    let small_response = response(vec![("content-type".to_owned(), "text/nope".to_owned())]);

    drop(small_response.refused_content_type());

    let baseline = allocation_counter::measure(|| {
        let out = small_response.refused_content_type();
        std::hint::black_box(&out);
    });
    let measured = allocation_counter::measure(|| {
        let out = long_response.refused_content_type();
        std::hint::black_box(&out);
    });

    let extra = measured.bytes_total.saturating_sub(baseline.bytes_total);
    assert!(
        extra < (VALUE_BYTES / 2) as u64,
        "refusing a {VALUE_BYTES}-byte content type allocated {extra} bytes — \
         the value was lower-cased whole to be looked up",
    );

    // The verdict itself must not change: it is still refused, and still names
    // the type well enough for an operator to act on.
    let refused = long_response
        .refused_content_type()
        .expect("an unlisted type must still be refused");
    assert!(
        refused.starts_with("text/"),
        "the refusal must still name the type: {:?}",
        refused.get(..32),
    );
    assert!(
        refused.len() < VALUE_BYTES,
        "the whole value was carried into the refusal: {} bytes",
        refused.len(),
    );

    // And an allowed type is still allowed, whatever case it arrives in.
    let fine = response(vec![(
        "Content-Type".to_owned(),
        "TEXT/PLAIN; charset=utf-8".to_owned(),
    )]);
    assert!(
        fine.refused_content_type().is_none(),
        "an allowed type must survive, in any case",
    );
}

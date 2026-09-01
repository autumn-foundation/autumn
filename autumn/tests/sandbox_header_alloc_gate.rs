//! Isolated integration test: a request header the guest never sees must not be
//! copied on the way to being discarded.
//!
//! `canonicalize_headers` lower-cases, filters through the request allowlist,
//! and sorts. It used to clone every value and *then* filter, so a `Cookie` or
//! `Authorization` was duplicated in full before being thrown away. That was
//! merely wasteful while the metadata ceiling refused such a request — and
//! became reachable the moment the ceiling stopped charging for headers the
//! frame discards, which it should not charge for, since they never cross.
//! The two properties travel together: not counting them means not copying
//! them.
//!
//! This has to be measured rather than asserted structurally. The *output* of
//! the function is identical either way — only the transient allocation
//! differs — so a test that inspects the returned headers passes against the
//! defect. Allocation is the observable.
//!
//! Its own test binary because `allocation-counter` installs a counting
//! `#[global_allocator]`, a process-wide side effect per CLAUDE.md's
//! isolated-test rules, and one that would tax every allocation in the
//! consolidated suite.

#![cfg(feature = "plugin-sandbox")]

use autumn_web::plugin_sandbox::{
    DeclaredRoute, ResourceLimits, SandboxCapability, SandboxHost, SandboxManifest, SandboxRequest,
};

/// Big enough that one copy of it cannot hide inside the noise of the rest of
/// the request path, and far below any ceiling so the request is served rather
/// than refused.
const CREDENTIAL_BYTES: usize = 512 * 1024;

fn manifest() -> SandboxManifest {
    SandboxManifest {
        name: "autumn-plugin-hello".to_owned(),
        version: "0.1.0".to_owned(),
        wire_version: 1,
        prefix: "/hello".to_owned(),
        capabilities: vec![SandboxCapability::HttpRequest],
        sha256: "0".repeat(64),
        routes: vec![DeclaredRoute {
            method: "GET".to_owned(),
            path: "/hello/greet".to_owned(),
        }],
        limits: ResourceLimits::default(),
    }
}

fn request(dropped: Option<(String, String)>) -> SandboxRequest {
    let mut headers = vec![("accept".to_owned(), "text/plain".to_owned())];
    if let Some((name, value)) = dropped {
        headers.push((name, value));
    }
    SandboxRequest {
        method: "GET".to_owned(),
        route: "/hello/greet".to_owned(),
        path: "/hello/greet".to_owned(),
        query: String::new(),
        path_params: vec![],
        headers,
        body: vec![],
    }
}

#[test]
fn a_dropped_request_header_is_not_copied_before_it_is_dropped() {
    let wasm = wat::parse_str(autumn_web::plugin_sandbox::test_guests::HELLO)
        .expect("the fixture is valid WAT");
    let host = SandboxHost::from_module(manifest(), &wasm).expect("loads");

    // Both requests are built *outside* the measured windows, including the
    // credential's own `String`. What is being measured is what the sandbox
    // does with a header it is handed, not the cost of handing it one — and
    // allocating half a megabyte inside the window would swamp exactly the
    // difference this is looking for.
    let plain = request(None);
    let with_credential = request(Some((
        "Cookie".to_owned(),
        "s=".repeat(CREDENTIAL_BYTES / 2),
    )));

    // Warm-up outside the measured windows too: whatever the first run sets up,
    // neither measurement should be charged for.
    drop(host.run(&plain));

    let without = allocation_counter::measure(|| {
        let outcome = host.run(&plain);
        std::hint::black_box(&outcome);
    });
    let with = allocation_counter::measure(|| {
        let outcome = host.run(&with_credential);
        std::hint::black_box(&outcome);
    });

    // The `Cookie` is dropped by the allowlist and never reaches the guest, so
    // serving the request with one must not cost a copy of it. The caller's own
    // `String` is built outside the measured window on purpose — what is being
    // measured is what the sandbox does with it, not the cost of holding it.
    let extra = with.bytes_total.saturating_sub(without.bytes_total);
    assert!(
        extra < CREDENTIAL_BYTES as u64,
        "serving a request with a {CREDENTIAL_BYTES}-byte dropped header allocated \
         {extra} bytes more than the same request without it — the header was \
         copied on its way to being discarded",
    );
}

#[test]
fn deciding_a_header_name_is_not_wanted_does_not_copy_the_name() {
    // The value was the obvious half. The name is the same trap one step
    // smaller: looking a header up in the allowlist by lower-casing it first
    // allocates a copy of a string the caller chose the length of, purely to
    // conclude it is not wanted. A dropped header is not charged against the
    // metadata ceiling — rightly, it never crosses — so nothing else bounds
    // how long that name may be.
    //
    // Same shape as the value gate, and measured for the same reason: the
    // returned headers are identical whether the predicate compares in place
    // or against a lower-cased copy.
    let wasm = wat::parse_str(autumn_web::plugin_sandbox::test_guests::HELLO)
        .expect("the fixture is valid WAT");
    let host = SandboxHost::from_module(manifest(), &wasm).expect("loads");

    let plain = request(None);
    let with_long_name = request(Some(("X".repeat(CREDENTIAL_BYTES), "1".to_owned())));

    drop(host.run(&plain));

    let without = allocation_counter::measure(|| {
        let outcome = host.run(&plain);
        std::hint::black_box(&outcome);
    });
    let with = allocation_counter::measure(|| {
        let outcome = host.run(&with_long_name);
        std::hint::black_box(&outcome);
    });

    let extra = with.bytes_total.saturating_sub(without.bytes_total);
    assert!(
        extra < CREDENTIAL_BYTES as u64,
        "deciding a {CREDENTIAL_BYTES}-byte header name is not on the allowlist \
         allocated {extra} bytes — the name was copied in order to reject it",
    );
}

//! Isolated integration test: guest stderr must not be decoded in full to keep
//! 512 characters of it.
//!
//! `stderr_excerpt` reached its excerpt through `String::from_utf8_lossy`,
//! which builds the entire replacement string before anything truncates it.
//! Each maximal invalid subpart becomes a three-byte U+FFFD, so a guest that
//! fills its 64 KiB stderr budget with invalid bytes materialises 192 KiB
//! beside the still-live buffer. `FIXED_HOST_BUFFER_BYTES` budgets the buffer
//! and an I/O scratch that is already gone by this stage, so that expansion sat
//! outside the footprint the manifest validates `max_concurrency` against.
//!
//! The same argument is already written down one function up, where the stdout
//! frame is decoded *strictly* for exactly this reason. Stderr was the sibling
//! it had not been applied to.
//!
//! This has to be measured rather than asserted structurally: the excerpt is
//! identical either way — only the transient allocation differs — so a test
//! that inspects `SandboxOutcome::stderr` passes against the defect.
//! Allocation is the observable.
//!
//! Its own test binary because `allocation-counter` installs a counting
//! `#[global_allocator]`, a process-wide side effect per CLAUDE.md's
//! isolated-test rules.

#![cfg(feature = "plugin-sandbox")]

use autumn_web::plugin_sandbox::{
    CapabilityGrants, CapabilityQuotas, DeclaredRoute, ResourceLimits, SandboxCapability,
    SandboxHost, SandboxManifest, SandboxRequest,
};

/// Fills its whole stderr budget with one byte, then answers normally.
///
/// `FILLBYTE` is the byte. Two instantiations differ only in that byte, so the
/// module, the request and the response path are identical between them and
/// the measured difference is the decode alone.
const STDERR_FILL_GUEST: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGk=\"}\0a\00")

  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  (func (export "_start")
    ;; One stderr budget's worth of the chosen byte, written in one call.
    (memory.fill (i32.const 65536) (i32.const FILLBYTE) (i32.const 65536))
    (i32.store (i32.const 0) (i32.const 65536))
    (i32.store (i32.const 4) (i32.const 65536))
    (drop (call $fd_write (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 16)))
    ;; Then an ordinary answer, so the excerpt is produced on the success path
    ;; rather than only when something has already gone wrong.
    (i32.store (i32.const 0) (i32.const 128))
    (i32.store (i32.const 4) (call $strlen (i32.const 128)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16)))
  )
)"#;

/// The stderr budget, and so the size of the buffer each guest fills.
const STDERR_BUDGET_BYTES: usize = 64 * 1024;

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
        grants: CapabilityGrants::default(),
        quotas: CapabilityQuotas::default(),
    }
}

fn request() -> SandboxRequest {
    SandboxRequest {
        method: "GET".to_owned(),
        route: "/hello/greet".to_owned(),
        path: "/hello/greet".to_owned(),
        query: String::new(),
        path_params: vec![],
        headers: vec![("accept".to_owned(), "text/plain".to_owned())],
        body: vec![],
    }
}

fn host_filling_stderr_with(fill: u8) -> SandboxHost {
    let wat = STDERR_FILL_GUEST.replace("FILLBYTE", &fill.to_string());
    let wasm = wat::parse_str(&wat).expect("the fixture is valid WAT");
    SandboxHost::from_module(manifest(), &wasm).expect("loads")
}

#[test]
fn invalid_stderr_is_not_expanded_in_full_to_excerpt_it() {
    // 0xFF is never a valid UTF-8 byte and never a continuation, so every one
    // of them is its own maximal invalid subpart: 64 KiB of them is 64 Ki
    // replacement characters, three bytes each.
    let invalid = host_filling_stderr_with(0xff);
    // 'X' is one valid byte per character, so the lossy decode of it borrows
    // rather than allocating at all — the baseline the expansion stands out
    // against.
    let valid = host_filling_stderr_with(b'X');

    let plain = request();

    // Warm-ups outside the measured windows: neither measurement should be
    // charged for whatever the first run of each host sets up.
    let warm_invalid = invalid.run(&plain);
    let warm_valid = valid.run(&plain);
    // Both guests must actually have filled the buffer, or this measures two
    // runs that did nothing and passes for the wrong reason.
    assert!(
        !warm_invalid.stderr.is_empty() && !warm_valid.stderr.is_empty(),
        "a guest wrote no stderr: the fixture is not exercising the decode",
    );
    assert!(
        warm_invalid.stderr.contains(char::REPLACEMENT_CHARACTER),
        "the invalid guest's stderr did not reach the decode: {:?}",
        warm_invalid.stderr,
    );

    let with_valid = allocation_counter::measure(|| {
        let outcome = valid.run(&plain);
        std::hint::black_box(&outcome);
    });
    let with_invalid = allocation_counter::measure(|| {
        let outcome = invalid.run(&plain);
        std::hint::black_box(&outcome);
    });

    // Decoded in full, the invalid run allocates three bytes per byte of the
    // budget where the valid run allocates none, so the difference is about
    // three times the budget. Decoded a chunk at a time, both keep one excerpt
    // and the difference is a rounding error. The bound is set well below the
    // defect and well above the noise so it is neither flaky nor vacuous.
    let extra = with_invalid
        .bytes_total
        .saturating_sub(with_valid.bytes_total);
    assert!(
        extra < (STDERR_BUDGET_BYTES / 2) as u64,
        "serving a request whose guest wrote {STDERR_BUDGET_BYTES} bytes of invalid \
         stderr allocated {extra} bytes more than the same request with valid \
         stderr — the buffer was expanded in full on the way to a 512-character \
         excerpt",
    );
}

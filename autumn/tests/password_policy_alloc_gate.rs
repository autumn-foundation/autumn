//! Isolated integration test: allocation gate for
//! [`autumn_web::auth::validate_password`] on a realistic signup-form
//! workload (Bolt follow-up to the request-ingress allocation gates —
//! #2198/#2205/#2214/#2232 — looking at the password path instead).
//!
//! Its own binary for the same reason `config_alloc_gate` is: `allocation-counter`
//! installs a process-wide counting `#[global_allocator]`, a process-wide side
//! effect per CLAUDE.md's isolated-test rules, and taxing every allocation in
//! the consolidated suite to measure a handful here is not a trade worth making.
//!
//! `validate_password` runs on every registration and password-change
//! submission — a real, public entry point, independent of whether the caller
//! goes on to bcrypt-hash the password. Bcrypt's own cost (deliberately
//! ~100ms+ at the default cost factor) dwarfs anything measured here by many
//! orders of magnitude, so this gate isolates the pure-Rust policy-check cost
//! the same way the `version_history`/`attribute_encryption` benches isolate
//! their pure-Rust component from the DB round trip alongside it — a
//! deliberately slow security primitive elsewhere in the request is not a
//! reason to stop counting allocations in the code around it.

use autumn_web::auth::{BreachCheck, PasswordPolicy, validate_password};

/// Enough repetitions that a per-call allocation cannot hide inside noise.
const CALLS: usize = 100;

/// A signup-form-shaped workload: some weak/common passwords, some strong
/// passphrases, mixed-case and multibyte, each checked against a realistic
/// two-string context (email + username) the way a registration handler
/// would call it. `validate_password` accumulates every applicable failure
/// rather than short-circuiting, so a weak password's checks all still run.
fn workload() -> Vec<(&'static str, [&'static str; 2])> {
    vec![
        ("password123", ["alice@example.com", "alice"]),
        ("Tr0ub4dour&3-Staple-Horse", ["bob@example.com", "bob"]),
        ("qwerty", ["carol@example.com", "carol"]),
        (
            "Correct-Horse-Battery-Staple-9",
            ["dave@example.com", "dave"],
        ),
        ("héllo-wörld-42", ["eve@example.com", "eve"]),
        ("N0t-A-C0mm0n-Passphrase!", ["frank@example.com", "frank"]),
        ("letmein2024", ["grace@example.com", "grace"]),
        ("Zx9!kQ2m-Lp7$Wc4", ["heidi@example.com", "heidi"]),
    ]
}

const fn policy() -> PasswordPolicy {
    PasswordPolicy::new(8, true, BreachCheck::Off)
}

/// The runtime has to be current-thread, matching `config_alloc_gate`'s
/// `measure_per_request`: `allocation-counter` counts thread-locally, so
/// anything a worker thread allocated would go uncounted.
#[test]
fn validate_password_allocations_on_a_signup_workload() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime should build");
    let runtime_policy = policy();
    let items = workload();

    // Warm-up outside the measured window: builds the `OnceLock` weak-password
    // corpus, which a steady-state call must not pay for.
    runtime.block_on(async {
        for (pw, ctx) in &items {
            let _ = validate_password(pw, &runtime_policy, ctx).await;
        }
    });

    let info = allocation_counter::measure(|| {
        runtime.block_on(async {
            for _ in 0..CALLS {
                for (pw, ctx) in &items {
                    let v = validate_password(pw, &runtime_policy, ctx).await;
                    std::hint::black_box(&v);
                }
            }
        });
    });

    let calls = (CALLS * items.len()) as u64;
    let per_call_blocks = info.count_total / calls;
    let per_call_bytes = info.bytes_total / calls;
    println!(
        "validate_password: {per_call_blocks} blocks / {per_call_bytes} bytes per call \
         ({calls} calls, {} blocks / {} bytes total)",
        info.count_total, info.bytes_total
    );

    // This workload, `reject_common = true`, 2-string context, debug profile,
    // deterministic across runs: 3,401 blocks / 67,648 bytes for 800 calls
    // before the fix below (the password got `.to_lowercase()`'d twice per
    // call — once in the common-corpus check, once more inside
    // `is_similar_to_context` — for the same string), **2,601 / 53,772**
    // after sharing the one lowercase between both checks (-23.5% blocks /
    // -20.5% bytes). The ceiling sits at the current measurement plus a little
    // headroom for feature-set variance, same convention as
    // `config_alloc_gate`'s ceilings; a failure a hair over the line means
    // re-measure and re-derive, not nudge upwards.
    assert!(
        info.count_total <= 2_700,
        "validate_password allocated {} blocks over {calls} calls, over the \
         2,700-block ceiling (2,601 measured; 3,401 was the pre-fix baseline)",
        info.count_total,
    );
    assert!(
        info.bytes_total <= 56_000,
        "validate_password allocated {} bytes over {calls} calls, over the \
         56,000-byte ceiling (53,772 measured; 67,648 was the pre-fix baseline)",
        info.bytes_total,
    );
}

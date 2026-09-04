//! Drives real requests through the production **CSRF** middleware
//! (`autumn_web::security::CsrfLayer`) with HMAC signing active, so the
//! per-request cost of signed-token verification can be profiled.
//!
//! Signed CSRF tokens (`security.csrf.enabled = true` +
//! `security.signing_secret.secret`) are only reachable through the real
//! config → router build path (`ResolvedSigningKeys` has no public
//! constructor), so — like `request_pipeline.rs` — this goes through
//! `TestApp::build()` rather than constructing `CsrfLayer` directly. Unlike
//! `request_pipeline.rs` (which leaves CSRF and session signing off), this
//! bench turns signing on: a `GET` mints a `{uuid}.{hmac_hex}` cookie the way
//! a first page load would, then repeated `POST`s echo it back via the
//! `X-CSRF-Token` header the way an authenticated SPA/JS client does — the
//! shape scaffolded forms and `htmx` submissions also produce (cookie +
//! matching token on every mutating request).
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check that the traffic isn't silently being
//! rejected: it is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench csrf_verify
//! BIN=$(find target/release/deps -maxdepth 1 -name "csrf_verify-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 1000
//! callgrind_annotate --threshold=80 callgrind.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs, subtracted, isolate the marginal per-request cost from the
//! # one-time mint + warm-up (see `request_pipeline.rs` for why).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 200
//! ```
//!
//! `--iterations N` issues one `GET` + two `POST`s per round after a fixed
//! 50-round warm-up (plus the one-time mint request).

use std::hint::black_box;

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::security::{CsrfConfig, SecurityConfig};
use autumn_web::test::TestApp;

#[get("/notes")]
async fn list_notes() -> &'static str {
    "[]"
}

#[post("/notes")]
async fn create_note() -> (StatusCode, &'static str) {
    (StatusCode::CREATED, "created")
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let mut config = AutumnConfig {
        profile: Some("test".into()),
        security: SecurityConfig {
            csrf: CsrfConfig {
                enabled: true,
                ..CsrfConfig::default()
            },
            ..SecurityConfig::default()
        },
        ..AutumnConfig::default()
    };
    // `SigningSecretConfig` isn't part of the public API surface (only
    // `AutumnConfig`/`SecurityConfig`/`CsrfConfig` are re-exported), so its
    // one field reachable through `SecurityConfig::signing_secret` has to be
    // set by assignment rather than struct-literal syntax.
    config.security.signing_secret.secret =
        Some("bolt-csrf-bench-signing-secret-0123456789abcdef0123456789abcdef".into());

    let client = TestApp::new()
        .config(config)
        .routes(routes![list_notes, create_note])
        .build();

    rt.block_on(async {
        // One-time mint: a first page load with no cookie yet. Signs and sets
        // the `{uuid}.{hmac_hex}` cookie every subsequent request reuses, the
        // same way a browser keeps one CSRF cookie for the life of a session.
        let mint = client.get("/notes").send().await;
        assert_eq!(mint.status, StatusCode::OK, "mint request must succeed");
        let set_cookie = mint
            .header("set-cookie")
            .expect("csrf layer must set a cookie on first request");
        let token = set_cookie
            .split(';')
            .next()
            .and_then(|kv| kv.split_once('='))
            .map(|(_, v)| v.to_owned())
            .expect("set-cookie must be `name=value; ...`");

        for _ in 0..50 {
            let response = client
                .post("/notes")
                .header("x-csrf-token", &token)
                .send()
                .await;
            assert_eq!(
                response.status,
                StatusCode::CREATED,
                "warm-up POST must pass CSRF"
            );
        }

        for _ in 0..iterations {
            black_box(client.get("/notes").send().await.status);
            black_box(
                client
                    .post("/notes")
                    .header("x-csrf-token", &token)
                    .send()
                    .await
                    .status,
            );
            black_box(
                client
                    .post("/notes")
                    .header("x-csrf-token", &token)
                    .send()
                    .await
                    .status,
            );
        }
    });

    println!("completed {} requests", iterations * 3 + 51);
}

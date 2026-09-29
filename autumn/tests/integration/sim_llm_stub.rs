//! Sim-testing W5.b (item 6, issue #1797) Definition of Done: the seeded LLM
//! stub (`autumn_web::sim::llm`) drives an agent's retry/fallback path
//! deterministically under the paused virtual clock.
//!
//! Everything here is seed-driven and needs no database, so it runs in the
//! consolidated integration binary under default features (no Docker, no
//! feature gate). It asserts:
//!
//! 1. canned responses return deterministically for matched prompts, with a
//!    deterministic seed-derived fallback for unmatched ones;
//! 2. a scheduled fault fires on the expected call and returns the expected
//!    [`LlmError`];
//! 3. per-call latency is only observable after `sim.advance(..)` (virtual-time
//!    integration), and the drawn latency is seed-deterministic;
//! 4. the same seed replays the identical `(response, fault)` sequence, while a
//!    different seed diverges;
//! 5. a small agent retry loop recovers from an injected fault and succeeds on
//!    the next call — the ratified use case.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use autumn_web::sim::Sim;
use autumn_web::sim::llm::{LlmCall, LlmClient, LlmError, LlmRequest, SeededLlm};
use autumn_web::sim_test;

/// A five-second latency window — comfortably longer than any zero-duration
/// drain step, so the sleep is released strictly by `sim.advance`.
const LATENCY: Duration = Duration::from_secs(5);

/// Drive `req` through a tiny agent-style retry loop: retry on error up to
/// `attempts` times, returning the first success (or the last error).
async fn call_with_retry(
    llm: &dyn LlmClient,
    req: LlmRequest,
    attempts: usize,
) -> Result<String, LlmError> {
    let mut last: Result<String, LlmError> = Err(LlmError::Timeout);
    for _ in 0..attempts {
        match llm.complete(req.clone()).await {
            Ok(resp) => return Ok(resp.text),
            Err(err) => last = Err(err),
        }
    }
    last.map(|_| String::new())
}

/// Run a single latency-bearing completion under the paused clock and return the
/// latency the stub recorded — proving the sleep is released by `advance`.
async fn drive_latency(sim: &Sim, stub: &Arc<SeededLlm>) -> Duration {
    let s = Arc::clone(stub);
    let handle = tokio::spawn(async move { s.complete(LlmRequest::new("m", "x")).await });
    // The spawned task parks on its latency sleep; a zero-duration drain leaves
    // it pending.
    sim.run_to_idle().await;
    // Advancing the virtual clock by the window releases the sleep.
    sim.advance(LATENCY).await;
    sim.run_to_idle().await;
    handle.await.unwrap().unwrap();
    stub.calls().last().unwrap().latency
}

/// Resolve 16 completions against a fresh stub (no latency, so awaits resolve
/// immediately) and return the response-text and recorded-call sequences — the
/// determinism signal a same-seed replay compares on.
async fn response_fault_sequence(seed: u64) -> (Vec<Result<String, LlmError>>, Vec<LlmCall>) {
    let stub = SeededLlm::builder(seed)
        .canned_response("hi", "hello")
        .fault_probability(0.5, LlmError::ServiceUnavailable)
        .build();
    let mut texts = Vec::new();
    for i in 0..16 {
        // Half the prompts are canned ("hi"), half are unique fallbacks.
        let prompt = if i % 2 == 0 {
            "hi there".to_string()
        } else {
            format!("free-form {i}")
        };
        texts.push(
            stub.complete(LlmRequest::new("model", prompt))
                .await
                .map(|r| r.text),
        );
    }
    (texts, stub.calls())
}

#[sim_test]
async fn seeded_llm_stub_drives_retry_fallback_deterministically(sim: Sim) {
    // ── 1. Canned responses + deterministic fallback ────────────────────────
    let llm = SeededLlm::builder(sim.seed())
        .canned_response("ping", "pong")
        .build();

    let matched_a = llm.complete(LlmRequest::new("m", "ping please")).await;
    let matched_b = llm.complete(LlmRequest::new("m", "ping please")).await;
    assert_eq!(matched_a.unwrap().text, "pong");
    assert_eq!(matched_b.unwrap().text, "pong");

    let unmatched = llm.complete(LlmRequest::new("m", "unknown-prompt")).await;
    let fallback = unmatched.unwrap().text;
    assert!(
        fallback.contains("unknown-prompt"),
        "fallback should acknowledge the prompt: {fallback}"
    );
    // The fallback is stable for the same (seed, prompt) — a fresh stub agrees.
    let llm_again = SeededLlm::builder(sim.seed()).build();
    let fallback_again = llm_again
        .complete(LlmRequest::new("m", "unknown-prompt"))
        .await
        .unwrap()
        .text;
    assert_eq!(
        fallback, fallback_again,
        "fallback must be seed-deterministic"
    );

    // ── 2. A scheduled fault fires on the expected call ─────────────────────
    let faulty = SeededLlm::builder(sim.seed())
        .fault_at(1, LlmError::RateLimited)
        .build();
    assert!(faulty.complete(LlmRequest::new("m", "a")).await.is_ok());
    assert!(
        matches!(
            faulty.complete(LlmRequest::new("m", "b")).await,
            Err(LlmError::RateLimited)
        ),
        "call #1 must be the scheduled RateLimited fault"
    );
    assert!(faulty.complete(LlmRequest::new("m", "c")).await.is_ok());

    // ── 3. Per-call latency is only observable after advancing the clock ────
    let slow = Arc::new(
        SeededLlm::builder(sim.seed())
            .latency_up_to(LATENCY)
            .build(),
    );
    let done = Arc::new(AtomicBool::new(false));
    let (slow2, done2) = (Arc::clone(&slow), Arc::clone(&done));
    let handle = tokio::spawn(async move {
        let out = slow2.complete(LlmRequest::new("m", "slow")).await;
        done2.store(true, Ordering::SeqCst);
        out
    });

    // Let the spawned task reach and park on its latency sleep. `run_to_idle`
    // only flushes zero-duration timers, so a 5s sleep stays pending.
    sim.run_to_idle().await;
    assert!(
        !done.load(Ordering::SeqCst),
        "the completion must still be sleeping on its virtual latency"
    );

    // Advancing virtual time by the latency window releases the sleep.
    sim.advance(LATENCY).await;
    sim.run_to_idle().await;
    assert!(
        done.load(Ordering::SeqCst),
        "advancing the sim clock must release the latency sleep"
    );
    let slow_text = handle.await.unwrap().unwrap().text;
    assert!(slow_text.contains("slow"));
    // The recorded latency is within the window and strictly positive.
    let recorded = slow.calls();
    assert_eq!(recorded.len(), 1);
    let slow_latency = recorded[0].latency;
    assert!(slow_latency > Duration::ZERO && slow_latency <= LATENCY);

    // Latency magnitude is seed-deterministic: a fresh same-seed stub draws the
    // same first latency, a different-seed stub (very likely) does not.
    let slow_same = Arc::new(
        SeededLlm::builder(sim.seed())
            .latency_up_to(LATENCY)
            .build(),
    );
    assert_eq!(
        drive_latency(&sim, &slow_same).await,
        slow_latency,
        "same seed ⇒ identical drawn latency"
    );

    // ── 4. Same seed ⇒ identical (response, fault) sequence; diff seed diverges
    let (texts_a, calls_a) = response_fault_sequence(sim.seed()).await;
    let (texts_b, calls_b) = response_fault_sequence(sim.seed()).await;
    assert_eq!(texts_a, texts_b, "same seed ⇒ identical response sequence");
    assert_eq!(calls_a, calls_b, "same seed ⇒ identical fault schedule");

    let (texts_c, calls_c) = response_fault_sequence(sim.seed().wrapping_add(1)).await;
    assert!(
        texts_c != texts_a || calls_c != calls_a,
        "a different seed must diverge in some observable dimension"
    );

    // ── 5. Agent retry/fallback scenario: recover from the injected fault ───
    let agent_llm = SeededLlm::builder(sim.seed())
        .canned_response("deploy", "deploying now")
        .fault_at(0, LlmError::ServiceUnavailable)
        .build();
    let recovered = call_with_retry(&agent_llm, LlmRequest::new("m", "deploy the app"), 3)
        .await
        .expect("the retry after the injected fault must succeed");
    assert_eq!(recovered, "deploying now");
    let agent_calls = agent_llm.calls();
    assert_eq!(agent_calls.len(), 2, "exactly one fault then one success");
    assert_eq!(agent_calls[0].error, Some(LlmError::ServiceUnavailable));
    assert_eq!(agent_calls[1].error, None);
}

//! Seeded, deterministic fake LLM client for simulation tests (sim-testing W5,
//! issue #1797).
//!
//! [`SeededLlm`] is a **test double** — an `OpenAI`-style completion stub — that
//! downstream apps wire into their agent code so a `#[sim_test]` can exercise
//! **retry / fallback** paths deterministically, without a live model or a
//! flaky, timing-dependent fault source. It is emphatically **not** a
//! production model client.
//!
//! # What it gives a test
//!
//! * **Deterministic canned responses** — register `(prompt-match, response)`
//!   pairs; an unmatched prompt gets a stable seed-derived fallback response.
//! * **A seeded fault schedule** — an explicit `(call_index, error)` list and/or
//!   a probabilistic draw, so the exact call that fails (and with which
//!   [`LlmError`]) is reproducible.
//! * **A seeded latency schedule** — a per-call virtual latency slept on
//!   [`tokio::time::sleep`], so the delay is only released when the test advances
//!   the sim clock ([`Sim::advance`](crate::sim::Sim::advance)).
//!
//! # The determinism contract
//!
//! Every stochastic choice is drawn from a **dedicated seeded stream** derived
//! from the sim seed (`seed ^ salt`, mirroring the chaos lane), independent of
//! the app-facing entropy source. The fallback response for an unmatched prompt
//! is a pure, order-independent function of `(seed, prompt)`. So the **same seed
//! and the same configuration replay the identical `(response, fault, latency)`
//! sequence**, and a different seed (very likely) diverges. Each call's recorded
//! latency and outcome are readable through [`SeededLlm::calls`] — the log a
//! determinism assertion compares two runs on.
//!
//! # Standalone
//!
//! [`SeededLlm`] needs no database and does not route through the
//! [`Chaos`](crate::sim::Chaos) builder: a test constructs it directly from the
//! sim (via [`Sim::seed`](crate::sim::Sim::seed) or
//! [`Sim::seeded_entropy`](crate::sim::Sim::seeded_entropy)) and injects it
//! wherever the code under test expects an [`LlmClient`].
//!
//! ```rust
//! use autumn_web::sim::llm::{LlmClient, LlmError, LlmRequest, SeededLlm};
//!
//! # async fn demo() {
//! let llm = SeededLlm::builder(0x5EED)
//!     .canned_response("weather", "It is sunny.")
//!     .fault_at(0, LlmError::RateLimited)
//!     .build();
//!
//! // A tiny agent retry loop: the first call is rate-limited, the retry wins.
//! let req = LlmRequest::new("test-model", "what is the weather?");
//! let mut last = Err(LlmError::RateLimited);
//! for _ in 0..2 {
//!     last = llm.complete(req.clone()).await;
//!     if last.is_ok() {
//!         break;
//!     }
//! }
//! assert_eq!(last.unwrap().text, "It is sunny.");
//! # }
//! ```

// The builder methods take `self` by value and record floats at runtime, so they
// are not `const`-eligible; this narrowly-scoped allow keeps the module clean
// under the workspace's `nursery` lint set (matching the parent `sim` module and
// the sibling `chaos` module) without masking real issues.
#![allow(clippy::missing_const_for_fn)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::entropy::{Entropy, SeededEntropy, derive_uuid_from};

/// Salt `XOR`ed into the sim seed to derive the **fault** decision stream,
/// keeping it independent of the latency stream and the app-facing entropy
/// source. An arbitrary fixed non-zero constant, distinct from the other salts.
const LLM_FAULT_SALT: u64 = 0x11A0_FA17_11A0_FA17;

/// Salt `XOR`ed into the sim seed to derive the **latency** magnitude stream, on
/// its own sub-stream so enabling latency never shifts the fault schedule.
const LLM_LATENCY_SALT: u64 = 0x1A7E_0C77_1A7E_0C77;

/// A single completion request handed to an [`LlmClient`].
///
/// Deliberately minimal — a `model` name and a `prompt` string — since the stub
/// only matches on the prompt. `#[non_exhaustive]` so richer inputs (message
/// roles, tools) can be added without a breaking change; build one with
/// [`LlmRequest::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    /// The requested model name (informational; echoed back on the response).
    pub model: String,
    /// The prompt text the stub matches canned responses against.
    pub prompt: String,
}

impl LlmRequest {
    /// Build a request for `model` with `prompt`.
    #[must_use]
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
        }
    }
}

/// A successful completion returned by an [`LlmClient`].
///
/// `#[non_exhaustive]` so fields (token usage, finish reason) can be added
/// later; tests read [`text`](LlmResponse::text) / [`model`](LlmResponse::model).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    /// The generated completion text.
    pub text: String,
    /// The model that produced it (echoes the request, or the stub default).
    pub model: String,
}

/// A completion failure an agent's retry / fallback path branches on.
///
/// These mirror the fault classes a real completion API surfaces. `#[non_exhaustive]`
/// so more fault classes can be added without a breaking change — callers must
/// include a wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LlmError {
    /// The request timed out waiting for a completion.
    #[error("llm request timed out")]
    Timeout,
    /// The caller was rate-limited (HTTP 429 shape); typically retryable after a
    /// backoff.
    #[error("llm request was rate limited")]
    RateLimited,
    /// The service is temporarily unavailable (HTTP 503 shape); typically
    /// retryable.
    #[error("llm service unavailable")]
    ServiceUnavailable,
    /// A server-side error (HTTP 5xx shape) carrying a short message.
    #[error("llm server error: {0}")]
    Server(String),
}

/// A dyn-safe asynchronous completion client.
///
/// The single seam the code under test depends on: production wires in a real
/// client, a `#[sim_test]` wires in a [`SeededLlm`]. Object-safe by returning a
/// boxed future (matching the crate's [`MailTransport`](crate::mail::MailTransport)
/// convention) so it can be held as `Arc<dyn LlmClient>`.
pub trait LlmClient: Send + Sync {
    /// Complete `req`, resolving to the generated text or a typed [`LlmError`].
    fn complete<'a>(
        &'a self,
        req: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>>;
}

/// One recorded completion call: the per-call determinism signal.
///
/// [`SeededLlm::calls`] returns these in call order — the reproducible latency /
/// outcome schedule a run produced. `#[non_exhaustive]` so fields can be added
/// without breaking readers; the derived [`PartialEq`] is what a same-seed
/// determinism assertion compares two runs on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCall {
    /// The zero-based call sequence number.
    pub seq: u64,
    /// The virtual latency this call slept before resolving.
    pub latency: Duration,
    /// The injected error, or `None` when the call returned a response.
    pub error: Option<LlmError>,
}

/// Builder for a [`SeededLlm`].
///
/// Construct one with [`SeededLlm::builder`] (from a seed, e.g.
/// [`Sim::seed`](crate::sim::Sim::seed)) or [`SeededLlm::from_entropy`] (from a
/// seeded [`Entropy`] handle, e.g. [`Sim::seeded_entropy`](crate::sim::Sim::seeded_entropy)),
/// chain the setters, then [`build`](SeededLlmBuilder::build). `#[non_exhaustive]`
/// so future knobs are additive.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SeededLlmBuilder {
    seed: u64,
    default_model: String,
    canned: Vec<(String, String)>,
    explicit_faults: Vec<(u64, LlmError)>,
    fault_prob: f64,
    fault_kind: LlmError,
    max_latency: Option<Duration>,
}

impl SeededLlmBuilder {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            default_model: "sim-llm".to_string(),
            canned: Vec::new(),
            explicit_faults: Vec::new(),
            fault_prob: 0.0,
            fault_kind: LlmError::ServiceUnavailable,
            max_latency: None,
        }
    }

    /// Set the model name echoed back when a request carries an empty model.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Register a canned response for any prompt **containing** `prompt_match`.
    ///
    /// Rules are tried in registration order; the first substring match wins. A
    /// prompt matching no rule gets the deterministic seed-derived fallback.
    #[must_use]
    pub fn canned_response(
        mut self,
        prompt_match: impl Into<String>,
        response: impl Into<String>,
    ) -> Self {
        self.canned.push((prompt_match.into(), response.into()));
        self
    }

    /// Fail the call at zero-based `call_index` with `error`.
    ///
    /// Explicit faults are a lookup — they do not draw from the fault stream, so
    /// they compose predictably with a probabilistic schedule.
    #[must_use]
    pub fn fault_at(mut self, call_index: u64, error: LlmError) -> Self {
        self.explicit_faults.push((call_index, error));
        self
    }

    /// Fail each non-explicitly-scheduled call with probability `p` (clamped to
    /// `[0.0, 1.0]`), returning `error`.
    ///
    /// The decision is drawn from the seeded fault stream, so the failing calls
    /// are identical on every run of a given seed.
    #[must_use]
    pub fn fault_probability(mut self, p: f64, error: LlmError) -> Self {
        self.fault_prob = clamp_prob(p);
        self.fault_kind = error;
        self
    }

    /// Give each call a deterministic virtual latency in `[1ns, max]`, drawn from
    /// the seeded latency stream and slept on [`tokio::time::sleep`].
    ///
    /// Under the paused sim runtime the sleep is released only when the test
    /// advances virtual time, so latency is observable exactly at
    /// [`Sim::advance`](crate::sim::Sim::advance). A zero `max` installs no
    /// latency.
    #[must_use]
    pub fn latency_up_to(mut self, max: Duration) -> Self {
        self.max_latency = (max > Duration::ZERO).then_some(max);
        self
    }

    /// Finish building the [`SeededLlm`].
    #[must_use]
    pub fn build(self) -> SeededLlm {
        SeededLlm {
            seed: self.seed,
            default_model: self.default_model,
            canned: self.canned,
            explicit_faults: self.explicit_faults,
            fault_prob: self.fault_prob,
            fault_kind: self.fault_kind,
            max_latency: self.max_latency,
            fault_stream: SeededEntropy::shared(self.seed ^ LLM_FAULT_SALT),
            latency_stream: SeededEntropy::shared(self.seed ^ LLM_LATENCY_SALT),
            call_seq: AtomicU64::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }
}

/// A seeded, deterministic fake [`LlmClient`] for simulation tests.
///
/// Build one with [`SeededLlm::builder`] / [`SeededLlm::from_entropy`]. See the
/// [module docs](self) for the determinism contract and a worked example.
pub struct SeededLlm {
    seed: u64,
    default_model: String,
    canned: Vec<(String, String)>,
    explicit_faults: Vec<(u64, LlmError)>,
    fault_prob: f64,
    fault_kind: LlmError,
    max_latency: Option<Duration>,
    /// Dedicated seeded fault-decision stream (`seed ^ LLM_FAULT_SALT`).
    fault_stream: Arc<dyn Entropy>,
    /// Dedicated seeded latency-magnitude stream (`seed ^ LLM_LATENCY_SALT`).
    latency_stream: Arc<dyn Entropy>,
    call_seq: AtomicU64,
    calls: Mutex<Vec<LlmCall>>,
}

impl std::fmt::Debug for SeededLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeededLlm")
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

impl SeededLlm {
    /// Start building a stub whose deterministic streams derive from `seed`.
    ///
    /// Pass [`Sim::seed`](crate::sim::Sim::seed) so the stub replays in lockstep
    /// with the rest of the simulation.
    #[must_use]
    pub fn builder(seed: u64) -> SeededLlmBuilder {
        SeededLlmBuilder::new(seed)
    }

    /// Start building a stub seeded from an [`Entropy`] handle, e.g.
    /// [`Sim::seeded_entropy`](crate::sim::Sim::seeded_entropy).
    ///
    /// Draws a single `u64` from `source` as the builder seed, so a fixed sim
    /// seed still yields a reproducible stub. Pass the handle by reference:
    /// `SeededLlm::from_entropy(sim.seeded_entropy().as_ref())`.
    #[must_use]
    pub fn from_entropy(source: &dyn Entropy) -> SeededLlmBuilder {
        SeededLlmBuilder::new(source.next_u64())
    }

    /// The recorded per-call latency / outcome schedule, in call order.
    ///
    /// The determinism signal: two same-seed, same-config runs return equal
    /// logs. Empty until [`complete`](LlmClient::complete) has been called.
    #[must_use]
    pub fn calls(&self) -> Vec<LlmCall> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Draw this call's deterministic latency in `[1ns, max]` from the latency
    /// stream, or `Duration::ZERO` when no latency is configured.
    fn draw_latency(&self) -> Duration {
        let Some(max) = self.max_latency else {
            return Duration::ZERO;
        };
        let max_nanos = max.as_nanos();
        if max_nanos == 0 {
            return Duration::ZERO;
        }
        let draw = u128::from(self.latency_stream.next_u64());
        // Floor at 1ns so the sleep is always released strictly by an advance,
        // never eagerly by a zero-duration drain step; ceiling at `max`.
        let picked = 1 + (draw % max_nanos);
        Duration::from_nanos(u64::try_from(picked).unwrap_or(u64::MAX))
    }

    /// Decide whether the probabilistic fault fires for this call.
    #[allow(clippy::cast_precision_loss)] // 53-bit mantissa: the shifted value fits f64 exactly
    fn fault_fires(&self) -> bool {
        if self.fault_prob <= 0.0 {
            return false;
        }
        let draw = self.fault_stream.next_u64();
        // Uniform in [0, 1): take the top 53 bits and divide by 2^53.
        let unit = (draw >> 11) as f64 / (1u64 << 53) as f64;
        unit < self.fault_prob
    }

    /// Resolve the outcome (error or response) for `call` / `req`, without
    /// touching the latency stream.
    fn outcome(&self, call: u64, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        if let Some((_, err)) = self.explicit_faults.iter().find(|(idx, _)| *idx == call) {
            return Err(err.clone());
        }
        if self.fault_fires() {
            return Err(self.fault_kind.clone());
        }
        let model = if req.model.is_empty() {
            self.default_model.clone()
        } else {
            req.model.clone()
        };
        let text = self
            .canned
            .iter()
            .find(|(pat, _)| req.prompt.contains(pat.as_str()))
            .map_or_else(|| fallback_text(self.seed, &req.prompt), |(_, r)| r.clone());
        Ok(LlmResponse { text, model })
    }

    fn record(&self, seq: u64, latency: Duration, error: Option<LlmError>) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(LlmCall {
                seq,
                latency,
                error,
            });
    }
}

impl LlmClient for SeededLlm {
    fn complete<'a>(
        &'a self,
        req: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let seq = self.call_seq.fetch_add(1, Ordering::SeqCst);
            // Draw the latency first so the schedule advances by exactly one
            // draw per call, then sleep it — released only by a clock advance.
            let latency = self.draw_latency();
            if latency > Duration::ZERO {
                tokio::time::sleep(latency).await;
            }
            let outcome = self.outcome(seq, &req);
            self.record(seq, latency, outcome.as_ref().err().cloned());
            outcome
        })
    }
}

/// Clamp a probability into `[0.0, 1.0]`, mapping `NaN` to `0.0` (matching the
/// chaos lane's clamp).
fn clamp_prob(p: f64) -> f64 {
    if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) }
}

/// Build the deterministic fallback completion text for an unmatched prompt.
///
/// A pure, order-independent function of `(seed, prompt)`: the same pair always
/// yields the same text (regardless of how many calls preceded it) and a
/// different seed diverges. Uses the crate's seed-derived id namespace so the
/// stability is guaranteed by the same machinery the entropy seam is tested on.
fn fallback_text(seed: u64, prompt: &str) -> String {
    let mut tag = b"sim-llm-fallback:".to_vec();
    tag.extend_from_slice(prompt.as_bytes());
    let id = derive_uuid_from(seed, &tag);
    format!("sim-llm[{id}]: acknowledged \"{prompt}\"")
}

#[cfg(test)]
mod tests {
    use super::{LlmError, SeededLlm, clamp_prob, fallback_text};

    #[test]
    fn probabilities_are_clamped() {
        assert!((clamp_prob(-1.0) - 0.0).abs() < f64::EPSILON);
        assert!((clamp_prob(2.0) - 1.0).abs() < f64::EPSILON);
        assert!((clamp_prob(f64::NAN) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fallback_is_seed_and_prompt_stable_and_diverges() {
        // Same (seed, prompt) ⇒ identical text, regardless of call order.
        assert_eq!(fallback_text(7, "hello"), fallback_text(7, "hello"));
        // Different prompt ⇒ different text.
        assert_ne!(fallback_text(7, "hello"), fallback_text(7, "world"));
        // Different seed ⇒ different text.
        assert_ne!(fallback_text(7, "hello"), fallback_text(8, "hello"));
    }

    #[test]
    fn explicit_fault_is_a_pure_lookup() {
        let llm = SeededLlm::builder(1).fault_at(2, LlmError::Timeout).build();
        // The outcome helper decides faults without needing the async runtime.
        assert!(matches!(
            llm.outcome(2, &super::LlmRequest::new("m", "p")),
            Err(LlmError::Timeout)
        ));
        assert!(llm.outcome(0, &super::LlmRequest::new("m", "p")).is_ok());
    }

    #[test]
    fn from_entropy_seed_is_reproducible() {
        use crate::entropy::SeededEntropy;
        // Two fresh sources of the same seed draw the same builder seed, so the
        // captured seed (and therefore the fallback text) agrees.
        let a = SeededLlm::from_entropy(&SeededEntropy::new(3)).build();
        let b = SeededLlm::from_entropy(&SeededEntropy::new(3)).build();
        assert_eq!(a.seed, b.seed);
        assert_eq!(fallback_text(a.seed, "x"), fallback_text(b.seed, "x"));
    }
}

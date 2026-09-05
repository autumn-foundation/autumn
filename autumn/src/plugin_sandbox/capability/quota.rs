//! Per-request capability quotas (issue #1632).
//!
//! Fuel is the guest's ceiling. It is not the host's: a `kv-set` costs the guest
//! one call frame and costs the host a cache round-trip; a `job-enqueue` costs
//! it a durable write. A plugin whose fuel budget is generous — which every
//! plugin that renders a page has — could spend all of it on host work priced
//! at nothing.
//!
//! So every capability carries a count, and the counts share one budget:
//!
//! * a per-capability ceiling (`kv_reads`, `outbound_calls`, …) bounds one
//!   surface, and
//! * `calls` bounds the *sum*, so a plugin cannot spend every per-capability
//!   ceiling at once and call that staying within its quota.
//!
//! Exceeding one denies that call and records it. It does not fail the request:
//! see the module header on why a denial is an answer.

use std::sync::Mutex;
use std::time::Instant;

use super::super::grants::CapabilityQuotas;
use super::super::manifest::SandboxCapability;
use super::CapabilityCall;

/// What one request has spent.
///
/// Counters rather than a rate: a request is the unit an operator reasons about
/// ("this plugin may touch the cache 64 times to render its panel"), and it is
/// the unit `max_concurrency` already bounds, so the two multiply into a rate
/// without either having to measure time.
#[derive(Debug, Clone)]
pub struct QuotaLedger {
    declared: CapabilityQuotas,
    kv_reads: u32,
    kv_writes: u32,
    outbound_calls: u32,
    db_reads: u32,
    db_writes: u32,
    job_enqueues: u32,
    calls: u32,
}

impl QuotaLedger {
    /// A fresh ledger for one request.
    #[must_use]
    pub const fn new(declared: CapabilityQuotas) -> Self {
        Self {
            declared,
            kv_reads: 0,
            kv_writes: 0,
            outbound_calls: 0,
            db_reads: 0,
            db_writes: 0,
            job_enqueues: 0,
            calls: 0,
        }
    }

    /// The ceilings this ledger enforces.
    #[must_use]
    pub const fn declared(&self) -> &CapabilityQuotas {
        &self.declared
    }

    /// Charge one call against both its per-capability counter and the shared
    /// `calls` budget.
    ///
    /// Both are checked before either is committed. Charging one and then
    /// failing the other would spend a per-capability unit on a call that was
    /// refused, so a plugin's second surface would run short because its first
    /// one hit the shared ceiling — a quota that punishes the wrong capability
    /// is one an author cannot act on.
    ///
    /// # Errors
    ///
    /// Names the quota field that is spent.
    pub fn charge(&mut self, call: &CapabilityCall) -> Result<(), &'static str> {
        let (counter, ceiling, field) = match call {
            CapabilityCall::KvGet { .. } => {
                (&mut self.kv_reads, self.declared.kv_reads, "kv_reads")
            }
            CapabilityCall::KvSet { .. } | CapabilityCall::KvDelete { .. } => {
                (&mut self.kv_writes, self.declared.kv_writes, "kv_writes")
            }
            CapabilityCall::HttpFetch { .. } => (
                &mut self.outbound_calls,
                self.declared.outbound_calls,
                "outbound_calls",
            ),
            CapabilityCall::DbGet { .. } | CapabilityCall::DbQuery { .. } => {
                (&mut self.db_reads, self.declared.db_reads, "db_reads")
            }
            CapabilityCall::DbInsert { .. }
            | CapabilityCall::DbUpdate { .. }
            | CapabilityCall::DbDelete { .. } => {
                (&mut self.db_writes, self.declared.db_writes, "db_writes")
            }
            CapabilityCall::JobEnqueue { .. } => (
                &mut self.job_enqueues,
                self.declared.job_enqueues,
                "job_enqueues",
            ),
        };
        if *counter >= ceiling {
            return Err(field);
        }
        if self.calls >= self.declared.calls {
            return Err("calls");
        }
        *counter = counter.saturating_add(1);
        self.calls = self.calls.saturating_add(1);
        Ok(())
    }
}

// ── Rate ─────────────────────────────────────────────────────────────────

/// Calls per second, per (plugin, capability), across requests.
///
/// The counters above bound one request. They do not bound a *rate*: a plugin
/// whose panel is fetched a thousand times a second spends a thousand times its
/// per-request budget, and every one of those calls is legitimate as far as the
/// ledger can see. This is the ceiling on the aggregate, keyed the way the
/// framework's tiered rate limiting is keyed for clients — by the thing being
/// limited rather than by the thing asking.
///
/// One bucket per capability rather than one per plugin, so a plugin that is
/// busy on the cache does not lose its ability to enqueue a job. And one
/// limiter per plugin, so exceeding it "denies the call without affecting other
/// plugins or host routes" the way the acceptance criterion asks: no other
/// plugin shares this state, and no host route consults it.
#[derive(Debug)]
pub struct CapabilityRateLimiter {
    /// One bucket per capability, indexed by `SandboxCapability::ALL`'s order.
    buckets: Vec<Mutex<Bucket>>,
    per_second: u32,
}

#[derive(Debug)]
struct Bucket {
    /// Tokens available, scaled by `SCALE` so a refill smaller than one token
    /// is not lost to integer truncation.
    tokens: u64,
    last: Instant,
}

/// Fixed-point scale for the token count.
const SCALE: u64 = 1_000;

impl CapabilityRateLimiter {
    /// A limiter allowing `per_second` calls per capability, per second.
    ///
    /// The bucket starts full, so a plugin's first request is never throttled
    /// by a limiter that has only just been built.
    #[must_use]
    pub fn new(per_second: u32) -> Self {
        let full = u64::from(per_second).saturating_mul(SCALE);
        Self {
            buckets: SandboxCapability::ALL
                .iter()
                .map(|_| {
                    Mutex::new(Bucket {
                        tokens: full,
                        last: Instant::now(),
                    })
                })
                .collect(),
            per_second,
        }
    }

    /// Take one token for `capability`, refilling for elapsed time first.
    ///
    /// Returns `false` when the bucket is empty, which the caller turns into a
    /// [`quota-exceeded`](super::DenialReason::QuotaExceeded) denial.
    #[must_use]
    pub fn try_take(&self, capability: SandboxCapability) -> bool {
        let Some(index) = SandboxCapability::ALL
            .iter()
            .position(|known| *known == capability)
        else {
            // A capability this build does not know cannot have been granted —
            // `SandboxCapability::parse` refuses the name — so this is
            // unreachable through the manifest. Refusing rather than allowing
            // keeps it fail-closed if it ever becomes reachable.
            return false;
        };
        let Some(bucket) = self.buckets.get(index) else {
            return false;
        };
        // A poisoned lock means another thread panicked while holding it. The
        // bucket is a pair of plain numbers with no invariant a panic could
        // break, so the count is taken rather than the request being failed —
        // and `PoisonError::into_inner` is how that is spelled without
        // `unwrap`, which this module's panic gate forbids.
        let mut bucket = bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let full = u64::from(self.per_second).saturating_mul(SCALE);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.last);
        // `as_micros` rather than `as_secs_f64`: the refill has to be monotone
        // in elapsed time and identical on every platform, and a float divide
        // is neither.
        let refill = u64::try_from(elapsed.as_micros())
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.per_second))
            .saturating_mul(SCALE)
            / 1_000_000;
        bucket.tokens = bucket.tokens.saturating_add(refill).min(full);
        bucket.last = now;
        if bucket.tokens < SCALE {
            return false;
        }
        bucket.tokens = bucket.tokens.saturating_sub(SCALE);
        true
    }
}

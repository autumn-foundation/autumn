//! Replay-side tape for the effect seams outside the request and the database.
//!
//! The database has a wire-level stub server; everything else a request can
//! touch — an outbound HTTP call, a job enqueue, a cache read, a mail send,
//! the resolved tenant — is served from here (#1634). The posture is exactly
//! the database tape's:
//!
//! * a recorded effect is served from the capsule, never performed live;
//! * an effect the capsule cannot answer is a **divergence**, not a fallback
//!   to the real thing;
//! * a recorded effect the replayed run never asked for is a divergence too,
//!   because reaching the recorded outcome without the recorded effects is not
//!   a reproduction.
//!
//! The tape is installed as a **task-local** rather than a process global, the
//! way [`clock::with_replay_request_scope`](crate::capsule::clock) marks the
//! replay request's task. That is what lets several capsules replay
//! concurrently in one `cargo test` process — which is exactly what a
//! committed regression corpus does.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// The tape itself only ever serves a replay, but `current_tape`/`tape_active`
// are probed from the serving path (the HTTP client, job enqueue, the cache,
// the mailer, tenancy), so this module is gated like one.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;

use crate::capsule::schema::{
    CacheEffect, CapsuleBody, CapsuleEffects, HttpEffect, JobEffect, MailEffect,
};

// ── Divergences ─────────────────────────────────────────────────────────────

/// Which effect seam a divergence happened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectSeam {
    /// Outbound HTTP through [`http_client`](crate::http_client).
    Http,
    /// Background job enqueue.
    Job,
    /// Cache read or write.
    Cache,
    /// Mail handed to the mailer.
    Mail,
    /// The tenant context the run resolved.
    Tenant,
}

impl EffectSeam {
    /// Short label used in the human summary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Http => "outbound http",
            Self::Job => "job enqueue",
            Self::Cache => "cache",
            Self::Mail => "mail",
            Self::Tenant => "tenancy",
        }
    }
}

/// Why a replayed effect did not line up with the tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDivergenceKind {
    /// The tape holds no recording of this effect at all.
    Unrecorded,
    /// The next recorded effect on this seam was a different one.
    Mismatch,
    /// The seam ran past the end of its recorded effects.
    Exhausted,
    /// The run finished with recorded effects it never asked for.
    Unconsumed,
}

impl EffectDivergenceKind {
    /// Short label used in the human summary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unrecorded => "unrecorded effect",
            Self::Mismatch => "effect mismatch",
            Self::Exhausted => "tape exhausted",
            Self::Unconsumed => "unconsumed effects",
        }
    }
}

/// One place where a replayed run's effects left the capsule.
#[derive(Debug, Clone, Serialize)]
pub struct EffectDivergence {
    /// Which seam.
    pub seam: EffectSeam,
    /// What went wrong.
    pub kind: EffectDivergenceKind,
    /// Position in the seam's recorded list.
    pub index: usize,
    /// What the tape expected next, when it expected anything.
    pub expected: Option<String>,
    /// What the replayed run actually did.
    pub actual: String,
    /// Human-readable explanation, safe to print.
    pub detail: String,
}

/// What the tape can say about a cache read.
///
/// A named three-state answer rather than a nested `Option`, because the three
/// cases mean genuinely different things to the caller and to the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedValue {
    /// The capsule never saw this key. A divergence has been logged; the read
    /// replays as a miss.
    Unrecorded,
    /// The recording read this key and missed.
    Miss,
    /// The recording read this key and got these bytes.
    Hit(Vec<u8>),
}

/// What the tape can say about a job enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnqueueVerdict {
    /// Matched a recorded enqueue that succeeded; nothing reaches a queue.
    Queued,
    /// Matched a recorded enqueue that the backend **rejected**; the caller
    /// reproduces the error.
    Failed(String),
    /// Did not match the recording. A divergence has been logged.
    Diverged,
}

/// What the tape can say about a mail send.
///
/// Gated with the seam that consumes it: a build without `mail` has no
/// `Mailer::send` to serve, though a capsule recorded by a `mail` build still
/// carries — and still reports — its mail tape.
#[cfg(any(test, feature = "mail"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailVerdict {
    /// Matched a recorded successful send; nothing is delivered.
    Sent,
    /// Matched a recorded send that *failed*; the caller reproduces the error.
    Failed(String),
    /// Did not match the recording. A divergence has been logged.
    Diverged,
}

// ── The tape ────────────────────────────────────────────────────────────────

/// An ordered seam's recorded entries and how far the run has consumed them.
#[derive(Debug)]
struct Ordered<T> {
    pending: VecDeque<T>,
    consumed: usize,
}

impl<T> Ordered<T> {
    fn new(entries: Vec<T>) -> Self {
        Self {
            pending: entries.into(),
            consumed: 0,
        }
    }
}

/// The recorded effects a replayed run is served from.
#[derive(Debug)]
pub struct ReplayEffects {
    http: Mutex<Ordered<HttpEffect>>,
    jobs: Mutex<Ordered<JobEffect>>,
    mail: Mutex<Ordered<MailEffect>>,
    /// Recorded cache *reads*, by key rather than in order: one key is
    /// legitimately read many times in a run, and a recorded read order would
    /// make an innocent extra hit look like a divergence.
    cache: Mutex<BTreeMap<String, CacheSlot>>,
    /// Recorded cache *writes*, in order. Writes are ordered effects like an
    /// enqueue or a send — dropping one, or changing what it stores, is a
    /// behaviour change the capsule can see — so they are consumed and
    /// compared rather than merely applied.
    cache_writes: Mutex<Ordered<CacheWrite>>,
    /// The recorded tenant, and whether the replayed run ever asked for it.
    ///
    /// Tracked like every other seam: a router whose updated code no longer
    /// resolves a tenant would otherwise leave the recording untouched and
    /// still report `Reproduced` on an unchanged response.
    tenant: Option<String>,
    tenant_read: std::sync::atomic::AtomicBool,
    divergences: Mutex<Vec<EffectDivergence>>,
    /// Total effects the tape was built with, so the verdict can say how much
    /// of the recording the run actually met.
    recorded: usize,
    served: AtomicUsize,
    /// Set when the replayed router actually entered the consuming scope.
    ///
    /// `ReportingLayer` establishes that scope, so a router assembled without
    /// it — a hand-built `axum::Router`, or a regression test whose factory
    /// skips `TestApp` — would run with the clock and the entropy source
    /// serving stable non-consuming values and still be able to report
    /// `Reproduced`. The verdict warns instead of pretending.
    scope_entered: std::sync::atomic::AtomicBool,
}

/// One recorded cache write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheWrite {
    key: String,
    /// `None` when the recorded value was not decodable; the key still has to
    /// match.
    value: Option<Vec<u8>>,
}

/// One cache key's recorded value and whether the run has read it.
#[derive(Debug)]
struct CacheSlot {
    /// `None` is a recorded miss (or a hit the backend could not serialize —
    /// see [`ReplayEffects::cache_get`]).
    value: Option<Vec<u8>>,
    /// Whether the recording ever *read* this key, as opposed to only writing
    /// it. Only recorded reads are owed back by the replay.
    was_read: bool,
    /// Whether the replayed run has touched it.
    touched: bool,
}

impl ReplayEffects {
    /// Build the tape a capsule's recorded effects describe.
    #[must_use]
    pub fn new(effects: CapsuleEffects) -> Self {
        // Seeded from the recorded **reads** only, and from the *first* read of
        // each key.
        //
        // Folding writes in as well would break the commonest cache shape
        // there is: a handler that misses, computes, fills, and (later, or on
        // the next request) hits. The recorded `Get{value: None}` and the
        // `Insert{…}` that followed it would collapse into one pre-seeded hit,
        // the replayed handler would take the hit branch its recording never
        // took, and the capsule would report a mismatch caused entirely by
        // replay. A write the replayed run performs is applied by
        // `cache_insert` at the moment it happens, which is what makes
        // fill-then-read-back work with the recorded ordering intact.
        let mut cache: BTreeMap<String, CacheSlot> = BTreeMap::new();
        for entry in &effects.cache {
            if let CacheEffect::Get { key, value } = entry {
                cache.entry(key.clone()).or_insert_with(|| CacheSlot {
                    value: value.as_ref().and_then(|encoded| decode(encoded)),
                    was_read: true,
                    touched: false,
                });
            }
        }
        let cache_writes: Vec<CacheWrite> = effects
            .cache
            .iter()
            .filter_map(|entry| match entry {
                CacheEffect::Insert { key, value } => Some(CacheWrite {
                    key: key.clone(),
                    value: decode(value),
                }),
                CacheEffect::Get { .. } => None,
            })
            .collect();
        let recorded = effects
            .http
            .len()
            .saturating_add(effects.jobs.len())
            .saturating_add(effects.mail.len())
            .saturating_add(cache.len())
            .saturating_add(cache_writes.len());
        Self {
            http: Mutex::new(Ordered::new(effects.http)),
            jobs: Mutex::new(Ordered::new(effects.jobs)),
            mail: Mutex::new(Ordered::new(effects.mail)),
            cache: Mutex::new(cache),
            cache_writes: Mutex::new(Ordered::new(cache_writes)),
            tenant: effects.tenant.and_then(|tenant| tenant.id),
            tenant_read: std::sync::atomic::AtomicBool::new(false),
            divergences: Mutex::new(Vec::new()),
            recorded,
            served: AtomicUsize::new(0),
            scope_entered: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Record that the replayed router entered the consuming scope.
    pub(crate) fn mark_scope_entered(&self) {
        self.scope_entered.store(true, Ordering::SeqCst);
    }

    /// Whether the replayed router entered the consuming scope.
    #[must_use]
    pub fn scope_entered(&self) -> bool {
        self.scope_entered.load(Ordering::SeqCst)
    }

    /// How many recorded effects the tape holds.
    #[must_use]
    pub const fn recorded(&self) -> usize {
        self.recorded
    }

    /// How many of them the replayed run consumed.
    #[must_use]
    pub fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// The divergences observed so far.
    #[must_use]
    pub fn divergences(&self) -> Vec<EffectDivergence> {
        self.divergences
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default()
    }

    fn diverge(&self, divergence: EffectDivergence) {
        if let Ok(mut log) = self.divergences.lock() {
            log.push(divergence);
        }
    }

    /// The response the capsule recorded for the next outbound call, or `None`
    /// when the tape cannot answer it — in which case a divergence has been
    /// logged and the caller must fail the call rather than dial the peer.
    #[must_use]
    pub(crate) fn next_http(&self, method: &str, url: &str) -> Option<HttpEffect> {
        let Ok(mut seam) = self.http.lock() else {
            return None;
        };
        let index = seam.consumed;
        let Some(next) = seam.pending.front() else {
            drop(seam);
            let actual = describe_http(method, url);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Http,
                kind: if self.recorded_http_is_empty() {
                    EffectDivergenceKind::Unrecorded
                } else {
                    EffectDivergenceKind::Exhausted
                },
                index,
                expected: None,
                actual: actual.clone(),
                detail: format!(
                    "the replayed handler made an outbound request the capsule has no \
                     recording for ({actual}); it was refused rather than sent"
                ),
            });
            return None;
        };
        if !next.method.eq_ignore_ascii_case(method) || !matches_redacted(&next.url, url) {
            let expected = describe_http(&next.method, &next.url);
            drop(seam);
            let actual = describe_http(method, url);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Http,
                kind: EffectDivergenceKind::Mismatch,
                index,
                expected: Some(expected.clone()),
                actual: actual.clone(),
                detail: format!(
                    "the replayed handler called {actual}, but the capsule's next recorded \
                     outbound call was {expected}"
                ),
            });
            return None;
        }
        let served = seam.pending.pop_front();
        seam.consumed = seam.consumed.saturating_add(1);
        drop(seam);
        self.served.fetch_add(1, Ordering::SeqCst);
        served
    }

    /// Whether the capsule recorded no outbound calls at all — the difference
    /// between "this run never called out" and "it ran off the end".
    fn recorded_http_is_empty(&self) -> bool {
        self.http
            .lock()
            .is_ok_and(|seam| seam.consumed == 0 && seam.pending.is_empty())
    }

    /// Whether the next recorded enqueue is the one the run just made.
    ///
    /// `true` means the enqueue is satisfied from the tape and must **not** be
    /// written to a queue; `false` means it diverged and the caller should fail
    /// it. `delay_secs` is compared when the caller states one — a job that
    /// used to run immediately and now runs in an hour is a behaviour change
    /// the capsule can see — and ignored when it does not.
    #[must_use]
    pub(crate) fn next_job(
        &self,
        name: &str,
        payload: &serde_json::Value,
        delay_secs: Option<i64>,
    ) -> EnqueueVerdict {
        let Ok(mut seam) = self.jobs.lock() else {
            return EnqueueVerdict::Diverged;
        };
        let index = seam.consumed;
        let Some(next) = seam.pending.front() else {
            drop(seam);
            let actual = describe_job(name, payload);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Job,
                kind: EffectDivergenceKind::Unrecorded,
                index,
                expected: None,
                actual: actual.clone(),
                detail: format!(
                    "the replayed run enqueued a job the capsule has no recording for \
                     ({actual}); nothing was written to a queue"
                ),
            });
            return EnqueueVerdict::Diverged;
        };
        // The delay is compared only when the *caller* stated one. Not every
        // enqueue entry point knows its delay in seconds at the point the guard
        // runs — `enqueue_at` holds an absolute instant and no clock — and
        // comparing a `None` the caller could not supply against a `Some` the
        // recording computed would fail a faithful replay.
        let delay_diverged = delay_secs.is_some_and(|delay| next.delay_secs != Some(delay));
        if next.name != name || delay_diverged || !json_matches_redacted(&next.payload, payload) {
            let expected = describe_job(&next.name, &next.payload);
            drop(seam);
            let actual = describe_job(name, payload);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Job,
                kind: EffectDivergenceKind::Mismatch,
                index,
                expected: Some(expected.clone()),
                actual: actual.clone(),
                detail: format!(
                    "the replayed run enqueued {actual}, but the capsule's next recorded \
                     enqueue was {expected}"
                ),
            });
            return EnqueueVerdict::Diverged;
        }
        let error = seam.pending.pop_front().and_then(|recorded| recorded.error);
        seam.consumed = seam.consumed.saturating_add(1);
        drop(seam);
        self.served.fetch_add(1, Ordering::SeqCst);
        error.map_or(EnqueueVerdict::Queued, EnqueueVerdict::Failed)
    }

    /// What the capsule recorded for a cache key.
    ///
    /// [`CachedValue::Unrecorded`] is a divergence: a replayed run reading a
    /// key the recording never read has taken a different path through the
    /// handler.
    #[must_use]
    pub(crate) fn cache_get(&self, key: &str) -> CachedValue {
        let Ok(mut cache) = self.cache.lock() else {
            return CachedValue::Unrecorded;
        };
        // A cache key is routinely built out of request values, so redaction
        // may have masked part of it on the way to disk; fall back to a
        // placeholder-tolerant scan before calling the read unrecorded.
        let resolved = if cache.contains_key(key) {
            Some(key.to_owned())
        } else {
            // Exactly one match, or none. A masked key like
            // `user:[FILTERED]:profile` matches every subject's entry, and
            // serving the first would hand a handler that read the *wrong*
            // subject's cache entry the recorded value and report no
            // divergence — turning a real bug into a clean reproduction.
            let mut candidates = cache
                .keys()
                .filter(|recorded| matches_redacted(recorded, key));
            match (candidates.next().cloned(), candidates.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        };
        let Some(slot) = resolved.and_then(|recorded| cache.get_mut(&recorded)) else {
            drop(cache);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Cache,
                kind: EffectDivergenceKind::Unrecorded,
                index: 0,
                expected: None,
                actual: key.to_owned(),
                detail: format!(
                    "the replayed run read cache key {key:?}, which the capsule has no \
                     recording for; it was served as a miss"
                ),
            });
            return CachedValue::Unrecorded;
        };
        let first_touch = !slot.touched;
        slot.touched = true;
        let value = slot.value.clone();
        drop(cache);
        if first_touch {
            self.served.fetch_add(1, Ordering::SeqCst);
        }
        value.map_or(CachedValue::Miss, CachedValue::Hit)
    }

    /// Apply a cache write made during replay, and check it against the
    /// recording.
    ///
    /// Two jobs at once. The value lands in the read map so a read-back later
    /// in the same run finds it, exactly as it did in production. And the write
    /// is *consumed* from the recorded write tape and compared — a write the
    /// code dropped, added, or changed the value of is a behaviour change, and
    /// without this it could ride along under an unchanged response and still
    /// report `Reproduced`. Nothing is ever sent to a live backend.
    pub(crate) fn cache_insert(&self, key: &str, value: &[u8]) {
        if let Ok(mut cache) = self.cache.lock() {
            cache
                .entry(key.to_owned())
                .and_modify(|slot| {
                    slot.value = Some(value.to_vec());
                    slot.touched = true;
                })
                .or_insert_with(|| CacheSlot {
                    value: Some(value.to_vec()),
                    was_read: false,
                    touched: true,
                });
        }
        let Ok(mut seam) = self.cache_writes.lock() else {
            return;
        };
        let index = seam.consumed;
        let Some(next) = seam.pending.front() else {
            drop(seam);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Cache,
                kind: EffectDivergenceKind::Unrecorded,
                index,
                expected: None,
                actual: key.to_owned(),
                detail: format!(
                    "the replayed run wrote cache key {key:?}, which the capsule has no \
                     recording for"
                ),
            });
            return;
        };
        // The recorded value may have been masked on its way to disk, so it is
        // compared the way every other redacted recording is.
        let value_matches = next
            .value
            .as_ref()
            .is_none_or(|recorded| bytes_match_redacted(recorded, value));
        if !matches_redacted(&next.key, key) || !value_matches {
            let expected = next.key.clone();
            drop(seam);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Cache,
                kind: EffectDivergenceKind::Mismatch,
                index,
                expected: Some(expected.clone()),
                actual: key.to_owned(),
                detail: format!(
                    "the replayed run wrote cache key {key:?}, but the capsule's next recorded \
                     write was {expected:?}"
                ),
            });
            return;
        }
        seam.pending.pop_front();
        seam.consumed = seam.consumed.saturating_add(1);
        drop(seam);
        self.served.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether the next recorded mail send is the one the run just made, and
    /// the delivery error the recording produced for it.
    ///
    #[cfg(any(test, feature = "mail"))]
    #[must_use]
    pub(crate) fn next_mail(&self, to: &[String], subject: &str) -> MailVerdict {
        let Ok(mut seam) = self.mail.lock() else {
            return MailVerdict::Diverged;
        };
        let index = seam.consumed;
        let Some(next) = seam.pending.front() else {
            drop(seam);
            let actual = describe_mail(to, subject);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Mail,
                kind: EffectDivergenceKind::Unrecorded,
                index,
                expected: None,
                actual: actual.clone(),
                detail: format!(
                    "the replayed run sent mail the capsule has no recording for ({actual}); \
                     nothing was delivered"
                ),
            });
            return MailVerdict::Diverged;
        };
        let recipients_match = next.to.len() == to.len()
            && next
                .to
                .iter()
                .zip(to.iter())
                .all(|(recorded, actual)| matches_redacted(recorded, actual));
        if !recipients_match || !matches_redacted(&next.subject, subject) {
            let expected = describe_mail(&next.to, &next.subject);
            drop(seam);
            let actual = describe_mail(to, subject);
            self.diverge(EffectDivergence {
                seam: EffectSeam::Mail,
                kind: EffectDivergenceKind::Mismatch,
                index,
                expected: Some(expected.clone()),
                actual: actual.clone(),
                detail: format!(
                    "the replayed run sent {actual}, but the capsule's next recorded send was \
                     {expected}"
                ),
            });
            return MailVerdict::Diverged;
        }
        let error = seam.pending.pop_front().and_then(|recorded| recorded.error);
        seam.consumed = seam.consumed.saturating_add(1);
        drop(seam);
        self.served.fetch_add(1, Ordering::SeqCst);
        error.map_or(MailVerdict::Sent, MailVerdict::Failed)
    }

    /// The tenant the recording resolved, when it resolved one.
    #[must_use]
    pub(crate) fn tenant(&self) -> Option<String> {
        if self.tenant.is_some() {
            self.tenant_read.store(true, Ordering::SeqCst);
        }
        self.tenant.clone()
    }

    /// Close the tape and report every recorded effect the run never asked
    /// for, alongside the divergences it already logged.
    #[must_use]
    pub fn finish(&self) -> Vec<EffectDivergence> {
        let mut divergences = self.divergences();
        if let Ok(seam) = self.http.lock()
            && !seam.pending.is_empty()
        {
            divergences.push(unconsumed(
                EffectSeam::Http,
                seam.consumed,
                seam.pending.len(),
                seam.pending
                    .front()
                    .map(|next| format!("{} {}", next.method, next.url)),
            ));
        }
        if let Ok(seam) = self.jobs.lock()
            && !seam.pending.is_empty()
        {
            divergences.push(unconsumed(
                EffectSeam::Job,
                seam.consumed,
                seam.pending.len(),
                seam.pending.front().map(|next| next.name.clone()),
            ));
        }
        if let Ok(seam) = self.mail.lock()
            && !seam.pending.is_empty()
        {
            divergences.push(unconsumed(
                EffectSeam::Mail,
                seam.consumed,
                seam.pending.len(),
                seam.pending.front().map(|next| next.subject.clone()),
            ));
        }
        if self.tenant.is_some() && !self.tenant_read.load(Ordering::SeqCst) {
            divergences.push(unconsumed(
                EffectSeam::Tenant,
                0,
                1,
                self.tenant.clone(),
            ));
        }
        if let Ok(seam) = self.cache_writes.lock()
            && !seam.pending.is_empty()
        {
            divergences.push(unconsumed(
                EffectSeam::Cache,
                seam.consumed,
                seam.pending.len(),
                seam.pending.front().map(|next| next.key.clone()),
            ));
        }
        if let Ok(cache) = self.cache.lock() {
            let missed: Vec<&String> = cache
                .iter()
                .filter(|(_, slot)| slot.was_read && !slot.touched)
                .map(|(key, _)| key)
                .collect();
            if !missed.is_empty() {
                divergences.push(unconsumed(
                    EffectSeam::Cache,
                    0,
                    missed.len(),
                    missed.first().map(|key| (*key).clone()),
                ));
            }
        }
        divergences
    }
}

/// The divergence a seam reports for recorded effects nobody asked for.
fn unconsumed(
    seam: EffectSeam,
    consumed: usize,
    remaining: usize,
    next: Option<String>,
) -> EffectDivergence {
    let label = seam.label();
    let next_text = next.unwrap_or_else(|| "?".to_owned());
    EffectDivergence {
        seam,
        kind: EffectDivergenceKind::Unconsumed,
        index: consumed,
        expected: Some(next_text.clone()),
        actual: String::new(),
        detail: format!(
            "{remaining} recorded {label} effect(s) were never asked for by the replayed run \
             (the next one was {next_text}); reaching the recorded outcome without the \
             recorded effects is not a reproduction"
        ),
    }
}

/// Decode a base64 field, treating an undecodable one as absent.
fn decode(encoded: &str) -> Option<Vec<u8>> {
    STANDARD.decode(encoded.as_bytes()).ok()
}

/// The recorded body of an effect, as the bytes a replayed caller receives.
#[must_use]
pub fn body_bytes(body: &CapsuleBody) -> Vec<u8> {
    match body {
        CapsuleBody::Absent | CapsuleBody::Skipped { .. } => Vec::new(),
        CapsuleBody::Text(text) => text.as_bytes().to_vec(),
        CapsuleBody::Base64(encoded) => decode(encoded).unwrap_or_default(),
    }
}

/// How an outbound call is named in a divergence report.
///
/// The query string is dropped. The `actual` side of a divergence is what the
/// *live* run produced — a real API key, a real token — and the report is
/// printed to a terminal and into CI logs, right beside a recorded side that
/// redaction dutifully masked. Reporting the path is what an operator needs to
/// see; reporting the credential is a leak the capsule format went to some
/// trouble to prevent.
fn describe_http(method: &str, url: &str) -> String {
    let path = url.split_once('?').map_or(url, |(head, _)| head);
    let elided = if path.len() < url.len() { "?…" } else { "" };
    format!("{method} {path}{elided}")
}

/// How a job enqueue is named in a divergence report.
///
/// The job name and the payload's *shape* — its keys — but not its values, for
/// the same reason [`describe_http`] drops the query string.
fn describe_job(name: &str, payload: &serde_json::Value) -> String {
    match payload {
        serde_json::Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            format!("{name} {{{}}}", keys.join(", "))
        }
        serde_json::Value::Null => format!("{name} (null payload)"),
        other => format!("{name} ({} payload)", json_kind(other)),
    }
}

/// The bare type name of a JSON value, for [`describe_job`].
const fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// How a mail send is named in a divergence report.
///
/// Recipients are the PII here, so only their count and domains reach the
/// report — the report is printed to a terminal and to CI logs.
#[cfg(any(test, feature = "mail"))]
fn describe_mail(to: &[String], subject: &str) -> String {
    let domains: Vec<&str> = to
        .iter()
        .map(|address| address.rsplit('@').next().unwrap_or("?"))
        .collect();
    format!(
        "{subject:?} -> {} recipient(s) at {}",
        to.len(),
        domains.join(", ")
    )
}

// ── Matching against redacted recordings ────────────────────────────────────

/// The placeholder redaction leaves behind, treated as a wildcard when the
/// tape is matched against a live call.
const FILTERED: &str = crate::log::filter::FILTERED_PLACEHOLDER;
/// Its percent-encoded spelling, which is what a masked *query* value looks
/// like in a recorded URL.
const FILTERED_URLENCODED: &str = "%5BFILTERED%5D";

/// Whether a recorded string matches what the replayed run actually produced.
///
/// This is the whole reason effect matching is not `==`. The capsule holds the
/// **redacted** spelling — `Bearer [FILTERED]`, `?token=%5BFILTERED%5D`,
/// `{"api_key":"[FILTERED]"}` — while the replayed handler produces the real
/// one, because it reads the real configuration and the real recorded request.
/// Comparing them literally would make every capsule that redacted anything
/// report a divergence on the first outbound call, which is precisely the false
/// "the code changed" verdict the whole design exists to avoid.
///
/// So each placeholder is a wildcard: the literal segments around it must
/// appear, in order, and nothing is asserted about what stood where the
/// placeholder is — the capsule does not carry those bytes, so nothing *can*
/// be asserted about them.
fn matches_redacted(recorded: &str, actual: &str) -> bool {
    if recorded == actual {
        return true;
    }
    let placeholder = if recorded.contains(FILTERED) {
        FILTERED
    } else if recorded.contains(FILTERED_URLENCODED) {
        FILTERED_URLENCODED
    } else {
        // No placeholder and not equal: an ordinary difference.
        return false;
    };
    let mut rest = actual;
    let mut segments = recorded.split(placeholder).peekable();
    let mut first = true;
    while let Some(segment) = segments.next() {
        let last = segments.peek().is_none();
        if first {
            let Some(stripped) = rest.strip_prefix(segment) else {
                return false;
            };
            rest = stripped;
            first = false;
        } else if segment.is_empty() {
            // Back-to-back placeholders, or one at the very end: nothing to
            // anchor on, so whatever remains is accepted.
        } else if let Some(found) = rest.find(segment) {
            let after = found.saturating_add(segment.len());
            rest = rest.get(after..).unwrap_or_default();
        } else {
            return false;
        }
        if last && !segment.is_empty() && !recorded.ends_with(placeholder) {
            // The final literal segment has to be the *tail*, or a recorded
            // `?a=1` would match an actual `?a=1&b=2`.
            return rest.is_empty();
        }
    }
    true
}

/// Whether recorded bytes match what the replayed run produced, tolerating
/// redaction placeholders in a UTF-8 recording.
///
/// Binary values are compared exactly: there is nothing in them for a
/// placeholder to stand in.
fn bytes_match_redacted(recorded: &[u8], actual: &[u8]) -> bool {
    if recorded == actual {
        return true;
    }
    match (std::str::from_utf8(recorded), std::str::from_utf8(actual)) {
        (Ok(recorded), Ok(actual)) => matches_redacted(recorded, actual),
        _ => false,
    }
}

/// Whether a recorded JSON payload matches the one the replayed run produced,
/// with redaction placeholders treated as wildcards.
///
/// Structure is compared exactly — a payload that gained or lost a key *is* a
/// change — but a masked leaf matches whatever stood there, for the same reason
/// [`matches_redacted`] exists.
fn json_matches_redacted(recorded: &serde_json::Value, actual: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (recorded, actual) {
        (Value::String(recorded), Value::String(actual)) => matches_redacted(recorded, actual),
        // A masked value need not have been a string: `{"pin": 1234}` is
        // recorded as `{"pin": "[FILTERED]"}`.
        (Value::String(recorded), _) if recorded == FILTERED => true,
        (Value::Object(recorded), Value::Object(actual)) => {
            recorded.len() == actual.len()
                && recorded.iter().all(|(key, recorded_value)| {
                    actual.get(key).is_some_and(|actual_value| {
                        json_matches_redacted(recorded_value, actual_value)
                    })
                })
        }
        (Value::Array(recorded), Value::Array(actual)) => {
            recorded.len() == actual.len()
                && recorded
                    .iter()
                    .zip(actual.iter())
                    .all(|(recorded, actual)| json_matches_redacted(recorded, actual))
        }
        _ => recorded == actual,
    }
}

// ── Task-local installation ─────────────────────────────────────────────────

tokio::task_local! {
    /// The effect tape serving the replayed run on this task.
    ///
    /// Task-local for the same reason the replay clock's marker is: a
    /// `tokio::spawn`ed side task carried no capture scope when the failure was
    /// recorded, so it must not consume tape entries the recorded handler is
    /// still owed — and, just as importantly, several capsules must be able to
    /// replay concurrently inside one `cargo test` process.
    static REPLAY_EFFECTS: Arc<ReplayEffects>;
}

/// Run `future` with `tape` serving its effect seams.
pub async fn with_effect_tape<F: Future>(tape: Arc<ReplayEffects>, future: F) -> F::Output {
    REPLAY_EFFECTS.scope(tape, future).await
}

/// The effect tape serving the current task, if any.
#[must_use]
pub fn current_tape() -> Option<Arc<ReplayEffects>> {
    REPLAY_EFFECTS.try_with(Arc::clone).ok()
}

/// Whether a tape is serving the current task, without cloning the handle.
///
/// The presence test alone, for callers on the request path that would
/// otherwise pay an atomic refcount pair per request just to ask.
#[must_use]
pub fn tape_active() -> bool {
    REPLAY_EFFECTS
        .try_with(|tape| tape.mark_scope_entered())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use crate::capsule::effects::{
        CachedValue, EffectDivergenceKind, EffectSeam, EnqueueVerdict, MailVerdict, ReplayEffects,
    };
    use crate::capsule::schema::{
        CacheEffect, CapsuleBody, CapsuleEffects, HttpEffect, JobEffect, MailEffect, TenantEffect,
    };

    fn http(method: &str, url: &str, status: u16) -> HttpEffect {
        HttpEffect {
            method: method.to_owned(),
            url: url.to_owned(),
            request_headers: Vec::new(),
            request_body: CapsuleBody::Absent,
            status,
            response_headers: Vec::new(),
            response_body: CapsuleBody::Text("{}".to_owned()),
            error: None,
        }
    }

    #[test]
    fn a_redacted_recording_still_matches_the_call_the_handler_makes() {
        // The capsule holds the *masked* spelling; the replayed handler
        // produces the real one. Comparing literally would report a divergence
        // on the first outbound call of every capsule that redacted anything.
        let tape = ReplayEffects::new(CapsuleEffects {
            http: vec![http(
                "GET",
                "https://api.example/items?key=%5BFILTERED%5D&page=2",
                200,
            )],
            jobs: vec![JobEffect {
                name: "notify".to_owned(),
                payload: serde_json::json!({"to": "a@example.com", "token": "[FILTERED]"}),
                delay_secs: None,
                error: None,
            }],
            ..CapsuleEffects::default()
        });

        assert!(
            tape.next_http("GET", "https://api.example/items?key=sk-live-42&page=2")
                .is_some(),
            "a masked query value must match whatever really stood there"
        );
        assert_eq!(
            tape.next_job(
                "notify",
                &serde_json::json!({"to": "a@example.com", "token": "real-token"}),
                None
            ),
            EnqueueVerdict::Queued
        );
        assert!(tape.divergences().is_empty(), "{:?}", tape.divergences());
    }

    #[test]
    fn a_wildcard_does_not_swallow_a_genuine_difference() {
        let tape = ReplayEffects::new(CapsuleEffects {
            http: vec![http(
                "GET",
                "https://api.example/items?key=%5BFILTERED%5D",
                200,
            )],
            ..CapsuleEffects::default()
        });
        // Same masked prefix, different path: still a divergence.
        assert!(
            tape.next_http("GET", "https://api.example/other?key=sk-live-42")
                .is_none()
        );
        assert_eq!(tape.divergences().len(), 1);
    }

    #[test]
    fn a_masked_json_payload_still_compares_structurally() {
        let tape = ReplayEffects::new(CapsuleEffects {
            jobs: vec![JobEffect {
                name: "notify".to_owned(),
                payload: serde_json::json!({"token": "[FILTERED]"}),
                delay_secs: None,
                error: None,
            }],
            ..CapsuleEffects::default()
        });
        // An extra key is a real change, placeholder or not.
        assert_eq!(
            tape.next_job(
                "notify",
                &serde_json::json!({"token": "real", "extra": 1}),
                None
            ),
            EnqueueVerdict::Diverged
        );
        assert_eq!(tape.divergences().len(), 1);
    }

    #[test]
    fn an_outbound_call_is_served_from_the_tape_in_recorded_order() {
        let tape = ReplayEffects::new(CapsuleEffects {
            http: vec![
                http("GET", "https://a.example/one", 200),
                http("GET", "https://a.example/two", 503),
            ],
            ..CapsuleEffects::default()
        });

        let first = tape
            .next_http("GET", "https://a.example/one")
            .expect("the first recorded exchange serves the first call");
        assert_eq!(first.status, 200);
        let second = tape
            .next_http("GET", "https://a.example/two")
            .expect("the second call takes the second exchange");
        assert_eq!(second.status, 503);
        assert!(tape.divergences().is_empty(), "a faithful run diverges not");
    }

    #[test]
    fn an_outbound_call_the_capsule_never_recorded_is_a_divergence_not_a_live_call() {
        let tape = ReplayEffects::new(CapsuleEffects::default());
        assert!(
            tape.next_http("POST", "https://payments.example/charge")
                .is_none(),
            "an unrecorded call must never fall through to the network"
        );
        let divergences = tape.divergences();
        assert_eq!(divergences.len(), 1);
        assert_eq!(divergences[0].seam, EffectSeam::Http);
        assert!(
            divergences[0].detail.contains("payments.example"),
            "the report must name the call: {}",
            divergences[0].detail
        );
    }

    #[test]
    fn an_outbound_call_to_a_different_url_than_recorded_is_a_divergence() {
        let tape = ReplayEffects::new(CapsuleEffects {
            http: vec![http("GET", "https://a.example/one", 200)],
            ..CapsuleEffects::default()
        });
        assert!(tape.next_http("GET", "https://b.example/other").is_none());
        let divergences = tape.divergences();
        assert_eq!(divergences.len(), 1);
        assert_eq!(
            divergences[0].expected.as_deref(),
            Some("GET https://a.example/one")
        );
    }

    #[test]
    fn a_recorded_call_the_replay_never_made_is_a_divergence_too() {
        // Symmetric with the database tape's `UnconsumedExchanges`: reaching
        // the recorded outcome without the recorded effects is not a
        // reproduction.
        let tape = ReplayEffects::new(CapsuleEffects {
            http: vec![http("GET", "https://a.example/one", 200)],
            ..CapsuleEffects::default()
        });
        let divergences = tape.finish();
        assert_eq!(divergences.len(), 1);
        assert!(
            divergences[0].detail.contains("never"),
            "the report must say the recorded call was never made: {}",
            divergences[0].detail
        );
    }

    #[test]
    fn an_enqueue_is_asserted_against_the_tape_and_never_reaches_a_queue() {
        let tape = ReplayEffects::new(CapsuleEffects {
            jobs: vec![JobEffect {
                name: "send_receipt".to_owned(),
                payload: serde_json::json!({"order": 7}),
                delay_secs: None,
                error: None,
            }],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.next_job("send_receipt", &serde_json::json!({"order": 7}), None),
            EnqueueVerdict::Queued,
            "the recorded enqueue is satisfied from the tape"
        );
        assert!(tape.divergences().is_empty());
        assert_eq!(
            tape.next_job("send_receipt", &serde_json::json!({"order": 8}), None),
            EnqueueVerdict::Diverged,
            "an enqueue the recording never made must not be satisfied"
        );
        assert_eq!(tape.divergences().len(), 1);
    }

    #[test]
    fn a_rescheduled_enqueue_is_a_divergence() {
        // A job that used to run immediately and now runs in an hour is a
        // behaviour change the capsule can see.
        let tape = ReplayEffects::new(CapsuleEffects {
            jobs: vec![JobEffect {
                name: "send_receipt".to_owned(),
                payload: serde_json::json!({}),
                delay_secs: None,
                error: None,
            }],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.next_job("send_receipt", &serde_json::json!({}), Some(3600)),
            EnqueueVerdict::Diverged
        );
        assert_eq!(tape.divergences().len(), 1);
    }

    #[test]
    fn a_recorded_miss_replays_as_a_miss_even_when_the_run_filled_the_key() {
        // The commonest cache shape there is: miss, compute, fill. Folding the
        // recorded write into the initial map would pre-seed a hit and send the
        // replayed handler down a branch the recording never took.
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![
                CacheEffect::Get {
                    key: "widgets".to_owned(),
                    value: None,
                },
                CacheEffect::Insert {
                    key: "widgets".to_owned(),
                    value: base64_of(b"41"),
                },
            ],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.cache_get("widgets"),
            CachedValue::Miss,
            "the recorded miss must replay as a miss"
        );
        tape.cache_insert("widgets", b"41");
        assert_eq!(
            tape.cache_get("widgets"),
            CachedValue::Hit(b"41".to_vec()),
            "the run's own fill is readable back, as it was in production"
        );
        assert!(tape.divergences().is_empty());
    }

    #[test]
    fn an_ambiguous_masked_cache_key_is_treated_as_unrecorded() {
        // `user:[FILTERED]:profile` matches every subject's entry; serving the
        // first would hand a handler that read the *wrong* subject's entry the
        // recorded value and report no divergence.
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![
                CacheEffect::Get {
                    key: "user:[FILTERED]:a".to_owned(),
                    value: Some(base64_of(b"1")),
                },
                CacheEffect::Get {
                    key: "user:[FILTERED]:b".to_owned(),
                    value: Some(base64_of(b"2")),
                },
            ],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.cache_get("user:alice:a"),
            CachedValue::Hit(b"1".to_vec())
        );
        // A key that matches both recorded patterns is refused rather than
        // guessed.
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![
                CacheEffect::Get {
                    key: "user:[FILTERED]".to_owned(),
                    value: Some(base64_of(b"1")),
                },
                CacheEffect::Get {
                    key: "user:[FILTERED]x".to_owned(),
                    value: Some(base64_of(b"2")),
                },
            ],
            ..CapsuleEffects::default()
        });
        assert_eq!(tape.cache_get("user:bobx"), CachedValue::Unrecorded);
        assert_eq!(tape.divergences().len(), 1);
    }

    #[test]
    fn a_cache_read_is_served_by_key_and_an_unrecorded_key_diverges() {
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![
                CacheEffect::Get {
                    key: "user:7".to_owned(),
                    value: Some(base64_of(b"{\"a\":1}")),
                },
                CacheEffect::Get {
                    key: "user:9".to_owned(),
                    value: None,
                },
            ],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.cache_get("user:7"),
            CachedValue::Hit(b"{\"a\":1}".to_vec()),
            "a recorded hit serves its bytes"
        );
        assert_eq!(
            tape.cache_get("user:9"),
            CachedValue::Miss,
            "a recorded miss replays as a miss, not as a divergence"
        );
        assert_eq!(tape.cache_get("user:404"), CachedValue::Unrecorded);
        assert_eq!(tape.divergences().len(), 1, "the unrecorded key diverges");
    }

    #[test]
    fn a_mail_send_is_asserted_and_never_delivered() {
        let tape = ReplayEffects::new(CapsuleEffects {
            mail: vec![MailEffect {
                to: vec!["a@example.com".to_owned()],
                from: None,
                subject: "Receipt".to_owned(),
                body: CapsuleBody::Text("thanks".to_owned()),
                error: None,
            }],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.next_mail(&["a@example.com".to_owned()], "Receipt"),
            MailVerdict::Sent,
            "a recorded success is served as one, and nothing is delivered"
        );
        assert!(tape.divergences().is_empty());
        assert_eq!(
            tape.next_mail(&["b@example.com".to_owned()], "Receipt"),
            MailVerdict::Diverged
        );
        assert_eq!(tape.divergences().len(), 1);
    }

    #[test]
    fn a_recorded_enqueue_failure_is_reproduced_as_a_failure() {
        // A handler whose 500 came from `enqueue(..).await?` must meet the
        // rejection again, not be handed the success it never got.
        let tape = ReplayEffects::new(CapsuleEffects {
            jobs: vec![JobEffect {
                name: "send_receipt".to_owned(),
                payload: serde_json::json!({}),
                delay_secs: None,
                error: Some("queue is down".to_owned()),
            }],
            ..CapsuleEffects::default()
        });
        assert_eq!(
            tape.next_job("send_receipt", &serde_json::json!({}), None),
            EnqueueVerdict::Failed("queue is down".to_owned())
        );
        assert!(tape.divergences().is_empty());
    }

    #[test]
    fn a_cache_write_the_code_dropped_is_a_divergence() {
        // A recorded write nobody performs must not ride along under an
        // unchanged response.
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![CacheEffect::Insert {
                key: "widgets".to_owned(),
                value: base64_of(b"41"),
            }],
            ..CapsuleEffects::default()
        });
        let divergences = tape.finish();
        assert_eq!(divergences.len(), 1, "{divergences:?}");
        assert_eq!(divergences[0].kind, EffectDivergenceKind::Unconsumed);
    }

    #[test]
    fn a_cache_write_with_a_changed_value_is_a_divergence() {
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![CacheEffect::Insert {
                key: "widgets".to_owned(),
                value: base64_of(b"41"),
            }],
            ..CapsuleEffects::default()
        });
        tape.cache_insert("widgets", b"42");
        let divergences = tape.divergences();
        assert_eq!(divergences.len(), 1, "{divergences:?}");
        assert_eq!(divergences[0].kind, EffectDivergenceKind::Mismatch);
    }

    #[test]
    fn a_matching_cache_write_is_consumed_and_readable_back() {
        let tape = ReplayEffects::new(CapsuleEffects {
            cache: vec![CacheEffect::Insert {
                key: "widgets".to_owned(),
                value: base64_of(b"41"),
            }],
            ..CapsuleEffects::default()
        });
        tape.cache_insert("widgets", b"41");
        assert!(tape.finish().is_empty(), "{:?}", tape.finish());
        assert_eq!(
            tape.cache_get("widgets"),
            CachedValue::Hit(b"41".to_vec()),
            "the run's own write is readable back, as it was in production"
        );
    }

    #[test]
    fn a_recorded_tenant_the_replayed_run_never_read_is_a_divergence() {
        // A router whose updated code no longer resolves a tenant would
        // otherwise leave the recording untouched and still report a clean
        // reproduction on an unchanged response.
        let tape = ReplayEffects::new(CapsuleEffects {
            tenant: Some(TenantEffect {
                id: Some("acme".to_owned()),
            }),
            ..CapsuleEffects::default()
        });
        let divergences = tape.finish();
        assert_eq!(divergences.len(), 1, "{divergences:?}");
        assert_eq!(divergences[0].seam, EffectSeam::Tenant);
    }

    #[test]
    fn the_recorded_tenant_is_served_without_consulting_live_config() {
        let tape = ReplayEffects::new(CapsuleEffects {
            tenant: Some(TenantEffect {
                id: Some("acme".to_owned()),
            }),
            ..CapsuleEffects::default()
        });
        assert_eq!(tape.tenant().as_deref(), Some("acme"));
    }

    fn base64_of(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

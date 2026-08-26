//! Bitemporal, tamper-evident record ledger for `#[repository]` writes (issue #1699).
//!
//! Autumn already records raw change history (`crate::version_history`, #700),
//! but a column-level diff log cannot be *queried as state*. This module
//! promotes that history to a ledger: every write to an opted-in entity appends
//! an immutable [`LedgerRevision`] carrying a **full row snapshot**, both time
//! axes (valid time and transaction time), and a hash linking it to the previous
//! revision of the same record.
//!
//! # Opting in
//!
//! ```rust,ignore
//! #[repository(Invoice, soft_delete, ledgered = true)]
//! pub trait InvoiceRepository {}
//! ```
//!
//! That single marker is the only per-model change required. `ledgered = true`
//! implies `versioned = true`, so every write path the version-history feature
//! already covers — hand-written handlers, generated `api = "…"` endpoints,
//! `#[job]`/`#[mailer]` paths, bulk saves, upserts, dependent cascades — appends
//! a revision automatically.
//!
//! # Querying the past
//!
//! ```rust,ignore
//! // Exact state at a past transaction instant.
//! let then = repo.ledger_as_of(id, last_tuesday).await?;
//!
//! // Field-level delta between two instants.
//! let delta = repo.ledger_diff(id, last_tuesday, now).await?;
//!
//! // Prove the stored history was never rewritten.
//! let report = repo.ledger_verify(id).await?;
//! assert!(report.is_intact());
//! ```
//!
//! # Bitemporality
//!
//! Each revision carries two instants:
//!
//! * `recorded_at` — **transaction time**: when the database learned the fact.
//!   Always set by the framework from the write's own clock read.
//! * `valid_from` — **valid time**: when the fact became true in the business
//!   domain. Defaults to `recorded_at`; a model can supply its own via
//!   `ledgered(valid_time = "effective_at")` on the repository.
//!
//! A revision's valid interval is `[valid_from, next_revision.valid_from)` —
//! derived at read time rather than stored, so no revision is ever updated after
//! it is written and the hash chain stays append-only.
//!
//! # Threat model
//!
//! The chain is **tamper-evident**, not tamper-proof.
//! [`verify_chain_against`](crate::ledger::verify_chain_against) — which the
//! generated `ledger_verify` calls — detects any mutation, insertion, deletion,
//! or reordering of stored revisions that does not also re-derive every
//! subsequent hash, plus, by cross-checking the head against the live row, a
//! truncated tail and any write that reached the table without appending a
//! revision.
//!
//! (That link is written out in full because `lib.rs` puts an outer `///`
//! comment on `pub mod ledger;`: rustdoc concatenates it with this header and
//! resolves the whole block from the **crate root**, where only the re-exported
//! types are in scope. A bare `[`verify_chain_against`]` fails
//! `-D rustdoc::broken_intra_doc_links`.)
//!
//! What it cannot see is a *consistent* rewrite: the hashing rule is open
//! source, so an adversary with write access to the ledger table can re-derive a
//! whole chain (and adjust the row to match). Nothing stored inside the same
//! database can prevent that. Pin [`LedgerHead::hash`] somewhere the database
//! cannot reach — an append-only object store, a notary, a second operator's
//! inbox — and a rewritten chain disagrees with the pin.
//!
//! # Fidelity boundary
//!
//! A snapshot goes through the model's durable per-field codec, not
//! `serde_json::to_value`, so it carries every column — `#[private]` and
//! `#[encrypted]` fields included, which serde omits — and reconstruction is
//! byte-for-byte identical to what a live query would have returned at that
//! instant. Encrypted columns are stored as recoverable ciphertext, exactly as
//! the durable commit-hook queue stores them, and come back decrypted.
//!
//! Three consequences worth knowing:
//!
//! * Declaring `#[version_history(sensitive = [...])]` columns on a ledgered
//!   repository is a **compile error** — a redacted column could not be
//!   reconstructed, so the fidelity guarantee would be unprovable.
//! * `ledger_diff` compares the *reconstructed models*, not the stored bytes:
//!   an encrypted column carries a fresh nonce per write, so raw snapshots would
//!   report it as changed on every revision. A column the model hides from
//!   serialization therefore does not appear in a diff, though it is fully
//!   preserved in an as-of reconstruction and fully covered by the hash.
//! * The live-row cross-check in `ledger_verify` compares the same public
//!   projection, for the same reason. A hidden column that drifted out of band
//!   is caught by the chain, not by that cross-check.

use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};

use crate::version_history::{ColumnChange, VersionOp};

/// The ledger table every revision is appended to.
pub const LEDGER_TABLE: &str = "_autumn_ledger_revisions";

/// Domain-separation tag mixed into every revision hash.
///
/// Bumping this invalidates every previously computed hash, so it is part of
/// the on-disk format: change it only alongside a migration that re-chains
/// existing revisions.
const HASH_DOMAIN: &str = "autumn.ledger.revision.v1";

// ── Revision ─────────────────────────────────────────────────────────

/// One immutable revision of a ledgered record.
///
/// Revisions are append-only and numbered per record: `seq` starts at 1 for the
/// insert and increases by exactly one per subsequent write. There is no public
/// API that updates or deletes a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerRevision {
    /// Auto-incrementing primary key in the ledger table.
    pub id: i64,
    /// Table name of the ledgered model.
    pub table_name: String,
    /// Tenant scope, for revisions written by tenant-scoped repositories.
    pub tenant_id: Option<String>,
    /// Primary key of the record this revision belongs to.
    pub record_id: i64,
    /// Position of this revision in the record's chain, starting at 1.
    pub seq: i64,
    /// The mutation that produced this revision.
    pub op: VersionOp,
    /// Authenticated user id, or `"system"` when no session is in scope.
    pub actor: String,
    /// Request / trace correlation id, when one was in scope.
    pub request_id: Option<String>,
    /// Full column values of the record *after* the mutation.
    pub snapshot: serde_json::Value,
    /// Valid time: when the fact became true in the business domain.
    pub valid_from: DateTime<Utc>,
    /// Transaction time: when the database learned the fact.
    pub recorded_at: DateTime<Utc>,
    /// Hash of the previous revision of this record; `None` for `seq == 1`.
    pub prev_hash: Option<String>,
    /// This revision's hash, over its own fields and `prev_hash`.
    pub hash: String,
}

impl LedgerRevision {
    /// Recompute this revision's hash from its stored fields.
    ///
    /// A stored [`hash`](Self::hash) that differs from this value means the row
    /// was mutated out of band after it was written.
    #[must_use]
    pub fn compute_hash(&self) -> String {
        revision_hash(&RevisionHashInput {
            prev_hash: self.prev_hash.as_deref(),
            table_name: &self.table_name,
            tenant_id: self.tenant_id.as_deref(),
            record_id: self.record_id,
            seq: self.seq,
            op: self.op,
            actor: &self.actor,
            request_id: self.request_id.as_deref(),
            snapshot: &self.snapshot,
            valid_from: self.valid_from,
            recorded_at: self.recorded_at,
        })
    }
}

/// The exact field set a revision's hash covers.
///
/// Used both by the write path (which has the values before a row exists) and
/// by [`LedgerRevision::compute_hash`] (which has them after reading one back),
/// so the two can never drift.
#[derive(Debug, Clone, Copy)]
pub struct RevisionHashInput<'a> {
    /// Hash of the previous revision, or `None` at the head of a chain.
    pub prev_hash: Option<&'a str>,
    /// Table name of the ledgered model.
    pub table_name: &'a str,
    /// Tenant scope, when tenant-scoped.
    pub tenant_id: Option<&'a str>,
    /// Primary key of the record.
    pub record_id: i64,
    /// Position in the record's chain, starting at 1.
    pub seq: i64,
    /// The mutation that produced the revision.
    pub op: VersionOp,
    /// Authenticated user id, or `"system"`.
    pub actor: &'a str,
    /// Request / trace correlation id.
    pub request_id: Option<&'a str>,
    /// Full column values after the mutation.
    pub snapshot: &'a serde_json::Value,
    /// Valid time.
    pub valid_from: DateTime<Utc>,
    /// Transaction time.
    pub recorded_at: DateTime<Utc>,
}

// ── Canonicalization & hashing ───────────────────────────────────────

/// Serialize `value` to JSON with every object's keys in sorted order.
///
/// `serde_json::to_string` preserves whatever key order the value carries, which
/// depends on how it was built and on whether `serde_json`'s `preserve_order`
/// feature is on somewhere in the dependency graph. Hashes must not depend on
/// either, so the ledger canonicalizes first: objects are emitted with
/// lexicographically sorted keys, arrays keep their order (order is data), and
/// scalars use `serde_json`'s own compact encoding.
#[must_use]
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (idx, key) in keys.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                // A JSON string literal is exactly what `to_string` on a
                // `Value::String` produces, escapes included.
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push(':');
                // `keys` came from `map`, so every lookup hits.
                if let Some(child) = map.get(key) {
                    write_canonical(child, out);
                }
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Truncate an instant to microsecond precision.
///
/// Both storage tiers keep microseconds: Postgres `TIMESTAMPTZ` truncates
/// anything finer, and the `SQLite` text encoding round-trips six subsecond
/// digits. Hashing a nanosecond-precision instant that the database then stores
/// as microseconds would make every freshly written revision fail
/// [`verify_chain`], so the write path truncates before it both binds and
/// hashes.
#[must_use]
pub fn truncate_to_micros(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(6)
}

/// Compute a revision's hash.
///
/// The preimage is domain-separated and length-prefixed: each field is fed to
/// the digest as its big-endian byte length followed by its bytes, so no two
/// distinct field tuples can produce the same byte stream.
#[must_use]
pub fn revision_hash(input: &RevisionHashInput<'_>) -> String {
    use sha2::{Digest, Sha256};

    fn field(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    }
    // Nullable fields carry a presence tag before their bytes. Without it a
    // `NULL` and an empty string hash identically, so flipping a revision's
    // `tenant_id` from NULL to `''` — which changes which tenant's chain the row
    // belongs to — would leave every stored hash valid.
    fn optional_field(hasher: &mut Sha256, value: Option<&str>) {
        field(hasher, if value.is_some() { b"\x01" } else { b"\x00" });
        field(hasher, value.unwrap_or("").as_bytes());
    }

    let mut hasher = Sha256::new();
    let hasher = &mut hasher;
    field(hasher, HASH_DOMAIN.as_bytes());
    optional_field(hasher, input.prev_hash);
    field(hasher, input.table_name.as_bytes());
    optional_field(hasher, input.tenant_id);
    field(hasher, input.record_id.to_string().as_bytes());
    field(hasher, input.seq.to_string().as_bytes());
    field(hasher, input.op.as_str().as_bytes());
    field(hasher, input.actor.as_bytes());
    optional_field(hasher, input.request_id);
    field(hasher, format_instant(input.valid_from).as_bytes());
    field(hasher, format_instant(input.recorded_at).as_bytes());
    field(hasher, canonical_json(input.snapshot).as_bytes());

    hex::encode(hasher.finalize_reset())
}

/// Render an instant in the fixed encoding the hash preimage uses.
fn format_instant(at: DateTime<Utc>) -> String {
    truncate_to_micros(at).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

// ── Verification ─────────────────────────────────────────────────────

/// How a chain was broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerBreak {
    /// A revision's stored hash does not match its stored fields: the row was
    /// mutated after it was written.
    HashMismatch,
    /// A revision's `prev_hash` does not match the previous revision's hash:
    /// a revision was inserted, replaced, or re-chained.
    PrevHashMismatch,
    /// A sequence number is absent: a revision was deleted.
    MissingRevision,
    /// A sequence number repeats or moves backwards: a revision was inserted.
    DuplicateSeq,
    /// The chain does not start at `seq = 1` with a null `prev_hash`.
    BrokenChainStart,
    /// A sequence number cannot be followed (`seq` is at `i64::MAX`), so the
    /// chain cannot be continued or checked past this point.
    UnusableSeq,
    /// The newest revision does not describe the row the table actually holds.
    ///
    /// The hash chain proves that the revisions *present* were not edited; it
    /// cannot prove that no revision is *missing from the end*, because a
    /// truncated chain is internally consistent. Cross-checking the head against
    /// the live row closes that gap, and with it every write that reached the
    /// table without appending a revision.
    LiveStateMismatch,
}

impl LedgerBreak {
    /// Short, stable identifier for this break kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HashMismatch => "hash_mismatch",
            Self::PrevHashMismatch => "prev_hash_mismatch",
            Self::MissingRevision => "missing_revision",
            Self::DuplicateSeq => "duplicate_seq",
            Self::BrokenChainStart => "broken_chain_start",
            Self::UnusableSeq => "unusable_seq",
            Self::LiveStateMismatch => "live_state_mismatch",
        }
    }
}

impl std::fmt::Display for LedgerBreak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first broken link found in a record's chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerBreakReport {
    /// Sequence number the break was found at. For [`LedgerBreak::MissingRevision`]
    /// this is the *absent* sequence number, not the row that exposed the gap.
    pub seq: i64,
    /// Primary key of the offending ledger row, when one exists. `None` for a
    /// deleted revision.
    pub revision_id: Option<i64>,
    /// What kind of break this is.
    pub kind: LedgerBreak,
    /// Human-readable explanation, safe to surface to an operator.
    pub detail: String,
}

/// Result of verifying one record's revision chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerVerification {
    /// The record whose chain was verified.
    pub record_id: i64,
    /// How many stored revisions were examined.
    pub revisions_checked: usize,
    /// Hash of the last revision, when the chain is intact and non-empty.
    pub head_hash: Option<String>,
    /// The first broken link, or `None` when the chain is intact.
    pub broken: Option<LedgerBreakReport>,
}

impl LedgerVerification {
    /// Whether the chain verified with no broken link.
    #[must_use]
    pub const fn is_intact(&self) -> bool {
        self.broken.is_none()
    }
}

/// The head of a record's chain, for pinning outside the database.
///
/// Recording `hash` somewhere the application database cannot reach turns the
/// tamper-*evidence* this module provides into tamper-*detection* even against
/// an adversary who can rewrite whole chains — a rewritten chain produces a
/// different head hash than the one that was pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerHead {
    /// The record this head belongs to.
    pub record_id: i64,
    /// Sequence number of the newest revision.
    pub seq: i64,
    /// Hash of the newest revision.
    pub hash: String,
    /// Transaction time of the newest revision.
    pub recorded_at: DateTime<Utc>,
}

/// How the live table row compares to the chain's head revision.
///
/// A hash chain proves the revisions that are *present* were not edited. It
/// cannot prove that none is missing from the *end*: lopping the last two
/// revisions off leaves a chain that verifies perfectly. Nor can it see a write
/// that reached the table without appending a revision at all. Comparing the
/// head revision against the row the table actually holds closes both gaps, so
/// [`verify_chain_against`] takes the outcome of that comparison as an input.
///
/// The comparison itself is the caller's, not this module's: deciding which
/// columns are even comparable needs per-table knowledge this layer does not
/// have. The generated `ledger_verify` encodes the head revision and the live
/// row through the *same* projection — the model's durable per-field codec,
/// which carries `#[private]` and `#[encrypted]` columns that the model's public
/// JSON omits — minus the columns encrypted in randomized mode, whose ciphertext
/// carries a fresh nonce per write and so could never compare equal. Everything
/// dropped from that comparison is still covered by the revision hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerLiveState {
    /// The live row was not read, or moved under the reader, so no cross-check
    /// is performed. Chain-internal breaks are still reported.
    NotChecked,
    /// The record has no row in the table.
    Absent,
    /// The live row is the one the head revision describes.
    Matches,
    /// The live row is not the one the head revision describes.
    Diverged,
}

/// Verify one record's revision chain and report the first broken link.
///
/// Equivalent to [`verify_chain_against`] with [`LedgerLiveState::NotChecked`]:
/// it proves the stored revisions were not edited, inserted into, or deleted
/// from the middle, but cannot see a truncated tail or a write that never
/// appended a revision. The generated `ledger_verify` reads the live row and
/// calls [`verify_chain_against`] instead.
#[must_use]
pub fn verify_chain(record_id: i64, revisions: &[LedgerRevision]) -> LedgerVerification {
    verify_chain_against(record_id, revisions, LedgerLiveState::NotChecked)
}

/// Verify one record's revision chain against the row the table holds, and
/// report the first broken link.
///
/// `revisions` must be the record's revisions in ascending `seq` order, as the
/// generated `ledger_verify` reads them. Detects, in `seq` order:
///
/// * a chain that does not start at `seq = 1` with a null `prev_hash`
///   ([`LedgerBreak::BrokenChainStart`], or [`LedgerBreak::MissingRevision`]
///   when the head revisions were deleted);
/// * a gap in the sequence ([`LedgerBreak::MissingRevision`]);
/// * a repeated or backwards sequence number ([`LedgerBreak::DuplicateSeq`]);
/// * a row whose contents no longer hash to its stored hash
///   ([`LedgerBreak::HashMismatch`]);
/// * a row whose `prev_hash` no longer matches its predecessor
///   ([`LedgerBreak::PrevHashMismatch`]);
/// * a `seq` that cannot be followed ([`LedgerBreak::UnusableSeq`]);
///
/// and finally, when `live` is not [`LedgerLiveState::NotChecked`]:
///
/// * a head revision that does not describe the live row
///   ([`LedgerBreak::LiveStateMismatch`]) — a truncated tail, a row erased out
///   of band, or a write that reached the table without appending a revision.
///
/// With `live` [`NotChecked`](LedgerLiveState::NotChecked), an empty slice
/// verifies as intact: a record with no revisions may simply never have been
/// written. With a live row present it does not — a row that exists with no
/// history is evidence its history was erased.
#[must_use]
pub fn verify_chain_against(
    record_id: i64,
    revisions: &[LedgerRevision],
    live: LedgerLiveState,
) -> LedgerVerification {
    let checked = revisions.len();
    let broken = first_break(revisions).or_else(|| live_state_break(revisions, live));
    let head_hash = if broken.is_none() {
        revisions.last().map(|r| r.hash.clone())
    } else {
        None
    };
    LedgerVerification {
        record_id,
        revisions_checked: checked,
        head_hash,
        broken,
    }
}

/// Cross-check the head revision against the live row.
///
/// Runs only after the chain itself verifies, so a mismatch here always means
/// the *end* of the history is missing rather than its middle being edited.
fn live_state_break(
    revisions: &[LedgerRevision],
    live: LedgerLiveState,
) -> Option<LedgerBreakReport> {
    match (live, revisions.last()) {
        (LedgerLiveState::NotChecked | LedgerLiveState::Matches, _)
        | (LedgerLiveState::Absent, None) => None,
        (LedgerLiveState::Absent, Some(head)) => Some(LedgerBreakReport {
            seq: head.seq,
            revision_id: Some(head.id),
            kind: LedgerBreak::LiveStateMismatch,
            detail: format!(
                "the ledger's newest revision is {}, but the record no longer \
                 exists in the table; the row was erased outside the repository",
                head.seq
            ),
        }),
        (LedgerLiveState::Diverged, None) => Some(LedgerBreakReport {
            seq: 0,
            revision_id: None,
            kind: LedgerBreak::LiveStateMismatch,
            detail: "the record exists but its ledger is empty; \
                     every revision of its history was deleted"
                .to_owned(),
        }),
        (LedgerLiveState::Diverged, Some(head)) => Some(LedgerBreakReport {
            seq: head.seq,
            revision_id: Some(head.id),
            kind: LedgerBreak::LiveStateMismatch,
            detail: format!(
                "revision {} is the newest in the ledger but does not describe \
                 the row the table holds; either the tail of the history was \
                 deleted or a write reached the table without appending a revision",
                head.seq
            ),
        }),
    }
}

fn first_break(revisions: &[LedgerRevision]) -> Option<LedgerBreakReport> {
    let mut expected_seq: i64 = 1;
    let mut prev: Option<&LedgerRevision> = None;

    for revision in revisions {
        if revision.seq != expected_seq {
            return Some(sequence_break(revision, expected_seq, prev.is_none()));
        }
        if let Some(report) = link_break(revision, prev) {
            return Some(report);
        }
        let Some(next_seq) = revision.seq.checked_add(1) else {
            // Saturating here would make two consecutive `i64::MAX` rows look
            // contiguous, hiding an inserted revision behind an overflow.
            return Some(LedgerBreakReport {
                seq: revision.seq,
                revision_id: Some(revision.id),
                kind: LedgerBreak::UnusableSeq,
                detail: "sequence number is at i64::MAX and cannot be followed; \
                         the chain cannot be continued or checked past this revision"
                    .to_owned(),
            });
        };
        expected_seq = next_seq;
        prev = Some(revision);
    }
    None
}

/// Classify a sequence number that is not the one the chain expected.
fn sequence_break(
    revision: &LedgerRevision,
    expected_seq: i64,
    at_chain_start: bool,
) -> LedgerBreakReport {
    if revision.seq < expected_seq {
        return LedgerBreakReport {
            seq: revision.seq,
            revision_id: Some(revision.id),
            kind: LedgerBreak::DuplicateSeq,
            detail: format!(
                "revision {} repeats or precedes sequence {expected_seq}; \
                 a revision was inserted into the chain",
                revision.seq
            ),
        };
    }
    let detail = if at_chain_start {
        format!(
            "chain starts at sequence {} instead of 1; revision {expected_seq} was deleted",
            revision.seq
        )
    } else {
        format!(
            "sequence {expected_seq} is absent between {} and {}; a revision was deleted",
            expected_seq.saturating_sub(1),
            revision.seq
        )
    };
    LedgerBreakReport {
        seq: expected_seq,
        revision_id: None,
        kind: LedgerBreak::MissingRevision,
        detail,
    }
}

/// Check one revision's own hash and its link to `prev`.
fn link_break(
    revision: &LedgerRevision,
    prev: Option<&LedgerRevision>,
) -> Option<LedgerBreakReport> {
    if revision.compute_hash() != revision.hash {
        return Some(LedgerBreakReport {
            seq: revision.seq,
            revision_id: Some(revision.id),
            kind: LedgerBreak::HashMismatch,
            detail: format!(
                "revision {} no longer hashes to its stored digest; \
                 the stored revision was mutated out of band",
                revision.seq
            ),
        });
    }

    match prev {
        None => {
            if revision.prev_hash.is_some() {
                return Some(LedgerBreakReport {
                    seq: revision.seq,
                    revision_id: Some(revision.id),
                    kind: LedgerBreak::BrokenChainStart,
                    detail: "the first revision carries a prev_hash; \
                             the revision it chains to is missing"
                        .to_owned(),
                });
            }
        }
        Some(previous) => {
            if revision.prev_hash.as_deref() != Some(previous.hash.as_str()) {
                return Some(LedgerBreakReport {
                    seq: revision.seq,
                    revision_id: Some(revision.id),
                    kind: LedgerBreak::PrevHashMismatch,
                    detail: format!(
                        "revision {}'s prev_hash does not match revision {}'s hash; \
                         the chain was re-linked",
                        revision.seq, previous.seq
                    ),
                });
            }
        }
    }
    None
}

// ── As-of reconstruction ─────────────────────────────────────────────

/// The instants an as-of query resolves against.
///
/// Both axes are optional. `transaction` restricts to what the database knew at
/// that instant; `valid` restricts to what was true in the business domain at
/// that instant. `LedgerAsOf::default()` — both `None` — selects the newest
/// revision by sequence, which is the live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LedgerAsOf {
    /// Transaction-time bound: ignore revisions recorded after this instant.
    pub transaction: Option<DateTime<Utc>>,
    /// Valid-time bound: ignore revisions that became true after this instant.
    pub valid: Option<DateTime<Utc>>,
}

impl LedgerAsOf {
    /// An as-of query on transaction time alone — "what did the database hold
    /// at this instant".
    #[must_use]
    pub const fn transaction(at: DateTime<Utc>) -> Self {
        Self {
            transaction: Some(at),
            valid: None,
        }
    }

    /// An as-of query on valid time alone — "what was true at this instant,
    /// according to everything the database knows now".
    #[must_use]
    pub const fn valid(at: DateTime<Utc>) -> Self {
        Self {
            transaction: None,
            valid: Some(at),
        }
    }

    /// A fully bitemporal as-of query.
    #[must_use]
    pub const fn bitemporal(transaction: DateTime<Utc>, valid: DateTime<Utc>) -> Self {
        Self {
            transaction: Some(transaction),
            valid: Some(valid),
        }
    }
}

/// Select the revision that was in force at `as_of`.
///
/// `revisions` must be in ascending `seq` order. Selection is bitemporal:
///
/// 1. discard revisions recorded after `as_of.transaction` (facts the database
///    did not yet hold);
/// 2. of what remains, discard revisions valid from after `as_of.valid` (facts
///    that were not yet true);
/// 3. return the newest survivor.
///
/// Both bounds **filter**; the winner is always the greatest `seq` among what
/// survives. A revision is a full snapshot of the row, so a later revision does
/// not sit beside an earlier one on a timeline — it replaces it outright. Valid
/// time says when a revision's statement *starts* being true, which is exactly
/// an eligibility question:
///
/// * `LedgerAsOf::transaction(t)` — the newest revision the database held at
///   `t`, i.e. what a plain query returned then.
/// * `LedgerAsOf::valid(v)` — the newest revision whose statement had taken
///   effect by `v`. A future-dated revision is invisible until its instant
///   arrives; a back-dated correction is visible from the instant it claims,
///   and supersedes what it corrects from then on.
/// * `LedgerAsOf::bitemporal(t, v)` — the newest revision the database held at
///   `t` whose statement had taken effect by `v`: what it believed *then* about
///   *then*.
///
/// Ordering by `valid_from` instead would return superseded state whenever a
/// correction moves an instant earlier — the corrected row would keep answering
/// for every instant past the one it used to claim.
///
/// Returns `None` when no revision qualifies, i.e. the record did not exist yet
/// (or, under a valid-time bound, was not yet in effect).
///
/// The returned revision's [`snapshot`](LedgerRevision::snapshot) is the record's
/// exact state at that instant, including a soft-deleted state: a ledgered
/// entity is required to be `soft_delete`, so a delete revision still describes
/// a row that exists. Callers wanting live-only semantics check the model's
/// `deleted_at` exactly as a live query would.
#[must_use]
pub fn snapshot_as_of(revisions: &[LedgerRevision], as_of: LedgerAsOf) -> Option<&LedgerRevision> {
    revisions
        .iter()
        .filter(|r| as_of.transaction.is_none_or(|t| r.recorded_at <= t))
        .filter(|r| as_of.valid.is_none_or(|v| r.valid_from <= v))
        .max_by_key(|r| r.seq)
}

// ── Diffing ──────────────────────────────────────────────────────────

/// The field-level delta of one record between two instants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerDiff {
    /// The record the delta describes.
    pub record_id: i64,
    /// Sequence number in force at the `from` instant, or `None` when the
    /// record did not exist yet.
    pub from_seq: Option<i64>,
    /// Sequence number in force at the `to` instant, or `None` when the record
    /// did not exist yet.
    pub to_seq: Option<i64>,
    /// Changed columns, sorted by column name. Empty when nothing changed.
    pub changes: Vec<ColumnChange>,
}

impl LedgerDiff {
    /// Whether anything changed between the two instants.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Compute the field-level delta between two full snapshots.
///
/// Unlike [`crate::version_history::compute_diff`], which reports the columns of
/// an update changeset, this walks the *union* of both snapshots' keys: a column
/// present only in `before` is reported as removed (`after: None`) and one
/// present only in `after` as added (`before: None`). Output is sorted by column
/// name so a diff is stable regardless of either snapshot's key order.
///
/// A non-object snapshot is treated as an empty object, so diffing against a
/// record that did not yet exist reports every column as added.
#[must_use]
pub fn diff_snapshots(before: &serde_json::Value, after: &serde_json::Value) -> Vec<ColumnChange> {
    let empty = serde_json::Map::new();
    let before_obj = before.as_object().unwrap_or(&empty);
    let after_obj = after.as_object().unwrap_or(&empty);

    let mut columns: Vec<&String> = before_obj.keys().chain(after_obj.keys()).collect();
    columns.sort_unstable();
    columns.dedup();

    columns
        .into_iter()
        .filter_map(|column| {
            let before_val = before_obj.get(column);
            let after_val = after_obj.get(column);
            if before_val == after_val {
                return None;
            }
            Some(ColumnChange::new(
                column.clone(),
                before_val.cloned(),
                after_val.cloned(),
            ))
        })
        .collect()
}

/// Compute the delta of one record between two instants over its revisions.
///
/// `revisions` must be in ascending `seq` order. Each instant is resolved with
/// [`snapshot_as_of`]; a record that did not exist at an instant contributes an
/// empty snapshot, so the delta reports its columns as added or removed.
#[must_use]
pub fn diff_as_of(
    record_id: i64,
    revisions: &[LedgerRevision],
    from: LedgerAsOf,
    to: LedgerAsOf,
) -> LedgerDiff {
    let from_rev = snapshot_as_of(revisions, from);
    let to_rev = snapshot_as_of(revisions, to);
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let before = from_rev.map_or(&empty, |r| &r.snapshot);
    let after = to_rev.map_or(&empty, |r| &r.snapshot);

    LedgerDiff {
        record_id,
        from_seq: from_rev.map(|r| r.seq),
        to_seq: to_rev.map(|r| r.seq),
        changes: diff_snapshots(before, after),
    }
}

// ── Repository-seam guardrails ───────────────────────────────────────

/// Marker proving a model tolerates hard deletion.
///
/// A `ledgered` repository never emits `purge` (the soft-delete hard-delete
/// escape hatch), so calling it on a ledgered repository does not compile.
/// This trait exists so the guarantee is nameable in documentation and in the
/// compile-fail fixtures that pin it.
///
/// It is deliberately unimplementable by hand for a ledgered model: a hard
/// delete erases the row the ledger reconstructs, which no marker can make safe.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is a ledgered entity, so it cannot be hard-deleted",
    label = "hard delete would erase the row this entity's ledger reconstructs",
    note = "ledgered repositories soft-delete: call `delete_by_id` (which records a revision) \
            and `restore` instead of `purge`"
)]
pub trait LedgerHardDeleteAllowed {}

/// A ledgered write that the repository seam refused.
///
/// Every guardrail Autumn can enforce at compile time is enforced there — a
/// `ledgered` repository without `soft_delete` does not compile, and `purge` is
/// not generated for one. This error covers what only the runtime can see.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LedgerError {
    /// A revision could not be appended because the record's chain could not be
    /// read — the ledger table is missing, or its migration has not been applied.
    #[error(
        "ledger chain for {table}#{record_id} is unreadable: {detail}; \
         a ledgered write cannot proceed without appending a revision"
    )]
    ChainUnreadable {
        /// Table of the ledgered model.
        table: String,
        /// Primary key of the record being written.
        record_id: i64,
        /// Underlying reason.
        detail: String,
    },
    /// A hard-deleting parent's `dependent(..., on_delete = destroy)` cascade
    /// reached a ledgered child.
    ///
    /// Neither outcome is available: erasing the child destroys the record its
    /// ledger reconstructs, and soft-deleting it leaves a live foreign key
    /// pointing at a parent row that is about to be deleted, which the database
    /// rejects and rolls back. The parent's macro cannot see that the child is
    /// ledgered — they are separate `#[repository]` invocations — so this is
    /// refused at runtime with a typed error rather than at compile time.
    #[error(
        "cannot cascade a hard delete into ledgered {table}#{record_id}: erasing it \
         would destroy the record its ledger reconstructs, and keeping it would leave \
         a foreign key pointing at a deleted parent. Make the parent repository \
         `soft_delete` (a soft parent delete soft-deletes this child and records a \
         revision), or change the association to `on_delete = nullify`"
    )]
    HardDeleteCascade {
        /// Table of the ledgered child.
        table: String,
        /// Primary key of the child the cascade reached.
        record_id: i64,
    },
    /// A record's stored chain is broken, so its past state cannot be trusted.
    #[error("ledger chain for {table}#{record_id} is broken at revision {seq}: {detail}")]
    ChainBroken {
        /// Table of the ledgered model.
        table: String,
        /// Primary key of the record.
        record_id: i64,
        /// Sequence number of the first broken link.
        seq: i64,
        /// What kind of break, and why.
        detail: String,
    },
}

// ── Model-side trait ─────────────────────────────────────────────────

/// Implemented by models opted into the ledger.
///
/// `#[repository(Model, soft_delete, ledgered = true)]` generates this
/// implementation. It exists to carry the model's valid-time source; everything
/// else the ledger needs comes from [`crate::version_history::VersionedRecord`],
/// which `ledgered` also implies.
pub trait LedgeredRecord: crate::version_history::VersionedRecord {
    /// Valid time for the revision this record is about to produce.
    ///
    /// `None` — the default — means "valid from when the database learned it",
    /// so the write path falls back to the revision's transaction time. Declare
    /// `ledgered(valid_time = "effective_at")` on the repository to read the
    /// instant from a column instead.
    fn ledger_valid_from(&self) -> Option<DateTime<Utc>> {
        None
    }
}

/// Extract a valid-time instant from the column shapes a model may use.
///
/// Generated `LedgeredRecord` impls go through this so a `valid_time` column may
/// be `DateTime<Utc>`, `NaiveDateTime`, or an `Option` of either without the
/// macro having to know which.
#[doc(hidden)]
pub trait LedgerValidTimeValue {
    /// This value as a UTC instant, or `None` when it carries none.
    fn ledger_valid_from(&self) -> Option<DateTime<Utc>>;
}

impl LedgerValidTimeValue for DateTime<Utc> {
    fn ledger_valid_from(&self) -> Option<DateTime<Utc>> {
        Some(*self)
    }
}

impl LedgerValidTimeValue for Option<DateTime<Utc>> {
    fn ledger_valid_from(&self) -> Option<DateTime<Utc>> {
        *self
    }
}

impl LedgerValidTimeValue for chrono::NaiveDateTime {
    fn ledger_valid_from(&self) -> Option<DateTime<Utc>> {
        Some(self.and_utc())
    }
}

impl LedgerValidTimeValue for Option<chrono::NaiveDateTime> {
    fn ledger_valid_from(&self) -> Option<DateTime<Utc>> {
        self.as_ref().map(chrono::NaiveDateTime::and_utc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use serde_json::json;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0)
            .single()
            .expect("valid instant")
    }

    /// Build a well-formed chain of `n` revisions whose snapshots carry an
    /// incrementing `title`, each one hashed and linked to its predecessor.
    fn chain(n: i64) -> Vec<LedgerRevision> {
        let mut out: Vec<LedgerRevision> = Vec::new();
        let mut prev_hash: Option<String> = None;
        for seq in 1..=n {
            let snapshot = json!({ "id": 7, "title": format!("v{seq}"), "deleted_at": null });
            let op = if seq == 1 {
                VersionOp::Insert
            } else {
                VersionOp::Update
            };
            let recorded_at = at(seq * 10);
            let hash = revision_hash(&RevisionHashInput {
                prev_hash: prev_hash.as_deref(),
                table_name: "widgets",
                tenant_id: None,
                record_id: 7,
                seq,
                op,
                actor: "alice",
                request_id: None,
                snapshot: &snapshot,
                valid_from: recorded_at,
                recorded_at,
            });
            out.push(LedgerRevision {
                id: seq,
                table_name: "widgets".to_owned(),
                tenant_id: None,
                record_id: 7,
                seq,
                op,
                actor: "alice".to_owned(),
                request_id: None,
                snapshot,
                valid_from: recorded_at,
                recorded_at,
                prev_hash: prev_hash.clone(),
                hash: hash.clone(),
            });
            prev_hash = Some(hash);
        }
        out
    }

    // ── canonical_json ───────────────────────────────────────────────

    #[test]
    fn canonical_json_sorts_object_keys() {
        let value = json!({ "b": 1, "a": 2, "c": 3 });
        assert_eq!(canonical_json(&value), r#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn canonical_json_is_insertion_order_independent() {
        let mut first = serde_json::Map::new();
        first.insert("z".to_owned(), json!(1));
        first.insert("a".to_owned(), json!(2));
        let mut second = serde_json::Map::new();
        second.insert("a".to_owned(), json!(2));
        second.insert("z".to_owned(), json!(1));

        assert_eq!(
            canonical_json(&serde_json::Value::Object(first)),
            canonical_json(&serde_json::Value::Object(second)),
        );
    }

    #[test]
    fn canonical_json_sorts_nested_objects_and_preserves_array_order() {
        let value = json!({ "outer": { "b": [3, 1, 2], "a": { "y": 1, "x": 2 } } });
        assert_eq!(
            canonical_json(&value),
            r#"{"outer":{"a":{"x":2,"y":1},"b":[3,1,2]}}"#
        );
    }

    #[test]
    fn canonical_json_escapes_keys_and_values() {
        let value = json!({ "a\"b": "c\nd" });
        assert_eq!(canonical_json(&value), r#"{"a\"b":"c\nd"}"#);
    }

    #[test]
    fn canonical_json_handles_scalars_and_nulls() {
        assert_eq!(canonical_json(&json!(null)), "null");
        assert_eq!(canonical_json(&json!(true)), "true");
        assert_eq!(canonical_json(&json!(12)), "12");
        assert_eq!(canonical_json(&json!("s")), "\"s\"");
        assert_eq!(canonical_json(&json!([])), "[]");
        assert_eq!(canonical_json(&json!({})), "{}");
    }

    // ── revision_hash ────────────────────────────────────────────────

    fn base_input(snapshot: &serde_json::Value) -> RevisionHashInput<'_> {
        RevisionHashInput {
            prev_hash: None,
            table_name: "widgets",
            tenant_id: None,
            record_id: 7,
            seq: 1,
            op: VersionOp::Insert,
            actor: "alice",
            request_id: None,
            snapshot,
            valid_from: at(0),
            recorded_at: at(0),
        }
    }

    #[test]
    fn revision_hash_is_deterministic() {
        let snapshot = json!({ "id": 7, "title": "hello" });
        let input = base_input(&snapshot);
        assert_eq!(revision_hash(&input), revision_hash(&input));
    }

    #[test]
    fn revision_hash_is_hex_sha256() {
        let snapshot = json!({});
        let hash = revision_hash(&base_input(&snapshot));
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn revision_hash_ignores_snapshot_key_order() {
        let a = json!({ "id": 7, "title": "hello" });
        let mut map = serde_json::Map::new();
        map.insert("title".to_owned(), json!("hello"));
        map.insert("id".to_owned(), json!(7));
        let b = serde_json::Value::Object(map);

        assert_eq!(
            revision_hash(&base_input(&a)),
            revision_hash(&base_input(&b))
        );
    }

    #[test]
    fn revision_hash_changes_with_every_covered_field() {
        let snapshot = json!({ "id": 7, "title": "hello" });
        let other_snapshot = json!({ "id": 7, "title": "changed" });
        let baseline = revision_hash(&base_input(&snapshot));

        let mutations: Vec<RevisionHashInput<'_>> = vec![
            RevisionHashInput {
                prev_hash: Some("aa"),
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                table_name: "gadgets",
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                tenant_id: Some("t1"),
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                record_id: 8,
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                seq: 2,
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                op: VersionOp::Update,
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                actor: "mallory",
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                request_id: Some("r1"),
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                snapshot: &other_snapshot,
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                valid_from: at(1),
                ..base_input(&snapshot)
            },
            RevisionHashInput {
                recorded_at: at(1),
                ..base_input(&snapshot)
            },
        ];

        for (idx, mutated) in mutations.iter().enumerate() {
            assert_ne!(
                revision_hash(mutated),
                baseline,
                "mutation {idx} must change the hash"
            );
        }
    }

    #[test]
    fn revision_hash_length_prefixes_defeat_field_smuggling() {
        // Without length prefixes, moving a suffix of `actor` onto the front of
        // `request_id` would produce the same byte stream.
        let snapshot = json!({});
        let a = RevisionHashInput {
            actor: "ab",
            request_id: Some("c"),
            ..base_input(&snapshot)
        };
        let b = RevisionHashInput {
            actor: "a",
            request_id: Some("bc"),
            ..base_input(&snapshot)
        };
        assert_ne!(revision_hash(&a), revision_hash(&b));
    }

    #[test]
    fn revision_hash_truncates_sub_microsecond_precision() {
        let snapshot = json!({});
        let micros = Utc.timestamp_nanos(1_800_000_000_000_123_000);
        let nanos = Utc.timestamp_nanos(1_800_000_000_000_123_456);
        let a = RevisionHashInput {
            recorded_at: micros,
            ..base_input(&snapshot)
        };
        let b = RevisionHashInput {
            recorded_at: nanos,
            ..base_input(&snapshot)
        };
        assert_eq!(revision_hash(&a), revision_hash(&b));
    }

    #[test]
    fn compute_hash_matches_the_write_path_hash() {
        let revisions = chain(3);
        for revision in &revisions {
            assert_eq!(revision.compute_hash(), revision.hash);
        }
    }

    // ── verify_chain ─────────────────────────────────────────────────

    #[test]
    fn verify_chain_accepts_an_intact_chain() {
        let revisions = chain(3);
        let report = verify_chain(7, &revisions);
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.revisions_checked, 3);
        assert_eq!(
            report.head_hash.as_deref(),
            Some(revisions[2].hash.as_str())
        );
    }

    #[test]
    fn verify_chain_accepts_an_empty_chain() {
        let report = verify_chain(7, &[]);
        assert!(report.is_intact());
        assert_eq!(report.revisions_checked, 0);
        assert_eq!(report.head_hash, None);
    }

    #[test]
    fn verify_chain_has_no_false_positives_across_repeated_runs() {
        let revisions = chain(12);
        for _ in 0..5 {
            assert!(verify_chain(7, &revisions).is_intact());
        }
    }

    #[test]
    fn verify_chain_detects_a_mutated_revision() {
        let mut revisions = chain(3);
        revisions[1].snapshot = json!({ "id": 7, "title": "tampered", "deleted_at": null });

        let report = verify_chain(7, &revisions);
        let broken = report.broken.expect("mutation must be detected");
        assert_eq!(broken.kind, LedgerBreak::HashMismatch);
        assert_eq!(broken.seq, 2);
        assert_eq!(broken.revision_id, Some(2));
        assert_eq!(report.head_hash, None);
    }

    #[test]
    fn verify_chain_detects_a_re_hashed_mutation_at_the_next_link() {
        let mut revisions = chain(3);
        revisions[1].snapshot = json!({ "id": 7, "title": "tampered", "deleted_at": null });
        revisions[1].hash = revisions[1].compute_hash();

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("a re-hashed mutation must break the next link");
        assert_eq!(broken.kind, LedgerBreak::PrevHashMismatch);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn verify_chain_detects_a_deleted_revision() {
        let mut revisions = chain(4);
        revisions.remove(1); // drop seq 2

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("deletion detected");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 2);
        assert_eq!(broken.revision_id, None);
    }

    #[test]
    fn verify_chain_detects_a_deleted_head_revision() {
        let mut revisions = chain(3);
        revisions.remove(0); // drop seq 1

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("head deletion detected");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 1);
    }

    #[test]
    fn verify_chain_detects_an_inserted_revision() {
        let mut revisions = chain(3);
        let forged = LedgerRevision {
            id: 99,
            seq: 2,
            snapshot: json!({ "id": 7, "title": "forged", "deleted_at": null }),
            ..revisions[1].clone()
        };
        revisions.insert(2, forged); // now seq 1, 2, 2, 3

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("insertion detected");
        assert_eq!(broken.kind, LedgerBreak::DuplicateSeq);
        assert_eq!(broken.seq, 2);
        assert_eq!(broken.revision_id, Some(99));
    }

    #[test]
    fn verify_chain_detects_an_appended_forgery() {
        let mut revisions = chain(3);
        let mut forged = revisions[2].clone();
        forged.id = 99;
        forged.seq = 4;
        forged.snapshot = json!({ "id": 7, "title": "forged", "deleted_at": null });
        revisions.push(forged);

        let broken = verify_chain(7, &revisions).broken.expect("append detected");
        assert_eq!(broken.kind, LedgerBreak::HashMismatch);
        assert_eq!(broken.seq, 4);
    }

    #[test]
    fn verify_chain_detects_a_dangling_chain_start() {
        let mut revisions = chain(2);
        revisions[0].prev_hash = Some("deadbeef".to_owned());
        revisions[0].hash = revisions[0].compute_hash();
        revisions[1].prev_hash = Some(revisions[0].hash.clone());
        revisions[1].hash = revisions[1].compute_hash();

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("dangling start detected");
        assert_eq!(broken.kind, LedgerBreak::BrokenChainStart);
        assert_eq!(broken.seq, 1);
    }

    #[test]
    fn verify_chain_reports_the_first_break_when_several_exist() {
        let mut revisions = chain(4);
        revisions[1].snapshot = json!({ "id": 7, "title": "first tamper" });
        revisions[3].snapshot = json!({ "id": 7, "title": "second tamper" });

        let broken = verify_chain(7, &revisions).broken.expect("break detected");
        assert_eq!(broken.seq, 2, "the earliest break must be reported");
    }

    // ── snapshot_as_of ───────────────────────────────────────────────

    #[test]
    fn snapshot_as_of_returns_none_before_the_first_revision() {
        let revisions = chain(3);
        assert!(snapshot_as_of(&revisions, LedgerAsOf::transaction(at(0))).is_none());
    }

    #[test]
    fn snapshot_as_of_selects_the_revision_in_force() {
        let revisions = chain(3); // recorded at 10, 20, 30
        let picked = snapshot_as_of(&revisions, LedgerAsOf::transaction(at(25)))
            .expect("a revision is in force at t=25");
        assert_eq!(picked.seq, 2);
        assert_eq!(picked.snapshot["title"], json!("v2"));
    }

    #[test]
    fn snapshot_as_of_is_inclusive_of_the_recording_instant() {
        let revisions = chain(3);
        let picked = snapshot_as_of(&revisions, LedgerAsOf::transaction(at(20)))
            .expect("the revision recorded exactly at t=20 is in force");
        assert_eq!(picked.seq, 2);
    }

    #[test]
    fn snapshot_as_of_with_no_bounds_returns_the_head() {
        let revisions = chain(3);
        let picked = snapshot_as_of(&revisions, LedgerAsOf::default()).expect("head");
        assert_eq!(picked.seq, 3);
    }

    #[test]
    fn snapshot_as_of_separates_valid_time_from_transaction_time() {
        // A correction recorded late (transaction time 40) about a fact that
        // became true early (valid time 5).
        let mut revisions = chain(2);
        revisions[1].valid_from = at(5);
        revisions[1].recorded_at = at(40);
        revisions[1].hash = revisions[1].compute_hash();

        // As the database knew it at t=30: only the insert had been recorded.
        let known_then = snapshot_as_of(&revisions, LedgerAsOf::transaction(at(30))).expect("rev");
        assert_eq!(known_then.seq, 1);

        // As the database knows it now, asking what was true at t=6: the
        // back-dated correction wins.
        let true_then = snapshot_as_of(&revisions, LedgerAsOf::valid(at(6))).expect("rev");
        assert_eq!(true_then.seq, 2);

        // Bitemporal: what the database believed at t=30 about t=6. The only
        // revision it held then became valid at t=10, so as far as it knew, the
        // record did not yet exist at t=6.
        assert!(snapshot_as_of(&revisions, LedgerAsOf::bitemporal(at(30), at(6))).is_none());

        // Once the back-dated correction is on record, the same valid-time
        // question gets the corrected answer.
        let corrected =
            snapshot_as_of(&revisions, LedgerAsOf::bitemporal(at(50), at(6))).expect("rev");
        assert_eq!(corrected.seq, 2);

        // And it keeps answering for every later instant: a revision is a full
        // snapshot, so the correction replaced the insert rather than carving a
        // window out of it.
        let later =
            snapshot_as_of(&revisions, LedgerAsOf::bitemporal(at(50), at(20))).expect("rev");
        assert_eq!(later.seq, 2);
    }

    #[test]
    fn snapshot_as_of_hides_a_revision_that_is_not_yet_in_effect() {
        // A future-dated statement: recorded now, true from t=500.
        let mut revisions = chain(1);
        revisions[0].valid_from = at(500);
        revisions[0].hash = revisions[0].compute_hash();

        assert!(snapshot_as_of(&revisions, LedgerAsOf::valid(at(100))).is_none());
        assert_eq!(
            snapshot_as_of(&revisions, LedgerAsOf::valid(at(500)))
                .expect("in effect")
                .seq,
            1
        );
    }

    #[test]
    fn snapshot_as_of_breaks_valid_time_ties_by_sequence() {
        let mut revisions = chain(3);
        for revision in &mut revisions {
            revision.valid_from = at(5);
            revision.hash = revision.compute_hash();
        }
        let picked = snapshot_as_of(&revisions, LedgerAsOf::valid(at(5))).expect("rev");
        assert_eq!(picked.seq, 3, "the latest correction wins a valid-time tie");
    }

    // ── diffing ──────────────────────────────────────────────────────

    #[test]
    fn diff_snapshots_reports_changed_added_and_removed_columns() {
        let before = json!({ "a": 1, "b": 2, "gone": true });
        let after = json!({ "a": 1, "b": 3, "added": "x" });

        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].column, "added");
        assert_eq!(changes[0].before, None);
        assert_eq!(changes[0].after, Some(json!("x")));
        assert_eq!(changes[1].column, "b");
        assert_eq!(changes[1].before, Some(json!(2)));
        assert_eq!(changes[1].after, Some(json!(3)));
        assert_eq!(changes[2].column, "gone");
        assert_eq!(changes[2].before, Some(json!(true)));
        assert_eq!(changes[2].after, None);
    }

    #[test]
    fn diff_snapshots_of_identical_snapshots_is_empty() {
        let value = json!({ "a": 1, "b": [1, 2] });
        assert!(diff_snapshots(&value, &value).is_empty());
    }

    #[test]
    fn diff_snapshots_is_sorted_regardless_of_key_order() {
        let mut before = serde_json::Map::new();
        before.insert("z".to_owned(), json!(1));
        before.insert("a".to_owned(), json!(1));
        let after = json!({ "a": 2, "z": 2 });

        let changes = diff_snapshots(&serde_json::Value::Object(before), &after);
        let columns: Vec<&str> = changes.iter().map(|c| c.column.as_str()).collect();
        assert_eq!(columns, vec!["a", "z"]);
    }

    #[test]
    fn diff_as_of_between_two_instants() {
        let revisions = chain(3);
        let diff = diff_as_of(
            7,
            &revisions,
            LedgerAsOf::transaction(at(15)),
            LedgerAsOf::transaction(at(35)),
        );
        assert_eq!(diff.from_seq, Some(1));
        assert_eq!(diff.to_seq, Some(3));
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].column, "title");
        assert_eq!(diff.changes[0].before, Some(json!("v1")));
        assert_eq!(diff.changes[0].after, Some(json!("v3")));
    }

    #[test]
    fn diff_as_of_before_creation_reports_every_column_as_added() {
        let revisions = chain(2);
        let diff = diff_as_of(
            7,
            &revisions,
            LedgerAsOf::transaction(at(0)),
            LedgerAsOf::transaction(at(15)),
        );
        assert_eq!(diff.from_seq, None);
        assert_eq!(diff.to_seq, Some(1));
        assert_eq!(diff.changes.len(), 3);
        assert!(diff.changes.iter().all(|c| c.before.is_none()));
    }

    #[test]
    fn diff_as_of_over_an_unchanged_window_is_empty() {
        let revisions = chain(3);
        let diff = diff_as_of(
            7,
            &revisions,
            LedgerAsOf::transaction(at(21)),
            LedgerAsOf::transaction(at(29)),
        );
        assert!(diff.is_empty());
        assert_eq!(diff.from_seq, Some(2));
        assert_eq!(diff.to_seq, Some(2));
    }

    // ── live-state cross-check (tail truncation & ledger bypass) ─────

    #[test]
    fn verify_accepts_a_head_that_matches_the_live_row() {
        let report = verify_chain_against(7, &chain(3), LedgerLiveState::Matches);
        assert!(report.is_intact(), "{report:?}");
    }

    #[test]
    fn verify_detects_a_truncated_tail_against_the_live_row() {
        // Revisions 4 and 5 are lopped off. The surviving chain is internally
        // perfect — only the live row exposes the erasure.
        let full = chain(5);
        let truncated = &full[..3];

        assert!(
            verify_chain(7, truncated).is_intact(),
            "a truncated chain is internally consistent by construction"
        );

        let broken = verify_chain_against(7, truncated, LedgerLiveState::Diverged)
            .broken
            .expect("the live row must expose the truncation");
        assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
        assert_eq!(broken.seq, 3, "the break is reported at the surviving head");
    }

    #[test]
    fn verify_detects_a_write_that_never_appended_a_revision() {
        // The table moved on — a restore, a counter-cache bump, a raw UPDATE.
        let broken = verify_chain_against(7, &chain(2), LedgerLiveState::Diverged)
            .broken
            .expect("a ledger-bypassing write must be detected");
        assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
        assert_eq!(broken.seq, 2);
        assert!(
            broken.detail.contains("without appending a revision"),
            "{}",
            broken.detail
        );
    }

    #[test]
    fn verify_detects_a_wholly_erased_chain_behind_a_live_row() {
        let broken = verify_chain_against(7, &[], LedgerLiveState::Diverged)
            .broken
            .expect("a live record with no history must be detected");
        assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
        assert_eq!(broken.seq, 0);
    }

    #[test]
    fn verify_detects_a_row_erased_out_of_band() {
        let revisions = chain(2);
        let broken = verify_chain_against(7, &revisions, LedgerLiveState::Absent)
            .broken
            .expect("a chain whose record no longer exists must be detected");
        assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
        assert_eq!(broken.seq, 2);
    }

    #[test]
    fn verify_accepts_an_absent_record_with_no_revisions() {
        assert!(verify_chain_against(7, &[], LedgerLiveState::Absent).is_intact());
    }

    #[test]
    fn a_chain_internal_break_outranks_a_live_state_mismatch() {
        // Both faults present: the earlier, more specific one is reported.
        let mut revisions = chain(3);
        revisions[1].snapshot = json!({ "id": 7, "title": "tampered" });

        let broken = verify_chain_against(7, &revisions, LedgerLiveState::Diverged)
            .broken
            .expect("break detected");
        assert_eq!(broken.kind, LedgerBreak::HashMismatch);
        assert_eq!(broken.seq, 2);
    }

    #[test]
    fn verify_skips_the_cross_check_when_the_head_moved_under_the_reader() {
        // A concurrent write is not tampering: rather than report a divergence
        // it cannot trust, `ledger_verify` passes `NotChecked`.
        let report = verify_chain_against(7, &chain(2), LedgerLiveState::NotChecked);
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.head_hash.as_deref(), Some(chain(2)[1].hash.as_str()));
    }

    // ── sequence overflow ────────────────────────────────────────────

    #[test]
    fn verify_refuses_to_follow_a_saturated_sequence_number() {
        // Two rows both at i64::MAX would look contiguous under saturating
        // arithmetic, hiding the inserted one.
        let mut revisions = chain(1);
        revisions[0].seq = i64::MAX;
        revisions[0].hash = revisions[0].compute_hash();

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("a chain that starts at i64::MAX cannot be checked");
        // The chain must start at seq 1, so this is caught before the overflow.
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);

        let mut revisions = chain(1);
        revisions[0].seq = 1;
        let mut tail = revisions[0].clone();
        tail.id = 2;
        tail.seq = i64::MAX;
        tail.prev_hash = Some(revisions[0].hash.clone());
        tail.hash = tail.compute_hash();
        revisions.push(tail);
        let broken = verify_chain(7, &revisions).broken.expect("gap detected");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 2);
    }

    // ── preimage: optional fields carry a presence tag ────────────────

    #[test]
    fn revision_hash_distinguishes_absent_from_empty_optionals() {
        let snapshot = json!({});
        for (absent, empty) in [
            (
                RevisionHashInput {
                    tenant_id: None,
                    ..base_input(&snapshot)
                },
                RevisionHashInput {
                    tenant_id: Some(""),
                    ..base_input(&snapshot)
                },
            ),
            (
                RevisionHashInput {
                    request_id: None,
                    ..base_input(&snapshot)
                },
                RevisionHashInput {
                    request_id: Some(""),
                    ..base_input(&snapshot)
                },
            ),
            (
                RevisionHashInput {
                    prev_hash: None,
                    ..base_input(&snapshot)
                },
                RevisionHashInput {
                    prev_hash: Some(""),
                    ..base_input(&snapshot)
                },
            ),
        ] {
            assert_ne!(
                revision_hash(&absent),
                revision_hash(&empty),
                "a NULL optional must not hash like an empty string"
            );
        }
    }

    // ── as-of: transaction-only queries follow the chain, not valid time ──

    #[test]
    fn as_of_transaction_only_returns_the_newest_revision_the_database_held() {
        // A back-dated correction: recorded last (seq 2), but valid from before
        // the insert it corrects. A plain query after both writes returns the
        // correction, so an as-of query with no valid-time bound must too.
        let mut revisions = chain(2);
        revisions[1].valid_from = at(5);
        revisions[1].hash = revisions[1].compute_hash();

        let now = snapshot_as_of(&revisions, LedgerAsOf::transaction(at(100)))
            .expect("a revision is in force");
        assert_eq!(
            now.seq, 2,
            "a transaction-time query must return the latest state, not the \
             one with the greatest valid_from"
        );

        let head = snapshot_as_of(&revisions, LedgerAsOf::default()).expect("head");
        assert_eq!(head.seq, 2, "the unbounded query is the live state");
    }

    #[test]
    fn a_valid_time_bound_filters_eligibility_rather_than_reordering_the_chain() {
        let mut revisions = chain(2);
        revisions[1].valid_from = at(5);
        revisions[1].hash = revisions[1].compute_hash();

        // seq 1 is valid from t=10, seq 2 (a back-dated correction) from t=5.
        // The correction is the newest statement in effect at both instants; it
        // superseded the insert rather than carving a window out of it.
        assert_eq!(
            snapshot_as_of(&revisions, LedgerAsOf::valid(at(6)))
                .expect("rev")
                .seq,
            2
        );
        assert_eq!(
            snapshot_as_of(&revisions, LedgerAsOf::valid(at(20)))
                .expect("rev")
                .seq,
            2
        );
        // Before either statement took effect, there is nothing to return.
        assert!(snapshot_as_of(&revisions, LedgerAsOf::valid(at(1))).is_none());
    }

    // ── misc surface ─────────────────────────────────────────────────

    #[test]
    fn ledger_break_strings_are_stable() {
        assert_eq!(LedgerBreak::HashMismatch.as_str(), "hash_mismatch");
        assert_eq!(LedgerBreak::PrevHashMismatch.as_str(), "prev_hash_mismatch");
        assert_eq!(LedgerBreak::MissingRevision.as_str(), "missing_revision");
        assert_eq!(LedgerBreak::DuplicateSeq.as_str(), "duplicate_seq");
        assert_eq!(LedgerBreak::BrokenChainStart.as_str(), "broken_chain_start");
        assert_eq!(format!("{}", LedgerBreak::HashMismatch), "hash_mismatch");
    }

    #[test]
    fn verification_serde_roundtrip() {
        let report = verify_chain(7, &chain(2));
        let json = serde_json::to_string(&report).expect("serialize");
        let back: LedgerVerification = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
    }

    #[test]
    fn ledger_error_messages_name_the_record() {
        let err = LedgerError::ChainBroken {
            table: "widgets".to_owned(),
            record_id: 7,
            seq: 2,
            detail: "hash_mismatch".to_owned(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("widgets#7"), "{rendered}");
        assert!(rendered.contains("revision 2"), "{rendered}");
    }

    #[test]
    fn valid_time_value_shapes() {
        let instant = at(3);
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&instant),
            Some(instant)
        );
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&Some(instant)),
            Some(instant)
        );
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&None::<DateTime<Utc>>),
            None
        );
        let naive = instant.naive_utc();
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&naive),
            Some(instant)
        );
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&None::<chrono::NaiveDateTime>),
            None
        );
    }
}

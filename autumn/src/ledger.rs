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
//! The chain is **tamper-evident**, not tamper-proof. [`verify_chain`] detects
//! any mutation, insertion, deletion, or reordering of stored revisions that
//! does not also re-derive every subsequent hash. An adversary with write access
//! to the ledger table *and* the framework's hashing rule can rewrite a whole
//! chain; nothing stored inside the same database can prevent that. Pin
//! [`LedgerHead::hash`] somewhere the database cannot reach (an append-only
//! object store, a notary, a second operator's inbox) to close that gap.
//!
//! # Fidelity boundary
//!
//! A snapshot is the model's own serialized column values, so
//! [`snapshot_as_of`] reconstruction is byte-for-byte identical to what a live
//! query would have returned at that instant. Two documented exceptions:
//!
//! * Declaring `#[version_history(sensitive = [...])]` columns on a ledgered
//!   repository is a **compile error** — a redacted column could not be
//!   reconstructed, so the fidelity guarantee would be unprovable.
//! * Columns opted into at-rest encryption (`versioned_ciphertext`, #805) are
//!   snapshotted as ciphertext, exactly as version history stores them. As-of
//!   reconstruction of such a column yields the ciphertext, not the plaintext.

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
    // RED: not implemented yet.
    let _ = value;
    String::new()
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
    // RED: not implemented yet.
    if true {
        let _ = input;
        return String::new();
    }
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    };

    field(HASH_DOMAIN.as_bytes());
    field(input.prev_hash.unwrap_or("").as_bytes());
    field(input.table_name.as_bytes());
    field(input.tenant_id.unwrap_or("").as_bytes());
    field(input.record_id.to_string().as_bytes());
    field(input.seq.to_string().as_bytes());
    field(input.op.as_str().as_bytes());
    field(input.actor.as_bytes());
    field(input.request_id.unwrap_or("").as_bytes());
    field(format_instant(input.valid_from).as_bytes());
    field(format_instant(input.recorded_at).as_bytes());
    field(canonical_json(input.snapshot).as_bytes());

    hex::encode(hasher.finalize())
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

/// Verify one record's revision chain and report the first broken link.
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
///   ([`LedgerBreak::PrevHashMismatch`]).
///
/// An empty slice verifies as intact with no head: a record with no revisions is
/// a record that was never written, which is not evidence of tampering.
#[must_use]
pub fn verify_chain(record_id: i64, revisions: &[LedgerRevision]) -> LedgerVerification {
    // RED: not implemented yet.
    if true {
        return LedgerVerification {
            record_id,
            revisions_checked: revisions.len(),
            head_hash: None,
            broken: None,
        };
    }
    let checked = revisions.len();
    let broken = first_break(revisions);
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
        expected_seq = revision.seq.saturating_add(1);
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
fn link_break(revision: &LedgerRevision, prev: Option<&LedgerRevision>) -> Option<LedgerBreakReport> {
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
/// that instant. `AsOf::default()` — both `None` — selects the latest revision,
/// which is the live state.
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
/// 3. return the survivor with the greatest `valid_from`, breaking ties by the
///    greatest `seq` — the latest correction wins.
///
/// Returns `None` when no revision qualifies, i.e. the record did not exist yet.
///
/// The returned revision's [`snapshot`](LedgerRevision::snapshot) is the record's
/// exact state at that instant, including a soft-deleted state: a ledgered
/// entity is required to be `soft_delete`, so a delete revision still describes
/// a row that exists. Callers wanting live-only semantics check the model's
/// `deleted_at` exactly as a live query would.
#[must_use]
pub fn snapshot_as_of(revisions: &[LedgerRevision], as_of: LedgerAsOf) -> Option<&LedgerRevision> {
    // RED: not implemented yet.
    if true {
        let _ = as_of;
        return None;
    }
    revisions
        .iter()
        .filter(|r| as_of.transaction.is_none_or(|t| r.recorded_at <= t))
        .filter(|r| as_of.valid.is_none_or(|v| r.valid_from <= v))
        .max_by(|a, b| {
            a.valid_from
                .cmp(&b.valid_from)
                .then_with(|| a.seq.cmp(&b.seq))
        })
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
    // RED: not implemented yet.
    if true {
        let _ = (before, after);
        return Vec::new();
    }
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
        Utc.timestamp_opt(1_800_000_000 + secs, 0).single().expect("valid instant")
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

    fn base_input<'a>(snapshot: &'a serde_json::Value) -> RevisionHashInput<'a> {
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

        assert_eq!(revision_hash(&base_input(&a)), revision_hash(&base_input(&b)));
    }

    #[test]
    fn revision_hash_changes_with_every_covered_field() {
        let snapshot = json!({ "id": 7, "title": "hello" });
        let other_snapshot = json!({ "id": 7, "title": "changed" });
        let baseline = revision_hash(&base_input(&snapshot));

        let mutations: Vec<RevisionHashInput<'_>> = vec![
            RevisionHashInput { prev_hash: Some("aa"), ..base_input(&snapshot) },
            RevisionHashInput { table_name: "gadgets", ..base_input(&snapshot) },
            RevisionHashInput { tenant_id: Some("t1"), ..base_input(&snapshot) },
            RevisionHashInput { record_id: 8, ..base_input(&snapshot) },
            RevisionHashInput { seq: 2, ..base_input(&snapshot) },
            RevisionHashInput { op: VersionOp::Update, ..base_input(&snapshot) },
            RevisionHashInput { actor: "mallory", ..base_input(&snapshot) },
            RevisionHashInput { request_id: Some("r1"), ..base_input(&snapshot) },
            RevisionHashInput { snapshot: &other_snapshot, ..base_input(&snapshot) },
            RevisionHashInput { valid_from: at(1), ..base_input(&snapshot) },
            RevisionHashInput { recorded_at: at(1), ..base_input(&snapshot) },
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
        let a = RevisionHashInput { actor: "ab", request_id: Some("c"), ..base_input(&snapshot) };
        let b = RevisionHashInput { actor: "a", request_id: Some("bc"), ..base_input(&snapshot) };
        assert_ne!(revision_hash(&a), revision_hash(&b));
    }

    #[test]
    fn revision_hash_truncates_sub_microsecond_precision() {
        let snapshot = json!({});
        let micros = Utc.timestamp_nanos(1_800_000_000_000_123_000);
        let nanos = Utc.timestamp_nanos(1_800_000_000_000_123_456);
        let a = RevisionHashInput { recorded_at: micros, ..base_input(&snapshot) };
        let b = RevisionHashInput { recorded_at: nanos, ..base_input(&snapshot) };
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
        assert_eq!(report.head_hash.as_deref(), Some(revisions[2].hash.as_str()));
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

        let broken = verify_chain(7, &revisions).broken.expect("deletion detected");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 2);
        assert_eq!(broken.revision_id, None);
    }

    #[test]
    fn verify_chain_detects_a_deleted_head_revision() {
        let mut revisions = chain(3);
        revisions.remove(0); // drop seq 1

        let broken = verify_chain(7, &revisions).broken.expect("head deletion detected");
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

        let broken = verify_chain(7, &revisions).broken.expect("insertion detected");
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

        let broken = verify_chain(7, &revisions).broken.expect("dangling start detected");
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

        // Bitemporal: what the database believed at t=30 about t=6.
        let believed = snapshot_as_of(&revisions, LedgerAsOf::bitemporal(at(30), at(6)))
            .expect("rev");
        assert_eq!(believed.seq, 1);
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
        assert_eq!(LedgerValidTimeValue::ledger_valid_from(&instant), Some(instant));
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&Some(instant)),
            Some(instant)
        );
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&None::<DateTime<Utc>>),
            None
        );
        let naive = instant.naive_utc();
        assert_eq!(LedgerValidTimeValue::ledger_valid_from(&naive), Some(instant));
        assert_eq!(
            LedgerValidTimeValue::ledger_valid_from(&None::<chrono::NaiveDateTime>),
            None
        );
    }
}

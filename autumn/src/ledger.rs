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
//!   Always set by the framework, from the *database's* clock at the point the
//!   append has read the record's chain head, and clamped so it never precedes
//!   the revision before it (#2323).
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
//! [`verify_chain_with_high_water`](crate::ledger::verify_chain_with_high_water)
//! — which the generated `ledger_verify` calls — detects any mutation,
//! insertion, deletion, or reordering of stored revisions that does not also
//! re-derive every subsequent hash, plus a truncated tail and any write that
//! reached the table without appending a revision.
//!
//! (That link is written out in full because `lib.rs` puts an outer `///`
//! comment on `pub mod ledger;`: rustdoc concatenates it with this header and
//! resolves the whole block from the **crate root**, where only the re-exported
//! *types* are in scope — no free function is, which is why the
//! `monotonic_recorded_at` link below is written out too. A bare
//! `[`verify_chain_with_high_water`]` fails
//! `-D rustdoc::broken_intra_doc_links`.)
//!
//! Three checks sit on top of the chain walk, each seeing something the walk
//! cannot:
//!
//! * the **live row**, compared against the head revision, catches a truncated
//!   tail and any write that bypassed the ledger — but only in the window before
//!   the next ordinary write lands;
//! * the **high-water mark** ([`LedgerHighWater`], #2323), which lives outside
//!   the deletable revision rows and never decreases, makes a post-truncation
//!   append allocate past the deleted sequence number instead of re-using it. A
//!   deleted revision therefore leaves a permanent gap rather than a window, and
//!   a wholly erased chain stops looking like a row that predates ledgering. The
//!   mark is cross-checked against the chain in both directions, so rolling it
//!   back, rewriting it or deleting its row is itself reported;
//! * **transaction time** is read from the database and clamped against the
//!   chain's own floor (see
//!   [`monotonic_recorded_at`](crate::ledger::monotonic_recorded_at)), so a
//!   `recorded_at` that moves backwards along a chain is a break rather than a
//!   plausible clock step.
//!
//! What none of them can see is a *consistent* rewrite: the hashing rule is open
//! source, so an adversary with write access to the ledger tables can re-derive
//! a whole chain, adjust the row to match, and re-establish the mark. The mark
//! raises the bar — the same attacker now needs `DELETE` on a second table and
//! has to keep it consistent — but it does not close the class.
//!
//! Nothing stored inside the same database closes that gap. Pin
//! [`LedgerHead::hash`] somewhere the database cannot reach — an append-only
//! object store, a notary, a second operator's inbox — and a rewritten or
//! re-numbered chain disagrees with the pin. For an audit posture that pin is
//! required, not optional.
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
//! * The live-row cross-check in `ledger_verify` compares the durable codec
//!   projection of both sides, decrypted — so `#[private]` and `#[encrypted]`
//!   columns are covered there too. Only a column whose key is gone entirely
//!   drops out of that comparison.

use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};

use crate::version_history::{ColumnChange, VersionOp};

/// The ledger table every revision is appended to.
pub const LEDGER_TABLE: &str = "_autumn_ledger_revisions";

/// How far ahead of the database's own clock a chain's transaction-time floor
/// may legitimately sit before [`monotonic_recorded_at`] refuses to write
/// behind it.
///
/// One hour: comfortably past any real clock disagreement between a database
/// host and its own past self (an NTP correction, a leap-second smear, a
/// virtual-machine migration), and far short of the open-ended jump an
/// out-of-band write to the high-water mark could otherwise ratchet a record's
/// transaction time to.
pub const LEDGER_MAX_CLOCK_SKEW_SECS: i64 = 3_600;

/// The table holding each record's high-water mark (issue #2323).
///
/// A revision's sequence number is allocated from the greater of the chain head
/// and this mark, plus one, and the mark lives *outside* the deletable revision
/// rows — so deleting the newest revision and letting an ordinary write land no
/// longer re-uses the deleted sequence number. See [`LedgerHighWater`].
pub const LEDGER_HIGH_WATER_TABLE: &str = "_autumn_ledger_high_water";

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
///
/// `#[non_exhaustive]` because new detection classes keep arriving as the ledger
/// learns to see more — #2323 alone added four. Adding the attribute is itself a
/// breaking change for an exhaustive downstream `match`, which is why it lands
/// now, while the whole module is still unreleased, rather than after the next
/// variant forces the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
    /// A record's transaction time moves backwards along its chain.
    ///
    /// The write path sources `recorded_at` from the database and clamps it
    /// against the predecessor's, so a regression cannot be produced by a
    /// well-behaved writer on either tier — see [`monotonic_recorded_at`]. One
    /// in stored data is a forgery, or a chain written by a pre-#2323 writer
    /// across a host clock step.
    RecordedAtRegression,
    /// The record has revisions but no [high-water mark](LedgerHighWater).
    ///
    /// The mark is what makes a deleted tail survive the next append, so
    /// deleting the mark row is the obvious way to restore that attack. Its
    /// absence beside a live chain is therefore reported rather than tolerated:
    /// the migration that creates the table backfills a mark for every chain
    /// that already existed, so "revisions but no mark" is never a legitimate
    /// state.
    ///
    /// Unlike [`HighWaterBehind`](Self::HighWaterBehind) this does **not** heal.
    /// The write path refuses to append over a chain whose mark is gone rather
    /// than re-create it, precisely so ordinary traffic cannot launder the
    /// deletion away — so the record's writes fail until an operator has looked
    /// and re-run the mark migration's backfill. That is the intended trade: a
    /// loud stop rather than a quiet repair.
    HighWaterMissing,
    /// The high-water mark is behind the chain's newest revision.
    ///
    /// Either the mark was rolled back, or revisions were appended by something
    /// that did not maintain it. Self-healing: the record's next ledgered write
    /// re-establishes the mark above both.
    HighWaterBehind,
    /// The high-water mark reaches the chain's newest revision but does not
    /// describe it — a different hash, or a different transaction time.
    ///
    /// Either the head revision was replaced by a re-hashed forgery, or the mark
    /// itself was rewritten to match one.
    HighWaterMismatch,
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
            Self::RecordedAtRegression => "recorded_at_regression",
            Self::HighWaterMissing => "high_water_missing",
            Self::HighWaterBehind => "high_water_behind",
            Self::HighWaterMismatch => "high_water_mismatch",
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

/// A record's high-water mark: the highest sequence number its chain has ever
/// reached, kept outside the revision rows (issue #2323).
///
/// A revision's sequence number used to be allocated purely from the rows that
/// survive in `_autumn_ledger_revisions`. Delete the newest revision and let an
/// *ordinary* application write land: the append reads `N-1`, re-allocates `N`,
/// chains onto `N-1`'s hash and matches the live row, so the chain walk and the
/// live-row cross-check both report intact. An attacker who deletes and then
/// waits for ordinary traffic closes the detection window themselves.
///
/// The mark closes that. It lives in [`LEDGER_HIGH_WATER_TABLE`], is never
/// decreased (the writer's upsert refuses to lower it), and the write path
/// allocates one past the greater of the two — so the same attack allocates `N+1`
/// and leaves a permanent gap at `N` that [`verify_chain_with_high_water`]
/// reports as [`LedgerBreak::MissingRevision`].
///
/// It is never *authoritative*, only cross-checked. Verification compares it
/// with the chain in both directions and reports either side disagreeing, so
/// rolling the mark back ([`LedgerBreak::HighWaterBehind`]), rewriting it
/// ([`LedgerBreak::HighWaterMismatch`]) or deleting its row
/// ([`LedgerBreak::HighWaterMissing`]) is itself an accusation. What it does not
/// do is close the class: an adversary with `DELETE` on the revisions table
/// usually has it on this one too, and rewriting both consistently is still
/// possible. Pinning [`LedgerHead::hash`] outside the database remains required
/// for an audit posture.
///
/// `recorded_at` doubles as the floor the next revision's transaction time is
/// clamped against, which is what makes transaction time non-decreasing along a
/// chain even across the gap a deleted revision leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerHighWater {
    /// The record this mark belongs to.
    pub record_id: i64,
    /// Highest sequence number ever allocated for the record.
    pub seq: i64,
    /// Hash of the revision that was written at [`seq`](Self::seq).
    pub hash: String,
    /// Transaction time of the revision that was written at
    /// [`seq`](Self::seq).
    pub recorded_at: DateTime<Utc>,
}

/// A record's chain head and high-water mark, read together (issue #2323).
///
/// The two are what an audit posture pins outside the database, and they are
/// only meaningful as a pair: the head hash is what a rewritten chain
/// disagrees with, and the mark is what proves no revision is missing from the
/// end. Reading them with two separate calls lets an ordinary append land in
/// between, so an auditor can hold a head at sequence `N` beside a mark at
/// `N+1` and read a concurrent write as a truncation. This type exists so the
/// pair always comes from one statement, and one snapshot.
///
/// Either field is `None` for a record that has neither — one never written,
/// or one predating the day its model was ledgered. A `high_water` of `None`
/// beside a `Some` head is the state
/// [`LedgerBreak::HighWaterMissing`] reports; this type does not judge, it
/// reports what is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerPin {
    /// The newest revision's sequence number and hash.
    pub head: Option<LedgerHead>,
    /// The out-of-band high-water mark.
    pub high_water: Option<LedgerHighWater>,
}

/// What a verification knows about a record's
/// [high-water mark](LedgerHighWater).
///
/// Distinguishes "the mark row is gone" from "no mark was read", exactly as
/// [`LedgerLiveState`] distinguishes a missing row from an unread one. The
/// difference matters: an absent mark beside a live chain is an accusation
/// ([`LedgerBreak::HighWaterMissing`]), while an unread one must be silent — the
/// pre-#2323 [`verify_chain_against`] entry point, and the case where a
/// concurrent write moved the chain head under the reader, both pass
/// [`NotChecked`](Self::NotChecked).
///
/// The lifetime is deliberate: [`Present`](Self::Present) borrows rather than
/// owning so verification, which runs on a schedule over whole tables, does not
/// clone a hash per record. Do not "simplify" it to an owned mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerHighWaterState<'a> {
    /// The mark was not read, or moved under the reader, so no cross-check is
    /// performed. Chain-internal breaks are still reported.
    NotChecked,
    /// The record has no mark row. Beside a non-empty chain this is itself a
    /// break: the migration backfills a mark for every chain that already
    /// existed, so the only way to reach this state is to delete one.
    Absent,
    /// The record's mark, as stored.
    Present(&'a LedgerHighWater),
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
/// JSON omits — and decrypts both before comparing. Raw ciphertext is never
/// comparable across two encodings (a fresh nonce per write in randomized mode;
/// a re-encryption under the new key after a rotation), but the plaintext
/// underneath is, so a revision whose only change was to an encrypted column
/// stays visible. Only a column whose key is gone entirely drops out, from both
/// sides, and the revision hash still covers it.
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
/// Equivalent to [`verify_chain_with_high_water`] with no live row and no
/// high-water mark: it proves the stored revisions were not edited, inserted
/// into, or deleted from the middle, and that their transaction time never moves
/// backwards, but cannot see a truncated tail or a write that never appended a
/// revision. The generated `ledger_verify` reads both and calls
/// [`verify_chain_with_high_water`] instead.
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
/// An empty slice always verifies as intact, live row or not: a record may never
/// have been written, or may predate the day its model was ledgered (the
/// migration guide says ledgering is not retroactive). `revisions_checked`
/// reports the emptiness. [`verify_chain_with_high_water`] is what tells that
/// apart from a *wholly erased* chain, whose mark outlives its rows.
#[must_use]
pub fn verify_chain_against(
    record_id: i64,
    revisions: &[LedgerRevision],
    live: LedgerLiveState,
) -> LedgerVerification {
    verify_chain_with_high_water(record_id, revisions, live, LedgerHighWaterState::NotChecked)
}

/// Verify one record's revision chain against the row the table holds **and**
/// its out-of-band [high-water mark](LedgerHighWater), and report the first
/// broken link (issue #2323).
///
/// The superset of [`verify_chain_against`], which is this function with
/// `high_water = None`. The generated `ledger_verify` reads the mark and calls
/// this.
///
/// Breaks are reported in order of how specific they are:
///
/// 1. **chain-internal** — everything [`verify_chain_against`] lists;
/// 2. **high-water** — the mark and the chain disagree:
///    * the mark reaches past the newest surviving revision: the tail was
///      deleted, and the sequence numbers between them are gone
///      ([`LedgerBreak::MissingRevision`], reported at the lowest absent one);
///    * the chain reaches past the mark ([`LedgerBreak::HighWaterBehind`]);
///    * they agree on the sequence number but not on the revision it names
///      ([`LedgerBreak::HighWaterMismatch`]);
///    * the chain is non-empty and there is no mark at all
///      ([`LedgerBreak::HighWaterMissing`]);
/// 3. **live-state** — the head revision does not describe the live row
///    ([`LedgerBreak::LiveStateMismatch`]);
/// 4. **transaction time** — it moves backwards along the chain
///    ([`LedgerBreak::RecordedAtRegression`]). Ranked last on purpose; see
///    ranked last on purpose, so a chain written before #2323 across a host
///    clock step cannot permanently mask a truncation on the same record.
///
/// A truncated tail trips both (2) and (3); (2) is reported because it names the
/// sequence number that is gone, which the live-row comparison cannot.
///
/// An empty chain with no mark still verifies as intact: that is what every
/// existing row looks like on the day a team adopts `ledgered`, and ledgering is
/// not retroactive. An empty chain *with* a mark does not — the mark proves the
/// chain existed, so its erasure is reported. The migration that creates the
/// mark table backfills one for every chain that already existed, so a chain
/// written before #2323 is covered too.
#[must_use]
pub fn verify_chain_with_high_water(
    record_id: i64,
    revisions: &[LedgerRevision],
    live: LedgerLiveState,
    high_water: LedgerHighWaterState<'_>,
) -> LedgerVerification {
    let checked = revisions.len();
    let broken = first_break(revisions)
        .or_else(|| high_water_break(revisions, high_water))
        .or_else(|| live_state_break(revisions, live))
        .or_else(|| recorded_at_break(revisions));
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

/// Cross-check the chain against its out-of-band high-water mark.
///
/// Runs after the chain itself verifies, so the chain side of every comparison
/// here is known-consistent and a disagreement is always about what is *missing*
/// from the end, or about the mark itself.
///
/// Neither source is trusted over the other: a mark ahead of the chain accuses
/// the chain, a mark behind it accuses the mark, and a mark that names a
/// different revision at the same sequence number accuses both without guessing
/// which. That is what keeps the mark from being a second thing to forge
/// silently.
fn high_water_break(
    revisions: &[LedgerRevision],
    high_water: LedgerHighWaterState<'_>,
) -> Option<LedgerBreakReport> {
    let high_water = match high_water {
        // Nothing was read, so there is nothing to disagree with.
        LedgerHighWaterState::NotChecked => return None,
        LedgerHighWaterState::Absent => None,
        LedgerHighWaterState::Present(mark) => Some(mark),
    };
    match (revisions.last(), high_water) {
        // No chain and no mark: a record that was never written, or one that
        // predates the day its model was ledgered. Not an accusation — see
        // `live_state_break` for why that false positive matters.
        (None, None) => None,
        // A mark with no chain at all. The mark is proof the chain existed, so
        // this is the wholesale erasure that was previously indistinguishable
        // from the case above.
        (None, Some(mark)) => Some(LedgerBreakReport {
            seq: 1,
            revision_id: None,
            kind: LedgerBreak::MissingRevision,
            detail: format!(
                "the high-water mark reaches revision {} but the record has no \
                 revisions at all; the whole chain was erased",
                mark.seq
            ),
        }),
        (Some(head), None) => Some(LedgerBreakReport {
            seq: head.seq,
            revision_id: Some(head.id),
            kind: LedgerBreak::HighWaterMissing,
            detail: format!(
                "the chain reaches revision {} but the record has no high-water \
                 mark; the mark that would expose a deleted tail was itself removed",
                head.seq
            ),
        }),
        (Some(head), Some(mark)) => head_versus_mark(head, mark),
    }
}

/// Compare a non-empty chain's head revision with its mark.
fn head_versus_mark(head: &LedgerRevision, mark: &LedgerHighWater) -> Option<LedgerBreakReport> {
    if mark.seq > head.seq {
        // Saturating is safe: `first_break` already refused a chain whose head
        // sits at `i64::MAX`, so `head.seq + 1` cannot wrap here.
        let absent = head.seq.saturating_add(1);
        return Some(LedgerBreakReport {
            seq: absent,
            revision_id: None,
            kind: LedgerBreak::MissingRevision,
            detail: format!(
                "the high-water mark reaches revision {} but the chain stops at {}; \
                 revision {absent} onwards was deleted from the end",
                mark.seq, head.seq
            ),
        });
    }
    if mark.seq < head.seq {
        return Some(LedgerBreakReport {
            seq: head.seq,
            revision_id: Some(head.id),
            kind: LedgerBreak::HighWaterBehind,
            detail: format!(
                "the chain reaches revision {} but its high-water mark stops at {}; \
                 the mark was rolled back, or revisions were appended without it",
                head.seq, mark.seq
            ),
        });
    }
    if mark.hash != head.hash || mark.recorded_at != head.recorded_at {
        return Some(LedgerBreakReport {
            seq: head.seq,
            revision_id: Some(head.id),
            kind: LedgerBreak::HighWaterMismatch,
            detail: format!(
                "the high-water mark names revision {} but does not describe the \
                 revision stored there; the head revision was replaced, or the mark \
                 was rewritten to match a replacement",
                head.seq
            ),
        });
    }
    None
}

/// Cross-check the head revision against the live row.
///
/// Runs only after the chain itself verifies, so a mismatch here always means
/// the *end* of the history is missing rather than its middle being edited.
fn live_state_break(
    revisions: &[LedgerRevision],
    live: LedgerLiveState,
) -> Option<LedgerBreakReport> {
    // An empty chain is never a break, whatever the live row says.
    //
    // A record may simply never have been written — or may predate the day its
    // model was ledgered. Ledgering is non-destructive but not retroactive, so
    // rows written before the marker went on legitimately have no chain until
    // their first subsequent write: that is the documented, expected state of
    // every existing row on the day a team adopts the feature, and accusing it
    // would put a false positive in front of every such deployment, against the
    // one metric this module is held to.
    //
    // A *wholly* erased chain used to be indistinguishable from a pre-ledgering
    // row from inside the database. `high_water_break` tells them apart now
    // (#2323): the mark lives outside the deletable rows and survives the
    // erasure, so a chain that has a mark and no revisions is reported while one
    // with neither stays silent. `revisions_checked == 0` on the report is still
    // what makes the empty case visible to a caller that cares.
    let head = revisions.last()?;

    match live {
        LedgerLiveState::NotChecked | LedgerLiveState::Matches => None,
        LedgerLiveState::Absent => Some(LedgerBreakReport {
            seq: head.seq,
            revision_id: Some(head.id),
            kind: LedgerBreak::LiveStateMismatch,
            detail: format!(
                "the ledger's newest revision is {}, but the record no longer \
                 exists in the table; the row was erased outside the repository",
                head.seq
            ),
        }),
        LedgerLiveState::Diverged => Some(LedgerBreakReport {
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

/// Report the first link along which transaction time moves backwards.
///
/// Reported **last**, after the structural, high-water and live-state checks,
/// and that ordering is deliberate. A chain written by a pre-#2323 writer — from
/// the host's own `Utc::now()`, unclamped — can carry a legitimate regression
/// across ordinary NTP skew between two application nodes, and those rows are
/// immutable, so the report never goes away. Ranked any earlier it would
/// permanently *mask* a truncation or a live-row divergence on the same record,
/// blinding the very checks it ships beside. Ranked last it is what the report
/// says when nothing else is wrong.
///
/// Valid time is deliberately not checked: a back-dated correction is the whole
/// point of the second axis.
fn recorded_at_break(revisions: &[LedgerRevision]) -> Option<LedgerBreakReport> {
    revisions.windows(2).find_map(|pair| {
        let [previous, revision] = pair else {
            return None;
        };
        (revision.recorded_at < previous.recorded_at).then(|| LedgerBreakReport {
            seq: revision.seq,
            revision_id: Some(revision.id),
            kind: LedgerBreak::RecordedAtRegression,
            detail: format!(
                "revision {} was recorded before revision {}, which precedes it in the \
                 chain; transaction time cannot move backwards along a chain a post-#2323 \
                 writer produced",
                revision.seq, previous.seq
            ),
        })
    })
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

/// The transaction time a revision must carry, given the database's own clock
/// read and the chain's current floor (issue #2323).
///
/// `recorded_at` used to be `Utc::now()` on the writing node. Across a
/// multi-node deployment with clock skew, or after a host clock adjustment, a
/// later sequence could carry an earlier instant — and `snapshot_as_of` filters
/// on `recorded_at` before taking the greatest surviving `seq`, so an as-of
/// query could return a revision that was not yet current at the instant asked
/// about. Verification never noticed: it walks the chain in `seq` order.
///
/// Two things fix that together. The write path now reads `db_now` from the
/// database (`clock_timestamp()` on Postgres, `strftime(…, 'now')` on `SQLite`)
/// at the point the append has already read the record's chain head — so the
/// instant reflects database ordering rather than a host clock. And this
/// function clamps it against `floor`, the greater of the predecessor
/// revision's `recorded_at` and the high-water mark's, which makes transaction
/// time non-decreasing along a chain *by construction* rather than by
/// assumption — including across the gap a deleted revision leaves, since the
/// mark outlives the row.
///
/// Both sides are truncated to microseconds first, the precision both storage
/// tiers keep, so the value that enters the hash preimage is exactly the value
/// the database stores.
///
/// The clamp is **bounded**, and that bound is load-bearing. An unbounded
/// `max` would be a one-way ratchet driven by whatever `floor` says: a single
/// out-of-band `UPDATE` pushing a record's floor to the year 2999 would make
/// every subsequent revision carry — and *hash* — an instant in the far future,
/// which `verify` could not object to (the hash would be correct) while
/// [`snapshot_as_of`] silently stopped returning any of them. So a floor more
/// than [`LEDGER_MAX_CLOCK_SKEW_SECS`] ahead of the database's own clock is
/// refused rather than honoured: this returns `None` and the write path turns
/// that into a [`LedgerError::ChainUnreadable`]. Refusing writes something
/// nowhere; honouring it would hash an attacker's instant into the chain and
/// destroy the record's transaction-time history irreversibly.
///
/// What this does not promise: `db_now` is read before the transaction commits,
/// so a revision can still become *visible* slightly after the instant it
/// carries. Closing that would need a commit timestamp, which cannot be read
/// before the insert that has to hash it.
#[must_use]
pub fn monotonic_recorded_at(
    db_now: DateTime<Utc>,
    floor: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let now = truncate_to_micros(db_now);
    let Some(floor) = floor.map(truncate_to_micros) else {
        return Some(now);
    };
    if floor > now + chrono::TimeDelta::seconds(LEDGER_MAX_CLOCK_SKEW_SECS) {
        return None;
    }
    Some(now.max(floor))
}

/// Parse the instant the `SQLite` arm of the write path reads from the database
/// clock.
///
/// `SQLite` has no `clock_timestamp()`; the write path reads
/// `strftime('%Y-%m-%d %H:%M:%f', 'now')`, which renders UTC to millisecond
/// precision (`2026-09-01 18:08:53.597`). `SQLite` documents that every `'now'`
/// inside one `sqlite3_step` sees the same value, so the read is coherent with
/// the chain-head read it rides along with.
///
/// Parsed here rather than bound through `TimestamptzSqlite` so the accepted
/// encoding is pinned by a test in this crate instead of by whichever format
/// list Diesel's `SQLite` timestamp decoder happens to carry.
///
/// Returns `None` for anything that is not that encoding, which the write path
/// turns into a [`LedgerError::ChainUnreadable`] rather than falling back to a
/// host clock: a ledgered write that cannot establish its transaction time must
/// not proceed.
///
/// `pub` only so the generated write path can call it, and `#[doc(hidden)]`
/// accordingly — the same convention as [`LedgerValidTimeValue`].
#[doc(hidden)]
#[must_use]
pub fn parse_sqlite_instant(raw: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
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
    fn verify_does_not_accuse_a_row_that_predates_ledgering() {
        // Every existing row on the day a team adopts `ledgered` is in exactly
        // this state — live, with no chain yet — and the migration guide says so.
        // Reporting it as tampering would be a false positive on an untouched
        // deployment. `revisions_checked` is what tells a caller the chain is
        // empty; `verify_reports_a_wholly_erased_chain_against_its_mark` covers
        // telling this apart from a wholly erased chain.
        let report = verify_chain_against(7, &[], LedgerLiveState::Diverged);
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.revisions_checked, 0);
        assert_eq!(report.head_hash, None);
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

    // ── the shipped migrations ───────────────────────────────────────

    /// The framework's Postgres migrations are duplicated between the
    /// control-plane `migrations/` set and the shard-required
    /// `version_history_migrations/` set, and `__diesel_schema_migrations` keys
    /// on the bare version string — so if the two copies ever drift, whichever
    /// set is applied first silently wins and the other is skipped, leaving a
    /// schema nobody wrote. Nothing enforced that until now; #2323 doubles the
    /// surface, so pin it.
    #[test]
    fn the_ledger_postgres_migrations_are_identical_in_both_sets() {
        for (control, shard_required) in [
            (
                include_str!("../migrations/20260826000000_create_ledger_revisions/up.sql"),
                include_str!(
                    "../version_history_migrations/20260826000000_create_ledger_revisions/up.sql"
                ),
            ),
            (
                include_str!("../migrations/20260826000000_create_ledger_revisions/down.sql"),
                include_str!(
                    "../version_history_migrations/20260826000000_create_ledger_revisions/down.sql"
                ),
            ),
            (
                include_str!("../migrations/20260901213107_create_ledger_high_water/up.sql"),
                include_str!(
                    "../version_history_migrations/20260901213107_create_ledger_high_water/up.sql"
                ),
            ),
            (
                include_str!("../migrations/20260901213107_create_ledger_high_water/down.sql"),
                include_str!(
                    "../version_history_migrations/20260901213107_create_ledger_high_water/down.sql"
                ),
            ),
        ] {
            assert_eq!(
                control, shard_required,
                "the control-plane and shard-required copies of a ledger migration must \
                 stay byte-identical; they share a version string, so a drift is silent"
            );
        }
    }

    /// Both tiers must agree on the table and column names the write path binds
    /// against, whatever their type spellings.
    #[test]
    fn both_tiers_declare_the_same_high_water_columns() {
        let pg = include_str!(
            "../version_history_migrations/20260901213107_create_ledger_high_water/up.sql"
        );
        let sqlite = include_str!(
            "../version_history_migrations_sqlite/20260901213107_create_ledger_high_water/up.sql"
        );
        for fragment in [
            LEDGER_HIGH_WATER_TABLE,
            "table_name",
            "tenant_key",
            "record_id",
            "high_seq",
            "head_hash",
            "recorded_at",
            "PRIMARY KEY (table_name, tenant_key, record_id)",
        ] {
            assert!(pg.contains(fragment), "postgres migration lacks {fragment}");
            assert!(
                sqlite.contains(fragment),
                "sqlite migration lacks {fragment}"
            );
        }
    }

    // ── high-water mark (#2323): truncation survives the next append ──

    /// The mark a well-behaved writer would have left for `revisions`.
    fn mark_for(revisions: &[LedgerRevision]) -> LedgerHighWater {
        let head = revisions.last().expect("a non-empty chain");
        LedgerHighWater {
            record_id: head.record_id,
            seq: head.seq,
            hash: head.hash.clone(),
            recorded_at: head.recorded_at,
        }
    }

    #[test]
    fn verify_accepts_a_chain_its_high_water_mark_agrees_with() {
        let revisions = chain(3);
        let report = verify_chain_with_high_water(
            7,
            &revisions,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark_for(&revisions)),
        );
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(
            report.head_hash.as_deref(),
            Some(revisions[2].hash.as_str())
        );
    }

    #[test]
    fn verify_reports_a_truncated_tail_the_high_water_mark_remembers() {
        // The #2318 live-row cross-check only sees this in the window before the
        // next write. The mark sees it whenever `verify` runs.
        let full = chain(3);
        let mark = mark_for(&full);
        let truncated = &full[..2];

        let broken = verify_chain_with_high_water(
            7,
            truncated,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("the mark must expose the deleted head revision");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(
            broken.seq, 3,
            "the absent sequence number, not the survivor"
        );
        assert_eq!(broken.revision_id, None);
    }

    #[test]
    fn verify_reports_the_gap_a_post_truncation_append_leaves() {
        // The issue's headline case. Revision 3 is deleted, then an ordinary
        // write lands: because the writer allocates `max(head, mark) + 1` it
        // takes 4, not 3, so the evidence survives the append.
        let full = chain(4);
        let mut surviving: Vec<LedgerRevision> = full[..2].to_vec();
        surviving.push(full[3].clone());

        let broken = verify_chain_with_high_water(
            7,
            &surviving,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark_for(&surviving)),
        )
        .broken
        .expect("the gap left by the re-numbered append must be reported");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn verify_reports_a_wholly_erased_chain_against_its_mark() {
        // Previously indistinguishable from a row that predates ledgering — the
        // limitation `verify_does_not_accuse_a_row_that_predates_ledgering`
        // documents. The mark tells the two apart.
        let mark = mark_for(&chain(3));
        let broken = verify_chain_with_high_water(
            7,
            &[],
            LedgerLiveState::Absent,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("an erased chain whose mark survives must be reported");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 1);
    }

    #[test]
    fn verify_still_accepts_a_row_that_predates_ledgering() {
        // No revisions and no mark: exactly what every existing row looks like
        // on the day a team adopts `ledgered`. Still not an accusation.
        let report = verify_chain_with_high_water(
            7,
            &[],
            LedgerLiveState::Diverged,
            LedgerHighWaterState::Absent,
        );
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.revisions_checked, 0);
    }

    #[test]
    fn verify_reports_a_deleted_high_water_row() {
        // Deleting the mark is the obvious way to restore the original attack,
        // so its absence beside a live chain is itself the accusation.
        let broken = verify_chain_with_high_water(
            7,
            &chain(3),
            LedgerLiveState::Matches,
            LedgerHighWaterState::Absent,
        )
        .broken
        .expect("a chain with no mark at all must be reported");
        assert_eq!(broken.kind, LedgerBreak::HighWaterMissing);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn verify_reports_a_high_water_mark_rolled_backwards() {
        let revisions = chain(3);
        let mut mark = mark_for(&revisions);
        mark.seq = 1;
        mark.hash = revisions[0].hash.clone();
        mark.recorded_at = revisions[0].recorded_at;

        let broken = verify_chain_with_high_water(
            7,
            &revisions,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("a rolled-back mark must be reported");
        assert_eq!(broken.kind, LedgerBreak::HighWaterBehind);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn verify_reports_a_rewritten_high_water_hash() {
        let revisions = chain(3);
        let mut mark = mark_for(&revisions);
        mark.hash = "0".repeat(64);

        let broken = verify_chain_with_high_water(
            7,
            &revisions,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("a mark that no longer names the head revision must be reported");
        assert_eq!(broken.kind, LedgerBreak::HighWaterMismatch);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn verify_reports_a_high_water_mark_whose_instant_was_rewritten() {
        // `recorded_at` on the mark is the floor the next write clamps against,
        // so moving it is a way to steer future transaction times.
        let revisions = chain(3);
        let mut mark = mark_for(&revisions);
        mark.recorded_at = at(9_999);

        let broken = verify_chain_with_high_water(
            7,
            &revisions,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("a mark whose instant disagrees with the head must be reported");
        assert_eq!(broken.kind, LedgerBreak::HighWaterMismatch);
    }

    #[test]
    fn a_chain_internal_break_outranks_a_high_water_break() {
        let mut revisions = chain(3);
        let mark = mark_for(&revisions);
        revisions[1].snapshot = json!({ "id": 7, "title": "tampered" });

        let broken = verify_chain_with_high_water(
            7,
            &revisions,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("break detected");
        assert_eq!(broken.kind, LedgerBreak::HashMismatch);
        assert_eq!(broken.seq, 2);
    }

    #[test]
    fn a_high_water_break_outranks_a_live_state_mismatch() {
        // A truncated tail trips both. `MissingRevision` names the sequence
        // number that is gone, which `LiveStateMismatch` cannot.
        let full = chain(3);
        let mark = mark_for(&full);
        let broken = verify_chain_with_high_water(
            7,
            &full[..2],
            LedgerLiveState::Diverged,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("break detected");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn verify_chain_against_still_checks_without_a_mark() {
        // The three-argument entry point stays exactly as it was: no mark, no
        // high-water reporting, so a caller that has not migrated is unaffected.
        let report = verify_chain_against(7, &chain(3), LedgerLiveState::Matches);
        assert!(report.is_intact(), "{report:?}");
    }

    // ── transaction time is non-decreasing along a chain (#2323) ──────

    #[test]
    fn verify_reports_a_transaction_time_that_moves_backwards() {
        // Sourcing `recorded_at` from the database and clamping it against the
        // predecessor makes a regression impossible by construction, so one in
        // stored data is either a forgery or a chain written by a pre-#2323
        // writer across a clock step.
        let mut revisions = chain(3);
        revisions[2].recorded_at = revisions[1].recorded_at - chrono::Duration::seconds(1);
        revisions[2].hash = revisions[2].compute_hash();

        let broken = verify_chain(7, &revisions)
            .broken
            .expect("a backwards transaction time must be reported");
        assert_eq!(broken.kind, LedgerBreak::RecordedAtRegression);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn a_transaction_time_regression_never_masks_a_real_break() {
        // A chain written before #2323 can carry a legitimate regression across
        // NTP skew between two application nodes, and its rows are immutable — so
        // if the regression outranked the other checks it would blind them on
        // that record forever. Both faults present: the truncation is reported.
        let full = chain(4);
        let mark = mark_for(&full);
        let mut truncated = full[..3].to_vec();
        truncated[2].recorded_at = truncated[1].recorded_at - chrono::Duration::seconds(1);
        truncated[2].hash = truncated[2].compute_hash();
        // Re-link so only the regression and the truncation remain.
        let mut fixed = truncated.clone();
        fixed[2].prev_hash = Some(fixed[1].hash.clone());
        fixed[2].hash = fixed[2].compute_hash();

        let broken = verify_chain_with_high_water(
            7,
            &fixed,
            LedgerLiveState::Matches,
            LedgerHighWaterState::Present(&mark),
        )
        .broken
        .expect("break detected");
        assert_eq!(
            broken.kind,
            LedgerBreak::MissingRevision,
            "the deleted revision outranks the backwards clock: {broken:?}"
        );
        assert_eq!(broken.seq, 4);

        // With nothing else wrong, the regression is what the report says.
        let broken = verify_chain(7, &fixed)
            .broken
            .expect("the regression is still reported on its own");
        assert_eq!(broken.kind, LedgerBreak::RecordedAtRegression);
        assert_eq!(broken.seq, 3);
    }

    #[test]
    fn equal_transaction_times_are_not_a_regression() {
        // Two writes inside one database clock tick are legitimate: the
        // guarantee is non-decreasing, not strictly increasing.
        let mut revisions = chain(3);
        revisions[2].recorded_at = revisions[1].recorded_at;
        revisions[2].hash = revisions[2].compute_hash();

        assert!(verify_chain(7, &revisions).is_intact());
    }

    #[test]
    fn valid_time_may_move_backwards_without_being_a_break() {
        // Valid time is a business claim: a back-dated correction is the whole
        // point of the second axis.
        let mut revisions = chain(3);
        revisions[2].valid_from = revisions[0].valid_from - chrono::Duration::days(30);
        revisions[2].hash = revisions[2].compute_hash();

        assert!(verify_chain(7, &revisions).is_intact());
    }

    #[test]
    fn monotonic_recorded_at_never_precedes_its_floor() {
        let now = at(100);
        assert_eq!(monotonic_recorded_at(now, None), Some(now));
        assert_eq!(monotonic_recorded_at(now, Some(at(50))), Some(now));
        assert_eq!(monotonic_recorded_at(now, Some(at(150))), Some(at(150)));
        assert_eq!(monotonic_recorded_at(now, Some(now)), Some(now));
    }

    #[test]
    fn monotonic_recorded_at_truncates_both_sides_to_micros() {
        // The value that is hashed has to be the value the database stores, and
        // both tiers keep microseconds.
        let now = at(100) + chrono::Duration::nanoseconds(1_500);
        assert_eq!(
            monotonic_recorded_at(now, None),
            Some(at(100) + chrono::Duration::microseconds(1))
        );
        let floor = at(200) + chrono::Duration::nanoseconds(1_500);
        assert_eq!(
            monotonic_recorded_at(at(100), Some(floor)),
            Some(at(200) + chrono::Duration::microseconds(1))
        );
    }

    #[test]
    fn monotonic_recorded_at_refuses_a_floor_far_ahead_of_the_database_clock() {
        // The high-water mark's instant carries no hash of its own, so an
        // out-of-band UPDATE could otherwise ratchet a record's transaction time
        // arbitrarily far forward — every later revision would hash a future
        // instant correctly while `snapshot_as_of` quietly stopped returning it.
        let now = at(0);
        let inside = now + chrono::Duration::seconds(LEDGER_MAX_CLOCK_SKEW_SECS);
        assert_eq!(monotonic_recorded_at(now, Some(inside)), Some(inside));

        let outside = inside + chrono::Duration::microseconds(1);
        assert_eq!(
            monotonic_recorded_at(now, Some(outside)),
            None,
            "a floor past the tolerated skew must refuse the write, not honour it"
        );
        assert_eq!(
            monotonic_recorded_at(now, Some(now + chrono::Duration::days(365_000))),
            None
        );
    }

    #[test]
    fn sqlite_instants_round_trip_from_the_database_clock() {
        // `strftime('%Y-%m-%d %H:%M:%f', 'now')` — what the SQLite arm of the
        // write path reads, to millisecond precision.
        let parsed = parse_sqlite_instant("2026-09-01 18:08:53.597")
            .expect("the documented SQLite clock encoding parses");
        assert_eq!(
            parsed.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            "2026-09-01T18:08:53.597000Z"
        );

        // Whole seconds carry no fractional part at all.
        assert_eq!(
            parse_sqlite_instant("2026-09-01 18:08:53")
                .expect("a fractionless instant parses")
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            "2026-09-01T18:08:53.000000Z"
        );

        assert_eq!(parse_sqlite_instant("not an instant"), None);
        assert_eq!(parse_sqlite_instant(""), None);
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

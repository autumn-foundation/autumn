//! Server side of the sync protocol: backends + mountable router.
//!
//! Mount on the remote Autumn app:
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use autumn_web::sync::{LwwResolver, PgSyncBackend, server};
//!
//! # async fn wire(database_url: String) {
//! let backend = PgSyncBackend::new(database_url);
//! backend.ensure_schema().expect("sync schema");
//! autumn_web::app()
//!     .nest("/sync", server::router(Arc::new(backend), Arc::new(LwwResolver)))
//!     .run()
//!     .await;
//! # }
//! ```

// autumn-panic-gate: request-path module — production code path must be panic-free.
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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{FromRequestParts, OptionalFromRequestParts, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{Array, BigInt, Bool, Jsonb, Nullable, Text, Timestamptz};

use super::SyncError;
use super::protocol::{
    Change, ChangeOutcome, MAX_PULL_LIMIT, MAX_PUSH_CHANGES, Op, PullQuery, PullResponse,
    PushRequest, PushResponse, RemoteRow, Version,
};
use super::resolver::{ConflictResolver, Resolution};

fn backend_err(err: impl std::fmt::Display) -> SyncError {
    SyncError::Backend(err.to_string())
}

/// The tenant/principal scope key partitioning synced data server-side.
///
/// Every synced row, tombstone, and push-dedup record belongs to exactly
/// one scope: pushes write into the scope derived from the request, pulls
/// return only that scope's rows, and the same `(collection, pk)` in two
/// scopes is two **distinct** rows. Without scoping, every authenticated
/// device would read and write one shared data set.
///
/// The scope is **never client-supplied** — the wire protocol
/// ([`PushRequest`], [`PullQuery`]) carries no scope field, so a client
/// cannot name another tenant's partition. It is derived server-side from
/// the authenticated request: authentication middleware inserts a
/// `SyncScope` request extension (e.g. built from the user id the bearer
/// token authenticated), and [`scoped_router`] reads it, **failing
/// closed** (`500`) when it is missing. [`router`] instead defaults
/// requests without the extension to the constant [`Self::GLOBAL`] scope —
/// the single-tenant/local deployment mode where all devices deliberately
/// share one data set.
///
/// ```rust,no_run
/// # async fn authenticate(_r: &axum::extract::Request) -> Result<String, axum::http::StatusCode> { unimplemented!() }
/// use autumn_web::sync::SyncScope;
///
/// async fn attach_sync_scope(
///     mut request: axum::extract::Request,
///     next: axum::middleware::Next,
/// ) -> Result<axum::response::Response, axum::http::StatusCode> {
///     let user_id = authenticate(&request).await?; // 401 on failure
///     request.extensions_mut().insert(SyncScope::new(user_id));
///     Ok(next.run(request).await)
/// }
/// ```
///
/// As an extractor (the [`CsrfToken`](crate::security::CsrfToken)
/// convention: extension-carried, fail-closed), `SyncScope` is also
/// available to the app's own handlers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyncScope(String);

impl SyncScope {
    /// The constant scope [`router`] assigns when no auth middleware
    /// inserted a `SyncScope` extension: one shared, single-tenant data
    /// set.
    pub const GLOBAL: &'static str = "global";

    /// A scope keyed by `scope` — derive it from the **authenticated**
    /// principal (user id, tenant id), never from client-supplied request
    /// data.
    #[must_use]
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    /// The single-tenant [`Self::GLOBAL`] scope.
    #[must_use]
    pub fn global() -> Self {
        Self(Self::GLOBAL.to_owned())
    }

    /// The scope key string, as passed to [`SyncBackend`] methods.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SyncScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S> FromRequestParts<S> for SyncScope
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or(SCOPE_MISSING_REJECTION)
    }
}

impl<S> OptionalFromRequestParts<S> for SyncScope
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

/// Fail-closed rejection for a missing scope: a `scoped_router` deployment
/// whose auth middleware forgot to insert the extension is MISCONFIGURED —
/// falling back to a shared scope would silently merge tenants' data.
const SCOPE_MISSING_REJECTION: (StatusCode, &str) = (
    StatusCode::INTERNAL_SERVER_ERROR,
    "sync scope missing: this deployment requires authentication middleware \
     to insert a SyncScope request extension (see \
     autumn_web::sync::server::scoped_router)",
);

/// Validate a push batch before applying anything: every upsert must carry
/// `Some(payload)`, and every change must carry a batch-unique `change_id`.
///
/// An upsert with a missing payload would create a **live** server row
/// with a NULL payload — clients pulling it materialize an invisible row
/// (the store's `get`/`list` treat a `None` payload as absent) while still
/// advancing their cursor past it, so the bad state silently replicates
/// everywhere.
///
/// A `change_id` repeated WITHIN one batch is just as corrupting: the
/// first occurrence would insert the dedup record and the second would hit
/// it and take the replay path — answered like a completed ack while the
/// change was **never applied**. `SyncStore::confirm_pushed` treats that
/// outcome as the ack for its own pending entry, clears it, and records a
/// version, so the second change's data would stay local-only forever.
///
/// Both shapes are rejected up front as protocol violations; the batch is
/// atomic, so nothing is applied (no rows, no dedup records, no versions
/// burned). Shared by both backends so they stay conformant (see the
/// conformance suite), and surfaced by the router as a 4xx response.
fn validate_push(request: &PushRequest) -> Result<(), SyncError> {
    let mut seen = std::collections::HashSet::with_capacity(request.changes.len());
    for change in &request.changes {
        if change.op == Op::Upsert && change.payload.is_none() {
            return Err(SyncError::Protocol(format!(
                "upsert change {} ({}/{}) has no payload — upserts must carry \
                 a JSON payload (only deletes may omit it); nothing from this \
                 batch was applied",
                change.change_id, change.collection, change.pk
            )));
        }
        if !seen.insert(change.change_id.as_str()) {
            return Err(SyncError::Protocol(format!(
                "change_id {} appears more than once in this push batch — \
                 each change must carry a unique id (a repeat would replay \
                 the first occurrence's outcome instead of applying, while \
                 the pushing client records the ack as its own); nothing \
                 from this batch was applied",
                change.change_id
            )));
        }
    }
    Ok(())
}

/// Build the [`RemoteRow`] a cleanly applied (or client-winning) change
/// produces.
fn row_from_change(change: &Change, device_id: &str, version: Version) -> RemoteRow {
    RemoteRow {
        collection: change.collection.clone(),
        pk: change.pk.clone(),
        payload: if change.op == Op::Delete {
            None
        } else {
            change.payload.clone()
        },
        version,
        deleted: change.op == Op::Delete,
        updated_at: change.updated_at,
        device_id: device_id.to_owned(),
    }
}

/// Build the post-resolution row for a conflicting change. Every branch
/// assigns the fresh `version` so all devices (including the loser)
/// converge on the resolved state via their next pull.
fn resolved_row(
    resolution: Resolution,
    change: &Change,
    device_id: &str,
    server: &RemoteRow,
    version: Version,
) -> RemoteRow {
    match resolution {
        Resolution::KeepServer => RemoteRow {
            version,
            ..server.clone()
        },
        Resolution::TakeClient => row_from_change(change, device_id, version),
        Resolution::Merge(payload) => RemoteRow {
            collection: change.collection.clone(),
            pk: change.pk.clone(),
            payload: Some(payload),
            version,
            deleted: false,
            updated_at: change.updated_at.max(server.updated_at),
            device_id: device_id.to_owned(),
        },
    }
}

/// The tombstone written back for a change whose claimed `base_version` is,
/// provably, an edit of a deleted row: either the row is ABSENT with a
/// non-zero base STRICTLY below the scope's tombstone horizon (its
/// tombstone was physically dropped, so there is nothing to conflict
/// against), or the row is present only as a TOMBSTONE and the base
/// predates that tombstone's incarnation (see [`ServerRow`]). Without this
/// a device offline past a GC would silently resurrect the row:
/// `SyncEngine` pushes pending changes BEFORE its pull can learn it needs
/// a full resync, so the clean-apply path was reachable first.
///
/// This shape is **deterministically server-winning** — the
/// [`ConflictResolver`] is deliberately bypassed. The deleted row's payload
/// and deletion timestamp were GC'd with the tombstone, so any resolver
/// input would have to be fabricated (and a clock-based policy like the
/// default LWW would let a device with a fast clock resurrect the row).
/// Custom resolvers therefore get **no say** on GC'd-tombstone conflicts:
/// the deletion sticks, the pusher receives the tombstone as its
/// `Resolved` outcome and converges on the delete. `updated_at` is the
/// server's processing time (informational only — never compared). A
/// genuinely new insert (`base_version == 0`) never takes this path — it
/// clean-applies as before, and a client that saw the delete recreates
/// cleanly (base equal to the tombstone's version, or — after the
/// tombstone was GC'd — equal to the scope's horizon).
fn gc_tombstone_row(change: &Change, version: Version) -> RemoteRow {
    RemoteRow {
        collection: change.collection.clone(),
        pk: change.pk.clone(),
        payload: None,
        version,
        deleted: true,
        updated_at: Utc::now(),
        device_id: String::new(),
    }
}

/// A stored server row plus its **incarnation marker** — server-side
/// bookkeeping only, never part of the wire protocol ([`RemoteRow`] is
/// what pulls and `Resolved` outcomes carry).
///
/// `created_version` is the version at which this INCARNATION of the pk
/// began: set on first insert, and re-set whenever the row is REVIVED —
/// a change turning a tombstone (or nothing) back into a live row, via a
/// clean recreate or a resolver decision. It never moves on updates,
/// deletes, or conflict resolutions of a live row, so `base_version <
/// created_version` is exact evidence that a pushed base referred to a
/// PREVIOUS incarnation (a delete + recreate happened in between —
/// whether or not the delete's tombstone was ever GC'd), while
/// `base_version >= created_version` proves the base belongs to the
/// current lineage and the resolver has real inputs to work with.
#[derive(Debug, Clone)]
struct ServerRow {
    row: RemoteRow,
    created_version: Version,
}

/// Decide what one (non-duplicate) change writes, given the CURRENT server
/// row and the freshly allocated `version`. Returns the row to persist
/// (with its incarnation marker — see [`ServerRow`]) and whether this was
/// a clean apply (`Applied`) rather than a conflict (`Resolved`). This is
/// the single authoritative copy of the conflict-arm policy — both
/// backends call it, so they cannot diverge:
///
/// - **Previous-incarnation base against a LIVE row** (`base_version > 0`
///   and below the row's `created_version`): the pk was deleted and
///   recreated since the pusher last saw it (or the base is a version no
///   incarnation of this pk ever had — equally unverifiable).
///   Deterministically server-winning: the live row is kept, re-versioned
///   so the pusher converges via its next pull (the standard `KeepServer`
///   shape). A resolver decision here would compare across incarnations —
///   a fast client clock (LWW) could clobber or even delete the new
///   incarnation. No horizon is consulted: the incarnation marker is
///   exact, so an unrelated same-scope GC can never force this arm onto
///   an ordinary stale edit.
/// - **Previous-incarnation base against a TOMBSTONE**: the deletion
///   sticks — a fresh [`gc_tombstone_row`], the resolver bypassed
///   (covers tombstones materialized by earlier stale pushes and plain
///   delete→recreate→delete lineages alike).
/// - **Same-incarnation stale base** (`base_version >= created_version`,
///   `!= version` — or a base-0 create racing an existing row): an
///   ordinary conflict, the resolver decides — regardless of any GC
///   horizon. A revival this produces (tombstone → live) starts a new
///   incarnation.
/// - **Absent row with a STRICTLY pre-horizon base** (`base_version > 0`,
///   below the REQUESTING SCOPE's tombstone horizon): tombstones the
///   pusher never saw may have been dropped in `(base, horizon]` — no
///   incarnation evidence survives, so the per-scope horizon is the
///   proof, and the deletion sticks via a fresh [`gc_tombstone_row`].
///   The comparison is deliberately STRICT, mirroring the pull path's
///   `session_start < horizon` staleness check: the horizon is the
///   newest DROPPED tombstone version, so a base EQUAL to it means the
///   pusher pulled that very delete before going offline — its
///   intentional recreate over its own local tombstone must proceed, not
///   be answered with another tombstone.
/// - **Everything else** (base matches the current version, a base-0
///   create against no row, or an at-horizon base against an absent
///   row): a clean apply. The incarnation marker only ever advances on a
///   **transition to live**: a recreate over a seen tombstone, a fresh
///   live insert, or an at-horizon recreate starts a new incarnation,
///   while an update or delete of the live row — and a no-op delete over
///   an existing tombstone — keeps the current marker (bumping it on a
///   redundant delete would misroute another device's legitimate
///   recreate from the original tombstone's version into the
///   server-winning arm). `base_version == 0` never takes a
///   server-winning arm.
fn apply_change_row(
    change: &Change,
    device_id: &str,
    current: Option<&ServerRow>,
    horizon: Version,
    resolver: &dyn ConflictResolver,
    version: Version,
) -> (ServerRow, bool) {
    let previous_incarnation =
        change.base_version > 0 && current.is_some_and(|c| change.base_version < c.created_version);
    match current {
        Some(cur) if previous_incarnation && !cur.row.deleted => (
            ServerRow {
                row: resolved_row(Resolution::KeepServer, change, device_id, &cur.row, version),
                created_version: cur.created_version,
            },
            false,
        ),
        Some(cur) if previous_incarnation => (
            ServerRow {
                row: gc_tombstone_row(change, version),
                created_version: cur.created_version,
            },
            false,
        ),
        Some(cur) if cur.row.version != change.base_version => {
            let resolution = resolver.resolve(device_id, change, &cur.row);
            let row = resolved_row(resolution, change, device_id, &cur.row, version);
            let created_version = if cur.row.deleted && !row.deleted {
                // Revival from a tombstone: a new incarnation begins.
                version
            } else {
                cur.created_version
            };
            (
                ServerRow {
                    row,
                    created_version,
                },
                false,
            )
        }
        None if change.base_version > 0 && change.base_version < horizon => (
            ServerRow {
                row: gc_tombstone_row(change, version),
                created_version: version,
            },
            false,
        ),
        _ => {
            let row = row_from_change(change, device_id, version);
            let created_version = match current {
                // Updating/deleting the live row: the incarnation continues.
                Some(cur) if !cur.row.deleted => cur.created_version,
                // A no-op delete over an existing tombstone: no live
                // incarnation begins, so the marker must NOT move — a
                // device that pulled the ORIGINAL tombstone recreates from
                // its version, and bumping the marker here would misroute
                // that legitimate recreate into the previous-incarnation
                // server-winning arm. The marker only ever advances on a
                // transition to LIVE.
                Some(cur) if row.deleted => cur.created_version,
                // A recreate over a seen tombstone, a fresh live insert —
                // or a tombstone minted from nothing (a base-0 delete of a
                // pk that never existed, which has no prior marker to
                // keep): a new incarnation.
                _ => version,
            };
            (
                ServerRow {
                    row,
                    created_version,
                },
                true,
            )
        }
    }
}

/// Storage backend for the server sync endpoints.
///
/// Implementations must apply each push batch atomically (all-or-nothing)
/// and dedup on `(scope, device_id, change_id)` so retries are idempotent.
/// Methods are synchronous; the router runs them on the blocking pool.
///
/// # Scoping
///
/// `apply_push` and `pull_since` take a `scope` key (see [`SyncScope`] for
/// where it comes from) that partitions ALL synced state: rows, tombstones,
/// and dedup records. The same `(collection, pk)` under two scopes is two
/// independent rows, and a push can never read or modify another scope's
/// row. Versions still come from ONE global sequence shared by every scope
/// — that is per-scope-safe (each scope's feed is a filtered subsequence,
/// so "version > cursor" with a scope predicate never skips that scope's
/// rows) and keeps the cursor semantics unchanged. The GC methods
/// ([`Self::gc_tombstones`], [`Self::gc_applied`]) are maintenance
/// operations SWEEPING all scopes, but tombstone horizons are tracked
/// **per scope**: a scope's horizon only advances when its own tombstones
/// are dropped. That matters beyond politeness — with a shared horizon,
/// one tenant's GC would both force other tenants' full resyncs and,
/// worse, push their perfectly ordinary stale-base conflicts into the
/// pre-horizon server-winning arms, silently discarding edits their
/// configured resolver should have settled.
pub trait SyncBackend: Send + Sync + 'static {
    /// Apply one push batch atomically **within `scope`**, returning one outcome per change
    /// (request order). Conflicts are settled via `resolver` and assigned a
    /// new version. Retries of an already-applied change REPLAY the
    /// original outcome: clean applies answer `AlreadyApplied` with the
    /// originally assigned version, and `Resolved` outcomes answer the same
    /// `Resolved { row }` the lost response carried (snapshotted in the
    /// dedup record, so later writes or tombstone GC cannot alter the
    /// replay — see [`AppliedRecord`]).
    ///
    /// Conflict routing keys on **incarnation evidence**: every stored row
    /// remembers the version its current incarnation began at (set on
    /// insert and on every revival — see `ServerRow`). A non-zero
    /// `base_version` BELOW that marker referred to a previous incarnation
    /// (a delete + recreate happened in between, whether or not the
    /// tombstone was GC'd) and is **deterministically server-winning** —
    /// the live row answers re-versioned (`KeepServer`) and a tombstone
    /// answers a fresh tombstone (see [`gc_tombstone_row`]) — while a base
    /// at or above the marker is a same-incarnation conflict the resolver
    /// settles normally, **regardless of any GC horizon** (an unrelated
    /// same-scope GC can never suppress ordinary conflict resolution).
    /// Only an ABSENT row still needs the per-scope tombstone horizon: a
    /// non-zero base STRICTLY below it means tombstones the pusher never
    /// saw may have been dropped, and the deletion sticks — while a base
    /// EQUAL to the horizon (the newest dropped tombstone's version,
    /// mirroring the pull path's strict `session_start < horizon` check)
    /// means the pusher pulled that very delete, so its intentional
    /// recreate proceeds as a fresh-incarnation apply. `base_version == 0`
    /// creates/recreates always clean-apply or conflict normally.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Protocol`] when the batch violates the protocol
    /// — an [`Op::Upsert`] change without `Some(payload)`, or a `change_id`
    /// repeated within the batch (see [`validate_push`]) — with nothing
    /// applied (the router surfaces this as a 4xx response), or
    /// [`SyncError::Backend`] on storage failure (the whole batch rolls
    /// back).
    fn apply_push(
        &self,
        scope: &str,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError>;

    /// Return `scope`'s rows with version greater than `cursor` (ascending,
    /// at most `limit`), or `FullResyncRequired` when a non-zero
    /// `session_start` predates the tombstone GC horizon.
    ///
    /// `session_start` is the cursor the client's sync session started
    /// from (all pages of one paginated catch-up pass the same value). The
    /// staleness check keys on it — not on the per-page `cursor` — so a
    /// multi-page catch-up from `0` is never mistaken for a stale client
    /// just because an intermediate page cursor is still below the
    /// horizon. Single-shot callers pass `session_start == cursor`.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn pull_since(
        &self,
        scope: &str,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError>;

    /// Physically drop tombstone rows (across ALL scopes) with version at
    /// or below `up_to` and advance each affected scope's tombstone
    /// horizon. Returns the number of rows removed. Clients whose session
    /// then trails their scope's horizon get a full resync. Never runs
    /// implicitly — call it deliberately (e.g. from a scheduled task) once
    /// all active devices are expected to have synced past `up_to`
    /// (`i64::MAX` means "everything so far").
    ///
    /// Each scope's persisted horizon becomes the **highest tombstone
    /// version actually dropped in that scope** — never `up_to` itself.
    /// That value is tight and safe by construction: it is a committed
    /// version (clients setting their cursor to `max(next_cursor,
    /// tombstone_horizon)` after a completed pull can never jump past
    /// unseen rows), a session at or above it has provably seen every
    /// dropped tombstone of its scope, and scopes whose tombstones were
    /// untouched keep their horizon — no forced resyncs and no suppressed
    /// conflict resolution in other tenants. On Postgres the sweep takes
    /// the push advisory lock so no push is in flight mid-sweep (an
    /// allocated-but-uncommitted version below a dropped tombstone could
    /// otherwise be skipped by a concurrent pull's cursor jump).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError>;

    /// Drop push-dedup records older than `older_than` so the dedup store
    /// (`autumn_sync_applied` on Postgres) does not grow forever. Returns
    /// the number of records removed. Never runs implicitly — pair it with
    /// your [`Self::gc_tombstones`] schedule, and keep the retention window
    /// **longer than any client's plausible offline retry horizon**: a
    /// device retrying a change whose dedup record was GC'd will re-apply
    /// it (content-idempotent for upserts/deletes, but it bumps the row
    /// version and, under a custom resolver, may re-run resolution). The
    /// dedup records also carry the resolved-row snapshots that let retries
    /// replay `Resolved` outcomes, so this retention window is the only
    /// bound on that replay — row-table writes and tombstone GC never
    /// invalidate it.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn gc_applied(&self, older_than: DateTime<Utc>) -> Result<u64, SyncError>;

    /// `scope`'s current tombstone GC horizon (`0` if no tombstone of that
    /// scope was ever dropped).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn tombstone_horizon(&self, scope: &str) -> Result<Version, SyncError>;

    /// The most recently assigned version (`0` if nothing was ever pushed).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn latest_version(&self) -> Result<Version, SyncError>;
}

/// Push-dedup record: the version a change was assigned when first
/// applied, when — and, for `Resolved` outcomes, a snapshot of the resolved
/// row so a retry after a lost response REPLAYS the original outcome.
/// Without the snapshot a retry answered `AlreadyApplied` would look like a
/// clean ack to the client: it would record the resolved version without
/// ever seeing the resolved row, keep its losing payload visible, and its
/// next edit (based on that version) would clean-apply over the resolution
/// — e.g. resurrecting a server-winning delete. Snapshotting in the dedup
/// record (rather than reloading from the rows table) keeps the replay
/// independent of later writes and tombstone GC; the only window remains
/// `gc_applied` retention, which already documents that post-GC retries
/// re-apply. Mirrors a row of `autumn_sync_applied` on Postgres.
#[derive(Debug, Clone)]
struct AppliedRecord {
    version: Version,
    applied_at: DateTime<Utc>,
    /// `Some` when the original outcome was `Resolved { row }`.
    resolved_row: Option<RemoteRow>,
}

#[derive(Debug, Default)]
struct MemoryState {
    /// Rows keyed by `(scope, collection, pk)` — the scope prefix is the
    /// partition, mirroring the Postgres primary key. Values carry the
    /// incarnation marker alongside the wire row (see [`ServerRow`]).
    rows: BTreeMap<(String, String, String), ServerRow>,
    /// Dedup records keyed by `(scope, device_id, change_id)`: `device_id`
    /// and `change_id` are client-supplied, so without the scope prefix a
    /// client in another scope could replay a foreign record's outcome (and
    /// read its resolved-row snapshot).
    applied: HashMap<(String, String, String), AppliedRecord>,
    next_version: Version,
    /// PER-SCOPE tombstone horizons: the highest tombstone version ever
    /// dropped in each scope (absent = 0). Per scope so one tenant's GC
    /// neither forces other tenants' full resyncs nor routes their
    /// ordinary conflicts into the pre-horizon server-winning arms.
    horizons: HashMap<String, Version>,
}

impl MemoryState {
    const fn allocate_version(&mut self) -> Version {
        self.next_version = self.next_version.saturating_add(1);
        self.next_version
    }
}

/// In-memory [`SyncBackend`] for tests, demos, and single-process setups.
/// Semantically identical to [`PgSyncBackend`] (both pass the same
/// conformance suite) but nothing survives a restart.
#[derive(Debug, Default)]
pub struct MemorySyncBackend {
    state: Mutex<MemoryState>,
}

impl MemorySyncBackend {
    /// An empty in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, SyncError> {
        self.state
            .lock()
            .map_err(|_| SyncError::Backend("memory backend mutex poisoned".into()))
    }
}

impl SyncBackend for MemorySyncBackend {
    fn apply_push(
        &self,
        scope: &str,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        validate_push(request)?;
        // One lock scope = one atomic batch, mirroring the PG transaction.
        let mut state = self.lock()?;
        // The REQUESTING SCOPE's horizon (see MemoryState::horizons).
        let horizon = state.horizons.get(scope).copied().unwrap_or(0);
        let mut outcomes = Vec::with_capacity(request.changes.len());
        for change in &request.changes {
            let dedup_key = (
                scope.to_owned(),
                request.device_id.clone(),
                change.change_id.clone(),
            );
            if let Some(record) = state.applied.get(&dedup_key) {
                // Replay the ORIGINAL outcome: a Resolved result must come
                // back as the same Resolved row, not a clean-looking
                // AlreadyApplied ack (see AppliedRecord).
                outcomes.push(record.resolved_row.as_ref().map_or(
                    ChangeOutcome::AlreadyApplied {
                        version: record.version,
                    },
                    |row| ChangeOutcome::Resolved { row: row.clone() },
                ));
                continue;
            }
            let row_key = (
                scope.to_owned(),
                change.collection.clone(),
                change.pk.clone(),
            );
            let current = state.rows.get(&row_key).cloned();
            let version = state.allocate_version();
            // The conflict-arm policy is the shared apply_change_row —
            // one authoritative copy for both backends.
            let (stored, clean) = apply_change_row(
                change,
                &request.device_id,
                current.as_ref(),
                horizon,
                resolver,
                version,
            );
            let outcome = if clean {
                state.rows.insert(row_key, stored);
                ChangeOutcome::Applied { version }
            } else {
                let row = stored.row.clone();
                state.rows.insert(row_key, stored);
                ChangeOutcome::Resolved { row }
            };
            let resolved_row = match &outcome {
                ChangeOutcome::Resolved { row } => Some(row.clone()),
                _ => None,
            };
            state.applied.insert(
                dedup_key,
                AppliedRecord {
                    version,
                    applied_at: Utc::now(),
                    resolved_row,
                },
            );
            outcomes.push(outcome);
        }
        drop(state);
        Ok(PushResponse { outcomes })
    }

    fn pull_since(
        &self,
        scope: &str,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError> {
        // One MutexGuard covers BOTH the horizon read and the row scan, so
        // they observe a single consistent state — the mutex is this
        // backend's analogue of the Postgres pull's REPEATABLE READ
        // snapshot (a GC can only run entirely before or entirely after
        // this pull, never between its two reads).
        let state = self.lock()?;
        // The REQUESTING SCOPE's horizon — both for the staleness check
        // and the per-page value the client's GC-race guard reads.
        let horizon = state.horizons.get(scope).copied().unwrap_or(0);
        if session_start > 0 && session_start < horizon {
            return Ok(PullResponse::FullResyncRequired {
                tombstone_horizon: horizon,
            });
        }
        let mut rows: Vec<RemoteRow> = state
            .rows
            .iter()
            .filter(|((row_scope, _, _), stored)| row_scope == scope && stored.row.version > cursor)
            .map(|(_, stored)| stored.row.clone())
            .collect();
        drop(state);
        rows.sort_by_key(|row| row.version);
        rows.truncate(usize::try_from(limit.max(0)).unwrap_or(usize::MAX));
        let next_cursor = rows.last().map_or(cursor, |row| row.version);
        Ok(PullResponse::Ok {
            rows,
            next_cursor,
            tombstone_horizon: horizon,
        })
    }

    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        let mut state = self.lock()?;
        // Global sweep, PER-SCOPE horizons: each scope's horizon advances
        // to the highest tombstone version actually dropped in THAT scope
        // (see the trait docs) — inherently a committed version under this
        // mutex, so no clamp is needed, and untouched scopes keep their
        // horizon.
        let mut per_scope: HashMap<String, Version> = HashMap::new();
        let mut removed = 0u64;
        state.rows.retain(|(row_scope, _, _), stored| {
            if stored.row.deleted && stored.row.version <= up_to {
                let max = per_scope.entry(row_scope.clone()).or_insert(0);
                *max = (*max).max(stored.row.version);
                removed = removed.saturating_add(1);
                false
            } else {
                true
            }
        });
        for (gc_scope, dropped_max) in per_scope {
            let horizon = state.horizons.entry(gc_scope).or_insert(0);
            *horizon = (*horizon).max(dropped_max);
        }
        drop(state);
        Ok(removed)
    }

    fn gc_applied(&self, older_than: DateTime<Utc>) -> Result<u64, SyncError> {
        let mut state = self.lock()?;
        let before = state.applied.len();
        state
            .applied
            .retain(|_, record| record.applied_at >= older_than);
        // `retain` only ever removes, so `before >= len()` holds by
        // construction — but a `usize` underflow here would be a silent
        // ~1.8e19 "removed" count in release, so saturate and assert the
        // invariant in debug rather than trusting it silently.
        debug_assert!(
            before >= state.applied.len(),
            "retain must not grow the applied set"
        );
        let removed = before.saturating_sub(state.applied.len());
        drop(state);
        Ok(removed as u64)
    }

    fn tombstone_horizon(&self, scope: &str) -> Result<Version, SyncError> {
        Ok(self.lock()?.horizons.get(scope).copied().unwrap_or(0))
    }

    fn latest_version(&self) -> Result<Version, SyncError> {
        Ok(self.lock()?.next_version)
    }
}

// ── Postgres backend ─────────────────────────────────────────────────────

/// Idempotent DDL for the server-side shadow tables. Deliberately not part
/// of the framework migrations, so apps that never mount the sync router
/// see zero schema churn.
const PG_SCHEMA_DDL: &str = "
CREATE SEQUENCE IF NOT EXISTS autumn_sync_version_seq;
CREATE TABLE IF NOT EXISTS autumn_sync_rows (
    -- Tenant/principal partition key, derived server-side from the
    -- authenticated request (see SyncScope) — never client-supplied.
    scope TEXT NOT NULL,
    collection TEXT NOT NULL,
    pk TEXT NOT NULL,
    payload JSONB,
    version BIGINT NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL,
    device_id TEXT NOT NULL DEFAULT '',
    -- The version this INCARNATION of the row began at: set on insert and
    -- on every revival from a tombstone/absent state. A pushed base below
    -- it referred to a previous (deleted + recreated) incarnation and is
    -- settled server-winning without consulting the resolver.
    created_version BIGINT NOT NULL,
    PRIMARY KEY (scope, collection, pk)
);
CREATE INDEX IF NOT EXISTS autumn_sync_rows_scope_version_idx
    ON autumn_sync_rows (scope, version);
CREATE TABLE IF NOT EXISTS autumn_sync_applied (
    -- Scope prefix on the dedup key too: device_id/change_id are
    -- client-supplied, so without it a client in another scope could
    -- replay a foreign record's outcome (and read its resolved_row).
    scope TEXT NOT NULL,
    device_id TEXT NOT NULL,
    change_id TEXT NOT NULL,
    version BIGINT NOT NULL DEFAULT 0,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Snapshot of the resolved row when the original outcome was Resolved,
    -- so retries replay the resolution instead of a clean-looking
    -- AlreadyApplied ack (NULL for clean applies).
    resolved_row JSONB,
    PRIMARY KEY (scope, device_id, change_id)
);
CREATE TABLE IF NOT EXISTS autumn_sync_horizons (
    -- Per-scope tombstone GC horizon: the highest tombstone version ever
    -- physically dropped in that scope. Tracked PER SCOPE so one tenant's
    -- GC neither forces other tenants' full resyncs nor routes their
    -- ordinary conflicts into the pre-horizon server-winning arms.
    scope TEXT PRIMARY KEY,
    horizon BIGINT NOT NULL
);
";

/// Well-known key for the transaction-scoped Postgres advisory lock
/// ([`pg_advisory_xact_lock`]) taken at the top of every
/// [`PgSyncBackend::apply_push`] transaction. The value is the ASCII bytes
/// `"ATMNSYNC"` as a big-endian `i64` — stable, documented, and unlikely to
/// collide with application locks.
///
/// The lock serializes push batches, which is **load-bearing** for two
/// correctness guarantees that Postgres READ COMMITTED semantics alone do
/// not give:
///
/// 1. **No skipped versions on pull.** Versions come from a sequence, and
///    sequences are non-transactional: without the lock, a push holding
///    version 5 can commit *after* a push holding version 6, and a pull in
///    between sees only 6, persists `cursor = 6`, and never receives row 5.
///    With the lock, pushes commit in version order, so any READ COMMITTED
///    pull observes a clean version prefix and `max(version seen)` is a
///    safe cursor.
/// 2. **Concurrent first-inserts of one pk engage the resolver.** `SELECT
///    … FOR UPDATE` locks nothing for a row that does not exist yet, so two
///    concurrent transactions both creating pk X would each take the clean
///    `Applied` path and the later `ON CONFLICT … DO UPDATE` would silently
///    overwrite the earlier write without running the [`ConflictResolver`].
///    Serialized pushes make the second transaction see the first's
///    committed row and route through the conflict path.
///
/// [`pg_advisory_xact_lock`]:
///     https://www.postgresql.org/docs/current/functions-admin.html#FUNCTIONS-ADVISORY-LOCKS
const PG_PUSH_ADVISORY_LOCK_KEY: i64 = 0x4154_4D4E_5359_4E43; // "ATMNSYNC"

#[derive(QueryableByName)]
struct PgRowRecord {
    #[diesel(sql_type = Text)]
    collection: String,
    #[diesel(sql_type = Text)]
    pk: String,
    #[diesel(sql_type = Nullable<Jsonb>)]
    payload: Option<serde_json::Value>,
    #[diesel(sql_type = BigInt)]
    version: i64,
    #[diesel(sql_type = Bool)]
    deleted: bool,
    #[diesel(sql_type = Timestamptz)]
    updated_at: DateTime<Utc>,
    #[diesel(sql_type = Text)]
    device_id: String,
    #[diesel(sql_type = BigInt)]
    created_version: i64,
}

impl PgRowRecord {
    fn into_server_row(self) -> ServerRow {
        ServerRow {
            row: RemoteRow {
                collection: self.collection,
                pk: self.pk,
                payload: self.payload,
                version: self.version,
                deleted: self.deleted,
                updated_at: self.updated_at,
                device_id: self.device_id,
            },
            created_version: self.created_version,
        }
    }
}

#[derive(QueryableByName)]
struct PgVersionRecord {
    #[diesel(sql_type = BigInt)]
    version: i64,
}

/// Dedup-record row: the `change_id` it answers for (batched lookups return
/// one row per matched `change_id`, so the caller needs it to key the result
/// back to the change), plus the originally assigned version and the
/// resolved-row snapshot for `Resolved` outcomes (see [`AppliedRecord`]).
#[derive(QueryableByName)]
struct PgAppliedRecord {
    #[diesel(sql_type = Text)]
    change_id: String,
    #[diesel(sql_type = BigInt)]
    version: i64,
    #[diesel(sql_type = Nullable<Jsonb>)]
    resolved_row: Option<serde_json::Value>,
}

/// One dropped tombstone from the GC sweep's `DELETE … RETURNING`: enough
/// to advance the dropped row's scope's horizon.
#[derive(QueryableByName)]
struct PgDroppedRecord {
    #[diesel(sql_type = Text)]
    scope: String,
    #[diesel(sql_type = BigInt)]
    version: i64,
}

#[derive(QueryableByName)]
struct PgSequenceRecord {
    #[diesel(sql_type = BigInt)]
    last_value: i64,
    #[diesel(sql_type = Bool)]
    is_called: bool,
}

/// Upsert every row in `stored` in ONE round trip via `UNNEST`-driven
/// multi-row `INSERT … ON CONFLICT`, instead of one `INSERT` per row.
///
/// Callers must pass at most one entry per `(collection, pk)` — a single
/// `INSERT … ON CONFLICT` statement cannot affect the same conflict target
/// twice (Postgres raises "ON CONFLICT DO UPDATE command cannot affect row a
/// second time"). [`SyncBackend::apply_push`] satisfies this by keeping only
/// the LAST decided state per key in its `current` map before calling here.
fn pg_upsert_rows<'a, C>(
    conn: &mut C,
    scope: &str,
    stored: impl Iterator<Item = &'a ServerRow>,
) -> Result<(), diesel::result::Error>
where
    C: diesel::connection::LoadConnection<Backend = diesel::pg::Pg>,
{
    let stored: Vec<&ServerRow> = stored.collect();
    if stored.is_empty() {
        return Ok(());
    }
    let collections: Vec<&str> = stored.iter().map(|s| s.row.collection.as_str()).collect();
    let pks: Vec<&str> = stored.iter().map(|s| s.row.pk.as_str()).collect();
    let payloads: Vec<Option<serde_json::Value>> =
        stored.iter().map(|s| s.row.payload.clone()).collect();
    let versions: Vec<i64> = stored.iter().map(|s| s.row.version).collect();
    let deleteds: Vec<bool> = stored.iter().map(|s| s.row.deleted).collect();
    let updated_ats: Vec<DateTime<Utc>> = stored.iter().map(|s| s.row.updated_at).collect();
    let device_ids: Vec<&str> = stored.iter().map(|s| s.row.device_id.as_str()).collect();
    let created_versions: Vec<i64> = stored.iter().map(|s| s.created_version).collect();

    sql_query(
        "INSERT INTO autumn_sync_rows \
         (scope, collection, pk, payload, version, deleted, updated_at, device_id, \
          created_version) \
         SELECT $1, t.* FROM UNNEST( \
           $2::text[], $3::text[], $4::jsonb[], $5::bigint[], $6::bool[], $7::timestamptz[], \
           $8::text[], $9::bigint[] \
         ) AS t(collection, pk, payload, version, deleted, updated_at, device_id, created_version) \
         ON CONFLICT (scope, collection, pk) DO UPDATE SET \
         payload = excluded.payload, version = excluded.version, \
         deleted = excluded.deleted, updated_at = excluded.updated_at, \
         device_id = excluded.device_id, \
         created_version = excluded.created_version",
    )
    .bind::<Text, _>(scope)
    .bind::<Array<Text>, _>(collections)
    .bind::<Array<Text>, _>(pks)
    .bind::<Array<Nullable<Jsonb>>, _>(payloads)
    .bind::<Array<BigInt>, _>(versions)
    .bind::<Array<Bool>, _>(deleteds)
    .bind::<Array<Timestamptz>, _>(updated_ats)
    .bind::<Array<Text>, _>(device_ids)
    .bind::<Array<BigInt>, _>(created_versions)
    .execute(conn)
    .map(|_| ())
}

/// Insert every applied-change dedup record in ONE round trip via
/// `UNNEST`-driven multi-row `INSERT`, instead of one `INSERT` per change.
/// `change_id` is batch-unique (`validate_push` rejects repeats), so this
/// insert can never collide with itself.
fn pg_insert_applied<C>(
    conn: &mut C,
    scope: &str,
    device_id: &str,
    records: &[(String, Version, Option<serde_json::Value>)],
) -> Result<(), diesel::result::Error>
where
    C: diesel::connection::LoadConnection<Backend = diesel::pg::Pg>,
{
    if records.is_empty() {
        return Ok(());
    }
    let change_ids: Vec<&str> = records.iter().map(|(id, _, _)| id.as_str()).collect();
    let versions: Vec<i64> = records.iter().map(|(_, version, _)| *version).collect();
    let resolved_rows: Vec<Option<serde_json::Value>> =
        records.iter().map(|(_, _, row)| row.clone()).collect();
    sql_query(
        "INSERT INTO autumn_sync_applied (scope, device_id, change_id, version, resolved_row) \
         SELECT $1, $2, t.change_id, t.version, t.resolved_row \
         FROM UNNEST($3::text[], $4::bigint[], $5::jsonb[]) \
           AS t(change_id, version, resolved_row)",
    )
    .bind::<Text, _>(scope)
    .bind::<Text, _>(device_id)
    .bind::<Array<Text>, _>(change_ids)
    .bind::<Array<BigInt>, _>(versions)
    .bind::<Array<Nullable<Jsonb>>, _>(resolved_rows)
    .execute(conn)
    .map(|_| ())
}

/// Allocate `n` fresh, unique, monotonically increasing versions in ONE
/// round trip, instead of one `nextval()` call per change. Ordering across
/// calls to `next()` on the returned iterator matches the order `nextval()`
/// would have assigned them one at a time (ascending), so a caller that
/// consumes them in request order reproduces the old per-change assignment.
///
/// The `ORDER BY gs.ord` is load-bearing, not cosmetic: a plain `SELECT`
/// over a `FROM` clause has no guaranteed row order without it, so nothing
/// would otherwise pin "first row back" to "first `nextval()` call" — and a
/// caller that assumes that pairing (as this one does) needs it to be exact
/// (`nextval()` is `PARALLEL UNSAFE`, so this can't actually be parallelized
/// today, but the explicit ordinal makes that a documented guarantee rather
/// than an accident of the current planner).
fn pg_next_versions<C>(conn: &mut C, n: usize) -> Result<Vec<Version>, diesel::result::Error>
where
    C: diesel::connection::LoadConnection<Backend = diesel::pg::Pg>,
{
    if n == 0 {
        return Ok(Vec::new());
    }
    sql_query(
        "SELECT nextval('autumn_sync_version_seq') AS version \
         FROM generate_series(1, $1) WITH ORDINALITY AS gs(n, ord) \
         ORDER BY gs.ord",
    )
    .bind::<BigInt, _>(i64::try_from(n).unwrap_or(i64::MAX))
    .load::<PgVersionRecord>(conn)
    .map(|records| records.into_iter().map(|record| record.version).collect())
}

/// The scope's tombstone GC horizon — `0` when no tombstone of that scope
/// was ever dropped.
fn pg_horizon<C>(conn: &mut C, scope: &str) -> Result<Version, diesel::result::Error>
where
    C: diesel::connection::LoadConnection<Backend = diesel::pg::Pg>,
{
    let record = sql_query("SELECT horizon AS version FROM autumn_sync_horizons WHERE scope = $1")
        .bind::<Text, _>(scope)
        .get_result::<PgVersionRecord>(conn)
        .optional()?;
    Ok(record.map_or(0, |r| r.version))
}

/// The most recently assigned version — the sequence's `last_value` once it
/// has been called, `0` on a fresh backend.
fn pg_latest_version<C>(conn: &mut C) -> Result<Version, diesel::result::Error>
where
    C: diesel::connection::LoadConnection<Backend = diesel::pg::Pg>,
{
    let record = sql_query("SELECT last_value, is_called FROM autumn_sync_version_seq")
        .get_result::<PgSequenceRecord>(conn)?;
    Ok(if record.is_called {
        record.last_value
    } else {
        0
    })
}

/// Postgres-backed [`SyncBackend`] persisting into shadow tables.
///
/// State lives in `autumn_sync_rows` / `autumn_sync_applied` /
/// `autumn_sync_horizons`. Each push batch applies in one transaction;
/// versions come from the `autumn_sync_version_seq` sequence, making the
/// row table itself the change feed.
///
/// Connections honor the URL's `sslmode` through the crate's shared
/// TLS-aware path (see `with_sync_pg_connection!`): `sslmode=require` /
/// `verify-full` URLs — the production posture — connect through the same
/// rustls connector as the app's pool, because the workspace bundles
/// libpq WITHOUT SSL support and a raw [`PgConnection`] cannot reach
/// TLS-only servers at all.
#[derive(Debug, Clone)]
pub struct PgSyncBackend {
    database_url: String,
}

/// Run `$body` with a `&mut` sync diesel connection to `$url` that honors
/// the connection string's `sslmode` — the SAME TLS-aware path the app
/// pool and the migration/wait checks use (see
/// [`crate::db::establish_migration_connection`], issue #1585 review):
/// TLS-off URLs keep the historical native [`PgConnection`], while
/// `sslmode=require` / `verify-full` URLs connect through the pool's
/// rustls connector wrapped in diesel-async's sync
/// `AsyncConnectionWrapper` — the workspace bundles libpq without SSL
/// (`pq-sys` `bundled_without_openssl`), so the native path cannot reach
/// TLS-only servers even though the async pool connects fine.
///
/// The body is expanded once per concrete connection type, so it may only
/// use the sync diesel APIs both provide: queries, `transaction()`, and
/// `SimpleConnection::batch_execute` — which covers everything this
/// backend needs. Advisory locks are plain SQL, and the pull path's READ
/// ONLY REPEATABLE READ posture is set via `SET TRANSACTION` as the first
/// statement inside `transaction()` (semantically identical to
/// `build_transaction()`, which is an inherent `PgConnection` method the
/// wrapper does not expose).
///
/// The connect AND `$body` both run on a freshly spawned
/// [`std::thread::scope`] thread, never the calling one — load-bearing, not
/// an optimization: the rustls arm's connection bridges every sync diesel
/// call through its own internal `block_on`
/// (see [`crate::db::establish_migration_connection`]'s doc comment), which
/// panics if run from a thread already inside some ambient runtime's
/// context — e.g. `PgSyncBackend::ensure_schema` (or any other method here)
/// called directly from an async body rather than from `spawn_blocking`. A
/// freshly spawned thread has never entered any runtime, so it is always
/// safe. `scope` (rather than a plain `'static` `std::thread::spawn`) lets
/// `$body` borrow from the enclosing method (e.g. `&self`, request data)
/// without needing to be `'static`.
macro_rules! with_sync_pg_connection {
    ($url:expr, |$conn:ident| $body:expr) => {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    match crate::db::establish_migration_connection($url).map_err(backend_err)? {
                        crate::db::MigrationConnection::Native(mut native) => {
                            let $conn = &mut native;
                            $body
                        }
                        crate::db::MigrationConnection::Rustls { mut conn, runtime } => {
                            let result = {
                                let $conn = &mut conn;
                                $body
                            };
                            // The runtime drives the connection's tokio
                            // driver task: it must outlive every use of
                            // `conn`.
                            drop(conn);
                            drop(runtime);
                            result
                        }
                    }
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    };
}

impl PgSyncBackend {
    /// A backend connecting to `database_url`. Call [`Self::ensure_schema`]
    /// once at startup before serving.
    #[must_use]
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
        }
    }

    /// Create the shadow tables and the version sequence if they do not
    /// exist. Idempotent (`CREATE ... IF NOT EXISTS` throughout);
    /// deliberately not part of the framework migrations so non-sync apps
    /// see zero schema churn. Blocking — run at startup (or inside
    /// `spawn_blocking`).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] if the connection fails or the DDL
    /// cannot be applied.
    pub fn ensure_schema(&self) -> Result<(), SyncError> {
        use diesel::connection::SimpleConnection;
        with_sync_pg_connection!(&self.database_url, |conn| {
            conn.batch_execute(PG_SCHEMA_DDL).map_err(backend_err)
        })
    }
}

impl SyncBackend for PgSyncBackend {
    /// Batches the per-change round trips into a fixed number of statements
    /// for the whole push, independent of `request.changes.len()`: one dedup
    /// lookup, one `FOR UPDATE` row fetch, one version-sequence draw, one
    /// row upsert, one dedup-record insert — instead of five per change (see
    /// `docs/perf/sync-push-batching/` for the profile). `apply_change_row`
    /// (the conflict-arm policy) is untouched and still runs once per
    /// change, in request order, against the SAME inputs it always got —
    /// only how those inputs are fetched changed.
    ///
    /// A pk touched more than once in one batch (two queued local edits, or
    /// a create immediately followed by an edit, before either was ever
    /// synced) still needs SEQUENTIAL semantics: the second change must see
    /// the first's outcome as its `current` row, exactly as the old
    /// same-transaction re-`SELECT … FOR UPDATE` would (Postgres read-your-
    /// own-writes). Here that carry-forward is an in-memory map, seeded once
    /// from a single batched fetch and updated after each change is decided
    /// — so the loop below still runs sequentially in request order, it just
    /// never talks to Postgres inside the loop.
    #[allow(clippy::too_many_lines)] // one linear batching algorithm, clearest unsplit
    fn apply_push(
        &self,
        scope: &str,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        validate_push(request)?;
        with_sync_pg_connection!(&self.database_url, |conn| {
            conn.transaction::<_, diesel::result::Error, _>(|conn| {
                // Serialize push batches (held until commit). See the doc
                // comment on PG_PUSH_ADVISORY_LOCK_KEY for why this is
                // required for correctness, not just politeness.
                sql_query("SELECT pg_advisory_xact_lock($1)")
                    .bind::<BigInt, _>(PG_PUSH_ADVISORY_LOCK_KEY)
                    .execute(conn)?;
                // The REQUESTING SCOPE's horizon, stable for the whole batch:
                // GC serializes on the same advisory lock, so it cannot move
                // mid-transaction.
                let horizon = pg_horizon(conn, scope)?;

                // 1. Batch dedup check: every change_id in one round trip.
                // `validate_push` already rejected repeats within the batch,
                // so this is a plain equality lookup against EARLIER pushes.
                let change_ids: Vec<&str> = request
                    .changes
                    .iter()
                    .map(|c| c.change_id.as_str())
                    .collect();
                let applied: HashMap<String, PgAppliedRecord> = if change_ids.is_empty() {
                    HashMap::new()
                } else {
                    sql_query(
                        "SELECT change_id, version, resolved_row FROM autumn_sync_applied \
                         WHERE scope = $1 AND device_id = $2 AND change_id = ANY($3)",
                    )
                    .bind::<Text, _>(scope)
                    .bind::<Text, _>(&request.device_id)
                    .bind::<Array<Text>, _>(&change_ids)
                    .load::<PgAppliedRecord>(conn)?
                    .into_iter()
                    .map(|record| (record.change_id.clone(), record))
                    .collect()
                };

                // 2. Batch current-row fetch (FOR UPDATE): one round trip
                // for every DISTINCT (collection, pk) among changes that
                // will actually apply — a dedup hit never touches
                // autumn_sync_rows, same as before.
                let mut to_apply_keys: Vec<(&str, &str)> = Vec::new();
                let mut seen_keys = std::collections::HashSet::new();
                for change in &request.changes {
                    if applied.contains_key(&change.change_id) {
                        continue;
                    }
                    if seen_keys.insert((change.collection.as_str(), change.pk.as_str())) {
                        to_apply_keys.push((change.collection.as_str(), change.pk.as_str()));
                    }
                }
                let mut current: HashMap<(String, String), ServerRow> = if to_apply_keys.is_empty()
                {
                    HashMap::new()
                } else {
                    let collections: Vec<&str> = to_apply_keys.iter().map(|(c, _)| *c).collect();
                    let pks: Vec<&str> = to_apply_keys.iter().map(|(_, p)| *p).collect();
                    sql_query(
                        "SELECT collection, pk, payload, version, deleted, updated_at, device_id, \
                         created_version \
                         FROM autumn_sync_rows \
                         WHERE scope = $1 AND (collection, pk) IN ( \
                           SELECT * FROM UNNEST($2::text[], $3::text[]) \
                         ) FOR UPDATE",
                    )
                    .bind::<Text, _>(scope)
                    .bind::<Array<Text>, _>(&collections)
                    .bind::<Array<Text>, _>(&pks)
                    .load::<PgRowRecord>(conn)?
                    .into_iter()
                    .map(|record| {
                        (
                            (record.collection.clone(), record.pk.clone()),
                            record.into_server_row(),
                        )
                    })
                    .collect()
                };

                // 3. Batch version allocation: exactly one nextval() per
                // change that will actually apply — a dedup hit burns no
                // version, same as before (`pg_next_version` was called
                // AFTER the dedup `continue`).
                let to_apply_count = request.changes.len().saturating_sub(applied.len());
                let mut versions = pg_next_versions(conn, to_apply_count)?.into_iter();

                // 4. Decide every change's outcome in memory (pure — see
                // above), then batch-write.
                let mut outcomes = Vec::with_capacity(request.changes.len());
                let mut to_record: Vec<(String, Version, Option<serde_json::Value>)> =
                    Vec::with_capacity(to_apply_count);
                for change in &request.changes {
                    if let Some(record) = applied.get(&change.change_id) {
                        // Replay the ORIGINAL outcome: a Resolved result must
                        // come back as the same Resolved row, not a
                        // clean-looking AlreadyApplied ack (see AppliedRecord).
                        outcomes.push(match &record.resolved_row {
                            Some(value) => ChangeOutcome::Resolved {
                                row: serde_json::from_value(value.clone()).map_err(|e| {
                                    diesel::result::Error::DeserializationError(e.into())
                                })?,
                            },
                            None => ChangeOutcome::AlreadyApplied {
                                version: record.version,
                            },
                        });
                        continue;
                    }

                    let Some(version) = versions.next() else {
                        return Err(diesel::result::Error::QueryBuilderError(Box::new(
                            std::io::Error::other(
                                "apply_push: version pool exhausted (internal invariant \
                                 violated — to_apply_count must equal the number of \
                                 non-duplicate changes)",
                            ),
                        )));
                    };
                    let key = (change.collection.clone(), change.pk.clone());
                    // The conflict-arm policy is the shared apply_change_row
                    // — one authoritative copy for both backends. `current`
                    // reflects any EARLIER change in this same batch to the
                    // same key (inserted below), exactly like the old
                    // re-SELECT would have.
                    let (stored, clean) = apply_change_row(
                        change,
                        &request.device_id,
                        current.get(&key),
                        horizon,
                        resolver,
                        version,
                    );
                    let outcome = if clean {
                        ChangeOutcome::Applied { version }
                    } else {
                        ChangeOutcome::Resolved {
                            row: stored.row.clone(),
                        }
                    };
                    let resolved_row = match &outcome {
                        ChangeOutcome::Resolved { row } => Some(
                            serde_json::to_value(row)
                                .map_err(|e| diesel::result::Error::SerializationError(e.into()))?,
                        ),
                        _ => None,
                    };
                    to_record.push((change.change_id.clone(), version, resolved_row));
                    // A pk written twice in this batch keeps only its LAST
                    // value here — the same end state the old sequential
                    // per-change UPSERTs left, just decided once instead of
                    // written twice.
                    current.insert(key, stored);
                    outcomes.push(outcome);
                }

                // 5. Batch-write: one UPSERT for the final state of every
                // touched row, one INSERT for every applied change's dedup
                // record.
                pg_upsert_rows(conn, scope, current.values())?;
                pg_insert_applied(conn, scope, &request.device_id, &to_record)?;

                Ok(PushResponse { outcomes })
            })
            .map_err(backend_err)
        })
    }

    fn pull_since(
        &self,
        scope: &str,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError> {
        // REPEATABLE READ is load-bearing: the horizon read and the row
        // scan must observe ONE MVCC snapshot. Under the default READ
        // COMMITTED each statement snapshots independently, so a concurrent
        // gc_tombstones committing between the two would let this pull pass
        // the staleness check against the OLD horizon while the row scan
        // already misses the GC'd tombstones — the client would advance its
        // cursor past deletions it never received (and never be told to
        // resync), keeping a stale row visible forever. With one snapshot
        // the response is internally consistent: the client either sees the
        // tombstones or (on its next pull, against the advanced horizon)
        // gets FullResyncRequired.
        //
        // Cost: none worth noting. A READ ONLY REPEATABLE READ transaction
        // in Postgres cannot fail with a serialization error (40001 arises
        // from write-write conflicts; plain SELECTs have none — only
        // SERIALIZABLE can abort read-only transactions), so no retry loop
        // is needed, and unlike taking the GC/push advisory lock this
        // serializes nothing: pulls, pushes, and GC all keep running
        // concurrently.
        //
        // The posture is set via `SET TRANSACTION ISOLATION LEVEL
        // REPEATABLE READ, READ ONLY` as the FIRST statement inside
        // `transaction()` — semantically identical to Postgres's
        // `BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ` (SET
        // TRANSACTION applies when it precedes any query in the
        // transaction), and unlike `build_transaction()` (an inherent
        // PgConnection method) it works on both connection types the
        // TLS-aware macro dispatches to.
        with_sync_pg_connection!(&self.database_url, |conn| {
            conn.transaction::<_, diesel::result::Error, _>(|conn| {
                sql_query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                    .execute(conn)?;
                let horizon = pg_horizon(conn, scope)?;
                if session_start > 0 && session_start < horizon {
                    return Ok(PullResponse::FullResyncRequired {
                        tombstone_horizon: horizon,
                    });
                }
                let rows: Vec<RemoteRow> = sql_query(
                    "SELECT collection, pk, payload, version, deleted, updated_at, device_id, \
                     created_version \
                     FROM autumn_sync_rows \
                     WHERE scope = $1 AND version > $2 ORDER BY version LIMIT $3",
                )
                .bind::<Text, _>(scope)
                .bind::<BigInt, _>(cursor)
                .bind::<BigInt, _>(limit.max(0))
                .get_results::<PgRowRecord>(conn)?
                .into_iter()
                .map(|record| record.into_server_row().row)
                .collect();
                let next_cursor = rows.last().map_or(cursor, |row| row.version);
                Ok(PullResponse::Ok {
                    rows,
                    next_cursor,
                    tombstone_horizon: horizon,
                })
            })
            .map_err(backend_err)
        })
    }

    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        with_sync_pg_connection!(&self.database_url, |conn| {
            conn.transaction::<_, diesel::result::Error, _>(|conn| {
                // Serialize with pushes (the SAME advisory lock apply_push
                // takes): with an in-flight push holding an allocated-but-
                // uncommitted version below a tombstone this sweep drops, the
                // persisted horizon could cover that version before its row is
                // visible — a client pulling in between would jump its cursor
                // past the row and miss it forever. Holding the push lock, no
                // push is in flight while we sweep, so every version at or
                // below anything we drop is committed.
                sql_query("SELECT pg_advisory_xact_lock($1)")
                    .bind::<BigInt, _>(PG_PUSH_ADVISORY_LOCK_KEY)
                    .execute(conn)?;
                // Global sweep, PER-SCOPE horizons: each scope's horizon
                // advances to the highest tombstone version actually dropped
                // in THAT scope (see the trait docs) — inherently a committed
                // version, so no explicit clamp is needed, and scopes whose
                // tombstones were untouched keep their horizon (no forced
                // resyncs, no suppressed conflict resolution elsewhere).
                let dropped = sql_query(
                    "DELETE FROM autumn_sync_rows WHERE deleted AND version <= $1 \
                 RETURNING scope, version",
                )
                .bind::<BigInt, _>(up_to)
                .get_results::<PgDroppedRecord>(conn)?;
                let removed = dropped.len();
                let mut per_scope: HashMap<String, Version> = HashMap::new();
                for record in dropped {
                    let max = per_scope.entry(record.scope).or_insert(0);
                    *max = (*max).max(record.version);
                }
                // One statement for every scope touched by this sweep instead
                // of one per scope: a sweep across a multi-tenant instance
                // can touch hundreds/thousands of distinct scopes, and each
                // was previously a separate round trip inside this same
                // advisory-locked transaction. `UNNEST` fans the arrays back
                // out into rows server-side, so the ON CONFLICT/GREATEST
                // semantics are identical to issuing the loop above — this is
                // a batching change, not a behavior change.
                if !per_scope.is_empty() {
                    let (scopes, horizons): (Vec<String>, Vec<Version>) =
                        per_scope.into_iter().unzip();
                    sql_query(
                        "INSERT INTO autumn_sync_horizons (scope, horizon) \
                     SELECT * FROM UNNEST($1::TEXT[], $2::BIGINT[]) AS t(scope, horizon) \
                     ON CONFLICT (scope) DO UPDATE SET \
                     horizon = GREATEST(autumn_sync_horizons.horizon, excluded.horizon)",
                    )
                    .bind::<Array<Text>, _>(scopes)
                    .bind::<Array<BigInt>, _>(horizons)
                    .execute(conn)?;
                }
                Ok(removed as u64)
            })
            .map_err(backend_err)
        })
    }

    fn gc_applied(&self, older_than: DateTime<Utc>) -> Result<u64, SyncError> {
        with_sync_pg_connection!(&self.database_url, |conn| {
            let removed = sql_query("DELETE FROM autumn_sync_applied WHERE applied_at < $1")
                .bind::<Timestamptz, _>(older_than)
                .execute(conn)
                .map_err(backend_err)?;
            Ok(removed as u64)
        })
    }

    fn tombstone_horizon(&self, scope: &str) -> Result<Version, SyncError> {
        with_sync_pg_connection!(&self.database_url, |conn| {
            pg_horizon(conn, scope).map_err(backend_err)
        })
    }

    fn latest_version(&self) -> Result<Version, SyncError> {
        with_sync_pg_connection!(&self.database_url, |conn| {
            pg_latest_version(conn).map_err(backend_err)
        })
    }
}

// ── Router ───────────────────────────────────────────────────────────────

/// Build the **single-tenant** sync router (`POST /push`, `GET /pull`).
///
/// Generic over the host router's state so it mounts both on an Autumn app
/// (`AppBuilder::nest("/sync", router(...))` shares [`crate::AppState`] and
/// the app's global middleware) and on a bare `axum::Router` in tests.
/// **Mount it behind authentication** — the endpoints trust `device_id` as
/// sent, and anyone who can reach them can read and write every synced row
/// in scope.
///
/// # Scoping: this constructor is single-tenant
///
/// Requests without a [`SyncScope`] request extension read and write the
/// one shared [`SyncScope::GLOBAL`] scope — appropriate for single-user or
/// local deployments where every authenticated device deliberately sees
/// the same data set. When auth middleware DOES insert a `SyncScope`
/// extension, it is honored. **Multi-user deployments must use
/// [`scoped_router`]** (which fails closed when the extension is missing)
/// and derive the scope from the authenticated principal, or every user
/// silently shares the global partition.
///
/// Request bounds and validation: push batches larger than
/// [`MAX_PUSH_CHANGES`](crate::sync::protocol::MAX_PUSH_CHANGES) changes
/// are rejected with `413` (request bodies are additionally capped by
/// axum's default body limit), batches containing an upsert without a
/// payload — or a `change_id` repeated within the batch — are rejected
/// with `422` (nothing applied — see
/// [`SyncBackend::apply_push`]), and the pull `limit` is clamped to at most
/// [`MAX_PULL_LIMIT`](crate::sync::protocol::MAX_PULL_LIMIT) (minimum 1).
pub fn router<S>(
    backend: Arc<dyn SyncBackend>,
    resolver: Arc<dyn ConflictResolver>,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    build_router(backend, resolver, false)
}

/// Build the **multi-tenant** sync router.
///
/// Like [`router`], but every request must carry a [`SyncScope`] request
/// extension inserted by the deployment's authentication middleware
/// (derived from the authenticated principal — e.g. the user id the
/// bearer token proved). Requests without one are rejected with `500` and
/// nothing is read or written: a missing scope here is a server
/// misconfiguration, and falling back to a shared scope would silently
/// merge tenants' data (same fail-closed posture as the auth middleware
/// itself).
///
/// See [`SyncScope`] for a middleware example, and [`router`] for the
/// request bounds shared by both constructors.
pub fn scoped_router<S>(
    backend: Arc<dyn SyncBackend>,
    resolver: Arc<dyn ConflictResolver>,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    build_router(backend, resolver, true)
}

/// Constant-time string equality for bearer-token checks in the auth
/// middleware mounted in front of [`router`] / [`scoped_router`].
///
/// A plain `==` short-circuits on the first differing byte, so response
/// timing leaks how much of a guessed token matched — this compares every
/// byte unconditionally (via `subtle`, the same primitive autumn's webhook
/// signature and CSRF checks use). Only the *length* of the expected token
/// is observable, which is not secret-dependent per guess.
///
/// This is pure equality: callers must still fail closed on an
/// unset/empty expected token *before* comparing (an empty expected value
/// would otherwise equal an empty presented one).
#[must_use]
pub fn constant_time_token_eq(presented: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Resolve the request's scope: the extension when present, else the
/// GLOBAL default ([`router`]) or a fail-closed rejection
/// ([`scoped_router`]).
fn request_scope(extension: Option<SyncScope>, require_scope: bool) -> Result<SyncScope, ()> {
    match extension {
        Some(scope) => Ok(scope),
        None if require_scope => Err(()),
        None => Ok(SyncScope::global()),
    }
}

fn scope_missing_response() -> axum::response::Response {
    tracing::error!(
        "sync request reached scoped_router without a SyncScope request \
         extension — the deployment's auth middleware must insert one \
         (rejecting, fail-closed)"
    );
    SCOPE_MISSING_REJECTION.into_response()
}

fn build_router<S>(
    backend: Arc<dyn SyncBackend>,
    resolver: Arc<dyn ConflictResolver>,
    require_scope: bool,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let push_backend = Arc::clone(&backend);
    axum::Router::new()
        .route(
            "/push",
            post(
                move |scope: Option<SyncScope>, Json(request): Json<PushRequest>| {
                    let backend = Arc::clone(&push_backend);
                    let resolver = Arc::clone(&resolver);
                    async move {
                        let Ok(scope) = request_scope(scope, require_scope) else {
                            return scope_missing_response();
                        };
                        if request.changes.len() > MAX_PUSH_CHANGES {
                            return (
                                StatusCode::PAYLOAD_TOO_LARGE,
                                format!(
                                    "push batch of {} changes exceeds the limit of \
                                     {MAX_PUSH_CHANGES}",
                                    request.changes.len()
                                ),
                            )
                                .into_response();
                        }
                        let result = tokio::task::spawn_blocking(move || {
                            backend.apply_push(scope.as_str(), &request, resolver.as_ref())
                        })
                        .await;
                        respond(result)
                    }
                },
            ),
        )
        .route(
            "/pull",
            get(
                move |scope: Option<SyncScope>, Query(query): Query<PullQuery>| {
                    let backend = Arc::clone(&backend);
                    async move {
                        let Ok(scope) = request_scope(scope, require_scope) else {
                            return scope_missing_response();
                        };
                        let limit = query.limit.clamp(1, MAX_PULL_LIMIT);
                        let session_start = query.session_start();
                        let result = tokio::task::spawn_blocking(move || {
                            backend.pull_since(scope.as_str(), query.cursor, limit, session_start)
                        })
                        .await;
                        respond(result)
                    }
                },
            ),
        )
}

fn respond<T: serde::Serialize>(
    result: Result<Result<T, SyncError>, tokio::task::JoinError>,
) -> axum::response::Response {
    match result {
        Ok(Ok(response)) => Json(response).into_response(),
        // Protocol violations (e.g. an upsert without a payload) are CLIENT
        // errors: answer 422 so the pushing device surfaces the bug instead
        // of retrying a batch the server will never accept.
        Ok(Err(err @ SyncError::Protocol(_))) => {
            tracing::warn!(error = %err, "sync request rejected");
            (StatusCode::UNPROCESSABLE_ENTITY, err.to_string()).into_response()
        }
        Ok(Err(err)) => {
            tracing::error!(error = %err, "sync request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "sync request panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn constant_time_token_eq_is_plain_equality() {
        assert!(super::constant_time_token_eq("sync-secret", "sync-secret"));
        assert!(!super::constant_time_token_eq("sync-secret", "sync-secreT"));
        assert!(!super::constant_time_token_eq("sync", "sync-secret"));
        assert!(!super::constant_time_token_eq("", "sync-secret"));
        // Pure equality: an empty expected token DOES equal an empty
        // presented one — callers must reject empty expected values before
        // comparing (the routers' documented fail-closed requirement).
        assert!(super::constant_time_token_eq("", ""));
    }
}

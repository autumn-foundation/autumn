//! What a granted capability actually *does*, and everything it is checked
//! against on the way (issue #1632).
//!
//! [`manifest`](super::manifest) decides what a plugin may ask for.
//! [`grants`](super::grants) decides what each grant is scoped to. This module
//! is where a request from the guest meets both, plus the quota and the audit
//! ledger, before it reaches a subsystem:
//!
//! ```text
//!  guest writes {"op":"call","call":"kv-get","key":"cart"}
//!        │
//!        ▼
//!  granted?      ── no ─► denied: capability-not-granted   (recorded)
//!        │ yes
//!        ▼
//!  in scope?     ── no ─► denied: not-in-grant             (recorded)
//!        │ yes
//!        ▼
//!  under quota?  ── no ─► denied: quota-exceeded           (recorded)
//!        │ yes
//!        ▼
//!  derive the physical name from the manifest, never from the guest
//!        │
//!        ▼
//!  backend ─────────────► {"op":"call_result","id":1,"status":"ok",…} (recorded)
//! ```
//!
//! # The guest never names a physical thing
//!
//! Every step above is ordinary defence. The step that makes cross-tenant and
//! host-table access *impossible* rather than merely refused is the second to
//! last one: the guest names a **logical** key, table, host or job type, and the
//! host derives the physical name from the manifest and the ambient tenant. A
//! guest cannot ask for another tenant's row because there is no field in the
//! wire protocol where the tenant would go. It cannot ask for a host-application
//! table because the physical name is `plugin_<plugin>_<table>` and both halves
//! come from a validated manifest. It cannot smuggle SQL because no call carries
//! any.
//!
//! That is why the adversarial corpus in
//! `autumn/tests/integration/plugin_sandbox_capabilities.rs` is mostly about
//! *spelling*: an escape here would have to be a name that survives derivation,
//! not a missing check.
//!
//! # Denials are answers, not failures
//!
//! A refused call comes back as a `call_result` the guest can read. It does not
//! trap and it does not fail the request. A plugin that hits a ceiling should
//! degrade — render the panel without the live number — and an author debugging
//! one needs to see *which* rule refused. Every denial is also recorded in the
//! outcome's ledger and logged once, which is what the operator audit surface
//! is built from.
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

pub mod audit;
pub mod db;
pub mod jobs;
pub mod kv;
pub mod outbound;
pub mod quota;
pub mod render;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::grants::CapabilityQuotas;
use super::manifest::{SandboxCapability, SandboxManifest};
use quota::QuotaLedger;

pub use audit::{
    ActivitySummary, CapabilityEvent, CapabilityOutcome, MAX_LOG_EVENTS, PluginActivityLog,
};
// Aliased for use *inside* this module only, so the `pub use` above can carry
// the real names without shadowing itself.
use audit::{CapabilityEvent as Event, CapabilityOutcome as Outcome};
pub use db::{MemoryPluginStore, PluginStore, QueryPage, Scope, StoreError};
pub use jobs::MemoryJobSink;
pub use jobs::{JobSink, PluginJob};
pub use kv::{CacheKvStore, KvStore, MemoryKvStore};
pub use outbound::{
    ALLOWED_OUTBOUND_REQUEST_HEADERS, ALLOWED_OUTBOUND_RESPONSE_HEADERS, MAX_OUTBOUND_HEADERS,
    MAX_RESPONSE_HEADER_BYTES, OutboundHttp, OutboundRequest, OutboundResponse, RecordingHttp,
};
pub use quota::CapabilityRateLimiter;
pub use render::{ALLOWED_ATTRIBUTES, ALLOWED_TAGS, FragmentNode, RenderError};

/// The most columns one plugin row may carry.
///
/// A row crosses the wire as a map the guest wrote, is held in host memory, and
/// (for the DB capability) becomes a statement's column list. Every one of those
/// is work an untrusted map sizes, so the count is bounded before any of it
/// happens.
pub const MAX_ROW_COLUMNS: usize = 32;

/// Longest accepted text in one row value or KV key, in bytes.
pub const MAX_VALUE_TEXT_BYTES: usize = 64 * 1024;

/// Longest accepted logical KV key, in bytes.
///
/// The key is the one identifier a guest chooses freely — tables, hosts, job
/// types and slots all have to appear in the manifest — so it is the one that
/// needs a length of its own rather than inheriting a grant's.
pub const MAX_KV_KEY_BYTES: usize = 512;

/// Most bytes one row may carry across all of its columns.
///
/// [`MAX_VALUE_TEXT_BYTES`] bounds one column, and 32 of them bounds a row at
/// 2 MiB — twice what a whole reply may be. A row that cannot be handed back is
/// a row that must not be accepted, so the ceiling that lets a `db-get` answer
/// is applied where the row goes *in*.
pub const MAX_ROW_BYTES: usize = 256 * 1024;

/// Most bytes the rows in one `db-get` or `db-query` answer may carry.
///
/// Rows are the one result whose size the *guest* chooses: a `db_rows` quota of
/// 500 against [`MAX_ROW_BYTES`] is 125 MiB the host would materialise, encode
/// and hold, per request, on a quota the plugin's own manifest sets. So the
/// budget travels into the store (see [`PluginStore::query`]) rather than being
/// checked once the whole answer exists — by which point the allocation has
/// already happened.
///
/// Comfortably under the host's queued-reply ceiling, so a reply that fits this
/// always fits the queue and a legal read never fails a later call.
///
/// [`PluginStore::query`]: db::PluginStore::query
pub const MAX_RESULT_BYTES: usize = 512 * 1024;

/// Longest accepted plugin row id, in bytes.
pub const MAX_ROW_ID_BYTES: usize = 128;

// ── Values ───────────────────────────────────────────────────────────────

/// One scalar in a plugin row or job payload.
///
/// Deliberately **not** `serde_json::Value`. Arbitrary JSON is a recursive
/// structure an untrusted guest chooses the depth of, and every consumer of it
/// — the row encoder, the store, the job payload — would need its own depth
/// bound to be safe. A closed set of scalars has no depth to bound, so the only
/// ceilings left are a column count and a byte length, both checked once here.
///
/// The `untagged` representation is what a plugin author would write by hand:
/// `{"sku":"A-1","qty":3,"active":true}` rather than
/// `{"sku":{"text":"A-1"}}`. The variants are ordered so the match is
/// unambiguous — a JSON integer is `Int` because `Bool` cannot hold it and
/// `Int` is tried before `Float`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginValue {
    /// JSON `null`.
    Null,
    /// JSON `true` / `false`.
    Bool(bool),
    /// A JSON integer.
    Int(i64),
    /// A JSON number that is not an integer.
    Float(f64),
    /// A JSON string.
    Text(String),
}

impl PluginValue {
    /// The bytes this value costs in host memory, for the quota.
    #[must_use]
    pub const fn weight(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            _ => 8,
        }
    }
}

impl fmt::Display for PluginValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::Text(value) => f.write_str(value),
        }
    }
}

/// One row: column name to scalar, ordered so encodings are deterministic.
pub type PluginRow = BTreeMap<String, PluginValue>;

/// Check one row against the bounds every backend inherits.
///
/// Checked in the host rather than in each backend, so a store implementation
/// an embedder wrote cannot be the place the bound is missing.
///
/// # Errors
///
/// Names the first rule the row broke.
pub fn check_row(row: &PluginRow) -> Result<(), String> {
    if row.len() > MAX_ROW_COLUMNS {
        return Err(format!(
            "a row may carry at most {MAX_ROW_COLUMNS} columns; this one carries {}",
            row.len()
        ));
    }
    let weight = row_weight(row);
    if weight > MAX_ROW_BYTES {
        return Err(format!(
            "a row may carry at most {MAX_ROW_BYTES} bytes across its columns; this one carries \
             {weight}"
        ));
    }
    for (column, value) in row {
        if !super::grants::is_grantable_ident(column) {
            return Err(format!(
                "column name {:?} is not a lower-case `[a-z][a-z0-9_]*` identifier; the host \
                 derives a physical column name from it",
                super::manifest::rejected(column)
            ));
        }
        if value.weight() > MAX_VALUE_TEXT_BYTES {
            return Err(format!(
                "column {column:?} carries {} bytes, over the {MAX_VALUE_TEXT_BYTES}-byte ceiling",
                value.weight()
            ));
        }
    }
    Ok(())
}

/// What one row costs in host memory: its values, plus its column names.
///
/// Column names count because they are repeated in every row of an answer and
/// in the JSON of every reply, so a row of 32 long names is not free.
#[must_use]
pub fn row_weight(row: &PluginRow) -> usize {
    row.iter().fold(0, |sum, (column, value)| {
        sum.saturating_add(column.len())
            .saturating_add(value.weight())
    })
}

/// Keep rows while they fit [`MAX_RESULT_BYTES`], reporting whether any were
/// left behind.
///
/// The second line of defence, not the first: [`PluginStore::query`] is given
/// the same budget so a well-behaved store never builds the rows this would
/// discard. It runs anyway, because the store is an embedder's trait
/// implementation and "the host does not hold more than this" cannot rest on
/// someone else's loop.
///
/// A row the host would have accepted always survives: [`MAX_ROW_BYTES`] is
/// under this ceiling, so a `db-get` of a legally-stored row is never answered
/// with nothing. A row *larger* than the whole result budget is dropped rather
/// than exempted — it can only have come from a store that returned what the
/// host would never have stored, and passing it through is exactly the
/// oversized reply this exists to refuse.
///
/// [`PluginStore::query`]: db::PluginStore::query
#[must_use]
pub fn bounded_rows(rows: Vec<PluginRow>) -> (Vec<PluginRow>, bool) {
    let mut kept = Vec::with_capacity(rows.len());
    let mut total = 0_usize;
    let mut truncated = false;
    for row in rows {
        let weight = row_weight(&row);
        // No "but keep the first one" exemption. Letting an empty `kept` skip
        // the ceiling meant one corrupt or oversized row from an embedder's
        // store was serialized in full — and a single row can be arbitrarily
        // large when it did not come through `check_row`.
        if total.saturating_add(weight) > MAX_RESULT_BYTES {
            truncated = true;
            break;
        }
        total = total.saturating_add(weight);
        kept.push(row);
    }
    (kept, truncated)
}

// ── The calls ────────────────────────────────────────────────────────────

/// One thing a guest asks the host to do on its behalf.
///
/// Internally tagged by `call`, so a frame reads as
/// `{"op":"call","call":"kv-get","id":1,"key":"cart"}` — one flat object a
/// plugin author can write without a code generator, which is the same reason
/// the request frame is what it is.
///
/// There is no variant carrying SQL, a URL path on the host, a tenant, or a
/// physical table name, and that absence is the design: a call cannot ask for
/// something out of scope because the wire has nowhere to put it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum CapabilityCall {
    /// Read one key from the plugin's per-tenant namespace.
    KvGet {
        /// Correlation id the result echoes.
        id: u64,
        /// The logical key.
        key: String,
    },
    /// Write one key.
    KvSet {
        /// Correlation id.
        id: u64,
        /// The logical key.
        key: String,
        /// The value.
        value: PluginValue,
    },
    /// Delete one key.
    KvDelete {
        /// Correlation id.
        id: u64,
        /// The logical key.
        key: String,
    },
    /// Call a declared upstream.
    HttpFetch {
        /// Correlation id.
        id: u64,
        /// HTTP method.
        method: String,
        /// Absolute URL. Its host must appear in `[grants].hosts`.
        url: String,
        /// Request headers.
        #[serde(default)]
        headers: Vec<(String, String)>,
        /// Request body, as text.
        #[serde(default)]
        body: String,
    },
    /// Insert one row into a declared plugin table.
    DbInsert {
        /// Correlation id.
        id: u64,
        /// Logical table name, which must appear in `[grants].tables`.
        table: String,
        /// The row.
        row: PluginRow,
    },
    /// Read one row by id.
    DbGet {
        /// Correlation id.
        id: u64,
        /// Logical table name.
        table: String,
        /// The row id, as returned by `db-insert`.
        row_id: String,
    },
    /// Read rows matching an equality filter.
    DbQuery {
        /// Correlation id.
        id: u64,
        /// Logical table name.
        table: String,
        /// Column equality filter; an empty map matches every row the plugin
        /// and tenant own.
        #[serde(default)]
        filter: PluginRow,
        /// Largest number of rows to return, capped by the `db_rows` quota.
        #[serde(default)]
        limit: u32,
        /// Resume after this `row_id`, for reading past one page.
        ///
        /// A page can end early on the `db_rows` quota or on
        /// [`MAX_RESULT_BYTES`], and `truncated` tells the guest that happened —
        /// but without a place to say "carry on from here", the same filter
        /// would return the same prefix forever and the rows behind it would be
        /// unreachable. Rows come back in ascending `row_id` order (see
        /// [`PluginStore::query`](db::PluginStore::query)), so the last
        /// `row_id` of one page is the `after` of the next.
        #[serde(default)]
        after: Option<String>,
    },
    /// Replace one row by id.
    DbUpdate {
        /// Correlation id.
        id: u64,
        /// Logical table name.
        table: String,
        /// The row id.
        row_id: String,
        /// The replacement row.
        row: PluginRow,
    },
    /// Delete one row by id.
    DbDelete {
        /// Correlation id.
        id: u64,
        /// Logical table name.
        table: String,
        /// The row id.
        row_id: String,
    },
    /// Enqueue one declared job type.
    JobEnqueue {
        /// Correlation id.
        id: u64,
        /// The job type, which must appear in `[grants].job_types`.
        job_type: String,
        /// The job's arguments.
        #[serde(default)]
        payload: PluginRow,
    },
}

impl CapabilityCall {
    /// The correlation id the result echoes.
    #[must_use]
    pub const fn id(&self) -> u64 {
        match self {
            Self::KvGet { id, .. }
            | Self::KvSet { id, .. }
            | Self::KvDelete { id, .. }
            | Self::HttpFetch { id, .. }
            | Self::DbInsert { id, .. }
            | Self::DbGet { id, .. }
            | Self::DbQuery { id, .. }
            | Self::DbUpdate { id, .. }
            | Self::DbDelete { id, .. }
            | Self::JobEnqueue { id, .. } => *id,
        }
    }

    /// The capability this call needs.
    #[must_use]
    pub const fn capability(&self) -> SandboxCapability {
        match self {
            Self::KvGet { .. } | Self::KvSet { .. } | Self::KvDelete { .. } => {
                SandboxCapability::Kv
            }
            Self::HttpFetch { .. } => SandboxCapability::HttpOutbound,
            Self::DbInsert { .. }
            | Self::DbGet { .. }
            | Self::DbQuery { .. }
            | Self::DbUpdate { .. }
            | Self::DbDelete { .. } => SandboxCapability::Db,
            Self::JobEnqueue { .. } => SandboxCapability::Jobs,
        }
    }

    /// The logical thing this call names, before any check has run.
    ///
    /// Distinct from the *validated* target: a call refused for naming a host
    /// the manifest never granted still names one, and an operator asking "what
    /// did this plugin reach for" is asking exactly about those. Recording only
    /// what passed would make the audit surface answer the easy half of the
    /// question.
    ///
    /// The URL case reports the **host**, not the URL: a URL is a guest-chosen
    /// string with a query string on it, and the audit ledger is a bounded
    /// buffer that is not the place to keep one.
    #[must_use]
    pub fn target(&self) -> String {
        match self {
            Self::KvGet { key, .. } | Self::KvSet { key, .. } | Self::KvDelete { key, .. } => {
                key.clone()
            }
            // The *authority* it reached for, even when `host_of` refuses the
            // URL — because the refusals are the interesting ones. Recording a
            // placeholder would put every userinfo, backslash and scheme-
            // confusion attempt under one label, so `attacker.test` in
            // `https://api.example.com@attacker.test/` would never appear in the
            // one surface built to show what a plugin reached for.
            Self::HttpFetch { url, .. } => {
                outbound::host_of(url).unwrap_or_else(|| outbound::attempted_authority(url))
            }
            Self::DbInsert { table, .. }
            | Self::DbGet { table, .. }
            | Self::DbQuery { table, .. }
            | Self::DbUpdate { table, .. }
            | Self::DbDelete { table, .. } => table.clone(),
            Self::JobEnqueue { job_type, .. } => job_type.clone(),
        }
    }

    /// The operation name the audit ledger records.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::KvGet { .. } => "kv-get",
            Self::KvSet { .. } => "kv-set",
            Self::KvDelete { .. } => "kv-delete",
            Self::HttpFetch { .. } => "http-fetch",
            Self::DbInsert { .. } => "db-insert",
            Self::DbGet { .. } => "db-get",
            Self::DbQuery { .. } => "db-query",
            Self::DbUpdate { .. } => "db-update",
            Self::DbDelete { .. } => "db-delete",
            Self::JobEnqueue { .. } => "job-enqueue",
        }
    }
}

// ── The results ──────────────────────────────────────────────────────────

/// Why a call was refused.
///
/// A single machine-readable word, because a guest that can tell "you may not"
/// from "not this time" can degrade correctly, and an author reading a denial
/// needs to know which rule to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DenialReason {
    /// The manifest does not grant the capability this call needs.
    CapabilityNotGranted,
    /// The capability is granted, but `[grants]` does not name this target.
    NotInGrant,
    /// A per-request quota is spent.
    QuotaExceeded,
    /// The call itself is malformed — an over-long key, an unparseable URL.
    Malformed,
    /// The host has no backend wired for this capability.
    Unavailable,
    /// The backend was reached and refused or failed.
    BackendError,
    /// An upstream answered, and its answer was over a byte ceiling.
    ///
    /// Distinct from [`QuotaExceeded`](Self::QuotaExceeded) because the two mean
    /// opposite things to an operator: a quota denial means nothing left the
    /// host, and this means the call *was made* and the answer was discarded.
    /// The audit surface counts the host as called.
    ResponseTooLarge,
}

impl DenialReason {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityNotGranted => "capability-not-granted",
            Self::NotInGrant => "not-in-grant",
            Self::QuotaExceeded => "quota-exceeded",
            Self::Malformed => "malformed",
            Self::Unavailable => "unavailable",
            Self::BackendError => "backend-error",
            Self::ResponseTooLarge => "response-too-large",
        }
    }
}

impl fmt::Display for DenialReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the host hands back for one call.
///
/// `#[must_use]` because dropping it drops the guest's *answer*: the frame the
/// host owes it in return for the call it made.
#[must_use]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CallResult {
    /// The call succeeded.
    Ok {
        /// Correlation id, echoed.
        id: u64,
        /// The answer.
        value: CallValue,
    },
    /// The call was refused.
    Denied {
        /// Correlation id, echoed.
        id: u64,
        /// Which rule refused it.
        reason: DenialReason,
        /// One line for the author, already bounded and escaped.
        detail: String,
    },
}

impl CallResult {
    /// Build a denial.
    pub fn denied(id: u64, reason: DenialReason, detail: impl Into<String>) -> Self {
        Self::Denied {
            id,
            reason,
            detail: detail.into(),
        }
    }

    /// The correlation id.
    #[must_use]
    pub const fn id(&self) -> u64 {
        match self {
            Self::Ok { id, .. } | Self::Denied { id, .. } => *id,
        }
    }

    /// The denial reason, if this is one.
    #[must_use]
    pub const fn denial(&self) -> Option<DenialReason> {
        match self {
            Self::Ok { .. } => None,
            Self::Denied { reason, .. } => Some(*reason),
        }
    }
}

/// The answer to a successful call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CallValue {
    /// Nothing to say — a successful write or delete.
    Done,
    /// One optional scalar, from `kv-get`.
    Value {
        /// The value, or `null` for a miss. `found` distinguishes a stored
        /// `null` from a missing key, which a `null` alone could not.
        value: PluginValue,
        /// Whether the key existed.
        found: bool,
    },
    /// A row id, from `db-insert`.
    RowId {
        /// The id the store assigned.
        row_id: String,
    },
    /// Rows, from `db-get` and `db-query`.
    Rows {
        /// The rows, each carrying its `row_id` alongside its columns.
        rows: Vec<PluginRow>,
        /// Whether [`MAX_RESULT_BYTES`] stopped the answer short.
        ///
        /// On the wire so a guest can tell "that is all of them" from "that is
        /// all that fits". Without it a plugin paging through its own table
        /// would read a short answer as the end of the table and stop, which is
        /// silent data loss in the plugin rather than in the host.
        truncated: bool,
    },
    /// An HTTP response, from `http-fetch`.
    Http {
        /// The upstream status code.
        status: u16,
        /// Response headers the host allowed through.
        headers: Vec<(String, String)>,
        /// Response body, as text.
        body: String,
    },
    /// A job id, from `job-enqueue`.
    JobId {
        /// The id the queue assigned.
        job_id: String,
    },
}

// ── The services ─────────────────────────────────────────────────────────

/// The backends a host will honour a capability against.
///
/// Deliberately **not** `#[non_exhaustive]`, unlike the types in this module
/// that the host produces and a caller only reads: this one is built by the
/// embedder, and `CapabilityServices { kv: …, ..CapabilityServices::none() }` is
/// how. Marking it would make that spelling impossible outside this crate,
/// which is the only spelling there is.
///
/// Every field is optional, and a missing one is not a hole: a call whose
/// backend is absent is denied [`DenialReason::Unavailable`] and recorded, the
/// same as any other refusal. That is what lets `SandboxHost::run` — which has
/// no application state at all — keep working unchanged, and what lets a test
/// exercise one capability without standing up the other four.
#[derive(Clone, Default)]
pub struct CapabilityServices {
    /// The tenant every scoped capability binds to.
    ///
    /// `None` is the single-tenant application: keys and rows land in a
    /// namespace of their own rather than in some other tenant's.
    pub tenant: Option<String>,
    /// Backing store for `kv`.
    pub kv: Option<Arc<dyn KvStore>>,
    /// Client for `http-outbound`.
    pub http: Option<Arc<dyn OutboundHttp>>,
    /// Store for `db`.
    pub db: Option<Arc<dyn PluginStore>>,
    /// Queue for `jobs`.
    pub jobs: Option<Arc<dyn JobSink>>,
    /// The plugin's own calls-per-second ceiling, shared across its requests.
    ///
    /// `None` leaves only the per-request counters, which is the right default
    /// for a direct `SandboxHost::run` caller: a rate ceiling with no shared
    /// state is a ceiling that resets every call, which is worse than saying it
    /// is not there. `SandboxedPlugin` builds one per plugin.
    pub rate: Option<Arc<quota::CapabilityRateLimiter>>,
}

impl fmt::Debug for CapabilityServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityServices")
            .field("tenant", &self.tenant)
            .field("kv", &self.kv.is_some())
            .field("http", &self.http.is_some())
            .field("db", &self.db.is_some())
            .field("jobs", &self.jobs.is_some())
            .field("rate", &self.rate.is_some())
            .finish()
    }
}

impl CapabilityServices {
    /// No backends at all — every capability call is `unavailable`.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The same services, bound to `tenant`.
    #[must_use]
    pub fn for_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }
}

// ── The runtime ──────────────────────────────────────────────────────────

/// The tenant namespace segment used when the application is single-tenant.
///
/// A literal rather than an empty string, so a single-tenant deployment's keys
/// are as unambiguously namespaced as a multi-tenant one's — and so that a
/// tenant later appearing does not collide with the keys written before it.
///
/// **Never spellable by a real tenant.** A tenant id has no charset
/// restriction, so a literal any tenant could also be named would put a
/// deployment's tenant `_` in the same namespace as its single-tenant
/// requests — reachable in a tenancy migration, or with the plugin router
/// mounted outside the tenancy middleware. Presence is therefore encoded in the
/// segment itself: see [`tenant_segment`].
pub const NO_TENANT: &str = "-";

/// Prefix marking a namespace segment that names a real tenant.
const TENANT_PRESENT: char = 't';

/// The namespace segment for `tenant`, injective in `Option<&str>`.
///
/// A present tenant is `TENANT_PRESENT` followed by its id; absence is
/// [`NO_TENANT`], which no present tenant can produce because every one of them
/// starts with that prefix. This is the whole of "an absent tenant is not a
/// tenant named `_`", and it is a derivation rather than a check for the same
/// reason the physical table name is: the colliding spelling stops existing
/// instead of being refused.
#[must_use]
pub fn tenant_segment(tenant: Option<&str>) -> String {
    tenant.map_or_else(
        || NO_TENANT.to_owned(),
        |id| format!("{TENANT_PRESENT}{id}"),
    )
}

/// Enforcement, once, for every capability.
///
/// Holds the manifest's grants and quotas, the tenant, the backends and the
/// ledger, so that "is this allowed" is answered in one place rather than in
/// five backends that each have to remember.
pub struct CapabilityRuntime {
    /// Read by each capability module to name itself in a derived key or a
    /// job record; never written after construction.
    pub(super) plugin: String,
    capabilities: Vec<SandboxCapability>,
    /// Read by the outbound module to hand an implementation the host list it
    /// needs for a per-hop decision; never written after construction.
    pub(super) grants: super::grants::CapabilityGrants,
    /// The backends. `pub(super)` so each capability module reaches its own and
    /// no more; nothing outside this module tree can substitute one.
    pub(super) services: CapabilityServices,
    quotas: QuotaLedger,
    rate: Option<Arc<quota::CapabilityRateLimiter>>,
    events: Vec<Event>,
    /// Calls the ledger could not hold, so a truncated summary says it is one.
    dropped_events: u64,
    /// `(operation, outcome)` pairs already warned about this request.
    warned: Vec<(&'static str, Outcome)>,
}

/// The most distinct `(operation, outcome)` pairs one request warns about.
///
/// Both sets are closed — ten operations, a handful of outcomes — so this is a
/// ceiling the vocabulary already implies rather than a policy. It exists so the
/// scan above stays bounded if either set grows.
const MAX_WARNED: usize = 64;

/// Truncate a guest-chosen target to [`MAX_TARGET_CHARS`] *characters*.
///
/// By characters, before escaping: `rejected` expands a hostile character to as
/// many as ten bytes, so truncating afterwards would still have materialised the
/// expansion first.
fn bounded_target(target: &str) -> String {
    let mut chars = target.chars();
    let kept: String = chars.by_ref().take(MAX_TARGET_CHARS).collect();
    if chars.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}

/// Characters of a guest-chosen target the audit ledger keeps.
///
/// Deliberately far below the 512 characters the manifest module's `rejected`
/// escaper allows. A ledger entry answers "which table / host / key", and no honest one
/// needs a paragraph — while `rejected` escapes a hostile character to as many
/// as ten bytes, so 512 characters is up to 5 KiB per entry and
/// [`MAX_EVENTS`] of those is megabytes the request footprint never budgeted.
pub const MAX_TARGET_CHARS: usize = 96;

/// The most events one request's ledger holds.
///
/// Bounded for the same reason the denial ledger is: a guest that calls in a
/// loop must not be able to turn the evidence of it into the host's
/// memory-exhaustion channel. The per-request `calls` quota bounds the count
/// long before this does; this is the backstop for a build whose quota was
/// raised.
pub const MAX_EVENTS: usize = 512;

impl fmt::Debug for CapabilityRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityRuntime")
            .field("plugin", &self.plugin)
            .field("capabilities", &self.capabilities)
            .field("events", &self.events.len())
            .finish_non_exhaustive()
    }
}

impl CapabilityRuntime {
    /// Build the runtime one request will use.
    #[must_use]
    pub fn new(manifest: &SandboxManifest, services: CapabilityServices) -> Self {
        Self {
            plugin: manifest.name.clone(),
            capabilities: manifest.capabilities.clone(),
            grants: manifest.grants.clone(),
            rate: services.rate.clone(),
            services,
            quotas: QuotaLedger::new(manifest.quotas),
            events: Vec::new(),
            dropped_events: 0,
            warned: Vec::new(),
        }
    }

    /// The quotas this runtime enforces.
    #[must_use]
    pub const fn quotas(&self) -> &CapabilityQuotas {
        self.quotas.declared()
    }

    /// The tenant every scoped call binds to, as the caller supplied it.
    ///
    /// `None` when the caller supplied none. That is right for a single-tenant
    /// application and **wrong** for a multi-tenant one whose plugin router was
    /// mounted outside the tenancy middleware: every tenant's keys and rows
    /// would pool into one namespace. Nothing inside the sandbox can tell those
    /// two apart — `CURRENT_TENANT` is simply absent in both — so the caller is
    /// the one that has to know, which is why
    /// [`CapabilityServices::for_tenant`] exists and why
    /// `SandboxedPlugin::serve` reads the task-local on the async task rather
    /// than inside `spawn_blocking`, where it is absent for a third reason.
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.services.tenant.as_deref()
    }

    /// The namespace segment every scoped derivation uses.
    ///
    /// [`tenant_segment`] of [`tenant`](Self::tenant): the *physical* tenant,
    /// the way [`physical_table`](db::physical_table) is the physical table.
    /// Never the raw id, because the raw id cannot say whether there was one.
    #[must_use]
    pub fn tenant_key(&self) -> String {
        tenant_segment(self.tenant())
    }

    /// Everything this request did, in order.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Take the ledger, leaving it empty.
    #[must_use]
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Calls this request made that the bounded ledger could not hold.
    ///
    /// Non-zero means the activity summary is a floor, not a count — which is
    /// the one thing an operator must not have to guess at.
    #[must_use]
    pub const fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Whether this plugin may use `capability` at all.
    ///
    /// `is_granted`, not `grants`, for the reason
    /// [`SandboxManifest::is_granted`](super::manifest::SandboxManifest::is_granted)
    /// carries the same name: `grants` is the field holding the scope lists.
    #[must_use]
    pub fn is_granted(&self, capability: SandboxCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Answer one call.
    ///
    /// Never fails and never panics: everything a guest can get wrong comes back
    /// as a [`CallResult::Denied`] it can read.
    pub fn dispatch(&mut self, call: &CapabilityCall) -> CallResult {
        let id = call.id();
        let capability = call.capability();
        // What the call *names*, computed before any check, so every refusal
        // records what was reached for rather than a dash. The operator surface
        // is built from these, and "this plugin tried to read `users`" is the
        // half of the answer a validated-target-only ledger would drop.
        //
        // Bounded on the way in: `record` truncates and neutralises it, because
        // a KV key is a guest-chosen string that ends up in a log line and in
        // the rendered summary.
        let named = call.target();

        // The shared `calls` budget is charged first, for every dispatch —
        // including one that is about to be refused. A refusal is cheaper than
        // a call but it is not free: it costs a grant scan, an encoded reply, a
        // ledger entry and a log line, and leaving refusals unmetered let a
        // plugin with a generous fuel budget emit ~10^5 warn lines per request
        // against a `calls` quota of 128. Fuel alone is the wrong ceiling for
        // work whose cost is a log write.
        //
        // The *per-capability* counters stay where they are, below, so a call
        // refused before it could reach a backend does not spend the budget
        // that bounds backend work.
        if let Err(field) = self.quotas.charge_call() {
            return self.record(
                call,
                &named,
                CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!("the per-request `{field}` quota is spent"),
                ),
            );
        }

        // Then the capability, before the scope: a plugin that was never
        // granted `db` must learn *that*, not that some table is not in a list
        // it does not have.
        if !self.is_granted(capability) {
            return self.record(
                call,
                &named,
                CallResult::denied(
                    id,
                    DenialReason::CapabilityNotGranted,
                    format!("this plugin was not granted `{capability}`"),
                ),
            );
        }

        // Scope before the per-capability charge: an out-of-scope target never
        // reaches a backend, so spending the budget that bounds backend work on
        // it would let one manifest mistake starve the calls the plugin is
        // entitled to make.
        let target = match self.target_of(call) {
            Ok(target) => target,
            Err(result) => return self.record(call, &named, result),
        };

        if let Err(field) = self.quotas.charge_capability(call) {
            return self.record(
                call,
                &target,
                CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!("the per-request `{field}` quota is spent"),
                ),
            );
        }

        // The rate ceiling last of the three, and after the per-request charge:
        // it is the only one that depends on what *other* requests did, so a
        // plugin reading its denials sees "I asked too much" before "everyone
        // asked too much at once", which is the order the two are fixable in.
        if self
            .rate
            .as_ref()
            .is_some_and(|rate| !rate.try_take(capability))
        {
            return self.record(
                call,
                &target,
                CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!("this plugin is over its `{capability}` calls-per-second ceiling"),
                ),
            );
        }

        let result = self.perform(call, &target);
        self.record(call, &target, result)
    }

    /// The thing this call names, checked against the grant list.
    ///
    /// Returns the *logical* target — the same string
    /// [`CapabilityCall::target`] names, once it has been checked against the
    /// grant list. A physical name is derived later and is an implementation
    /// detail nobody auditing the plugin asked about.
    fn target_of(&self, call: &CapabilityCall) -> Result<String, CallResult> {
        let id = call.id();
        match call {
            CapabilityCall::KvGet { key, .. }
            | CapabilityCall::KvSet { key, .. }
            | CapabilityCall::KvDelete { key, .. } => {
                // The one identifier a guest chooses freely, so it is the one
                // with a shape rule of its own. Control characters are refused
                // because the key is printed in the audit surface an operator
                // reads.
                if key.is_empty() || key.len() > MAX_KV_KEY_BYTES {
                    return Err(CallResult::denied(
                        id,
                        DenialReason::Malformed,
                        format!("a kv key must be 1..={MAX_KV_KEY_BYTES} bytes"),
                    ));
                }
                if key.chars().any(char::is_control) {
                    return Err(CallResult::denied(
                        id,
                        DenialReason::Malformed,
                        "a kv key may not carry control characters",
                    ));
                }
                Ok(key.clone())
            }
            CapabilityCall::HttpFetch { url, .. } => {
                let host = outbound::host_of(url).ok_or_else(|| {
                    CallResult::denied(
                        id,
                        DenialReason::Malformed,
                        "expected an absolute http/https URL with a bare host and no userinfo",
                    )
                })?;
                self.require_grant(id, SandboxCapability::HttpOutbound, &host, "hosts")?;
                Ok(host)
            }
            CapabilityCall::DbInsert { table, .. }
            | CapabilityCall::DbGet { table, .. }
            | CapabilityCall::DbQuery { table, .. }
            | CapabilityCall::DbUpdate { table, .. }
            | CapabilityCall::DbDelete { table, .. } => {
                self.require_grant(id, SandboxCapability::Db, table, "tables")?;
                Ok(table.clone())
            }
            CapabilityCall::JobEnqueue { job_type, .. } => {
                self.require_grant(id, SandboxCapability::Jobs, job_type, "job_types")?;
                Ok(job_type.clone())
            }
        }
    }

    fn require_grant(
        &self,
        id: u64,
        capability: SandboxCapability,
        entry: &str,
        field: &'static str,
    ) -> Result<(), CallResult> {
        if self.grants.allows(capability, entry) {
            return Ok(());
        }
        Err(CallResult::denied(
            id,
            DenialReason::NotInGrant,
            format!(
                "`[grants].{field}` does not name {entry:?}",
                entry = super::manifest::rejected(entry)
            ),
        ))
    }

    /// Run the call against its backend, everything already checked.
    fn perform(&self, call: &CapabilityCall, target: &str) -> CallResult {
        match call {
            CapabilityCall::KvGet { .. }
            | CapabilityCall::KvSet { .. }
            | CapabilityCall::KvDelete { .. } => kv::perform(self, call, target),
            CapabilityCall::HttpFetch { .. } => outbound::perform(self, call, target),
            CapabilityCall::DbInsert { .. }
            | CapabilityCall::DbGet { .. }
            | CapabilityCall::DbQuery { .. }
            | CapabilityCall::DbUpdate { .. }
            | CapabilityCall::DbDelete { .. } => db::perform(self, call, target),
            CapabilityCall::JobEnqueue { .. } => jobs::perform(self, call, target),
        }
    }

    /// Record one call and hand the result back unchanged.
    ///
    /// Every exit from `dispatch` goes through here, so there is no path that
    /// answers the guest without also telling the operator.
    fn record(&mut self, call: &CapabilityCall, target: &str, result: CallResult) -> CallResult {
        // Bounded and neutralised once, here, rather than at each of the six
        // exits. A KV key is the one identifier a guest chooses freely, a table
        // or job type is bounded only by the wire, and from here both go into a
        // `tracing` line and the rendered operator summary — which a terminal
        // renders. `MAX_TARGET_CHARS` is far below what `rejected` would allow,
        // because a ledger this deep with 512-character entries is megabytes the
        // footprint never budgeted.
        let target = super::manifest::rejected(&bounded_target(target));
        let outcome = match result.denial() {
            None => Outcome::Allowed,
            Some(DenialReason::QuotaExceeded) => Outcome::QuotaExceeded,
            Some(reason) => Outcome::Denied(reason),
        };
        if matches!(outcome, Outcome::Allowed) {
            tracing::debug!(
                plugin = self.plugin,
                capability = call.capability().as_str(),
                operation = call.operation(),
                target = target.as_str(),
                "sandboxed plugin used a granted capability"
            );
        } else if self
            .warned
            .iter()
            .any(|(operation, seen)| *operation == call.operation() && *seen == outcome)
        {
            // Already said, this request. Deduplicated by `(operation,
            // outcome)` exactly as `HostState::deny` deduplicates a refused
            // import, and for the same reason: a guest that calls in a loop
            // must not be able to turn the evidence of it into the operator's
            // log-storage problem. The ledger below still counts every one, so
            // nothing is lost — only repeated.
        } else {
            // Denials are warned, once, at the point they happen — whoever is
            // driving the host should not have to remember to log it.
            tracing::warn!(
                plugin = self.plugin,
                capability = call.capability().as_str(),
                operation = call.operation(),
                target = target.as_str(),
                outcome = %outcome,
                "sandboxed plugin was refused a capability call"
            );
            // Bounded by the vocabulary: at most one entry per (operation,
            // outcome) pair, and both sets are closed.
            if self.warned.len() < MAX_WARNED {
                self.warned.push((call.operation(), outcome));
            }
        }
        if self.events.len() < MAX_EVENTS {
            self.events.push(Event {
                capability: call.capability(),
                operation: call.operation(),
                target,
                outcome,
            });
        } else {
            // Counted rather than dropped silently. The ledger is bounded
            // because a guest chooses how many entries it makes, but an
            // operator reading "12 db-inserts" off a truncated ledger would be
            // reading a number the plugin chose. `MAX_EVENTS` is not
            // unreachable: the quotas that would bound it live in the plugin's
            // own manifest, so the plugin sets them.
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::grants::MAX_QUOTA;
    use super::*;
    use jobs::MemoryJobSink;
    use kv::MemoryKvStore;
    use outbound::{OutboundResponse, RecordingHttp};

    /// A manifest granting everything, so each test can turn one thing off
    /// rather than assembling a manifest per case.
    fn manifest(capabilities: &[SandboxCapability]) -> SandboxManifest {
        let caps = capabilities
            .iter()
            .map(|capability| format!("\"{}\"", capability.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut grants = String::from("[grants]\n");
        if capabilities.contains(&SandboxCapability::HttpOutbound) {
            grants.push_str("hosts = [\"api.example.com\"]\n");
        }
        if capabilities.contains(&SandboxCapability::Db) {
            grants.push_str("tables = [\"orders\"]\n");
        }
        if capabilities.contains(&SandboxCapability::Jobs) {
            grants.push_str("job_types = [\"reindex\"]\n");
        }
        if capabilities.contains(&SandboxCapability::Render) {
            grants.push_str("slots = [\"order-summary\"]\n");
        }
        SandboxManifest::parse(&format!(
            r#"
name = "shop"
version = "0.1.0"
wire_version = 1
prefix = "/shop"
capabilities = [{caps}]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/shop/panel"

{grants}
"#,
            digest = "c".repeat(64)
        ))
        .expect("the fixture manifest is valid")
    }

    fn everything() -> Vec<SandboxCapability> {
        SandboxCapability::ALL.to_vec()
    }

    struct Fixture {
        runtime: CapabilityRuntime,
        kv: Arc<MemoryKvStore>,
        db: Arc<db::MemoryPluginStore>,
        jobs: Arc<MemoryJobSink>,
        http: Arc<RecordingHttp>,
    }

    fn fixture(capabilities: &[SandboxCapability], tenant: Option<&str>) -> Fixture {
        let kv = MemoryKvStore::new();
        let db = db::MemoryPluginStore::new();
        let jobs = MemoryJobSink::new();
        let http = RecordingHttp::new();
        let mut services = CapabilityServices {
            kv: Some(Arc::clone(&kv) as Arc<dyn KvStore>),
            db: Some(Arc::clone(&db) as Arc<dyn PluginStore>),
            jobs: Some(Arc::clone(&jobs) as Arc<dyn JobSink>),
            http: Some(Arc::clone(&http) as Arc<dyn OutboundHttp>),
            ..CapabilityServices::none()
        };
        if let Some(tenant) = tenant {
            services = services.for_tenant(tenant);
        }
        Fixture {
            runtime: CapabilityRuntime::new(&manifest(capabilities), services),
            kv,
            db,
            jobs,
            http,
        }
    }

    fn row(pairs: &[(&str, &str)]) -> PluginRow {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), PluginValue::Text((*value).to_owned())))
            .collect()
    }

    // ── Capability gating ────────────────────────────────────────────

    #[test]
    fn an_ungranted_capability_is_denied_and_recorded() {
        let mut fixture = fixture(&[SandboxCapability::HttpRequest], None);
        let result = fixture.runtime.dispatch(&CapabilityCall::KvGet {
            id: 1,
            key: "cart".to_owned(),
        });
        assert_eq!(result.denial(), Some(DenialReason::CapabilityNotGranted));
        assert_eq!(fixture.runtime.events().len(), 1);
        assert_eq!(
            fixture.runtime.events().first().map(|event| event.outcome),
            Some(audit::CapabilityOutcome::Denied(
                DenialReason::CapabilityNotGranted
            ))
        );
        assert!(fixture.kv.keys().is_empty(), "nothing was written");
    }

    #[test]
    fn a_refusal_records_what_was_reached_for_not_a_dash() {
        // "What did this plugin do" includes what it *tried* to do. A ledger
        // that named only the targets that passed would answer the easy half.
        let mut granted = fixture(&everything(), Some("alpha"));
        let _ = granted.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://attacker.test/steal".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        let _ = granted.runtime.dispatch(&CapabilityCall::DbQuery {
            id: 2,
            table: "users".to_owned(),
            filter: PluginRow::new(),
            limit: 1,
            after: None,
        });
        let _ = granted.runtime.dispatch(&CapabilityCall::JobEnqueue {
            id: 3,
            job_type: "drain-accounts".to_owned(),
            payload: PluginRow::new(),
        });
        let targets: Vec<&str> = granted
            .runtime
            .events()
            .iter()
            .map(|event| event.target.as_str())
            .collect();
        assert_eq!(targets, vec!["attacker.test", "users", "drain-accounts"]);

        // And it is neutralised on the way in: the one identifier a guest
        // chooses freely ends up in a log line and in the operator summary.
        let mut hostile = fixture(&[SandboxCapability::HttpRequest], None);
        let _ = hostile.runtime.dispatch(&CapabilityCall::KvGet {
            id: 4,
            key: "a\u{1b}[2K\rforged".to_owned(),
        });
        let recorded = hostile
            .runtime
            .events()
            .first()
            .map(|event| event.target.clone())
            .unwrap_or_default();
        assert!(!recorded.contains('\u{1b}'), "{recorded:?}");
    }

    #[test]
    fn an_ungranted_call_cannot_spend_the_shared_call_budget() {
        // The order of the checks is load-bearing: if a call to an ungranted
        // capability charged the shared `calls` quota first, a plugin could be
        // starved of the surface it *was* granted by one it was not.
        let mut ungranted = fixture(&[SandboxCapability::HttpRequest], None);
        for id in 0..1_000 {
            let _ = ungranted.runtime.dispatch(&CapabilityCall::KvGet {
                id,
                key: "cart".to_owned(),
            });
        }
        // The ledger is bounded, but nothing was ever charged: a granted call
        // on a fresh runtime with the same quotas still succeeds.
        let mut granted = fixture(&everything(), None);
        assert!(
            granted
                .runtime
                .dispatch(&CapabilityCall::KvGet {
                    id: 1,
                    key: "cart".to_owned(),
                })
                .denial()
                .is_none()
        );
    }

    // ── KV containment ───────────────────────────────────────────────

    #[test]
    fn one_tenants_key_is_unreadable_when_another_is_active() {
        let store = MemoryKvStore::new();
        let services = |tenant: &str| {
            CapabilityServices {
                kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
                ..CapabilityServices::none()
            }
            .for_tenant(tenant)
        };

        let manifest = manifest(&everything());
        let mut alpha = CapabilityRuntime::new(&manifest, services("alpha"));
        let _ = alpha.dispatch(&CapabilityCall::KvSet {
            id: 1,
            key: "cart".to_owned(),
            value: PluginValue::Text("alpha's".to_owned()),
        });

        let mut beta = CapabilityRuntime::new(&manifest, services("beta"));
        let result = beta.dispatch(&CapabilityCall::KvGet {
            id: 2,
            key: "cart".to_owned(),
        });
        assert_eq!(
            result,
            CallResult::Ok {
                id: 2,
                value: CallValue::Value {
                    value: PluginValue::Null,
                    found: false
                }
            },
            "tenant beta must not see tenant alpha's key"
        );
    }

    #[test]
    fn no_tenant_key_can_be_spelled_to_reach_another_tenants_namespace() {
        // The escape a naive `format!("{plugin}:{tenant}:{key}")` would allow:
        // tenant `a` asking for the key `b:cart` lands where tenant `b` writes
        // `cart`. Both segments are escaped, so the two keys stay distinct.
        let store = MemoryKvStore::new();
        let manifest = manifest(&everything());
        let services = |tenant: &str| {
            CapabilityServices {
                kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
                ..CapabilityServices::none()
            }
            .for_tenant(tenant)
        };

        let mut beta = CapabilityRuntime::new(&manifest, services("b"));
        let _ = beta.dispatch(&CapabilityCall::KvSet {
            id: 1,
            key: "cart".to_owned(),
            value: PluginValue::Text("beta's".to_owned()),
        });

        let mut alpha = CapabilityRuntime::new(&manifest, services("a"));
        let result = alpha.dispatch(&CapabilityCall::KvGet {
            id: 2,
            key: "b:cart".to_owned(),
        });
        assert_eq!(
            result,
            CallResult::Ok {
                id: 2,
                value: CallValue::Value {
                    value: PluginValue::Null,
                    found: false
                }
            }
        );
        assert_eq!(store.keys().len(), 1, "{:?}", store.keys());
    }

    #[test]
    fn a_tenant_id_carrying_the_separator_cannot_collide_with_another() {
        // Tenant ids are the application's, not the manifest's: they come from
        // a header or a subdomain, and nothing validates their charset.
        assert_ne!(
            kv::namespaced_key("shop", Some("b%3Ax"), "k"),
            kv::namespaced_key("shop", Some("b"), "%3Ax:k")
        );
        assert_ne!(
            kv::namespaced_key("shop", Some("a"), "b:c"),
            kv::namespaced_key("shop", Some("a:b"), "c")
        );
    }

    #[test]
    fn two_plugins_never_share_a_key() {
        assert_ne!(
            kv::namespaced_key("shop", Some("t"), "cart"),
            kv::namespaced_key("other", Some("t"), "cart")
        );
    }

    #[test]
    fn no_tenant_is_not_a_tenant_that_could_be_named() {
        // A tenant id has no charset restriction, so the single-tenant sentinel
        // must be a segment no tenant could also be. It used to be `_`, which a
        // deployment can name a tenant — and then that tenant shared a
        // namespace with every request that arrived without one, reachable in a
        // tenancy migration or with the router mounted outside the tenancy
        // middleware.
        for spelling in [NO_TENANT, "_", "", "-", "t", "t-"] {
            assert_ne!(
                tenant_segment(Some(spelling)),
                tenant_segment(None),
                "a tenant named {spelling:?} reached the single-tenant namespace"
            );
            assert_ne!(
                kv::namespaced_key("shop", Some(spelling), "cart"),
                kv::namespaced_key("shop", None, "cart"),
                "{spelling:?}"
            );
        }
        // And distinct tenants stay distinct, which is the property the marker
        // must not have broken.
        assert_ne!(tenant_segment(Some("a")), tenant_segment(Some("b")));
    }

    #[test]
    fn a_stored_value_over_the_current_ceiling_is_refused_on_the_way_out() {
        // Lowering `kv_value_bytes` is meant to reduce authority, but the store
        // keeps what the old ceiling allowed. Serving it anyway would make the
        // tightened quota cosmetic, and a large enough value would overrun the
        // reply queue and fail the request rather than being refused.
        let kv = MemoryKvStore::new();
        let manifest = manifest(&everything());
        let key = kv::namespaced_key(&manifest.name, Some("alpha"), "cart");
        let _ = kv.set(&key, PluginValue::Text("x".repeat(4096)));

        let mut quotas = manifest.quotas;
        quotas.kv_value_bytes = 64;
        let tightened = SandboxManifest { quotas, ..manifest };
        let mut runtime = CapabilityRuntime::new(
            &tightened,
            CapabilityServices {
                kv: Some(Arc::clone(&kv) as Arc<dyn KvStore>),
                ..CapabilityServices::none()
            }
            .for_tenant("alpha"),
        );
        let result = runtime.dispatch(&CapabilityCall::KvGet {
            id: 1,
            key: "cart".to_owned(),
        });
        assert_eq!(
            result.denial(),
            Some(DenialReason::QuotaExceeded),
            "{result:?}"
        );
    }

    #[test]
    fn a_query_filter_may_not_carry_the_row_id_that_would_widen_it() {
        // `validated_row` strips `row_id` on a write, which is right: a row read
        // back carries its id and echoing it means nothing. On a *filter* the
        // same strip turns "the row with this id" into "every row this tenant
        // has" — narrowing that silently widens, which is the one direction a
        // containment bug can go and still look like it worked.
        let mut fixture = fixture(&everything(), Some("alpha"));
        for sku in ["A-1", "A-2", "A-3"] {
            let inserted = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
                id: 1,
                table: "orders".to_owned(),
                row: row(&[("sku", sku)]),
            });
            assert!(inserted.denial().is_none(), "{inserted:?}");
        }
        let result = fixture.runtime.dispatch(&CapabilityCall::DbQuery {
            id: 2,
            table: "orders".to_owned(),
            filter: row(&[(db::ID_COLUMN, "r1")]),
            limit: 0,
            after: None,
        });
        assert_eq!(result.denial(), Some(DenialReason::Malformed), "{result:?}");
    }

    #[test]
    fn a_truncated_page_can_be_continued_from_where_it_stopped() {
        // A ceiling the guest is told about and cannot act on is worse than no
        // ceiling: `truncated` would say "there is more" while the same filter
        // returned the same prefix forever.
        let mut fixture = fixture(&everything(), Some("alpha"));
        for sku in ["A-1", "A-2", "A-3", "A-4"] {
            let inserted = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
                id: 1,
                table: "orders".to_owned(),
                row: row(&[("sku", sku)]),
            });
            assert!(inserted.denial().is_none(), "{inserted:?}");
        }

        let page = |runtime: &mut CapabilityRuntime, after: Option<String>| {
            let result = runtime.dispatch(&CapabilityCall::DbQuery {
                id: 2,
                table: "orders".to_owned(),
                filter: PluginRow::new(),
                limit: 2,
                after,
            });
            let CallResult::Ok {
                value: CallValue::Rows { rows, .. },
                ..
            } = result
            else {
                panic!("the query should have succeeded: {result:?}")
            };
            rows
        };

        let first = page(&mut fixture.runtime, None);
        assert_eq!(first.len(), 2, "{first:?}");
        let last = first
            .last()
            .and_then(|row| row.get(db::ID_COLUMN))
            .map(ToString::to_string)
            .expect("every returned row carries its id");
        let second = page(&mut fixture.runtime, Some(last));
        assert_eq!(second.len(), 2, "{second:?}");
        // Disjoint, which is the whole property: a cursor that returned the
        // same rows again would page forever without advancing.
        for row in &second {
            assert!(!first.contains(row), "{row:?} came back twice");
        }
    }

    #[test]
    fn a_row_at_the_column_ceiling_survives_a_read_modify_write() {
        // The documented pattern: read a row, change a field, send it back. The
        // row comes back carrying the `row_id` the host added, so a row stored
        // at exactly `MAX_ROW_COLUMNS` arrived at `db-update` with one column
        // too many — refused for a column the guest never wrote. The ceilings
        // must be a function of what the plugin stored, not of where the row
        // had been.
        let mut fixture = fixture(&everything(), Some("alpha"));
        let full: PluginRow = (0..MAX_ROW_COLUMNS)
            .map(|index| {
                (
                    format!("c{index}"),
                    PluginValue::Int(i64::try_from(index).unwrap_or(0)),
                )
            })
            .collect();
        assert_eq!(full.len(), MAX_ROW_COLUMNS);

        let inserted = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 1,
            table: "orders".to_owned(),
            row: full,
        });
        let CallResult::Ok {
            value: CallValue::RowId { row_id },
            ..
        } = inserted
        else {
            panic!("a row at the ceiling should store: {inserted:?}")
        };

        let read = fixture.runtime.dispatch(&CapabilityCall::DbGet {
            id: 2,
            table: "orders".to_owned(),
            row_id: row_id.clone(),
        });
        let CallResult::Ok {
            value: CallValue::Rows { rows, .. },
            ..
        } = read
        else {
            panic!("the read should have succeeded: {read:?}")
        };
        let mut echoed = rows.first().cloned().unwrap_or_default();
        assert_eq!(
            echoed.len(),
            MAX_ROW_COLUMNS + 1,
            "the row comes back with the host's id, which is the whole problem"
        );
        echoed.insert("c0".to_owned(), PluginValue::Int(99));

        let updated = fixture.runtime.dispatch(&CapabilityCall::DbUpdate {
            id: 3,
            table: "orders".to_owned(),
            row_id,
            row: echoed,
        });
        assert!(
            updated.denial().is_none(),
            "writing back what was read must not be refused: {updated:?}"
        );
    }

    #[test]
    fn a_page_cut_by_the_row_quota_says_so_too() {
        // `truncated` is the guest's only signal that more rows exist. A page
        // cut by `db_rows` looked complete, and with `limit: 0` the guest is
        // never told the quota — so a full page and a finished table were
        // indistinguishable, and the cursor documented for continuing was never
        // reached.
        let base = manifest(&everything());
        let mut quotas = base.quotas;
        quotas.db_rows = 2;
        let manifest = SandboxManifest { quotas, ..base };
        let store = db::MemoryPluginStore::new();
        let mut runtime = CapabilityRuntime::new(
            &manifest,
            CapabilityServices {
                db: Some(Arc::clone(&store) as Arc<dyn PluginStore>),
                ..CapabilityServices::none()
            }
            .for_tenant("alpha"),
        );
        for sku in ["A-1", "A-2", "A-3", "A-4"] {
            let inserted = runtime.dispatch(&CapabilityCall::DbInsert {
                id: 1,
                table: "orders".to_owned(),
                row: row(&[("sku", sku)]),
            });
            assert!(inserted.denial().is_none(), "{inserted:?}");
        }

        let result = runtime.dispatch(&CapabilityCall::DbQuery {
            id: 2,
            table: "orders".to_owned(),
            filter: PluginRow::new(),
            limit: 0,
            after: None,
        });
        let CallResult::Ok {
            value: CallValue::Rows { rows, truncated },
            ..
        } = result
        else {
            panic!("the query should have succeeded: {result:?}")
        };
        assert_eq!(rows.len(), 2, "the quota caps the page");
        assert!(truncated, "and the guest is told there is more");
    }

    // ── Outbound containment ─────────────────────────────────────────

    #[test]
    fn a_declared_host_is_reachable_through_the_framework_client() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        fixture.http.answer(
            "https://api.example.com/v1/orders",
            OutboundResponse {
                status: 200,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: "[]".to_owned(),
                final_url: "https://api.example.com/v1/orders".to_owned(),
            },
        );
        let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "get".to_owned(),
            url: "https://api.example.com/v1/orders".to_owned(),
            headers: vec![("accept".to_owned(), "application/json".to_owned())],
            body: String::new(),
        });
        assert_eq!(
            result,
            CallResult::Ok {
                id: 1,
                value: CallValue::Http {
                    status: 200,
                    headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                    body: "[]".to_owned(),
                }
            }
        );
        assert_eq!(fixture.http.seen().len(), 1);
    }

    #[test]
    fn every_shape_of_undeclared_host_is_denied_and_never_leaves() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        for url in [
            "https://attacker.test/",
            "https://api.example.com.attacker.test/",
            "https://evil-api.example.com/",
            "https://api.example.com@attacker.test/",
            "https://attacker.test/?to=api.example.com",
            "http://attacker.test/api.example.com",
            "file:///etc/passwd",
            "//api.example.com/",
            "/relative",
            "https://API.EXAMPLE.COM.attacker.test/",
            "https://[::1]/",
        ] {
            let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
                id: 9,
                method: "GET".to_owned(),
                url: url.to_owned(),
                headers: Vec::new(),
                body: String::new(),
            });
            assert!(
                matches!(
                    result.denial(),
                    Some(DenialReason::NotInGrant | DenialReason::Malformed)
                ),
                "{url} was allowed: {result:?}"
            );
        }
        assert!(
            fixture.http.seen().is_empty(),
            "not one of those may have left: {:?}",
            fixture.http.seen()
        );
    }

    #[test]
    fn a_redirect_off_the_granted_host_is_refused_after_the_fact() {
        // Checking the URL the guest wrote bounds the first hop and nothing
        // else. A client that follows redirects — which most do by default —
        // would otherwise send the body to a host the manifest never named and
        // record one allowed call to the granted one.
        let mut fixture = fixture(&everything(), Some("alpha"));
        fixture
            .http
            .answer_from("https://api.example.com/r", "https://attacker.test/collect");
        let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "POST".to_owned(),
            url: "https://api.example.com/r".to_owned(),
            headers: Vec::new(),
            body: "tenant data".to_owned(),
        });
        assert_eq!(
            result.denial(),
            Some(DenialReason::NotInGrant),
            "{result:?}"
        );

        // …and the operator sees where it ended up, not just that something
        // was refused.
        let events = fixture.runtime.events();
        assert!(
            events
                .iter()
                .any(|event| event.outcome != audit::CapabilityOutcome::Allowed),
            "{events:?}"
        );
    }

    #[test]
    fn an_implementation_is_handed_what_it_needs_to_refuse_a_hop_itself() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        fixture.http.answer(
            "https://api.example.com/v1",
            OutboundResponse::from_url("https://api.example.com/v1", 200, "ok"),
        );
        let _ = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://api.example.com/v1".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        let seen = fixture.http.seen();
        let sent = seen.first().expect("the call left");
        assert!(
            !sent.follow_redirects,
            "redirects are never followed for a plugin"
        );
        assert_eq!(sent.allowed_hosts, vec!["api.example.com".to_owned()]);
        assert!(
            sent.timeout > std::time::Duration::ZERO,
            "a call has a deadline"
        );
    }

    #[test]
    fn an_upstream_that_answered_counts_as_a_host_that_was_called() {
        // A response discarded for being over a byte ceiling still *left the
        // host*. Filing it under "refused" would make the audit surface
        // undercount exactly the calls an operator most wants to see.
        let mut manifest = manifest(&everything());
        manifest.quotas.outbound_response_bytes = 4;
        let http = RecordingHttp::new();
        http.answer(
            "https://api.example.com/big",
            OutboundResponse::from_url("https://api.example.com/big", 200, "far too long"),
        );
        let mut runtime = CapabilityRuntime::new(
            &manifest,
            CapabilityServices {
                http: Some(Arc::clone(&http) as Arc<dyn OutboundHttp>),
                ..CapabilityServices::none()
            },
        );
        let result = runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://api.example.com/big".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        assert_eq!(result.denial(), Some(DenialReason::ResponseTooLarge));

        let log = audit::PluginActivityLog::new();
        log.ingest("shop", runtime.take_events());
        let summary = log.summary("shop", std::time::Duration::from_secs(3600));
        assert_eq!(summary.hosts.get("api.example.com"), Some(&1));
        assert!(summary.refused_targets.is_empty(), "{summary:?}");
    }

    #[test]
    fn the_same_host_on_another_port_is_the_granted_host() {
        // The grant is about where the bytes go, and the name decides that.
        assert_eq!(
            outbound::host_of("https://api.example.com:8443/v1"),
            Some("api.example.com".to_owned())
        );
        assert_eq!(
            outbound::host_of("https://api.example.com:not-a-port/"),
            None
        );
    }

    #[test]
    fn a_header_a_plugin_may_not_set_is_denied_before_the_call_leaves() {
        // A fresh fixture per case: a refused call still spends its quota (see
        // `a_refused_call_still_spends_its_quota_so_a_guest_cannot_spin`), and
        // four refusals in one request would exhaust `outbound_calls` and turn
        // the fifth denial into a quota one.
        for (name, value) in [
            ("authorization", "Bearer stolen"),
            ("cookie", "session=stolen"),
            ("host", "attacker.test"),
            ("x-forwarded-for", "10.0.0.1"),
        ] {
            let mut one = fixture(&everything(), Some("alpha"));
            let result = one.runtime.dispatch(&CapabilityCall::HttpFetch {
                id: 1,
                method: "GET".to_owned(),
                url: "https://api.example.com/".to_owned(),
                headers: vec![(name.to_owned(), value.to_owned())],
                body: String::new(),
            });
            assert_eq!(result.denial(), Some(DenialReason::NotInGrant), "{name}");
            assert!(one.http.seen().is_empty(), "{name} left the host");
        }

        let mut fixture = fixture(&everything(), Some("alpha"));
        let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 2,
            method: "GET".to_owned(),
            url: "https://api.example.com/".to_owned(),
            headers: vec![("accept".to_owned(), "a\r\nx-evil: 1".to_owned())],
            body: String::new(),
        });
        assert_eq!(result.denial(), Some(DenialReason::Malformed));
        assert!(fixture.http.seen().is_empty());
    }

    #[test]
    fn a_row_no_reply_could_carry_is_refused_where_it_goes_in() {
        // `MAX_VALUE_TEXT_BYTES` bounds one column and 32 of them bounds a row
        // at 2 MiB — twice what one reply may be. Accepting such a row would
        // store something `db-get` could never hand back, so the ceiling that
        // makes a read answerable belongs on the write.
        let mut fixture = fixture(&everything(), Some("alpha"));
        let fat: PluginRow = (0..8)
            .map(|index| {
                (
                    format!("c{index}"),
                    PluginValue::Text("x".repeat(MAX_VALUE_TEXT_BYTES)),
                )
            })
            .collect();
        assert!(
            row_weight(&fat) > MAX_ROW_BYTES,
            "the fixture is fat enough"
        );
        let result = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 1,
            table: "orders".to_owned(),
            row: fat,
        });
        assert_eq!(result.denial(), Some(DenialReason::Malformed), "{result:?}");
        assert!(fixture.db.keys().is_empty(), "{:?}", fixture.db.keys());
    }

    #[test]
    fn a_query_is_bounded_by_bytes_and_says_when_it_was_cut() {
        // The finding: `db_rows` defaults to 500 and a row may be
        // `MAX_ROW_BYTES`, so a granted plugin could make the host materialise,
        // encode and hold a hundred megabytes per request against a quota its
        // own manifest sets. The budget now reaches the store, and the guest is
        // told its answer was cut rather than reading a short page as the end
        // of the table.
        let mut fixture = fixture(&everything(), Some("alpha"));
        let wide = "x".repeat(MAX_VALUE_TEXT_BYTES);
        for index in 0..16 {
            let mut row = row(&[("kind", "wide")]);
            // Three, not four: four maxed-out columns plus the names is over
            // `MAX_ROW_BYTES`, and a row that cannot be stored cannot
            // demonstrate an answer that is too big to return.
            for column in 0..3 {
                row.insert(format!("c{column}"), PluginValue::Text(wide.clone()));
            }
            row.insert("n".to_owned(), PluginValue::Int(index));
            let inserted = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
                id: 1,
                table: "orders".to_owned(),
                row,
            });
            assert!(inserted.denial().is_none(), "row {index}: {inserted:?}");
        }

        let result = fixture.runtime.dispatch(&CapabilityCall::DbQuery {
            id: 2,
            table: "orders".to_owned(),
            filter: row(&[("kind", "wide")]),
            limit: 0,
            after: None,
        });
        let CallResult::Ok {
            value: CallValue::Rows { rows, truncated },
            ..
        } = result
        else {
            panic!("the query should have succeeded: {result:?}")
        };
        assert!(truncated, "16 wide rows do not fit one reply");
        assert!(!rows.is_empty(), "and the answer is not empty either");
        let carried: usize = rows.iter().map(row_weight).sum();
        assert!(
            carried <= MAX_RESULT_BYTES.saturating_add(MAX_ROW_BYTES),
            "the answer carried {carried} bytes"
        );
    }

    #[test]
    fn an_upstream_cannot_answer_with_unbounded_headers() {
        // The allow-list says which response headers come back and nothing
        // about their size, and every one of them is the *upstream's* choice
        // rather than the plugin's or the host's. A legal body plus megabytes
        // of `etag` is an oversized allocation the host makes and then fails
        // the plugin's request over.
        let mut fixture = fixture(&everything(), Some("alpha"));
        let mut answer = OutboundResponse::from_url("https://api.example.com/", 200, "ok");
        answer.headers = (0..64)
            .map(|_| ("etag".to_owned(), "x".repeat(4096)))
            .collect();
        fixture.http.answer("https://api.example.com/", answer);

        let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://api.example.com/".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        let CallResult::Ok {
            value: CallValue::Http { headers, .. },
            ..
        } = result
        else {
            panic!("the fetch should have succeeded: {result:?}")
        };
        assert!(headers.len() <= MAX_OUTBOUND_HEADERS, "{}", headers.len());
        let carried: usize = headers
            .iter()
            .map(|(name, value)| name.len().saturating_add(value.len()))
            .sum();
        assert!(
            carried <= MAX_RESPONSE_HEADER_BYTES,
            "the reply carried {carried} bytes of headers"
        );
    }

    // ── DB containment ───────────────────────────────────────────────

    #[test]
    fn a_plugin_reads_and_writes_only_its_own_tenant_scoped_table() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let inserted = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 1,
            table: "orders".to_owned(),
            row: row(&[("sku", "A-1")]),
        });
        let CallResult::Ok {
            value: CallValue::RowId { row_id },
            ..
        } = inserted
        else {
            panic!("the insert should have succeeded: {inserted:?}")
        };

        // The physical row landed under the derived name, never under a name
        // the guest chose.
        // Derived, never spelled: a literal here is a test that silently stops
        // testing anything the moment the derivation changes.
        let orders = db::physical_table("shop", "orders").expect("derivable");
        assert_eq!(
            fixture.db.keys(),
            vec![(orders, tenant_segment(Some("alpha")), row_id.clone())]
        );

        let read = fixture.runtime.dispatch(&CapabilityCall::DbGet {
            id: 2,
            table: "orders".to_owned(),
            row_id,
        });
        let CallResult::Ok {
            value: CallValue::Rows { rows, .. },
            ..
        } = read
        else {
            panic!("the read should have succeeded: {read:?}")
        };
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn an_undeclared_table_is_denied_however_it_is_spelled() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        for table in [
            "users",
            "orders_secret",
            "public.users",
            "orders; drop table users",
            "\"users\"",
            "Orders",
            "plugin_shop_orders",
            "",
        ] {
            let result = fixture.runtime.dispatch(&CapabilityCall::DbQuery {
                id: 1,
                table: table.to_owned(),
                filter: PluginRow::new(),
                limit: 10,
                after: None,
            });
            assert_eq!(
                result.denial(),
                Some(DenialReason::NotInGrant),
                "{table} was allowed"
            );
        }
    }

    #[test]
    fn another_tenants_rows_are_invisible_and_unwritable() {
        let store = db::MemoryPluginStore::new();
        // Seeded directly: a host-application table, and another tenant's row
        // in the plugin's own table.
        store.seed(
            "users",
            &tenant_segment(Some("alpha")),
            "u1",
            row(&[("email", "ceo@example.com")]),
        );
        // Derived, not hard-coded: a literal here turns the whole test vacuous
        // the moment the derivation changes, because beta's row would land in a
        // table alpha was never going to reach anyway.
        let orders = db::physical_table("shop", "orders").expect("derivable");
        store.seed(
            &orders,
            &tenant_segment(Some("beta")),
            "r99",
            row(&[("sku", "beta's")]),
        );

        let manifest = manifest(&everything());
        let mut alpha = CapabilityRuntime::new(
            &manifest,
            CapabilityServices {
                db: Some(Arc::clone(&store) as Arc<dyn PluginStore>),
                ..CapabilityServices::none()
            }
            .for_tenant("alpha"),
        );

        let listed = alpha.dispatch(&CapabilityCall::DbQuery {
            id: 1,
            table: "orders".to_owned(),
            filter: PluginRow::new(),
            limit: 100,
            after: None,
        });
        assert_eq!(
            listed,
            CallResult::Ok {
                id: 1,
                value: CallValue::Rows {
                    rows: Vec::new(),
                    truncated: false
                }
            },
            "an unfiltered query returns this tenant's rows, which are none"
        );

        for call in [
            CapabilityCall::DbGet {
                id: 2,
                table: "orders".to_owned(),
                row_id: "r99".to_owned(),
            },
            CapabilityCall::DbUpdate {
                id: 3,
                table: "orders".to_owned(),
                row_id: "r99".to_owned(),
                row: row(&[("sku", "stolen")]),
            },
            CapabilityCall::DbDelete {
                id: 4,
                table: "orders".to_owned(),
                row_id: "r99".to_owned(),
            },
        ] {
            let result = alpha.dispatch(&call);
            // Each call gets the *specific* answer it should, rather than
            // "anything except a populated row": a loop that accepts a denial
            // or an empty read for every operation passes even if `db-delete`
            // started returning `Done` after deleting beta's row.
            let expected_empty = matches!(call, CapabilityCall::DbGet { .. });
            if expected_empty {
                assert_eq!(
                    result,
                    CallResult::Ok {
                        id: result.id(),
                        value: CallValue::Rows {
                            rows: Vec::new(),
                            truncated: false
                        }
                    },
                    "a read of another tenant's id must miss, not error"
                );
            } else {
                assert_eq!(
                    result.denial(),
                    Some(DenialReason::BackendError),
                    "{call:?} must not touch another tenant's row"
                );
            }
        }
        // Untouched, in every sense.
        assert_eq!(
            store.keys(),
            vec![
                (orders, tenant_segment(Some("beta")), "r99".to_owned()),
                (
                    "users".to_owned(),
                    tenant_segment(Some("alpha")),
                    "u1".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_row_may_not_carry_the_column_that_decides_its_tenant() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let result = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 1,
            table: "orders".to_owned(),
            row: row(&[("tenant_id", "beta")]),
        });
        assert_eq!(result.denial(), Some(DenialReason::NotInGrant));
    }

    #[test]
    fn a_row_id_echoed_back_is_stripped_rather_than_refused() {
        // `db-get` stamps `row_id` onto the row it hands back, so the obvious
        // read-modify-write echoes it. Refusing that would make the obvious
        // code the wrong code; the id is the row's address and travels in its
        // own field, so a value for it in the row conveys nothing and can
        // override nothing. `tenant_id` stays a hard refusal above, because a
        // row that could set *that* would choose its own tenant.
        let mut fixture = fixture(&everything(), Some("alpha"));
        let inserted = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 1,
            table: "orders".to_owned(),
            row: row(&[("sku", "A-1"), ("row_id", "r999")]),
        });
        let CallResult::Ok {
            value: CallValue::RowId { row_id },
            ..
        } = inserted
        else {
            panic!("the insert should have succeeded: {inserted:?}")
        };
        assert_ne!(row_id, "r999", "the store assigns the id, never the guest");

        let read = fixture.runtime.dispatch(&CapabilityCall::DbGet {
            id: 2,
            table: "orders".to_owned(),
            row_id: row_id.clone(),
        });
        let CallResult::Ok {
            value: CallValue::Rows { rows, .. },
            ..
        } = read
        else {
            panic!("the read should have succeeded: {read:?}")
        };
        let stored = rows.first().cloned().unwrap_or_default();
        assert_eq!(
            stored.get("row_id"),
            Some(&PluginValue::Text(row_id.clone())),
            "the row comes back addressed by the id the store assigned"
        );

        // …and that same row writes straight back.
        let updated = fixture.runtime.dispatch(&CapabilityCall::DbUpdate {
            id: 3,
            table: "orders".to_owned(),
            row_id,
            row: stored,
        });
        assert!(updated.denial().is_none(), "{updated:?}");
    }

    #[test]
    fn two_plugins_can_never_be_named_onto_one_physical_table() {
        // The collision that matters is not punctuation, it is the *separator*:
        // a hostile author picks a plugin name that shifts the boundary onto a
        // victim's table. Both manifests validate, both consent screens are
        // truthful, and `AppBuilder` sees two distinct plugin names.
        let victim = db::physical_table("shop", "orders_v2");
        let attacker = db::physical_table("shop_orders", "v2");
        assert!(victim.is_some() && attacker.is_some());
        assert_ne!(victim, attacker, "a boundary shift must not collide");

        // Punctuation and case are the same bug one variation along.
        let names = ["my-shop", "my.shop", "my_shop", "My_Shop", "MY.SHOP"];
        let derived: Vec<String> = names
            .iter()
            .filter_map(|name| db::physical_table(name, "orders"))
            .collect();
        assert_eq!(derived.len(), names.len(), "{derived:?}");
        let mut unique = derived.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "collision in {derived:?}");

        // And the derived name is still a bare, unquoted SQL identifier.
        for name in &derived {
            assert!(
                name.starts_with("plugin_")
                    && name.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                "{name}"
            );
        }
    }

    #[test]
    fn a_truncated_ledger_says_so_rather_than_reporting_a_smaller_number() {
        // `MAX_EVENTS` is reachable: the quotas that would bound it live in the
        // plugin's own manifest, so the plugin sets them. An operator reading
        // "12 db-inserts" off a truncated ledger would be reading a number the
        // plugin chose.
        let mut manifest = manifest(&everything());
        manifest.quotas.calls = MAX_QUOTA;
        manifest.quotas.kv_reads = MAX_QUOTA;
        let mut runtime = CapabilityRuntime::new(
            &manifest,
            CapabilityServices {
                kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
                ..CapabilityServices::none()
            },
        );
        for id in 0..(MAX_EVENTS as u64 + 25) {
            let _ = runtime.dispatch(&CapabilityCall::KvGet {
                id,
                key: "k".to_owned(),
            });
        }
        assert_eq!(runtime.events().len(), MAX_EVENTS);
        assert_eq!(runtime.dropped_events(), 25);

        let log = audit::PluginActivityLog::new();
        let dropped = runtime.dropped_events();
        log.ingest("shop", runtime.take_events());
        log.ingest_dropped("shop", dropped);
        let summary = log.summary("shop", std::time::Duration::from_secs(3600));
        assert_eq!(summary.dropped, 25);
        let rendered = summary.to_string();
        assert!(rendered.contains("floor"), "{rendered}");
    }

    #[test]
    fn an_audit_target_is_bounded_before_it_is_escaped() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let _ = fixture.runtime.dispatch(&CapabilityCall::KvGet {
            id: 1,
            key: "\u{202e}".repeat(400),
        });
        let recorded = fixture
            .runtime
            .events()
            .first()
            .map(|event| event.target.clone())
            .unwrap_or_default();
        // `rejected` expands a hostile character to as many as ten bytes, so
        // the bound has to be on characters and has to be applied first.
        assert!(
            recorded.len() <= MAX_TARGET_CHARS * 10 + 8,
            "{} bytes: {recorded}",
            recorded.len()
        );
    }

    #[test]
    fn the_in_memory_stores_refuse_rather_than_growing_without_a_ceiling() {
        // A plugin declares its own `kv_writes` and `db_writes`, and both may
        // legally be `MAX_QUOTA`. A shipped store with no ceiling of its own
        // turns that into the host's memory.
        let kv = MemoryKvStore::with_capacity(2);
        assert!(kv.set("a", PluginValue::Int(1)).is_ok());
        assert!(kv.set("b", PluginValue::Int(1)).is_ok());
        assert!(kv.set("c", PluginValue::Int(1)).is_err());
        // Overwriting an existing key adds nothing, so it is never refused.
        assert!(kv.set("a", PluginValue::Int(2)).is_ok());

        let store = db::MemoryPluginStore::with_capacity(1);
        let scope = db::Scope {
            table: "plugin_shop__orders".to_owned(),
            tenant: "alpha".to_owned(),
        };
        assert!(store.insert(&scope, PluginRow::new()).is_ok());
        assert!(store.insert(&scope, PluginRow::new()).is_err());
    }

    #[test]
    fn a_full_store_is_a_denial_the_guest_can_read() {
        let mut runtime = CapabilityRuntime::new(
            &manifest(&everything()),
            CapabilityServices {
                kv: Some(MemoryKvStore::with_capacity(0) as Arc<dyn KvStore>),
                ..CapabilityServices::none()
            }
            .for_tenant("alpha"),
        );
        assert_eq!(
            runtime
                .dispatch(&CapabilityCall::KvSet {
                    id: 1,
                    key: "cart".to_owned(),
                    value: PluginValue::Int(1)
                })
                .denial(),
            Some(DenialReason::BackendError)
        );
    }

    #[test]
    fn no_physical_table_is_derivable_from_a_name_the_grant_list_could_not_hold() {
        assert_eq!(
            db::physical_table("shop", "orders"),
            Some("plugin_shop__orders".to_owned())
        );
        // A plugin name's charset is wider than SQL's, so it is escaped
        // injectively rather than folded: `.` is `_2e` and `-` is `_2d`, and
        // `__` — which no escape can produce — is the separator.
        assert_eq!(
            db::physical_table("my.shop-v2", "orders"),
            Some("plugin_my_2eshop_2dv2__orders".to_owned())
        );
        for (plugin, table) in [
            ("shop", "users; drop table x"),
            ("shop", "\"users\""),
            ("shop", "Orders"),
            ("shop\"; --", "orders"),
            ("shop", ""),
            ("", "orders"),
            ("shop", &"o".repeat(64)),
            // Long, but each half is legal: the *derived* name is what cannot
            // fit, and `SandboxManifest::validate` refuses this at load so an
            // operator never approves a capability that can only be denied.
            (&"p".repeat(40), &"t".repeat(30)),
        ] {
            assert_eq!(db::physical_table(plugin, table), None, "{plugin}/{table}");
        }
    }

    // ── Jobs ─────────────────────────────────────────────────────────

    #[test]
    fn a_job_carries_the_enqueuing_plugin_and_tenant_not_the_guests_word_for_them() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let result = fixture.runtime.dispatch(&CapabilityCall::JobEnqueue {
            id: 1,
            job_type: "reindex".to_owned(),
            payload: row(&[("since", "2026-01-01")]),
        });
        assert!(result.denial().is_none(), "{result:?}");
        assert_eq!(
            fixture.jobs.queued(),
            vec![jobs::PluginJob {
                plugin: "shop".to_owned(),
                job_type: "reindex".to_owned(),
                tenant: Some("alpha".to_owned()),
                payload: row(&[("since", "2026-01-01")]),
            }]
        );
    }

    #[test]
    fn an_undeclared_job_type_never_reaches_the_queue() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        for job_type in ["send-invoice", "reindex-all", "REINDEX", ""] {
            let result = fixture.runtime.dispatch(&CapabilityCall::JobEnqueue {
                id: 1,
                job_type: job_type.to_owned(),
                payload: PluginRow::new(),
            });
            assert_eq!(
                result.denial(),
                Some(DenialReason::NotInGrant),
                "{job_type}"
            );
        }
        assert!(fixture.jobs.queued().is_empty());
    }

    // ── Quotas ───────────────────────────────────────────────────────

    #[test]
    fn a_spent_quota_denies_the_call_and_nothing_else() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let ceiling = fixture.runtime.quotas().kv_reads;
        for id in 0..ceiling {
            let result = fixture.runtime.dispatch(&CapabilityCall::KvGet {
                id: u64::from(id),
                key: "cart".to_owned(),
            });
            assert!(result.denial().is_none(), "call {id}: {result:?}");
        }
        let over = fixture.runtime.dispatch(&CapabilityCall::KvGet {
            id: 999,
            key: "cart".to_owned(),
        });
        assert_eq!(over.denial(), Some(DenialReason::QuotaExceeded));

        // A different capability is untouched: quotas are per surface, so
        // running out of reads does not cost the plugin its ability to enqueue.
        let job = fixture.runtime.dispatch(&CapabilityCall::JobEnqueue {
            id: 1000,
            job_type: "reindex".to_owned(),
            payload: PluginRow::new(),
        });
        assert!(job.denial().is_none(), "{job:?}");
    }

    #[test]
    fn one_plugins_spent_quota_does_not_touch_another_plugins() {
        let manifest = manifest(&everything());
        let store = MemoryKvStore::new();
        let services = CapabilityServices {
            kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
            ..CapabilityServices::none()
        }
        .for_tenant("alpha");
        let mut first = CapabilityRuntime::new(&manifest, services.clone());
        for id in 0..manifest.quotas.kv_reads {
            let _ = first.dispatch(&CapabilityCall::KvGet {
                id: u64::from(id),
                key: "cart".to_owned(),
            });
        }
        assert_eq!(
            first
                .dispatch(&CapabilityCall::KvGet {
                    id: 999,
                    key: "cart".to_owned()
                })
                .denial(),
            Some(DenialReason::QuotaExceeded)
        );

        let mut second = CapabilityRuntime::new(&manifest, services);
        assert!(
            second
                .dispatch(&CapabilityCall::KvGet {
                    id: 1,
                    key: "cart".to_owned()
                })
                .denial()
                .is_none()
        );
    }

    #[test]
    fn the_shared_call_budget_bounds_the_sum_of_every_surface() {
        let mut manifest = manifest(&everything());
        manifest.quotas.calls = 3;
        let mut runtime = CapabilityRuntime::new(
            &manifest,
            CapabilityServices {
                kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
                jobs: Some(MemoryJobSink::new() as Arc<dyn JobSink>),
                ..CapabilityServices::none()
            },
        );
        for id in 0..3 {
            assert!(
                runtime
                    .dispatch(&CapabilityCall::KvGet {
                        id,
                        key: "k".to_owned()
                    })
                    .denial()
                    .is_none()
            );
        }
        // The fourth call is refused even though `job_enqueues` is untouched.
        assert_eq!(
            runtime
                .dispatch(&CapabilityCall::JobEnqueue {
                    id: 4,
                    job_type: "reindex".to_owned(),
                    payload: PluginRow::new(),
                })
                .denial(),
            Some(DenialReason::QuotaExceeded)
        );
    }

    #[test]
    fn a_calls_per_second_ceiling_is_shared_across_requests_and_kept_per_capability() {
        let manifest = manifest(&everything());
        // One token per second, not two: the assertion below needs the bucket
        // to still be empty when it runs, and a `per_second` of 2 gave it only
        // 500 ms of slack — enough for a loaded debug-build runner to refill
        // one token between the take and the check and flip this green to red.
        let rate = Arc::new(quota::CapabilityRateLimiter::new(1));
        let services = CapabilityServices {
            kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
            jobs: Some(MemoryJobSink::new() as Arc<dyn JobSink>),
            rate: Some(Arc::clone(&rate)),
            ..CapabilityServices::none()
        };
        let mut first = CapabilityRuntime::new(&manifest, services.clone());
        assert!(
            first
                .dispatch(&CapabilityCall::KvGet {
                    id: 1,
                    key: "k".to_owned()
                })
                .denial()
                .is_none()
        );
        // A *second request* of the same plugin shares the bucket.
        let mut second = CapabilityRuntime::new(&manifest, services);
        assert_eq!(
            second
                .dispatch(&CapabilityCall::KvGet {
                    id: 3,
                    key: "k".to_owned()
                })
                .denial(),
            Some(DenialReason::QuotaExceeded)
        );
        // A different capability has its own bucket, so a busy cache does not
        // cost the plugin its ability to enqueue.
        assert!(
            second
                .dispatch(&CapabilityCall::JobEnqueue {
                    id: 4,
                    job_type: "reindex".to_owned(),
                    payload: PluginRow::new()
                })
                .denial()
                .is_none()
        );
    }

    #[test]
    fn an_oversized_row_or_key_is_refused_before_it_is_stored() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let long = "k".repeat(MAX_KV_KEY_BYTES + 1);
        assert_eq!(
            fixture
                .runtime
                .dispatch(&CapabilityCall::KvGet { id: 1, key: long })
                .denial(),
            Some(DenialReason::Malformed)
        );
        let wide: PluginRow = (0..=MAX_ROW_COLUMNS)
            .map(|index| (format!("c{index}"), PluginValue::Int(1)))
            .collect();
        assert_eq!(
            fixture
                .runtime
                .dispatch(&CapabilityCall::DbInsert {
                    id: 2,
                    table: "orders".to_owned(),
                    row: wide
                })
                .denial(),
            Some(DenialReason::Malformed)
        );
        assert!(fixture.db.keys().is_empty());
    }

    #[test]
    fn a_missing_backend_is_a_denial_rather_than_a_silent_success() {
        let mut runtime =
            CapabilityRuntime::new(&manifest(&everything()), CapabilityServices::none());
        assert_eq!(
            runtime
                .dispatch(&CapabilityCall::KvSet {
                    id: 1,
                    key: "cart".to_owned(),
                    value: PluginValue::Int(1)
                })
                .denial(),
            Some(DenialReason::Unavailable)
        );
    }

    #[test]
    fn a_response_header_a_plugin_may_not_see_never_reaches_it() {
        // The response side of the outbound allow-list. Nothing exercised it,
        // so deleting the filter left the suite green — and `set-cookie` from
        // an upstream is a cookie the *plugin* then learns.
        let mut fixture = fixture(&everything(), Some("alpha"));
        fixture.http.answer(
            "https://api.example.com/v1",
            OutboundResponse {
                status: 200,
                headers: vec![
                    ("content-type".to_owned(), "application/json".to_owned()),
                    ("set-cookie".to_owned(), "session=stolen".to_owned()),
                    ("x-upstream-secret".to_owned(), "s3cret".to_owned()),
                ],
                body: "{}".to_owned(),
                final_url: "https://api.example.com/v1".to_owned(),
            },
        );
        let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://api.example.com/v1".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        let CallResult::Ok {
            value: CallValue::Http { headers, .. },
            ..
        } = result
        else {
            panic!("the call should have succeeded: {result:?}")
        };
        assert_eq!(
            headers,
            vec![("content-type".to_owned(), "application/json".to_owned())]
        );
    }

    #[test]
    fn a_method_or_header_count_outside_the_allow_list_is_refused() {
        // A fresh fixture per case: the method check happens inside `perform`,
        // after the per-capability charge, so five cases in one request would
        // exhaust the default `outbound_calls` of four and turn the fifth
        // refusal into a quota one.
        for method in ["CONNECT", "TRACE", "OPTIONS", " GET", ""] {
            let mut fixture = fixture(&everything(), Some("alpha"));
            let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
                id: 1,
                method: method.to_owned(),
                url: "https://api.example.com/".to_owned(),
                headers: Vec::new(),
                body: String::new(),
            });
            assert_eq!(result.denial(), Some(DenialReason::Malformed), "{method}");
            assert!(fixture.http.seen().is_empty(), "{method} left the host");
        }
        let mut fixture = fixture(&everything(), Some("alpha"));
        let many: Vec<(String, String)> = (0..=MAX_OUTBOUND_HEADERS)
            .map(|_| ("accept".to_owned(), "*/*".to_owned()))
            .collect();
        assert_eq!(
            fixture
                .runtime
                .dispatch(&CapabilityCall::HttpFetch {
                    id: 2,
                    method: "GET".to_owned(),
                    url: "https://api.example.com/".to_owned(),
                    headers: many,
                    body: String::new(),
                })
                .denial(),
            Some(DenialReason::Malformed)
        );
        assert!(fixture.http.seen().is_empty());
    }

    #[test]
    fn every_byte_ceiling_a_quota_names_is_actually_applied() {
        // Four ceilings that had no test between them, so four `if` statements
        // nothing reached.
        let mut manifest = manifest(&everything());
        manifest.quotas.kv_value_bytes = 8;
        manifest.quotas.db_rows = 2;
        manifest.quotas.outbound_response_bytes = 8;
        let http = RecordingHttp::new();
        http.answer(
            "https://api.example.com/big",
            OutboundResponse::from_url("https://api.example.com/big", 200, "x".repeat(64)),
        );
        let db = db::MemoryPluginStore::new();
        let mut runtime = CapabilityRuntime::new(
            &manifest,
            CapabilityServices {
                kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
                db: Some(Arc::clone(&db) as Arc<dyn PluginStore>),
                http: Some(Arc::clone(&http) as Arc<dyn OutboundHttp>),
                ..CapabilityServices::none()
            }
            .for_tenant("alpha"),
        );

        // `kv_value_bytes`.
        assert_eq!(
            runtime
                .dispatch(&CapabilityCall::KvSet {
                    id: 1,
                    key: "cart".to_owned(),
                    value: PluginValue::Text("far too long a value".to_owned())
                })
                .denial(),
            Some(DenialReason::QuotaExceeded)
        );

        // `db_rows`, both as the clamp on an explicit limit and as the default
        // for `limit: 0`.
        for index in 0..5 {
            let inserted = runtime.dispatch(&CapabilityCall::DbInsert {
                id: 10 + index,
                table: "orders".to_owned(),
                row: row(&[("sku", "A")]),
            });
            assert!(inserted.denial().is_none(), "{inserted:?}");
        }
        for (id, limit) in [(20, 0), (21, 100)] {
            let result = runtime.dispatch(&CapabilityCall::DbQuery {
                id,
                table: "orders".to_owned(),
                filter: PluginRow::new(),
                limit,
                after: None,
            });
            let CallResult::Ok {
                value: CallValue::Rows { rows, .. },
                ..
            } = result
            else {
                panic!("the query should have succeeded: {result:?}")
            };
            assert_eq!(rows.len(), 2, "limit {limit} is clamped to `db_rows`");
        }

        // `outbound_response_bytes`.
        assert_eq!(
            runtime
                .dispatch(&CapabilityCall::HttpFetch {
                    id: 30,
                    method: "GET".to_owned(),
                    url: "https://api.example.com/big".to_owned(),
                    headers: Vec::new(),
                    body: String::new(),
                })
                .denial(),
            Some(DenialReason::ResponseTooLarge)
        );
    }

    #[test]
    fn a_row_id_outside_its_bounds_is_refused_before_the_store_sees_it() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        for row_id in [String::new(), "r".repeat(MAX_ROW_ID_BYTES + 1)] {
            assert_eq!(
                fixture
                    .runtime
                    .dispatch(&CapabilityCall::DbGet {
                        id: 1,
                        table: "orders".to_owned(),
                        row_id: row_id.clone(),
                    })
                    .denial(),
                Some(DenialReason::Malformed),
                "{} bytes",
                row_id.len()
            );
        }
    }

    #[test]
    fn a_full_job_queue_is_a_denial_the_guest_can_read() {
        // `MemoryJobSink::bounded` existed with no caller — test-support API
        // written for a test that was never written, which is the same thing as
        // an untested `BackendError` arm.
        let sink = jobs::MemoryJobSink::bounded(1);
        let mut runtime = CapabilityRuntime::new(
            &manifest(&everything()),
            CapabilityServices {
                jobs: Some(Arc::clone(&sink) as Arc<dyn JobSink>),
                ..CapabilityServices::none()
            }
            .for_tenant("alpha"),
        );
        let enqueue = |id| CapabilityCall::JobEnqueue {
            id,
            job_type: "reindex".to_owned(),
            payload: PluginRow::new(),
        };
        assert!(runtime.dispatch(&enqueue(1)).denial().is_none());
        assert_eq!(
            runtime.dispatch(&enqueue(2)).denial(),
            Some(DenialReason::BackendError)
        );
        assert_eq!(sink.queued().len(), 1);
    }

    #[test]
    fn the_zero_configuration_job_queue_is_finite() {
        // `MemoryJobSink::new()` used to be unbounded, and this slice ships no
        // consumer that removes a queued job — so an application that wired the
        // exported default gave a granted plugin a way to grow the host without
        // limit. The quotas slow that and never stop it. There is no unbounded
        // spelling any more; a default that must be tightened to be safe is a
        // default that will be shipped as it is.
        //
        // Driven against the sink rather than through `dispatch`: the per-request
        // quotas would refuse long before the queue filled, and it is the queue
        // that outlives the request.
        let sink = jobs::MemoryJobSink::new();
        let job = || PluginJob {
            plugin: "shop".to_owned(),
            job_type: "reindex".to_owned(),
            tenant: Some("alpha".to_owned()),
            payload: PluginRow::new(),
        };
        for index in 0..jobs::DEFAULT_JOB_DEPTH {
            assert!(sink.enqueue(job()).is_ok(), "job {index}");
        }
        let over = sink.enqueue(job());
        assert!(over.is_err(), "{over:?}");
        assert_eq!(sink.queued().len(), jobs::DEFAULT_JOB_DEPTH);
    }

    // ── Audit ────────────────────────────────────────────────────────

    #[test]
    fn one_surface_answers_what_this_plugin_did() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        fixture.http.answer(
            "https://api.example.com/v1",
            OutboundResponse {
                status: 200,
                headers: Vec::new(),
                body: "ok".to_owned(),
                final_url: "https://api.example.com/v1".to_owned(),
            },
        );
        let _ = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://api.example.com/v1".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        let _ = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 2,
            method: "GET".to_owned(),
            url: "https://attacker.test/".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        let _ = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 3,
            table: "orders".to_owned(),
            row: row(&[("sku", "A-1")]),
        });
        let _ = fixture.runtime.dispatch(&CapabilityCall::JobEnqueue {
            id: 4,
            job_type: "reindex".to_owned(),
            payload: PluginRow::new(),
        });

        let log = audit::PluginActivityLog::new();
        log.ingest("shop", fixture.runtime.take_events());
        let summary = log.summary("shop", std::time::Duration::from_secs(3600));

        assert_eq!(summary.hosts.get("api.example.com"), Some(&1));
        assert_eq!(summary.tables.get("plugin_shop_orders"), None);
        assert_eq!(summary.tables.get("orders"), Some(&1));
        assert_eq!(summary.job_types.get("reindex"), Some(&1));
        assert_eq!(summary.denied.get("http-fetch"), Some(&1));
        let rendered = summary.to_string();
        assert!(rendered.contains("api.example.com"), "{rendered}");
        assert!(rendered.contains("reindex"), "{rendered}");
    }

    #[test]
    fn the_audit_record_never_carries_a_value() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        let _ = fixture.runtime.dispatch(&CapabilityCall::KvSet {
            id: 1,
            key: "cart".to_owned(),
            value: PluginValue::Text("the customer's card number".to_owned()),
        });
        let _ = fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 2,
            table: "orders".to_owned(),
            row: row(&[("note", "the customer's home address")]),
        });
        let serialized = serde_json::to_string(fixture.runtime.events()).expect("events serialize");
        assert!(!serialized.contains("card number"), "{serialized}");
        assert!(!serialized.contains("home address"), "{serialized}");
        assert!(
            serialized.contains("cart"),
            "the key is a shape, not a value"
        );
    }
}

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
use audit::{CapabilityEvent, CapabilityOutcome};
use quota::QuotaLedger;

pub use audit::{ActivitySummary, CapabilityEvent as AuditEvent, PluginActivityLog};
pub use db::{MemoryPluginStore, PluginStore, Scope, StoreError};
pub use quota::CapabilityRateLimiter;
pub use render::{FragmentNode, RenderError};
pub use jobs::{JobSink, PluginJob};
pub use jobs::MemoryJobSink;
pub use kv::{CacheKvStore, KvStore, MemoryKvStore};
pub use outbound::{OutboundHttp, OutboundRequest, OutboundResponse, RecordingHttp};

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
    pub fn weight(&self) -> usize {
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

/// Why a row was refused before it reached anything.
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
            Self::HttpFetch { url, .. } => outbound::host_of(url)
                .unwrap_or_else(|| "(not an absolute http/https URL)".to_owned()),
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
        }
    }
}

impl fmt::Display for DenialReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the host hands back for one call.
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
    #[must_use]
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
pub const NO_TENANT: &str = "_";

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
    grants: super::grants::CapabilityGrants,
    /// The backends. `pub(super)` so each capability module reaches its own and
    /// no more; nothing outside this module tree can substitute one.
    pub(super) services: CapabilityServices,
    quotas: QuotaLedger,
    rate: Option<Arc<quota::CapabilityRateLimiter>>,
    events: Vec<CapabilityEvent>,
}

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
        }
    }

    /// The quotas this runtime enforces.
    #[must_use]
    pub const fn quotas(&self) -> &CapabilityQuotas {
        self.quotas.declared()
    }

    /// The tenant every scoped call binds to, as a namespace segment.
    #[must_use]
    pub fn tenant(&self) -> &str {
        self.services.tenant.as_deref().unwrap_or(NO_TENANT)
    }

    /// Everything this request did, in order.
    #[must_use]
    pub fn events(&self) -> &[CapabilityEvent] {
        &self.events
    }

    /// Take the ledger, leaving it empty.
    #[must_use]
    pub fn take_events(&mut self) -> Vec<CapabilityEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether this plugin may use `capability` at all.
    #[must_use]
    pub fn grants(&self, capability: SandboxCapability) -> bool {
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

        // Order matters. The capability check is first because a plugin that
        // was never granted `db` must learn that, not that it is over a quota
        // it could never legitimately spend — and because a call to an
        // ungranted capability must not be able to spend the shared `calls`
        // budget a granted one needs.
        if !self.grants(capability) {
            return self.record(
                call,
                named,
                CallResult::denied(
                    id,
                    DenialReason::CapabilityNotGranted,
                    format!("this plugin was not granted `{capability}`"),
                ),
            );
        }

        // Scope before quota, for the same reason: an out-of-scope target is a
        // manifest mistake the author has to fix, and letting it consume the
        // request's call budget would report the wrong problem on the next call.
        let target = match self.target_of(call) {
            Ok(target) => target,
            Err(result) => return self.record(call, named, result),
        };

        if let Err(field) = self.quotas.charge(call) {
            return self.record(
                call,
                target.clone(),
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
                target.clone(),
                CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!("this plugin is over its `{capability}` calls-per-second ceiling"),
                ),
            );
        }

        let result = self.perform(call, &target);
        self.record(call, target, result)
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
    fn perform(&mut self, call: &CapabilityCall, target: &str) -> CallResult {
        match call {
            CapabilityCall::KvGet { .. } | CapabilityCall::KvSet { .. } | CapabilityCall::KvDelete { .. } => {
                kv::perform(self, call, target)
            }
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
    fn record(&mut self, call: &CapabilityCall, target: String, result: CallResult) -> CallResult {
        // Bounded and neutralised once, here, rather than at each of the six
        // exits. A KV key is the one identifier a guest chooses freely, it can
        // carry up to `MAX_KV_KEY_BYTES` of anything, and from here it goes into
        // a `tracing` line and into the rendered operator summary — both of
        // which a terminal renders.
        let target = super::manifest::rejected(&target);
        let outcome = match result.denial() {
            None => CapabilityOutcome::Allowed,
            Some(DenialReason::QuotaExceeded) => CapabilityOutcome::QuotaExceeded,
            Some(reason) => CapabilityOutcome::Denied(reason),
        };
        if matches!(outcome, CapabilityOutcome::Allowed) {
            tracing::debug!(
                plugin = self.plugin,
                capability = call.capability().as_str(),
                operation = call.operation(),
                target = target.as_str(),
                "sandboxed plugin used a granted capability"
            );
        } else {
            // Denials are warned, once, at the point they happen — the same
            // treatment `HostState::deny` gives a forbidden import, and for the
            // same reason: whoever is driving the host should not have to
            // remember to log it.
            tracing::warn!(
                plugin = self.plugin,
                capability = call.capability().as_str(),
                operation = call.operation(),
                target = target.as_str(),
                outcome = %outcome,
                "sandboxed plugin was refused a capability call"
            );
        }
        if self.events.len() < MAX_EVENTS {
            self.events.push(CapabilityEvent {
                capability: call.capability(),
                operation: call.operation(),
                target,
                outcome,
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::grants::MAX_QUOTA;
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
        granted.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://attacker.test/steal".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        granted.runtime.dispatch(&CapabilityCall::DbQuery {
            id: 2,
            table: "users".to_owned(),
            filter: PluginRow::new(),
            limit: 1,
        });
        granted.runtime.dispatch(&CapabilityCall::JobEnqueue {
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
        hostile.runtime.dispatch(&CapabilityCall::KvGet {
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
        let services = |tenant: &str| CapabilityServices {
            kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
            ..CapabilityServices::none()
        }
        .for_tenant(tenant);

        let manifest = manifest(&everything());
        let mut alpha = CapabilityRuntime::new(&manifest, services("alpha"));
        alpha.dispatch(&CapabilityCall::KvSet {
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
        let services = |tenant: &str| CapabilityServices {
            kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
            ..CapabilityServices::none()
        }
        .for_tenant(tenant);

        let mut beta = CapabilityRuntime::new(&manifest, services("b"));
        beta.dispatch(&CapabilityCall::KvSet {
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
            kv::namespaced_key("shop", "b%3Ax", "k"),
            kv::namespaced_key("shop", "b", "%3Ax:k")
        );
        assert_ne!(
            kv::namespaced_key("shop", "a", "b:c"),
            kv::namespaced_key("shop", "a:b", "c")
        );
    }

    #[test]
    fn two_plugins_never_share_a_key() {
        assert_ne!(
            kv::namespaced_key("shop", "t", "cart"),
            kv::namespaced_key("other", "t", "cart")
        );
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
    fn the_same_host_on_another_port_is_the_granted_host() {
        // The grant is about where the bytes go, and the name decides that.
        assert_eq!(
            outbound::host_of("https://api.example.com:8443/v1"),
            Some("api.example.com".to_owned())
        );
        assert_eq!(outbound::host_of("https://api.example.com:not-a-port/"), None);
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
    fn a_quota_counts_what_reached_a_backend_and_fuel_counts_every_call() {
        // Two ceilings, two jobs. A call refused at the *grant* check never
        // reaches a backend, so it does not spend the quota that bounds backend
        // work — spending it there would let a manifest mistake starve the
        // calls the plugin is entitled to make. What bounds a guest spinning on
        // refusals is fuel: `CAPABILITY_CALL_FUEL` plus the reply's bytes is
        // charged for every call, granted or not, and it is charged by the host
        // rather than by the ledger. See `host::HostState::service`.
        let mut fixture = fixture(&everything(), Some("alpha"));
        let ceiling = fixture.runtime.quotas().outbound_calls;
        for id in 0..(u64::from(ceiling) * 4) {
            let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
                id,
                method: "GET".to_owned(),
                url: "https://attacker.test/".to_owned(),
                headers: Vec::new(),
                body: String::new(),
            });
            assert_eq!(result.denial(), Some(DenialReason::NotInGrant));
        }
        // The granted host is still reachable: no amount of asking for one that
        // was never granted costs the plugin the one that was.
        fixture.http.answer(
            "https://api.example.com/",
            OutboundResponse {
                status: 204,
                headers: Vec::new(),
                body: String::new(),
            },
        );
        let allowed = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 99,
            method: "GET".to_owned(),
            url: "https://api.example.com/".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        assert!(allowed.denial().is_none(), "{allowed:?}");

        // A call that got as far as a backend does spend the quota, whatever
        // the backend then said.
        for id in 100..(100 + u64::from(ceiling) - 1) {
            let result = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
                id,
                method: "GET".to_owned(),
                url: "https://api.example.com/missing".to_owned(),
                headers: Vec::new(),
                body: String::new(),
            });
            assert_eq!(result.denial(), Some(DenialReason::BackendError));
        }
        let over = fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 999,
            method: "GET".to_owned(),
            url: "https://api.example.com/".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        assert_eq!(over.denial(), Some(DenialReason::QuotaExceeded));
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
        assert_eq!(
            fixture.db.keys(),
            vec![(
                "plugin_shop_orders".to_owned(),
                "alpha".to_owned(),
                row_id.clone()
            )]
        );

        let read = fixture.runtime.dispatch(&CapabilityCall::DbGet {
            id: 2,
            table: "orders".to_owned(),
            row_id,
        });
        let CallResult::Ok {
            value: CallValue::Rows { rows },
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
        store.seed("users", "alpha", "u1", row(&[("email", "ceo@example.com")]));
        store.seed(
            "plugin_shop_orders",
            "beta",
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
        });
        assert_eq!(
            listed,
            CallResult::Ok {
                id: 1,
                value: CallValue::Rows { rows: Vec::new() }
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
            assert!(
                matches!(result, CallResult::Denied { .. })
                    || result
                        == CallResult::Ok {
                            id: result.id(),
                            value: CallValue::Rows { rows: Vec::new() }
                        },
                "{call:?} reached another tenant's row: {result:?}"
            );
        }
        // Untouched, in every sense.
        assert_eq!(
            store.keys(),
            vec![
                (
                    "plugin_shop_orders".to_owned(),
                    "beta".to_owned(),
                    "r99".to_owned()
                ),
                ("users".to_owned(), "alpha".to_owned(), "u1".to_owned()),
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
            value: CallValue::Rows { rows },
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
    fn no_physical_table_is_derivable_from_a_name_the_grant_list_could_not_hold() {
        assert_eq!(
            db::physical_table("shop", "orders"),
            Some("plugin_shop_orders".to_owned())
        );
        // A plugin name's own charset is wider than SQL's; punctuation is
        // folded rather than emitted.
        assert_eq!(
            db::physical_table("my.shop-v2", "orders"),
            Some("plugin_my_shop_v2_orders".to_owned())
        );
        for (plugin, table) in [
            ("shop", "users; drop table x"),
            ("shop", "\"users\""),
            ("shop", "Orders"),
            ("shop\"; --", "orders"),
            ("shop", ""),
            ("", "orders"),
            ("shop", &"o".repeat(64)),
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
                tenant: "alpha".to_owned(),
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
            assert_eq!(result.denial(), Some(DenialReason::NotInGrant), "{job_type}");
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
            first.dispatch(&CapabilityCall::KvGet {
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
        let rate = Arc::new(quota::CapabilityRateLimiter::new(2));
        let services = CapabilityServices {
            kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
            jobs: Some(MemoryJobSink::new() as Arc<dyn JobSink>),
            rate: Some(Arc::clone(&rate)),
            ..CapabilityServices::none()
        };
        let mut first = CapabilityRuntime::new(&manifest, services.clone());
        for id in 0..2 {
            assert!(
                first
                    .dispatch(&CapabilityCall::KvGet {
                        id,
                        key: "k".to_owned()
                    })
                    .denial()
                    .is_none()
            );
        }
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
            },
        );
        fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 1,
            method: "GET".to_owned(),
            url: "https://api.example.com/v1".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        fixture.runtime.dispatch(&CapabilityCall::HttpFetch {
            id: 2,
            method: "GET".to_owned(),
            url: "https://attacker.test/".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        });
        fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 3,
            table: "orders".to_owned(),
            row: row(&[("sku", "A-1")]),
        });
        fixture.runtime.dispatch(&CapabilityCall::JobEnqueue {
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
        let rendered = summary.render("shop", std::time::Duration::from_secs(3600));
        assert!(rendered.contains("api.example.com"), "{rendered}");
        assert!(rendered.contains("reindex"), "{rendered}");
    }

    #[test]
    fn the_audit_record_never_carries_a_value() {
        let mut fixture = fixture(&everything(), Some("alpha"));
        fixture.runtime.dispatch(&CapabilityCall::KvSet {
            id: 1,
            key: "cart".to_owned(),
            value: PluginValue::Text("the customer's card number".to_owned()),
        });
        fixture.runtime.dispatch(&CapabilityCall::DbInsert {
            id: 2,
            table: "orders".to_owned(),
            row: row(&[("note", "the customer's home address")]),
        });
        let serialized =
            serde_json::to_string(fixture.runtime.events()).expect("events serialize");
        assert!(!serialized.contains("card number"), "{serialized}");
        assert!(!serialized.contains("home address"), "{serialized}");
        assert!(serialized.contains("cart"), "the key is a shape, not a value");
    }

    #[test]
    fn the_ledger_is_bounded_by_a_guests_call_rate() {
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
        for id in 0..(MAX_EVENTS as u64 * 2) {
            runtime.dispatch(&CapabilityCall::KvGet {
                id,
                key: "k".to_owned(),
            });
        }
        assert_eq!(runtime.events().len(), MAX_EVENTS);
    }
}

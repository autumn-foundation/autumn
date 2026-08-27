//! On-disk shape of a failure capsule.
//!
//! A capsule is a single JSON document describing one failed request: the
//! (redacted) request that produced it, the clock readings the handler took,
//! the database traffic it generated, and the outcome the client received.
//! Everything here is `serde`-round-trippable and versioned by
//! [`CAPSULE_FORMAT_VERSION`] so a capsule recorded by one build is either
//! replayable by another or rejected outright — never silently misread.
//!
//! Byte-valued fields (wire frames, bind parameters) are base64-encoded so a
//! capsule stays a plain, diffable JSON file.

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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Version of the capsule document format understood by this build.
///
/// Bumped whenever the schema changes in a way a previous reader cannot
/// tolerate. Replay refuses any capsule whose `format_version` differs.
///
/// A *semantic* field counts as such a change even though `serde` would
/// happily ignore it. [`Capsule::db_roles`] is the case that made this
/// concrete: a v1 reader skips the unknown field, rebuilds no database
/// topology for a capsule whose `db` is `null`, and a handler that checks
/// pool availability before querying takes a branch the recording never took
/// — a `mismatch` the guide tells operators to read as "the bug is gone".
/// Tolerating the document silently is precisely what the version gate exists
/// to prevent, so adding the field bumps the version.
///
/// | version | added |
/// | --- | --- |
/// | 1 | request, clock, database tape, outcome (#1598) |
/// | 2 | [`Capsule::db_roles`] |
/// | 3 | [`Capsule::effects`] (outbound HTTP, jobs, cache, mail, tenancy, randomness) and [`Capsule::job`] (#1634) |
///
/// Version 3 is the same kind of semantic bump version 2 was, one seam wider:
/// a v2 reader would skip `effects` entirely, replay a handler whose outbound
/// call, cache read or minted identifier is nowhere in the document, and
/// report a verdict on a run that never met the recording's effects.
pub const CAPSULE_FORMAT_VERSION: u32 = 3;

/// Errors surfaced when reading a capsule back from disk.
#[derive(Debug)]
pub enum CapsuleError {
    /// The capsule file could not be read.
    Io(std::io::Error),
    /// The capsule was not valid JSON, or did not match the schema.
    Malformed(serde_json::Error),
    /// The capsule was written by an incompatible format version.
    VersionMismatch {
        /// The version recorded in the capsule.
        found: u32,
        /// The version this build understands.
        expected: u32,
    },
}

impl std::fmt::Display for CapsuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "failed to read capsule: {error}"),
            Self::Malformed(error) => write!(f, "capsule is not a valid capsule document: {error}"),
            Self::VersionMismatch { found, expected } => {
                let direction = if found < expected {
                    "older than"
                } else {
                    "newer than"
                };
                write!(
                    f,
                    "capsule format version {found} is {direction} the version this build \
                     understands ({expected}), so replaying it would judge the handler against \
                     effects the document cannot describe. Re-record the capsule with this \
                     build, or replay it with the Autumn version that wrote it — see the \
                     \"Compatibility across Autumn versions\" section of the failure-capsule \
                     guide."
                )
            }
        }
    }
}

impl std::error::Error for CapsuleError {}

/// A recorded failure: one request, everything it observed, and how it ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capsule {
    /// Format version of this document; see [`CAPSULE_FORMAT_VERSION`].
    pub format_version: u32,
    /// Capsule identifier — the request id when one was available.
    pub id: String,
    /// When the capsule was written.
    pub captured_at: DateTime<Utc>,
    /// `autumn-web` version of the build that recorded it.
    pub autumn_version: String,
    /// Identity of the recording application.
    #[serde(default)]
    pub app: AppInfo,
    /// The redacted request that produced the failure.
    pub request: CapsuleRequest,
    /// What the client received.
    pub outcome: CapsuleOutcome,
    /// Clock readings taken during the request, in the order they were read.
    #[serde(default)]
    pub clock: Vec<DateTime<Utc>>,
    /// Monotonic clock readings taken during the request, in read order, as
    /// microseconds since the recording clock's origin. Serves
    /// `ClockSource::monotonic` during replay the way `clock` serves `now()`.
    /// Absent (empty) in capsules written before this field existed.
    #[serde(default)]
    pub clock_monotonic_us: Vec<u64>,
    /// Database traffic recorded for the request, when DB capture was active.
    #[serde(default)]
    pub db: Option<CapsuleDb>,
    /// Database roles the recording application had configured, whatever
    /// traffic the request produced.
    ///
    /// `db` is `None` when the request issued no wire traffic at all, which is
    /// a different fact from "this application has no database": a handler or
    /// state initializer that checks `state.pool()` or replica availability
    /// *before* querying would otherwise see a shape production never had.
    /// Absent in capsules recorded before this field existed.
    #[serde(default)]
    pub db_roles: Vec<String>,
    /// Set when a size cap stopped recording partway through; such a capsule
    /// is not replayable.
    #[serde(default)]
    pub truncated: bool,
    /// Human-readable notes about degraded capture (e.g. "db capture
    /// unavailable").
    #[serde(default)]
    pub notes: Vec<String>,
    /// Framework effects the recorded run produced outside the request and the
    /// database: outbound HTTP, job enqueues, cache reads and writes, mail
    /// sends, the resolved tenant, and the random bytes it drew (#1634).
    ///
    /// Each seam is served from here on replay, in recorded order, with the
    /// same no-live-effects posture the database tape has.
    #[serde(default)]
    pub effects: CapsuleEffects,
    /// The job this capsule replays, when the failure happened inside a job
    /// execution rather than while serving a request.
    ///
    /// `None` is the ordinary request capsule. When it is `Some`, `request`
    /// holds a synthetic descriptor of the job (so every field that names "the
    /// recorded entry point" still resolves) and replay dispatches the named
    /// job with the recorded payload instead of driving the router.
    #[serde(default)]
    pub job: Option<CapsuleJob>,
}

impl Capsule {
    /// Parse a capsule document, rejecting an incompatible format version.
    ///
    /// # Errors
    ///
    /// Returns [`CapsuleError::Malformed`] when the JSON does not match the
    /// schema and [`CapsuleError::VersionMismatch`] when the document was
    /// written by an incompatible build.
    pub fn from_json(json: &str) -> Result<Self, CapsuleError> {
        let capsule: Self = serde_json::from_str(json).map_err(CapsuleError::Malformed)?;
        if capsule.format_version == CAPSULE_FORMAT_VERSION {
            Ok(capsule)
        } else {
            Err(CapsuleError::VersionMismatch {
                found: capsule.format_version,
                expected: CAPSULE_FORMAT_VERSION,
            })
        }
    }
}

/// Identity of the application that recorded a capsule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    /// Application name, when the build exposed one.
    #[serde(default)]
    pub name: Option<String>,
    /// Active profile (e.g. `prod`).
    #[serde(default)]
    pub profile: Option<String>,
    /// Whether the recording binary was compiled with `debug_assertions` —
    /// `false` means a release build. `autumn replay` uses this to compile the
    /// replay binary the same way, so `cfg(debug_assertions)`-gated code and
    /// release-only failures behave as they did in the failing run. Absent in
    /// capsules recorded before this field existed.
    #[serde(default)]
    pub debug_assertions: Option<bool>,
}

/// The redacted request a capsule replays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleRequest {
    /// HTTP method (e.g. `GET`).
    pub method: String,
    /// Request target including the (redacted) query string.
    pub uri: String,
    /// Matched route template (e.g. `/users/{id}`), when routing had resolved.
    #[serde(default)]
    pub route: Option<String>,
    /// HTTP version, formatted as `http::Version` debug-prints it.
    pub http_version: String,
    /// Request headers in wire order, sensitive values already masked.
    pub headers: Vec<(String, String)>,
    /// Non-sensitive headers whose values are valid HTTP bytes but not valid
    /// UTF-8 (`obs-text` metadata), as `(name, base64(value))`. Kept apart so
    /// `headers` stays diffable text; replay restores both sets. A name with
    /// *any* obs-text value moves here wholesale — all its values, in
    /// original order — so `get_all(name)` order survives the split. Empty
    /// in capsules written before this field existed.
    #[serde(default)]
    pub binary_headers: Vec<(String, String)>,
    /// The (redacted) request body.
    pub body: CapsuleBody,
    /// Sorted list of what redaction masked, prefixed by location — e.g.
    /// `header:authorization`, `query:token`, `body:user.password`.
    #[serde(default)]
    pub redacted_keys: Vec<String>,
    /// The raw peer socket the request arrived on (`ConnectInfo`), before
    /// any trusted-proxy resolution — the proxy's own address and the real
    /// source port. Replay restores it verbatim so code inspecting the peer
    /// directly sees what the server saw.
    #[serde(default)]
    pub peer_addr: Option<std::net::SocketAddr>,
    /// The client address the trusted-proxies resolver settled on, when it
    /// ran. Replay re-anchors `ClientAddr` on this so identity-reading
    /// handlers reproduce without a real peer socket.
    #[serde(default)]
    pub client_addr: Option<std::net::IpAddr>,
    /// The external host the resolver settled on, restored so `ClientHost`
    /// replays the value the failing request saw rather than re-deriving one
    /// from an untrusted synthetic peer.
    #[serde(default)]
    pub client_host: Option<String>,
    /// The external scheme the resolver settled on, restored for
    /// `ClientScheme` for the same reason.
    #[serde(default)]
    pub client_scheme: Option<String>,
}

/// A captured request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleBody {
    /// The request carried no body.
    Absent,
    /// A UTF-8 body, stored verbatim after redaction.
    Text(String),
    /// A non-UTF-8 body, base64-encoded.
    Base64(String),
    /// The body was larger than the capture cap and was deliberately never
    /// consumed, so the handler still received it intact.
    Skipped {
        /// `Content-Length` the client declared, when it declared one.
        #[serde(default)]
        declared_len: Option<usize>,
    },
}

/// What the client received for the captured request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleOutcome {
    /// An ordinary response (always a 5xx for a recorded capsule).
    Status {
        /// HTTP status code.
        code: u16,
        /// Error message, from `AutumnErrorInfo` when present.
        message: String,
        /// Problem Details `type` URI, when the error carried one.
        #[serde(default)]
        problem_type: Option<String>,
    },
    /// A caught handler panic, turned into a sanitized 500.
    Panic {
        /// Status the client received (always 500).
        status: u16,
        /// The panic payload.
        payload: String,
        /// Backtrace, when `RUST_BACKTRACE` was set.
        #[serde(default)]
        backtrace: Option<String>,
    },
}

/// Database traffic recorded for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleDb {
    /// One tape per pooled connection the request touched.
    pub connections: Vec<ConnectionTape>,
}

/// Everything recorded on a single pooled connection.
///
/// `prologue`, `statements` and `catalog` carry the connection's *history*
/// (birth-to-request setup, prepared-statement metadata, `pg_catalog` lookups)
/// so a replayed request sees a warm connection even though it was captured on
/// one that had already been used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionTape {
    /// Recorder-assigned connection identifier.
    pub id: u64,
    /// Which pool role recorded this connection: `"primary"` or `"replica"`.
    /// Replay rebuilds one stub pool per role so a write-then-read request
    /// claims each tape from the pool it was recorded on. Capsules written
    /// before this field existed deserialize as `"primary"`, matching what
    /// they were.
    #[serde(default = "default_tape_role")]
    pub role: String,
    /// Exchanges from connection birth up to the first request binding.
    #[serde(default)]
    pub prologue: Vec<Exchange>,
    /// Parse/Describe metadata keyed by SQL, replayed on demand.
    #[serde(default)]
    pub statements: Vec<Exchange>,
    /// `pg_catalog` / `information_schema` lookups, replayed on demand.
    #[serde(default)]
    pub catalog: Vec<Exchange>,
    /// The request's own exchanges, in order.
    #[serde(default)]
    pub exchanges: Vec<Exchange>,
}

/// The role a tape deserializes with when the capsule predates roles: every
/// pre-role capsule was recorded on the primary.
fn default_tape_role() -> String {
    "primary".to_owned()
}

/// The `role` string replica-recorded tapes carry.
pub const TAPE_ROLE_REPLICA: &str = "replica";
/// The `role` string primary-recorded tapes carry.
pub const TAPE_ROLE_PRIMARY: &str = "primary";

/// Which Postgres protocol carried an exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeProtocol {
    /// Simple `Query` protocol (`batch_execute`).
    Simple,
    /// Extended protocol (Parse/Bind/Execute).
    Extended,
}

/// One request/response round trip on a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// Protocol that carried it.
    pub protocol: ExchangeProtocol,
    /// The SQL text the frontend sent.
    pub sql: String,
    /// Bind parameters, in order.
    #[serde(default)]
    pub binds: Vec<BindValue>,
    /// Raw backend frames, up to and including `ReadyForQuery`.
    #[serde(default, with = "b64")]
    pub response: Vec<u8>,
    /// Number of `DataRow` frames in `response`, for reporting.
    #[serde(default)]
    pub row_count: usize,
    /// Error text when the backend answered with `ErrorResponse`.
    #[serde(default)]
    pub error: Option<String>,
}

/// A single bind parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindValue {
    /// SQL `NULL` (wire length `-1`).
    Null,
    /// Raw parameter bytes.
    Value(#[serde(with = "b64")] Vec<u8>),
    /// A value byte-equal to something redaction masked. Excluded from replay
    /// bind comparison, because the capsule does not carry the real bytes.
    Masked,
}

// ── Effect tape (#1634) ─────────────────────────────────────────────────────

/// Every framework effect one recorded run produced outside the request and
/// the database.
///
/// Each list is in the order the run performed the effect. Ordering is
/// per-seam rather than global on purpose: two outbound calls the handler
/// `join!`s have no deterministic interleaving against each other, let alone
/// against a cache read on a third task, so a single global order would be a
/// fact the recording cannot actually establish — and replay would then report
/// a divergence for an interleaving that was never guaranteed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleEffects {
    /// Outbound HTTP request/response pairs made through
    /// [`http_client`](crate::http_client), in call order. Outbound webhook
    /// deliveries are covered here too: they send through the same client.
    #[serde(default)]
    pub http: Vec<HttpEffect>,
    /// Background jobs the run enqueued, in enqueue order.
    #[serde(default)]
    pub jobs: Vec<JobEffect>,
    /// Cache reads and writes, in call order.
    #[serde(default)]
    pub cache: Vec<CacheEffect>,
    /// Mail the run handed to the mailer, in send order.
    #[serde(default)]
    pub mail: Vec<MailEffect>,
    /// The tenant context the run resolved, when the app is multi-tenant.
    #[serde(default)]
    pub tenant: Option<TenantEffect>,
    /// Random byte draws taken through [`Entropy`](crate::entropy::Entropy),
    /// in draw order.
    #[serde(default)]
    pub random: Vec<RandomEffect>,
}

impl CapsuleEffects {
    /// Whether the run recorded no effects at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.http.is_empty()
            && self.jobs.is_empty()
            && self.cache.is_empty()
            && self.mail.is_empty()
            && self.tenant.is_none()
            && self.random.is_empty()
    }
}

/// One outbound HTTP request and the response it received.
///
/// Headers and structured bodies are masked through the same
/// `[log] filter_parameters` list the inbound request is: an outbound
/// `Authorization` header carries a downstream credential exactly the way an
/// inbound one carries the caller's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpEffect {
    /// HTTP method the client sent.
    pub method: String,
    /// Absolute request URL, with its query string redacted.
    pub url: String,
    /// Request headers the caller set, already masked.
    #[serde(default)]
    pub request_headers: Vec<(String, String)>,
    /// The (redacted) request body.
    #[serde(default = "absent_body")]
    pub request_body: CapsuleBody,
    /// Status the peer answered with. `0` when the call never got a response
    /// (see `error`).
    pub status: u16,
    /// Response headers, already masked.
    #[serde(default)]
    pub response_headers: Vec<(String, String)>,
    /// The (redacted) response body.
    #[serde(default = "absent_body")]
    pub response_body: CapsuleBody,
    /// Transport-level failure text, when the call produced no response at
    /// all. Replay reproduces the failure rather than a status.
    #[serde(default)]
    pub error: Option<String>,
}

/// A background job the recorded run enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEffect {
    /// Registered job name.
    pub name: String,
    /// The (redacted) JSON payload.
    pub payload: serde_json::Value,
    /// Delay the enqueue asked for, in seconds, when it was a scheduled
    /// enqueue.
    #[serde(default)]
    pub delay_secs: Option<i64>,
}

/// The job a job-scoped capsule replays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleJob {
    /// Registered job name.
    pub name: String,
    /// The (redacted) JSON payload the failing execution ran with.
    pub payload: serde_json::Value,
}

/// One cache interaction.
///
/// Values are base64-encoded JSON — the representation
/// [`insert_cached`](crate::cache::insert_cached) already produces for
/// cross-replica backends — so a recorded hit can be handed back to
/// [`get_cached`](crate::cache::get_cached) on replay without the capsule
/// having to carry Rust types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum CacheEffect {
    /// A read. `value` is `None` for a miss, and for a hit whose value the
    /// backend could not serialize (an in-process-only type) — the two are
    /// distinguished on replay by whether the key appears at all.
    Get {
        /// Cache key read.
        key: String,
        /// base64 of the JSON bytes served, when the read hit and the value
        /// was serializable.
        #[serde(default)]
        value: Option<String>,
    },
    /// A write.
    Insert {
        /// Cache key written.
        key: String,
        /// base64 of the JSON bytes written.
        value: String,
    },
}

impl CacheEffect {
    /// The key this interaction touched.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Get { key, .. } | Self::Insert { key, .. } => key,
        }
    }
}

/// One message handed to the mailer.
///
/// Replay asserts the send happened and never delivers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailEffect {
    /// `To` recipients. `Mail` carries no `Cc`/`Bcc`, so neither does this.
    #[serde(default)]
    pub to: Vec<String>,
    /// `From`, when the message set one explicitly.
    #[serde(default)]
    pub from: Option<String>,
    /// Subject line, masked like any other recorded text.
    pub subject: String,
    /// The (redacted) body.
    #[serde(default = "absent_body")]
    pub body: CapsuleBody,
    /// The delivery error the recorded send produced, when it failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// The tenant context the recorded run resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantEffect {
    /// Resolved tenant id, or `None` when the run resolved no tenant.
    #[serde(default)]
    pub id: Option<String>,
}

/// One draw from the framework's entropy source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RandomEffect {
    /// The bytes the draw produced.
    #[serde(with = "b64")]
    pub bytes: Vec<u8>,
}

/// `serde(default)` for the body fields on effect records: an effect whose
/// document omits a body carried none.
const fn absent_body() -> CapsuleBody {
    CapsuleBody::Absent
}

/// base64 (standard alphabet) serde adapter for byte fields.
pub(crate) mod b64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Builders that assemble capsule fixtures without a live database.
///
/// Replay tests need capsules whose tapes look exactly like recorded ones, but
/// standing up Postgres for every such test is far too slow. These builders
/// take the response frames as a prebuilt byte blob (the wire module owns
/// frame construction) and wrap the surrounding bookkeeping.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{
        AppInfo, BindValue, CAPSULE_FORMAT_VERSION, Capsule, CapsuleBody, CapsuleDb,
        CapsuleEffects, CapsuleOutcome, CapsuleRequest, ConnectionTape, Exchange, ExchangeProtocol,
    };

    /// An extended-protocol exchange with prebuilt backend frames.
    #[must_use]
    pub fn exchange(sql: &str, binds: Vec<BindValue>, response: Vec<u8>) -> Exchange {
        Exchange {
            protocol: ExchangeProtocol::Extended,
            sql: sql.to_owned(),
            binds,
            response,
            row_count: 0,
            error: None,
        }
    }

    /// A simple-protocol (`batch_execute`) exchange with prebuilt frames.
    #[must_use]
    pub fn simple_exchange(sql: &str, response: Vec<u8>) -> Exchange {
        Exchange {
            protocol: ExchangeProtocol::Simple,
            sql: sql.to_owned(),
            binds: Vec::new(),
            response,
            row_count: 0,
            error: None,
        }
    }

    /// A connection tape carrying only request exchanges.
    #[must_use]
    pub const fn connection_tape(id: u64, exchanges: Vec<Exchange>) -> ConnectionTape {
        ConnectionTape {
            id,
            // `String::new()` is const; an empty role reads as primary
            // everywhere a role is consulted, matching pre-role capsules.
            role: String::new(),
            prologue: Vec::new(),
            statements: Vec::new(),
            catalog: Vec::new(),
            exchanges,
        }
    }

    /// A minimal `GET` request record for a fixture capsule.
    #[must_use]
    pub fn request(method: &str, uri: &str) -> CapsuleRequest {
        CapsuleRequest {
            method: method.to_owned(),
            uri: uri.to_owned(),
            route: None,
            http_version: "HTTP/1.1".to_owned(),
            headers: Vec::new(),
            binary_headers: Vec::new(),
            body: CapsuleBody::Absent,
            redacted_keys: Vec::new(),
            peer_addr: None,
            client_addr: None,
            client_host: None,
            client_scheme: None,
        }
    }

    /// A fixture capsule at the current format version.
    #[must_use]
    pub fn capsule(request: CapsuleRequest, outcome: CapsuleOutcome) -> Capsule {
        Capsule {
            format_version: CAPSULE_FORMAT_VERSION,
            id: "fixture".to_owned(),
            captured_at: chrono::Utc::now(),
            autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
            app: AppInfo::default(),
            request,
            outcome,
            clock: Vec::new(),
            clock_monotonic_us: Vec::new(),
            db: None,
            db_roles: Vec::new(),
            truncated: false,
            notes: Vec::new(),
            effects: CapsuleEffects::default(),
            job: None,
        }
    }

    /// Attach connection tapes to a fixture capsule.
    #[must_use]
    pub fn with_connections(mut capsule: Capsule, connections: Vec<ConnectionTape>) -> Capsule {
        capsule.db = Some(CapsuleDb { connections });
        capsule
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Capsule {
        let mut capsule = test_support::capsule(
            test_support::request("POST", "/orders?page=2"),
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: Some("https://autumn.dev/problems/internal".to_owned()),
            },
        );
        capsule.id = "req-1".to_owned();
        capsule.request.headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("authorization".to_owned(), "[FILTERED]".to_owned()),
        ];
        capsule.request.body = CapsuleBody::Text("{\"a\":1}".to_owned());
        capsule.request.redacted_keys = vec!["header:authorization".to_owned()];
        capsule.clock = vec![Utc::now()];
        capsule.notes = vec!["db capture unavailable".to_owned()];
        capsule = test_support::with_connections(
            capsule,
            vec![test_support::connection_tape(
                1,
                vec![test_support::exchange(
                    "SELECT 1",
                    vec![
                        BindValue::Null,
                        BindValue::Value(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                        BindValue::Masked,
                    ],
                    vec![b'Z', 0, 0, 0, 5, b'I'],
                )],
            )],
        );
        capsule
    }

    /// #1634 Phase 0: the effect tape round-trips, and every kind survives.
    #[test]
    fn capsule_effects_round_trip() {
        let mut capsule = sample();
        capsule.effects.http.push(HttpEffect {
            method: "POST".to_owned(),
            url: "https://payments.example/charge".to_owned(),
            request_headers: vec![("authorization".to_owned(), "[FILTERED]".to_owned())],
            request_body: CapsuleBody::Text("{\"amount\":10}".to_owned()),
            status: 502,
            response_headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            response_body: CapsuleBody::Base64("AAEC".to_owned()),
            error: None,
        });
        capsule.effects.jobs.push(JobEffect {
            name: "send_receipt".to_owned(),
            payload: serde_json::json!({"order": 7}),
            delay_secs: Some(30),
        });
        capsule.effects.cache.push(CacheEffect::Get {
            key: "user:7".to_owned(),
            value: Some("eyJhIjoxfQ==".to_owned()),
        });
        capsule.effects.cache.push(CacheEffect::Insert {
            key: "user:7".to_owned(),
            value: "eyJhIjoxfQ==".to_owned(),
        });
        capsule.effects.mail.push(MailEffect {
            to: vec!["a@example.com".to_owned()],
            from: Some("noreply@example.com".to_owned()),
            subject: "Receipt".to_owned(),
            body: CapsuleBody::Text("thanks".to_owned()),
            error: None,
        });
        capsule.effects.tenant = Some(TenantEffect {
            id: Some("acme".to_owned()),
        });
        capsule.effects.random.push(RandomEffect {
            bytes: vec![0xAA, 0xBB, 0xCC],
        });

        let json = serde_json::to_string(&capsule).expect("capsule serializes");
        let parsed = Capsule::from_json(&json).expect("capsule round-trips");
        assert_eq!(parsed.effects, capsule.effects);
        assert_eq!(
            parsed.effects.random.first().map(|draw| draw.bytes.clone()),
            Some(vec![0xAA, 0xBB, 0xCC]),
            "random draws are byte-exact after the base64 hop"
        );
    }

    /// A job-entry capsule names the job it replays instead of a route.
    #[test]
    fn capsule_job_entry_round_trips() {
        let mut capsule = sample();
        capsule.job = Some(CapsuleJob {
            name: "send_receipt".to_owned(),
            payload: serde_json::json!({"order": 7}),
        });
        let json = serde_json::to_string(&capsule).expect("capsule serializes");
        let parsed = Capsule::from_json(&json).expect("capsule round-trips");
        assert_eq!(
            parsed.job.as_ref().map(|job| job.name.as_str()),
            Some("send_receipt")
        );
    }

    /// The effect tape is a semantic addition a v2 reader would skip while
    /// happily reporting a reproduction, so it bumps the format version.
    ///
    /// Asserted through the gate rather than against the constant: a bare
    /// `CAPSULE_FORMAT_VERSION >= 3` is true forever and proves nothing about
    /// what the reader actually does with a v2 document.
    #[test]
    fn a_capsule_without_the_effect_tape_cannot_be_read_by_this_build() {
        let mut json = serde_json::to_value(sample()).expect("capsule serializes");
        json["format_version"] = serde_json::json!(2);
        // Exactly what a v2 capsule looks like: no `effects`, no `job`.
        if let serde_json::Value::Object(map) = &mut json {
            map.remove("effects");
            map.remove("job");
        }
        Capsule::from_json(&json.to_string()).expect_err(
            "a document with no effect tape must not load as one that has an empty one",
        );
    }

    /// A capsule written before the effect tape existed must be refused, not
    /// read with empty seams — the "spurious pass" the compatibility AC forbids.
    #[test]
    fn a_pre_effects_capsule_is_refused_by_the_version_gate() {
        let mut capsule = sample();
        capsule.format_version = 2;
        let json = serde_json::to_string(&capsule).expect("capsule serializes");
        let error = Capsule::from_json(&json).expect_err("a v2 capsule must be refused");
        let message = error.to_string();
        assert!(
            message.contains("older") || message.contains("re-record"),
            "the refusal must be actionable: {message}"
        );
    }

    #[test]
    fn capsule_json_roundtrips_v1() {
        let capsule = sample();
        let json = serde_json::to_string(&capsule).expect("capsule serializes");
        let parsed = Capsule::from_json(&json).expect("capsule round-trips");

        assert_eq!(parsed.format_version, CAPSULE_FORMAT_VERSION);
        assert_eq!(parsed.id, "req-1");
        assert_eq!(parsed.request, capsule.request);
        assert_eq!(parsed.outcome, capsule.outcome);
        assert_eq!(parsed.clock, capsule.clock);
        assert_eq!(parsed.db, capsule.db);
        assert_eq!(parsed.notes, capsule.notes);

        // Byte fields survive the base64 hop intact.
        let db = parsed.db.expect("db tape present");
        let exchange = db
            .connections
            .first()
            .and_then(|tape| tape.exchanges.first())
            .expect("one exchange");
        assert_eq!(exchange.response, vec![b'Z', 0, 0, 0, 5, b'I']);
        assert_eq!(
            exchange.binds,
            vec![
                BindValue::Null,
                BindValue::Value(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                BindValue::Masked,
            ]
        );
    }

    #[test]
    fn capsule_with_unknown_future_field_still_loads() {
        let json = serde_json::to_value(sample()).expect("capsule serializes");
        let mut object = match json {
            serde_json::Value::Object(map) => map,
            other => panic!("capsule must serialize to an object, got {other}"),
        };
        object.insert("future_knob".to_owned(), serde_json::json!({"a": [1, 2]}));
        let json = serde_json::Value::Object(object).to_string();

        let parsed = Capsule::from_json(&json)
            .expect("a capsule carrying an unknown field must still load (forward compatibility)");
        assert_eq!(parsed.id, "req-1");
    }

    #[test]
    fn load_rejects_format_version_mismatch() {
        let mut capsule = sample();
        capsule.format_version = CAPSULE_FORMAT_VERSION + 1;
        let json = serde_json::to_string(&capsule).expect("capsule serializes");

        let error = Capsule::from_json(&json)
            .expect_err("a future format version must be rejected, not silently read");
        match error {
            CapsuleError::VersionMismatch { found, expected } => {
                assert_eq!(found, CAPSULE_FORMAT_VERSION + 1);
                assert_eq!(expected, CAPSULE_FORMAT_VERSION);
            }
            other => panic!("expected a version mismatch, got {other}"),
        }
        assert!(
            CapsuleError::VersionMismatch {
                found: 99,
                expected: CAPSULE_FORMAT_VERSION,
            }
            .to_string()
            .contains("format version 99"),
            "the mismatch message must name the offending version"
        );
    }
}

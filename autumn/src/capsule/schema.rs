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
    )
)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Version of the capsule document format understood by this build.
///
/// Bumped whenever the schema changes in a way a previous reader cannot
/// tolerate. Replay refuses any capsule whose `format_version` differs.
pub const CAPSULE_FORMAT_VERSION: u32 = 1;

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
            Self::VersionMismatch { found, expected } => write!(
                f,
                "capsule format version {found} is not supported by this build \
                 (expected {expected}); re-record the capsule with a matching Autumn build"
            ),
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
    /// Database traffic recorded for the request, when DB capture was active.
    #[serde(default)]
    pub db: Option<CapsuleDb>,
    /// Set when a size cap stopped recording partway through; such a capsule
    /// is not replayable.
    #[serde(default)]
    pub truncated: bool,
    /// Human-readable notes about degraded capture (e.g. "db capture
    /// unavailable").
    #[serde(default)]
    pub notes: Vec<String>,
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
    /// The (redacted) request body.
    pub body: CapsuleBody,
    /// Sorted list of what redaction masked, prefixed by location — e.g.
    /// `header:authorization`, `query:token`, `body:user.password`.
    #[serde(default)]
    pub redacted_keys: Vec<String>,
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
        CapsuleOutcome, CapsuleRequest, ConnectionTape, Exchange, ExchangeProtocol,
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
            body: CapsuleBody::Absent,
            redacted_keys: Vec::new(),
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
            db: None,
            truncated: false,
            notes: Vec::new(),
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

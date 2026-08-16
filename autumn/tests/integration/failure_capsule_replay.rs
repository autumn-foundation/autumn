//! Integration tests for capsule *replay* (issue #1598) — the in-process stub
//! `PostgreSQL` server, the replay driver and the verdict. Every test here is
//! Docker-free: the capsules are hand-built fixtures, and the "database" is a
//! [`tokio::io::duplex`] pipe with a recorded tape on the far end.
//!
//! Verifies that:
//!   * a real diesel handler decodes real rows out of a recorded tape;
//!   * a recorded 500 and a recorded panic both replay to the same outcome;
//!   * a query the tape never recorded, or a bind that does not match, is
//!     reported as a divergence rather than silently answered;
//!   * a *recorded* exchange the replayed run never issued is reported too, so
//!     a run that reaches the recorded outcome without following the recorded
//!     database effects cannot claim a reproduction;
//!   * a bind the capsule masked is excluded from that comparison;
//!   * replay never dials a socket;
//!   * clock reads come out of the capsule, and over-reading warns;
//!   * a truncated capsule and a capsule from another format version are
//!     refused with exit code 2;
//!   * a 401 replayed from a recorded 5xx points at redaction as the cause.

use std::sync::Arc;

use chrono::{DateTime, TimeZone as _, Utc};

use autumn_web::AppState;
use autumn_web::capsule::{
    AppInfo, BindValue, CAPSULE_FORMAT_VERSION, Capsule, CapsuleBody, CapsuleDb, CapsuleError,
    CapsuleOutcome, CapsuleRequest, ConnectionTape, DivergenceKind, DivergenceLog, Exchange,
    ExchangeProtocol, ReplayClock, Verdict, execute, pool_from_capsule, print_refusal,
    print_verdict, refusal_reason,
};
use autumn_web::config::AutumnConfig;
use autumn_web::db::Db;
use autumn_web::test::TestApp;
use autumn_web::{AutumnError, get, routes};
use axum::extract::State;
use diesel_async::RunQueryDsl as _;

// ── Postgres backend frame builders ─────────────────────────────────────────
//
// Deliberately hand-rolled rather than reused from `capsule::wire::build`:
// these fixtures must describe the wire independently of the code under test,
// and `wire` is crate-private anyway.

/// `[tag][len including the four length bytes][payload]`.
fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = i32::try_from(payload.len() + 4).expect("frame fits in i32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn cstr(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn parse_complete() -> Vec<u8> {
    frame(b'1', &[])
}

fn bind_complete() -> Vec<u8> {
    frame(b'2', &[])
}

fn parameter_description(oids: &[u32]) -> Vec<u8> {
    let mut payload = i16::try_from(oids.len())
        .expect("oid count")
        .to_be_bytes()
        .to_vec();
    for oid in oids {
        payload.extend_from_slice(&oid.to_be_bytes());
    }
    frame(b't', &payload)
}

/// `RowDescription` from `(name, type_oid)` pairs, all declared text-format
/// (which is what a real server sends for a statement-level Describe — the
/// actual row encoding is decided by the `Bind` result-format codes).
fn row_description(cols: &[(&str, u32)]) -> Vec<u8> {
    let mut payload = i16::try_from(cols.len())
        .expect("column count")
        .to_be_bytes()
        .to_vec();
    for (name, oid) in cols {
        cstr(&mut payload, name);
        payload.extend_from_slice(&0i32.to_be_bytes()); // table oid
        payload.extend_from_slice(&0i16.to_be_bytes()); // attnum
        payload.extend_from_slice(&oid.to_be_bytes());
        payload.extend_from_slice(&(-1i16).to_be_bytes()); // type length
        payload.extend_from_slice(&(-1i32).to_be_bytes()); // atttypmod
        payload.extend_from_slice(&0i16.to_be_bytes()); // format code
    }
    frame(b'T', &payload)
}

/// `DataRow`. `tokio-postgres` always binds with result format code 1, so every
/// field here is **binary**-encoded (see `int8`/`text` below).
fn data_row(fields: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut payload = i16::try_from(fields.len())
        .expect("field count")
        .to_be_bytes()
        .to_vec();
    for field in fields {
        match field {
            Some(bytes) => {
                payload.extend_from_slice(
                    &i32::try_from(bytes.len()).expect("field len").to_be_bytes(),
                );
                payload.extend_from_slice(bytes);
            }
            None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    frame(b'D', &payload)
}

fn command_complete(tag: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    cstr(&mut payload, tag);
    frame(b'C', &payload)
}

fn ready_for_query(state: u8) -> Vec<u8> {
    frame(b'Z', &[state])
}

/// Binary wire form of a Postgres `int8` (OID 20).
fn int8(value: i64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// Binary wire form of a Postgres `text` (OID 25) — the raw UTF-8 bytes.
fn text(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

const OID_INT8: u32 = 20;
const OID_TEXT: u32 = 25;

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flat_map(|part| part.iter().copied()).collect()
}

// ── Capsule fixture builders ────────────────────────────────────────────────

fn exchange(sql: &str, binds: Vec<BindValue>, response: Vec<u8>) -> Exchange {
    Exchange {
        protocol: ExchangeProtocol::Extended,
        sql: sql.to_owned(),
        binds,
        response,
        row_count: 0,
        error: None,
    }
}

fn request(method: &str, uri: &str) -> CapsuleRequest {
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

fn capsule(request: CapsuleRequest, outcome: CapsuleOutcome) -> Capsule {
    Capsule {
        format_version: CAPSULE_FORMAT_VERSION,
        id: "fixture".to_owned(),
        captured_at: Utc::now(),
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
    }
}

fn with_connections(mut capsule: Capsule, connections: Vec<ConnectionTape>) -> Capsule {
    capsule.db = Some(CapsuleDb { connections });
    capsule
}

/// The `SELECT id, name FROM widgets` statement metadata a warm connection
/// would already carry (the `Parse`/`Describe` answer).
///
/// `param_oids` must match the query's placeholders exactly: `tokio-postgres`
/// builds its `Statement` from this `ParameterDescription` and rejects a `Bind`
/// of a different arity client-side, before any wire traffic.
fn widgets_statement(sql: &str, param_oids: &[u32]) -> Exchange {
    exchange(
        sql,
        Vec::new(),
        concat(&[
            parse_complete(),
            parameter_description(param_oids),
            row_description(&[("id", OID_INT8), ("name", OID_TEXT)]),
            ready_for_query(b'I'),
        ]),
    )
}

/// The `Bind`/`Execute` answer carrying two rows.
fn widgets_rows(sql: &str, binds: Vec<BindValue>) -> Exchange {
    exchange(
        sql,
        binds,
        concat(&[
            bind_complete(),
            data_row(&[Some(int8(1)), Some(text("anvil"))]),
            data_row(&[Some(int8(2)), Some(text("rope"))]),
            command_complete("SELECT 2"),
            ready_for_query(b'I'),
        ]),
    )
}

/// A tape for `sql` holding both the statement metadata and one execution.
fn widgets_tape(sql: &str, param_oids: &[u32], binds: Vec<BindValue>) -> ConnectionTape {
    ConnectionTape {
        id: 7,
        role: "primary".to_owned(),
        prologue: Vec::new(),
        statements: vec![widgets_statement(sql, param_oids)],
        catalog: Vec::new(),
        exchanges: vec![widgets_rows(sql, binds)],
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName, Debug)]
struct Widget {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

const WIDGETS_SQL: &str = "SELECT id, name FROM widgets";
const WIDGET_BY_ID_SQL: &str = "SELECT id, name FROM widgets WHERE id = $1";
const GADGETS_SQL: &str = "SELECT id, name FROM gadgets";

#[get("/widgets")]
async fn widgets(mut db: Db) -> Result<String, AutumnError> {
    let rows: Vec<Widget> = diesel::sql_query(WIDGETS_SQL)
        .load(&mut *db)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("widgets query failed: {error}"))
        })?;
    Ok(rows
        .iter()
        .map(|row| format!("{}:{}", row.id, row.name))
        .collect::<Vec<_>>()
        .join(","))
}

/// Same query, but with a bind — the bind-comparison tests drive this one.
#[get("/widget")]
async fn widget(mut db: Db) -> Result<String, AutumnError> {
    let rows: Vec<Widget> = diesel::sql_query(WIDGET_BY_ID_SQL)
        .bind::<diesel::sql_types::BigInt, _>(1i64)
        .load(&mut *db)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("widget query failed: {error}"))
        })?;
    Ok(rows
        .iter()
        .map(|row| format!("{}:{}", row.id, row.name))
        .collect::<Vec<_>>()
        .join(","))
}

/// Uses two pooled connections in a fixed order, one query each. The replay
/// pool hands tape *i* to the *i*-th connection it opens, so this is the shape
/// that catches tapes listed in the wrong order.
#[get("/two-connections")]
async fn two_connections(State(state): State<AppState>) -> Result<String, AutumnError> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::internal_server_error_msg("no pool"))?
        .clone();
    let mut first = pool
        .get()
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(format!("first: {error}")))?;
    let mut second = pool
        .get()
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(format!("second: {error}")))?;

    let widgets: Vec<Widget> = diesel::sql_query(WIDGETS_SQL)
        .load(&mut first)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("widgets query failed: {error}"))
        })?;
    let gadgets: Vec<Widget> = diesel::sql_query(GADGETS_SQL)
        .load(&mut second)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("gadgets query failed: {error}"))
        })?;
    Ok(format!("{}/{}", widgets.len(), gadgets.len()))
}

/// Asks the database something the tape never recorded.
#[get("/unrecorded")]
async fn unrecorded(mut db: Db) -> Result<String, AutumnError> {
    let rows: Vec<Widget> = diesel::sql_query("SELECT id, name FROM gadgets")
        .load(&mut *db)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("gadgets query failed: {error}"))
        })?;
    Ok(format!("{} rows", rows.len()))
}

#[get("/fail")]
async fn fail() -> Result<&'static str, AutumnError> {
    Err(AutumnError::internal_server_error_msg("database on fire"))
}

#[get("/boom")]
async fn boom() -> &'static str {
    panic!("kaboom in handler");
}

/// The same panic mounted on a bare axum router, where nothing catches it.
async fn bare_boom() -> &'static str {
    panic!("kaboom in handler");
}

#[get("/denied")]
async fn denied() -> Result<&'static str, AutumnError> {
    Err(AutumnError::unauthorized_msg("missing credentials"))
}

/// Reads the clock twice and reports both readings.
#[get("/clock")]
async fn clock_twice(State(state): State<AppState>) -> String {
    let first = state.clock().now();
    let second = state.clock().now();
    format!("{}|{}", first.to_rfc3339(), second.to_rfc3339())
}

// ── Harness ─────────────────────────────────────────────────────────────────

fn test_config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config
}

/// A [`ReplayClock`] the test keeps a handle on (`TestApp::with_clock` takes
/// ownership, and the over-read count has to be readable afterwards).
struct SharedClock(Arc<ReplayClock>);

impl autumn_web::time::ClockSource for SharedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0.now()
    }
}

/// A recorded instant, far enough in the past that it cannot be confused with
/// a reading that leaked in from the real wall clock.
fn at(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2001, 3, 4, 5, 6, second)
        .single()
        .expect("valid timestamp")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stub_pool_serves_recorded_rows_to_a_real_handler() {
    let capsule = with_connections(
        capsule(
            request("GET", "/widgets"),
            CapsuleOutcome::Status {
                code: 500,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![widgets_tape(WIDGETS_SQL, &[], Vec::new())],
    );
    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&capsule, Arc::clone(&divergences)).expect("replay pool builds");

    let client = TestApp::new()
        .config(test_config())
        .routes(routes![widgets])
        .with_db(pool)
        .build();
    let response = client.get("/widgets").send().await;

    response.assert_ok();
    assert_eq!(
        response.text(),
        "1:anvil,2:rope",
        "a real diesel handler must decode the rows the tape recorded"
    );
    assert!(
        divergences.is_empty(),
        "a faithful replay must produce no divergences, got {:?}",
        divergences.entries()
    );
}

#[tokio::test]
async fn replay_reproduces_the_recorded_500() {
    let recorded = capsule(
        request("GET", "/fail"),
        CapsuleOutcome::Status {
            code: 500,
            message: "database on fire".to_owned(),
            problem_type: None,
        },
    );
    let router = TestApp::new()
        .config(test_config())
        .routes(routes![fail])
        .build()
        .into_router();

    let divergences = Arc::new(DivergenceLog::new());
    let outcome = execute(router, &recorded, divergences, None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the same 500 must count as a reproduction, got {outcome:?}"
    );
    match outcome.actual {
        CapsuleOutcome::Status { code, .. } => assert_eq!(code, 500),
        other @ CapsuleOutcome::Panic { .. } => {
            panic!("expected a status outcome, got {other:?}")
        }
    }
}

#[tokio::test]
async fn replay_reproduces_the_recorded_panic() {
    let recorded = capsule(
        request("GET", "/boom"),
        CapsuleOutcome::Panic {
            status: 500,
            payload: "kaboom in handler".to_owned(),
            backtrace: None,
        },
    );

    // A bare axum router: the panic escapes into the driver, which is the path
    // `execute`'s `catch_unwind` exists for.
    let bare = axum::Router::new().route("/boom", axum::routing::get(bare_boom));
    let divergences = Arc::new(DivergenceLog::new());
    let outcome = execute(bare, &recorded, Arc::clone(&divergences), None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "a replayed panic must be captured and compared, not abort the run: {outcome:?}"
    );
    match &outcome.actual {
        CapsuleOutcome::Panic { payload, .. } => assert!(
            payload.contains("kaboom in handler"),
            "the captured payload must name the panic, got {payload:?}"
        ),
        other @ CapsuleOutcome::Status { .. } => {
            panic!("expected a panic outcome, got {other:?}")
        }
    }

    // And through the framework, whose panic middleware turns it into a 500
    // before the driver ever sees it: still a reproduction.
    let framework = TestApp::new()
        .config(test_config())
        .routes(routes![boom])
        .build()
        .into_router();
    let outcome = execute(framework, &recorded, Arc::new(DivergenceLog::new()), None).await;
    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "a panic the framework catches into the recorded status is still a reproduction: {outcome:?}"
    );
}

#[tokio::test]
async fn replay_reports_divergence_on_unrecorded_query() {
    let recorded = with_connections(
        capsule(
            request("GET", "/unrecorded"),
            CapsuleOutcome::Status {
                code: 200,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![widgets_tape(WIDGETS_SQL, &[], Vec::new())],
    );
    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let router = TestApp::new()
        .config(test_config())
        .routes(routes![unrecorded])
        .with_db(pool)
        .build()
        .into_router();

    let outcome = execute(router, &recorded, Arc::clone(&divergences), None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Diverged,
        "a query the tape never recorded must be a divergence: {outcome:?}"
    );
    let entries = divergences.entries();
    assert!(
        entries.iter().any(|entry| {
            entry.kind == DivergenceKind::UnrecordedQuery && entry.actual_sql.contains("gadgets")
        }),
        "the divergence must name the unexpected SQL, got {entries:?}"
    );
    assert!(
        outcome
            .divergences
            .iter()
            .any(|entry| entry.detail.contains("gadgets")),
        "the outcome must carry the divergence detail, got {:?}",
        outcome.divergences
    );
}

#[tokio::test]
async fn replay_reports_divergence_when_a_recorded_exchange_is_never_issued() {
    // The recording executed the query twice; the replayed handler issues it
    // once. The status still matches, so without an accounting of the *whole*
    // tape this run would be reported as a faithful reproduction.
    let mut tape = widgets_tape(WIDGETS_SQL, &[], Vec::new());
    tape.exchanges.push(widgets_rows(WIDGETS_SQL, Vec::new()));
    let recorded = with_connections(
        capsule(
            request("GET", "/widgets"),
            CapsuleOutcome::Status {
                code: 200,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![tape],
    );

    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let router = TestApp::new()
        .config(test_config())
        .routes(routes![widgets])
        .with_db(pool)
        .build()
        .into_router();

    let outcome = execute(router, &recorded, Arc::clone(&divergences), None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Diverged,
        "a run that skipped a recorded exchange has not reproduced the recording: {outcome:?}"
    );
    let entry = outcome
        .divergences
        .iter()
        .find(|entry| entry.kind == DivergenceKind::UnconsumedExchanges)
        .unwrap_or_else(|| {
            panic!(
                "the leftover exchange must be reported, got {:?}",
                outcome.divergences
            )
        });
    assert_eq!(
        entry.connection, 7,
        "the divergence must name the connection"
    );
    assert_eq!(
        entry.exchange_index, 1,
        "the divergence must point at the first unconsumed exchange"
    );
    assert_eq!(
        entry.expected_sql.as_deref(),
        Some(WIDGETS_SQL),
        "the divergence must quote the SQL that was never issued"
    );
    assert!(
        entry.detail.contains(WIDGETS_SQL),
        "the detail must name the unissued statement, got {:?}",
        entry.detail
    );
}

#[tokio::test]
async fn replay_reports_divergence_when_a_whole_tape_is_never_claimed() {
    // The recording used the database; the replayed handler does not touch it
    // at all, so the pool never opens a connection and the tape is never
    // claimed. The 500 comes back either way — that is exactly the run that
    // must not be called a reproduction.
    let recorded = with_connections(
        capsule(
            request("GET", "/fail"),
            CapsuleOutcome::Status {
                code: 500,
                message: "database on fire".to_owned(),
                problem_type: None,
            },
        ),
        vec![widgets_tape(WIDGETS_SQL, &[], Vec::new())],
    );

    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let router = TestApp::new()
        .config(test_config())
        .routes(routes![fail])
        .with_db(pool)
        .build()
        .into_router();

    let outcome = execute(router, &recorded, Arc::clone(&divergences), None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Diverged,
        "the same 500 without any of the recorded database traffic is not a reproduction: \
         {outcome:?}"
    );
    assert!(
        outcome.divergences.iter().any(|entry| {
            entry.kind == DivergenceKind::UnconsumedExchanges
                && entry.connection == 7
                && entry.exchange_index == 0
        }),
        "an unclaimed tape must be accounted for like a partly-consumed one, got {:?}",
        outcome.divergences
    );
}

#[tokio::test]
async fn tapes_are_claimed_in_the_order_the_recorded_request_used_its_connections() {
    // Connection ids are process-wide birth order, so the request's *first*
    // connection can carry the higher id — exactly the ordering that goes wrong
    // if tapes are sorted by id anywhere between the recorder and the replay
    // pool. The capsule lists them in first-use order, and the replay pool must
    // hand them out in that order: connection 20's tape to the first connection
    // opened, connection 3's to the second.
    let mut widgets = widgets_tape(WIDGETS_SQL, &[], Vec::new());
    widgets.id = 20;
    let mut gadgets = widgets_tape(GADGETS_SQL, &[], Vec::new());
    gadgets.id = 3;

    let recorded = with_connections(
        capsule(
            request("GET", "/two-connections"),
            CapsuleOutcome::Status {
                code: 200,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![widgets, gadgets],
    );

    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let client = TestApp::new()
        .config(test_config())
        .routes(routes![two_connections])
        .with_db(pool)
        .build();

    let response = client.get("/two-connections").send().await;
    response.assert_ok();
    assert_eq!(
        response.text(),
        "2/2",
        "each connection must be answered from the tape the request used it for"
    );
    assert!(
        divergences.entries().is_empty(),
        "tapes handed out in first-use order must replay clean, got {:?}",
        divergences.entries()
    );
    assert!(
        divergences.unconsumed().is_empty(),
        "and both tapes must be fully consumed, got {:?}",
        divergences.unconsumed()
    );
}

#[tokio::test]
async fn a_capsule_without_database_traffic_is_unaffected_by_the_tape_audit() {
    // `db: None`: there is nothing to consume, so the verdict must be decided
    // by the outcome alone.
    let recorded = capsule(
        request("GET", "/fail"),
        CapsuleOutcome::Status {
            code: 500,
            message: "database on fire".to_owned(),
            problem_type: None,
        },
    );
    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let router = TestApp::new()
        .config(test_config())
        .routes(routes![fail])
        .with_db(pool)
        .build()
        .into_router();

    let outcome = execute(router, &recorded, divergences, None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "a capsule with no database tape must still reproduce: {outcome:?}"
    );
}

#[tokio::test]
async fn replay_reports_divergence_on_bind_mismatch() {
    // The tape recorded the query bound to 99; the handler binds 1.
    let recorded = with_connections(
        capsule(
            request("GET", "/widget"),
            CapsuleOutcome::Status {
                code: 200,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![widgets_tape(
            WIDGET_BY_ID_SQL,
            &[OID_INT8],
            vec![BindValue::Value(99i64.to_be_bytes().to_vec())],
        )],
    );
    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let client = TestApp::new()
        .config(test_config())
        .routes(routes![widget])
        .with_db(pool)
        .build();

    let response = client.get("/widget").send().await;
    assert_eq!(
        response.status.as_u16(),
        500,
        "a bind mismatch must fail the query rather than serve the wrong rows"
    );
    let entries = divergences.entries();
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == DivergenceKind::BindMismatch),
        "expected a bind mismatch, got {entries:?}"
    );
}

#[tokio::test]
async fn replay_ignores_bind_mismatch_for_masked_binds() {
    // Redaction blanked the bind, so the capsule cannot carry the real bytes —
    // the comparison must be skipped rather than reported.
    let recorded = with_connections(
        capsule(
            request("GET", "/widget"),
            CapsuleOutcome::Status {
                code: 200,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![widgets_tape(
            WIDGET_BY_ID_SQL,
            &[OID_INT8],
            vec![BindValue::Masked],
        )],
    );
    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, Arc::clone(&divergences)).expect("replay pool builds");
    let client = TestApp::new()
        .config(test_config())
        .routes(routes![widget])
        .with_db(pool)
        .build();

    let response = client.get("/widget").send().await;
    response.assert_ok();
    assert_eq!(response.text(), "1:anvil,2:rope");
    assert!(
        divergences.is_empty(),
        "a masked bind must be excluded from comparison, got {:?}",
        divergences.entries()
    );
}

#[tokio::test]
async fn replay_never_opens_a_socket() {
    // The manager URL points at a port nothing can be listening on; if replay
    // ever dialled it, the checkout would fail instead of serving rows.
    let recorded = with_connections(
        capsule(
            request("GET", "/widgets"),
            CapsuleOutcome::Status {
                code: 200,
                message: String::new(),
                problem_type: None,
            },
        ),
        vec![widgets_tape(WIDGETS_SQL, &[], Vec::new())],
    );
    let divergences = Arc::new(DivergenceLog::new());
    let pool = pool_from_capsule(&recorded, divergences).expect("replay pool builds");

    let mut conn = pool.get().await.expect("stub connection checks out");
    let rows: Vec<Widget> = diesel::sql_query(WIDGETS_SQL)
        .load(&mut conn)
        .await
        .expect("the stub answers without any network");
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn replay_serves_clock_reads_from_the_capsule() {
    let mut recorded = capsule(
        request("GET", "/clock"),
        CapsuleOutcome::Status {
            code: 200,
            message: String::new(),
            problem_type: None,
        },
    );
    // Deliberately generous: middleware may take readings of its own before the
    // handler runs, and the claim under test is that every reading comes out of
    // the capsule — not that the handler is the only reader.
    recorded.clock = (1..=8).map(at).collect();

    let clock = Arc::new(ReplayClock::new(recorded.clock.clone(), at(0)));
    let client = TestApp::new()
        .config(test_config())
        .routes(routes![clock_twice])
        .with_clock(SharedClock(Arc::clone(&clock)))
        .build();

    // The real replay driver wraps its router call in the replay-request
    // scope, which is what entitles the request to consume recorded readings
    // (reads outside it — boot, spawned tasks — are non-consuming). This
    // harness stands in for the driver, so it wraps the same way.
    let response =
        autumn_web::capsule::clock::with_replay_request_scope(client.get("/clock").send()).await;
    response.assert_ok();

    let body = response.text();
    let (first, second) = body
        .split_once('|')
        .unwrap_or_else(|| panic!("handler must report two readings, got {body:?}"));
    let recorded_readings: Vec<String> = recorded
        .clock
        .iter()
        .map(chrono::DateTime::to_rfc3339)
        .collect();
    assert!(
        recorded_readings.iter().any(|reading| reading == first),
        "the first reading {first:?} must come from the capsule, not the wall clock"
    );
    assert!(
        recorded_readings.iter().any(|reading| reading == second),
        "the second reading {second:?} must come from the capsule, not the wall clock"
    );
    assert!(first < second, "readings must be served in recorded order");
    assert_eq!(
        clock.over_reads(),
        0,
        "eight recorded readings must be more than the run needs"
    );
}

#[tokio::test]
async fn clock_over_read_reuses_last_reading_and_warns() {
    let mut recorded = capsule(
        request("GET", "/clock"),
        CapsuleOutcome::Status {
            code: 200,
            message: String::new(),
            problem_type: None,
        },
    );
    // One reading recorded, two reads at replay: the code changed.
    recorded.clock = vec![at(1)];

    let clock = Arc::new(ReplayClock::new(recorded.clock.clone(), at(0)));
    let router = TestApp::new()
        .config(test_config())
        .routes(routes![clock_twice])
        .with_clock(SharedClock(Arc::clone(&clock)))
        .build()
        .into_router();

    let outcome = execute(
        router,
        &recorded,
        Arc::new(DivergenceLog::new()),
        Some(clock.as_ref()),
    )
    .await;

    assert!(
        clock.over_reads() >= 1,
        "reading past the end of the recording must be counted"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("clock")),
        "the verdict must warn about the over-read, got {:?}",
        outcome.warnings
    );
}

#[test]
fn replay_rejects_truncated_capsule() {
    let mut recorded = capsule(
        request("GET", "/widgets"),
        CapsuleOutcome::Status {
            code: 500,
            message: String::new(),
            problem_type: None,
        },
    );
    recorded.truncated = true;

    let reason = refusal_reason(&recorded).expect("a truncated capsule must be refused");
    assert!(
        reason.contains("truncated"),
        "the refusal must say why, got {reason:?}"
    );
    assert_eq!(
        print_refusal(&reason, std::path::Path::new("/tmp/capsule.json")),
        2,
        "refusing a capsule must exit 2"
    );

    let intact = capsule(
        request("GET", "/widgets"),
        CapsuleOutcome::Status {
            code: 500,
            message: String::new(),
            problem_type: None,
        },
    );
    assert!(
        refusal_reason(&intact).is_none(),
        "an intact capsule must not be refused"
    );
}

#[test]
fn replay_rejects_format_version_mismatch() {
    let mut recorded = capsule(
        request("GET", "/widgets"),
        CapsuleOutcome::Status {
            code: 500,
            message: String::new(),
            problem_type: None,
        },
    );
    recorded.format_version = CAPSULE_FORMAT_VERSION + 1;
    let json = serde_json::to_string(&recorded).expect("capsule serializes");

    let error = Capsule::from_json(&json).expect_err("a future format version must be rejected");
    assert!(matches!(error, CapsuleError::VersionMismatch { .. }));
    assert_eq!(
        print_refusal(
            &error.to_string(),
            std::path::Path::new("/tmp/capsule.json")
        ),
        2,
        "a cross-version capsule must exit 2, not be replayed"
    );
}

#[tokio::test]
async fn divergence_report_mentions_redaction_when_status_is_401() {
    let mut recorded = capsule(
        request("GET", "/denied"),
        CapsuleOutcome::Status {
            code: 500,
            message: "boom".to_owned(),
            problem_type: None,
        },
    );
    recorded.request.headers = vec![("authorization".to_owned(), "[FILTERED]".to_owned())];
    recorded.request.redacted_keys = vec!["header:authorization".to_owned()];

    let router = TestApp::new()
        .config(test_config())
        .routes(routes![denied])
        .build()
        .into_router();
    let outcome = execute(router, &recorded, Arc::new(DivergenceLog::new()), None).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Mismatch,
        "a 401 where a 500 was recorded is a mismatch: {outcome:?}"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("redact") || warning.contains("authenticated")),
        "the report must point at redaction as the likely cause, got {:?}",
        outcome.warnings
    );
    assert_eq!(
        print_verdict(&outcome, std::path::Path::new("/tmp/capsule.json")),
        1,
        "a mismatch exits 1"
    );
}

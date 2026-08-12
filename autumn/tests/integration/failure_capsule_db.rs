//! Integration tests for the failure-capsule **recording pool** (issue #1598).
//!
//! These exercise the wire-level tee: a `Pool<AsyncPgConnection>` whose
//! connections run through [`RecordingStream`], the `SET
//! autumn.capsule_request` marker `Db::checkout` sends, and the per-connection
//! memo that keeps a warm pooled connection replayable.
//!
//! Verifies that:
//!   * a failing request's capsule carries the rows the handler actually read;
//!   * the housekeeping marker statement is never itself recorded;
//!   * a *second* request on an already-warm pooled connection still gets the
//!     prepared-statement metadata its replay needs (the `ConnectionMemo`
//!     regression — F3, the load-bearing one);
//!   * connection birth (diesel-async's `SET TIME ZONE` / `SET
//!     CLIENT_ENCODING`) is kept as the tape's prologue;
//!   * a checkout with no capture scope *clears* the connection's binding, so
//!     unattributed work cannot leak into a live request's capsule (F2);
//!   * blowing the capsule byte budget marks the capsule truncated (F13);
//!   * a request that checks out a **shard** connection says so and marks its
//!     capsule truncated, because shard pools are not recorded in this slice;
//!   * a TLS-required database URL turns DB capture off with a note (F7).
//!
//! All but the last need a live `PostgreSQL`, so they are Docker-gated.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use autumn_web::AutumnError;
use autumn_web::capsule::{
    Capsule, CaptureScope, CaptureSettings, ConnectionTape, Exchange, ExchangeProtocol,
};
use autumn_web::config::{AutumnConfig, DatabaseConfig};
use autumn_web::db::Db;
use autumn_web::test::{TestApp, TestDb};
use autumn_web::{get, routes};
use axum::extract::{Query, State};
use diesel_async::RunQueryDsl as _;

// ── Fixtures ────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct Label {
    #[diesel(sql_type = diesel::sql_types::Text)]
    label: String,
}

#[derive(serde::Deserialize)]
struct TagParams {
    tag: Option<String>,
}

/// Reads one row through the pooled connection, then fails. The label travels
/// as a bind parameter so the capsule has binds to record.
#[get("/db-fail")]
async fn db_fail(mut db: Db, Query(params): Query<TagParams>) -> Result<&'static str, AutumnError> {
    let tag = params.tag.unwrap_or_else(|| "alpha".to_owned());
    let row: Label = diesel::sql_query("SELECT $1::text AS label")
        .bind::<diesel::sql_types::Text, _>(tag)
        .get_result(&mut db)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("query failed: {error}"))
        })?;
    Err(AutumnError::internal_server_error_msg(format!(
        "read {} then exploded",
        row.label
    )))
}

/// Reads a row, then hands a *scope-free* task the same single-slot pool, then
/// reads again — the F2 shape. The middle query must not reach this request's
/// capsule, because its checkout sends the clearing marker.
#[get("/db-fail-with-orphan")]
async fn db_fail_with_orphan(
    State(state): State<autumn_web::AppState>,
) -> Result<&'static str, AutumnError> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::internal_server_error_msg("no pool"))?
        .clone();

    read_label(&pool, "alpha").await?;

    // A detached task carries no `CAPSULE_SCOPE` task-local, so its checkout
    // must clear the connection's binding rather than inherit ours.
    let orphan_pool = pool.clone();
    tokio::spawn(async move { read_label(&orphan_pool, "orphan").await })
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(format!("join: {error}")))??;

    read_label(&pool, "beta").await?;

    Err(AutumnError::internal_server_error_msg("exploded"))
}

async fn read_label(
    pool: &diesel_async::pooled_connection::deadpool::Pool<autumn_web::db::RuntimeConnection>,
    tag: &str,
) -> Result<String, AutumnError> {
    let mut db = Db::connect_for_test(pool).await?;
    let row: Label = diesel::sql_query("SELECT $1::text AS label")
        .bind::<diesel::sql_types::Text, _>(tag)
        .get_result(&mut db)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("query failed: {error}"))
        })?;
    Ok(row.label)
}

// ── Harness ─────────────────────────────────────────────────────────────────

/// The Postgres URL the recording pool is built against.
async fn db_url() -> String {
    TestDb::shared().await.url().to_owned()
}

fn capture_config(dir: &Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.failure_capture.enabled = true;
    config.failure_capture.dir = dir.to_string_lossy().into_owned();
    config
}

fn capsule_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
}

async fn await_capsules(dir: &Path, expected: usize) -> Vec<PathBuf> {
    for _ in 0..200 {
        let paths = capsule_paths(dir);
        if paths.len() >= expected {
            return paths;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    capsule_paths(dir)
}

fn read_capsule(path: &Path) -> Capsule {
    let json = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("capsule at {} must be readable: {error}", path.display()));
    Capsule::from_json(&json)
        .unwrap_or_else(|error| panic!("capsule at {} must parse: {error}", path.display()))
}

/// The single connection tape a capsule recorded, or a readable failure.
fn only_tape(capsule: &Capsule) -> &ConnectionTape {
    let db = capsule
        .db
        .as_ref()
        .unwrap_or_else(|| panic!("capsule {} recorded no database traffic", capsule.id));
    match db.connections.as_slice() {
        [tape] => tape,
        other => panic!("expected exactly one connection tape, got {}", other.len()),
    }
}

/// All SQL a tape holds, across every bucket — used for "must never appear"
/// assertions.
fn all_sql(tape: &ConnectionTape) -> Vec<&str> {
    tape.prologue
        .iter()
        .chain(&tape.statements)
        .chain(&tape.catalog)
        .chain(&tape.exchanges)
        .map(|exchange| exchange.sql.as_str())
        .collect()
}

/// Whether an exchange's recorded backend frames contain a `RowDescription`.
fn has_row_description(exchange: &Exchange) -> bool {
    frame_tags(&exchange.response).contains(&b'T')
}

/// Tags of the backend frames in a recorded response blob.
fn frame_tags(response: &[u8]) -> Vec<u8> {
    let mut tags = Vec::new();
    let mut offset = 0usize;
    while let (Some(&tag), Some(len)) = (
        response.get(offset),
        response
            .get(offset + 1..offset + 5)
            .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
            .map(i32::from_be_bytes),
    ) {
        let Ok(len) = usize::try_from(len) else { break };
        tags.push(tag);
        offset += len + 1;
    }
    tags
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn capsule_records_db_query_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool =
        autumn_web::capsule::build_recording_pool(&db_url().await, 2, Duration::from_secs(10))
            .expect("recording pool builds");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![db_fail])
        .with_db(pool)
        .build();

    client
        .get("/db-fail?tag=alpha")
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    assert_eq!(
        paths.len(),
        1,
        "the failing DB request must leave a capsule"
    );
    let capsule = read_capsule(&paths[0]);
    assert!(
        !capsule.truncated,
        "a single small query must fit the default budget"
    );

    let tape = only_tape(&capsule);
    let select = tape
        .exchanges
        .iter()
        .find(|exchange| exchange.sql.contains("SELECT $1::text"))
        .unwrap_or_else(|| {
            panic!(
                "the handler's SELECT must be a recorded exchange; recorded: {:?}",
                all_sql(tape)
            )
        });

    assert_eq!(select.protocol, ExchangeProtocol::Extended);
    assert_eq!(
        select.row_count, 1,
        "the capsule must count the row the handler read"
    );
    assert!(
        !select.response.is_empty(),
        "the capsule must carry the raw backend frames"
    );
    assert!(
        frame_tags(&select.response).contains(&b'D'),
        "the recorded response must include the DataRow the handler consumed"
    );
    assert!(
        frame_tags(&select.response).last() == Some(&b'Z'),
        "an exchange ends at ReadyForQuery, got {:?}",
        frame_tags(&select.response)
    );
    assert_eq!(
        select.binds.len(),
        1,
        "the text bind must be recorded, got {:?}",
        select.binds
    );

    // Replay answers a `Bind` out of `statements`, and tokio-postgres rejects
    // a `Bind` whose arity disagrees with the `ParameterDescription` it was
    // given — client-side, before the stub ever sees it. So every
    // extended-protocol exchange needs its prepared-statement metadata
    // recorded alongside it, keyed by the identical SQL text.
    for exchange in &tape.exchanges {
        if exchange.protocol != ExchangeProtocol::Extended {
            continue;
        }
        let described = tape
            .statements
            .iter()
            .find(|statement| statement.sql == exchange.sql)
            .unwrap_or_else(|| {
                panic!(
                    "no prepared-statement metadata recorded for {:?}; replay cannot \
                     answer its Bind. statements held: {:?}",
                    exchange.sql,
                    tape.statements
                        .iter()
                        .map(|statement| statement.sql.as_str())
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            frame_tags(&described.response).last() == Some(&b'Z'),
            "prepared-statement metadata must run through ReadyForQuery, got {:?}",
            frame_tags(&described.response)
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn capsule_excludes_housekeeping_marker_exchange() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool =
        autumn_web::capsule::build_recording_pool(&db_url().await, 2, Duration::from_secs(10))
            .expect("recording pool builds");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![db_fail])
        .with_db(pool)
        .build();

    client.get("/db-fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    let tape = only_tape(&capsule);

    for sql in all_sql(tape) {
        assert!(
            !sql.contains("autumn.capsule_request"),
            "the attribution marker is Autumn's own housekeeping and must never be \
             recorded (replaying it would re-bind a connection); found {sql:?}"
        );
    }
}

/// The `ConnectionMemo` regression (F3). The second request reuses a warm
/// pooled connection whose prepared statement is already cached, so no `Parse`
/// crosses the wire — yet its capsule must still carry the statement metadata,
/// or replay would answer the second request's `Bind` with nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn second_request_on_a_pooled_connection_carries_statement_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    // One slot: the second request is guaranteed the same, already-warm
    // connection.
    let pool =
        autumn_web::capsule::build_recording_pool(&db_url().await, 1, Duration::from_secs(10))
            .expect("recording pool builds");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![db_fail])
        .with_db(pool)
        .build();

    client
        .get("/db-fail?tag=first")
        .send()
        .await
        .assert_status(500);
    await_capsules(dir.path(), 1).await;
    client
        .get("/db-fail?tag=second")
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 2).await;
    assert_eq!(paths.len(), 2, "each failing request leaves one capsule");

    // Capsules are named with a monotonic sequence, so the later path is the
    // second request's.
    let second = read_capsule(&paths[1]);
    let tape = only_tape(&second);

    let described = tape
        .statements
        .iter()
        .find(|exchange| exchange.sql.contains("SELECT $1::text"))
        .unwrap_or_else(|| {
            panic!(
                "the warm connection's prepared-statement metadata must be carried into \
                 every later capsule, or replay cannot answer the Bind; tape held: {:?}",
                all_sql(tape)
            )
        });
    assert!(
        has_row_description(described),
        "the remembered statement exchange must include the RowDescription the client \
         needs to decode rows"
    );

    // And the request's own work is still there.
    assert!(
        tape.exchanges
            .iter()
            .any(|exchange| exchange.sql.contains("SELECT $1::text")),
        "the second request's own SELECT must still be recorded as an exchange"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn capsule_records_connection_prologue() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool =
        autumn_web::capsule::build_recording_pool(&db_url().await, 1, Duration::from_secs(10))
            .expect("recording pool builds");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![db_fail])
        .with_db(pool)
        .build();

    client.get("/db-fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    let tape = only_tape(&capsule);

    assert!(
        !tape.prologue.is_empty(),
        "the connection's birth traffic must be kept so replay can warm the \
         connection the same way"
    );
    let prologue_sql: Vec<&str> = tape
        .prologue
        .iter()
        .map(|exchange| exchange.sql.as_str())
        .collect();
    assert!(
        prologue_sql
            .iter()
            .any(|sql| sql.contains("TIME ZONE") || sql.contains("CLIENT_ENCODING")),
        "diesel-async's own session setup belongs in the prologue, got {prologue_sql:?}"
    );
}

/// F2: a checkout that happens outside any capture scope must *clear* the
/// connection's binding, so its queries cannot be attributed to whichever
/// request last used that connection.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn marker_clears_previous_request_binding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool =
        autumn_web::capsule::build_recording_pool(&db_url().await, 1, Duration::from_secs(10))
            .expect("recording pool builds");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![db_fail_with_orphan])
        .with_db(pool)
        .build();

    client
        .get("/db-fail-with-orphan")
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    let tape = only_tape(&capsule);

    let bound_binds: Vec<String> = tape
        .exchanges
        .iter()
        .flat_map(|exchange| exchange.binds.iter())
        .filter_map(|bind| match bind {
            autumn_web::capsule::BindValue::Value(bytes) => {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
            _ => None,
        })
        .collect();

    assert!(
        bound_binds.iter().any(|value| value == "alpha"),
        "the request's own queries must be recorded, got {bound_binds:?}"
    );
    assert!(
        bound_binds.iter().any(|value| value == "beta"),
        "the request's post-orphan query must still be recorded, got {bound_binds:?}"
    );
    assert!(
        !bound_binds.iter().any(|value| value == "orphan"),
        "a checkout with no capture scope must clear the connection binding; the \
         detached task's query leaked into this request's capsule: {bound_binds:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn exceeding_max_capsule_bytes_marks_truncated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    // Far below one prologue exchange: recording must stop and say so rather
    // than write a capsule that looks complete.
    config.failure_capture.max_capsule_bytes = 16;

    let pool =
        autumn_web::capsule::build_recording_pool(&db_url().await, 1, Duration::from_secs(10))
            .expect("recording pool builds");

    let client = TestApp::new()
        .config(config)
        .routes(routes![db_fail])
        .with_db(pool)
        .build();

    client.get("/db-fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    assert!(
        capsule.truncated,
        "blowing max_capsule_bytes must mark the capsule truncated so replay refuses it"
    );
}

/// Slice-one honesty: `[[database.shards]]` pools are **not** built through the
/// recording factory (only the control topology is), so a request that reaches
/// for a shard connection generates database traffic no capsule can hold. Such
/// a capsule must say so and be marked truncated, so `autumn replay` refuses it
/// instead of presenting a tape that silently omits the shard's effects.
///
/// Needs no Docker: deadpool builds shard pools lazily, so the checkout is
/// attempted (which is what flags the scope) and then fails against a URL
/// nothing is listening on.
#[tokio::test]
async fn a_shard_checkout_marks_the_capsule_truncated_with_a_note() {
    use autumn_web::AppState;
    use autumn_web::capsule::CAPSULE_SCOPE;
    use autumn_web::config::ShardConfig;
    use autumn_web::sharding::{HashShardRouter, ShardKeyOverride, ShardedDb, create_shard_set};
    use axum::extract::FromRequestParts as _;

    let database = DatabaseConfig {
        // Keep the doomed checkout fast: nothing listens on this URL.
        connect_timeout_secs: 1,
        shards: vec![ShardConfig {
            name: "alpha".to_owned(),
            primary_url: "postgres://127.0.0.1:1/alpha".to_owned(),
            slots: None,
            replica_url: None,
            primary_pool_size: None,
            replica_pool_size: None,
            replica_fallback: None,
        }],
        ..Default::default()
    };
    let shards = create_shard_set(&database, Arc::new(HashShardRouter))
        .expect("lazy shard pools build without a server")
        .expect("shards are configured");
    let state = AppState::for_test().with_shards(shards);
    state.insert_extension(AutumnConfig::default());

    let scope = Arc::new(CaptureScope::new(
        "shard-gap".to_owned(),
        Arc::new(CaptureSettings::default()),
        Arc::new(autumn_web::log::filter::ParameterFilter::new(&[], &[])),
    ));

    CAPSULE_SCOPE
        .scope(Arc::clone(&scope), async {
            let (mut parts, ()) = axum::http::Request::builder()
                .uri("/orders")
                .body(())
                .expect("request builds")
                .into_parts();
            parts
                .extensions
                .insert(ShardKeyOverride("tenant-1".to_owned()));
            // The checkout cannot succeed (no server); flagging the scope must
            // not depend on it succeeding.
            let _ = ShardedDb::from_request_parts(&mut parts, &state).await;
        })
        .await;

    assert!(
        scope
            .notes()
            .iter()
            .any(|note| note.to_ascii_lowercase().contains("shard")),
        "a capsule for a shard-using request must explain the missing shard traffic, \
         got {:?}",
        scope.notes()
    );
    assert!(
        scope.is_truncated(),
        "a capsule whose request used an unrecorded shard pool must be truncated so \
         replay refuses it rather than presenting it as complete"
    );
}

/// F7: capture cannot tee a TLS connection, so a TLS-required database URL
/// turns DB capture off — an ordinary pool boots, and capsules say why they
/// have no database traffic. No database is contacted (deadpool connects
/// lazily), so this needs no Docker.
#[tokio::test]
async fn tls_database_url_disables_db_capture_with_note() {
    let reason = autumn_web::capsule::record_db::capture_unavailable_reason(
        "postgres://user:pw@db.example.com:5432/app?sslmode=require",
    )
    .expect("a TLS-required URL must not be recordable");
    assert!(
        reason.to_ascii_lowercase().contains("tls"),
        "the reason must name TLS so an operator can act on it, got {reason:?}"
    );
    assert!(
        autumn_web::capsule::record_db::capture_unavailable_reason(
            "postgres://user:pw@db.example.com:5432/app"
        )
        .is_none(),
        "a plaintext TCP URL is recordable"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let config = capture_config(dir.path());
    let database = DatabaseConfig {
        primary_url: Some("postgres://user:pw@127.0.0.1:1/app?sslmode=require".to_owned()),
        ..Default::default()
    };

    let provider = autumn_web::capsule::record_db::maybe_capture_pool_provider(None, &config)
        .expect("capture is enabled, so the provider is installed");
    let topology = provider(database)
        .await
        .expect("a TLS URL must still boot an ordinary pool, not fail the app");
    assert!(
        topology.is_some(),
        "the app must still get a database pool when capture steps aside"
    );

    let scope = CaptureScope::new(
        "tls-note".to_owned(),
        Arc::new(CaptureSettings::default()),
        Arc::new(autumn_web::log::filter::ParameterFilter::new(&[], &[])),
    );
    autumn_web::capsule::record_db::note_db_capture_unavailable(&scope);
    assert!(
        scope
            .notes()
            .iter()
            .any(|note| note.to_ascii_lowercase().contains("tls")),
        "capsules recorded on a TLS-database app must explain the missing DB tape, \
         got {:?}",
        scope.notes()
    );
}

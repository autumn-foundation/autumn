//! The red-team sweep for `#[confidential]` (#1771).
//!
//! The test stores and reads a confidential field over real HTTP, then dumps
//! every operator-reachable sink and greps it for the seeded plaintext marker:
//!
//! 1. **the database** — the `SQLite` file on disk, read as raw bytes;
//! 2. **the `db backup` artifact** — produced with `VACUUM INTO`, the statement
//!    `autumn db backup` runs for a `SQLite` target
//!    (`autumn-cli/src/db/sqlite_snapshot.rs`). `autumn-cli`'s own
//!    `a_backup_artifact_holds_no_confidential_plaintext` runs the command;
//! 3. **the full access and error log** — every `tracing` event, at every target
//!    and level, captured while the requests run;
//! 4. **a replay capsule** — the JSON file the failure-capture layer writes.
//!
//! The test asserts zero plaintext in all four sinks, and that the owning
//! session reads the value back.

#![cfg(feature = "db")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use autumn_web::confidential::{BlindIndex, FieldContext, RootKey, Sealed};
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{get, post, routes};
use serde::{Deserialize, Serialize};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

/// The seeded marker. Distinctive enough that one occurrence anywhere is a leak.
const MARKER: &str = "AUTUMN-REDTEAM-MARKER-lab-result-positive-7f3a";
const OWNER: &str = "user-42";
const TABLE: &str = "redteam_notes";

diesel::table! {
    redteam_notes (id) {
        id -> Integer,
        uid -> Text,
        owner_id -> Text,
        sealed_body -> Text,
        sealed_body_bidx -> Text,
    }
}

#[autumn_web::model(table = "redteam_notes")]
pub struct RedTeamNote {
    pub id: i32,
    pub uid: String,
    pub owner_id: String,
    #[confidential(blind_index)]
    pub sealed_body: Sealed,
    pub sealed_body_bidx: BlindIndex,
}

fn ctx(owner: &str, uid: &str) -> FieldContext {
    FieldContext::for_record(TABLE, "sealed_body", owner, uid)
}

// ── The database the handlers write to ──────────────────────────────────────

/// One `SQLite` file for this module, so the sweep can read the same bytes the
/// handlers wrote. The connection is behind a `Mutex` because the handlers are
/// async and Diesel's `SqliteConnection` is not `Sync`.
struct RedTeamDb {
    _dir: tempfile::TempDir,
    path: PathBuf,
    conn: Mutex<diesel::SqliteConnection>,
}

fn db() -> &'static RedTeamDb {
    static DB: OnceLock<RedTeamDb> = OnceLock::new();
    DB.get_or_init(|| {
        use diesel::Connection as _;
        use diesel::connection::SimpleConnection as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("redteam.sqlite3");
        let mut conn =
            diesel::SqliteConnection::establish(path.to_str().expect("utf-8 path")).expect("open");
        conn.batch_execute(
            "CREATE TABLE redteam_notes (id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, \
             owner_id TEXT NOT NULL, sealed_body TEXT NOT NULL, sealed_body_bidx TEXT NOT NULL)",
        )
        .expect("create table");
        RedTeamDb {
            _dir: dir,
            path,
            conn: Mutex::new(conn),
        }
    })
}

// ── The application under test ──────────────────────────────────────────────

/// What the client sends and reads back. Both fields are opaque strings the
/// client produced.
#[derive(Serialize, Deserialize)]
struct NotePayload {
    uid: String,
    sealed_body: Sealed,
    sealed_body_bidx: BlindIndex,
}

/// Establishes the session that owns the notes.
#[get("/login/{owner}")]
async fn log_in(
    session: autumn_web::session::Session,
    autumn_web::extract::Path(owner): autumn_web::extract::Path<String>,
) -> &'static str {
    session.insert("owner_id", owner).await;
    "ok"
}

/// Stores a sealed note for the authenticated session.
#[post("/notes")]
async fn store_note(
    session: autumn_web::session::Session,
    axum::Json(payload): axum::Json<NotePayload>,
) -> Result<&'static str, autumn_web::AutumnError> {
    use diesel::prelude::*;

    let owner: String = session
        .get("owner_id")
        .await
        .ok_or_else(|| autumn_web::AutumnError::unauthorized_msg("not signed in"))?;

    // The lock is scoped to the statement that needs it: a guard held across the
    // response build would serialize requests that are not touching the database.
    {
        let mut conn = db().conn.lock().expect("db lock");
        diesel::insert_into(redteam_notes::table)
            .values(NewRedTeamNote {
                uid: payload.uid,
                owner_id: owner,
                sealed_body: payload.sealed_body,
                sealed_body_bidx: payload.sealed_body_bidx,
            })
            .execute(&mut *conn)
            .map_err(|e| autumn_web::AutumnError::internal_server_error_msg(e.to_string()))?;
    }
    Ok("stored")
}

/// Returns the sealed note to the session that owns it, and to nobody else.
#[get("/notes/{uid}")]
async fn read_note(
    session: autumn_web::session::Session,
    autumn_web::extract::Path(uid): autumn_web::extract::Path<String>,
) -> Result<axum::Json<NotePayload>, autumn_web::AutumnError> {
    use diesel::prelude::*;

    let owner: String = session
        .get("owner_id")
        .await
        .ok_or_else(|| autumn_web::AutumnError::unauthorized_msg("not signed in"))?;

    let note: RedTeamNote = {
        let mut conn = db().conn.lock().expect("db lock");
        redteam_notes::table
            .filter(redteam_notes::uid.eq(&uid))
            .filter(redteam_notes::owner_id.eq(&owner))
            .first(&mut *conn)
            .map_err(|_| autumn_web::AutumnError::not_found_msg("no such note"))?
    };

    Ok(axum::Json(NotePayload {
        uid: note.uid,
        sealed_body: note.sealed_body,
        sealed_body_bidx: note.sealed_body_bidx,
    }))
}

/// Fails with the payload quoted into the error message — the shape that drags a
/// request body into the error log and into a capsule.
#[post("/notes/fail")]
async fn store_note_failing(body: String) -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(format!(
        "could not store {body}"
    )))
}

// ── Log capture ─────────────────────────────────────────────────────────────

/// A `tracing` writer that appends every formatted event to a shared buffer.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log lock")).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Capture every event, at every target and level: the access log, the error
/// log, SQL tracing and anything else the requests emit.
fn install_log_capture() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(buffer.clone())
            .with_filter(tracing_subscriber::filter::LevelFilter::TRACE),
    );
    (buffer, tracing::subscriber::set_default(subscriber))
}

// ── Sink helpers ────────────────────────────────────────────────────────────

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
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

async fn await_capsules(dir: &Path) -> Vec<PathBuf> {
    for _ in 0..100 {
        let found = capsule_paths(dir);
        if !found.is_empty() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    capsule_paths(dir)
}

/// Assert that `haystack` holds no occurrence of `needle`.
fn assert_blind(sink: &str, needle: &str, haystack: &[u8]) {
    let hit = haystack
        .windows(needle.len())
        .position(|w| w == needle.as_bytes());
    assert!(hit.is_none(), "{sink} leaked the value at byte {hit:?}");
}

/// Write the `db backup` artifact. `autumn db backup` runs this statement for a
/// `SQLite` target.
fn backup_artifact(into: &Path) {
    use diesel::RunQueryDsl as _;
    let mut conn = db().conn.lock().expect("db lock");
    diesel::sql_query("VACUUM INTO ?")
        .bind::<diesel::sql_types::Text, _>(into.to_str().expect("utf-8 path"))
        .execute(&mut *conn)
        .expect("VACUUM INTO");
}

// ── The sweep ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_operator_reachable_sink_holds_the_confidential_plaintext() {
    let dir = tempfile::tempdir().expect("temp dir");
    let capsules = dir.path().join("capsules");
    std::fs::create_dir_all(&capsules).expect("capsule dir");

    // The key exists only here, standing in for the client. Nothing hands it to
    // the server, and it has no serialized form that could.
    let key = RootKey::generate();
    let uid = "note-sweep";
    let sealed = key.seal(&ctx(OWNER, uid), MARKER).expect("seal");
    let token = key.blind_index(&ctx(OWNER, uid), MARKER);

    let (log, _guard) = install_log_capture();
    let client = TestApp::new()
        .config(capture_config(&capsules))
        .routes(routes![log_in, store_note, read_note, store_note_failing])
        .build();

    client
        .get(&format!("/login/{OWNER}"))
        .send()
        .await
        .assert_ok();

    // Store, through a handler that writes the row to the database.
    let payload = NotePayload {
        uid: uid.to_owned(),
        sealed_body: sealed.clone(),
        sealed_body_bidx: token.clone(),
    };
    let body = serde_json::to_string(&payload).expect("serialize");
    client
        .post("/notes")
        .json(&serde_json::to_value(&payload).expect("to value"))
        .send()
        .await
        .assert_ok();

    // Return: the owning authenticated session reads the row back and opens it.
    let response = client.get(&format!("/notes/{uid}")).send().await;
    response.assert_ok();
    let read_back: NotePayload = response.json();
    assert_eq!(
        key.unseal(&ctx(OWNER, uid), &read_back.sealed_body)
            .expect("unseal"),
        MARKER,
        "the owning session recovers the plaintext it stored"
    );
    assert_eq!(read_back.sealed_body_bidx, token);

    // A failing request, so a capsule is written with the body in it.
    client
        .post("/notes/fail")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .assert_status(500);
    let capsule_files = await_capsules(&capsules).await;
    assert!(!capsule_files.is_empty(), "expected a replay capsule");

    let backup = dir.path().join("backup.sqlite3");
    backup_artifact(&backup);

    // ── The verdict ─────────────────────────────────────────────────────────
    let db_bytes = std::fs::read(&db().path).expect("read db");
    let backup_bytes = std::fs::read(&backup).expect("read backup");
    assert_blind("the database", MARKER, &db_bytes);
    assert_blind("the `db backup` artifact", MARKER, &backup_bytes);
    assert_blind(
        "the access and error log",
        MARKER,
        log.contents().as_bytes(),
    );
    for path in &capsule_files {
        assert_blind(
            &format!("the replay capsule {}", path.display()),
            MARKER,
            &std::fs::read(path).expect("read capsule"),
        );
    }

    // The sinks are not empty: this proves the sweep read real output.
    assert!(
        db_bytes
            .windows(sealed.as_envelope().len())
            .any(|w| w == sealed.as_envelope().as_bytes()),
        "the database must hold the sealed column"
    );
    assert!(!backup_bytes.is_empty(), "the backup must hold the dump");
    assert!(
        log.contents().contains("/notes/fail"),
        "the log must hold the requests the sweep made"
    );
    let capsule_text = std::fs::read_to_string(&capsule_files[0]).expect("read capsule");
    assert!(
        capsule_text.contains(sealed.as_envelope()) || capsule_text.contains("[FILTERED]"),
        "the capsule must hold the request body, sealed or filtered: {capsule_text}"
    );
}

/// A note belongs to the session that stored it, and to nobody else.
#[tokio::test]
async fn another_session_cannot_read_the_note() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = RootKey::generate();
    let uid = "note-owned";

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![log_in, store_note, read_note, store_note_failing])
        .build();

    client
        .get(&format!("/login/{OWNER}"))
        .send()
        .await
        .assert_ok();
    let payload = NotePayload {
        uid: uid.to_owned(),
        sealed_body: key.seal(&ctx(OWNER, uid), MARKER).expect("seal"),
        sealed_body_bidx: key.blind_index(&ctx(OWNER, uid), MARKER),
    };
    client
        .post("/notes")
        .json(&serde_json::to_value(&payload).expect("to value"))
        .send()
        .await
        .assert_ok();

    // A different session sees a 404, not the envelope.
    client.log_out();
    client.get("/login/user-7").send().await.assert_ok();
    client
        .get(&format!("/notes/{uid}"))
        .send()
        .await
        .assert_status(404);
}

/// The root key is what makes the whole scheme work, so it gets its own sweep.
#[tokio::test]
async fn the_root_key_reaches_no_sink() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bytes = [0x5au8; 32];
    let hex_key = hex::encode(bytes);
    let key = RootKey::from_bytes(bytes);
    let uid = "note-key-sweep";

    let (log, _guard) = install_log_capture();
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![log_in, store_note, read_note, store_note_failing])
        .build();

    client
        .get(&format!("/login/{OWNER}"))
        .send()
        .await
        .assert_ok();
    let payload = NotePayload {
        uid: uid.to_owned(),
        sealed_body: key.seal(&ctx(OWNER, uid), MARKER).expect("seal"),
        sealed_body_bidx: key.blind_index(&ctx(OWNER, uid), MARKER),
    };
    client
        .post("/notes")
        .json(&serde_json::to_value(&payload).expect("to value"))
        .send()
        .await
        .assert_ok();

    // The key never left this test, so nothing the server wrote can hold it.
    let rendered = format!(
        "{key:?} {:?} {:?}",
        payload.sealed_body, payload.sealed_body_bidx
    );
    assert_blind("Debug output", &hex_key, rendered.as_bytes());
    assert_blind("the log", &hex_key, log.contents().as_bytes());
    assert_blind(
        "the wire payload",
        &hex_key,
        serde_json::to_string(&payload)
            .expect("serialize")
            .as_bytes(),
    );
    assert_blind(
        "the database",
        &hex_key,
        &std::fs::read(&db().path).expect("read db"),
    );
}

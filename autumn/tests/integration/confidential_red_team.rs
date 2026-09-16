//! The red-team sweep for `#[confidential]` (#1771).
//!
//! One test exercises confidential-field CRUD, then dumps every
//! operator-reachable sink and greps it for the seeded plaintext marker:
//!
//! 1. **the database** — the `SQLite` file on disk, read as raw bytes;
//! 2. **the `db backup` artifact** — produced with `VACUUM INTO`, the statement
//!    `autumn db backup` itself runs for a `SQLite` target
//!    (`autumn-cli/src/db/sqlite_snapshot.rs`);
//! 3. **the full access and error log** — every `tracing` event at every target
//!    and level, captured while the request runs;
//! 4. **a replay capsule** — the JSON file the failure-capture layer writes.
//!
//! The success metric is the assertion: zero plaintext occurrences in all four,
//! while the owning session reads the value back correctly.

#![cfg(feature = "db")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::confidential::{BlindIndex, FieldContext, RootKey, Sealed};
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{post, routes};
use serde::{Deserialize, Serialize};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

/// The seeded marker. Distinctive enough that a single occurrence anywhere is
/// proof of a leak.
const MARKER: &str = "AUTUMN-REDTEAM-MARKER-lab-result-positive-7f3a";
const OWNER: &str = "user-42";
const TABLE: &str = "redteam_notes";

diesel::table! {
    redteam_notes (id) {
        id -> Integer,
        owner_id -> Text,
        body -> Text,
        body_bidx -> Text,
    }
}

#[autumn_web::model(table = "redteam_notes")]
pub struct RedTeamNote {
    pub id: i32,
    pub owner_id: String,
    #[confidential(blind_index)]
    pub body: Sealed,
    pub body_bidx: BlindIndex,
}

fn ctx() -> FieldContext {
    FieldContext::new(TABLE, "body", OWNER)
}

// ── The application under test ──────────────────────────────────────────────

/// What the client sends. Both fields are opaque strings the client produced.
#[derive(Serialize, Deserialize)]
struct NotePayload {
    owner_id: String,
    body: Sealed,
    body_bidx: BlindIndex,
}

/// Stores the note and hands it straight back to the owning session.
#[post("/notes")]
async fn store_note(axum::Json(payload): axum::Json<NotePayload>) -> axum::Json<NotePayload> {
    axum::Json(payload)
}

/// Fails with the payload quoted into the error message — the shape that drags
/// a request body into the error log and into a capsule.
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
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
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
/// log, SQL tracing and anything else the request emits.
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

/// Assert that `haystack` holds no occurrence of the marker.
fn assert_blind(sink: &str, haystack: &[u8]) {
    let hit = haystack
        .windows(MARKER.len())
        .position(|w| w == MARKER.as_bytes());
    assert!(
        hit.is_none(),
        "{sink} leaked the confidential plaintext at byte {:?}",
        hit.unwrap_or_default()
    );
}

/// Write the row through the model, then return the database path, the backup
/// artifact path, and the plaintext the owner reads back.
fn exercise_database(dir: &Path, key: &RootKey) -> (PathBuf, PathBuf, String) {
    use diesel::connection::SimpleConnection as _;
    use diesel::prelude::*;

    let db_path = dir.join("redteam.sqlite3");
    let mut conn = diesel::SqliteConnection::establish(db_path.to_str().unwrap()).unwrap();
    conn.batch_execute(
        "CREATE TABLE redteam_notes (id INTEGER PRIMARY KEY, owner_id TEXT NOT NULL, \
         body TEXT NOT NULL, body_bidx TEXT NOT NULL)",
    )
    .unwrap();

    // Create.
    diesel::insert_into(redteam_notes::table)
        .values(NewRedTeamNote {
            owner_id: OWNER.to_owned(),
            body: key.seal(&ctx(), MARKER).unwrap(),
            body_bidx: key.blind_index(&ctx(), MARKER),
        })
        .execute(&mut conn)
        .unwrap();

    // Read, through the blind index — the only server-side predicate there is.
    let token = key.blind_index(&ctx(), MARKER);
    let note: RedTeamNote = redteam_notes::table
        .filter(redteam_notes::body_bidx.eq(&token))
        .first(&mut conn)
        .unwrap();
    let recovered = key.unseal(&ctx(), &note.body).unwrap();

    // Update, then delete a second row: the whole CRUD surface touches the sinks.
    diesel::update(redteam_notes::table.filter(redteam_notes::id.eq(note.id)))
        .set(redteam_notes::body.eq(key.seal(&ctx(), MARKER).unwrap()))
        .execute(&mut conn)
        .unwrap();
    diesel::insert_into(redteam_notes::table)
        .values(NewRedTeamNote {
            owner_id: OWNER.to_owned(),
            body: key.seal(&ctx(), MARKER).unwrap(),
            body_bidx: key.blind_index(&ctx(), MARKER),
        })
        .execute(&mut conn)
        .unwrap();
    diesel::delete(redteam_notes::table.filter(redteam_notes::id.ne(note.id)))
        .execute(&mut conn)
        .unwrap();

    // `autumn db backup` runs exactly this statement for a SQLite target.
    let backup_path = dir.join("backup.sqlite3");
    diesel::sql_query("VACUUM INTO ?")
        .bind::<diesel::sql_types::Text, _>(backup_path.to_str().unwrap())
        .execute(&mut conn)
        .unwrap();

    (db_path, backup_path, recovered)
}

// ── The sweep ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_operator_reachable_sink_holds_the_confidential_plaintext() {
    let dir = tempfile::tempdir().expect("temp dir");
    let capsules = dir.path().join("capsules");
    std::fs::create_dir_all(&capsules).unwrap();

    // The key exists only here, standing in for the client. Nothing hands it to
    // the server, and it has no serialized form that could.
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), MARKER).expect("seal");
    let token = key.blind_index(&ctx(), MARKER);

    // 1 + 2. Database and backup artifact.
    let (db_path, backup_path, recovered) = exercise_database(dir.path(), &key);
    assert_eq!(recovered, MARKER, "the owning session reads its value back");

    // 3 + 4. Full log and replay capsule, over real HTTP requests.
    let (log, _guard) = install_log_capture();
    let client = TestApp::new()
        .config(capture_config(&capsules))
        .routes(routes![store_note, store_note_failing])
        .build();

    let payload = NotePayload {
        owner_id: OWNER.to_owned(),
        body: sealed.clone(),
        body_bidx: token.clone(),
    };
    let body = serde_json::to_string(&payload).expect("serialize");

    // The round trip: the server returns the sealed value to its owner (AC3).
    let response = client
        .post("/notes")
        .json(&serde_json::to_value(&payload).expect("to value"))
        .send()
        .await;
    response.assert_ok();
    let echoed: NotePayload = response.json();
    assert_eq!(
        key.unseal(&ctx(), &echoed.body).expect("unseal"),
        MARKER,
        "the owning session recovers the plaintext from the round trip"
    );
    assert_eq!(echoed.body_bidx, token);

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

    // ── The verdict ─────────────────────────────────────────────────────────
    assert_blind("the database", &std::fs::read(&db_path).expect("read db"));
    assert_blind(
        "the `db backup` artifact",
        &std::fs::read(&backup_path).expect("read backup"),
    );
    assert_blind("the access and error log", log.contents().as_bytes());
    for path in &capsule_files {
        assert_blind(
            &format!("the replay capsule {}", path.display()),
            &std::fs::read(path).expect("read capsule"),
        );
    }

    // The sinks are not empty: this proves the sweep looked at real output.
    assert!(
        std::fs::metadata(&backup_path).unwrap().len() > 0,
        "the backup artifact must hold the dumped database"
    );
    assert!(
        log.contents().contains("/notes/fail"),
        "the log must hold the requests the sweep made"
    );
    let capsule_text = std::fs::read_to_string(&capsule_files[0]).unwrap();
    assert!(
        capsule_text.contains(sealed.as_envelope()) || capsule_text.contains("[FILTERED]"),
        "the capsule must hold the request body, sealed or filtered: {capsule_text}"
    );
}

/// The root key is what makes the whole scheme work, so it gets its own sweep.
#[tokio::test]
async fn the_root_key_reaches_no_sink() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bytes = [0x5au8; 32];
    let hex_key = hex::encode(bytes);
    let key = RootKey::from_bytes(bytes);

    let (log, _guard) = install_log_capture();
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![store_note, store_note_failing])
        .build();

    let payload = NotePayload {
        owner_id: OWNER.to_owned(),
        body: key.seal(&ctx(), MARKER).expect("seal"),
        body_bidx: key.blind_index(&ctx(), MARKER),
    };
    client
        .post("/notes")
        .json(&serde_json::to_value(&payload).expect("to value"))
        .send()
        .await
        .assert_ok();

    // The key never left this test, so nothing the server wrote can hold it.
    let rendered = format!("{key:?} {:?} {:?}", payload.body, payload.body_bidx);
    assert!(!rendered.contains(&hex_key), "Debug output: {rendered}");
    assert!(
        !log.contents().contains(&hex_key),
        "the log must not hold the root key"
    );
    assert!(
        !serde_json::to_string(&payload).unwrap().contains(&hex_key),
        "the wire payload must not hold the root key"
    );
}

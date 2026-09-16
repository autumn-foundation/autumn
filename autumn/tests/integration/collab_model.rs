//! `#[collaborative]` end-to-end over a real `#[model]` + `#[repository]`
//! (issue #1806).
//!
//! The non-ignored tests prove the generated surface exists and that the merge
//! behaves through it, with no database. The `#[ignore]`d test needs Docker
//! (testcontainers) and proves the *persistence* half: a document survives a
//! round trip, and two concurrent edits merged through the column lose no
//! character.

#![cfg(all(feature = "db", feature = "collab"))]

use autumn_web::collab::{CollabText, collaborative_columns_for_table};
use autumn_web::hooks::Patch;
use autumn_web::repository::ReadRoute;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    test_collab_notes (id) {
        id -> Int8,
        body -> Text,
        scratch -> Text,
        title -> Text,
    }
}

/// Two collaborative fields alongside a plain one: opting in is per field.
#[autumn_web::model(table = "test_collab_notes")]
pub struct Note {
    #[id]
    pub id: i64,
    #[collaborative]
    pub body: CollabText,
    #[collaborative]
    pub scratch: CollabText,
    pub title: String,
}

#[autumn_web::repository(Note, table = "test_collab_notes")]
pub trait NoteRepository {
    /// A plain (non-collaborative) equality lookup still works untouched.
    fn find_by_title(title: String) -> Vec<Note>;
}

fn seeded() -> Note {
    Note {
        id: 1,
        body: CollabText::from_text("seed", "hello world"),
        scratch: CollabText::new(),
        title: "greeting".to_owned(),
    }
}

// ── Generated surface (no database) ──────────────────────────────────────────

/// AC1: the marker registers the column so a surface with no compile-time view
/// of the model can find it.
#[test]
fn collaborative_columns_are_registered() {
    assert_eq!(Note::__AUTUMN_COLLABORATIVE_COLUMNS, &["body", "scratch"]);
    assert_eq!(Note::collaborative_fields(), &["body", "scratch"]);
    let mut registered = collaborative_columns_for_table("test_collab_notes");
    registered.sort_unstable();
    assert_eq!(registered, vec!["body", "scratch"]);
}

/// The field-name-keyed accessors answer for a collaborative field and stay
/// quiet for anything else — never a panic.
#[test]
fn the_field_keyed_accessors_resolve_documents() {
    let mut note = seeded();
    assert_eq!(
        note.collaborative("body").map(CollabText::text).as_deref(),
        Some("hello world")
    );
    assert!(note.collaborative("title").is_none());
    assert!(note.collaborative("nope").is_none());

    note.collaborative_mut("scratch")
        .expect("scratch is collaborative")
        .insert("a", 0, "todo");
    assert_eq!(note.scratch_text(), "todo");
    assert!(note.collaborative_mut("title").is_none());
}

/// AC1: per-field, character-level merge through the generated helpers.
#[test]
fn generated_helpers_merge_concurrent_edits_character_by_character() {
    let mut ada = seeded();
    let mut linus = seeded();

    let from_ada = ada.body_insert("ada", 5, ",");
    let from_linus = linus.body_insert("linus", 11, "!");

    for op in from_linus {
        ada.body.apply(op);
    }
    for op in from_ada {
        linus.body.apply(op);
    }

    assert_eq!(ada.body_text(), "hello, world!");
    assert_eq!(linus.body_text(), ada.body_text());
}

/// A whole-field rewrite is still an edit, not a clobber: the other editor's
/// concurrent insertion outside the changed span survives.
#[test]
fn set_text_does_not_clobber_a_concurrent_edit() {
    let mut ada = seeded();
    let mut linus = seeded();

    let from_ada = ada.body_set_text("ada", "hello cruel world");
    let from_linus = linus.body_insert("linus", 11, "!");

    for op in from_linus {
        ada.body.apply(op);
    }
    for op in from_ada {
        linus.body.apply(op);
    }
    assert_eq!(ada.body_text(), "hello cruel world!");
    assert_eq!(linus.body_text(), ada.body_text());
}

/// Deleting through the generated helper converges with a concurrent insert.
#[test]
fn generated_remove_converges_with_a_concurrent_insert() {
    let mut ada = seeded();
    let mut linus = seeded();

    let from_ada = ada.body_remove(0, 6); // "world"
    let from_linus = linus.body_insert("linus", 11, "!");

    for op in from_linus {
        ada.body.apply(op);
    }
    for op in from_ada {
        linus.body.apply(op);
    }
    assert_eq!(ada.body_text(), "world!");
    assert_eq!(linus.body_text(), ada.body_text());

    // `_merge` reaches the same state from either direction.
    let mut merged = seeded();
    merged.body_merge(&ada.body);
    assert_eq!(merged.body_text(), "world!");
}

/// The model's JSON form carries the document, not just the rendered text, so
/// a version-history replay cannot silently drop concurrent edits.
#[test]
fn the_model_serializes_the_whole_document() {
    let json = serde_json::to_value(seeded()).expect("serialize note");
    assert!(
        json["body"]["elems"].is_array(),
        "the column carries the document: {json}"
    );
    assert_eq!(json["title"], "greeting");
}

// ── Persistence (requires Docker) ────────────────────────────────────────────

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default().start().await.expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let mut conn = pool.get().await.expect("conn");
    // The storage a collaborative column needs: `TEXT NOT NULL` with the
    // empty-document default, which is what the declarative lane emits for a
    // `#[collaborative]` field.
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_collab_notes (\
         id BIGSERIAL PRIMARY KEY, \
         body TEXT NOT NULL DEFAULT '{\"elems\":[]}', \
         scratch TEXT NOT NULL DEFAULT '{\"elems\":[]}', \
         title TEXT NOT NULL UNIQUE)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_collab_notes");
    (pool, container)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgNoteRepository {
    PgNoteRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

/// The round trip: a document survives the column, and two edits made against
/// the same loaded record both land after a merge — the loss last-write-wins
/// would have caused.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn documents_round_trip_and_merge_without_losing_characters() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let saved = repo
        .save(&NewNote {
            body: CollabText::from_text("seed", "hello world"),
            scratch: CollabText::new(),
            title: "greeting".to_owned(),
        })
        .await
        .expect("insert note");

    let loaded = repo
        .find_by_id(saved.id)
        .await
        .expect("query")
        .expect("note exists");
    assert_eq!(loaded.body_text(), "hello world");
    assert_eq!(loaded.scratch_text(), "");

    // Two editors load the same record and edit different spans.
    let mut ada = loaded.clone();
    let mut linus = loaded;
    ada.body_insert("ada", 5, ",");
    linus.body_insert("linus", 11, "!");

    // Both write back; the second write merges rather than overwriting.
    repo.update(
        ada.id,
        &UpdateNote {
            body: Patch::Set(ada.body.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("first write");
    let current = repo
        .find_by_id(ada.id)
        .await
        .expect("query")
        .expect("note exists");
    let mut merged = linus;
    merged.body_merge(&current.body);
    repo.update(
        merged.id,
        &UpdateNote {
            body: Patch::Set(merged.body.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("second write");

    let final_note = repo
        .find_by_id(merged.id)
        .await
        .expect("query")
        .expect("note exists");
    assert_eq!(
        final_note.body_text(),
        "hello, world!",
        "both edits survive the round trip"
    );
}

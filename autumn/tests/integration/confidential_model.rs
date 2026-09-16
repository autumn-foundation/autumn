//! A `#[confidential]` column over a real `SQLite` database (#1771).
//!
//! Proves the model-layer half: the column persists as the envelope the client
//! produced, the owning client reads its plaintext back, equality works through
//! the blind-index companion column, and the registry sees the column.
//!
//! NB: sync Diesel imports are kept *function-local* so the sync `RunQueryDsl`
//! never enters the module scope where `#[model]` expands its async query code.

#![cfg(feature = "db")]

use autumn_web::confidential::{self, BlindIndex, FieldContext, RootKey, Sealed};

diesel::table! {
    sealed_notes (id) {
        id -> Integer,
        owner_id -> Text,
        title -> Text,
        body -> Text,
        body_bidx -> Text,
    }
}

#[autumn_web::model(table = "sealed_notes")]
pub struct SealedNote {
    pub id: i32,
    pub owner_id: String,
    pub title: String,
    #[confidential(blind_index)]
    pub body: Sealed,
    pub body_bidx: BlindIndex,
}

const PLAINTEXT: &str = "AUTUMN-CONFIDENTIAL-MARKER-biopsy-scheduled";
const OWNER: &str = "user-42";

fn ctx() -> FieldContext {
    FieldContext::new("sealed_notes", "body", OWNER)
}

fn conn() -> diesel::SqliteConnection {
    use diesel::connection::SimpleConnection as _;
    use diesel::prelude::*;
    let mut c = SqliteConnection::establish(":memory:").unwrap();
    c.batch_execute(
        "CREATE TABLE sealed_notes (id INTEGER PRIMARY KEY, owner_id TEXT NOT NULL, \
         title TEXT NOT NULL, body TEXT NOT NULL, body_bidx TEXT NOT NULL)",
    )
    .unwrap();
    c
}

fn insert(c: &mut diesel::SqliteConnection, key: &RootKey, plaintext: &str, title: &str) {
    use diesel::prelude::*;
    diesel::insert_into(sealed_notes::table)
        .values(NewSealedNote {
            owner_id: OWNER.to_owned(),
            title: title.to_owned(),
            body: key.seal(&ctx(), plaintext).unwrap(),
            body_bidx: key.blind_index(&ctx(), plaintext),
        })
        .execute(c)
        .unwrap();
}

#[test]
fn the_column_stores_the_envelope_and_the_owner_reads_the_plaintext_back() {
    use diesel::prelude::*;
    let key = RootKey::generate();
    let mut c = conn();
    insert(&mut c, &key, PLAINTEXT, "checkup");

    // Raw on-disk value: selecting into `String` bypasses the wrapper.
    let raw: String = sealed_notes::table
        .select(sealed_notes::body)
        .first(&mut c)
        .unwrap();
    assert!(!raw.contains(PLAINTEXT), "the column must hold ciphertext");

    // The model reads back the envelope, and only the client opens it.
    let note: SealedNote = sealed_notes::table.first(&mut c).unwrap();
    assert_eq!(note.body.as_envelope(), raw);
    assert_eq!(key.unseal(&ctx(), &note.body).unwrap(), PLAINTEXT);
}

#[test]
fn a_model_debug_line_shows_no_envelope() {
    use diesel::prelude::*;
    let key = RootKey::generate();
    let mut c = conn();
    insert(&mut c, &key, PLAINTEXT, "checkup");
    let note: SealedNote = sealed_notes::table.first(&mut c).unwrap();

    let rendered = format!("{note:?}");
    assert!(!rendered.contains(PLAINTEXT), "{rendered}");
    assert!(
        !rendered.contains(note.body.as_envelope()),
        "Debug must not print the envelope: {rendered}"
    );
    assert!(rendered.contains("<sealed>"), "{rendered}");
}

#[test]
fn equality_works_through_the_blind_index_companion_column() {
    use diesel::prelude::*;
    let key = RootKey::generate();
    let mut c = conn();
    insert(&mut c, &key, PLAINTEXT, "checkup");
    insert(&mut c, &key, "something else entirely", "other");

    let token = key.blind_index(&ctx(), PLAINTEXT);
    let hits: Vec<SealedNote> = sealed_notes::table
        .filter(sealed_notes::body_bidx.eq(&token))
        .load(&mut c)
        .unwrap();

    assert_eq!(hits.len(), 1, "the token selects exactly its own row");
    assert_eq!(hits[0].title, "checkup");
    assert_eq!(key.unseal(&ctx(), &hits[0].body).unwrap(), PLAINTEXT);
}

#[test]
fn sealing_is_randomized_so_two_rows_of_one_value_differ_on_disk() {
    use diesel::prelude::*;
    let key = RootKey::generate();
    let mut c = conn();
    insert(&mut c, &key, PLAINTEXT, "first");
    insert(&mut c, &key, PLAINTEXT, "second");

    let envelopes: Vec<String> = sealed_notes::table
        .select(sealed_notes::body)
        .load(&mut c)
        .unwrap();
    assert_ne!(
        envelopes[0], envelopes[1],
        "equal plaintexts must not produce equal ciphertext"
    );

    // The blind index is the part that *is* stable, which is why it exists.
    let tokens: Vec<String> = sealed_notes::table
        .select(sealed_notes::body_bidx)
        .load(&mut c)
        .unwrap();
    assert_eq!(tokens[0], tokens[1]);
}

#[test]
fn a_row_written_by_another_owner_does_not_open() {
    use diesel::prelude::*;
    let key = RootKey::generate();
    let mut c = conn();
    insert(&mut c, &key, PLAINTEXT, "checkup");
    let note: SealedNote = sealed_notes::table.first(&mut c).unwrap();

    // An operator who copies the envelope into another user's row produces a
    // value that key cannot open, because the owner is authenticated data.
    let moved = FieldContext::new("sealed_notes", "body", "user-7");
    assert!(key.unseal(&moved, &note.body).is_err());
}

#[test]
fn the_column_is_registered_for_composition() {
    assert_eq!(SealedNote::__AUTUMN_CONFIDENTIAL_COLUMNS, &["body"]);
    assert!(confidential::is_confidential_column("sealed_notes", "body"));
    assert!(!confidential::is_confidential_column(
        "sealed_notes",
        "title"
    ));
    assert!(confidential::is_confidential_column_name("body"));

    let names = confidential::registered_confidential_column_names();
    assert!(names.contains(&"body".to_owned()), "{names:?}");
    assert!(
        names.contains(&"body_bidx".to_owned()),
        "the token column is filtered too: {names:?}"
    );

    let descriptor = confidential::registered_confidential_columns()
        .into_iter()
        .find(|d| d.table == "sealed_notes" && d.column == "body")
        .expect("registered");
    assert_eq!(descriptor.model, "SealedNote");
    assert_eq!(descriptor.blind_index, Some("body_bidx"));
}

#[test]
fn version_history_treats_a_confidential_column_as_sensitive() {
    let mut cols: Vec<&'static str> = vec!["title"];
    confidential::merge_confidential_columns_for_table("sealed_notes", &mut cols);
    assert!(cols.contains(&"body"), "{cols:?}");
}

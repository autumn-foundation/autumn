//! Snag QA probe (#1771): the `#[repository]`-generated `find_by_<field>_bidx`
//! finder is the *only* sanctioned way to query a `#[confidential]` column —
//! `docs/guide/confidential-fields.md`'s "Equality lookups" section and
//! `tests/compile-fail/confidential_find_by.rs` both point developers at it.
//! `tests/compile-pass/confidential_blind_index_finder.rs` proves it compiles,
//! but nothing in the existing suite ever *runs* it against real data:
//! `confidential_model.rs` queries the blind-index column with a raw Diesel
//! `.filter(...)`, never through the generated repository method itself.
//!
//! This drives the actual generated finder over a real (`SQLite`, tempfile)
//! pool with many owners, to check the one property the whole feature exists
//! for: a blind-index lookup returns *exactly* the calling owner's row, never
//! another owner's, even when two owners store the identical plaintext.

#![cfg(feature = "sqlite")]

use autumn_web::confidential::{BlindIndex, FieldContext, RootKey, Sealed};
use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::repository::ReadRoute;
use diesel_async::pooled_connection::deadpool::Pool;

diesel::table! {
    bidx_notes (id) {
        id -> BigInt,
        owner_id -> Text,
        body -> Text,
        body_bidx -> Text,
    }
}

#[autumn_web::model(table = "bidx_notes")]
pub struct BidxNote {
    #[id]
    pub id: i64,
    pub owner_id: String,
    #[confidential(blind_index)]
    pub body: Sealed,
    pub body_bidx: BlindIndex,
}

#[autumn_web::repository(BidxNote, table = "bidx_notes")]
pub trait BidxNoteRepository {
    async fn find_by_body_bidx(&self, body_bidx: &BlindIndex) -> Vec<BidxNote>;
}

type SqlitePool = Pool<RuntimeConnection>;

fn build_pool(tmp: &tempfile::TempDir) -> SqlitePool {
    let db_path = tmp.path().join("bidx_notes.db");
    let url = format!("sqlite://{}", db_path.display());
    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured")
}

const fn build_repo(pool: SqlitePool) -> PgBidxNoteRepository {
    PgBidxNoteRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

/// Two "clusters" of owners share one of two plaintexts, so a same-value
/// collision across owners is actually exercised (not just distinct values).
const N_OWNERS: usize = 200;

async fn create_table(pool: &SqlitePool) {
    use diesel_async::SimpleAsyncConnection;
    let mut conn = pool.get().await.expect("checkout");
    conn.batch_execute(
        "CREATE TABLE bidx_notes (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            owner_id TEXT NOT NULL, \
            body TEXT NOT NULL, \
            body_bidx TEXT NOT NULL\
        )",
    )
    .await
    .expect("create bidx_notes");
}

/// The whole point of the blind index: N owners, some sharing plaintexts, and
/// a lookup by owner A's own token must return owner A's row and nothing else
/// — not another owner's row holding the same plaintext under a different key.
#[tokio::test]
async fn a_blind_index_lookup_never_crosses_owners() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let pool = build_pool(&tmp);
    create_table(&pool).await;
    let repo = build_repo(pool);

    let shared_a = "AUTUMN-MARKER-shared-diagnosis-A";
    let shared_b = "AUTUMN-MARKER-shared-diagnosis-B";

    let mut owners: Vec<(String, RootKey, String)> = Vec::new(); // (owner_id, key, plaintext)
    for i in 0..N_OWNERS {
        let owner_id = format!("owner-{i}");
        let key = RootKey::generate();
        let plaintext = if i % 2 == 0 { shared_a } else { shared_b }.to_owned();
        owners.push((owner_id, key, plaintext));
    }

    for (owner_id, key, plaintext) in &owners {
        let ctx = FieldContext::new("bidx_notes", "body", owner_id);
        let new_note = NewBidxNote {
            owner_id: owner_id.clone(),
            body: key.seal(&ctx, plaintext).expect("seal"),
            body_bidx: key.blind_index(&ctx, plaintext),
        };
        repo.save(&new_note).await.expect("save note");
    }

    // Every owner's own token, through the GENERATED finder, must return
    // exactly one row: their own.
    for (owner_id, key, plaintext) in &owners {
        let ctx = FieldContext::new("bidx_notes", "body", owner_id);
        let token = key.blind_index(&ctx, plaintext);
        let hits = repo.find_by_body_bidx(&token).await.expect("query");
        assert_eq!(
            hits.len(),
            1,
            "owner {owner_id}'s token must match exactly one row, got {}",
            hits.len()
        );
        assert_eq!(&hits[0].owner_id, owner_id, "must be the owner's own row");
        assert_eq!(
            key.unseal(&ctx, &hits[0].body).expect("unseal own row"),
            *plaintext
        );
    }

    // Cross-check: an owner's token must never appear as a hit for a DIFFERENT
    // owner sharing the same plaintext (the property that makes the feature
    // safe to call "per owner" at all).
    let (owner_a_id, owner_a_key, plaintext_a) = &owners[0];
    let ctx_a = FieldContext::new("bidx_notes", "body", owner_a_id);
    let token_a = owner_a_key.blind_index(&ctx_a, plaintext_a);
    let (other_owner_id, other_owner_key, other_plaintext) = &owners[2]; // also shared_a
    assert_eq!(plaintext_a, other_plaintext, "sanity: both hold shared_a");
    let ctx_other = FieldContext::new("bidx_notes", "body", other_owner_id);
    let token_other = other_owner_key.blind_index(&ctx_other, other_plaintext);
    assert_ne!(
        token_a, token_other,
        "two owners holding the identical plaintext must get DIFFERENT tokens"
    );
    let hits_for_a = repo.find_by_body_bidx(&token_a).await.expect("query");
    assert!(
        hits_for_a.iter().all(|n| n.owner_id == *owner_a_id),
        "owner A's token must never return owner C's row, even with equal plaintext: {:?}",
        hits_for_a.iter().map(|n| &n.owner_id).collect::<Vec<_>>()
    );
}

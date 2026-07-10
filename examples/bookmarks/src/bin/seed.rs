//! Seed binary for the bookmarks example.
//!
//! Two ways to populate the database:
//!
//! 1. **One-line faked seed** (the default `autumn seed` body below): inserts a
//!    batch of realistic fake bookmarks with a single
//!    `Bookmark::factory().fake().create_many(...)` call.
//!
//! 2. **On-demand fake seeding without editing this file**:
//!
//!    ```text
//!    autumn seed --count 200 --model Bookmark
//!    ```
//!
//!    `autumn seed` forwards `--count`/`--model` as `AUTUMN_SEED_COUNT` /
//!    `AUTUMN_SEED_MODEL`, and the dispatcher routes to
//!    `autumn_web::seed::fake_seed_model`, which drives the model's factory.
//!    Any `#[autumn_web::model]` registers automatically.
//!
//! This example crate is a binary (no `lib` target), so — like the factory
//! integration tests — the `Bookmark` model and its schema are declared inline
//! here rather than imported from `src/models`.

use autumn_web::seed::SeedContext;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

// ── Inline schema (mirrors src/schema.rs) ────────────────────────────────────

diesel::table! {
    bookmarks (id) {
        id -> Int8,
        url -> Text,
        title -> Text,
        tag -> Text,
        alive -> Bool,
        created_at -> Timestamp,
    }
}

// ── Model defined with #[model] to expose the generated `.fake()` factory ────

#[autumn_web::model]
pub struct Bookmark {
    #[id]
    pub id: i64,
    #[validate(url)]
    pub url: String,
    #[validate(length(min = 1, max = 200))]
    pub title: String,
    pub tag: String,
    #[default]
    pub alive: bool,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::main]
async fn main() {
    let ctx =
        SeedContext::build().expect("failed to build seed context — is the database running?");

    // `autumn seed --count N --model Bookmark` dispatch: generate N faked rows
    // for the named model and return, skipping the default body below.
    if let (Ok(model), Ok(count)) = (
        std::env::var("AUTUMN_SEED_MODEL"),
        std::env::var("AUTUMN_SEED_COUNT"),
    ) {
        let count: usize = count
            .parse()
            .expect("AUTUMN_SEED_COUNT must be a non-negative integer");
        let inserted = autumn_web::seed::fake_seed_model(&model, count, ctx.pool())
            .await
            .expect("fake-seed failed");
        println!("Inserted {inserted} faked `{model}` row(s).");
        return;
    }

    println!("Seeding database (profile: {})...", ctx.profile());

    // Idempotent guard: only bulk-seed when the table is empty, so re-running
    // `autumn seed` doesn't keep piling up rows.
    let mut db = ctx
        .conn()
        .await
        .expect("failed to acquire database connection");
    let existing: i64 = bookmarks::table
        .count()
        .get_result(&mut *db)
        .await
        .expect("failed to count existing bookmarks");
    drop(db);

    if existing > 0 {
        println!("Bookmarks already seeded ({existing} rows); nothing to do.");
        return;
    }

    // One-line faked seed: populate 200 realistic bookmarks to exercise
    // pagination and search.
    let rows = Bookmark::factory()
        .fake()
        .create_many(200, ctx.pool())
        .await;
    println!("Seed complete: inserted {} faked bookmarks.", rows.len());
}

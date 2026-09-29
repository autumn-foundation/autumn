//! Basic `#[repository]` CRUD proof on the `SQLite` runtime backend (issue
//! #1996, wave B+C).
//!
//! The scope-threading proof (`sqlite_scoped_repository.rs`) deliberately does
//! **not** instantiate a full `#[repository]`, because the default CRUD the
//! macro generates — pessimistic `FOR UPDATE` locking, an `ON CONFLICT … DO
//! UPDATE` upsert set-clause bounded to `QueryFragment<Pg>`, and a Postgres
//! multi-row batch insert (`BatchInsert<…, false>` / the `DEFAULT` keyword) —
//! used Postgres-only diesel query fragments that could not compile against the
//! `SQLite` backend at all.
//!
//! This test pins that those always-emitted constructs are now backend-portable:
//! it declares a **simple** model + repository (unscoped, unversioned, not
//! searchable, no hooks), boots an in-memory `SQLite` pool through the real
//! [`autumn_web::db::create_pool`] path, and exercises the generated CRUD end to
//! end — `save` (single-row insert + `RETURNING`), `find_by_id`, `update`
//! (read-modify-write, i.e. the `FOR UPDATE` → `maybe_for_update!` seam),
//! `count`, `save_many` (the cfg-dual batch-insert path), and `delete_by_id`.
//! Running to green means the whole generated CRUD surface type-checked **and**
//! executed on `SQLite` with no Docker.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_crud_basic`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        crud_notes (id) {
            id -> Int8,
            content -> Text,
            views -> Int8,
        }
    }
}

use schema::crud_notes;

#[autumn_web::model]
pub struct CrudNote {
    #[id]
    pub id: i64,
    pub content: String,
    pub views: i64,
}

#[autumn_web::repository(CrudNote)]
pub trait CrudNoteRepository {}

async fn boot_pool() -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection). The
    // pool's per-connection setup already turns on `foreign_keys`/`busy_timeout`.
    let config = DatabaseConfig {
        url: Some("sqlite://file:crud_basic?mode=memory&cache=shared".to_string()),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query(
            "CREATE TABLE crud_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 content TEXT NOT NULL, \
                 views BIGINT NOT NULL DEFAULT 0\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create crud_notes table");
    }

    pool
}

#[tokio::test]
async fn generated_crud_compiles_and_runs_on_sqlite() {
    let pool = boot_pool().await;
    let repo = PgCrudNoteRepository::with_pool_untracked(pool);

    // insert (single-row) + RETURNING
    let created = repo
        .save(&NewCrudNote {
            content: "first".to_string(),
            views: 0,
        })
        .await
        .expect("save inserts a row and returns it");
    assert_eq!(created.content, "first");
    assert!(created.id > 0, "autoincrement id assigned");

    // find_by_id round-trips the inserted row
    let found = repo
        .find_by_id(created.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(found.content, "first");
    assert_eq!(found.views, 0);

    // update — the read-modify-write path (the FOR UPDATE → maybe_for_update! seam)
    let updated = repo
        .update(
            created.id,
            &UpdateCrudNote {
                content: autumn_web::hooks::Patch::Set("first-edited".to_string()),
                views: autumn_web::hooks::Patch::Set(7),
            },
        )
        .await
        .expect("update mutates and returns the row");
    assert_eq!(updated.content, "first-edited");
    assert_eq!(updated.views, 7);

    // save_many — the cfg-dual batch-insert path (2 rows)
    let many = repo
        .save_many(&[
            NewCrudNote {
                content: "second".to_string(),
                views: 1,
            },
            NewCrudNote {
                content: "third".to_string(),
                views: 2,
            },
        ])
        .await
        .expect("save_many inserts a batch and returns the rows");
    assert_eq!(many.len(), 2);
    let mut contents: Vec<&str> = many.iter().map(|m| m.content.as_str()).collect();
    contents.sort_unstable();
    assert_eq!(contents, ["second", "third"]);

    // count — 3 rows total (1 from save + 2 from save_many)
    let total = repo.count().await.expect("count query");
    assert_eq!(total, 3);

    // delete_by_id removes one row
    repo.delete_by_id(created.id)
        .await
        .expect("delete_by_id removes the row");
    assert!(
        repo.find_by_id(created.id)
            .await
            .expect("find_by_id after delete")
            .is_none(),
        "deleted row is gone"
    );
    assert_eq!(repo.count().await.expect("count after delete"), 2);
}

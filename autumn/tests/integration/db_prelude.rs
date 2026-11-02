//! `autumn_web::db::prelude::*` sits next to `#[model]` and `#[repository]`
//! in the default (Postgres) build. `diesel::prelude::*` here gives E0034: it
//! also imports the blocking `RunQueryDsl`. The `SQLite` build has the same
//! check in `tests/sqlite_fleet.rs`.

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use autumn_web::db::prelude::*;

diesel::table! {
    db_prelude_notes (id) {
        id -> Int8,
        body -> Text,
    }
}

#[autumn_web::model(table = "db_prelude_notes")]
pub struct PreludeNote {
    #[id]
    pub id: i64,
    pub body: String,
}

#[autumn_web::repository(PreludeNote, table = "db_prelude_notes")]
pub trait PreludeNoteRepository {}

/// An async query with the prelude's `RunQueryDsl`.
pub async fn count(conn: &mut autumn_web::db::RuntimeConnection) -> QueryResult<i64> {
    db_prelude_notes::table.count().get_result(conn).await
}

#[test]
fn the_db_prelude_names_the_async_run_query_dsl() {
    // The module compiles: no E0034 from the macros. The prelude's
    // `RunQueryDsl` is the async trait.
    fn is_async_dsl<T: diesel_async::RunQueryDsl<autumn_web::db::RuntimeConnection>>(_: &T) {}
    is_async_dsl(&db_prelude_notes::table.count());
    let _ = count;
}

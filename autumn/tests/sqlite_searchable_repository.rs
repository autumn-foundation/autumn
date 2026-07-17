//! `#[repository(searchable)]` full-text search on the `SQLite` runtime backend
//! via **FTS5** (issue #1910).
//!
//! Postgres searchable repositories rank rows with a `tsvector` +
//! `websearch_to_tsquery`/`ts_rank_cd` query. `SQLite` has no `tsvector`, so the
//! generated codegen forks (via `backend_select!`): the `SQLite` arm queries an
//! **external-content FTS5 virtual table** (`"<table>__fts"`, tokenized with
//! `unicode61`) joined back to the base table (so the tenant / soft-delete /
//! owner predicates still filter the base table), `MATCH`-filtered and ranked
//! with `bm25()` (per-column weights from the `#[searchable(weight=...)]`
//! priorities). The user's query is run through the fail-closed
//! `sqlite_fts5_match_query` sanitizer so no FTS5 operator survives as syntax.
//! This suite drives that path end-to-end on an in-memory `SQLite` database (no
//! Docker):
//!
//! * token matches are found case-insensitively and non-matches excluded;
//! * case-folding is **full-Unicode** now (via `unicode61`): `äpfel` matches
//!   `Äpfel` (the old ASCII-only `lower()` limitation is gone) — the pinned
//!   behavioral flip of #1910;
//! * FTS5 query operators (`OR`, `col:`, `*`, …) are neutralized into literal
//!   terms by the sanitizer, and an all-operator query returns an empty page
//!   rather than everything (fail-closed);
//! * `bm25()` ranking sorts a higher-priority-field (title) hit above a
//!   lower-priority-field (body) hit;
//! * `search_page` returns the right page + total;
//! * **ADVERSARIAL cross-tenant isolation:** a search run under tenant B never
//!   returns tenant A's rows even when the term matches — including via an FTS5
//!   injection attempt (fail-closed tenant isolation is inviolable);
//! * the owner-scoped `search_page_scoped` path (#1841) never returns another
//!   owner's rows.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_searchable_repository`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::pagination::PageRequest;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::tenancy::with_tenant;

use diesel_async::RunQueryDsl as _;
use diesel_async::SimpleAsyncConnection as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

/// The FTS5 DDL the `AddSearch` migration emits for a `(title, body)` searchable
/// model: an external-content virtual table + the three maintenance triggers the
/// generated `MATCH`/`bm25()` query relies on. Hand-authored here (the runtime
/// suite hand-creates its tables rather than running the CLI migration) so it
/// mirrors `autumn-cli`'s `add_search_up_sql_for(Sqlite, ...)` exactly. No
/// `'rebuild'` backfill is needed — every table is created empty, then populated
/// through `repo.save`, which fires the `AFTER INSERT` trigger.
fn fts5_ddl(table: &str) -> String {
    format!(
        "CREATE VIRTUAL TABLE \"{table}__fts\" USING fts5(\"title\", \"body\", \
             content='{table}', content_rowid='id', tokenize='unicode61'); \
         CREATE TRIGGER \"{table}__fts_ai\" AFTER INSERT ON \"{table}\" BEGIN \
             INSERT INTO \"{table}__fts\"(rowid, \"title\", \"body\") \
                 VALUES (new.id, new.\"title\", new.\"body\"); \
         END; \
         CREATE TRIGGER \"{table}__fts_ad\" AFTER DELETE ON \"{table}\" BEGIN \
             INSERT INTO \"{table}__fts\"(\"{table}__fts\", rowid, \"title\", \"body\") \
                 VALUES('delete', old.id, old.\"title\", old.\"body\"); \
         END; \
         CREATE TRIGGER \"{table}__fts_au\" AFTER UPDATE ON \"{table}\" BEGIN \
             INSERT INTO \"{table}__fts\"(\"{table}__fts\", rowid, \"title\", \"body\") \
                 VALUES('delete', old.id, old.\"title\", old.\"body\"); \
             INSERT INTO \"{table}__fts\"(rowid, \"title\", \"body\") \
                 VALUES (new.id, new.\"title\", new.\"body\"); \
         END;"
    )
}

mod schema {
    autumn_web::reexports::diesel::table! {
        search_notes (id) {
            id -> Int8,
            title -> Text,
            body -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        tenant_search_notes (id) {
            id -> Int8,
            title -> Text,
            body -> Text,
            tenant_id -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        owner_search_notes (id) {
            id -> Int8,
            title -> Text,
            body -> Text,
            user_id -> Int8,
        }
    }
}

use schema::{owner_search_notes, search_notes, tenant_search_notes};

// Plain searchable repository (no tenant, no owner).
#[autumn_web::model(table = "search_notes")]
#[searchable(language = "english")]
pub struct SearchNote {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
}

#[autumn_web::repository(SearchNote, table = "search_notes", searchable)]
pub trait SearchNoteRepository {}

// Tenant-scoped searchable repository — for the cross-tenant isolation proof.
#[autumn_web::model(table = "tenant_search_notes")]
#[searchable(language = "english")]
pub struct TenantSearchNote {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(
    TenantSearchNote,
    table = "tenant_search_notes",
    tenant_scoped,
    searchable
)]
pub trait TenantSearchNoteRepository {}

// Owner-scoped searchable repository — for the `search_page_scoped` proof.
#[autumn_web::model(table = "owner_search_notes")]
#[searchable(language = "english")]
pub struct OwnerSearchNote {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
    pub user_id: i64,
}

#[autumn_web::repository(OwnerSearchNote, table = "owner_search_notes", owner = user_id, searchable)]
pub trait OwnerSearchNoteRepository {}

async fn boot_pool(db_name: &str) -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection).
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        // Base tables, plus the external-content FTS5 virtual table + triggers the
        // generated `MATCH`/`bm25()` query joins against (the FTS objects live
        // outside the model/schema surface, mirroring Postgres's `search_vector`).
        diesel::sql_query(
            "CREATE TABLE search_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 title TEXT NOT NULL, \
                 body TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create search_notes table");
        conn.batch_execute(&fts5_ddl("search_notes"))
            .await
            .expect("create search_notes FTS5 objects");
        diesel::sql_query(
            "CREATE TABLE tenant_search_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 title TEXT NOT NULL, \
                 body TEXT NOT NULL, \
                 tenant_id TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create tenant_search_notes table");
        conn.batch_execute(&fts5_ddl("tenant_search_notes"))
            .await
            .expect("create tenant_search_notes FTS5 objects");
        diesel::sql_query(
            "CREATE TABLE owner_search_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 title TEXT NOT NULL, \
                 body TEXT NOT NULL, \
                 user_id BIGINT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create owner_search_notes table");
        conn.batch_execute(&fts5_ddl("owner_search_notes"))
            .await
            .expect("create owner_search_notes FTS5 objects");
    }

    pool
}

/// Case-insensitive substring matching over SEARCH_FIELDS: matches are found
/// (title OR body), case is ignored, and non-matching rows are excluded.
#[tokio::test]
async fn search_matches_substrings_case_insensitively_on_sqlite() {
    let pool = boot_pool("search_basic").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    repo.save(&NewSearchNote {
        title: "Rust Programming".to_string(),
        body: "systems language".to_string(),
    })
    .await
    .expect("save note 1");
    repo.save(&NewSearchNote {
        title: "Cooking Basics".to_string(),
        body: "a guide to RUST-free pans".to_string(),
    })
    .await
    .expect("save note 2");
    repo.save(&NewSearchNote {
        title: "Gardening".to_string(),
        body: "how to grow tomatoes".to_string(),
    })
    .await
    .expect("save note 3");

    // Case-insensitive: lowercase query "rust" matches "Rust ..." (title) and
    // "... RUST-free ..." (body), but never the gardening row.
    let hits = repo.search("rust").await.expect("search rust");
    assert_eq!(
        hits.len(),
        2,
        "both rust rows match, gardening excluded: {hits:?}"
    );
    assert!(
        hits.iter()
            .all(|n| n.title.to_lowercase().contains("rust")
                || n.body.to_lowercase().contains("rust")),
        "every hit must actually contain the term: {hits:?}"
    );

    // Uppercase query still matches (case folded on both sides).
    let hits_upper = repo
        .search("PROGRAMMING")
        .await
        .expect("search PROGRAMMING");
    assert_eq!(hits_upper.len(), 1, "one programming row: {hits_upper:?}");
    assert_eq!(hits_upper[0].title, "Rust Programming");

    // A term matching nothing returns empty.
    let none = repo.search("kubernetes").await.expect("search kubernetes");
    assert!(none.is_empty(), "no rows match kubernetes: {none:?}");

    // Blank query short-circuits to empty (shared guard, both backends).
    let blank = repo.search("   ").await.expect("blank search");
    assert!(blank.is_empty(), "blank query returns nothing: {blank:?}");
}

/// FTS5 case-folding is **full-Unicode** now (issue #1910): the `unicode61`
/// tokenizer folds case (and diacritics) across the whole Unicode range, so the
/// lowercase query `äpfel` matches the stored `Äpfel` — the exact case the old
/// ASCII-only `lower()` fallback could NOT match (the pinned regression, now
/// flipped). This test pins the flip so the code and the documented behavior
/// agree.
#[tokio::test]
async fn search_case_folding_is_full_unicode_on_sqlite() {
    let pool = boot_pool("search_unicode_fold").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    repo.save(&NewSearchNote {
        title: "Äpfel Tart".to_string(),
        body: "a German apple dessert".to_string(),
    })
    .await
    .expect("save accented row");

    // ── ASCII case-folding still works ──
    let ascii = repo.search("APPLE").await.expect("search APPLE");
    assert_eq!(
        ascii.len(),
        1,
        "ASCII term matches case-insensitively: {ascii:?}"
    );

    // ── The #1910 flip: full-Unicode case-folding ──
    // The lowercase non-ASCII query "äpfel" (U+00E4) now MATCHES the stored
    // "Äpfel" (U+00C4) — `unicode61` folds the non-ASCII letter's case, so the
    // whole-token match succeeds where the old ASCII-only `lower()` fallback
    // produced two distinct codepoints and never matched (the old F4 limitation).
    let folded = repo.search("äpfel").await.expect("search äpfel");
    assert_eq!(
        folded.len(),
        1,
        "full-Unicode folding: lowercase 'äpfel' matches stored 'Äpfel' (#1910 flip): {folded:?}"
    );

    // And the reverse direction folds too (uppercase query vs stored title).
    let folded_upper = repo.search("ÄPFEL").await.expect("search ÄPFEL");
    assert_eq!(
        folded_upper.len(),
        1,
        "full-Unicode folding is symmetric: 'ÄPFEL' matches stored 'Äpfel': {folded_upper:?}"
    );
}

/// FTS5 query operators in the user's input are neutralized into literal terms
/// by the fail-closed `sqlite_fts5_match_query` sanitizer (issue #1910), so a
/// query can never inject FTS5 syntax, trigger a syntax error, or become a
/// match-everything wildcard.
#[tokio::test]
async fn search_sanitizes_fts5_operators_on_sqlite() {
    let pool = boot_pool("search_fts_ops").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    repo.save(&NewSearchNote {
        title: "Alpha".to_string(),
        body: "first note".to_string(),
    })
    .await
    .expect("save alpha row");
    repo.save(&NewSearchNote {
        title: "Beta".to_string(),
        body: "second note".to_string(),
    })
    .await
    .expect("save beta row");

    // `OR` is a literal term, not the FTS5 OR operator. Raw FTS5 `alpha OR beta`
    // would match BOTH rows; sanitized it is the implicit-AND of three literal
    // phrases "alpha" AND "or" AND "beta", which no single row contains → empty.
    let ored = repo
        .search("alpha OR beta")
        .await
        .expect("search alpha OR beta");
    assert!(
        ored.is_empty(),
        "`OR` must be a literal, not an operator (no row has all of alpha/or/beta): {ored:?}"
    );

    // A bare `alpha` still matches its one row (the operator neutralization does
    // not break ordinary search).
    let alpha = repo.search("alpha").await.expect("search alpha");
    assert_eq!(
        alpha.len(),
        1,
        "plain term still matches its row: {alpha:?}"
    );
    assert_eq!(alpha[0].title, "Alpha");

    // A `col:` column-filter operator is neutralized: "title:alpha" becomes the
    // literal phrase, whose tokens ("title","alpha") are AND-ed — no row contains
    // the token "title", so it matches nothing (never a targeted column search).
    let colon = repo
        .search("title:alpha")
        .await
        .expect("search title:alpha");
    assert!(
        colon.is_empty(),
        "`col:` must be neutralized to a literal, matching nothing here: {colon:?}"
    );

    // A `*` prefix operator is neutralized: "alph*" is a literal token "alph"
    // (star stripped by the tokenizer), which is NOT the full token "alpha", so
    // no prefix expansion happens and it matches nothing.
    let prefix = repo.search("alph*").await.expect("search alph*");
    assert!(
        prefix.is_empty(),
        "`*` prefix must be neutralized (no prefix expansion): {prefix:?}"
    );

    // FAIL-CLOSED: an all-operator query returns an empty page, never everything.
    let all_ops = repo
        .search("OR NEAR *")
        .await
        .expect("search all-operators");
    assert!(
        all_ops.is_empty(),
        "an all-operator query must return empty, not every row: {all_ops:?}"
    );

    // FAIL-CLOSED: a whitespace-only query yields no tokens → empty result with
    // NO FTS query run (the sanitizer returns None).
    let blank = repo.search("   ").await.expect("blank search");
    assert!(
        blank.is_empty(),
        "whitespace-only query returns nothing: {blank:?}"
    );
}

/// `search_page` returns the right page slice and total on `SQLite`.
#[tokio::test]
async fn search_page_paginates_on_sqlite() {
    let pool = boot_pool("search_page").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    for i in 0..3 {
        repo.save(&NewSearchNote {
            title: format!("Widget {i}"),
            body: "a fine widget".to_string(),
        })
        .await
        .expect("save widget");
    }
    // A row that does not match the term.
    repo.save(&NewSearchNote {
        title: "Gadget".to_string(),
        body: "not a match".to_string(),
    })
    .await
    .expect("save gadget");

    let req = PageRequest::new(1, 2);
    let page = repo
        .search_page("widget", &req)
        .await
        .expect("search_page widget");
    assert_eq!(
        page.total_elements, 3,
        "three widgets match, gadget excluded"
    );
    assert_eq!(page.content.len(), 2, "first page holds two rows");
    // id DESC ordering: newest widgets first.
    assert!(
        page.content[0].id > page.content[1].id,
        "results are ordered id DESC: {:?}",
        page.content
    );

    let req2 = PageRequest::new(2, 2);
    let page2 = repo
        .search_page("widget", &req2)
        .await
        .expect("search_page page 2");
    assert_eq!(
        page2.content.len(),
        1,
        "second page holds the remaining row"
    );
    assert_eq!(page2.total_elements, 3);
}

/// bm25 ranking sanity (issue #1910): a hit in a higher-priority field (title,
/// `#[searchable(weight = "A")]`) must rank above a hit in a lower-priority field
/// (body, `weight = "B"`). The per-column bm25 weights are derived from those
/// priorities, so the title match gets a more-negative (better) score and sorts
/// first — parity with the Postgres `setweight('A' > 'B')` / `ts_rank_cd` intent.
#[tokio::test]
async fn search_ranks_title_above_body_on_sqlite() {
    let pool = boot_pool("search_bm25_rank").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    // A body-only match, saved FIRST (higher id would otherwise win the id-DESC
    // tiebreak — proving the ordering is genuinely by rank, not insertion order).
    repo.save(&NewSearchNote {
        title: "Gardening basics".to_string(),
        body: "a quantum leap in tomatoes".to_string(),
    })
    .await
    .expect("save body-match row");
    // A title match, saved SECOND.
    repo.save(&NewSearchNote {
        title: "Quantum computing".to_string(),
        body: "an introduction".to_string(),
    })
    .await
    .expect("save title-match row");

    let hits = repo.search("quantum").await.expect("search quantum");
    assert_eq!(hits.len(), 2, "both rows match 'quantum': {hits:?}");
    assert_eq!(
        hits[0].title, "Quantum computing",
        "the title (weight A) match must rank above the body (weight B) match: {hits:?}"
    );
    assert_eq!(
        hits[1].title, "Gardening basics",
        "the body-only match ranks second despite its lower id: {hits:?}"
    );
}

/// ADVERSARIAL: fail-closed tenant isolation. A search run under tenant B must
/// never return tenant A's rows even when the term matches. Without the
/// `tenant_id = $n` predicate in the `SQLite` arm this would leak A's data.
#[tokio::test]
async fn search_never_crosses_tenants_on_sqlite() {
    let pool = boot_pool("search_tenant").await;
    let repo = PgTenantSearchNoteRepository::with_pool_untracked(pool);

    // Tenant A owns a matching row.
    with_tenant("tenant-a".to_string(), async {
        repo.save(&NewTenantSearchNote {
            title: "Confidential Alpha".to_string(),
            body: "tenant a secret".to_string(),
        })
        .await
        .expect("save under tenant-a");
    })
    .await;

    // Tenant B owns a different matching row.
    with_tenant("tenant-b".to_string(), async {
        repo.save(&NewTenantSearchNote {
            title: "Bravo Alpha Notes".to_string(),
            body: "tenant b material".to_string(),
        })
        .await
        .expect("save under tenant-b");
    })
    .await;

    // SECURITY: tenant B searching "alpha" must see ONLY its own row — never
    // tenant A's, even though A's title also contains "alpha".
    let b_hits = with_tenant("tenant-b".to_string(), async {
        repo.search("alpha").await.expect("search under tenant-b")
    })
    .await;
    assert_eq!(b_hits.len(), 1, "tenant-b sees exactly one row: {b_hits:?}");
    assert_eq!(b_hits[0].tenant_id, "tenant-b", "no cross-tenant leak");
    assert_eq!(b_hits[0].title, "Bravo Alpha Notes");

    // The same isolation holds for the paginated path.
    let req = PageRequest::new(1, 10);
    let b_page = with_tenant("tenant-b".to_string(), async {
        repo.search_page("alpha", &req)
            .await
            .expect("search_page under tenant-b")
    })
    .await;
    assert_eq!(
        b_page.total_elements, 1,
        "tenant-b's paged total counts only its own rows: {:?}",
        b_page.content
    );
    assert!(
        b_page.content.iter().all(|n| n.tenant_id == "tenant-b"),
        "no cross-tenant row in the page: {:?}",
        b_page.content
    );

    // And tenant A still finds its own row (search actually works per-tenant).
    let a_hits = with_tenant("tenant-a".to_string(), async {
        repo.search("alpha").await.expect("search under tenant-a")
    })
    .await;
    assert_eq!(a_hits.len(), 1, "tenant-a sees its own row: {a_hits:?}");
    assert_eq!(a_hits[0].tenant_id, "tenant-a");

    // ADVERSARIAL FTS injection: tenant B searches for a token that exists ONLY
    // in tenant A's row ("secret", in A's body) — including an FTS5-operator
    // injection attempt. The FTS `MATCH` genuinely matches A's row, but the
    // `tenant_id = ?` predicate on the JOINed base table filters it out, so
    // tenant B still sees nothing. The sanitizer additionally neutralizes the
    // operators, so neither layer can leak A's data.
    for probe in ["secret", "secret OR alpha", "tenant_id:tenant-a"] {
        let leaked = with_tenant("tenant-b".to_string(), async {
            repo.search(probe)
                .await
                .expect("adversarial search under tenant-b")
        })
        .await;
        assert!(
            leaked.iter().all(|n| n.tenant_id == "tenant-b"),
            "tenant-b must never see tenant-a rows via `{probe}`: {leaked:?}"
        );
        assert!(
            leaked.is_empty(),
            "`{probe}` matches only tenant-a data, so tenant-b sees nothing: {leaked:?}"
        );
    }
}

/// Owner-scoped `search_page_scoped` (#1841) never returns another owner's rows.
#[tokio::test]
async fn search_page_scoped_isolates_owner_on_sqlite() {
    let pool = boot_pool("search_owner").await;
    let repo = PgOwnerSearchNoteRepository::with_pool_untracked(pool);

    repo.save(&NewOwnerSearchNote {
        title: "Owner One Report".to_string(),
        body: "quarterly report".to_string(),
        user_id: 1,
    })
    .await
    .expect("save owner 1 row");
    repo.save(&NewOwnerSearchNote {
        title: "Owner Two Report".to_string(),
        body: "quarterly report".to_string(),
        user_id: 2,
    })
    .await
    .expect("save owner 2 row");

    let req = PageRequest::new(1, 10);

    // Owner 1's scoped search over "report" returns only owner 1's row.
    let p1 = repo
        .search_page_scoped(1, "report", &req)
        .await
        .expect("scoped search for owner 1");
    assert_eq!(p1.total_elements, 1, "owner 1 sees only their own row");
    assert!(
        p1.content.iter().all(|n| n.user_id == 1),
        "no other owner's row leaks to owner 1: {:?}",
        p1.content
    );

    // Owner 2's scoped search returns only owner 2's row.
    let p2 = repo
        .search_page_scoped(2, "report", &req)
        .await
        .expect("scoped search for owner 2");
    assert_eq!(p2.total_elements, 1, "owner 2 sees only their own row");
    assert!(
        p2.content.iter().all(|n| n.user_id == 2),
        "no other owner's row leaks to owner 2: {:?}",
        p2.content
    );

    // A non-owner (owner 3) sees nothing.
    let p3 = repo
        .search_page_scoped(3, "report", &req)
        .await
        .expect("scoped search for owner 3");
    assert_eq!(
        p3.total_elements, 0,
        "owner 3 owns no matching rows: {:?}",
        p3.content
    );
}

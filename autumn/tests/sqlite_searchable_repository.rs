//! `#[repository(searchable)]` full-text-search fallback on the `SQLite` runtime
//! backend (issue #1996).
//!
//! Postgres searchable repositories rank rows with a `tsvector` +
//! `websearch_to_tsquery`/`ts_rank_cd` query, which has no `SQLite` equivalent.
//! The generated codegen therefore forks (via `backend_select!`): the `SQLite`
//! arm matches each `SEARCH_FIELDS` column with a case-insensitive
//! `lower(col) LIKE '%term%' ESCAPE '\'` substring test, OR-ed across the fields,
//! ordered `id DESC` (unranked — FTS5 ranking is a separate slice, #1910). This
//! suite drives that fallback end-to-end on an in-memory `SQLite` database (no
//! Docker):
//!
//! * substring matches are found case-insensitively and non-matches excluded;
//! * case-insensitivity is **ASCII-only** (the query is folded with
//!   `to_ascii_lowercase()` to match `SQLite`'s ASCII-only `lower()`): ASCII
//!   terms fold both ways, but a non-ASCII letter matches only with matching
//!   case — full-Unicode/ICU folding is deferred to #1910;
//! * `LIKE` metacharacters (`%`, `_`) in the query match **literally** and can
//!   never become a match-everything wildcard;
//! * `search_page` returns the right page + total;
//! * **ADVERSARIAL cross-tenant isolation:** a search run under tenant B never
//!   returns tenant A's rows even when the term matches (fail-closed tenant
//!   isolation is inviolable);
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
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

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
        // No `search_vector` column: the SQLite fallback matches the plain
        // SEARCH_FIELDS columns directly.
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

/// The `SQLite` search case-insensitivity is **ASCII-only** (issue #1996): the
/// query side is folded with `to_ascii_lowercase()` to match `SQLite`'s built-in
/// `lower()`, which folds only ASCII A–Z. So an ASCII term matches
/// case-insensitively, and even an accented term's ASCII characters fold — but a
/// non-ASCII letter matches only with matching case (full-Unicode/ICU folding is
/// deferred to #1910 with FTS5 ranking). This test pins both halves so the code
/// and the documented divergence agree.
#[tokio::test]
async fn search_case_insensitivity_is_ascii_only_on_sqlite() {
    let pool = boot_pool("search_ascii_fold").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    repo.save(&NewSearchNote {
        title: "Äpfel Tart".to_string(),
        body: "a German apple dessert".to_string(),
    })
    .await
    .expect("save accented row");

    // ── ASCII case-folding works ──
    // A pure-ASCII term matches case-insensitively (uppercase query vs lowercase
    // stored "apple").
    let ascii = repo.search("APPLE").await.expect("search APPLE");
    assert_eq!(
        ascii.len(),
        1,
        "ASCII term matches case-insensitively: {ascii:?}"
    );

    // Even alongside a non-ASCII letter, the ASCII characters fold: query "ÄP"
    // lowercases (ASCII-only) to "Äp", whose pattern `%Äp%` still matches the
    // stored "Äpfel Tart" because SQLite's `lower()` leaves the non-ASCII "Ä"
    // untouched on BOTH sides. (The old full-Unicode `to_lowercase()` folded the
    // query's "Ä"→"ä", producing `%äp%` that never matched — the F4 bug.)
    let mixed = repo.search("ÄP").await.expect("search ÄP");
    assert_eq!(
        mixed.len(),
        1,
        "ASCII 'P' folds even next to a non-ASCII letter, so 'ÄP' matches 'Äpfel': {mixed:?}"
    );

    // ── The ASCII-only limitation, documented ──
    // A differently-cased non-ASCII query letter does NOT match: query "äpfel"
    // (lowercase ä, U+00E4) folds to itself (ASCII-only), but the stored "Äpfel"
    // keeps its uppercase "Ä" (U+00C4) through SQLite's ASCII-only `lower()`, so
    // the two non-ASCII codepoints differ and no row matches. Full-Unicode case
    // folding would match here; it is intentionally deferred (#1910).
    let non_ascii = repo.search("äpfel").await.expect("search äpfel");
    assert!(
        non_ascii.is_empty(),
        "non-ASCII case is NOT folded (ASCII-only limitation): 'äpfel' must not match 'Äpfel': {non_ascii:?}"
    );
}

/// LIKE metacharacters (`%`, `_`) in the query match literally — a query
/// containing `%` can never become a match-everything wildcard.
#[tokio::test]
async fn search_treats_like_metacharacters_literally_on_sqlite() {
    let pool = boot_pool("search_metachars").await;
    let repo = PgSearchNoteRepository::with_pool_untracked(pool);

    repo.save(&NewSearchNote {
        title: "Sale".to_string(),
        body: "50% off everything".to_string(),
    })
    .await
    .expect("save percent row");
    repo.save(&NewSearchNote {
        title: "Sale".to_string(),
        body: "50X off select items".to_string(),
    })
    .await
    .expect("save non-percent row");
    repo.save(&NewSearchNote {
        title: "Codes".to_string(),
        body: "use a_b as the coupon".to_string(),
    })
    .await
    .expect("save underscore row");
    repo.save(&NewSearchNote {
        title: "Codes".to_string(),
        body: "use axb as the coupon".to_string(),
    })
    .await
    .expect("save non-underscore row");

    // `%` is escaped: "50%" matches only the literal-percent row, NOT "50X".
    let pct = repo.search("50%").await.expect("search 50%");
    assert_eq!(pct.len(), 1, "only the literal 50% row matches: {pct:?}");
    assert!(
        pct[0].body.contains("50% off"),
        "matched the wrong row: {pct:?}"
    );

    // `_` is escaped: "a_b" matches only the literal-underscore row, NOT "axb"
    // (an unescaped `_` would match any single char and catch "axb" too).
    let underscore = repo.search("a_b").await.expect("search a_b");
    assert_eq!(
        underscore.len(),
        1,
        "only the literal a_b row matches, not axb: {underscore:?}"
    );
    assert!(
        underscore[0].body.contains("a_b"),
        "matched the wrong row: {underscore:?}"
    );

    // A bare `%` must NOT return everything (it would with an unescaped LIKE).
    let bare_pct = repo.search("%").await.expect("search bare %");
    assert_eq!(
        bare_pct.len(),
        1,
        "a literal '%' matches only the row containing '%', never all rows: {bare_pct:?}"
    );
}

/// `search_page` returns the right page slice and total on SQLite.
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

/// ADVERSARIAL: fail-closed tenant isolation. A search run under tenant B must
/// never return tenant A's rows even when the term matches. Without the
/// `tenant_id = $n` predicate in the SQLite arm this would leak A's data.
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

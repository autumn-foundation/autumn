//! `#[translatable]` end-to-end over a real `#[model]` + `#[repository]`
//! (issue #1384).
//!
//! The non-ignored tests prove the generated surface exists and that
//! resolution/observability behave, with no database. The `#[ignore]`d tests
//! need Docker (testcontainers) and prove the *persistence* half: an
//! independent value per locale tag, surviving a round trip, with a write to
//! one locale leaving the others byte-for-byte intact.

#![cfg(all(feature = "db", feature = "i18n"))]

use autumn_web::i18n::{Translated, with_locale_chain};
use autumn_web::repository::ReadRoute;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    test_translatable_articles (id) {
        id -> Int8,
        title -> Text,
        summary -> Text,
        slug -> Text,
    }
}

/// One translatable field alongside a plain one: AC7's "purely additive, opt-in
/// per field" has to hold on the same model.
#[autumn_web::model(table = "test_translatable_articles")]
pub struct Article {
    #[id]
    pub id: i64,
    #[translatable]
    pub title: Translated,
    #[translatable]
    pub summary: Translated,
    pub slug: String,
}

#[autumn_web::repository(Article, table = "test_translatable_articles")]
pub trait ArticleRepository {
    /// Plain (non-translatable) equality lookup still works untouched.
    fn find_by_slug(slug: String) -> Vec<Article>;
}

fn chain(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn seeded() -> Article {
    Article {
        id: 1,
        title: Translated::from_pairs([("en", "Hello world"), ("es", "Hola mundo")]),
        summary: Translated::from_pairs([("en", "A greeting")]),
        slug: "hello".to_owned(),
    }
}

// ── Generated surface (no database) ──────────────────────────────────────────

#[test]
fn translatable_columns_are_registered() {
    use autumn_web::i18n::translatable_columns_for_table;

    assert_eq!(
        Article::__AUTUMN_TRANSLATABLE_COLUMNS,
        &["title", "summary"]
    );
    assert_eq!(Article::translatable_fields(), &["title", "summary"]);
    let mut registered = translatable_columns_for_table("test_translatable_articles");
    registered.sort_unstable();
    assert_eq!(registered, vec!["summary", "title"]);
}

/// AC5: an app can see which locales a record carries, keyed by field name, so
/// it can render a "needs translation" affordance.
#[test]
fn available_locales_and_is_translated_answer_per_field() {
    let article = seeded();
    assert_eq!(article.available_locales("title"), vec!["en", "es"]);
    assert_eq!(article.available_locales("summary"), vec!["en"]);
    assert!(article.is_translated("title", "es"));
    assert!(!article.is_translated("summary", "es"));
    // An unknown / non-translatable field is empty, never a panic.
    assert!(article.available_locales("slug").is_empty());
    assert!(!article.is_translated("slug", "en"));
    assert!(article.translated("slug").is_none());

    // Per-field accessors carry the same answers.
    assert_eq!(article.title_locales(), vec!["en", "es"]);
    assert!(article.title_is_translated("es"));
    assert!(!article.summary_is_translated("es"));
}

/// AC2/AC3 through the generated accessors: resolution against the ambient
/// locale, then the fallback chain, then the documented sentinel.
#[tokio::test]
async fn generated_accessors_resolve_against_the_ambient_locale() {
    let article = seeded();
    let (title, summary, missing) = with_locale_chain("es", chain(&["en"]), async {
        (
            article.title_localized().map(str::to_owned),
            // `summary` has no `es`, so it falls back to `en`.
            article.summary_localized().map(str::to_owned),
            article.localized("nope").map(str::to_owned),
        )
    })
    .await;
    assert_eq!(title.as_deref(), Some("Hola mundo"));
    assert_eq!(summary.as_deref(), Some("A greeting"));
    assert_eq!(missing, None);

    // A locale outside the chain entirely resolves to the sentinel.
    let empty = Article {
        title: Translated::new(),
        ..seeded()
    };
    let got = with_locale_chain("fr", chain(&["de"]), async {
        (
            empty.title_localized().map(str::to_owned),
            empty.title.to_string(),
        )
    })
    .await;
    assert_eq!(got.0, None);
    assert_eq!(got.1, "", "Display renders the empty-string sentinel");
}

#[test]
fn setters_write_one_locale_at_a_time() {
    let mut article = seeded();
    article.set_title("fr", "Bonjour le monde");
    assert_eq!(article.title_in("fr"), Some("Bonjour le monde"));
    assert_eq!(article.title.get("en"), Some("Hello world"));
    assert_eq!(article.title.get("es"), Some("Hola mundo"));
}

/// AC1/AC4 at the wire level: the column value carries every locale
/// independently and survives an encode/decode round trip.
#[test]
fn the_column_encoding_carries_every_locale() {
    let encoded = seeded().title.encode_column();
    let decoded = Translated::decode_column(&encoded, "en");
    assert_eq!(decoded.get("en"), Some("Hello world"));
    assert_eq!(decoded.get("es"), Some("Hola mundo"));
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
    // The DDL `autumn generate model Post 'title:String{translatable}'` emits
    // for a translatable column — `TEXT NOT NULL DEFAULT '{}'`. The generator
    // side of that claim is pinned by
    // `generate::model::tests::translatable_model_plan_emits_model_schema_migration_and_feature`
    // in autumn-cli, which asserts the exact same substring against a real
    // written `up.sql`; this fixture is the runtime half.
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_translatable_articles (\
         id BIGSERIAL PRIMARY KEY, \
         title TEXT NOT NULL DEFAULT '{}', \
         summary TEXT NOT NULL DEFAULT '{}', \
         slug TEXT NOT NULL UNIQUE)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_translatable_articles");
    (pool, container)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgArticleRepository {
    PgArticleRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

/// AC1 + AC4: write `en`, then `es`; both are retrievable, and updating `en`
/// afterwards leaves `es` untouched. The round-trip test the issue names.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn translations_round_trip_without_clobbering_each_other() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // Write `en` only.
    let mut title = Translated::new();
    title.set("en", "Hello world");
    let created = repo
        .save(&NewArticle {
            title,
            summary: Translated::new(),
            slug: "round-trip".to_owned(),
        })
        .await
        .expect("insert");

    // Add `es` on top.
    let mut next = created.title.clone();
    next.set("es", "Hola mundo");
    repo.update(
        created.id,
        &UpdateArticle {
            title: autumn_web::hooks::Patch::Set(next),
            ..Default::default()
        },
    )
    .await
    .expect("add es");

    let loaded = repo
        .find_by_id(created.id)
        .await
        .expect("reload")
        .expect("row exists");
    assert_eq!(loaded.title.get("en"), Some("Hello world"));
    assert_eq!(loaded.title.get("es"), Some("Hola mundo"));
    assert_eq!(loaded.available_locales("title"), vec!["en", "es"]);

    // Update `en`; `es` must be byte-for-byte unchanged.
    let mut edited = loaded.title.clone();
    edited.set("en", "Hello again");
    repo.update(
        created.id,
        &UpdateArticle {
            title: autumn_web::hooks::Patch::Set(edited),
            ..Default::default()
        },
    )
    .await
    .expect("edit en");

    let reloaded = repo
        .find_by_id(created.id)
        .await
        .expect("reload again")
        .expect("row exists");
    assert_eq!(reloaded.title.get("en"), Some("Hello again"));
    assert_eq!(
        reloaded.title.get("es"),
        Some("Hola mundo"),
        "updating one locale must not clobber another"
    );
    // The untranslated field is still the documented empty sentinel.
    assert!(reloaded.summary.is_empty());
}

/// The success metric's storage half: one seeded record serves N locales, and
/// an untranslated locale falls back — read straight out of the database with
/// no locale argument anywhere in the query.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn one_stored_record_serves_several_locales() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let created = repo
        .save(&NewArticle {
            title: Translated::from_pairs([("en", "Hello world"), ("es", "Hola mundo")]),
            summary: Translated::new(),
            slug: "multi-locale".to_owned(),
        })
        .await
        .expect("insert");

    let loaded = repo
        .find_by_id(created.id)
        .await
        .expect("reload")
        .expect("row exists");
    for (locale, expected) in [
        ("es", "Hola mundo"),
        ("en", "Hello world"),
        ("fr", "Hello world"),
    ] {
        let got: Option<String> = with_locale_chain(locale, chain(&["en"]), async {
            loaded.title_localized().map(str::to_owned)
        })
        .await;
        assert_eq!(got.as_deref(), Some(expected), "locale {locale}");
    }
}

/// AC7: a plain column on the same table keeps working, and a row written
/// before the column was declared translatable still reads back its text.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn legacy_plain_text_rows_upgrade_in_place() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query(
        "INSERT INTO test_translatable_articles (title, summary, slug) \
         VALUES ('Plain old title', '', 'legacy')",
    )
    .execute(&mut conn)
    .await
    .expect("seed legacy row");

    let rows = repo.find_by_slug("legacy".to_owned()).await.expect("find");
    assert_eq!(rows.len(), 1);
    let got = with_locale_chain("en", chain(&["en"]), async {
        rows[0].title_localized().map(str::to_owned)
    })
    .await;
    assert_eq!(
        got.as_deref(),
        Some("Plain old title"),
        "a pre-existing text column reads as the default locale — no backfill needed"
    );
}

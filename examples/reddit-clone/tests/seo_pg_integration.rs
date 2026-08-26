//! Integration tests: route-level SEO in the example app, against a real
//! Postgres.
//!
//! The guide for this feature is `docs/guide/seo.md`. These tests prove the
//! three parts the guide describes:
//!
//! 1. `sitemap_lists_every_public_page` — `RedditSitemapSource` reads the
//!    database and produces one absolute URL for each community and each post,
//!    plus the two hub pages. It also proves the `<lastmod>` value comes from
//!    `posts.updated_at`.
//! 2. `sitemap_and_robots_render_the_expected_documents` — the entries and the
//!    `[seo.robots]` rules become a valid `sitemap.xml` and a `robots.txt`
//!    that points back at the sitemap.
//! 3. `sitemap_is_empty_without_a_database` — a source with no database does
//!    not fail the boot. It contributes no entries.
//!
//! The Docker-backed tests are `#[ignore]` (like the other PG integration
//! tests here) and run through testcontainers by default. When Docker is not
//! available, point them at a Postgres that already runs:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://autumn:autumn@127.0.0.1:5432/reddit \
//!   cargo test -p reddit-clone --test seo_pg_integration -- --ignored --test-threads=1
//! ```

use autumn_web::config::{AutumnConfig, DatabaseConfig, RobotsConfig, SeoConfig};
use autumn_web::seo::{SitemapSource as _, robots_txt, sitemap_xml};
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use reddit_clone::seo::RedditSitemapSource;
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");
const CREATE_TAGS: &str = include_str!("../migrations/20260702000001_create_tags/up.sql");
const POLYMORPHIC_COMMENTS: &str =
    include_str!("../migrations/20260820000000_polymorphic_comments/up.sql");

/// The base URL these tests configure. It must match `[seo] base_url` in shape
/// (absolute, no trailing slash), not in value.
const BASE_URL: &str = "https://autumn-reddit.example.com";

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

/// Keeps whichever backing Postgres alive for the duration of the test.
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, String, Pool<AsyncPgConnection>) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url.clone());
        let pool = Pool::builder(manager)
            .max_size(8)
            .build()
            .expect("build pool");
        return (PgHandle::External, url, pool);
    }
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Postgres port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url.clone());
    let pool = Pool::builder(manager)
        .max_size(8)
        .build()
        .expect("build pool");
    (PgHandle::Container(Box::new(container)), url, pool)
}

async fn setup_schema(conn: &mut AsyncPgConnection) {
    if std::env::var("AUTUMN_TEST_PG_URL").is_ok() {
        conn.batch_execute(
            "DROP TABLE IF EXISTS post_tags, tags, comments, votes, \
             live_feed_events, posts, subreddits, users CASCADE;",
        )
        .await
        .expect("reset reddit-clone tables");
    }
    // The migration files hold many semicolon-separated statements. Postgres
    // rejects multiple commands in one prepared statement, so use the simple
    // (unprepared) batch protocol.
    conn.batch_execute(CREATE_SCHEMA)
        .await
        .expect("create reddit schema");
    conn.batch_execute(ADD_AVATAR)
        .await
        .expect("add users.avatar column");
    conn.batch_execute(CREATE_TAGS)
        .await
        .expect("create tag tables");
    conn.batch_execute(POLYMORPHIC_COMMENTS)
        .await
        .expect("make comments polymorphic");
}

async fn seed_user(conn: &mut AsyncPgConnection, username: &str) -> i64 {
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ($1, 'h') RETURNING id")
        .bind::<Text, _>(username)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed user")
        .id
}

async fn seed_subreddit(conn: &mut AsyncPgConnection, creator_id: i64, slug: &str) -> i64 {
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) \
         VALUES ($1, $1, 'a community', $2) RETURNING id",
    )
    .bind::<Text, _>(slug)
    .bind::<BigInt, _>(creator_id)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed subreddit")
    .id
}

async fn seed_post(
    conn: &mut AsyncPgConnection,
    author_id: i64,
    subreddit_id: i64,
    slug: &str,
    updated_at: &str,
) -> i64 {
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id, updated_at) \
         VALUES ($1, $1, 'body text', $2, $3, $4::timestamp) RETURNING id",
    )
    .bind::<Text, _>(slug)
    .bind::<BigInt, _>(author_id)
    .bind::<BigInt, _>(subreddit_id)
    .bind::<Text, _>(updated_at)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed post")
    .id
}

/// Build the configuration the example builds from `autumn.toml`, but pointed
/// at this test's database.
fn test_config(database_url: Option<&str>) -> AutumnConfig {
    AutumnConfig {
        database: DatabaseConfig {
            primary_url: database_url.map(str::to_owned),
            ..Default::default()
        },
        seo: SeoConfig {
            base_url: Some(BASE_URL.to_owned()),
            robots: RobotsConfig {
                additional_rules: vec!["Disallow: /submit".to_owned()],
                ..Default::default()
            },
        },
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sitemap_lists_every_public_page() {
    let (_handle, url, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("checkout");
    setup_schema(&mut conn).await;

    let author = seed_user(&mut conn, "ferris").await;
    let rust = seed_subreddit(&mut conn, author, "rust").await;
    let gardening = seed_subreddit(&mut conn, author, "gardening").await;
    seed_post(
        &mut conn,
        author,
        rust,
        "hello-world",
        "2026-05-01 12:00:00",
    )
    .await;
    seed_post(
        &mut conn,
        author,
        gardening,
        "tomatoes",
        "2026-06-02 09:30:00",
    )
    .await;
    drop(conn);

    let config = test_config(Some(&url));
    reddit_clone::seo::init_base_url(&config);
    let source = RedditSitemapSource::from_config(&config);
    let entries = source.entries().await;
    let locs: Vec<&str> = entries.iter().map(|e| e.loc.as_str()).collect();

    // The two hub pages. Both are `#[get]` routes, so the framework cannot
    // derive them and the source must list them.
    assert!(
        locs.contains(&format!("{BASE_URL}/").as_str()),
        "front page missing: {locs:?}"
    );
    assert!(
        locs.contains(&format!("{BASE_URL}/r").as_str()),
        "community index missing: {locs:?}"
    );

    // One entry for each community.
    assert!(
        locs.contains(&format!("{BASE_URL}/r/rust").as_str()),
        "r/rust missing: {locs:?}"
    );
    assert!(
        locs.contains(&format!("{BASE_URL}/r/gardening").as_str()),
        "r/gardening missing: {locs:?}"
    );

    // One entry for each post, at the same path the `show` route serves.
    assert!(
        locs.contains(&format!("{BASE_URL}/r/rust/posts/hello-world").as_str()),
        "post URL missing: {locs:?}"
    );
    assert!(
        locs.contains(&format!("{BASE_URL}/r/gardening/posts/tomatoes").as_str()),
        "post URL missing: {locs:?}"
    );

    // `<lastmod>` comes from `posts.updated_at`, not from the crawl time.
    let hello = entries
        .iter()
        .find(|e| e.loc.ends_with("/posts/hello-world"))
        .expect("hello-world entry");
    assert_eq!(hello.lastmod.as_deref(), Some("2026-05-01"));

    // Every URL is absolute. A relative URL in a sitemap is invalid.
    for loc in &locs {
        assert!(loc.starts_with(BASE_URL), "relative URL in sitemap: {loc}");
    }

    // `/about` is a `#[static_get]` route. The framework derives its entry
    // from the static-route table, so the source must NOT list it too.
    assert!(
        !locs.contains(&format!("{BASE_URL}/about").as_str()),
        "the source must leave static routes to the framework: {locs:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sitemap_and_robots_render_the_expected_documents() {
    let (_handle, url, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("checkout");
    setup_schema(&mut conn).await;

    let author = seed_user(&mut conn, "ferris").await;
    let rust = seed_subreddit(&mut conn, author, "rust").await;
    seed_post(
        &mut conn,
        author,
        rust,
        "hello-world",
        "2026-05-01 12:00:00",
    )
    .await;
    drop(conn);

    let config = test_config(Some(&url));
    reddit_clone::seo::init_base_url(&config);
    let entries = RedditSitemapSource::from_config(&config).entries().await;

    // This is what the framework serves at GET /sitemap.xml.
    let xml = sitemap_xml(&entries, Some(BASE_URL));
    assert!(
        xml.contains(r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#),
        "sitemap must declare the sitemap namespace:\n{xml}"
    );
    assert!(
        xml.contains(&format!("<loc>{BASE_URL}/r/rust/posts/hello-world</loc>")),
        "post URL missing from the rendered sitemap:\n{xml}"
    );
    assert!(
        xml.contains("<lastmod>2026-05-01</lastmod>"),
        "lastmod missing from the rendered sitemap:\n{xml}"
    );

    // This is what the framework serves at GET /robots.txt under the `prod`
    // profile. The `Sitemap:` line is derived from `[seo] base_url`.
    let robots = robots_txt(
        "prod",
        Some(&format!("{BASE_URL}/sitemap.xml")),
        &config.seo.robots.additional_rules,
    );
    assert!(robots.contains("User-agent: *"), "robots:\n{robots}");
    assert!(robots.contains("Allow: /"), "robots:\n{robots}");
    assert!(robots.contains("Disallow: /submit"), "robots:\n{robots}");
    assert!(
        robots.contains(&format!("Sitemap: {BASE_URL}/sitemap.xml")),
        "robots must point at the sitemap:\n{robots}"
    );

    // The `dev` profile keeps a local run out of the index.
    let dev_robots = robots_txt("dev", None, &[]);
    assert!(
        dev_robots.contains("Disallow: /"),
        "dev must disallow every crawler:\n{dev_robots}"
    );
}

#[tokio::test]
async fn sitemap_is_empty_without_a_database() {
    // No `primary_url`, so `create_pool` returns no pool. The source must stay
    // quiet rather than fail the boot: `/sitemap.xml` then lists only the
    // static routes the framework derives.
    let config = test_config(None);
    let entries = RedditSitemapSource::from_config(&config).entries().await;
    assert!(
        entries.is_empty(),
        "a source with no database must contribute no entries; got {} entries",
        entries.len()
    );
}

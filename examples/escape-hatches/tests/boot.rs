//! Boots the real binary. `TestApp` cannot install a pool provider, error
//! pages, or an exception filter, so this test proves that `app()` wires them.
//!
//! - H12: the URL has no password. The pool reads it from a file.
//! - H10: an unknown SKU gets the stockroom 404 page.
//! - H11: a query timeout answers 503 with `Retry-After`.
//! - H7: pages carry `Cache-Control: no-store`.
//!
//! Needs Docker:
//!
//! ```text
//! cargo test -p escape-hatches --test boot -- --ignored
//! ```

use autumn_web::reexports::diesel::Connection;
use autumn_web::reexports::diesel::connection::SimpleConnection;
use autumn_web::reexports::diesel::pg::PgConnection;
use escape_hatches::MIGRATIONS;

/// Products in the large table. The report sorts all of them, so it cannot
/// finish inside the 20 ms statement timeout below.
const BULK_ROWS: u32 = 300_000;

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn real_binary_wires_every_app_level_hatch() {
    let db = example_e2e::provision_postgres(1).await;
    let url = db.urls()[0].clone();

    // A release step runs migrations with its own credentials. The app does not.
    let setup_url = url.clone();
    tokio::task::spawn_blocking(move || {
        autumn_web::migrate::run_pending(&setup_url, MIGRATIONS).expect("migrate");
        let mut conn = PgConnection::establish(&setup_url).expect("connect");
        conn.batch_execute(&format!(
            "INSERT INTO products (sku, name, category, stock, price_cents) \
             VALUES ('A-1', 'Hammer', 'tools', 4, 1500); \
             INSERT INTO products (sku, name, category, stock, price_cents) \
             SELECT 'BULK-' || n, 'Bulk ' || n, 'bulk-' || (n % 10), n % 50, n \
             FROM generate_series(1, {BULK_ROWS}) AS n;"
        ))
        .expect("seed");
    })
    .await
    .expect("setup task");

    // H12: the password lives only in a file, as a secrets sidecar writes it.
    let secrets = tempfile::tempdir().expect("tempdir");
    let password_file = secrets.path().join("db-password");
    std::fs::write(&password_file, "postgres\n").expect("write password");
    let url_without_password = url.replace("postgres:postgres@", "postgres@");
    assert_ne!(url, url_without_password, "the URL must lose its password");

    // The real `autumn.toml`, with a tiny statement timeout for H11.
    let config_dir = tempfile::tempdir().expect("tempdir");
    let config = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/autumn.toml"))
        .expect("read autumn.toml");
    let config = config.replace(
        r#"statement_timeout = "5s""#,
        r#"statement_timeout = "20ms""#,
    );
    assert!(
        config.contains("20ms"),
        "autumn.toml must set statement_timeout"
    );
    std::fs::write(config_dir.path().join("autumn.toml"), config).expect("write config");

    let password_path = password_file.to_str().expect("utf-8 path");
    let config_path = config_dir.path().to_str().expect("utf-8 path");
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_escape-hatches"),
        env!("CARGO_MANIFEST_DIR"),
        &[
            ("AUTUMN_MANIFEST_DIR", config_path),
            ("AUTUMN_DATABASE__URL", &url_without_password),
            ("AUTUMN_DATABASE__AUTO_MIGRATE", "false"),
            ("STOCKROOM_DB_PASSWORD_FILE", password_path),
        ],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn the escape-hatches binary");
    let base = app.base_url();
    let http = reqwest::Client::new();

    // H12 + H7: a page that reads the database works.
    let page = http
        .get(format!("{base}/products/A-1"))
        .send()
        .await
        .expect("GET product");
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert!(page.text().await.expect("body").contains("Hammer"));

    // H10: the stockroom 404 page, with a link to order the SKU.
    let missing = http
        .get(format!("{base}/products/ZZ-9"))
        .send()
        .await
        .expect("GET 404");
    assert_eq!(missing.status(), 404);
    let body = missing.text().await.expect("body");
    assert!(body.contains(r#"href="/supplier/items/ZZ-9""#), "{body}");

    // H11: the report times out in Postgres. The filter adds `Retry-After`.
    let report = http
        .get(format!("{base}/reports/stock-value"))
        .send()
        .await
        .expect("GET report");
    assert_eq!(report.status(), 503);
    assert_eq!(report.headers()["retry-after"], "2");
}

//! Starts the real binary. `TestApp` cannot install a pool provider, error
//! pages, or an exception filter. So this test proves that `app()` wires
//! them, and the other hatches that read the environment:
//!
//! - H12: the URL has no password. The pool reads it from a file. The app
//!   applies its own migration at boot with that password.
//! - H7: pages carry `Cache-Control: no-store`.
//! - H10: an unknown SKU gets the stockroom 404 page.
//! - H8: the exports folder is the project folder's `exports/`.
//! - H5 + H13: the scanner token comes from the environment. A checkout
//!   answers `201` with `Location`.
//! - H11: a query timeout answers 503 with `Retry-After`.
//!
//! Needs Docker:
//!
//! ```text
//! cargo test -p escape-hatches --test boot -- --ignored
//! ```

use autumn_web::reexports::diesel::Connection;
use autumn_web::reexports::diesel::connection::SimpleConnection;
use autumn_web::reexports::diesel::pg::PgConnection;

const TOKEN: &str = "boot-test-token";

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn real_binary_wires_app_level_hatches() {
    let db = example_e2e::provision_postgres(1).await;
    let url = db.urls()[0].clone();

    // H12: only a file holds the password. A secrets sidecar writes it.
    let secrets = tempfile::tempdir().expect("tempdir");
    let password_file = secrets.path().join("db-password");
    std::fs::write(&password_file, "postgres\n").expect("write password");
    let url_without_password = url.replace("postgres:postgres@", "postgres@");
    assert_ne!(url, url_without_password, "the URL must lose its password");

    // The project folder: the real `autumn.toml`, with a short statement
    // timeout for H11, and an export file for H8.
    let project = tempfile::tempdir().expect("tempdir");
    let config = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/autumn.toml"))
        .expect("read autumn.toml");
    let config = config.replace(
        r#"statement_timeout = "5s""#,
        r#"statement_timeout = "500ms""#,
    );
    assert!(
        config.contains("500ms"),
        "autumn.toml must set statement_timeout"
    );
    std::fs::write(project.path().join("autumn.toml"), config).expect("write config");
    std::fs::create_dir(project.path().join("exports")).expect("mkdir exports");
    std::fs::write(
        project.path().join("exports").join("stock.csv"),
        "sku,stock\n",
    )
    .expect("write export");

    let password_path = password_file.to_str().expect("utf-8 path");
    let project_path = project.path().to_str().expect("utf-8 path");
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_escape-hatches"),
        env!("CARGO_MANIFEST_DIR"),
        &[
            ("AUTUMN_MANIFEST_DIR", project_path),
            ("AUTUMN_DATABASE__URL", &url_without_password),
            ("STOCKROOM_DB_PASSWORD_FILE", password_path),
            ("STOCKROOM_SCANNER_TOKEN", TOKEN),
        ],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn the escape-hatches binary");
    let base = app.base_url();
    let http = reqwest::Client::new();

    // The app applied its migration at boot, so the table exists.
    let setup_url = url.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = PgConnection::establish(&setup_url).expect("connect");
        conn.batch_execute(
            "INSERT INTO products (sku, name, category, stock, price_cents) \
             VALUES ('A-1', 'Hammer', 'tools', 4, 1500);",
        )
        .expect("seed: the app must have applied its migration");
    })
    .await
    .expect("setup task");

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
        .header("accept", "text/html")
        .send()
        .await
        .expect("GET 404");
    assert_eq!(missing.status(), 404);
    let body = missing.text().await.expect("body");
    assert!(body.contains(r#"href="/supplier/items/ZZ-9""#), "{body}");

    // H8: the export file in the project folder.
    let export = http
        .get(format!("{base}/exports/stock.csv"))
        .send()
        .await
        .expect("GET export");
    assert_eq!(export.status(), 200);
    assert_eq!(export.text().await.expect("body"), "sku,stock\n");

    // H5: no token, no checkout. H13: with the token, 201 and `Location`.
    let cart =
        serde_json::json!({ "order_ref": "o-1", "lines": [{ "sku": "A-1", "quantity": 1 }] });
    let refused = http
        .post(format!("{base}/api/checkout"))
        .json(&cart)
        .send()
        .await
        .expect("POST without token");
    assert_eq!(refused.status(), 401);
    let created = http
        .post(format!("{base}/api/checkout"))
        .bearer_auth(TOKEN)
        .json(&cart)
        .send()
        .await
        .expect("POST checkout");
    assert_eq!(created.status(), 201);
    assert_eq!(created.headers()["location"], "/api/orders/o-1");

    // H11: another session locks the table, so the report waits until the
    // statement timeout stops it. The filter adds `Retry-After`.
    let lock_url = url.clone();
    let mut lock = tokio::task::spawn_blocking(move || {
        let mut conn = PgConnection::establish(&lock_url).expect("connect");
        conn.batch_execute("BEGIN; LOCK TABLE products IN ACCESS EXCLUSIVE MODE;")
            .expect("lock products");
        conn
    })
    .await
    .expect("lock task");
    let report = http
        .get(format!("{base}/reports/stock-value"))
        .header("accept", "application/json")
        .send()
        .await
        .expect("GET report");
    lock.batch_execute("ROLLBACK;").expect("unlock products");
    assert_eq!(report.status(), 503);
    assert_eq!(report.headers()["retry-after"], "2");
    let problem: serde_json::Value = report.json().await.expect("problem JSON");
    assert_eq!(problem["code"], "autumn.query_timeout", "{problem}");
}

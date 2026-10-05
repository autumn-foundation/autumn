//! Tests for each escape hatch. Each test names the hatch it proves (H1..H13,
//! see `PLAN.md`).
//!
//! Two tiers:
//!
//! - No Docker: layers, plugins, routers, filters, error pages.
//! - Docker: everything that reads or writes rows. These tests share one
//!   Postgres container and its tables, so run them on one thread:
//!
//! ```text
//! cargo test -p escape-hatches --test hatches                                         # tier 1
//! cargo test -p escape-hatches --test hatches -- --include-ignored --test-threads=1   # both
//! ```

mod support;

use std::sync::Arc;

use autumn_web::config::{AutumnConfig, MockEnv};
use autumn_web::error_pages::{ErrorContext, ErrorPageRenderer};
use autumn_web::middleware::{AutumnErrorInfo, ExceptionFilter, ExceptionFilterLayer};
use autumn_web::plugin::Plugin;
use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use escape_hatches::hatches::error_pages::StockroomErrorPages;
use escape_hatches::hatches::exports;
use escape_hatches::hatches::password_file::PasswordFilePool;
use escape_hatches::hatches::retry_after::RetryAfterOn503;
use escape_hatches::hatches::supplier_plugin::SupplierPlugin;
use escape_hatches::models::{Cart, CartLine, Receipt, UpdateProduct};
use escape_hatches::repositories::{PgProductRepository, ProductRepository};
use escape_hatches::{supplier, scanner_guard};
use serde_json::{Value, json};
use support::{SCANNER_TOKEN, fresh_db, product, seed, stock_of};

// ── Helpers ────────────────────────────────────────────────────────────────

/// The app as `TestApp` can build it. `app()` wires the same pieces; the
/// `boot` test proves the parts that `TestApp` cannot reach.
fn client(pool: Option<autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<autumn_web::RuntimeConnection>>, exports_dir: &std::path::Path) -> TestClient {
    let app = TestApp::new()
        .routes(escape_hatches::routes())
        .scoped("/api", scanner_guard(Some(SCANNER_TOKEN)), escape_hatches::api_routes())
        .nest(exports::PREFIX, exports::router(exports_dir))
        .plugin(SupplierPlugin::sample())
        .layer(escape_hatches::hatches::cache_control::no_store());
    match pool {
        Some(pool) => app.with_db(pool).build(),
        None => app.build(),
    }
}

fn no_db_client() -> TestClient {
    let dir = tempfile::tempdir().expect("tempdir");
    client(None, dir.path())
}

fn cart(order_ref: &str, lines: &[(&str, i32)]) -> Value {
    json!(Cart {
        order_ref: order_ref.to_owned(),
        lines: lines
            .iter()
            .map(|(sku, quantity)| CartLine { sku: (*sku).to_owned(), quantity: *quantity })
            .collect(),
    })
}

async fn post_api(client: &TestClient, path: &str, body: &Value) -> autumn_web::test::TestResponse {
    client
        .post(path)
        .header("authorization", &format!("Bearer {SCANNER_TOKEN}"))
        .json(body)
        .send()
        .await
}

// ── Tier 1: no database ────────────────────────────────────────────────────

/// H7: every page says `no-store`, because stock counts are live.
#[tokio::test]
async fn cache_control_no_store_on_every_response() {
    let client = no_db_client();
    client
        .get("/supplier/items")
        .send()
        .await
        .assert_ok()
        .assert_header("cache-control", "no-store");
}

/// H7: a route that sets its own `Cache-Control` keeps it.
#[tokio::test]
async fn cache_control_keeps_a_route_value() {
    #[get("/cached")]
    #[public]
    async fn cached() -> impl IntoResponse {
        ([("cache-control", "max-age=60")], "fixed")
    }
    let client = TestApp::new()
        .routes(routes![cached])
        .layer(escape_hatches::hatches::cache_control::no_store())
        .build();
    client
        .get("/cached")
        .send()
        .await
        .assert_ok()
        .assert_header("cache-control", "max-age=60");
}

/// H8: files that a job writes at runtime are served from their own folder.
#[tokio::test]
async fn exports_serves_runtime_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("stock.csv"), "sku,stock\nA-1,5\n").expect("write export");
    let client = client(None, dir.path());

    client
        .get("/exports/stock.csv")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("sku,stock\nA-1,5\n")
        .assert_header("cache-control", "no-store");
    client.get("/exports/missing.csv").send().await.assert_status(404);
}

/// H8: a path cannot climb out of the exports folder.
#[tokio::test]
async fn exports_refuses_path_traversal() {
    let root = tempfile::tempdir().expect("tempdir");
    let exports_dir = root.path().join("exports");
    std::fs::create_dir(&exports_dir).expect("mkdir");
    std::fs::write(root.path().join("secret.txt"), "secret").expect("write secret");
    let client = client(None, &exports_dir);

    let response = client.get("/exports/../secret.txt").send().await;
    assert_ne!(response.status, StatusCode::OK, "path traversal must not serve a file");
    assert!(!response.text().contains("secret"));
}

/// H8: the nested exports router is declared, so `autumn routes audit` sees it.
#[test]
fn exports_declares_its_route() {
    let routes = exports::routes();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].method, "GET");
    assert!(routes[0].path.starts_with(exports::PREFIX));
}

/// H9: the plugin mounts the supplier's plain Axum router under `/supplier`.
#[tokio::test]
async fn supplier_plugin_serves_the_catalog() {
    let client = no_db_client();
    let items: Value = client.get("/supplier/items").send().await.assert_ok().json();
    assert!(items.as_array().is_some_and(|items| !items.is_empty()), "{items}");

    let sku = items[0]["sku"].as_str().expect("sku").to_owned();
    let item: Value = client
        .get(&format!("/supplier/items/{sku}"))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(item["sku"], sku);
    client.get("/supplier/items/NO-SUCH-SKU").send().await.assert_status(404);
}

/// H9: the plugin passes the framework's plugin conformance checks. Its
/// routes carry its name and stay under its prefix.
#[test]
fn supplier_plugin_passes_conformance() {
    let plugin = SupplierPlugin::sample();
    let name = plugin.name().into_owned();
    let routes = autumn_web::app()
        .plugin(plugin)
        .plugin_route_infos()
        .expect("route manifest");
    let report = run_conformance(&ConformanceConfig::new(&name).prefix("/supplier"), &routes);
    assert!(report.passed(), "{}", report.to_text_report());
}

/// The real `app()` declares every route, raw routers included. So
/// `autumn routes audit` can see all of them.
#[test]
fn app_declares_every_route() {
    let routes = escape_hatches::app().plugin_route_infos().expect("route manifest");
    let mut listed: Vec<String> = routes.iter().map(|r| format!("{} {}", r.method, r.path)).collect();
    listed.sort();
    for expected in [
        "GET /",
        "GET /api/orders/{order_ref}",
        "GET /exports/{*path}",
        "GET /products/{sku}",
        "GET /reports/stock-value",
        "GET /supplier/items",
        "GET /supplier/items/{sku}",
        "POST /api/checkout",
        "POST /api/products/{sku}/reserve",
        "POST /api/restock",
    ] {
        assert!(listed.contains(&expected.to_owned()), "missing {expected}: {listed:#?}");
    }
}

/// `merge`: the same plain router, merged as-is, gets the app middleware.
#[tokio::test]
async fn merged_raw_router_gets_app_middleware() {
    let client = TestApp::new()
        .merge(supplier::router(supplier::Catalog::sample()))
        .build();
    let response = client.get("/items").send().await;
    response.assert_ok();
    assert!(response.header("x-request-id").is_some(), "request id layer must run");
}

/// H10: a 404 for an unknown SKU links to the supplier catalog.
#[test]
fn not_found_page_links_to_the_supplier_catalog() {
    let ctx = ErrorContext {
        status: StatusCode::NOT_FOUND,
        message: "not found".to_owned(),
        path: "/products/ZZ-9".to_owned(),
        request_id: None,
        details: None,
        is_dev: false,
    };
    let page = StockroomErrorPages.render_404(&ctx).into_string();
    assert!(page.contains("ZZ-9"), "{page}");
    assert!(page.contains(r#"href="/supplier/items/ZZ-9""#), "{page}");
    assert!(page.contains(r#"href="/""#), "{page}");
}

/// H10: a 404 for another path has no supplier link.
#[test]
fn not_found_page_for_other_paths_has_no_supplier_link() {
    let ctx = ErrorContext {
        status: StatusCode::NOT_FOUND,
        message: "not found".to_owned(),
        path: "/nowhere".to_owned(),
        request_id: None,
        details: None,
        is_dev: false,
    };
    let page = StockroomErrorPages.render_404(&ctx).into_string();
    assert!(!page.contains("/supplier/items/"), "{page}");
}

/// H10: a SKU in the path is escaped in the page.
#[test]
fn not_found_page_escapes_the_sku() {
    let ctx = ErrorContext {
        status: StatusCode::NOT_FOUND,
        message: "not found".to_owned(),
        path: "/products/<script>".to_owned(),
        request_id: None,
        details: None,
        is_dev: false,
    };
    let page = StockroomErrorPages.render_404(&ctx).into_string();
    assert!(!page.contains("<script>"), "{page}");
}

fn error_info(status: StatusCode) -> AutumnErrorInfo {
    AutumnErrorInfo {
        status,
        message: "x".to_owned(),
        details: None,
        problem_type: None,
        backtrace_string: None,
    }
}

/// H11: a 503 without `Retry-After` gets one.
#[test]
fn retry_after_is_added_to_a_503() {
    let response = (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response();
    let response = RetryAfterOn503.filter(&error_info(StatusCode::SERVICE_UNAVAILABLE), response);
    assert_eq!(response.headers()["retry-after"], "2");
}

/// H11: a 503 that has `Retry-After` keeps its value. Other statuses do not change.
#[test]
fn retry_after_keeps_existing_values_and_other_statuses() {
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response();
    response
        .headers_mut()
        .insert("retry-after", "30".parse().expect("header"));
    let response = RetryAfterOn503.filter(&error_info(StatusCode::SERVICE_UNAVAILABLE), response);
    assert_eq!(response.headers()["retry-after"], "30");

    let response = (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    let response = RetryAfterOn503.filter(&error_info(StatusCode::INTERNAL_SERVER_ERROR), response);
    assert!(response.headers().get("retry-after").is_none());
}

/// H11: the filter sees the framework's query-timeout error in the pipeline.
#[tokio::test]
async fn retry_after_on_a_query_timeout_response() {
    #[get("/slow")]
    #[public]
    async fn slow() -> AutumnResult<&'static str> {
        Err(AutumnError::query_timeout("statement timeout"))
    }
    let client = TestApp::new()
        .routes(routes![slow])
        .layer(ExceptionFilterLayer::new(vec![Arc::new(RetryAfterOn503)]))
        .build();
    client
        .get("/slow")
        .send()
        .await
        .assert_status(503)
        .assert_header("retry-after", "2");
}

/// H5: the scanner API refuses a call with no bearer token.
#[tokio::test]
async fn api_requires_a_bearer_token() {
    let client = no_db_client();
    client
        .post("/api/checkout")
        .json(&cart("o-1", &[("A-1", 1)]))
        .send()
        .await
        .assert_status(401);
    client
        .post("/api/checkout")
        .header("authorization", "Bearer wrong")
        .json(&cart("o-1", &[("A-1", 1)]))
        .send()
        .await
        .assert_status(401);
}

/// H5: with no token configured, the API refuses every call.
#[tokio::test]
async fn api_with_no_configured_token_refuses_all_calls() {
    let client = TestApp::new()
        .scoped("/api", scanner_guard(None), escape_hatches::api_routes())
        .build();
    post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 1)]))
        .await
        .assert_status(401);
}

/// The app's real `prod` config, from `autumn.toml`.
fn prod_config() -> AutumnConfig {
    let env = MockEnv::new()
        .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
        .with("AUTUMN_ENV", "prod");
    AutumnConfig::load_with_env(&env).expect("load prod config")
}

/// H6: in `prod`, CSRF is on. A bearer client has no CSRF cookie, so `/api/`
/// must be exempt. A browser form route stays protected.
#[tokio::test]
async fn csrf_exempts_the_bearer_api_only() {
    let config = prod_config();
    assert!(config.security.csrf.enabled, "prod turns CSRF on");
    assert_eq!(config.security.csrf.exempt_paths, vec!["/api/".to_owned()]);

    #[post("/form")]
    #[public]
    async fn form() -> &'static str {
        "ok"
    }
    let client = TestApp::new()
        .config(config)
        .routes(routes![form])
        .scoped("/api", scanner_guard(Some(SCANNER_TOKEN)), escape_hatches::api_routes())
        .build();

    // No CSRF token on a browser route: refused.
    client.post("/form").send().await.assert_status(403);
    // No CSRF token on the bearer API: not refused by CSRF. (No DB here, so
    // the handler answers 503. The point is that it is not 403.)
    let response = post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 1)])).await;
    assert_ne!(response.status, StatusCode::FORBIDDEN, "{}", response.text());
}

// ── Tier 2: Postgres (Docker) ──────────────────────────────────────────────

/// Why `with_lock` exists. Two requests read the same row, then each writes
/// an absolute value. The second write erases the first. Five units, two
/// sales, and the table says four.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn hazard_repository_read_modify_write_loses_an_update() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 5, 100)]).await;
    let repo = PgProductRepository::with_pool_untracked(db.pool());

    let first = repo.find_by_sku("A-1".to_owned()).await.expect("read")[0].clone();
    let second = repo.find_by_sku("A-1".to_owned()).await.expect("read")[0].clone();
    for read in [first, second] {
        let change = UpdateProduct { stock: Patch::Set(read.stock - 1), ..Default::default() };
        repo.update(read.id, &change).await.expect("write");
    }

    assert_eq!(stock_of(db, "A-1").await, 4, "one sale is lost");
}

/// The convention for one product: `with_lock`. Ten buyers, three units.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn convention_reserve_with_lock_never_oversells() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 3, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let body = json!({ "quantity": 1 });
    let calls = (0..10)
        .map(|_| post_api(&client, "/api/products/A-1/reserve", &body))
        .collect();
    let statuses: Vec<u16> = futures_join(calls).await;
    assert_eq!(statuses.iter().filter(|s| **s == 200).count(), 3, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|s| **s == 409).count(), 7, "{statuses:?}");
    assert_eq!(stock_of(db, "A-1").await, 0);
}

/// Send every call at once and collect the status codes.
async fn futures_join<F>(calls: Vec<F>) -> Vec<u16>
where
    F: std::future::Future<Output = autumn_web::test::TestResponse>,
{
    futures::future::join_all(calls)
        .await
        .iter()
        .map(|response| response.status.as_u16())
        .collect()
}

/// H1: a cart is all lines or none. Line two fails, so line one rolls back.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checkout_reserves_every_line_or_none() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 5, 100), product("B-1", "tools", 1, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let response = post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 2), ("B-1", 2)])).await;
    response.assert_status(409).assert_body_contains("B-1");
    assert_eq!(stock_of(db, "A-1").await, 5, "line one rolled back");
    assert_eq!(stock_of(db, "B-1").await, 1);

    post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 2), ("B-1", 1)]))
        .await
        .assert_status(201);
    assert_eq!(stock_of(db, "A-1").await, 3);
    assert_eq!(stock_of(db, "B-1").await, 0);
}

/// H13: checkout answers `201 Created` with a `Location` that reads the order.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checkout_returns_201_with_location() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 5, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let response = post_api(&client, "/api/checkout", &cart("o-7", &[("A-1", 2)])).await;
    response.assert_status(201).assert_header("location", "/api/orders/o-7");
    let expected = Receipt {
        order_ref: "o-7".to_owned(),
        lines: vec![CartLine { sku: "A-1".to_owned(), quantity: 2 }],
    };
    assert_eq!(response.json::<Receipt>(), expected);

    let read: Receipt = client
        .get("/api/orders/o-7")
        .header("authorization", &format!("Bearer {SCANNER_TOKEN}"))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(read, expected);
}

/// H1: a retried checkout with a used `order_ref` changes nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checkout_with_a_used_order_ref_changes_nothing() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 5, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 1)])).await.assert_status(201);
    post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 1)]))
        .await
        .assert_status(409)
        .assert_body_contains("o-1");
    assert_eq!(stock_of(db, "A-1").await, 4);
}

/// H1: bad input is refused before any write.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checkout_refuses_bad_carts() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 5, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    post_api(&client, "/api/checkout", &cart("o-1", &[])).await.assert_status(422);
    post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 0)])).await.assert_status(422);
    post_api(&client, "/api/checkout", &cart("", &[("A-1", 1)])).await.assert_status(422);
    post_api(&client, "/api/checkout", &cart("o-1", &[("NOPE", 1)]))
        .await
        .assert_status(404)
        .assert_body_contains("NOPE");
    assert_eq!(stock_of(db, "A-1").await, 5);
}

/// H1: two lines for one SKU count as one line with the sum.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checkout_merges_lines_for_the_same_sku() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 3, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    post_api(&client, "/api/checkout", &cart("o-1", &[("A-1", 2), ("A-1", 2)]))
        .await
        .assert_status(409);
    assert_eq!(stock_of(db, "A-1").await, 3);
}

/// H1: carts that touch the same SKUs in opposite order do not deadlock and
/// do not oversell. The handler locks rows in one fixed order.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checkout_concurrent_carts_never_oversell_or_deadlock() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 6, 100), product("B-1", "tools", 6, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let bodies: Vec<Value> = (0..10)
        .map(|n| {
            if n % 2 == 0 {
                cart(&format!("o-{n}"), &[("A-1", 1), ("B-1", 1)])
            } else {
                cart(&format!("o-{n}"), &[("B-1", 1), ("A-1", 1)])
            }
        })
        .collect();
    let calls = bodies.iter().map(|body| post_api(&client, "/api/checkout", body)).collect();
    let statuses = futures_join(calls).await;

    assert!(statuses.iter().all(|s| *s == 201 || *s == 409), "{statuses:?}");
    assert_eq!(statuses.iter().filter(|s| **s == 201).count(), 6, "{statuses:?}");
    assert_eq!(stock_of(db, "A-1").await, 0);
    assert_eq!(stock_of(db, "B-1").await, 0);
}

/// H2: restock adds to every product in a category in one statement.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restock_adds_to_a_whole_category() {
    let db = fresh_db().await;
    seed(
        db,
        &[
            product("A-1", "tools", 1, 100),
            product("A-2", "tools", 0, 100),
            product("P-1", "paint", 2, 100),
        ],
    )
    .await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let body: Value = post_api(&client, "/api/restock", &json!({ "category": "tools", "add": 5 }))
        .await
        .assert_ok()
        .json();
    assert_eq!(body["updated"], 2);
    assert_eq!(stock_of(db, "A-1").await, 6);
    assert_eq!(stock_of(db, "A-2").await, 5);
    assert_eq!(stock_of(db, "P-1").await, 2, "other categories do not change");

    post_api(&client, "/api/restock", &json!({ "category": "tools", "add": 0 }))
        .await
        .assert_status(422);
}

/// H2: restock is relative. A checkout that runs between the restock's read
/// and write cannot be lost, because there is no read.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restock_and_checkout_together_lose_nothing() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 50, 100)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let restock = json!({ "category": "tools", "add": 1 });
    let mut bodies = Vec::new();
    for n in 0..10 {
        bodies.push(("/api/checkout", cart(&format!("o-{n}"), &[("A-1", 1)])));
        bodies.push(("/api/restock", restock.clone()));
    }
    let calls = bodies.iter().map(|(path, body)| post_api(&client, path, body)).collect();
    let statuses = futures_join(calls).await;
    assert!(statuses.iter().all(|s| *s == 200 || *s == 201), "{statuses:?}");
    assert_eq!(stock_of(db, "A-1").await, 50, "ten out, ten in");
}

/// H3: the report ranks the three most valuable products in each category.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn report_ranks_top_three_per_category() {
    let db = fresh_db().await;
    seed(
        db,
        &[
            product("T-1", "tools", 1, 100),
            product("T-2", "tools", 10, 100),
            product("T-3", "tools", 5, 100),
            product("T-4", "tools", 2, 100),
            product("P-1", "paint", 3, 1_000),
            product("P-2", "paint", 0, 5_000),
        ],
    )
    .await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    let rows: Value = client.get("/reports/stock-value").send().await.assert_ok().json();
    let summary: Vec<(String, String, i64, i64)> = rows
        .as_array()
        .expect("array")
        .iter()
        .map(|row| {
            (
                row["category"].as_str().expect("category").to_owned(),
                row["sku"].as_str().expect("sku").to_owned(),
                row["value_cents"].as_i64().expect("value"),
                row["rank"].as_i64().expect("rank"),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("paint".to_owned(), "P-1".to_owned(), 3_000, 1),
            ("paint".to_owned(), "P-2".to_owned(), 0, 2),
            ("tools".to_owned(), "T-2".to_owned(), 1_000, 1),
            ("tools".to_owned(), "T-3".to_owned(), 500, 2),
            ("tools".to_owned(), "T-4".to_owned(), 200, 3),
        ]
    );
}

/// The convention: the product page and the index read through the repository.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pages_list_and_show_products() {
    let db = fresh_db().await;
    seed(db, &[product("A-1", "tools", 4, 250)]).await;
    let client = client(Some(db.pool()), tempfile::tempdir().expect("tempdir").path());

    client
        .get("/")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("A-1")
        .assert_header("cache-control", "no-store");
    client
        .get("/products/A-1")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Product A-1")
        .assert_body_contains("4");
    client.get("/products/NOPE").send().await.assert_status(404);
}

/// H12: the pool reads the password from a file on each new connection. A
/// rotated file takes effect with no restart.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn password_file_is_read_on_each_new_connection() {
    use autumn_web::config::DatabaseConfig;
    use autumn_web::db::DatabasePoolProvider;

    let db = autumn_web::test::TestDb::shared().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("db-password");
    std::fs::write(&file, "wrong\n").expect("write password");

    let config = DatabaseConfig {
        url: Some(db.url().replace("postgres:postgres@", "postgres@")),
        pool_size: 1,
        connect_timeout_secs: 5,
        ..DatabaseConfig::default()
    };
    let pool = PasswordFilePool::new(&file)
        .create_pool(&config)
        .await
        .expect("build pool")
        .expect("a pool");
    assert!(pool.get().await.is_err(), "wrong password must fail");

    std::fs::write(&file, "postgres\n").expect("rotate password");
    let conn = pool.get().await;
    assert!(conn.is_ok(), "rotated password must work: {:?}", conn.err());
}

/// H12: the provider refuses a URL that asks for TLS. Its custom connect
/// step replaces Autumn's TLS setup, so it must not drop TLS without a word.
#[tokio::test]
async fn password_file_refuses_tls_urls() {
    use autumn_web::config::DatabaseConfig;
    use autumn_web::db::DatabasePoolProvider;

    let config = DatabaseConfig {
        url: Some("postgres://app@db.internal/app?sslmode=require".to_owned()),
        ..DatabaseConfig::default()
    };
    let result = PasswordFilePool::new("/run/secrets/db-password")
        .create_pool(&config)
        .await;
    assert!(result.is_err(), "TLS URL must be refused");
}

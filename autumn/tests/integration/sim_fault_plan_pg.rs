//! Authored fault scenarios (`FaultPlan`, issue #1680) — the Postgres database
//! lane.
//!
//! **Requires Docker**; picked up automatically by CI's `--ignored` sweep over
//! the consolidated `integration_tests` binary (see CLAUDE.md).
//!
//! `autumn/tests/sim_fault_plan_db.rs` is the fast, Docker-free proof of the
//! same machinery over the in-memory `SQLite` sim substrate. This file proves
//! the one thing that lane structurally cannot: that the fault interceptor
//! composes *underneath* the harness's own
//! [`DbConnectionInterceptor`](autumn_web::interceptor::DbConnectionInterceptor)
//! — the transactional-test isolation interceptor that hands every checkout the
//! same open, never-committed transaction.
//!
//! That composition is the whole risk. `with_db_interceptor` is documented as
//! "last one wins", so a plan installed the same way would silently replace
//! transactional isolation and quietly turn every Postgres fault test into one
//! that writes committed rows into a shared container. What this test asserts:
//!
//! - **AC2 (DB effect class, targetable by ordinal)** — `fail_db_checkout(2)`
//!   fails exactly the 2nd checkout: request #1 and #3 succeed, #2 is a 503.
//! - **AC4** — that 503 is captured in `FaultOutcome::server_errors` through
//!   `reporting.rs`, and `final_state.db_checkouts` counts all three checkouts
//!   (fired or not).
//! - **AC1** — transactional isolation is still in force *through* the composed
//!   chain: the row inserted by request #1 is visible to request #3, which is
//!   only true when both are handed the same in-transaction connection.

#![cfg(all(feature = "db", feature = "test-support"))]

use autumn_web::prelude::*;
use autumn_web::sim::{FaultEffect, FaultPlan};
use autumn_web::test::{TestApp, TestDb};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

/// The authoring seed. This plan is entirely explicit, so no seed-derived draw
/// happens here — the seed is asserted only as the outcome record's provenance.
const SEED: u64 = 0x1680;

diesel::table! {
    fault_plan_items (id) {
        id -> Int8,
        name -> Text,
    }
}

#[derive(Debug, Queryable, Selectable, serde::Serialize)]
#[diesel(table_name = fault_plan_items)]
struct Item {
    pub id: i64,
    pub name: String,
}

/// Inserts a row. Resolving `Db` checks out a connection, which is the seam the
/// fault interceptor sits on — a fired fault turns this into a 503 *before* the
/// handler body runs, so nothing is inserted.
#[post("/items")]
async fn create_item(mut db: Db) -> AutumnResult<(axum::http::StatusCode, Json<Item>)> {
    let item = diesel::insert_into(fault_plan_items::table)
        .values(fault_plan_items::name.eq("charged"))
        .returning(Item::as_returning())
        .get_result(&mut *db)
        .await?;
    Ok((axum::http::StatusCode::CREATED, Json(item)))
}

/// Reads the rows back. Under transactional test isolation this shares the
/// single open transaction with every other request in the test, so it sees
/// `create_item`'s uncommitted insert.
#[get("/items")]
async fn list_items(mut db: Db) -> AutumnResult<Json<Vec<Item>>> {
    let items = fault_plan_items::table
        .select(Item::as_select())
        .load(&mut *db)
        .await?;
    Ok(Json(items))
}

static SETUP_CELL: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn setup_table(db: &TestDb) {
    SETUP_CELL
        .get_or_init(|| async {
            db.execute_sql(
                "CREATE TABLE IF NOT EXISTS fault_plan_items (
                    id BIGSERIAL PRIMARY KEY,
                    name TEXT NOT NULL
                )",
            )
            .await;
        })
        .await;
}

/// AC2 + AC4 + AC1 over Postgres: the planned checkout ordinal fails, the 503 it
/// produces is captured through `reporting.rs`, and the harness's own
/// transactional-isolation interceptor still works underneath the plan.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fail_db_checkout_fires_on_the_target_ordinal_under_transactional_isolation() {
    let db = TestDb::shared().await;
    setup_table(db).await;

    let client = TestApp::new()
        .routes(routes![create_item, list_items])
        .with_transactional_db(db.url())
        .with_fault_plan(FaultPlan::from_seed(SEED).fail_db_checkout(2))
        .build();

    // Checkout #1: unplanned, so the insert lands in the test transaction.
    client.post("/items").send().await.assert_status(201);
    // Checkout #2: the planned ordinal. The `Db` extractor never resolves, so
    // the handler body never runs and nothing is inserted.
    client.post("/items").send().await.assert_status(503);
    // Checkout #3: unplanned again — and it reads back exactly the one row
    // request #1 wrote, which only holds if both requests were handed the same
    // in-transaction connection.
    client
        .get("/items")
        .send()
        .await
        .assert_ok()
        .assert_json::<Vec<serde_json::Value>, _>(|items| {
            assert_eq!(
                items.len(),
                1,
                "transactional isolation survives the composed fault interceptor: request #1's \
                 uncommitted row is visible, and the faulted request #2 wrote nothing"
            );
        });

    let outcome = client.fault_outcome().await;

    assert_eq!(outcome.seed, SEED);
    assert_eq!(
        outcome.fired.len(),
        1,
        "exactly the planned checkout failed; got {:?}",
        outcome.fired
    );
    let fired = &outcome.fired[0];
    assert_eq!(fired.effect, FaultEffect::DbCheckout);
    assert_eq!(fired.ordinal, 2);
    assert_eq!(fired.target_ordinal, 2);
    assert_eq!(
        fired.target, "primary",
        "the `Db` extractor checks out of the primary pool"
    );
    assert!(outcome.unfired.is_empty());
    assert!(outcome.suppressed.is_empty());

    assert_eq!(
        outcome.final_state.db_checkouts, 3,
        "every checkout is counted, fired or not"
    );

    assert_eq!(
        outcome.server_errors.len(),
        1,
        "the injected 503 reached reporting.rs; got {:?}",
        outcome.server_errors
    );
    let reported = &outcome.server_errors[0];
    assert_eq!(reported.status, 503);
    assert_eq!(reported.method.as_deref(), Some("POST"));
    assert_eq!(reported.route.as_deref(), Some("/items"));
    assert!(
        reported.message.contains("fault plan"),
        "the reported 5xx names the injected fault; got {:?}",
        reported.message
    );
}

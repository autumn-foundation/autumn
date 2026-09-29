//! #1346 (AC5) — the atomic-save correctness gate for nested (`has_many`) forms.
//!
//! A parent (`orders`) and its children (`line_items`, FK `order_id →
//! orders.id`) must be persisted **atomically**: either the whole order and
//! every line item lands, or nothing does. A half-saved order — a parent row
//! with only some of its children, or (worse) orphaned children — is never an
//! acceptable outcome.
//!
//! [`save_order_with_items`] is the canonical save path: it opens **one**
//! [`Db::tx`], inserts the order, reads back its generated `id`, stamps each
//! line item's `order_id` with that id, and inserts the children — all on the
//! single `conn` the closure is handed, with **raw diesel inserts** (never a
//! generated `Repo::create`, which would open its own `Db::tx` and trip the
//! nested-transaction guard).
//!
//! The negative test drives a save whose second child violates the
//! `quantity > 0` CHECK constraint, so the insert errors mid-loop and the whole
//! transaction rolls back. The correctness gate then asserts **zero** rows in
//! *both* tables — no orphaned order, no partial line items. The positive test
//! proves the happy path commits every child linked to the parent's id.
//!
//! Run with (requires Docker for the Postgres testcontainer):
//!
//!     cargo test -p autumn-web --features "db,test-support" \
//!         --test integration_tests nested_form_atomic_save -- --include-ignored

#![cfg(all(feature = "db", feature = "test-support"))]

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestDb};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

// ── Schema ─────────────────────────────────────────────────────────

mod schema {
    diesel::table! {
        atomic_orders (id) {
            id -> Int8,
            name -> Text,
        }
    }

    diesel::table! {
        atomic_line_items (id) {
            id -> Int8,
            order_id -> Int8,
            sku -> Text,
            quantity -> Int4,
        }
    }
}

use schema::{atomic_line_items, atomic_orders};

#[derive(Queryable, Selectable)]
#[diesel(table_name = atomic_orders)]
struct OrderRow {
    id: i64,
    #[allow(dead_code)]
    name: String,
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = atomic_line_items)]
struct LineItemRow {
    #[allow(dead_code)]
    id: i64,
    order_id: i64,
    #[allow(dead_code)]
    sku: String,
    #[allow(dead_code)]
    quantity: i32,
}

// ── The atomic save path (AC5) ─────────────────────────────────────

/// Persist an order and its line items in a **single** transaction.
///
/// Opens one [`Db::tx`], inserts the parent, reads back its generated `id`,
/// stamps each child's `order_id` with that id, and inserts every child with a
/// raw diesel insert on the same `conn`. Any error inside the closure (here, a
/// `quantity > 0` CHECK violation) propagates out and rolls the whole
/// transaction back, so a failed save leaves neither the order nor any line
/// item behind.
async fn save_order_with_items(
    db: &mut Db,
    order_name: String,
    // Each child as `(sku, quantity)` — no `order_id` yet; the parent's id is
    // read back inside the tx and stamped onto every row.
    items: Vec<(String, i32)>,
) -> AutumnResult<i64> {
    db.tx(|conn| {
        async move {
            // 1. Insert the parent, reading back its generated primary key.
            let order_id: i64 = diesel::insert_into(atomic_orders::table)
                .values(atomic_orders::name.eq(&order_name))
                .returning(atomic_orders::id)
                .get_result(conn)
                .await?;

            // 2. Stamp each child's FK with the freshly-read parent id and
            //    insert it. A row that violates the `quantity > 0` CHECK raises
            //    a DB error, which propagates out of the closure and rolls the
            //    whole tx back — including the parent insert above.
            for (sku, quantity) in &items {
                diesel::insert_into(atomic_line_items::table)
                    .values((
                        atomic_line_items::order_id.eq(order_id),
                        atomic_line_items::sku.eq(sku.as_str()),
                        atomic_line_items::quantity.eq(*quantity),
                    ))
                    .execute(conn)
                    .await?;
            }

            Ok::<_, diesel::result::Error>(order_id)
        }
        .scope_boxed()
    })
    .await
}

// ── Handlers ───────────────────────────────────────────────────────
//
// `Db::tx` needs a `Db`, and the only supported way to obtain one in an
// integration test is through the extractor — so the save is driven through a
// route on a non-transactional `TestApp` (`with_db`), where a committed tx
// really persists and a rolled-back tx really leaves nothing.

/// Save an order whose **second** line item is invalid (`quantity = 0`), so the
/// insert trips the CHECK constraint and the whole tx rolls back.
#[post("/orders/rollback")]
async fn rollback_handler(mut db: Db) -> AutumnResult<Json<i64>> {
    let id = save_order_with_items(
        &mut db,
        "Rollback Order".to_string(),
        vec![
            ("A-1".to_string(), 2),
            ("BAD".to_string(), 0), // violates `quantity > 0`
            ("C-3".to_string(), 5),
        ],
    )
    .await?;
    Ok(Json(id))
}

/// Save an order whose line items are all valid, so the tx commits.
#[post("/orders/commit")]
async fn commit_handler(mut db: Db) -> AutumnResult<Json<i64>> {
    let id = save_order_with_items(
        &mut db,
        "Committed Order".to_string(),
        vec![
            ("A-1".to_string(), 2),
            ("B-2".to_string(), 3),
            ("C-3".to_string(), 5),
        ],
    )
    .await?;
    Ok(Json(id))
}

// ── Setup ──────────────────────────────────────────────────────────

/// Serializes the two tests: both assert global row counts on the *shared*
/// tables, so they must not interleave (each truncates at the start of its
/// critical section).
static TABLES_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup_tables(db: &TestDb) {
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS atomic_orders (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL
        )",
    )
    .await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS atomic_line_items (
            id BIGSERIAL PRIMARY KEY,
            order_id BIGINT NOT NULL REFERENCES atomic_orders(id),
            sku TEXT NOT NULL,
            quantity INTEGER NOT NULL CHECK (quantity > 0)
        )",
    )
    .await;
    // Clean slate so the global-count assertions below are meaningful.
    db.execute_sql("TRUNCATE atomic_line_items, atomic_orders RESTART IDENTITY CASCADE")
        .await;
}

// ── Tests ──────────────────────────────────────────────────────────

/// AC5 correctness gate: a failing child rolls the entire order back, leaving
/// **zero** rows in both tables — no orphaned order, no partial line items.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn failing_child_rolls_back_entire_order() {
    let db = TestDb::shared().await;
    let _guard = TABLES_LOCK.lock().await;
    setup_tables(db).await;

    let client = TestApp::new()
        .routes(routes![rollback_handler])
        .with_db(db.pool())
        .build();

    // The save fails partway (invalid second child), so the handler surfaces a
    // 500 and the transaction is rolled back.
    client
        .post("/orders/rollback")
        .send()
        .await
        .assert_status(500);

    // The gate: nothing was persisted in EITHER table.
    let mut conn = db.pool().get().await.expect("db connection");
    let order_count: i64 = atomic_orders::table
        .count()
        .get_result(&mut *conn)
        .await
        .expect("count orders");
    let item_count: i64 = atomic_line_items::table
        .count()
        .get_result(&mut *conn)
        .await
        .expect("count line items");

    assert_eq!(
        order_count, 0,
        "a rolled-back save must leave zero orphaned order rows"
    );
    assert_eq!(
        item_count, 0,
        "a rolled-back save must leave zero orphaned/partial line-item rows"
    );
}

/// The happy path commits: the order is persisted with all N children, each
/// linked to the parent's generated id.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn all_valid_children_commit_linked_to_order() {
    let db = TestDb::shared().await;
    let _guard = TABLES_LOCK.lock().await;
    setup_tables(db).await;

    let client = TestApp::new()
        .routes(routes![commit_handler])
        .with_db(db.pool())
        .build();

    client.post("/orders/commit").send().await.assert_ok();

    let mut conn = db.pool().get().await.expect("db connection");

    let orders: Vec<OrderRow> = atomic_orders::table
        .select(OrderRow::as_select())
        .load(&mut *conn)
        .await
        .expect("load orders");
    assert_eq!(orders.len(), 1, "the order must be committed exactly once");
    let order_id = orders[0].id;

    let items: Vec<LineItemRow> = atomic_line_items::table
        .select(LineItemRow::as_select())
        .load(&mut *conn)
        .await
        .expect("load line items");
    assert_eq!(items.len(), 3, "all three children must be committed");
    assert!(
        items.iter().all(|it| it.order_id == order_id),
        "every child must be linked to the parent's generated id"
    );
}

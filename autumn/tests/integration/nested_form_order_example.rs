//! #1346 (AC7) — end-to-end "order with line items" create flow.
//!
//! This is the worked example for nested (`has_many`) forms: a parent `Order`
//! with a repeating collection of `LineItem` children, from the rendered `new`
//! view through the `create` handler to the atomic save.
//!
//! It lives as an integration test (rather than a standalone `examples/` binary)
//! so `cargo test` compiles and exercises it — the example cannot rot. The
//! view + failed-submit re-render path needs only the `maud` feature and runs
//! in any test run; the persistence half (the `create` handler + one-`Db::tx`
//! save) is gated behind `db` + `test-support` and driven through a real
//! Postgres testcontainer under `--include-ignored`.
//!
//! The flow:
//!
//! 1. A Maud `new` view built from [`form_tag`] + a parent
//!    [`required_text_input`] + [`inputs_for`] (the repeating child rows) +
//!    [`submit_button`]. CSRF/submit-token fields ride on the `<form>` tag.
//! 2. A `create` handler taking `NestedChangesetForm<OrderForm, LineItemForm>`,
//!    calling `.into_valid()`:
//!    - `Ok((order, items))` → save atomically in one `Db::tx` (parent insert,
//!      read back id, stamp each child's FK, insert children).
//!    - `Err(form)` → re-render the same view with per-row errors and the
//!      user's values pre-filled, returning `422`.

#![cfg(feature = "maud")]

use autumn_web::form::{form_tag, required_text_input, submit_button};
use autumn_web::nested_form::{
    InputsForOptions, NestedChangeset, NestedChild, decode_nested_urlencoded, inputs_for,
};

// ── Form types ─────────────────────────────────────────────────────
//
// `Serialize` is required by the parent field helpers (`required_text_input`
// reads the changeset's values back to pre-fill inputs); `Deserialize` +
// `Validate` drive decoding and per-field validation.

#[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
struct OrderForm {
    #[validate(length(min = 1, message = "Order name is required"))]
    name: String,
}

#[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
struct LineItemForm {
    #[validate(length(min = 1, message = "SKU is required"))]
    sku: String,
    #[validate(range(min = 1, message = "Quantity must be at least 1"))]
    quantity: i32,
}

impl NestedChild for LineItemForm {
    const COLLECTION: &'static str = "items";
}

// ── The view ───────────────────────────────────────────────────────

/// Render the order form: parent field, repeating line-item rows, submit.
///
/// Used both for the initial `new` page (a blank-but-present parent, one blank
/// child row) and to re-render after a failed submit, where the same
/// `changeset` carries the user's values and per-row errors.
fn render_order_form(
    changeset: &NestedChangeset<OrderForm, LineItemForm>,
    csrf_token: Option<&str>,
) -> maud::Markup {
    let opts = InputsForOptions {
        // Optional htmx "Add row" endpoint; the no-JS path still works via the
        // pre-rendered blank row `inputs_for` always emits.
        add_row_url: Some("/orders/line-item-row".to_string()),
        ..InputsForOptions::default()
    };
    form_tag(
        "/orders",
        "POST",
        csrf_token,
        maud::html! {
            // Parent field. The CSRF hidden input is emitted by `form_tag`.
            (required_text_input(&changeset.parent, "name", "Order name"))
            // Repeating child rows: each submitted row (pre-filled + inline
            // errors) plus a trailing blank template row.
            (inputs_for(changeset, &opts, |row| maud::html! {
                (row.required_text_input("sku", "SKU"))
                (row.number_input("quantity", "Quantity"))
                (row.destroy_checkbox("Remove"))
            }))
            (submit_button("Create order"))
        },
    )
}

// ── View / re-render tests (maud only, no DB) ──────────────────────

#[test]
fn new_view_renders_the_form_scaffold() {
    // A blank `new` page: parent name present-but-empty, no submitted rows.
    let changeset =
        decode_nested_urlencoded::<OrderForm, LineItemForm>(&[("name".to_string(), String::new())])
            .expect("blank parent decodes");

    let html = render_order_form(&changeset, Some("csrf-token-123")).into_string();

    assert!(html.contains(r#"action="/orders""#), "{html}");
    assert!(html.contains(r#"method="post""#), "{html}");
    // CSRF hidden field emitted by `form_tag`.
    assert!(
        html.contains(r#"name="_csrf" value="csrf-token-123""#),
        "{html}"
    );
    // Parent field and a blank child row.
    assert!(html.contains(r#"name="name""#), "{html}");
    assert!(html.contains(r#"name="items[0][sku]""#), "{html}");
    assert!(html.contains("Create order"), "{html}");
}

#[test]
fn failed_submit_re_renders_per_row_errors_and_prefills_values() {
    // A submission whose SECOND line item is invalid (quantity = 0).
    let submitted = vec![
        ("name".to_string(), "Widgets order".to_string()),
        ("items[0][sku]".to_string(), "A-1".to_string()),
        ("items[0][quantity]".to_string(), "2".to_string()),
        ("items[1][sku]".to_string(), "B-2".to_string()),
        ("items[1][quantity]".to_string(), "0".to_string()),
    ];
    let changeset =
        decode_nested_urlencoded::<OrderForm, LineItemForm>(&submitted).expect("parent decodes");

    // The handler's `into_valid()` would return `Err(form)` here — the child is
    // invalid, so nothing is saved and the view is re-rendered.
    assert!(!changeset.is_valid());
    assert!(
        !changeset.errors_for("items[1].quantity").is_empty(),
        "the invalid row must surface under its combined key"
    );

    let html = render_order_form(&changeset, Some("t")).into_string();

    // The offending row's message renders, scoped to that row's unique id.
    assert!(html.contains("Quantity must be at least 1"), "{html}");
    assert!(html.contains(r#"id="items-1-quantity-error""#), "{html}");
    // The valid sibling row has no quantity error block.
    assert!(!html.contains(r#"id="items-0-quantity-error""#), "{html}");
    // The user's values are pre-filled so nothing is retyped.
    assert!(html.contains(r#"value="A-1""#), "{html}");
    assert!(html.contains(r#"value="B-2""#), "{html}");
}

// ── The create handler + atomic save (DB) ──────────────────────────

#[cfg(all(feature = "db", feature = "test-support"))]
mod persisted {
    use super::{LineItemForm, OrderForm, render_order_form};
    use autumn_web::nested_form::NestedChangesetForm;
    use autumn_web::prelude::*;
    use autumn_web::test::{TestApp, TestDb};
    use axum::http::StatusCode;
    use axum::response::{Html, IntoResponse};
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use scoped_futures::ScopedFutureExt;

    mod schema {
        diesel::table! {
            example_orders (id) {
                id -> Int8,
                name -> Text,
            }
        }
        diesel::table! {
            example_line_items (id) {
                id -> Int8,
                order_id -> Int8,
                sku -> Text,
                quantity -> Int4,
            }
        }
    }
    use schema::{example_line_items, example_orders};

    /// Persist the validated order and its items in one transaction — the same
    /// atomic pattern proven in `nested_form_atomic_save.rs`.
    async fn save_order_with_items(
        db: &mut Db,
        order: OrderForm,
        items: Vec<LineItemForm>,
    ) -> AutumnResult<i64> {
        db.tx(|conn| {
            async move {
                let order_id: i64 = diesel::insert_into(example_orders::table)
                    .values(example_orders::name.eq(&order.name))
                    .returning(example_orders::id)
                    .get_result(conn)
                    .await?;
                for item in &items {
                    diesel::insert_into(example_line_items::table)
                        .values((
                            example_line_items::order_id.eq(order_id),
                            example_line_items::sku.eq(item.sku.as_str()),
                            example_line_items::quantity.eq(item.quantity),
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

    /// `create`: validate the nested form, then save atomically on success or
    /// re-render with per-row errors on failure.
    #[post("/orders")]
    async fn create_order(
        mut db: Db,
        form: NestedChangesetForm<OrderForm, LineItemForm>,
    ) -> axum::response::Response {
        // Capture the CSRF token before `into_valid` consumes the form, so the
        // re-render on the error path can carry it forward.
        let csrf = form.csrf_token().map(str::to_owned);
        match form.into_valid() {
            Ok((order, items)) => match save_order_with_items(&mut db, order, items).await {
                Ok(id) => (StatusCode::CREATED, format!("created order {id}")).into_response(),
                Err(e) => e.into_response(),
            },
            Err(form) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Html(render_order_form(&form.changeset, csrf.as_deref()).into_string()),
            )
                .into_response(),
        }
    }

    /// Two tests share these tables and assert global counts, so serialize them.
    static TABLES_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn setup_tables(db: &TestDb) {
        db.execute_sql(
            "CREATE TABLE IF NOT EXISTS example_orders (
                id BIGSERIAL PRIMARY KEY,
                name TEXT NOT NULL
            )",
        )
        .await;
        db.execute_sql(
            "CREATE TABLE IF NOT EXISTS example_line_items (
                id BIGSERIAL PRIMARY KEY,
                order_id BIGINT NOT NULL REFERENCES example_orders(id),
                sku TEXT NOT NULL,
                quantity INTEGER NOT NULL
            )",
        )
        .await;
        db.execute_sql("TRUNCATE example_line_items, example_orders RESTART IDENTITY CASCADE")
            .await;
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn create_persists_valid_order_with_children() {
        let db = TestDb::shared().await;
        let _guard = TABLES_LOCK.lock().await;
        setup_tables(db).await;

        let client = TestApp::new()
            .routes(routes![create_order])
            .with_db(db.pool())
            .build();

        let body = "name=Widgets+order\
            &items[0][sku]=A-1&items[0][quantity]=2\
            &items[1][sku]=B-2&items[1][quantity]=3";
        client
            .post("/orders")
            .form(body)
            .send()
            .await
            .assert_status(201);

        let mut conn = db.pool().get().await.expect("db connection");
        let orders: i64 = example_orders::table
            .count()
            .get_result(&mut *conn)
            .await
            .expect("count orders");
        let items: i64 = example_line_items::table
            .count()
            .get_result(&mut *conn)
            .await
            .expect("count line items");
        assert_eq!(orders, 1, "the order must be persisted");
        assert_eq!(items, 2, "both line items must be persisted");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn create_rejects_invalid_child_and_persists_nothing() {
        let db = TestDb::shared().await;
        let _guard = TABLES_LOCK.lock().await;
        setup_tables(db).await;

        let client = TestApp::new()
            .routes(routes![create_order])
            .with_db(db.pool())
            .build();

        // Second child quantity = 0 fails validation: no save happens at all.
        let body = "name=Widgets+order\
            &items[0][sku]=A-1&items[0][quantity]=2\
            &items[1][sku]=B-2&items[1][quantity]=0";
        let resp = client.post("/orders").form(body).send().await;
        resp.assert_status(422);
        resp.assert_body_contains("Quantity must be at least 1");

        // Validation ran before any DB work, so nothing is persisted.
        let mut conn = db.pool().get().await.expect("db connection");
        let orders: i64 = example_orders::table
            .count()
            .get_result(&mut *conn)
            .await
            .expect("count orders");
        assert_eq!(orders, 0, "a rejected submission must persist no order");
    }
}

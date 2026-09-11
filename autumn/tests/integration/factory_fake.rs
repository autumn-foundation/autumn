//! Tests for factory `.fake()`, `build_many`, and `create_many` (issue #1343).
//!
//! In-memory tests (`build`/`build_many`) run everywhere. The `create_many`
//! persistence test requires a live Postgres and is `#[ignore]`d like the other
//! DB factory tests.

#[cfg(feature = "db")]
mod fake_factory_tests {
    use autumn_web::fake;

    #[cfg(feature = "test-support")]
    use diesel_async::AsyncPgConnection;
    #[cfg(feature = "test-support")]
    use diesel_async::RunQueryDsl;
    #[cfg(feature = "test-support")]
    use diesel_async::pooled_connection::deadpool::Pool;

    // ── Schema ─────────────────────────────────────────────────
    diesel::table! {
        fake_articles (id) {
            id -> Int8,
            name -> Text,
            first_name -> Text,
            last_name -> Text,
            email -> Text,
            title -> Text,
            body -> Text,
            score -> Int4,
            active -> Bool,
            created_at -> Timestamptz,
            bio -> Nullable<Text>,
        }
    }

    // ── Model ──────────────────────────────────────────────────
    #[autumn_web::model(table = "fake_articles")]
    pub struct FakeArticle {
        #[id]
        pub id: i64,
        pub name: String,
        pub first_name: String,
        pub last_name: String,
        pub email: String,
        pub title: String,
        pub body: String,
        pub score: i32,
        pub active: bool,
        pub created_at: chrono::DateTime<chrono::Utc>,
        pub bio: Option<String>,
    }

    // ── In-memory (.fake().build()) tests ──────────────────────

    #[test]
    fn fake_fills_unset_fields() {
        let _guard = fake::test_serial_guard();
        fake::reseed(1);
        let a = FakeArticle::factory().fake().build();
        assert!(!a.name.is_empty(), "name should be faked");
        assert!(
            !a.first_name.is_empty(),
            "first_name should be faked via fake::first_name()"
        );
        assert!(
            !a.last_name.is_empty(),
            "last_name should be faked via fake::last_name()"
        );
        assert_eq!(a.email.matches('@').count(), 1, "email should be faked");
        assert!(!a.title.is_empty(), "title should be faked");
        assert!(!a.body.is_empty(), "body should be faked");
        assert!(a.bio.is_some(), "Option<String> bio should be Some(..)");
    }

    #[test]
    fn without_fake_fields_stay_default() {
        let a = FakeArticle::factory().build();
        assert_eq!(a.name, "");
        assert_eq!(a.email, "");
        assert_eq!(a.score, 0);
        assert!(!a.active);
        assert_eq!(a.bio, None);
    }

    #[test]
    fn explicit_override_wins_over_fake() {
        let _guard = fake::test_serial_guard();
        fake::reseed(1);
        let a = FakeArticle::factory().fake().title("Fixed Title").build();
        assert_eq!(a.title, "Fixed Title", "explicit set must survive .fake()");
        // Other unset fields are still faked.
        assert!(!a.name.is_empty());
    }

    #[test]
    fn explicit_override_regardless_of_call_order() {
        let _guard = fake::test_serial_guard();
        fake::reseed(1);
        // `.fake()` before the setter.
        let a = FakeArticle::factory().fake().name("Alice").build();
        assert_eq!(a.name, "Alice");
        fake::reseed(1);
        // setter before `.fake()`.
        let b = FakeArticle::factory().name("Bob").fake().build();
        assert_eq!(b.name, "Bob");
    }

    #[test]
    fn fake_is_deterministic_under_seed() {
        let _guard = fake::test_serial_guard();
        fake::reseed(99);
        let a = FakeArticle::factory().fake().build();
        fake::reseed(99);
        let b = FakeArticle::factory().fake().build();
        assert_eq!(a.name, b.name);
        assert_eq!(a.email, b.email);
        assert_eq!(a.title, b.title);
        assert_eq!(a.body, b.body);
        assert_eq!(a.score, b.score);
        assert_eq!(a.active, b.active);
        assert_eq!(a.created_at, b.created_at);
        assert_eq!(a.bio, b.bio);
    }

    #[test]
    fn fake_all_is_an_alias_for_fake() {
        // `.fake_all()` (AC2 spelling) must behave identically to `.fake()`.
        let _guard = fake::test_serial_guard();
        fake::reseed(77);
        let via_fake = FakeArticle::factory().fake().build();
        fake::reseed(77);
        let via_fake_all = FakeArticle::factory().fake_all().build();
        assert_eq!(via_fake.name, via_fake_all.name);
        assert_eq!(via_fake.email, via_fake_all.email);
        assert_eq!(via_fake.title, via_fake_all.title);
        assert_eq!(via_fake.body, via_fake_all.body);
        assert_eq!(via_fake.score, via_fake_all.score);
        assert_eq!(via_fake.bio, via_fake_all.bio);
    }

    #[test]
    fn build_many_with_fake_varies() {
        let _guard = fake::test_serial_guard();
        fake::reseed(5);
        let rows = FakeArticle::factory().fake().build_many(30);
        assert_eq!(rows.len(), 30);
        let distinct_names: std::collections::HashSet<_> =
            rows.iter().map(|r| r.name.clone()).collect();
        assert!(
            distinct_names.len() > 10,
            "expected varied names across the batch, got {}",
            distinct_names.len()
        );
    }

    #[test]
    fn build_many_without_fake_is_identical() {
        let rows = FakeArticle::factory().name("Same").build_many(3);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.name == "Same"));
    }

    #[test]
    fn fake_datetime_anchored_in_deterministic_mode() {
        let _guard = fake::test_serial_guard();
        fake::reseed(3);
        let a = FakeArticle::factory().fake().build();
        // Deterministic mode anchors to 2024-01-01T00:00:00Z minus a <=30d offset.
        let base = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(
            a.created_at <= base,
            "created_at should be at/before the base"
        );
        assert!(
            a.created_at >= base - chrono::Duration::days(31),
            "created_at should be within ~30 days of the base"
        );
    }

    // ── DB (create_many) test ──────────────────────────────────

    #[cfg(feature = "test-support")]
    async fn setup_table(pool: &Pool<AsyncPgConnection>) {
        let mut conn = pool.get().await.unwrap();
        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS fake_articles (
                id         BIGSERIAL PRIMARY KEY,
                name       TEXT NOT NULL DEFAULT '',
                first_name TEXT NOT NULL DEFAULT '',
                last_name  TEXT NOT NULL DEFAULT '',
                email      TEXT NOT NULL DEFAULT '',
                title      TEXT NOT NULL DEFAULT '',
                body       TEXT NOT NULL DEFAULT '',
                score      INT  NOT NULL DEFAULT 0,
                active     BOOL NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                bio        TEXT
            )",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        diesel::sql_query("TRUNCATE fake_articles RESTART IDENTITY")
            .execute(&mut *conn)
            .await
            .unwrap();
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    // The serial guard is held across the DB `.await`s to keep this reseed run
    // isolated from other reseed-based tests sharing the process-global RNG.
    #[allow(clippy::await_holding_lock)]
    async fn create_many_persists_distinct_records() {
        let _guard = fake::test_serial_guard();
        let db: &autumn_web::test::TestDb = autumn_web::test::TestDb::shared().await;
        setup_table(&db.pool()).await;

        fake::reseed(123);
        let rows = FakeArticle::factory()
            .fake()
            .create_many(5, &db.pool())
            .await;

        assert_eq!(rows.len(), 5);
        let distinct_ids: std::collections::HashSet<_> = rows.iter().map(|r| r.id).collect();
        assert_eq!(distinct_ids.len(), 5, "each row gets a distinct DB id");
        for r in &rows {
            assert!(r.id > 0);
            assert!(!r.name.is_empty());
            assert_eq!(r.email.matches('@').count(), 1);
        }
    }

    // ── Decimal shape (issue #2597) ──────────────────────────────────

    diesel::table! {
        fake_invoices (id) {
            id -> Int8,
            amount -> Numeric,
            quantity -> Numeric,
        }
    }

    #[autumn_web::model(table = "fake_invoices")]
    pub struct FakeInvoice {
        #[id]
        pub id: i64,
        #[decimal_shape(precision = 5, scale = 2)]
        pub amount: rust_decimal::Decimal,
        #[decimal_shape(precision = 5, scale = 0)]
        pub quantity: rust_decimal::Decimal,
    }

    #[test]
    fn fake_decimal_respects_declared_shape() {
        // The `#[decimal_shape]` attribute the generator emits must flow
        // through `#[model]` into `fake::decimal_with(p, s)`: every faked
        // value fits the declared `decimal{p,s}` by construction, instead of
        // the untyped `fake::decimal()` that overflows narrow columns ~90%
        // of the time. Seeded, so this cannot pass by luck.
        let _guard = fake::test_serial_guard();
        fake::reseed(2597);
        for _ in 0..200 {
            let inv = FakeInvoice::factory().fake().build();
            // decimal{5,2}: at most 3 integer digits, at most 2 fractional.
            assert!(
                inv.amount < rust_decimal::Decimal::new(1000, 0),
                "amount overflows decimal{{5,2}}: {}",
                inv.amount
            );
            assert!(
                inv.amount.scale() <= 2,
                "amount exceeds scale 2: {}",
                inv.amount
            );
            // decimal{5,0}: whole numbers only, at most 5 integer digits.
            assert_eq!(
                inv.quantity.scale(),
                0,
                "quantity must be whole: {}",
                inv.quantity
            );
            assert!(
                inv.quantity < rust_decimal::Decimal::new(100_000, 0),
                "quantity overflows decimal{{5,0}}: {}",
                inv.quantity
            );
        }
    }
}

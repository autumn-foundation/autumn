//! `Uuid` / `Decimal` / generated-enum model fields on the `SQLite` runtime
//! backend (issue #1924).
//!
//! `uuid::Uuid` and `rust_decimal::Decimal` are foreign to `autumn-web`, so it
//! can implement no diesel conversion for them — not even against a local
//! sql-type, because diesel blanket-implements `AsExpression` for every
//! `Expression`. #1924 solves that with two `TEXT`-backed newtypes,
//! [`autumn_web::db::sqlite_types::SqliteUuid`] and
//! [`autumn_web::db::sqlite_types::SqliteDecimal`], which `autumn generate`
//! renders in place of the wrapped types on a `SQLite` app.
//!
//! A generated `enum{…}` field is the third kind #1924 un-rejects. Its
//! `ToSql`/`FromSql<Text, Sqlite>` impls are emitted by `autumn generate` into
//! the app's own model file (the enum is a local type there), so this test
//! declares the enum exactly as `render_enum_decl` emits it on a `SQLite` app
//! and pins that the shape compiles and round-trips.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_uuid_decimal_conversions`.
#![cfg(feature = "sqlite")]

use std::str::FromStr as _;

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async, rust_decimal};

use autumn_web::db::sqlite_types::{SqliteDecimal, SqliteUuid};
use diesel::{ExpressionMethods as _, QueryDsl as _};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use rust_decimal::Decimal;
use uuid::Uuid;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        catalog_rows (id) {
            id -> Int8,
            // `SqliteUuid`, `SqliteDecimal` and a generated `enum{…}` all sit on
            // plain `TEXT` columns (#1924).
            external_id -> Text,
            owner_id -> Nullable<Text>,
            price -> Text,
            discount -> Nullable<Text>,
            status -> Text,
        }
    }
}

use schema::catalog_rows;

/// Byte-for-byte the shape `autumn generate model` emits for
/// `status:enum{draft,published}` on a `SQLite` app.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    diesel::expression::AsExpression,
    diesel::deserialize::FromSqlRow,
)]
#[diesel(sql_type = diesel::sql_types::Text)]
pub enum Status {
    #[default]
    #[serde(rename = "draft")]
    Draft,
    #[serde(rename = "published")]
    Published,
}

impl Status {
    pub const VARIANTS: [Self; 2] = [Self::Draft, Self::Published];

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Published => "published",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Self::Draft),
            "published" => Ok(Self::Published),
            _ => Err("must be one of draft, published".to_owned()),
        }
    }
}

impl diesel::serialize::ToSql<diesel::sql_types::Text, diesel::sqlite::Sqlite> for Status {
    fn to_sql<'b>(
        &'b self,
        out: &mut diesel::serialize::Output<'b, '_, diesel::sqlite::Sqlite>,
    ) -> diesel::serialize::Result {
        out.set_value(self.as_str());
        Ok(diesel::serialize::IsNull::No)
    }
}

impl diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::sqlite::Sqlite> for Status {
    fn from_sql(
        bytes: <diesel::sqlite::Sqlite as diesel::backend::Backend>::RawValue<'_>,
    ) -> diesel::deserialize::Result<Self> {
        let s = <String as diesel::deserialize::FromSql<
            diesel::sql_types::Text,
            diesel::sqlite::Sqlite,
        >>::from_sql(bytes)?;
        s.parse().map_err(Into::into)
    }
}

#[autumn_web::model]
pub struct CatalogRow {
    #[id]
    pub id: i64,
    pub external_id: SqliteUuid,
    pub owner_id: Option<SqliteUuid>,
    pub price: SqliteDecimal,
    pub discount: Option<SqliteDecimal>,
    pub status: Status,
}

#[autumn_web::repository(CatalogRow)]
pub trait CatalogRowRepository {}

async fn boot_pool(db_name: &str) -> SqlitePool {
    // Shared-cache in-memory database, one distinct name per test so the
    // parallel `#[tokio::test]`s never share rows.
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
        diesel::sql_query(
            "CREATE TABLE catalog_rows (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 external_id TEXT NOT NULL, \
                 owner_id TEXT, \
                 price TEXT NOT NULL, \
                 discount TEXT, \
                 status TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create catalog_rows table");
    }

    pool
}

fn uuid_of(n: u128) -> SqliteUuid {
    SqliteUuid::from(Uuid::from_u128(n))
}

fn decimal_of(s: &str) -> SqliteDecimal {
    SqliteDecimal::from(Decimal::from_str(s).expect("valid decimal"))
}

#[tokio::test]
async fn uuid_decimal_and_enum_fields_round_trip_on_sqlite() {
    let pool = boot_pool("uuid_decimal_roundtrip").await;
    let repo = PgCatalogRowRepository::with_pool_untracked(pool);

    let external_id = uuid_of(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    let owner_id = uuid_of(0xffff_ffff_ffff_ffff_ffff_ffff_ffff_fffe);
    // Trailing-zero scale (`0.10`, not `0.1`) and a 28-digit value prove the
    // TEXT encoding keeps the full `Decimal` representation, unlike `REAL`.
    let price = decimal_of("0.10");
    let discount = decimal_of("-1234567890123456789.0123456");

    let created = repo
        .save(&NewCatalogRow {
            external_id,
            owner_id: Some(owner_id),
            price,
            discount: Some(discount),
            status: Status::Published,
        })
        .await
        .expect("save inserts a row and returns it");
    assert!(created.id > 0, "autoincrement id assigned");

    let found = repo
        .find_by_id(created.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(found.external_id, external_id, "Uuid round-trips exactly");
    assert_eq!(found.owner_id, Some(owner_id), "Option<Uuid> round-trips");
    assert_eq!(found.price, price, "Decimal round-trips exactly");
    assert_eq!(
        found.price.to_string(),
        "0.10",
        "Decimal scale (trailing zero) survives the TEXT round-trip"
    );
    assert_eq!(
        found.discount,
        Some(discount),
        "negative high-precision Option<Decimal> round-trips"
    );
    assert_eq!(found.status, Status::Published, "enum round-trips as TEXT");

    // NULLs round-trip as `None`.
    let sparse = repo
        .save(&NewCatalogRow {
            external_id: uuid_of(7),
            owner_id: None,
            price: SqliteDecimal::default(),
            discount: None,
            status: Status::Draft,
        })
        .await
        .expect("save row with NULL uuid + decimal");
    let reloaded = repo
        .find_by_id(sparse.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(reloaded.owner_id, None, "NULL Uuid round-trips as None");
    assert_eq!(reloaded.discount, None, "NULL Decimal round-trips as None");
    assert_eq!(
        reloaded.price,
        SqliteDecimal::default(),
        "Decimal::ZERO round-trips"
    );
    assert_eq!(reloaded.status, Status::Draft);
}

#[tokio::test]
async fn uuid_decimal_and_enum_columns_are_filterable_on_sqlite() {
    use catalog_rows::dsl as t;

    let pool = boot_pool("uuid_decimal_filter").await;
    let repo = PgCatalogRowRepository::with_pool_untracked(pool.clone());

    let wanted = uuid_of(0xdead_beef);
    for (external_id, price, status) in [
        (wanted, decimal_of("19.99"), Status::Published),
        (uuid_of(1), decimal_of("5.00"), Status::Draft),
    ] {
        repo.save(&NewCatalogRow {
            external_id,
            owner_id: None,
            price,
            discount: None,
            status,
        })
        .await
        .expect("save row");
    }

    let mut conn = pool.get().await.expect("checkout");

    // `.eq(uuid)` proves `Uuid: AsExpression<UuidText>` binds as a parameter.
    let by_uuid: Vec<SqliteUuid> = t::catalog_rows
        .filter(t::external_id.eq(wanted))
        .select(t::external_id)
        .load(&mut *conn)
        .await
        .expect("filter by uuid");
    assert_eq!(by_uuid, vec![wanted], "Uuid binds in a WHERE clause");

    // `.eq(decimal)` proves the same for `Decimal: AsExpression<DecimalText>`.
    let by_decimal: Vec<SqliteDecimal> = t::catalog_rows
        .filter(t::price.eq(decimal_of("19.99")))
        .select(t::price)
        .load(&mut *conn)
        .await
        .expect("filter by decimal");
    assert_eq!(
        by_decimal,
        vec![decimal_of("19.99")],
        "Decimal binds in a WHERE clause"
    );

    // And the generated enum binds through its `Text`/`Sqlite` impls.
    let drafts: Vec<Status> = t::catalog_rows
        .filter(t::status.eq(Status::Draft))
        .select(t::status)
        .load(&mut *conn)
        .await
        .expect("filter by enum");
    assert_eq!(drafts, vec![Status::Draft], "enum binds in a WHERE clause");
}

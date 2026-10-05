//! The Postgres [`CapsuleStore`].
//!
//! Export reads all models in one snapshot, with `to_jsonb`. Import writes the
//! rows with `jsonb_populate_recordset` in one transaction. Export writes
//! `numeric`, `real`, `double precision` and `money` as text, so JSON loses no
//! digit. The SQL quotes all names, and each name is a checked identifier. The
//! subject id is a bound parameter.

use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection as _, AsyncPgConnection, RunQueryDsl as _};

use super::model::{CapsuleModel, DataCapsuleError, FieldSpec, Record, check_ident};
use super::store::{CapsuleFuture, CapsuleStore, ImportBatch, ModelData};

/// A [`CapsuleStore`] on a Postgres pool.
#[derive(Clone)]
pub struct PgCapsuleStore {
    pool: Pool<AsyncPgConnection>,
}

impl std::fmt::Debug for PgCapsuleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCapsuleStore").finish_non_exhaustive()
    }
}

impl PgCapsuleStore {
    /// Make a store on `pool`.
    #[must_use]
    pub const fn new(pool: Pool<AsyncPgConnection>) -> Self {
        Self { pool }
    }

    async fn conn(
        &self,
    ) -> Result<
        diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>,
        DataCapsuleError,
    > {
        self.pool
            .get()
            .await
            .map_err(|e| DataCapsuleError::Store(format!("no database connection: {e}")))
    }
}

/// Quote a checked identifier.
fn quote(name: &str) -> Result<String, DataCapsuleError> {
    check_ident(name)?;
    Ok(format!("\"{name}\""))
}

fn store_error(context: &str, error: &DieselError) -> DataCapsuleError {
    match error {
        // A missing parent is a conflict with the target data, the same as a
        // duplicate key. Neither is a server fault.
        DieselError::DatabaseError(
            DatabaseErrorKind::UniqueViolation | DatabaseErrorKind::ForeignKeyViolation,
            info,
        ) => DataCapsuleError::Conflict(format!("{context}: {}", info.message())),
        other => DataCapsuleError::Store(format!("{context}: {other}")),
    }
}

/// `true` for a type whose JSON number form can lose digits.
fn travels_as_text(data_type: &str) -> bool {
    let base = data_type.trim_end_matches("[]");
    base.starts_with("numeric") || base == "real" || base == "double precision" || base == "money"
}

/// One column, with the type that a domain is based on.
struct Column {
    field: FieldSpec,
    /// The type name, or for a domain the name of its base type.
    base_type: String,
}

/// The select expression of one column.
fn select_expr(column: &Column) -> Result<String, DataCapsuleError> {
    let col = quote(&column.field.name)?;
    // `money::text` depends on `lc_monetary` (for example `$1,234.50`).
    // Through `numeric` the text is a plain number in every locale.
    if column.base_type == "money" {
        return Ok(format!("{col}::numeric::text AS {col}"));
    }
    Ok(if travels_as_text(&column.base_type) {
        let cast = if column.base_type.ends_with("[]") {
            "text[]"
        } else {
            "text"
        };
        format!("{col}::{cast} AS {col}")
    } else {
        col
    })
}

#[derive(diesel::QueryableByName)]
struct ColumnRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    data_type: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    base_type: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    nullable: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    generated: bool,
}

#[derive(diesel::QueryableByName)]
struct RowsJson {
    #[diesel(sql_type = diesel::sql_types::Text)]
    rows: String,
}

#[derive(diesel::QueryableByName)]
struct SequenceRow {
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    seq: Option<String>,
}

async fn describe_table(
    conn: &mut AsyncPgConnection,
    table: &str,
) -> Result<Vec<Column>, DataCapsuleError> {
    let rows: Vec<ColumnRow> = diesel::sql_query(
        // `information_schema.columns.is_generated` exists on every version;
        // `pg_attribute.attgenerated` only from Postgres 12.
        "SELECT a.attname::text AS name, \
                format_type(a.atttypid, a.atttypmod) AS data_type, \
                format_type(CASE WHEN t.typtype = 'd' THEN t.typbasetype \
                                 ELSE a.atttypid END, NULL) AS base_type, \
                NOT a.attnotnull AS nullable, \
                COALESCE(c.is_generated = 'ALWAYS', false) AS generated \
         FROM pg_attribute a \
         JOIN pg_class r ON r.oid = a.attrelid \
         JOIN pg_type t ON t.oid = a.atttypid \
         JOIN pg_namespace n ON n.oid = r.relnamespace \
         LEFT JOIN information_schema.columns c \
           ON c.table_schema = n.nspname AND c.table_name = r.relname \
          AND c.column_name = a.attname \
         WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
    )
    .bind::<diesel::sql_types::Text, _>(quote(table)?)
    .load(conn)
    .await
    .map_err(|e| store_error(&format!("describe {table}"), &e))?;
    if rows.is_empty() {
        return Err(DataCapsuleError::Store(format!(
            "table {table:?} does not exist"
        )));
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut field = FieldSpec::new(r.name, r.data_type);
            field.nullable = r.nullable;
            field.generated = r.generated;
            Column {
                field,
                base_type: r.base_type,
            }
        })
        .collect())
}

fn fields(columns: Vec<Column>) -> Vec<FieldSpec> {
    columns.into_iter().map(|c| c.field).collect()
}

#[derive(diesel::QueryableByName)]
struct CastRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    ok: bool,
}

/// The type name of the subject column of `model`.
fn subject_type<'c>(
    model: &CapsuleModel,
    columns: &'c [Column],
) -> Result<&'c str, DataCapsuleError> {
    columns
        .iter()
        .find(|c| c.field.name == model.subject_column)
        .map(|c| c.field.data_type.as_str())
        .ok_or_else(|| {
            DataCapsuleError::Store(format!(
                "{} has no column {:?}",
                model.table, model.subject_column
            ))
        })
}

/// Check that the subject column type can read `subject`.
///
/// Run it outside a transaction: a failed cast aborts the transaction.
async fn check_subject(
    conn: &mut AsyncPgConnection,
    model: &CapsuleModel,
    columns: &[Column],
    subject: &str,
) -> Result<(), DataCapsuleError> {
    let subject_type = subject_type(model, columns)?;
    let invalid = |reason: String| {
        DataCapsuleError::InvalidInput(format!(
            "subject {subject:?} is not a valid {subject_type} for {}: {reason}",
            model.table
        ))
    };
    let row = diesel::sql_query(format!(
        "SELECT CAST($1 AS {subject_type}) IS NOT NULL AS ok"
    ))
    .bind::<diesel::sql_types::Text, _>(subject)
    .get_result::<CastRow>(conn)
    .await
    .map_err(|e| invalid(e.to_string()))?;
    // A cast that gives `NULL` (a custom type can do this) matches no row.
    if row.ok {
        Ok(())
    } else {
        Err(invalid("the cast gives NULL".to_owned()))
    }
}

/// The records of `model` whose subject column is `subject`.
///
/// The subject is cast to the column type, so `01` finds `1` in a `bigint`
/// column and an index on the column can be used.
async fn fetch_rows(
    conn: &mut AsyncPgConnection,
    model: &CapsuleModel,
    columns: &[Column],
    subject: &str,
) -> Result<Vec<Record>, DataCapsuleError> {
    let subject_type = subject_type(model, columns)?;
    let exprs = columns
        .iter()
        .map(select_expr)
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let pk = quote(&model.primary_key)?;
    // `subject_type` comes from `format_type`, which quotes as SQL needs.
    let sql = format!(
        "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.{pk}), '[]'::jsonb)::text AS rows \
         FROM (SELECT {exprs} FROM {table} WHERE {subject_col} = CAST($1 AS {subject_type})) t",
        table = quote(&model.table)?,
        subject_col = quote(&model.subject_column)?,
    );
    let row: RowsJson = diesel::sql_query(sql)
        .bind::<diesel::sql_types::Text, _>(subject)
        .get_result(conn)
        .await
        .map_err(|e| store_error(&format!("fetch {}", model.table), &e))?;
    serde_json::from_str(&row.rows).map_err(|e| DataCapsuleError::Json {
        file: model.table.clone(),
        message: e.to_string(),
    })
}

impl CapsuleStore for PgCapsuleStore {
    fn describe<'a>(&'a self, model: &'a CapsuleModel) -> CapsuleFuture<'a, Vec<FieldSpec>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            describe_table(&mut conn, &model.table).await.map(fields)
        })
    }

    fn fetch<'a>(
        &'a self,
        model: &'a CapsuleModel,
        subject: &'a str,
    ) -> CapsuleFuture<'a, Vec<Record>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let columns = describe_table(&mut conn, &model.table).await?;
            check_subject(&mut conn, model, &columns, subject).await?;
            fetch_rows(&mut conn, model, &columns, subject).await
        })
    }

    /// Read all models in one `REPEATABLE READ` snapshot, so no record points
    /// at a parent that a later write added or removed.
    fn fetch_subject<'a>(
        &'a self,
        models: &'a [CapsuleModel],
        subject: &'a str,
    ) -> CapsuleFuture<'a, Vec<ModelData>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            for model in models {
                let columns = describe_table(&mut conn, &model.table).await?;
                check_subject(&mut conn, model, &columns, subject).await?;
            }
            conn.transaction::<Vec<ModelData>, DataCapsuleError, _>(async move |conn| {
                diesel::sql_query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                    .execute(conn)
                    .await
                    .map_err(|e| store_error("start snapshot", &e))?;
                let mut data = Vec::with_capacity(models.len());
                for model in models {
                    let columns = describe_table(conn, &model.table).await?;
                    let rows = fetch_rows(conn, model, &columns, subject).await?;
                    data.push((fields(columns), rows));
                }
                Ok(data)
            })
            .await
        })
    }

    fn insert_all<'a>(&'a self, batches: &'a [ImportBatch<'a>]) -> CapsuleFuture<'a, ()> {
        Box::pin(async move {
            let mut statements = Vec::new();
            for batch in batches.iter().filter(|b| !b.records.is_empty()) {
                let rows =
                    serde_json::to_string(batch.records).map_err(|e| DataCapsuleError::Json {
                        file: batch.model.file.clone(),
                        message: e.to_string(),
                    })?;
                statements.push((insert_sql(batch)?, rows, batch));
            }
            let mut conn = self.conn().await?;
            let result: Result<(), DataCapsuleError> = conn
                .transaction::<(), DataCapsuleError, _>(async move |conn| {
                    for (sql, rows, batch) in &statements {
                        diesel::sql_query(sql.as_str())
                            .bind::<diesel::sql_types::Text, _>(rows)
                            .execute(conn)
                            .await
                            .map_err(|e| {
                                store_error(&format!("import {}", batch.model.table), &e)
                            })?;
                        advance_sequence(conn, &batch.model.table, &batch.model.primary_key)
                            .await?;
                    }
                    Ok(())
                })
                .await;
            result
        })
    }
}

impl From<DieselError> for DataCapsuleError {
    fn from(error: DieselError) -> Self {
        store_error("transaction", &error)
    }
}

/// The `INSERT` of one batch. Generated columns are skipped.
fn insert_sql(batch: &ImportBatch<'_>) -> Result<String, DataCapsuleError> {
    let columns = batch
        .model
        .fields
        .iter()
        .filter(|f| !f.generated)
        .map(|f| quote(&f.name))
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return Err(DataCapsuleError::Store(format!(
            "manifest of {} has no columns",
            batch.model.table
        )));
    }
    let table = quote(&batch.model.table)?;
    let money: Vec<&str> = batch
        .model
        .fields
        .iter()
        .filter(|f| !f.generated && f.data_type == "money")
        .map(|f| f.name.as_str())
        .collect();
    if money.is_empty() {
        let columns = columns.join(", ");
        return Ok(format!(
            "INSERT INTO {table} ({columns}) OVERRIDING SYSTEM VALUE \
             SELECT {columns} FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)"
        ));
    }
    // A `money` value arrives as a plain number. Read it through `numeric`, not
    // through the `money` input, which depends on the `lc_monetary` locale.
    let mut exprs = Vec::with_capacity(columns.len());
    for field in batch.model.fields.iter().filter(|f| !f.generated) {
        let col = quote(&field.name)?;
        exprs.push(if field.data_type == "money" {
            format!("(e.j ->> '{}')::numeric::money", field.name)
        } else {
            format!("r.{col}")
        });
    }
    let skip = money
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "INSERT INTO {table} ({columns}) OVERRIDING SYSTEM VALUE \
         SELECT {exprs} FROM jsonb_array_elements($1::jsonb) AS e(j) \
         CROSS JOIN LATERAL jsonb_populate_record(NULL::{table}, e.j - ARRAY[{skip}]::text[]) AS r",
        columns = columns.join(", "),
        exprs = exprs.join(", "),
    ))
}

/// Move a serial or identity sequence past the largest imported key.
async fn advance_sequence(
    conn: &mut AsyncPgConnection,
    table: &str,
    primary_key: &str,
) -> Result<(), DataCapsuleError> {
    let quoted_table = quote(table)?;
    let seq: SequenceRow = diesel::sql_query("SELECT pg_get_serial_sequence($1, $2) AS seq")
        .bind::<diesel::sql_types::Text, _>(&quoted_table)
        .bind::<diesel::sql_types::Text, _>(primary_key)
        .get_result(conn)
        .await
        .map_err(|e| store_error(&format!("sequence of {table}"), &e))?;
    let Some(seq) = seq.seq else {
        return Ok(());
    };
    let pk = quote(primary_key)?;
    diesel::sql_query(format!(
        "SELECT setval($1::regclass, m) \
         FROM (SELECT MAX({pk})::bigint AS m FROM {quoted_table}) s \
         WHERE m IS NOT NULL AND m > COALESCE(pg_sequence_last_value($1::regclass), 0)"
    ))
    .bind::<diesel::sql_types::Text, _>(seq)
    .execute(conn)
    .await
    .map_err(|e| store_error(&format!("advance sequence of {table}"), &e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossy_number_types_travel_as_text() {
        for t in [
            "numeric",
            "numeric(10,2)",
            "real",
            "double precision",
            "money",
            "numeric[]",
        ] {
            assert!(travels_as_text(t), "{t}");
        }
        for t in [
            "bigint",
            "integer",
            "text",
            "jsonb",
            "timestamp with time zone",
        ] {
            assert!(!travels_as_text(t), "{t}");
        }
    }

    #[test]
    fn select_expr_casts_only_lossy_types() {
        let col = |name: &str, base: &str| Column {
            field: FieldSpec::new(name, "domain_name"),
            base_type: base.to_owned(),
        };
        assert_eq!(
            select_expr(&col("n", "numeric")).unwrap(),
            "\"n\"::text AS \"n\""
        );
        assert_eq!(
            select_expr(&col("n", "real[]")).unwrap(),
            "\"n\"::text[] AS \"n\""
        );
        assert_eq!(select_expr(&col("id", "bigint")).unwrap(), "\"id\"");
        assert_eq!(
            select_expr(&col("m", "money")).unwrap(),
            "\"m\"::numeric::text AS \"m\""
        );
        assert!(select_expr(&col("a\"b", "text")).is_err());
    }

    #[test]
    fn insert_sql_skips_generated_columns() {
        let mut model = super::super::ModelManifest {
            table: "t".to_owned(),
            primary_key: "id".to_owned(),
            subject_column: "id".to_owned(),
            fields: vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("total", "numeric").generated(),
            ],
            relationships: Vec::new(),
            blob_columns: Vec::new(),
            record_count: 0,
            file: "records/t.json".to_owned(),
        };
        let records = Vec::new();
        let sql = insert_sql(&ImportBatch {
            model: &model,
            records: &records,
        })
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"t\" (\"id\") OVERRIDING SYSTEM VALUE SELECT \"id\" FROM \
             jsonb_populate_recordset(NULL::\"t\", $1::jsonb)"
        );
        model.fields.push(FieldSpec::new("fee", "money").nullable());
        let sql = insert_sql(&ImportBatch {
            model: &model,
            records: &records,
        })
        .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"t\" (\"id\", \"fee\") OVERRIDING SYSTEM VALUE SELECT r.\"id\", \
             (e.j ->> 'fee')::numeric::money FROM jsonb_array_elements($1::jsonb) AS e(j) \
             CROSS JOIN LATERAL jsonb_populate_record(NULL::\"t\", e.j - ARRAY['fee']::text[]) AS r"
        );
        model.fields.clear();
        assert!(
            insert_sql(&ImportBatch {
                model: &model,
                records: &records,
            })
            .is_err()
        );
    }
}

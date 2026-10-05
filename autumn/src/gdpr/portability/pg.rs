//! The Postgres [`CapsuleStore`].
//!
//! Export reads each row with `to_jsonb`. Import writes the rows with
//! `jsonb_populate_recordset` in one transaction. `numeric`, `real`,
//! `double precision` and `money` travel as text, so no digit is lost in
//! JSON. All names are checked identifiers, quoted in SQL. The subject id is
//! a bound parameter.

use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection as _, AsyncPgConnection, RunQueryDsl as _};

use super::model::{CapsuleError, CapsuleModel, FieldSpec, Record, check_ident};
use super::store::{CapsuleFuture, CapsuleStore, ImportBatch};

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
    ) -> Result<diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>, CapsuleError>
    {
        self.pool
            .get()
            .await
            .map_err(|e| CapsuleError::Store(format!("no database connection: {e}")))
    }
}

/// Quote a checked identifier.
fn quote(name: &str) -> Result<String, CapsuleError> {
    check_ident(name)?;
    Ok(format!("\"{name}\""))
}

fn store_error(context: &str, error: &DieselError) -> CapsuleError {
    match error {
        DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, info) => {
            CapsuleError::Conflict(format!("{context}: {}", info.message()))
        }
        other => CapsuleError::Store(format!("{context}: {other}")),
    }
}

/// `true` for a type whose JSON number form can lose digits.
fn travels_as_text(data_type: &str) -> bool {
    let base = data_type.trim_end_matches("[]");
    base.starts_with("numeric") || base == "real" || base == "double precision" || base == "money"
}

/// The select expression of one column.
fn select_expr(field: &FieldSpec) -> Result<String, CapsuleError> {
    let col = quote(&field.name)?;
    Ok(if travels_as_text(&field.data_type) {
        let cast = if field.data_type.ends_with("[]") {
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
) -> Result<Vec<FieldSpec>, CapsuleError> {
    let rows: Vec<ColumnRow> = diesel::sql_query(
        // `information_schema.columns.is_generated` exists on every version;
        // `pg_attribute.attgenerated` only from Postgres 12.
        "SELECT a.attname::text AS name, \
                format_type(a.atttypid, a.atttypmod) AS data_type, \
                NOT a.attnotnull AS nullable, \
                COALESCE(c.is_generated = 'ALWAYS', false) AS generated \
         FROM pg_attribute a \
         JOIN pg_class r ON r.oid = a.attrelid \
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
        return Err(CapsuleError::Store(format!(
            "table {table:?} does not exist"
        )));
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut field = FieldSpec::new(r.name, r.data_type);
            field.nullable = r.nullable;
            field.generated = r.generated;
            field
        })
        .collect())
}

impl CapsuleStore for PgCapsuleStore {
    fn describe<'a>(&'a self, model: &'a CapsuleModel) -> CapsuleFuture<'a, Vec<FieldSpec>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            describe_table(&mut conn, &model.table).await
        })
    }

    fn fetch<'a>(
        &'a self,
        model: &'a CapsuleModel,
        subject: &'a str,
    ) -> CapsuleFuture<'a, Vec<Record>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let fields = describe_table(&mut conn, &model.table).await?;
            let exprs = fields
                .iter()
                .map(select_expr)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let pk = quote(&model.primary_key)?;
            let sql = format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.{pk}), '[]'::jsonb)::text AS rows \
                 FROM (SELECT {exprs} FROM {table} WHERE {subject_col}::text = $1) t",
                table = quote(&model.table)?,
                subject_col = quote(&model.subject_column)?,
            );
            let row: RowsJson = diesel::sql_query(sql)
                .bind::<diesel::sql_types::Text, _>(subject)
                .get_result(&mut conn)
                .await
                .map_err(|e| store_error(&format!("fetch {}", model.table), &e))?;
            serde_json::from_str(&row.rows).map_err(|e| CapsuleError::Json {
                file: model.table.clone(),
                message: e.to_string(),
            })
        })
    }

    fn insert_all<'a>(&'a self, batches: &'a [ImportBatch<'a>]) -> CapsuleFuture<'a, ()> {
        Box::pin(async move {
            let mut statements = Vec::new();
            for batch in batches.iter().filter(|b| !b.records.is_empty()) {
                let rows =
                    serde_json::to_string(batch.records).map_err(|e| CapsuleError::Json {
                        file: batch.model.file.clone(),
                        message: e.to_string(),
                    })?;
                statements.push((insert_sql(batch)?, rows, batch));
            }
            let mut conn = self.conn().await?;
            let result: Result<(), CapsuleError> = conn
                .transaction::<(), CapsuleError, _>(async move |conn| {
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

impl From<DieselError> for CapsuleError {
    fn from(error: DieselError) -> Self {
        store_error("transaction", &error)
    }
}

/// The `INSERT` of one batch. Generated columns are skipped.
fn insert_sql(batch: &ImportBatch<'_>) -> Result<String, CapsuleError> {
    let columns = batch
        .model
        .fields
        .iter()
        .filter(|f| !f.generated)
        .map(|f| quote(&f.name))
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return Err(CapsuleError::Store(format!(
            "manifest of {} has no columns",
            batch.model.table
        )));
    }
    let columns = columns.join(", ");
    let table = quote(&batch.model.table)?;
    Ok(format!(
        "INSERT INTO {table} ({columns}) OVERRIDING SYSTEM VALUE \
         SELECT {columns} FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)"
    ))
}

/// Move a serial or identity sequence past the largest imported key.
async fn advance_sequence(
    conn: &mut AsyncPgConnection,
    table: &str,
    primary_key: &str,
) -> Result<(), CapsuleError> {
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
        assert_eq!(
            select_expr(&FieldSpec::new("n", "numeric")).unwrap(),
            "\"n\"::text AS \"n\""
        );
        assert_eq!(
            select_expr(&FieldSpec::new("n", "real[]")).unwrap(),
            "\"n\"::text[] AS \"n\""
        );
        assert_eq!(
            select_expr(&FieldSpec::new("id", "bigint")).unwrap(),
            "\"id\""
        );
        assert!(select_expr(&FieldSpec::new("a\"b", "text")).is_err());
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

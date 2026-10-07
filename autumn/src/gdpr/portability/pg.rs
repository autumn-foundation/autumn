//! The Postgres [`CapsuleStore`].
//!
//! Export reads all models in one snapshot, with `to_jsonb`. Import writes the
//! rows with `jsonb_populate_recordset` in one transaction. Export writes
//! `numeric`, `real`, `double precision` and `money` as text, so JSON loses no
//! digit. The SQL quotes all names, and each name is a checked identifier. The
//! subject id is a bound parameter.

use diesel::OptionalExtension as _;
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
    let base = column.base_type.as_str();
    if base == "money" {
        return Ok(format!("{col}::numeric::text AS {col}"));
    }
    if base.ends_with("[]") {
        // `to_jsonb` drops array bounds. An array with bounds other than 1
        // travels as its array literal, for example `[0:2]={1,2,3}`, which
        // import reads back with the bounds.
        let (json, literal) = if base == "money[]" {
            (
                format!("{col}::numeric[]::text[]"),
                format!("{col}::numeric[]::text"),
            )
        } else if travels_as_text(base) {
            (format!("{col}::text[]"), format!("{col}::text"))
        } else {
            (col.clone(), format!("{col}::text"))
        };
        return Ok(format!(
            "CASE WHEN array_dims({col}) IS NULL OR array_dims({col}) ~ '^(\\[1:[0-9]+\\])+$' \
             THEN to_jsonb({json}) ELSE to_jsonb({literal}) END AS {col}"
        ));
    }
    Ok(if travels_as_text(base) {
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
    /// The type name without a modifier. It differs from `base_type` only
    /// for a domain or an array of a domain.
    #[diesel(sql_type = diesel::sql_types::Text)]
    plain_type: String,
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
        // `pg_attribute.attgenerated` only from Postgres 12. The base type
        // follows the domain chain of the column type. When that chain ends
        // at an array, it follows the chain of the element type too and keeps
        // `[]`: `amount[]`, and a domain over `amount[]`, are `numeric[]`.
        // Postgres has one array type per element type, so one level is all.
        "SELECT a.attname::text AS name, \
                format_type(a.atttypid, a.atttypmod) AS data_type, \
                (WITH RECURSIVE outer_chain(oid, base, kind, category, elem) AS ( \
                     SELECT t.oid, t.typbasetype, t.typtype, t.typcategory, t.typelem \
                     UNION ALL \
                     SELECT b.oid, b.typbasetype, b.typtype, b.typcategory, b.typelem \
                     FROM outer_chain c JOIN pg_type b ON b.oid = c.base WHERE c.kind = 'd'), \
                 outer_base AS (SELECT * FROM outer_chain WHERE kind <> 'd' LIMIT 1), \
                 elem_chain(oid, base, kind) AS ( \
                     SELECT e.oid, e.typbasetype, e.typtype \
                     FROM outer_base o JOIN pg_type e ON e.oid = o.elem WHERE o.category = 'A' \
                     UNION ALL \
                     SELECT b.oid, b.typbasetype, b.typtype \
                     FROM elem_chain c JOIN pg_type b ON b.oid = c.base WHERE c.kind = 'd') \
                 SELECT COALESCE( \
                     (SELECT format_type(oid, NULL) || '[]' FROM elem_chain \
                      WHERE kind <> 'd' LIMIT 1), \
                     (SELECT format_type(oid, NULL) FROM outer_base))) AS base_type, \
                format_type(a.atttypid, NULL) AS plain_type, \
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
            // The manifest keeps the base type of a domain, so import can
            // treat a domain over `money` as `money`. A domain over a domain
            // resolves to the last base type.
            if r.base_type != r.plain_type {
                field.base_type = Some(r.base_type.clone());
            }
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
    #[diesel(sql_type = diesel::sql_types::Bool)]
    exact: bool,
}

/// The subject column of `model`.
fn subject_column<'c>(
    model: &CapsuleModel,
    columns: &'c [Column],
) -> Result<&'c Column, DataCapsuleError> {
    columns
        .iter()
        .find(|c| c.field.name == model.subject_column)
        .ok_or_else(|| {
            DataCapsuleError::Store(format!(
                "{} has no column {:?}",
                model.table, model.subject_column
            ))
        })
}

/// Check that the subject column type reads `subject` without a change.
///
/// An explicit cast to a type with a modifier can change the value:
/// `varchar(5)` cuts `ab123-extra` to `ab123`, and `numeric(6, 2)` rounds
/// `1.234` to `1.23`. Then the query finds the records of another subject. So
/// the value after the cast must be equal to the value as the base type
/// without a modifier.
///
/// A failed cast aborts the transaction, so run it last before the query.
async fn check_subject(
    conn: &mut AsyncPgConnection,
    model: &CapsuleModel,
    columns: &[Column],
    subject: &str,
) -> Result<(), DataCapsuleError> {
    let column = subject_column(model, columns)?;
    let subject_type = column.field.data_type.as_str();
    // Without a length, `character` and `bit` mean a length of 1. Compare
    // through the types that have no length limit.
    let bare = match column.base_type.as_str() {
        "character" => "bpchar",
        "bit" => "bit varying",
        other => other,
    };
    let invalid = |reason: String| {
        DataCapsuleError::InvalidInput(format!(
            "subject {subject:?} is not a valid {subject_type} for {}: {reason}",
            model.table
        ))
    };
    let row = diesel::sql_query(format!(
        "SELECT CAST($1 AS {subject_type}) IS NOT NULL AS ok, \
                CAST(CAST($1 AS {subject_type}) AS {bare}) IS NOT DISTINCT FROM \
                CAST($1 AS {bare}) AS exact"
    ))
    .bind::<diesel::sql_types::Text, _>(subject)
    .get_result::<CastRow>(conn)
    .await
    .map_err(|e| invalid(e.to_string()))?;
    // A cast that gives `NULL` (a custom type can do this) matches no row.
    if !row.ok {
        return Err(invalid("the cast gives NULL".to_owned()));
    }
    if !row.exact {
        return Err(invalid("the column type changes the value".to_owned()));
    }
    Ok(())
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
    let subject_type = subject_column(model, columns)?.field.data_type.as_str();
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
            conn.transaction::<Vec<ModelData>, DataCapsuleError, _>(async move |conn| {
                diesel::sql_query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                    .execute(conn)
                    .await
                    .map_err(|e| store_error("start snapshot", &e))?;
                // Lock every table first: no column type can change until the
                // end, so the subject check below stays true for the query.
                for model in models {
                    let table = quote(&model.table)?;
                    diesel::sql_query(format!("LOCK TABLE {table} IN ACCESS SHARE MODE"))
                        .execute(conn)
                        .await
                        .map_err(|e| store_error(&format!("lock {}", model.table), &e))?;
                }
                let mut data = Vec::with_capacity(models.len());
                for model in models {
                    let columns = describe_table(conn, &model.table).await?;
                    // A failed cast stops the transaction, and the export
                    // stops with it: the transaction only reads.
                    check_subject(conn, model, &columns, subject).await?;
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
                    }
                    // A rollback does not undo `setval`, so move the sequences
                    // only after every insert has succeeded. Check deferred
                    // constraints now too: at commit it is too late.
                    diesel::sql_query("SET CONSTRAINTS ALL IMMEDIATE")
                        .execute(conn)
                        .await
                        .map_err(|e| store_error("check deferred constraints", &e))?;
                    // Check every move first: one `setval` that fails after
                    // another one ran would leave a changed sequence.
                    // Each imported column can own a sequence, not only the key.
                    let mut moves = Vec::new();
                    for (_, _, batch) in &statements {
                        for field in batch.model.fields.iter().filter(|f| !f.generated) {
                            if let Some(next) =
                                plan_sequence(conn, &batch.model.table, &field.name).await?
                            {
                                moves.push(next);
                            }
                        }
                    }
                    for (seq, value, table) in moves {
                        diesel::sql_query("SELECT setval($1::regclass, $2)")
                            .bind::<diesel::sql_types::Text, _>(seq)
                            .bind::<diesel::sql_types::BigInt, _>(value)
                            .execute(conn)
                            .await
                            .map_err(|e| {
                                store_error(&format!("advance sequence of {table}"), &e)
                            })?;
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

/// `true` for a `money` or `money[]` column, or a domain over one.
fn is_money(field: &FieldSpec) -> bool {
    matches!(
        field.base_type.as_deref().unwrap_or(&field.data_type),
        "money" | "money[]"
    )
}

/// The locale-free read of a `money` or `money[]` value from `e.j`.
fn money_expr(field: &FieldSpec) -> String {
    let name = &field.name;
    if field.base_type.as_deref().unwrap_or(&field.data_type) == "money" {
        return format!("(e.j ->> '{name}')::numeric::money");
    }
    // The JSON text of a numeric array is an array literal after `[` and `]`
    // become `{` and `}`: the items are plain numbers or `null`. Nested
    // arrays keep their dimensions. A string is an array literal with its
    // bounds. A JSON `null` gives SQL `NULL`.
    format!(
        "CASE jsonb_typeof(e.j -> '{name}') \
         WHEN 'array' THEN translate((e.j -> '{name}')::text, '[]', '{{}}')::numeric[]::money[] \
         WHEN 'string' THEN (e.j ->> '{name}')::numeric[]::money[] END"
    )
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
        .filter(|f| !f.generated && is_money(f))
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
        exprs.push(if is_money(field) {
            money_expr(field)
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

#[allow(clippy::struct_excessive_bools)] // independent checks, one per column of the plan query
#[derive(diesel::QueryableByName)]
struct SequencePlan {
    /// The outermost imported key on the path of the sequence.
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    target: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    cache: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    min: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    max: i64,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    needed: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    can_read: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    can_update: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    cycle: bool,
}

/// The `setval` that moves the serial or identity sequence of `column` past
/// the imported values, as `(sequence, value, column)`, or `None` when the
/// column has no sequence or needs no move.
///
/// # Errors
///
/// [`DataCapsuleError::NotConfigured`] for a sequence with `CACHE` above 1:
/// other sessions can hold cached values, and `setval` does not take them
/// back. The same for a `CYCLE` sequence: it starts again at an imported key.
/// [`DataCapsuleError::Conflict`] for a key outside the sequence range.
async fn plan_sequence(
    conn: &mut AsyncPgConnection,
    table: &str,
    column: &str,
) -> Result<Option<(String, i64, String)>, DataCapsuleError> {
    let quoted_table = quote(table)?;
    // `pg_get_serial_sequence` reads the column name as it is, unquoted.
    let seq: SequenceRow = diesel::sql_query("SELECT pg_get_serial_sequence($1, $2) AS seq")
        .bind::<diesel::sql_types::Text, _>(&quoted_table)
        .bind::<diesel::sql_types::Text, _>(column)
        .get_result(conn)
        .await
        .map_err(|e| store_error(&format!("sequence of {table}.{column}"), &e))?;
    let Some(seq) = seq.seq else {
        return Ok(None);
    };
    let target = format!("{table}.{column}");
    let pk = quote(column)?;
    // The sequence makes only `start + k * inc`, so only a key on that path
    // can be a value it makes. The sequence moves to the outermost such key,
    // in its own direction. A key off the path (500 for `INCREMENT BY 3` from
    // 1) needs no move: moving to the path value before it would only use up
    // values that are still free. An unused sequence has no last value: then
    // compare with the value before its start. The arithmetic is in numeric:
    // `key - start` and `start - inc` can leave the bigint range.
    let plan: Option<SequencePlan> = diesel::sql_query(format!(
        "SELECT s.m AS target, s.cache, s.min, s.max, s.cycle, CASE WHEN s.inc > 0 \
           THEN s.m > COALESCE(s.last, s.start::numeric - s.inc) \
           ELSE s.m < COALESCE(s.last, s.start::numeric - s.inc) END AS needed, \
           s.can_read, has_sequence_privilege($1::regclass, 'UPDATE') AS can_update \
         FROM (SELECT CASE WHEN q.seqincrement > 0 \
                        THEN MAX(t.{pk}) FILTER (WHERE mod(t.{pk}::numeric - q.seqstart, q.seqincrement) = 0) \
                        ELSE MIN(t.{pk}) FILTER (WHERE mod(t.{pk}::numeric - q.seqstart, q.seqincrement) = 0) \
                      END::bigint AS m, \
                      has_sequence_privilege($1::regclass, 'SELECT, USAGE') AS can_read, \
                      CASE WHEN has_sequence_privilege($1::regclass, 'SELECT, USAGE') \
                        THEN pg_sequence_last_value($1::regclass) END AS last, \
                      q.seqincrement AS inc, q.seqstart AS start, q.seqcache AS cache, \
                      q.seqmin AS min, q.seqmax AS max, q.seqcycle AS cycle \
               FROM {quoted_table} t CROSS JOIN pg_sequence q \
               WHERE q.seqrelid = $1::regclass \
               GROUP BY q.seqincrement, q.seqstart, q.seqcache, q.seqmin, q.seqmax, \
                        q.seqcycle) s \
         WHERE s.m IS NOT NULL"
    ))
    .bind::<diesel::sql_types::Text, _>(&seq)
    .get_result(conn)
    .await
    .optional()
    .map_err(|e| store_error(&format!("sequence of {target}"), &e))?;
    let Some(plan) = plan else {
        return Ok(None);
    };
    // Without `SELECT` or `USAGE` the last value is hidden (an error, or
    // `NULL` on some versions). A used sequence would then look unused, and
    // `setval` could move it back onto values it already made.
    if !plan.can_read {
        return Err(DataCapsuleError::Store(format!(
            "the import role has no SELECT or USAGE privilege on the sequence of {target}: \
             import cannot read its last value"
        )));
    }
    if plan.cache > 1 {
        return Err(DataCapsuleError::NotConfigured(format!(
            "the sequence of {target} caches {} values; import needs CACHE 1",
            plan.cache
        )));
    }
    // After its last value, a `CYCLE` sequence starts again, at an imported
    // key.
    if plan.cycle {
        return Err(DataCapsuleError::NotConfigured(format!(
            "the sequence of {target} cycles; import needs NO CYCLE"
        )));
    }
    if !plan.needed {
        return Ok(None);
    }
    // `setval` needs `UPDATE`. Check it now: a later `setval` that fails
    // would leave the earlier ones in place.
    if !plan.can_update {
        return Err(DataCapsuleError::Store(format!(
            "the import role has no UPDATE privilege on the sequence of {target}"
        )));
    }
    if plan.target < plan.min || plan.target > plan.max {
        return Err(DataCapsuleError::Conflict(format!(
            "key {} of {target} is outside its sequence range ({}..{})",
            plan.target, plan.min, plan.max
        )));
    }
    Ok(Some((seq, plan.target, target)))
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
        // An array with bounds other than 1 travels as its array literal.
        assert_eq!(
            select_expr(&col("n", "real[]")).unwrap(),
            "CASE WHEN array_dims(\"n\") IS NULL OR array_dims(\"n\") ~ '^(\\[1:[0-9]+\\])+$' \
             THEN to_jsonb(\"n\"::text[]) ELSE to_jsonb(\"n\"::text) END AS \"n\""
        );
        assert_eq!(
            select_expr(&col("t", "text[]")).unwrap(),
            "CASE WHEN array_dims(\"t\") IS NULL OR array_dims(\"t\") ~ '^(\\[1:[0-9]+\\])+$' \
             THEN to_jsonb(\"t\") ELSE to_jsonb(\"t\"::text) END AS \"t\""
        );
        assert_eq!(select_expr(&col("id", "bigint")).unwrap(), "\"id\"");
        assert_eq!(
            select_expr(&col("m", "money")).unwrap(),
            "\"m\"::numeric::text AS \"m\""
        );
        assert_eq!(
            select_expr(&col("m", "money[]")).unwrap(),
            "CASE WHEN array_dims(\"m\") IS NULL OR array_dims(\"m\") ~ '^(\\[1:[0-9]+\\])+$' \
             THEN to_jsonb(\"m\"::numeric[]::text[]) ELSE to_jsonb(\"m\"::numeric[]::text) END AS \"m\""
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
        // A domain over `money` takes the same locale-free path.
        let mut cash = FieldSpec::new("fee", "cash").nullable();
        cash.base_type = Some("money".to_owned());
        model.fields[2] = cash;
        let sql = insert_sql(&ImportBatch {
            model: &model,
            records: &records,
        })
        .unwrap();
        assert!(sql.contains("(e.j ->> 'fee')::numeric::money"), "{sql}");
        // A `money[]` column is rebuilt element by element, in order.
        model.fields[2] = FieldSpec::new("fee", "money[]").nullable();
        let sql = insert_sql(&ImportBatch {
            model: &model,
            records: &records,
        })
        .unwrap();
        // The JSON text becomes an array literal, so nested arrays keep their
        // dimensions.
        assert!(
            sql.contains(
                "CASE jsonb_typeof(e.j -> 'fee') \
                 WHEN 'array' THEN translate((e.j -> 'fee')::text, '[]', '{}')::numeric[]::money[] \
                 WHEN 'string' THEN (e.j ->> 'fee')::numeric[]::money[] END"
            ),
            "{sql}"
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

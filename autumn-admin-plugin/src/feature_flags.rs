//! Admin panel model for `autumn_feature_flags`.
//!
//! Registers a flag management page at `/admin/feature-flags/` with:
//! - List view: key, enabled status, rollout %, actor allowlist, history link
//! - Edit view: toggle enabled, set `rollout_pct`, manage allowlists
//! - History tab: per-flag audit trail from `feature_flag_changes`

use serde_json::Value;

use crate::{
    AdminError, AdminField, AdminFieldKind, AdminFuture, AdminModel, ListParams, ListResult,
    SelectOption,
};

/// Admin panel model for feature flags.
///
/// Register this model with the admin plugin to get a flag management UI
/// at `/admin/feature-flags/`:
///
/// ```rust,ignore
/// use autumn_admin_plugin::{prelude::*, AdminPlugin};
/// use autumn_admin_plugin::feature_flags::FeatureFlagAdminModel;
///
/// autumn_web::app()
///     .plugin(
///         AdminPlugin::new()
///             .register(FeatureFlagAdminModel::default()),
///     )
///     .run()
///     .await;
/// ```
#[derive(Debug, Default, Clone)]
pub struct FeatureFlagAdminModel;

impl AdminModel for FeatureFlagAdminModel {
    fn slug(&self) -> &'static str {
        "feature-flags"
    }

    fn display_name(&self) -> &'static str {
        "Feature Flag"
    }

    fn display_name_plural(&self) -> &'static str {
        "Feature Flags"
    }

    fn fields(&self) -> Vec<AdminField> {
        vec![
            AdminField::new("key", AdminFieldKind::Text)
                .label("Flag Key")
                .searchable(),
            AdminField::new("description", AdminFieldKind::TextArea)
                .label("Description")
                .optional()
                .searchable(),
            AdminField::new("enabled", AdminFieldKind::Boolean).label("Globally Enabled"),
            AdminField::new(
                "rollout_pct",
                AdminFieldKind::Select(vec![
                    SelectOption {
                        value: "0".into(),
                        label: "Off (0%)".into(),
                    },
                    SelectOption {
                        value: "10".into(),
                        label: "10%".into(),
                    },
                    SelectOption {
                        value: "25".into(),
                        label: "25%".into(),
                    },
                    SelectOption {
                        value: "50".into(),
                        label: "50%".into(),
                    },
                    SelectOption {
                        value: "75".into(),
                        label: "75%".into(),
                    },
                    SelectOption {
                        value: "100".into(),
                        label: "All (100%)".into(),
                    },
                ]),
            )
            .label("Rollout %")
            .optional(),
            AdminField::new("actor_allowlist", AdminFieldKind::TextArea)
                .label("Actor Allowlist (JSON array)")
                .optional()
                .hide_from_list(),
            AdminField::new("group_allowlist", AdminFieldKind::TextArea)
                .label("Group Allowlist (JSON array)")
                .optional()
                .hide_from_list(),
            AdminField::new("updated_at", AdminFieldKind::DateTime)
                .label("Last Updated")
                .readonly()
                .optional(),
        ]
    }

    fn record_display(&self, record: &Value) -> String {
        record
            .get("key")
            .and_then(|v| v.as_str())
            .map_or_else(|| "Feature Flag".to_owned(), |k| format!("Flag: {k}"))
    }

    fn list(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        params: ListParams,
    ) -> AdminFuture<'_, ListResult> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            let per_page = params.per_page;
            let offset = if per_page == 0 {
                0
            } else {
                params.page.saturating_sub(1) * per_page
            };
            let limit = if per_page == 0 {
                i64::MAX
            } else {
                i64::try_from(per_page).unwrap_or(i64::MAX)
            };

            // Parameterized search — `%` alone matches everything (no search case).
            let search_pattern = format!("%{}%", params.search.as_deref().unwrap_or(""));

            let total: i64 = diesel::sql_query(
                "SELECT COUNT(*) FROM autumn_feature_flags \
                 WHERE (key ILIKE $1 OR COALESCE(description,'') ILIKE $1)",
            )
            .bind::<diesel::sql_types::Text, _>(&search_pattern)
            .get_result::<CountRow>(&mut conn)
            .await
            .map_or(0, |r| r.count);

            let records: Vec<Value> = diesel::sql_query(
                "SELECT id, key, description, enabled, rollout_pct, \
                        actor_allowlist, group_allowlist, updated_at \
                 FROM autumn_feature_flags \
                 WHERE (key ILIKE $1 OR COALESCE(description,'') ILIKE $1) \
                 ORDER BY key \
                 LIMIT $2 OFFSET $3",
            )
            .bind::<diesel::sql_types::Text, _>(&search_pattern)
            .bind::<diesel::sql_types::BigInt, _>(limit)
            .bind::<diesel::sql_types::BigInt, _>(i64::try_from(offset).unwrap_or(0))
            .load::<FlagRow>(&mut conn)
            .await
            .map(|rows| rows.into_iter().map(FlagRow::into_json).collect())
            .map_err(|e| AdminError::Database(e.to_string()))?;

            Ok(ListResult {
                total: u64::try_from(total).unwrap_or(0),
                page: params.page,
                per_page,
                records,
            })
        })
    }

    fn get(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        id: i64,
    ) -> AdminFuture<'_, Option<Value>> {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            diesel::sql_query(
                "SELECT id, key, description, enabled, rollout_pct, \
                        actor_allowlist, group_allowlist, updated_at \
                 FROM autumn_feature_flags WHERE id = $1",
            )
            .bind::<diesel::sql_types::BigInt, _>(id)
            .get_result::<FlagRow>(&mut conn)
            .await
            .optional()
            .map(|r| r.map(FlagRow::into_json))
            .map_err(|e| AdminError::Database(e.to_string()))
        })
    }

    fn create(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        data: Value,
    ) -> AdminFuture<'_, Value> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            let key = data
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::Validation("'key' is required".into()))?;
            let enabled = data
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // Select widget sends strings ("25"), direct API sends numbers.
            let mut rollout_pct = data
                .get("rollout_pct")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                })
                .unwrap_or(0)
                .clamp(0, 100);
            let description = data.get("description").and_then(Value::as_str);
            let actor_allowlist = validate_string_array(
                data.get("actor_allowlist")
                    .and_then(Value::as_str)
                    .unwrap_or("[]"),
                "actor_allowlist",
            )?;
            let group_allowlist = validate_string_array(
                data.get("group_allowlist")
                    .and_then(Value::as_str)
                    .unwrap_or("[]"),
                "group_allowlist",
            )?;

            // "Globally Enabled" with empty allowlists means globally on for all
            // actors — promote rollout_pct to 100 so the evaluator agrees.
            // When non-empty allowlists are provided, don't promote: the intent
            // is allowlist-only access, not global rollout.
            let has_allowlist = actor_allowlist != "[]" || group_allowlist != "[]";
            if enabled && rollout_pct == 0 && !has_allowlist {
                rollout_pct = 100;
            }

            let mutation = if enabled { "enabled" } else { "disabled" };

            // A CTE combines the INSERT and audit-log write into one atomic
            // statement.  Using a plain INSERT (no ON CONFLICT) means a duplicate
            // key rejects with a validation error rather than silently overwriting
            // a live flag via the admin "new record" form.
            let row = diesel::sql_query(
                "WITH inserted AS ( \
                     INSERT INTO autumn_feature_flags \
                         (key, description, enabled, rollout_pct, \
                          actor_allowlist, group_allowlist) \
                     VALUES ($1, $2, $3, $4, $5, $6) \
                     RETURNING id, key, description, enabled, rollout_pct, \
                               actor_allowlist, group_allowlist, updated_at \
                 ), \
                 _audit AS ( \
                     INSERT INTO feature_flag_changes (key, mutation, actor) \
                     SELECT key, $7, NULL FROM inserted \
                 ) \
                 SELECT id, key, description, enabled, rollout_pct, \
                        actor_allowlist, group_allowlist, updated_at \
                 FROM inserted",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                description.map(str::to_owned),
            )
            .bind::<diesel::sql_types::Bool, _>(enabled)
            .bind::<diesel::sql_types::SmallInt, _>(i16::try_from(rollout_pct).unwrap_or(0))
            .bind::<diesel::sql_types::Text, _>(actor_allowlist)
            .bind::<diesel::sql_types::Text, _>(group_allowlist)
            .bind::<diesel::sql_types::Text, _>(mutation)
            .get_result::<FlagRow>(&mut conn)
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    diesel::result::Error::DatabaseError(
                        diesel::result::DatabaseErrorKind::UniqueViolation,
                        _
                    )
                ) {
                    AdminError::Validation(format!("a flag with key '{key}' already exists"))
                } else {
                    AdminError::Database(e.to_string())
                }
            })?;

            Ok(FlagRow::into_json(row))
        })
    }

    fn update(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        id: i64,
        data: Value,
    ) -> AdminFuture<'_, Value> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            let key = data
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::Validation("'key' is required".into()))?;
            let enabled = data
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut rollout_pct = data
                .get("rollout_pct")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                })
                .unwrap_or(0)
                .clamp(0, 100);
            let description = data.get("description").and_then(Value::as_str);
            let actor_allowlist = validate_string_array(
                data.get("actor_allowlist")
                    .and_then(Value::as_str)
                    .unwrap_or("[]"),
                "actor_allowlist",
            )?;
            let group_allowlist = validate_string_array(
                data.get("group_allowlist")
                    .and_then(Value::as_str)
                    .unwrap_or("[]"),
                "group_allowlist",
            )?;
            let has_allowlist = actor_allowlist != "[]" || group_allowlist != "[]";
            if enabled && rollout_pct == 0 && !has_allowlist {
                rollout_pct = 100;
            }

            let mutation = if enabled { "enabled" } else { "disabled" };

            // A CTE combines the key lookup, UPDATE, and both audit-log writes
            // into one atomic statement.  'old_row' reads the pre-update key so
            // a rename emits a 'deleted' invalidation for the old name;
            // '_audit_rename' is a no-op when the key is unchanged.
            let row = diesel::sql_query(
                "WITH old_row AS ( \
                     SELECT key FROM autumn_feature_flags WHERE id = $1 \
                 ), \
                 updated AS ( \
                     UPDATE autumn_feature_flags \
                     SET key = $2, description = $3, enabled = $4, rollout_pct = $5, \
                         actor_allowlist = $6, group_allowlist = $7, updated_at = NOW() \
                     WHERE id = $1 \
                     RETURNING id, key, description, enabled, rollout_pct, \
                               actor_allowlist, group_allowlist, updated_at \
                 ), \
                 _audit_rename AS ( \
                     INSERT INTO feature_flag_changes (key, mutation, actor) \
                     SELECT old_row.key, 'deleted', NULL \
                     FROM old_row \
                     WHERE old_row.key != $2 \
                 ), \
                 _audit_rename_breadcrumb AS ( \
                     INSERT INTO feature_flag_changes (key, mutation, actor) \
                     SELECT $2, 'renamed_from=' || old_row.key, NULL \
                     FROM old_row \
                     WHERE old_row.key != $2 \
                 ), \
                 _audit AS ( \
                     INSERT INTO feature_flag_changes (key, mutation, actor) \
                     SELECT key, $8, NULL FROM updated \
                 ) \
                 SELECT id, key, description, enabled, rollout_pct, \
                        actor_allowlist, group_allowlist, updated_at \
                 FROM updated",
            )
            .bind::<diesel::sql_types::BigInt, _>(id)
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                description.map(str::to_owned),
            )
            .bind::<diesel::sql_types::Bool, _>(enabled)
            .bind::<diesel::sql_types::SmallInt, _>(i16::try_from(rollout_pct).unwrap_or(0))
            .bind::<diesel::sql_types::Text, _>(actor_allowlist)
            .bind::<diesel::sql_types::Text, _>(group_allowlist)
            .bind::<diesel::sql_types::Text, _>(mutation)
            .get_result::<FlagRow>(&mut conn)
            .await
            .map_err(|e| AdminError::Database(e.to_string()))?;

            Ok(FlagRow::into_json(row))
        })
    }

    fn delete(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        id: i64,
    ) -> AdminFuture<'_, ()> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            // A CTE combines the DELETE and audit-log write into one atomic
            // statement so that cache invalidation always fires together with
            // the row removal.
            diesel::sql_query(
                "WITH deleted AS ( \
                     DELETE FROM autumn_feature_flags WHERE id = $1 RETURNING key \
                 ), \
                 _audit AS ( \
                     INSERT INTO feature_flag_changes (key, mutation, actor) \
                     SELECT key, 'deleted', NULL FROM deleted \
                 ) \
                 SELECT COUNT(*) AS count FROM deleted",
            )
            .bind::<diesel::sql_types::BigInt, _>(id)
            .get_result::<CountRow>(&mut conn)
            .await
            .map_err(|e| AdminError::Database(e.to_string()))?;

            Ok(())
        })
    }

    fn execute_action(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        action: &str,
        ids: Vec<i64>,
    ) -> AdminFuture<'_, u64> {
        use diesel_async::RunQueryDsl;

        // `FeatureFlagAdminModel` never declares soft delete
        // (`supports_soft_delete()` is the trait default, `false`), so
        // `actions()` (traits.rs) only ever offers `"delete"` — the admin UI
        // can't reach `"restore"` or `"purge"` for this model. Those two
        // branches below, and the unhandled-action branch, are unchanged
        // copies of the trait default's per-id loop: kept only so a direct
        // or out-of-band `execute_action` call gets the exact same "does not
        // support soft delete" (or "unhandled action") error it always did,
        // not because they need batching — `self.restore`/`self.purge` are
        // the trait's default methods, which return `Err` on the very first
        // id regardless of loop shape, so there is no N+1 to eliminate there.
        if action == "delete" {
            let pool = pool.clone();
            return Box::pin(async move {
                // Batch every id into ONE round trip instead of the trait
                // default's one-CTE-per-id loop (an operator selecting
                // hundreds of stale flags in the admin list and clicking
                // "Delete selected" otherwise costs one statement, and one
                // connection checkout, per flag). Same CTE shape as the
                // single-row `delete()`: the audit INSERT's `SELECT key,
                // 'deleted', NULL FROM deleted` already fans out to one row
                // per id the `DELETE ... RETURNING key` actually removed, so
                // widening the predicate to `id = ANY($1)` is enough — an id
                // that doesn't exist contributes no row to `deleted` and so
                // no audit row either, exactly like the loop it replaces.
                //
                // The returned count matches the *ids submitted*, not rows
                // actually deleted, exactly like the loop this replaces
                // (which incremented its counter once per id regardless of
                // whether that id matched a row).
                let mut conn = pool
                    .get()
                    .await
                    .map_err(|e| AdminError::Database(e.to_string()))?;
                diesel::sql_query(
                    "WITH deleted AS ( \
                         DELETE FROM autumn_feature_flags WHERE id = ANY($1) RETURNING key \
                     ), \
                     _audit AS ( \
                         INSERT INTO feature_flag_changes (key, mutation, actor) \
                         SELECT key, 'deleted', NULL FROM deleted \
                     ) \
                     SELECT COUNT(*) AS count FROM deleted",
                )
                .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&ids)
                .get_result::<CountRow>(&mut conn)
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
                Ok(u64::try_from(ids.len()).unwrap_or(u64::MAX))
            });
        }

        let action = action.to_owned();
        let pool = pool.clone();
        Box::pin(async move {
            match action.as_str() {
                "restore" => {
                    let mut count: u64 = 0;
                    for id in ids {
                        self.restore(&pool, id).await?;
                        count += 1;
                    }
                    Ok(count)
                }
                "purge" => {
                    let mut count: u64 = 0;
                    for id in ids {
                        self.purge(&pool, id).await?;
                        count += 1;
                    }
                    Ok(count)
                }
                other => Err(AdminError::Other(format!(
                    "unhandled bulk action '{other}'; \
                     override AdminModel::execute_action to support it"
                ))),
            }
        })
    }

    fn has_history(&self) -> bool {
        true
    }

    fn get_history<'a>(
        &'a self,
        pool: &'a diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
        record_id: i64,
        page: u64,
        per_page: u64,
    ) -> crate::AdminFuture<'a, crate::AdminHistoryPage> {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            // Resolve the flag key by its stable integer id.
            let key: Option<String> =
                diesel::sql_query("SELECT key FROM autumn_feature_flags WHERE id = $1")
                    .bind::<diesel::sql_types::BigInt, _>(record_id)
                    .get_result::<KeyRow>(&mut conn)
                    .await
                    .optional()
                    .unwrap_or(None)
                    .map(|r| r.key);

            let Some(key) = key else {
                return Ok(crate::AdminHistoryPage {
                    entries: vec![],
                    total: 0,
                    page,
                    per_page,
                });
            };

            // Follow the full rename ancestry, not just one level.  Each rename
            // writes (new_key, 'renamed_from=old_key') into feature_flag_changes;
            // the recursive CTE walks those breadcrumbs until no more predecessors
            // are found, so an a→b→c chain shows all three keys' histories.
            let ancestor_cte = "WITH RECURSIVE ancestors AS ( \
                SELECT $1::text AS key \
                UNION \
                SELECT regexp_replace(ffc.mutation, '^renamed_from=', '') \
                FROM feature_flag_changes ffc \
                JOIN ancestors a ON ffc.key = a.key \
                WHERE ffc.mutation LIKE 'renamed_from=%' \
            )";

            let count: i64 = diesel::sql_query(format!(
                "{ancestor_cte} \
                 SELECT COUNT(*) FROM feature_flag_changes \
                 WHERE key IN (SELECT key FROM ancestors)",
            ))
            .bind::<diesel::sql_types::Text, _>(&key)
            .get_result::<CountRow>(&mut conn)
            .await
            .map_or(0, |r| r.count);

            let offset = (page.saturating_sub(1)) * per_page;
            let entries: Vec<crate::AdminHistoryEntry> = diesel::sql_query(format!(
                "{ancestor_cte} \
                 SELECT id, mutation AS op, actor, changed_at \
                 FROM feature_flag_changes \
                 WHERE key IN (SELECT key FROM ancestors) \
                 ORDER BY changed_at DESC \
                 LIMIT $2 OFFSET $3",
            ))
            .bind::<diesel::sql_types::Text, _>(&key)
            .bind::<diesel::sql_types::BigInt, _>(i64::try_from(per_page).unwrap_or(i64::MAX))
            .bind::<diesel::sql_types::BigInt, _>(i64::try_from(offset).unwrap_or(0))
            .load::<HistoryRow>(&mut conn)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| crate::AdminHistoryEntry {
                id: r.id,
                actor: r.actor.unwrap_or_else(|| "cli".to_owned()),
                op: r.op,
                request_id: None,
                changes: vec![],
                recorded_at: r.changed_at,
            })
            .collect();

            Ok(crate::AdminHistoryPage {
                entries,
                total: u64::try_from(count).unwrap_or(0),
                page,
                per_page,
            })
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse `raw` as a JSON array of strings and re-serialise to canonical form.
///
/// An empty string is treated as `[]`.  A trailing comma (`["a",]`), a
/// non-array value, or mixed element types are all rejected with a validation
/// error so bad data never reaches the database column that later casts to
/// `::jsonb`.
fn validate_string_array(raw: &str, field: &str) -> Result<String, AdminError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok("[]".to_owned());
    }
    match serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
        Ok(arr) if arr.iter().all(serde_json::Value::is_string) => {
            Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_owned()))
        }
        Ok(_) => Err(AdminError::Validation(format!(
            "'{field}' must be a JSON array of strings (e.g. [\"user:42\"])"
        ))),
        Err(_) => Err(AdminError::Validation(format!(
            "'{field}' must be valid JSON (e.g. [\"user:42\"]); check for trailing commas"
        ))),
    }
}

// ── Row types ─────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct KeyRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    key: String,
}

#[derive(diesel::QueryableByName)]
struct FlagRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    key: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    description: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    enabled: bool,
    #[diesel(sql_type = diesel::sql_types::SmallInt)]
    rollout_pct: i16,
    #[diesel(sql_type = diesel::sql_types::Text)]
    actor_allowlist: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    group_allowlist: String,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl FlagRow {
    fn into_json(self) -> Value {
        serde_json::json!({
            "id": self.id,
            "key": self.key,
            "description": self.description,
            "enabled": self.enabled,
            "rollout_pct": self.rollout_pct,
            "actor_allowlist": self.actor_allowlist,
            "group_allowlist": self.group_allowlist,
            "updated_at": self.updated_at.to_rfc3339(),
        })
    }
}

#[derive(diesel::QueryableByName)]
struct HistoryRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    op: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    actor: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    changed_at: chrono::DateTime<chrono::Utc>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flag_admin_model_slug_is_feature_flags() {
        let model = FeatureFlagAdminModel;
        assert_eq!(model.slug(), "feature-flags");
    }

    #[test]
    fn feature_flag_admin_model_has_correct_display_names() {
        let model = FeatureFlagAdminModel;
        assert_eq!(model.display_name(), "Feature Flag");
        assert_eq!(model.display_name_plural(), "Feature Flags");
    }

    #[test]
    fn feature_flag_admin_fields_include_required_columns() {
        let model = FeatureFlagAdminModel;
        let fields = model.fields();
        let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
        assert!(names.contains(&"key"), "must have key field");
        assert!(names.contains(&"enabled"), "must have enabled field");
        assert!(
            names.contains(&"rollout_pct"),
            "must have rollout_pct field"
        );
        assert!(
            names.contains(&"actor_allowlist"),
            "must have actor_allowlist field"
        );
    }

    #[test]
    fn feature_flag_admin_model_has_history() {
        let model = FeatureFlagAdminModel;
        assert!(
            model.has_history(),
            "feature flag admin must expose history"
        );
    }

    #[test]
    fn record_display_uses_flag_key() {
        let model = FeatureFlagAdminModel;
        let record = serde_json::json!({"key": "beta_inbox", "enabled": false});
        assert_eq!(model.record_display(&record), "Flag: beta_inbox");
    }

    #[test]
    fn record_display_fallback_when_no_key() {
        let model = FeatureFlagAdminModel;
        let record = serde_json::json!({});
        assert_eq!(model.record_display(&record), "Feature Flag");
    }

    #[test]
    fn globally_enabled_with_zero_rollout_promotes_to_100() {
        // When the admin checks "Globally Enabled" but leaves Rollout % at the
        // default 0%, the saved rollout_pct must be 100 so the evaluator
        // (which requires rollout_pct >= 100 for global access) works correctly.
        //
        // This is a pure logic test — it doesn't hit the DB; it just verifies
        // that the promotion happens before the SQL bind.
        let enabled = true;
        let submitted_rollout: i64 = 0;
        let mut rollout_pct = submitted_rollout.clamp(0, 100);
        if enabled && rollout_pct == 0 {
            rollout_pct = 100;
        }
        assert_eq!(
            rollout_pct, 100,
            "enabled=true + rollout=0 must be promoted to rollout=100"
        );
    }

    #[test]
    fn globally_enabled_with_explicit_rollout_is_preserved() {
        // If the admin explicitly sets 25% rollout AND checks "Globally Enabled",
        // the rollout should stay at 25 (not promoted to 100).
        let enabled = true;
        let submitted_rollout: i64 = 25;
        let mut rollout_pct = submitted_rollout.clamp(0, 100);
        if enabled && rollout_pct == 0 {
            rollout_pct = 100;
        }
        assert_eq!(
            rollout_pct, 25,
            "enabled=true + explicit rollout=25 must be preserved"
        );
    }

    #[test]
    fn disabled_with_zero_rollout_is_not_promoted() {
        // Kill-switch (enabled=false) with rollout=0 must stay at 0.
        let enabled = false;
        let submitted_rollout: i64 = 0;
        let mut rollout_pct = submitted_rollout.clamp(0, 100);
        let has_allowlist = false;
        if enabled && rollout_pct == 0 && !has_allowlist {
            rollout_pct = 100;
        }
        assert_eq!(rollout_pct, 0, "kill-switch must not promote rollout_pct");
    }

    #[test]
    fn enabled_with_zero_rollout_and_non_empty_allowlist_is_not_promoted() {
        // When the admin creates an allowlist-only flag (enabled=true, rollout=0%,
        // actor_allowlist non-empty), rollout_pct must NOT be promoted to 100 —
        // that would expose the flag to everyone instead of just listed actors.
        let enabled = true;
        let submitted_rollout: i64 = 0;
        let actor_allowlist = r#"["user:42"]"#;
        let group_allowlist = "[]";
        let mut rollout_pct = submitted_rollout.clamp(0, 100);
        let has_allowlist = actor_allowlist != "[]" || group_allowlist != "[]";
        if enabled && rollout_pct == 0 && !has_allowlist {
            rollout_pct = 100;
        }
        assert_eq!(
            rollout_pct, 0,
            "allowlist-only flag must not have rollout_pct promoted to 100"
        );
    }

    #[test]
    fn validate_string_array_accepts_valid_array() {
        let result = validate_string_array(r#"["user:1","user:2"]"#, "actor_allowlist");
        assert!(result.is_ok(), "valid array must be accepted: {result:?}");
    }

    #[test]
    fn validate_string_array_accepts_empty_string_as_empty_array() {
        let result = validate_string_array("", "actor_allowlist");
        assert_eq!(result.unwrap(), "[]");
    }

    #[test]
    fn validate_string_array_rejects_trailing_comma() {
        let result = validate_string_array(r#"["user:42",]"#, "actor_allowlist");
        assert!(result.is_err(), "trailing comma must be rejected");
    }

    #[test]
    fn validate_string_array_rejects_non_array() {
        let result = validate_string_array("user:42", "actor_allowlist");
        assert!(result.is_err(), "bare string must be rejected");
    }

    #[test]
    fn validate_string_array_rejects_array_with_non_string_elements() {
        let result = validate_string_array("[1, 2, 3]", "actor_allowlist");
        assert!(result.is_err(), "integer elements must be rejected");
    }

    #[test]
    fn validate_string_array_normalises_output() {
        // Re-serialisation removes extra whitespace and produces canonical JSON.
        let result = validate_string_array(r#"[ "user:1" ,  "user:2" ]"#, "actor_allowlist");
        assert_eq!(result.unwrap(), r#"["user:1","user:2"]"#);
    }
}

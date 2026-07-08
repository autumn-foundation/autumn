//! Admin panel model for scoped API tokens (`api_tokens`).
//!
//! Registers a management page at `/admin/api-tokens/` to create, list, and
//! revoke scoped service tokens (issue #1158). The raw token is shown **once**
//! at creation and is never stored or re-displayed; listing exposes only
//! non-secret metadata (name, principal, scopes, expiry, last-used, revoked).

use autumn_web::auth::{generate_raw_token, hash_api_token, scopes_from_json};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

use crate::{
    AdminError, AdminField, AdminFieldKind, AdminFuture, AdminModel, ListParams, ListResult,
};

/// Admin panel model for scoped API tokens.
///
/// Register with the admin plugin to get a token management UI at
/// `/admin/api-tokens/`:
///
/// ```rust,ignore
/// use autumn_admin_plugin::{AdminPlugin, tokens::TokenAdminModel};
///
/// autumn_web::app()
///     .plugin(AdminPlugin::new().register(TokenAdminModel::default()))
///     .run()
///     .await;
/// ```
///
/// Requires the managed `api_tokens` table (run `autumn migrate`).
#[derive(Debug, Default, Clone)]
pub struct TokenAdminModel;

impl AdminModel for TokenAdminModel {
    fn slug(&self) -> &'static str {
        "api-tokens"
    }

    fn display_name(&self) -> &'static str {
        "API Token"
    }

    fn display_name_plural(&self) -> &'static str {
        "API Tokens"
    }

    fn fields(&self) -> Vec<AdminField> {
        vec![
            AdminField::new("name", AdminFieldKind::Text)
                .label("Name")
                .searchable(),
            AdminField::new("principal_id", AdminFieldKind::Text)
                .label("Principal")
                .searchable()
                .create_only(),
            AdminField::new("scopes", AdminFieldKind::TextArea)
                .label("Scopes (JSON array)")
                .optional(),
            AdminField::new("expires_at", AdminFieldKind::DateTime)
                .label("Expires At (UTC)")
                .optional()
                .create_only(),
            AdminField::new("last_used_at", AdminFieldKind::DateTime)
                .label("Last Used")
                .readonly()
                .optional()
                .hide_from_list(),
            AdminField::new("revoked_at", AdminFieldKind::DateTime)
                .label("Revoked At")
                .readonly()
                .optional(),
            // Only populated in the response from `create` — the raw token,
            // surfaced once at issuance and never stored or re-displayed.
            AdminField::new("token", AdminFieldKind::Text)
                .label("Token (copy now — shown once)")
                .readonly()
                .optional()
                .hide_from_list(),
        ]
    }

    fn record_display(&self, record: &Value) -> String {
        record
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .map_or_else(|| "API Token".to_owned(), |n| format!("Token: {n}"))
    }

    fn list(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
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
            let search_pattern = format!("%{}%", params.search.as_deref().unwrap_or(""));

            let total: i64 = diesel::sql_query(
                "SELECT COUNT(*) FROM api_tokens \
                 WHERE (name ILIKE $1 OR principal_id ILIKE $1)",
            )
            .bind::<diesel::sql_types::Text, _>(&search_pattern)
            .get_result::<CountRow>(&mut conn)
            .await
            .map_or(0, |r| r.count);

            let records: Vec<Value> = diesel::sql_query(
                "SELECT id, name, principal_id, scopes::text AS scopes, created_at, \
                        expires_at, last_used_at, revoked_at \
                 FROM api_tokens \
                 WHERE (name ILIKE $1 OR principal_id ILIKE $1) \
                 ORDER BY id DESC \
                 LIMIT $2 OFFSET $3",
            )
            .bind::<diesel::sql_types::Text, _>(&search_pattern)
            .bind::<diesel::sql_types::BigInt, _>(limit)
            .bind::<diesel::sql_types::BigInt, _>(i64::try_from(offset).unwrap_or(0))
            .load::<TokenRow>(&mut conn)
            .await
            .map(|rows| rows.into_iter().map(TokenRow::into_json).collect())
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
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        id: i64,
    ) -> AdminFuture<'_, Option<Value>> {
        use diesel::OptionalExtension as _;
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            diesel::sql_query(
                "SELECT id, name, principal_id, scopes::text AS scopes, created_at, \
                        expires_at, last_used_at, revoked_at \
                 FROM api_tokens WHERE id = $1",
            )
            .bind::<diesel::sql_types::BigInt, _>(id)
            .get_result::<TokenRow>(&mut conn)
            .await
            .optional()
            .map(|r| r.map(TokenRow::into_json))
            .map_err(|e| AdminError::Database(e.to_string()))
        })
    }

    fn create(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        data: Value,
    ) -> AdminFuture<'_, Value> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let principal_id = data
                .get("principal_id")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| AdminError::Validation("'principal_id' is required".into()))?;
            let name = data.get("name").and_then(Value::as_str).unwrap_or("");
            let scopes = parse_scopes(data.get("scopes"))?;
            let expires_at = parse_expires_at(data.get("expires_at"))?;

            let raw = generate_raw_token();
            let hash = hash_api_token(&raw);
            let scopes_json = serde_json::Value::Array(
                scopes
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            )
            .to_string();
            let expires_at_naive = expires_at.map(|dt| dt.naive_utc());

            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            // Atomic INSERT RETURNING: the row's server-assigned columns (id,
            // created_at, …) come back in the same round-trip as the INSERT so
            // there is no window in which the token exists in the DB but its
            // metadata has not been returned to the caller.
            let row: TokenRow = diesel::sql_query(
                "INSERT INTO api_tokens (token_hash, principal_id, name, scopes, expires_at) \
                 VALUES ($1, $2, $3, $4::jsonb, $5) \
                 RETURNING id, name, principal_id, scopes::text AS scopes, \
                           created_at, expires_at, last_used_at, revoked_at",
            )
            .bind::<diesel::sql_types::Text, _>(&hash)
            .bind::<diesel::sql_types::Text, _>(principal_id)
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::Text, _>(&scopes_json)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Timestamp>, _>(expires_at_naive)
            .get_result::<TokenRow>(&mut conn)
            .await
            .map_err(|e| AdminError::Database(e.to_string()))?;

            let mut json = row.into_json();
            if let Value::Object(map) = &mut json {
                map.insert("token".to_owned(), Value::String(raw));
            }
            Ok(json)
        })
    }

    fn update(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        id: i64,
        data: Value,
    ) -> AdminFuture<'_, Value> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            // A token's secret/principal are immutable; only the human-readable
            // name and granted scopes are editable after issuance.
            let name = data.get("name").and_then(Value::as_str).unwrap_or("");
            let scopes = parse_scopes(data.get("scopes"))?;
            let scopes_json =
                serde_json::Value::Array(scopes.into_iter().map(Value::String).collect())
                    .to_string();

            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            diesel::sql_query("UPDATE api_tokens SET name = $1, scopes = $2::jsonb WHERE id = $3")
                .bind::<diesel::sql_types::Text, _>(name)
                .bind::<diesel::sql_types::Text, _>(&scopes_json)
                .bind::<diesel::sql_types::BigInt, _>(id)
                .execute(&mut conn)
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            self.get(&pool, id)
                .await?
                .ok_or_else(|| AdminError::Validation(format!("token {id} not found")))
        })
    }

    fn delete(
        &self,
        pool: &diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        id: i64,
    ) -> AdminFuture<'_, ()> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            // "Delete" a token means revoke it: keep the audit row, stop it
            // authenticating. Idempotent — re-revoking is a no-op.
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            diesel::sql_query(
                "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
                 WHERE id = $1 AND revoked_at IS NULL",
            )
            .bind::<diesel::sql_types::BigInt, _>(id)
            .execute(&mut conn)
            .await
            .map_err(|e| AdminError::Database(e.to_string()))?;
            Ok(())
        })
    }
}

/// Parse the `scopes` form value (JSON-array text or a JSON array) into a flat
/// list of scope strings.
fn parse_scopes(value: Option<&Value>) -> Result<Vec<String>, AdminError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v @ Value::Array(_)) => Ok(scopes_from_json(v)),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(Vec::new());
            }
            let parsed: Vec<String> = serde_json::from_str(trimmed).map_err(|_| {
                AdminError::Validation(
                    "'scopes' must be a JSON array of strings, e.g. [\"posts:read\"]".into(),
                )
            })?;
            Ok(parsed)
        }
        Some(_) => Err(AdminError::Validation(
            "'scopes' must be a JSON array of strings".into(),
        )),
    }
}

/// Parse the optional `expires_at` form value into a UTC instant.
fn parse_expires_at(value: Option<&Value>) -> Result<Option<DateTime<Utc>>, AdminError> {
    let Some(s) = value.and_then(Value::as_str) else {
        return Ok(None);
    };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(Some(dt.with_timezone(&Utc)));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(Some(DateTime::from_naive_utc_and_offset(naive, Utc)));
        }
    }
    Err(AdminError::Validation(format!(
        "'expires_at' is not a valid timestamp: {s}"
    )))
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct TokenRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    principal_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    scopes: String,
    #[diesel(sql_type = diesel::sql_types::Timestamp)]
    created_at: NaiveDateTime,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamp>)]
    expires_at: Option<NaiveDateTime>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamp>)]
    last_used_at: Option<NaiveDateTime>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamp>)]
    revoked_at: Option<NaiveDateTime>,
}

impl TokenRow {
    fn into_json(self) -> Value {
        let scopes: Value = serde_json::from_str(&self.scopes).unwrap_or(Value::Array(Vec::new()));
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "principal_id": self.principal_id,
            "scopes": scopes,
            "created_at": self.created_at.and_utc().to_rfc3339(),
            "expires_at": self.expires_at.map(|d| d.and_utc().to_rfc3339()),
            "last_used_at": self.last_used_at.map(|d| d.and_utc().to_rfc3339()),
            "revoked_at": self.revoked_at.map(|d| d.and_utc().to_rfc3339()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_metadata() {
        let m = TokenAdminModel;
        assert_eq!(m.slug(), "api-tokens");
        assert_eq!(m.display_name_plural(), "API Tokens");
        let field_names: Vec<_> = m.fields().iter().map(|f| f.name).collect();
        assert!(field_names.contains(&"scopes"));
        assert!(field_names.contains(&"principal_id"));
        // No secret column is ever a field except the one-time create token.
        assert!(!field_names.contains(&"token_hash"));
    }

    #[test]
    fn record_display_uses_name_when_present() {
        let m = TokenAdminModel;
        let with_name = serde_json::json!({"name": "ci-token"});
        assert_eq!(m.record_display(&with_name), "Token: ci-token");
    }

    #[test]
    fn record_display_falls_back_when_name_missing_or_empty() {
        let m = TokenAdminModel;
        assert_eq!(m.record_display(&serde_json::json!({})), "API Token");
        assert_eq!(
            m.record_display(&serde_json::json!({"name": ""})),
            "API Token"
        );
    }

    #[test]
    fn parse_scopes_accepts_json_text_and_arrays() {
        assert_eq!(parse_scopes(None).unwrap(), Vec::<String>::new());
        // Explicit JSON null → empty (same as absent)
        assert_eq!(
            parse_scopes(Some(&Value::Null)).unwrap(),
            Vec::<String>::new()
        );
        // Empty string → empty
        assert_eq!(
            parse_scopes(Some(&Value::String(String::new()))).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_scopes(Some(&Value::String("[\"a\",\"b\"]".into()))).unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(
            parse_scopes(Some(&serde_json::json!(["x"]))).unwrap(),
            vec!["x".to_owned()]
        );
        assert!(parse_scopes(Some(&Value::String("not json".into()))).is_err());
        // Non-string, non-array, non-null → error
        assert!(parse_scopes(Some(&Value::Bool(true))).is_err());
    }

    #[test]
    fn parse_expires_at_handles_formats_and_empty() {
        assert!(parse_expires_at(None).unwrap().is_none());
        assert!(
            parse_expires_at(Some(&Value::String(String::new())))
                .unwrap()
                .is_none()
        );
        // RFC3339 with timezone
        assert!(
            parse_expires_at(Some(&Value::String("2026-12-31T23:59:59Z".into())))
                .unwrap()
                .is_some()
        );
        // Without timezone (naive datetime format)
        assert!(
            parse_expires_at(Some(&Value::String("2026-12-31T23:59:59".into())))
                .unwrap()
                .is_some()
        );
        // Minute-precision format
        assert!(
            parse_expires_at(Some(&Value::String("2026-12-31T23:59".into())))
                .unwrap()
                .is_some()
        );
        assert!(parse_expires_at(Some(&Value::String("nonsense".into()))).is_err());
    }

    #[test]
    fn token_row_into_json_omits_secret_and_parses_scopes() {
        let row = TokenRow {
            id: 7,
            name: "ci".into(),
            principal_id: "service:ci".into(),
            scopes: "[\"posts:read\"]".into(),
            created_at: NaiveDateTime::default(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        let json = row.into_json();
        assert_eq!(json["id"], 7);
        assert_eq!(json["scopes"], serde_json::json!(["posts:read"]));
        assert!(json.get("token_hash").is_none());
        assert!(json.get("token").is_none());
    }
}

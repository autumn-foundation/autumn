//! `autumn config` — inspect and mutate runtime configuration values.
//!
//! All commands connect directly to the configured Postgres database over a
//! native `tokio_postgres` connection (see issue #1243) — no `psql` binary
//! required — following the same profile-aware URL-resolution strategy as
//! the app.
//!
//! # Commands
//!
//! ```text
//! autumn config list                    # list all overrides (key, value, updated_at)
//! autumn config get <key>               # print the current stored value for a key
//! autumn config set <key> <value>       # set (or replace) a key
//! autumn config unset <key>             # remove the override, restoring the default
//! autumn config history <key>           # show the change history for a key
//! ```

use autumn_web::config::{AutumnConfig, Env, OsEnv};

use crate::pg;

/// Options for `autumn config list`.
pub struct ListOptions;

/// Options for `autumn config get <key>`.
pub struct GetOptions {
    pub key: String,
}

/// Options for `autumn config set <key> <value>`.
pub struct SetOptions {
    pub key: String,
    pub value: String,
    pub actor: Option<String>,
}

/// Options for `autumn config unset <key>`.
pub struct UnsetOptions {
    pub key: String,
    pub actor: Option<String>,
}

/// Options for `autumn config history <key>`.
pub struct HistoryOptions {
    pub key: String,
    pub limit: usize,
}

const LIST_SQL: &str = "SELECT key, raw_value, updated_at::text AS updated_at \
     FROM autumn_runtime_config_values \
     ORDER BY key;";

const GET_SQL: &str = "SELECT key, raw_value, updated_at::text AS updated_at \
     FROM autumn_runtime_config_values \
     WHERE key = $1;";

// Per-key advisory lock: serializes concurrent set/unset on the same key so
// T2 blocks here until T1 commits, and the next statement then sees T1's
// committed row under READ COMMITTED's per-statement snapshot.
const CONFIG_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock(1, hashtext($1));";

const CONFIG_GET_PRIOR_SQL: &str =
    "SELECT raw_value FROM autumn_runtime_config_values WHERE key = $1;";

const CONFIG_UPSERT_SQL: &str = "INSERT INTO autumn_runtime_config_values (key, raw_value, updated_at) \
        VALUES ($1, $2, NOW()) \
        ON CONFLICT (key) DO UPDATE \
            SET raw_value = EXCLUDED.raw_value, \
                updated_at = EXCLUDED.updated_at;";

const CONFIG_SET_AUDIT_SQL: &str = "INSERT INTO autumn_runtime_config_changes \
    (key, old_value, new_value, actor) VALUES ($1, $2, $3, $4);";

const CONFIG_UNSET_DELETE_SQL: &str = "DELETE FROM autumn_runtime_config_values \
    WHERE key = $1 \
    RETURNING raw_value;";

const CONFIG_UNSET_AUDIT_SQL: &str = "INSERT INTO autumn_runtime_config_changes \
    (key, old_value, new_value, actor) VALUES ($1, $2, NULL, $3);";

const CONFIG_HISTORY_SQL: &str = "SELECT id::text, key, old_value, new_value, actor, changed_at::text AS changed_at \
     FROM autumn_runtime_config_changes \
     WHERE key = $1 \
     ORDER BY changed_at DESC \
     LIMIT $2;";

// ── Public entry points ────────────────────────────────────────────────────────

/// Run `autumn config list`.
pub fn run_list(_opts: &ListOptions) {
    let url = resolve_database_url();
    pg::block_on(async {
        let client = pg::connect_or_die("config list", &url).await;
        let rows = client
            .query(LIST_SQL, &[])
            .await
            .unwrap_or_else(|e| pg::die("config list", e));
        pg::print_table(
            &["key", "raw_value", "updated_at"],
            &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
        );
    });
}

/// Run `autumn config get <key>`.
pub fn run_get(opts: &GetOptions) {
    let url = resolve_database_url();
    pg::block_on(async {
        let client = pg::connect_or_die("config get", &url).await;
        let rows = client
            .query(GET_SQL, &[&opts.key])
            .await
            .unwrap_or_else(|e| pg::die("config get", e));
        if rows.is_empty() {
            eprintln!("\u{2717} Key '{}' has no active override.", opts.key);
            std::process::exit(1);
        }
        pg::print_table(
            &["key", "raw_value", "updated_at"],
            &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
        );
    });
}

/// Run `autumn config set <key> <value>`.
pub fn run_set(opts: &SetOptions) {
    let url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    pg::block_on(async {
        let mut client = pg::connect_or_die("config set", &url).await;
        let txn = client
            .transaction()
            .await
            .unwrap_or_else(|e| pg::die("config set", e));
        txn.execute(CONFIG_LOCK_SQL, &[&opts.key])
            .await
            .unwrap_or_else(|e| pg::die("config set", e));
        let prior_rows = txn
            .query(CONFIG_GET_PRIOR_SQL, &[&opts.key])
            .await
            .unwrap_or_else(|e| pg::die("config set", e));
        let old_value: Option<String> = prior_rows.first().map(|r| r.get::<_, String>(0));
        txn.execute(CONFIG_UPSERT_SQL, &[&opts.key, &opts.value])
            .await
            .unwrap_or_else(|e| pg::die("config set", e));
        txn.execute(
            CONFIG_SET_AUDIT_SQL,
            &[&opts.key, &old_value, &opts.value, &actor],
        )
        .await
        .unwrap_or_else(|e| pg::die("config set", e));
        txn.commit()
            .await
            .unwrap_or_else(|e| pg::die("config set", e));
    });

    eprintln!(
        "\u{2713} Set '{key}' = '{value}'",
        key = opts.key,
        value = opts.value
    );
}

/// Run `autumn config unset <key>`.
pub fn run_unset(opts: &UnsetOptions) {
    let url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    pg::block_on(async {
        let mut client = pg::connect_or_die("config unset", &url).await;
        let txn = client
            .transaction()
            .await
            .unwrap_or_else(|e| pg::die("config unset", e));
        txn.execute(CONFIG_LOCK_SQL, &[&opts.key])
            .await
            .unwrap_or_else(|e| pg::die("config unset", e));
        let removed = txn
            .query(CONFIG_UNSET_DELETE_SQL, &[&opts.key])
            .await
            .unwrap_or_else(|e| pg::die("config unset", e));
        if let Some(row) = removed.first() {
            let old_value: String = row.get(0);
            txn.execute(CONFIG_UNSET_AUDIT_SQL, &[&opts.key, &old_value, &actor])
                .await
                .unwrap_or_else(|e| pg::die("config unset", e));
        }
        txn.commit()
            .await
            .unwrap_or_else(|e| pg::die("config unset", e));
    });
    eprintln!(
        "\u{2713} Unset '{key}' (reverted to compile-time default)",
        key = opts.key
    );
}

/// Run `autumn config history <key>`.
pub fn run_history(opts: &HistoryOptions) {
    let url = resolve_database_url();
    let limit = i64::try_from(opts.limit).unwrap_or(i64::MAX);
    pg::block_on(async {
        let client = pg::connect_or_die("config history", &url).await;
        let rows = client
            .query(CONFIG_HISTORY_SQL, &[&opts.key, &limit])
            .await
            .unwrap_or_else(|e| pg::die("config history", e));
        pg::print_table(
            &["id", "key", "old_value", "new_value", "actor", "changed_at"],
            &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
        );
    });
}

// ── Database URL resolution (mirrors token.rs) ────────────────────────────────

pub fn resolve_database_url() -> String {
    if let Some(url) = resolve_primary_database_url_with_env(&OsEnv) {
        return url;
    }

    eprintln!("\u{2717} No database URL found.");
    eprintln!(
        "  Set database.primary_url (or database.url) in autumn.toml, autumn-<profile>.toml, \
         or set AUTUMN_DATABASE__PRIMARY_URL / AUTUMN_DATABASE__URL / DATABASE_URL."
    );
    std::process::exit(1);
}

pub fn resolve_primary_database_url_with_env(env: &dyn Env) -> Option<String> {
    resolve_primary_database_url_from_env_var(|key| env.var(key)).or_else(|| {
        AutumnConfig::load_with_env(env).ok().and_then(|config| {
            config
                .database
                .effective_primary_url()
                .map(ToOwned::to_owned)
        })
    })
}

fn resolve_primary_database_url_from_env_var<F>(env_var: F) -> Option<String>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    for var in [
        "AUTUMN_DATABASE__PRIMARY_URL",
        "AUTUMN_DATABASE__URL",
        "DATABASE_URL",
    ] {
        if let Ok(url) = env_var(var)
            && !url.is_empty()
        {
            return Some(url);
        }
    }

    None
}

#[cfg(test)]
fn resolve_primary_database_url_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> Option<String>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    if let Some(url) = resolve_primary_database_url_from_env_var(env_var) {
        return Some(url);
    }

    let database = table?.get("database").and_then(toml::Value::as_table)?;
    for key in ["primary_url", "url"] {
        if let Some(url) = database
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|url| !url.is_empty())
        {
            return Some(url.to_owned());
        }
    }

    None
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_sql_acquires_per_key_advisory_lock() {
        assert!(
            CONFIG_LOCK_SQL.contains("pg_advisory_xact_lock(1, hashtext($1))"),
            "set/unset should acquire the per-key advisory lock before reading/writing"
        );
    }

    #[test]
    fn unset_delete_sql_deletes_and_returns_prior_value() {
        assert!(CONFIG_UNSET_DELETE_SQL.contains("DELETE FROM autumn_runtime_config_values"));
        assert!(CONFIG_UNSET_DELETE_SQL.contains("RETURNING raw_value"));
    }

    #[test]
    fn config_set_sql_upserts() {
        assert!(
            CONFIG_UPSERT_SQL.contains("ON CONFLICT"),
            "config set must use INSERT ... ON CONFLICT"
        );
    }

    // ── Issue #1243: no more psql shell-out ─────────────────────────────────

    #[test]
    fn no_sql_constant_uses_psql_variable_syntax() {
        for sql in [
            LIST_SQL,
            GET_SQL,
            CONFIG_LOCK_SQL,
            CONFIG_GET_PRIOR_SQL,
            CONFIG_UPSERT_SQL,
            CONFIG_SET_AUDIT_SQL,
            CONFIG_UNSET_DELETE_SQL,
            CONFIG_UNSET_AUDIT_SQL,
            CONFIG_HISTORY_SQL,
        ] {
            assert!(
                !sql.contains(":'"),
                "SQL must use native $n placeholders, not psql variable substitution: {sql}"
            );
        }
    }

    #[test]
    fn no_sql_constant_uses_textual_transaction_control() {
        for sql in [CONFIG_UPSERT_SQL, CONFIG_UNSET_DELETE_SQL] {
            assert!(!sql.contains("BEGIN;"), "no textual BEGIN: {sql}");
            assert!(!sql.contains("COMMIT;"), "no textual COMMIT: {sql}");
        }
    }

    #[test]
    fn module_does_not_shell_out_to_psql() {
        let src = include_str!("config.rs");
        assert!(
            !src.contains("Command::new(\"psql\")"),
            "config.rs must not shell out to the psql binary (issue #1243)"
        );
    }

    #[test]
    fn resolve_prefers_primary_url_env_var() {
        let env = |key: &str| match key {
            "AUTUMN_DATABASE__PRIMARY_URL" => Ok("postgres://primary".to_owned()),
            "AUTUMN_DATABASE__URL" => Ok("postgres://legacy".to_owned()),
            "DATABASE_URL" => Ok("postgres://fallback".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_primary_database_url_from_sources(env, None).unwrap();
        assert_eq!(url, "postgres://primary");
    }

    #[test]
    fn resolve_falls_back_to_legacy_env_var() {
        let env = |key: &str| match key {
            "AUTUMN_DATABASE__URL" => Ok("postgres://legacy".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_primary_database_url_from_sources(env, None).unwrap();
        assert_eq!(url, "postgres://legacy");
    }

    #[test]
    fn resolve_falls_back_to_database_url_env_var() {
        let env = |key: &str| match key {
            "DATABASE_URL" => Ok("postgres://fallback".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_primary_database_url_from_sources(env, None).unwrap();
        assert_eq!(url, "postgres://fallback");
    }

    #[test]
    fn resolve_reads_primary_url_from_toml() {
        let table = toml::from_str::<toml::Table>(
            r#"
            [database]
            primary_url = "postgres://from-toml"
            "#,
        )
        .unwrap();
        let env = |_: &str| Err(std::env::VarError::NotPresent);
        let url = resolve_primary_database_url_from_sources(env, Some(&table)).unwrap();
        assert_eq!(url, "postgres://from-toml");
    }

    #[test]
    fn resolve_reads_url_from_toml_when_primary_url_absent() {
        let table = toml::from_str::<toml::Table>(
            r#"
            [database]
            url = "postgres://legacy-toml"
            "#,
        )
        .unwrap();
        let env = |_: &str| Err(std::env::VarError::NotPresent);
        let url = resolve_primary_database_url_from_sources(env, Some(&table)).unwrap();
        assert_eq!(url, "postgres://legacy-toml");
    }

    #[test]
    fn resolve_reads_url_from_active_profile_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("autumn.toml"), "").unwrap();
        std::fs::write(
            tmp.path().join("autumn-dev.toml"),
            r#"
            [database]
            url = "postgres://profile-file"
            "#,
        )
        .unwrap();

        let env = autumn_web::config::MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", tmp.path().to_str().unwrap())
            .with("AUTUMN_ENV", "dev");

        let url = resolve_primary_database_url_with_env(&env).unwrap();
        assert_eq!(url, "postgres://profile-file");
    }

    #[test]
    fn resolve_returns_none_when_no_source_found() {
        let env = |_: &str| Err(std::env::VarError::NotPresent);
        let url = resolve_primary_database_url_from_sources(env, None);
        assert!(url.is_none());
    }
}

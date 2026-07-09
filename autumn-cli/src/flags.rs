//! `autumn flags` — inspect and toggle feature flags at runtime.
//!
//! All commands connect directly to the configured Postgres database over a
//! native `tokio_postgres` connection (see issue #1243) — no `psql` binary
//! required. The database URL is resolved from `autumn.toml`, profile
//! overrides, or the `AUTUMN_DATABASE__PRIMARY_URL` / `AUTUMN_DATABASE__URL`
//! / `DATABASE_URL` environment variables.
//!
//! # Commands
//!
//! ```text
//! autumn flags list                       # list all flags with their current state
//! autumn flags enable <key>               # globally enable a flag (all actors)
//! autumn flags disable <key>              # globally disable a flag
//! autumn flags set-rollout <key> <pct>    # enable for pct% of actors (0–100)
//! autumn flags allow <key> <actor_id>     # add actor_id to the explicit allowlist
//! ```

use crate::pg::{self, ResultExt as _};
use tokio_postgres::types::ToSql;

// ── Options ───────────────────────────────────────────────────────────────────

/// Options for `autumn flags list`.
pub struct ListOptions;

/// Options for `autumn flags enable <key>`.
pub struct EnableOptions {
    pub key: String,
    pub actor: Option<String>,
}

/// Options for `autumn flags disable <key>`.
pub struct DisableOptions {
    pub key: String,
    pub actor: Option<String>,
}

/// Options for `autumn flags set-rollout <key> <pct>`.
pub struct SetRolloutOptions {
    pub key: String,
    pub pct: u8,
    pub actor: Option<String>,
}

/// Options for `autumn flags allow <key> <actor_id>`.
pub struct AllowOptions {
    pub key: String,
    pub actor_id: String,
    pub actor: Option<String>,
}

// ── SQL helpers ──────────────────────────────────────────────────────────────

const LIST_SQL: &str = "SELECT key, \
           CASE WHEN enabled THEN 'YES' ELSE 'no' END AS enabled, \
           rollout_pct || '%' AS rollout, \
           actor_allowlist, \
           group_allowlist, \
           updated_at::text AS updated_at \
    FROM autumn_feature_flags ORDER BY key;";

// enable() sets enabled=true + rollout_pct=100 (globally on for all actors).
const ENABLE_SQL: &str = "INSERT INTO autumn_feature_flags (key, enabled, rollout_pct) \
    VALUES ($1, TRUE, 100) \
    ON CONFLICT (key) DO UPDATE SET enabled = TRUE, rollout_pct = 100, updated_at = NOW();";

// disable() is a kill-switch: sets enabled=false, preserves rollout config.
const DISABLE_SQL: &str = "INSERT INTO autumn_feature_flags (key, enabled) \
    VALUES ($1, FALSE) \
    ON CONFLICT (key) DO UPDATE SET enabled = FALSE, updated_at = NOW();";

// set_rollout() also clears the kill-switch (sets enabled=true).
const SET_ROLLOUT_SQL: &str = "INSERT INTO autumn_feature_flags (key, enabled, rollout_pct) \
    VALUES ($1, TRUE, $2) \
    ON CONFLICT (key) DO UPDATE \
        SET enabled = TRUE, rollout_pct = $2, updated_at = NOW();";

const ALLOW_UPSERT_SQL: &str = "INSERT INTO autumn_feature_flags (key, enabled, rollout_pct) \
    VALUES ($1, TRUE, 0) ON CONFLICT (key) DO UPDATE \
        SET enabled = TRUE, \
            rollout_pct = CASE WHEN NOT autumn_feature_flags.enabled THEN 0 \
                               ELSE autumn_feature_flags.rollout_pct END, \
            updated_at = NOW();";

const ALLOW_UPDATE_SQL: &str = "UPDATE autumn_feature_flags \
    SET actor_allowlist = ( \
        SELECT json_agg(DISTINCT elem)::text \
        FROM ( \
            SELECT jsonb_array_elements_text(actor_allowlist::jsonb) AS elem \
            UNION SELECT $1::text \
        ) t \
    ), updated_at = NOW() \
    WHERE key = $2;";

// Shared audit-log insert for every mutation below; the DB trigger on this
// table fans out `NOTIFY autumn_flags, <key>` to running replicas.
const FLAG_AUDIT_SQL: &str =
    "INSERT INTO feature_flag_changes (key, mutation, actor) VALUES ($1, $2, $3);";

// ── Public runners ────────────────────────────────────────────────────────────

/// Run `autumn flags list`.
pub fn run_list(_opts: &ListOptions) {
    let db_url = resolve_database_url();
    let rows = pg::block_on_or_die("flags list", async {
        let client = pg::connect("flags list", &db_url).await?;
        client.query(LIST_SQL, &[]).await.pg()
    });
    pg::print_table(
        &[
            "key",
            "enabled",
            "rollout",
            "actor_allowlist",
            "group_allowlist",
            "updated_at",
        ],
        &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
    );
}

/// Run `autumn flags enable <key>`.
pub fn run_enable(opts: &EnableOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    pg::block_on_or_die("flags enable", async {
        let mut client = pg::connect("flags enable", &db_url).await?;
        pg::execute_with_audit(
            &mut client,
            ENABLE_SQL,
            &[&opts.key],
            None,
            FLAG_AUDIT_SQL,
            &[&opts.key, &"enabled", &actor],
        )
        .await
    });
    println!("✓ Flag '{}' enabled globally.", opts.key);
}

/// Run `autumn flags disable <key>`.
pub fn run_disable(opts: &DisableOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    pg::block_on_or_die("flags disable", async {
        let mut client = pg::connect("flags disable", &db_url).await?;
        pg::execute_with_audit(
            &mut client,
            DISABLE_SQL,
            &[&opts.key],
            None,
            FLAG_AUDIT_SQL,
            &[&opts.key, &"disabled", &actor],
        )
        .await
    });
    println!("✓ Flag '{}' disabled globally.", opts.key);
}

/// Run `autumn flags set-rollout <key> <pct>`.
pub fn run_set_rollout(opts: &SetRolloutOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    let pct = i16::from(opts.pct);
    let mutation = format!("rollout={}", opts.pct);
    pg::block_on_or_die("flags set-rollout", async {
        let mut client = pg::connect("flags set-rollout", &db_url).await?;
        pg::execute_with_audit(
            &mut client,
            SET_ROLLOUT_SQL,
            &[&opts.key, &pct],
            None,
            FLAG_AUDIT_SQL,
            &[&opts.key, &mutation, &actor],
        )
        .await
    });
    println!("✓ Flag '{}' rollout set to {}%.", opts.key, opts.pct);
}

/// Run `autumn flags allow <key> <actor_id>`.
pub fn run_allow(opts: &AllowOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    let mutation = format!("allowed_actor={}", opts.actor_id);
    pg::block_on_or_die("flags allow", async {
        let mut client = pg::connect("flags allow", &db_url).await?;
        let upsert_params: &[&(dyn ToSql + Sync)] = &[&opts.key];
        let update_params: &[&(dyn ToSql + Sync)] = &[&opts.actor_id, &opts.key];
        pg::execute_many_with_audit(
            &mut client,
            &[
                (ALLOW_UPSERT_SQL, upsert_params),
                (ALLOW_UPDATE_SQL, update_params),
            ],
            None,
            FLAG_AUDIT_SQL,
            &[&opts.key, &mutation, &actor],
        )
        .await
    });
    println!(
        "✓ Actor '{}' added to allowlist for flag '{}'.",
        opts.actor_id, opts.key
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn resolve_database_url() -> String {
    crate::config::resolve_database_url()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_queries_reference_correct_tables() {
        assert!(LIST_SQL.contains("autumn_feature_flags"));
        assert!(ENABLE_SQL.contains("autumn_feature_flags"));
        assert!(DISABLE_SQL.contains("autumn_feature_flags"));
        assert!(SET_ROLLOUT_SQL.contains("autumn_feature_flags"));
        assert!(ALLOW_UPSERT_SQL.contains("autumn_feature_flags"));
        assert!(ALLOW_UPDATE_SQL.contains("autumn_feature_flags"));
        assert!(FLAG_AUDIT_SQL.contains("feature_flag_changes"));
    }

    #[test]
    fn enable_sql_uses_upsert() {
        assert!(
            ENABLE_SQL.contains("ON CONFLICT"),
            "enable SQL must use INSERT ... ON CONFLICT to create the flag if absent"
        );
    }

    #[test]
    fn disable_sql_uses_upsert() {
        assert!(
            DISABLE_SQL.contains("ON CONFLICT"),
            "disable SQL must use INSERT ... ON CONFLICT"
        );
    }

    #[test]
    fn allow_upsert_sql_uses_upsert() {
        assert!(
            ALLOW_UPSERT_SQL.contains("ON CONFLICT"),
            "allow upsert SQL must use INSERT ... ON CONFLICT"
        );
    }

    // ── Issue #1243: no more psql shell-out ─────────────────────────────────

    #[test]
    fn no_sql_constant_uses_psql_variable_syntax() {
        for sql in [
            LIST_SQL,
            ENABLE_SQL,
            DISABLE_SQL,
            SET_ROLLOUT_SQL,
            ALLOW_UPSERT_SQL,
            ALLOW_UPDATE_SQL,
            FLAG_AUDIT_SQL,
        ] {
            assert!(
                !sql.contains(":'"),
                "SQL must use native $n placeholders, not psql variable substitution: {sql}"
            );
        }
    }

    #[test]
    fn no_sql_constant_uses_textual_transaction_control() {
        // Transactions are now driven by tokio_postgres::Client::transaction(),
        // not textual BEGIN/COMMIT statements sent through psql.
        for sql in [ENABLE_SQL, DISABLE_SQL, SET_ROLLOUT_SQL, ALLOW_UPSERT_SQL] {
            assert!(!sql.contains("BEGIN;"), "no textual BEGIN: {sql}");
            assert!(!sql.contains("COMMIT;"), "no textual COMMIT: {sql}");
        }
    }

    #[test]
    fn mutation_sql_uses_dollar_placeholders() {
        for sql in [
            ENABLE_SQL,
            DISABLE_SQL,
            SET_ROLLOUT_SQL,
            ALLOW_UPSERT_SQL,
            ALLOW_UPDATE_SQL,
            FLAG_AUDIT_SQL,
        ] {
            assert!(
                sql.contains('$'),
                "mutation SQL must take native params via $n: {sql}"
            );
        }
    }

    #[test]
    fn module_does_not_shell_out_to_psql() {
        let src = include_str!("flags.rs");
        assert!(
            !src.contains("Command::new(\"psql\")"),
            "flags.rs must not shell out to the psql binary (issue #1243)"
        );
    }
}

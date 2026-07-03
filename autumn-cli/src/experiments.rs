//! `autumn experiments` — inspect and manage A/B experiments at runtime.
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
//! autumn experiments list                                  # list all experiments
//! autumn experiments status <name>                         # show experiment details
//! autumn experiments set-weights <name> <v=w,v=w,...>      # update variant weights
//! autumn experiments conclude <name> <winner>              # pin winner, stop assignments
//! autumn experiments override <name> <actor_id> <variant>  # pin actor to variant (QA/staff)
//! ```

use crate::pg;

// ── Options ───────────────────────────────────────────────────────────────────

/// Options for `autumn experiments list`.
pub struct ListOptions;

/// Options for `autumn experiments status <name>`.
pub struct StatusOptions {
    pub name: String,
}

/// Options for `autumn experiments set-weights <name> <variants>`.
pub struct SetWeightsOptions {
    pub name: String,
    /// Comma-separated `variant=weight` pairs, e.g. `"control=50,treatment=50"`.
    pub weights: String,
    pub actor: Option<String>,
}

/// Options for `autumn experiments conclude <name> <winner>`.
pub struct ConcludeOptions {
    pub name: String,
    pub winner: String,
    pub actor: Option<String>,
}

/// Options for `autumn experiments override <name> <actor_id> <variant>`.
pub struct OverrideOptions {
    pub name: String,
    pub actor_id: String,
    pub variant: String,
    pub actor: Option<String>,
}

// ── SQL helpers ──────────────────────────────────────────────────────────────

const LIST_SQL: &str = "SELECT name, state::text, \
    (SELECT string_agg((v->>'name') || '=' || (v->>'weight'), ', ' ORDER BY v->>'name') \
        FROM jsonb_array_elements(variants::jsonb) v) AS variants, \
    winner, updated_at::text AS updated_at \
    FROM autumn_experiments ORDER BY name;";

const STATUS_SQL: &str = "SELECT name, description, state::text, variants::text, winner, \
    exclusion_group, updated_at::text AS updated_at \
    FROM autumn_experiments WHERE name = $1;";

const SET_WEIGHTS_EXISTS_SQL: &str = "SELECT COUNT(*)::bigint FROM autumn_experiments \
    WHERE name = $1 AND state NOT IN ('concluded', 'archived');";

const SET_WEIGHTS_SQL: &str =
    "UPDATE autumn_experiments SET variants = $1::jsonb, updated_at = NOW() WHERE name = $2;";

const CONCLUDE_WINNER_CHECK_SQL: &str = "SELECT COUNT(*)::bigint FROM autumn_experiments, \
    jsonb_array_elements(variants::jsonb) v \
    WHERE name = $1 AND state != 'archived' AND v->>'name' = $2;";

const CONCLUDE_SQL: &str = "UPDATE autumn_experiments SET state = 'concluded', winner = $1, \
    updated_at = NOW() WHERE name = $2;";

const OVERRIDE_VARIANT_CHECK_SQL: &str = "SELECT COUNT(*)::bigint FROM autumn_experiments, \
    jsonb_array_elements(variants::jsonb) v \
    WHERE name = $1 AND v->>'name' = $2;";

const OVERRIDE_UPSERT_SQL: &str = "INSERT INTO autumn_experiment_overrides (experiment, actor, variant) \
    VALUES ($1, $2, $3) \
    ON CONFLICT (experiment, actor) DO UPDATE SET variant = $3;";

// Shared audit-log insert for every mutation below; the DB trigger on this
// table fans out `NOTIFY autumn_experiments, <name>` to running replicas.
const EXPERIMENT_AUDIT_SQL: &str =
    "INSERT INTO autumn_experiment_changes (experiment, mutation, actor) VALUES ($1, $2, $3);";

// ── Public runners ────────────────────────────────────────────────────────────

/// Run `autumn experiments list`.
pub fn run_list(_opts: &ListOptions) {
    let db_url = resolve_database_url();
    pg::block_on(async {
        let client = pg::connect_or_die("experiments list", &db_url).await;
        let rows = client
            .query(LIST_SQL, &[])
            .await
            .unwrap_or_else(|e| pg::die("experiments list", e));
        pg::print_table(
            &["name", "state", "variants", "winner", "updated_at"],
            &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
        );
    });
}

/// Run `autumn experiments status <name>`.
pub fn run_status(opts: &StatusOptions) {
    let db_url = resolve_database_url();
    pg::block_on(async {
        let client = pg::connect_or_die("experiments status", &db_url).await;
        let rows = client
            .query(STATUS_SQL, &[&opts.name])
            .await
            .unwrap_or_else(|e| pg::die("experiments status", e));
        if rows.is_empty() {
            pg::die(
                "experiments status",
                format!("experiment '{}' not found", opts.name),
            );
        }
        pg::print_table(
            &[
                "name",
                "description",
                "state",
                "variants",
                "winner",
                "exclusion_group",
                "updated_at",
            ],
            &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
        );
    });
}

/// Run `autumn experiments set-weights <name> <weights>`.
pub fn run_set_weights(opts: &SetWeightsOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    // Parse "control=50,treatment=50" into JSON array of variant objects.
    let variants_json = parse_weights_to_json(&opts.weights).unwrap_or_else(|e| {
        eprintln!("autumn experiments set-weights: {e}");
        std::process::exit(1);
    });
    let mutation = format!("set_weights={variants_json}");
    // Bind as a typed JSON value (not a bare string) — the `$1::jsonb` cast in
    // SET_WEIGHTS_SQL makes Postgres report the parameter's type as jsonb, and
    // `postgres-types`'s `ToSql` for `String`/`&str` only declares itself
    // compatible with text-ish types, not JSON/JSONB.
    let variants_value: serde_json::Value = serde_json::from_str(&variants_json)
        .unwrap_or_else(|e| pg::die("experiments set-weights", e));
    pg::block_on(async {
        let mut client = pg::connect_or_die("experiments set-weights", &db_url).await;
        let exists = client
            .query_one(SET_WEIGHTS_EXISTS_SQL, &[&opts.name])
            .await
            .unwrap_or_else(|e| pg::die("experiments set-weights", e));
        if exists.get::<_, i64>(0) == 0 {
            pg::die(
                "experiments set-weights",
                format!(
                    "experiment '{}' does not exist or is concluded/archived",
                    opts.name
                ),
            );
        }
        let txn = client
            .transaction()
            .await
            .unwrap_or_else(|e| pg::die("experiments set-weights", e));
        txn.execute(SET_WEIGHTS_SQL, &[&variants_value, &opts.name])
            .await
            .unwrap_or_else(|e| pg::die("experiments set-weights", e));
        txn.execute(EXPERIMENT_AUDIT_SQL, &[&opts.name, &mutation, &actor])
            .await
            .unwrap_or_else(|e| pg::die("experiments set-weights", e));
        txn.commit()
            .await
            .unwrap_or_else(|e| pg::die("experiments set-weights", e));
    });
    println!(
        "✓ Experiment '{}' weights updated to {}.",
        opts.name, opts.weights
    );
}

/// Run `autumn experiments conclude <name> <winner>`.
pub fn run_conclude(opts: &ConcludeOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    let mutation = format!("concluded={}", opts.winner);
    pg::block_on(async {
        let mut client = pg::connect_or_die("experiments conclude", &db_url).await;
        let exists = client
            .query_one(CONCLUDE_WINNER_CHECK_SQL, &[&opts.name, &opts.winner])
            .await
            .unwrap_or_else(|e| pg::die("experiments conclude", e));
        if exists.get::<_, i64>(0) == 0 {
            pg::die(
                "experiments conclude",
                format!(
                    "'{}' is not a variant of a non-archived experiment '{}'",
                    opts.winner, opts.name
                ),
            );
        }
        let txn = client
            .transaction()
            .await
            .unwrap_or_else(|e| pg::die("experiments conclude", e));
        txn.execute(CONCLUDE_SQL, &[&opts.winner, &opts.name])
            .await
            .unwrap_or_else(|e| pg::die("experiments conclude", e));
        txn.execute(EXPERIMENT_AUDIT_SQL, &[&opts.name, &mutation, &actor])
            .await
            .unwrap_or_else(|e| pg::die("experiments conclude", e));
        txn.commit()
            .await
            .unwrap_or_else(|e| pg::die("experiments conclude", e));
    });
    println!(
        "✓ Experiment '{}' concluded with winner '{}'.",
        opts.name, opts.winner
    );
}

/// Run `autumn experiments override <name> <actor_id> <variant>`.
pub fn run_override(opts: &OverrideOptions) {
    let db_url = resolve_database_url();
    let actor = opts.actor.as_deref().unwrap_or("cli");
    let mutation = format!("override={}:{}", opts.actor_id, opts.variant);
    pg::block_on(async {
        let mut client = pg::connect_or_die("experiments override", &db_url).await;
        let exists = client
            .query_one(OVERRIDE_VARIANT_CHECK_SQL, &[&opts.name, &opts.variant])
            .await
            .unwrap_or_else(|e| pg::die("experiments override", e));
        if exists.get::<_, i64>(0) == 0 {
            pg::die(
                "experiments override",
                format!(
                    "'{}' is not a variant of experiment '{}'",
                    opts.variant, opts.name
                ),
            );
        }
        let txn = client
            .transaction()
            .await
            .unwrap_or_else(|e| pg::die("experiments override", e));
        txn.execute(
            OVERRIDE_UPSERT_SQL,
            &[&opts.name, &opts.actor_id, &opts.variant],
        )
        .await
        .unwrap_or_else(|e| pg::die("experiments override", e));
        txn.execute(EXPERIMENT_AUDIT_SQL, &[&opts.name, &mutation, &actor])
            .await
            .unwrap_or_else(|e| pg::die("experiments override", e));
        txn.commit()
            .await
            .unwrap_or_else(|e| pg::die("experiments override", e));
    });
    println!(
        "✓ Actor '{}' pinned to variant '{}' in experiment '{}'.",
        opts.actor_id, opts.variant, opts.name
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert `"control=50,treatment=50"` to `[{"name":"control","weight":50},...]`.
/// Returns an error message if any pair is malformed.
fn parse_weights_to_json(weights: &str) -> Result<String, String> {
    let mut parts: Vec<serde_json::Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in weights.split(',') {
        let pair = raw.trim();
        let mut it = pair.splitn(2, '=');
        let name = it.next().unwrap_or("").trim();
        if name.is_empty() {
            return Err(format!(
                "malformed weight spec {pair:?}: variant name is empty"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("duplicate variant name {name:?} in weight spec"));
        }
        let weight_str = it
            .next()
            .ok_or_else(|| format!("malformed weight spec {pair:?}: missing '=<weight>'"))?;
        let weight: u32 = weight_str.trim().parse().map_err(|_| {
            format!("malformed weight spec {pair:?}: weight must be a non-negative integer")
        })?;
        parts.push(serde_json::json!({"name": name, "weight": weight}));
    }
    Ok(serde_json::to_string(&parts).unwrap_or_else(|_| "[]".to_owned()))
}

fn resolve_database_url() -> String {
    crate::config::resolve_database_url()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_queries_reference_correct_tables() {
        assert!(LIST_SQL.contains("autumn_experiments"));
        assert!(STATUS_SQL.contains("autumn_experiments"));
        assert!(SET_WEIGHTS_EXISTS_SQL.contains("autumn_experiments"));
        assert!(SET_WEIGHTS_SQL.contains("autumn_experiments"));
        assert!(CONCLUDE_WINNER_CHECK_SQL.contains("autumn_experiments"));
        assert!(CONCLUDE_SQL.contains("autumn_experiments"));
        assert!(OVERRIDE_VARIANT_CHECK_SQL.contains("autumn_experiments"));
        assert!(OVERRIDE_UPSERT_SQL.contains("autumn_experiment_overrides"));
        assert!(EXPERIMENT_AUDIT_SQL.contains("autumn_experiment_changes"));
    }

    #[test]
    fn conclude_sql_sets_winner_and_state() {
        assert!(CONCLUDE_SQL.contains("state = 'concluded'"));
        assert!(CONCLUDE_SQL.contains("winner = $"));
    }

    #[test]
    fn override_sql_uses_upsert() {
        assert!(
            OVERRIDE_UPSERT_SQL.contains("ON CONFLICT"),
            "override SQL must use INSERT ... ON CONFLICT"
        );
    }

    #[test]
    fn mutation_sql_have_existence_checks() {
        for sql in [
            SET_WEIGHTS_EXISTS_SQL,
            CONCLUDE_WINNER_CHECK_SQL,
            OVERRIDE_VARIANT_CHECK_SQL,
        ] {
            assert!(
                sql.contains("COUNT(*)"),
                "mutation SQL must have an existence check: {sql}"
            );
        }
    }

    #[test]
    fn conclude_sql_validates_winner_in_variants() {
        assert!(
            CONCLUDE_WINNER_CHECK_SQL.contains("jsonb_array_elements"),
            "conclude SQL must check winner against variants: {CONCLUDE_WINNER_CHECK_SQL}"
        );
    }

    #[test]
    fn override_sql_validates_variant_in_variants() {
        assert!(
            OVERRIDE_VARIANT_CHECK_SQL.contains("jsonb_array_elements"),
            "override SQL must check variant against variants: {OVERRIDE_VARIANT_CHECK_SQL}"
        );
    }

    #[test]
    fn set_weights_sql_guards_non_editable_states() {
        assert!(
            SET_WEIGHTS_EXISTS_SQL.contains("concluded")
                && SET_WEIGHTS_EXISTS_SQL.contains("archived"),
            "set_weights SQL must reject concluded and archived experiments: {SET_WEIGHTS_EXISTS_SQL}"
        );
    }

    #[test]
    fn conclude_sql_guards_against_archived() {
        assert!(
            CONCLUDE_WINNER_CHECK_SQL.contains("archived"),
            "conclude SQL must reject archived experiments: {CONCLUDE_WINNER_CHECK_SQL}"
        );
    }

    // ── Issue #1243: no more psql shell-out ─────────────────────────────────

    #[test]
    fn no_sql_constant_uses_psql_variable_syntax() {
        for sql in [
            LIST_SQL,
            STATUS_SQL,
            SET_WEIGHTS_EXISTS_SQL,
            SET_WEIGHTS_SQL,
            CONCLUDE_WINNER_CHECK_SQL,
            CONCLUDE_SQL,
            OVERRIDE_VARIANT_CHECK_SQL,
            OVERRIDE_UPSERT_SQL,
            EXPERIMENT_AUDIT_SQL,
        ] {
            assert!(
                !sql.contains(":'"),
                "SQL must use native $n placeholders, not psql variable substitution: {sql}"
            );
        }
    }

    #[test]
    fn no_sql_constant_uses_textual_transaction_control_or_division_hack() {
        for sql in [
            SET_WEIGHTS_EXISTS_SQL,
            SET_WEIGHTS_SQL,
            CONCLUDE_WINNER_CHECK_SQL,
            CONCLUDE_SQL,
            OVERRIDE_VARIANT_CHECK_SQL,
            OVERRIDE_UPSERT_SQL,
        ] {
            assert!(!sql.contains("BEGIN;"), "no textual BEGIN: {sql}");
            assert!(!sql.contains("COMMIT;"), "no textual COMMIT: {sql}");
            assert!(
                !sql.contains("1/("),
                "no division-by-zero existence hack: {sql}"
            );
        }
    }

    #[test]
    fn module_does_not_shell_out_to_psql() {
        let src = include_str!("experiments.rs");
        assert!(
            !src.contains("Command::new(\"psql\")"),
            "experiments.rs must not shell out to the psql binary (issue #1243)"
        );
    }

    #[test]
    fn parse_weights_to_json_produces_valid_json() {
        let json = parse_weights_to_json("control=50,treatment=50").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).expect("invalid JSON");
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "control");
        assert_eq!(arr[0]["weight"], 50);
        assert_eq!(arr[1]["name"], "treatment");
        assert_eq!(arr[1]["weight"], 50);
    }

    #[test]
    fn parse_weights_handles_three_variants() {
        let json = parse_weights_to_json("control=33,treatment_a=33,treatment_b=34").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).expect("invalid JSON");
        assert_eq!(v.as_array().unwrap().len(), 3);
    }

    #[test]
    fn parse_weights_handles_whitespace() {
        let json = parse_weights_to_json(" control = 50 , treatment = 50 ").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["name"], "control");
    }

    #[test]
    fn parse_weights_errors_on_missing_equals() {
        let err = parse_weights_to_json("control50").unwrap_err();
        assert!(
            err.contains("malformed"),
            "expected malformed error, got: {err}"
        );
    }

    #[test]
    fn parse_weights_errors_on_non_integer_weight() {
        let err = parse_weights_to_json("control=abc").unwrap_err();
        assert!(
            err.contains("integer"),
            "expected integer error, got: {err}"
        );
    }
}

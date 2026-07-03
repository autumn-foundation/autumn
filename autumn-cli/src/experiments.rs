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

use crate::pg::{self, ResultExt as _};

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

// The three mutations below fold their validation guard directly into the
// mutating statement's own WHERE/EXISTS clause, instead of running a
// separate SELECT COUNT(*) check before opening a transaction. A pre-check
// followed by a later, separately-opened transaction leaves a real
// TOCTOU race window (a concurrent process can change the experiment's
// state between the check and the write); checking and mutating in the
// same statement makes the guard atomic with the write, and "zero rows
// affected" is the failure signal (see `pg::execute_with_audit`).
const SET_WEIGHTS_SQL: &str = "UPDATE autumn_experiments SET variants = $1::jsonb, updated_at = NOW() \
    WHERE name = $2 AND state NOT IN ('concluded', 'archived');";

const CONCLUDE_SQL: &str = "UPDATE autumn_experiments SET state = 'concluded', winner = $1, \
    updated_at = NOW() \
    WHERE name = $2 AND state != 'archived' AND EXISTS ( \
        SELECT 1 FROM jsonb_array_elements(variants::jsonb) v WHERE v->>'name' = $1 \
    );";

const OVERRIDE_UPSERT_SQL: &str = "INSERT INTO autumn_experiment_overrides (experiment, actor, variant) \
    SELECT $1, $2, $3 WHERE EXISTS ( \
        SELECT 1 FROM autumn_experiments, jsonb_array_elements(variants::jsonb) v \
        WHERE name = $1 AND v->>'name' = $3 \
    ) \
    ON CONFLICT (experiment, actor) DO UPDATE SET variant = $3;";

// Shared audit-log insert for every mutation below; the DB trigger on this
// table fans out `NOTIFY autumn_experiments, <name>` to running replicas.
const EXPERIMENT_AUDIT_SQL: &str =
    "INSERT INTO autumn_experiment_changes (experiment, mutation, actor) VALUES ($1, $2, $3);";

// ── Public runners ────────────────────────────────────────────────────────────

/// Run `autumn experiments list`.
pub fn run_list(_opts: &ListOptions) {
    let db_url = resolve_database_url();
    let rows = pg::block_on_or_die("experiments list", async {
        let client = pg::connect("experiments list", &db_url).await?;
        client.query(LIST_SQL, &[]).await.pg()
    });
    pg::print_table(
        &["name", "state", "variants", "winner", "updated_at"],
        &rows.iter().map(pg::row_to_strings).collect::<Vec<_>>(),
    );
}

/// Run `autumn experiments status <name>`.
pub fn run_status(opts: &StatusOptions) {
    let db_url = resolve_database_url();
    let rows = pg::block_on_or_die("experiments status", async {
        let client = pg::connect("experiments status", &db_url).await?;
        client.query(STATUS_SQL, &[&opts.name]).await.pg()
    });
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
    let variants_value: serde_json::Value = match serde_json::from_str(&variants_json) {
        Ok(v) => v,
        Err(e) => pg::die("experiments set-weights", e),
    };
    let not_found = format!(
        "experiment '{}' does not exist or is concluded/archived",
        opts.name
    );
    pg::block_on_or_die("experiments set-weights", async {
        let mut client = pg::connect("experiments set-weights", &db_url).await?;
        pg::execute_with_audit(
            &mut client,
            SET_WEIGHTS_SQL,
            &[&variants_value, &opts.name],
            Some(&not_found),
            EXPERIMENT_AUDIT_SQL,
            &[&opts.name, &mutation, &actor],
        )
        .await
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
    let not_found = format!(
        "'{}' is not a variant of a non-archived experiment '{}'",
        opts.winner, opts.name
    );
    pg::block_on_or_die("experiments conclude", async {
        let mut client = pg::connect("experiments conclude", &db_url).await?;
        pg::execute_with_audit(
            &mut client,
            CONCLUDE_SQL,
            &[&opts.winner, &opts.name],
            Some(&not_found),
            EXPERIMENT_AUDIT_SQL,
            &[&opts.name, &mutation, &actor],
        )
        .await
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
    let not_found = format!(
        "'{}' is not a variant of experiment '{}'",
        opts.variant, opts.name
    );
    pg::block_on_or_die("experiments override", async {
        let mut client = pg::connect("experiments override", &db_url).await?;
        pg::execute_with_audit(
            &mut client,
            OVERRIDE_UPSERT_SQL,
            &[&opts.name, &opts.actor_id, &opts.variant],
            Some(&not_found),
            EXPERIMENT_AUDIT_SQL,
            &[&opts.name, &mutation, &actor],
        )
        .await
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
        assert!(SET_WEIGHTS_SQL.contains("autumn_experiments"));
        assert!(CONCLUDE_SQL.contains("autumn_experiments"));
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

    // ── Issue #1243 follow-up: guard checks folded into the mutation itself
    // (via WHERE/EXISTS), not a separate pre-transaction check, so there is
    // no round trip between "is this valid?" and "make the change" for a
    // concurrent process to race into. ────────────────────────────────────

    #[test]
    fn conclude_sql_validates_winner_in_variants_atomically() {
        assert!(
            CONCLUDE_SQL.contains("EXISTS") && CONCLUDE_SQL.contains("jsonb_array_elements"),
            "conclude SQL must check winner against variants in its own WHERE clause: {CONCLUDE_SQL}"
        );
    }

    #[test]
    fn override_sql_validates_variant_in_variants_atomically() {
        assert!(
            OVERRIDE_UPSERT_SQL.contains("EXISTS")
                && OVERRIDE_UPSERT_SQL.contains("jsonb_array_elements"),
            "override SQL must check variant against variants in its own WHERE clause: {OVERRIDE_UPSERT_SQL}"
        );
    }

    #[test]
    fn set_weights_sql_guards_non_editable_states_in_its_own_where_clause() {
        assert!(
            SET_WEIGHTS_SQL.contains("concluded") && SET_WEIGHTS_SQL.contains("archived"),
            "set_weights SQL must reject concluded and archived experiments in its own WHERE clause: {SET_WEIGHTS_SQL}"
        );
    }

    #[test]
    fn conclude_sql_guards_against_archived_in_its_own_where_clause() {
        assert!(
            CONCLUDE_SQL.contains("archived"),
            "conclude SQL must reject archived experiments in its own WHERE clause: {CONCLUDE_SQL}"
        );
    }

    #[test]
    fn no_mutation_sql_is_a_separate_pre_transaction_check() {
        // There must be no standalone `SELECT COUNT(*)`-style existence
        // check left anywhere — every guard lives inside the mutating
        // statement's own WHERE/EXISTS clause instead.
        for sql in [SET_WEIGHTS_SQL, CONCLUDE_SQL, OVERRIDE_UPSERT_SQL] {
            assert!(
                !sql.trim_start().to_uppercase().starts_with("SELECT COUNT"),
                "guard must be folded into the mutation, not a separate COUNT(*) check: {sql}"
            );
        }
    }

    // ── Issue #1243: no more psql shell-out ─────────────────────────────────

    #[test]
    fn no_sql_constant_uses_psql_variable_syntax() {
        for sql in [
            LIST_SQL,
            STATUS_SQL,
            SET_WEIGHTS_SQL,
            CONCLUDE_SQL,
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
        for sql in [SET_WEIGHTS_SQL, CONCLUDE_SQL, OVERRIDE_UPSERT_SQL] {
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

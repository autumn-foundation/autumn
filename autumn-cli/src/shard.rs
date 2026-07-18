//! `autumn shard move-slot` — move a set of tenants' rows from one configured
//! shard to another (issue #1209 §3c).
//!
//! This is the framework-integrated counterpart to the worked
//! `examples/bookmarks-sharded` move tool: it resolves `--from` / `--to` by
//! their `[[database.shards]]` names (honoring `--profile` and env overrides,
//! exactly like `autumn migrate`), then for the given tenant key(s):
//!
//!   1. copies their rows source → destination,
//!   2. verifies the move (row counts **and** a `to_jsonb` content checksum
//!      match on both shards),
//!   3. deletes the source rows only with `--confirm`, after verification.
//!
//! It **never** edits routing — copy and verify, re-route the moved tenant(s),
//! then re-run with `--confirm` to delete. All columns (including the primary
//! key) are copied, so references that point at a moved row stay valid across
//! the move; this assumes the usual `BIGSERIAL`/explicit PKs rather than
//! `GENERATED ALWAYS AS IDENTITY`.
//!
//! **Per-tenant moves and hash slots:** this tool copies the rows of the
//! *specific* tenant key(s) you name. Under the default `HashShardRouter`, a
//! slot is shared by every key that hashes to it, so remapping a slot in
//! `autumn.toml` reroutes *all* of those co-tenants — not just the one you
//! copied — and any co-tenant whose rows you did not move would lose data. To
//! move a single tenant, use the `DirectoryShardRouter`
//! (`database.directory_shard_router = true`) and pin that tenant to the
//! destination. Only remap a hash slot when you have copied **every** key in
//! that slot.
//!
//! **Requires source quiescence.** The named tenant key(s) must be read-only on
//! the *source* shard — no inserts, updates, or **deletes** — from before step 1
//! (the initial copy) until step 3 (the post-cutover delete). Verification only
//! checks that the destination *contains* every current source row, so it
//! tolerates rows written to the destination after the re-route. It cannot,
//! however, tell a legitimate post-route write apart from a row that the dry-run
//! copied and that was then deleted on the source mid-move: both look like "on
//! the destination, not on the source", because the dry-run copy set is not
//! persisted between runs. Without source quiescence such a delete is silently
//! resurrected on the destination after cutover. Put the moved tenant into a
//! read-only / "moving" state before copying (gating writes at the directory
//! pin is a natural place) and keep it there until the move completes.
//!
//! Row movement uses `psql`'s `\copy` over a pipe (the same "shell out to the
//! standard Postgres tool" approach `autumn migrate` takes with `diesel`).

use std::process::{Command, Stdio};

/// Parsed arguments for `autumn shard move-slot`.
pub struct MoveSlotArgs {
    pub from: String,
    pub to: String,
    pub table: String,
    pub key_column: String,
    pub id_column: String,
    pub tenants: Vec<String>,
    pub confirm: bool,
    pub profile: Option<String>,
}

pub fn run_move_slot(args: &MoveSlotArgs) {
    eprintln!("\u{1F342} autumn shard move-slot\n");

    if args.tenants.is_empty() {
        fail("at least one --tenant is required");
    }
    if args.from == args.to {
        fail("--from and --to must be different shards");
    }
    validate_identifier("--table", &args.table);
    validate_identifier("--key-column", &args.key_column);
    validate_identifier("--id-column", &args.id_column);

    let (from_url, to_url) = resolve_move_urls(args);

    check_psql();

    let filter = build_key_filter(&args.key_column, &args.tenants);
    eprintln!(
        "Moving {} tenant(s) on table {:?}: shard {:?} \u{2192} shard {:?}",
        args.tenants.len(),
        args.table,
        args.from,
        args.to
    );
    eprintln!(
        "\u{26A0}\u{FE0F}  These tenant(s) MUST be quiesced (read-only) on source shard {:?} for the\n  \
         whole move — from this copy until the post-cutover delete. A delete on the\n  \
         source mid-move is otherwise resurrected on {:?} after cutover. See the\n  \
         `move-slot` module runbook.",
        args.from, args.to
    );

    // ── 1. Snapshot source and destination ────────────────────────────────
    // Verify first so re-runs with `--confirm` are idempotent: if the dry-run
    // already copied the rows to the destination and the slot was then flipped,
    // re-running with `--confirm` must skip the copy (which would fail on
    // duplicate PKs) and go straight to delete.
    eprintln!("\u{2192} Checking source\u{2026}");
    let src_count = psql_scalar(&from_url, &count_sql(&args.table, &filter));
    // Source rows captured as `md5(row)|id` snapshot entries, so a later
    // `--confirm` deletes exactly the rows that were verified copied — never a
    // row written to the source after this snapshot (a fresh id is absent from
    // the snapshot) and never a row whose content changed in the cutover window
    // (its current md5 no longer matches the staged entry).
    let mut verified_src_entries: Vec<String> = Vec::new();
    if src_count == "0" {
        eprintln!("\u{26A0}\u{FE0F}  No rows found on source shard for the given tenant(s).");
    } else {
        // Verify by content fingerprint that the destination CONTAINS every
        // source row, rather than requiring an exact match. After the dry-run
        // copy and a route flip, new writes for the moved tenant make the
        // destination a superset of the source; a `--confirm` re-run must then
        // skip the copy (re-inserting copied PKs would abort) and proceed to
        // delete the snapshotted source rows.
        let (src_entries, src_fp) =
            source_snapshot(&from_url, &args.table, &args.id_column, &filter);
        verified_src_entries = src_entries;
        let dst_fp = row_fingerprints(&to_url, &args.table, &filter);
        if dest_contains_source(&src_fp, &dst_fp) {
            eprintln!(
                "\u{2713} Destination already contains all source rows (previous dry-run); skipping copy."
            );
        } else {
            // ── 2. Copy source → destination ──────────────────────────────
            eprintln!("\u{2192} Copying rows\u{2026}");
            copy_rows(&from_url, &to_url, &args.table, &filter);

            // Re-verify after copy: destination must contain all source rows
            // (it may legitimately hold additional rows written post-route).
            let dst_fp2 = row_fingerprints(&to_url, &args.table, &filter);
            eprintln!("   source rows: {}", src_fp.len());
            eprintln!("   dest rows:   {}", dst_fp2.len());
            if !dest_contains_source(&src_fp, &dst_fp2) {
                fail(
                    "verification FAILED: destination does not contain all source rows. No rows deleted.",
                );
            }
        }
        eprintln!("\u{2713} Verified: destination contains all source rows.");
    }

    // ── 2b. Advance the destination PK sequence ───────────────────────────
    // PK values are copied verbatim, so the destination's BIGSERIAL/identity
    // sequence may still point below the moved ids and hand out a duplicate on
    // the next insert once traffic is routed there. Reset it to the table's
    // current max (no-op if the column has no owned sequence, or the table is
    // empty). Skipped when nothing was on the source to move.
    if src_count != "0" {
        eprintln!(
            "\u{2192} Advancing destination sequence for {:?}.{:?}\u{2026}",
            args.table, args.id_column
        );
        run_psql(
            &to_url,
            &[
                "--single-transaction",
                "-c",
                &reset_sequence_sql(&args.table, &args.id_column),
            ],
        );
    }

    // ── 3. Delete from source (only with --confirm) ───────────────────────
    if !args.confirm {
        eprintln!(
            "\u{2713} Copy verified but source rows were KEPT (no --confirm).\n  \
             Next, route these tenant(s) to {:?}, then re-run with --confirm to\n  \
             delete the stale source rows.\n  \
             \u{26A0}\u{FE0F}  Routing: prefer the directory router \
             (database.directory_shard_router = true) and pin each moved tenant\n  \
             to {:?}. Do NOT simply remap a hash slot in autumn.toml unless you\n  \
             copied EVERY tenant key in that slot — a slot is shared by all keys\n  \
             that hash to it, so flipping it reroutes co-tenants you did not copy\n  \
             and their rows would be lost.",
            args.to, args.to
        );
        return;
    }
    delete_source_and_report(&from_url, args, &filter, &verified_src_entries);
}

/// `--confirm` path: delete the verified source rows and report the outcome.
/// Fails (non-zero exit) if any moved-tenant rows remain on the source — the
/// fingerprint-guarded delete preserves rows changed/added during the cutover
/// window, and the source must not be treated as emptied while they linger.
fn delete_source_and_report(from_url: &str, args: &MoveSlotArgs, filter: &str, entries: &[String]) {
    eprintln!("\u{2192} Deleting rows from source (--confirm)\u{2026}");
    let remaining =
        delete_source_rows_by_id(from_url, &args.table, &args.id_column, filter, entries);
    if remaining != "0" {
        fail(&format!(
            "{remaining} moved-tenant row(s) still remain on source shard {:?} after delete.\n  \
             These changed during the cutover window (or were written after the snapshot) and\n  \
             were preserved to avoid losing uncopied data. Route the affected tenant(s) to {:?},\n  \
             then re-run this move with --confirm to copy and remove the stragglers. Do NOT\n  \
             decommission the source shard until this reports 0 rows remaining.",
            args.from, args.to
        ));
    }
    eprintln!(
        "\u{2713} Done. Source rows removed; shard {:?} now owns these tenant(s).\n  \
         Confirm routing sends them to {:?}: pin each tenant in the directory\n  \
         router, or (hash routing only) ensure every key in the moved slot was\n  \
         copied — a shared slot must move as a whole or co-tenants lose data.",
        args.to, args.to
    );
}

/// Delete from the source only the rows in `entries` — the `md5(row)|id` source
/// snapshot whose content was just verified present on the destination — rather
/// than everything matching the filter.
///
/// A row written to the source after the snapshot (e.g. a straggler write during
/// the cutover window) is not in `entries`, so it is preserved; this holds even
/// if a per-shard sequence handed that row an id value that also exists on the
/// destination, because `entries` come from the source snapshot, not the
/// destination. A snapshot row *updated* in the cutover window is likewise
/// preserved: the delete matches on the live row's reconstructed `md5(row)|id`,
/// which no longer equals the staged entry once its content changed — so its
/// uncopied new contents are never dropped.
///
/// Returns the number of moved-tenant rows still on the source after the delete
/// (as the raw `count(*)` string): `"0"` means the source is clean, anything else
/// means rows were intentionally preserved and the source must not yet be treated
/// as emptied.
fn delete_source_rows_by_id(
    from_url: &str,
    table: &str,
    id_column: &str,
    filter: &str,
    entries: &[String],
) -> String {
    use std::io::Write as _;

    if entries.is_empty() {
        eprintln!("  No verified source rows for these tenant(s); nothing deleted.");
        return psql_scalar(from_url, &count_sql(table, filter));
    }
    // Stream the snapshot entries into a temp table over stdin and delete by
    // joining against it, all in one transaction. This avoids building one giant
    // `ARRAY[...]` delete that could blow the OS argv / statement-size limit for
    // a large tenant, and keeps the cleanup atomic.
    let delete = delete_by_staged_ids_sql(table, id_column, filter);
    let mut child = Command::new("psql")
        .args([
            from_url,
            "-v",
            "ON_ERROR_STOP=1",
            "--single-transaction",
            "-c",
            stage_ids_create_sql(),
            "-c",
            stage_ids_copy_in_sql(),
            "-c",
            &delete,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| fail(&format!("failed to start delete psql: {e}")));

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for entry in entries {
            // Each entry is `md5(row)|id::text` from the snapshot; one per line
            // for the single-column `\copy ... FROM STDIN`.
            writeln!(stdin, "{entry}")
                .unwrap_or_else(|e| fail(&format!("failed to stream entries to psql: {e}")));
        }
    }

    let status = child
        .wait()
        .unwrap_or_else(|e| fail(&format!("delete psql did not finish: {e}")));
    if !status.success() {
        fail("source delete failed (see psql output above).");
    }

    // The fingerprint-guarded delete intentionally preserves any moved-tenant row
    // that changed in the cutover window (its current md5 no longer matches the
    // staged entry) or was inserted after the snapshot. Re-count what remains so
    // the caller never reports a clean "source removed" while live moved-tenant
    // rows still sit on the source — which would let an operator decommission it
    // and lose those rows.
    psql_scalar(from_url, &count_sql(table, filter))
}

/// Resolve `--from` / `--to` shard names to their primary URLs through the same
/// config + profile + env stack as `autumn migrate`, and refuse the move when
/// they resolve to the same database (distinct names can map to one URL via
/// profile/env overrides; copying a table onto itself would then delete the
/// live rows from that single database).
fn resolve_move_urls(args: &MoveSlotArgs) -> (String, String) {
    let effective = crate::migrate::effective_profile(args.profile.as_deref());
    let table = crate::migrate::read_autumn_toml_table_with_profile(Some(&effective));
    let shards = crate::migrate::resolve_shard_database_urls_from_sources(
        |k| std::env::var(k),
        table.as_ref(),
    );

    let from_url = resolve_shard_url(&args.from, &shards).unwrap_or_else(|e| fail(&e));
    let to_url = resolve_shard_url(&args.to, &shards).unwrap_or_else(|e| fail(&e));
    if from_url == to_url {
        fail(
            "--from and --to resolve to the same database URL (check profile / \
             AUTUMN_DATABASE__SHARDS__* overrides); refusing to move a table onto itself",
        );
    }
    (from_url, to_url)
}

/// Resolve a configured shard name to its primary URL.
fn resolve_shard_url(name: &str, shards: &[(String, String)]) -> Result<String, String> {
    if let Some((_, url)) = shards.iter().find(|(n, _)| n == name) {
        return Ok(url.clone());
    }
    let known: Vec<&str> = shards.iter().map(|(n, _)| n.as_str()).collect();
    Err(if known.is_empty() {
        format!("unknown shard {name:?}: no [[database.shards]] entries found")
    } else {
        format!("unknown shard {name:?}. Known shards: {}", known.join(", "))
    })
}

fn is_valid_simple_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphanumeric() && !(i == 0 && c.is_ascii_digit()))
}

/// Validate a plain or schema-qualified SQL identifier (`schema.table` or
/// `column`).  Each dot-separated part must match `[A-Za-z_][A-Za-z0-9_]*`.
/// Rejects anything else so names cannot inject SQL when interpolated.
fn validate_identifier(flag: &str, value: &str) {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.is_empty() || parts.len() > 2 {
        fail(&format!(
            "{flag} {value:?} must be a plain or schema-qualified SQL identifier \
             (e.g. `bookmarks` or `public.bookmarks`)"
        ));
    }
    for part in &parts {
        if !is_valid_simple_identifier(part) {
            fail(&format!(
                "{flag} {value:?} must be a plain or schema-qualified SQL identifier \
                 ([A-Za-z_][A-Za-z0-9_]* parts)"
            ));
        }
    }
}

/// Wrap each dot-separated part of an identifier in double quotes, producing a
/// SQL identifier that is safe for case-sensitive and schema-qualified names.
fn quote_identifier(value: &str) -> String {
    value
        .split('.')
        .map(|part| format!("\"{part}\""))
        .collect::<Vec<_>>()
        .join(".")
}

/// Build the `"key_column"::text = ANY(ARRAY['a','b',...]::text[])` predicate.
/// Single quotes in tenant keys are doubled for SQL safety.
///
/// The column is cast to `text` so the tool works for non-`TEXT` shard keys
/// (e.g. `tenant_id:i64` or a UUID key): the `--tenant` values arrive as strings
/// and comparing both sides as text avoids a `bigint = text[]` / `uuid = text[]`
/// operator mismatch that Postgres would reject before copying any rows.
fn build_key_filter(key_column: &str, tenants: &[String]) -> String {
    let list = tenants
        .iter()
        .map(|t| format!("'{}'", t.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let quoted_col = quote_identifier(key_column);
    format!("{quoted_col}::text = ANY(ARRAY[{list}]::text[])")
}

fn count_sql(table: &str, filter: &str) -> String {
    let qt = quote_identifier(table);
    format!("SELECT count(*) FROM {qt} WHERE {filter}")
}

// NOTE: `SELECT *` preserves column declaration order. Both shards are expected
// to have identical schemas (Autumn migrations run in order on all shards), so
// the order will match. If you have added columns manually outside the migration
// system, verify column order matches before using this tool.
//
// Copying the primary key is intentional: it keeps cross-table FK references
// valid after the move. If your table uses `GENERATED ALWAYS AS IDENTITY`,
// `\copy` will fail; use `GENERATED BY DEFAULT AS IDENTITY` or `BIGSERIAL`
// instead. The destination sequence is advanced automatically after a verified
// copy (see `reset_sequence_sql` / `--id-column`).
fn copy_out_sql(table: &str, filter: &str) -> String {
    let qt = quote_identifier(table);
    format!("\\copy (SELECT * FROM {qt} WHERE {filter}) TO STDOUT")
}

/// Rows are loaded into a session-temp staging table and then inserted with
/// `ON CONFLICT DO NOTHING`, so a confirm run after a partial dry-run (some
/// rows already copied, then more written during the cutover window) inserts
/// only the missing rows instead of aborting on duplicate primary keys.
const STAGING_TABLE: &str = "__autumn_move_staging";

fn staging_create_sql(table: &str) -> String {
    let qt = quote_identifier(table);
    format!("CREATE TEMP TABLE {STAGING_TABLE} (LIKE {qt})")
}

fn staging_copy_in_sql() -> String {
    format!("\\copy {STAGING_TABLE} FROM STDIN")
}

fn staging_insert_sql(table: &str) -> String {
    let qt = quote_identifier(table);
    format!("INSERT INTO {qt} SELECT * FROM {STAGING_TABLE} ON CONFLICT DO NOTHING")
}

/// Advance `table`'s owned sequence for `id_column` to its current max, so the
/// next insert on the destination shard doesn't collide with a copied PK.
///
/// `pg_get_serial_sequence` returns `NULL` when the column has no owned
/// sequence (e.g. a UUID or externally-assigned PK), in which case this is a
/// no-op. An empty table leaves the sequence untouched (`is_called = false`, so
/// the next value is the start). `table`/`id_column` are validated identifiers,
/// so the string literals here cannot inject.
fn reset_sequence_sql(table: &str, id_column: &str) -> String {
    let qt = quote_identifier(table);
    let qcol = quote_identifier(id_column);
    // pg_get_serial_sequence parses its first argument as a (possibly
    // schema-qualified) identifier with normal case folding, so pass the
    // *quoted* table to preserve case-sensitive / schema-qualified names (a raw
    // `Bookmarks` would fold to `bookmarks` and return NULL). Its second
    // argument, however, is treated as a literal column name (not parsed), so
    // the column must be *raw* (a quoted `"id"` would be looked up verbatim and
    // return NULL). Either NULL would make the reset a silent no-op.
    let tbl_lit = qt.replace('\'', "''");
    let col_lit = id_column.replace('\'', "''");
    format!(
        "DO $$ DECLARE seq text := pg_get_serial_sequence('{tbl_lit}', '{col_lit}'); mx bigint; \
         BEGIN IF seq IS NOT NULL THEN SELECT max({qcol}) INTO mx FROM {qt}; \
         PERFORM setval(seq, COALESCE(mx, 1), mx IS NOT NULL); END IF; END $$;"
    )
}

/// Stream rows from the source shard into the destination shard over a pipe,
/// the destination copy wrapped in a single transaction.
fn copy_rows(from_url: &str, to_url: &str, table: &str, filter: &str) {
    let mut src = Command::new("psql")
        .args([
            from_url,
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &copy_out_sql(table, filter),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| fail(&format!("failed to start source psql: {e}")));

    let src_out = src.stdout.take().expect("piped stdout");
    // Single session/transaction: stage the streamed rows, then upsert with
    // ON CONFLICT DO NOTHING so re-runs only insert rows not already present.
    let dst_status = Command::new("psql")
        .args([
            to_url,
            "-v",
            "ON_ERROR_STOP=1",
            "--single-transaction",
            "-c",
            &staging_create_sql(table),
            "-c",
            &staging_copy_in_sql(),
            "-c",
            &staging_insert_sql(table),
        ])
        .stdin(src_out)
        .status()
        .unwrap_or_else(|e| fail(&format!("failed to start destination psql: {e}")));

    let src_status = src
        .wait()
        .unwrap_or_else(|e| fail(&format!("source psql did not finish: {e}")));
    if !src_status.success() || !dst_status.success() {
        fail("copy failed (see psql output above). No rows deleted.");
    }
}

/// Run a one-shot `psql -At -c <sql>` and return its single scalar value.
fn psql_scalar(url: &str, sql: &str) -> String {
    let out = Command::new("psql")
        .args([url, "-v", "ON_ERROR_STOP=1", "-At", "-c", sql])
        .output()
        .unwrap_or_else(|e| fail(&format!("failed to run psql: {e}")));
    if !out.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        fail("psql query failed (see output above)");
    }
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Run a one-shot `psql -At -c <sql>` and return one `String` per output row
/// (empty when there are no rows).
fn psql_lines(url: &str, sql: &str) -> Vec<String> {
    let out = psql_scalar(url, sql);
    if out.is_empty() {
        return Vec::new();
    }
    out.lines().map(str::to_owned).collect()
}

/// Per-row content fingerprints (one `md5(to_jsonb(row))` per matching row,
/// sorted) for the filtered set. Used to recognize a destination that is a
/// *superset* of the source so a post-route `--confirm` re-run skips the copy
/// instead of aborting on already-copied primary keys.
fn row_fingerprints(url: &str, table: &str, filter: &str) -> Vec<String> {
    let qt = quote_identifier(table);
    psql_lines(
        url,
        &format!("SELECT md5(to_jsonb(t)::text) FROM {qt} t WHERE {filter} ORDER BY 1"),
    )
}

/// `SELECT md5(row) || '|' || id::text` for the filtered rows, so each row's
/// content fingerprint and its id are captured together in one snapshot query.
/// The id is cast to text so any id type (bigint, uuid, …) round-trips; the
/// `md5` is fixed 32-hex so it never contains the `|` separator.
fn source_snapshot_sql(table: &str, id_column: &str, filter: &str) -> String {
    let qt = quote_identifier(table);
    let qcol = quote_identifier(id_column);
    format!("SELECT md5(to_jsonb(t)::text) || '|' || {qcol}::text FROM {qt} t WHERE {filter}")
}

/// Capture the filtered source rows as `(entries, fingerprints)` in one query so
/// the two describe exactly the same snapshot. Each `entry` is the raw
/// `md5(row)|id` line: it is what `--confirm` deletes against — the delete
/// reconstructs `md5(row)|id` from each *live* source row and only removes rows
/// whose current content still matches a snapshot entry, so a source row updated
/// in the cutover window (same id, new md5) is preserved rather than dropped with
/// its uncopied contents. The `fingerprints` are the `md5(row)` halves, which
/// verification checks against the destination.
fn source_snapshot(
    url: &str,
    table: &str,
    id_column: &str,
    filter: &str,
) -> (Vec<String>, Vec<String>) {
    let entries = psql_lines(url, &source_snapshot_sql(table, id_column, filter));
    // md5 (32 hex, no `|`) is first, so the first `|` separates it from the id;
    // the id half may itself contain `|`, which is fine — the delete matches the
    // whole entry string, and verification only needs the md5 prefix here.
    let fps = entries
        .iter()
        .filter_map(|line| line.split_once('|').map(|(fp, _)| fp.to_owned()))
        .collect();
    (entries, fps)
}

/// SQL builders for the staged-snapshot delete: the verified source-snapshot
/// entries (`md5(row)|id` lines) are streamed into a temp table (no `ARRAY[...]`
/// argv/SQL-size limit for large tenants) and the delete joins against it. The
/// delete reconstructs `md5(row)|id` from each live source row and removes only
/// rows whose *current* content still matches a staged entry — so a row updated
/// in the cutover window (same id, different md5) is left in place, not dropped
/// with contents that were never copied to the destination.
const fn stage_ids_create_sql() -> &'static str {
    "CREATE TEMP TABLE __autumn_del_ids (entry text)"
}

const fn stage_ids_copy_in_sql() -> &'static str {
    "\\copy __autumn_del_ids FROM STDIN"
}

fn delete_by_staged_ids_sql(table: &str, id_column: &str, filter: &str) -> String {
    let qt = quote_identifier(table);
    let qcol = quote_identifier(id_column);
    // Reconstruct the same `md5(row)|id` string the snapshot captured and match
    // it against the staged entries: only rows whose current content (md5) and
    // id both still match are deleted. The alias `t` mirrors the snapshot query
    // so `to_jsonb(t)` sees the identical column set/order.
    format!(
        "DELETE FROM {qt} t WHERE {filter} AND EXISTS (\
         SELECT 1 FROM __autumn_del_ids d \
         WHERE d.entry = md5(to_jsonb(t)::text) || '|' || t.{qcol}::text)"
    )
}

/// Whether `dst` contains every fingerprint in `src`, counting multiplicity —
/// i.e. the destination holds (at least) all of the source's rows.
fn dest_contains_source(src: &[String], dst: &[String]) -> bool {
    let mut available: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for f in dst {
        *available.entry(f.as_str()).or_default() += 1;
    }
    for f in src {
        match available.get_mut(f.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => return false,
        }
    }
    true
}

fn run_psql(url: &str, args: &[&str]) {
    let mut full = vec![url];
    full.extend_from_slice(args);
    let status = Command::new("psql")
        .args(&full)
        .status()
        .unwrap_or_else(|e| fail(&format!("failed to run psql: {e}")));
    if !status.success() {
        fail("psql command failed (see output above)");
    }
}

fn check_psql() {
    if Command::new("psql").arg("--version").output().is_err() {
        fail(
            "`psql` not found on PATH. Install the PostgreSQL client tools to use `autumn shard move-slot`.",
        );
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("\u{2717} {msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards() -> Vec<(String, String)> {
        vec![
            ("shard0".to_owned(), "postgres://h/s0".to_owned()),
            ("shard1".to_owned(), "postgres://h/s1".to_owned()),
        ]
    }

    #[test]
    fn resolve_shard_url_finds_by_name() {
        assert_eq!(
            resolve_shard_url("shard1", &shards()).unwrap(),
            "postgres://h/s1"
        );
    }

    #[test]
    fn resolve_shard_url_unknown_lists_known() {
        let err = resolve_shard_url("nope", &shards()).unwrap_err();
        assert!(err.contains("shard0") && err.contains("shard1"), "{err}");
    }

    #[test]
    fn resolve_shard_url_unknown_without_shards_explains() {
        let err = resolve_shard_url("shard0", &[]).unwrap_err();
        assert!(err.contains("no [[database.shards]]"), "{err}");
    }

    #[test]
    fn build_key_filter_quotes_column_and_escapes_values() {
        let f = build_key_filter("tenant_id", &["acme".to_owned(), "o'brien".to_owned()]);
        assert_eq!(
            f,
            "\"tenant_id\"::text = ANY(ARRAY['acme', 'o''brien']::text[])"
        );
    }

    #[test]
    fn sql_builders_quote_table_and_interpolate_filter() {
        let f = build_key_filter("tenant_id", &["acme".to_owned()]);
        assert_eq!(
            count_sql("bookmarks", &f),
            "SELECT count(*) FROM \"bookmarks\" WHERE \"tenant_id\"::text = ANY(ARRAY['acme']::text[])"
        );
        assert_eq!(
            source_snapshot_sql("bookmarks", "id", &f),
            "SELECT md5(to_jsonb(t)::text) || '|' || \"id\"::text FROM \"bookmarks\" t \
             WHERE \"tenant_id\"::text = ANY(ARRAY['acme']::text[])"
        );
        assert_eq!(
            delete_by_staged_ids_sql("bookmarks", "id", &f),
            "DELETE FROM \"bookmarks\" t WHERE \"tenant_id\"::text = ANY(ARRAY['acme']::text[]) \
             AND EXISTS (SELECT 1 FROM __autumn_del_ids d \
             WHERE d.entry = md5(to_jsonb(t)::text) || '|' || t.\"id\"::text)"
        );
        assert!(
            copy_out_sql("bookmarks", &f).starts_with("\\copy (SELECT * FROM \"bookmarks\" WHERE")
        );
        assert_eq!(
            staging_create_sql("bookmarks"),
            "CREATE TEMP TABLE __autumn_move_staging (LIKE \"bookmarks\")"
        );
        assert_eq!(
            staging_copy_in_sql(),
            "\\copy __autumn_move_staging FROM STDIN"
        );
        assert_eq!(
            staging_insert_sql("bookmarks"),
            "INSERT INTO \"bookmarks\" SELECT * FROM __autumn_move_staging ON CONFLICT DO NOTHING"
        );
    }

    #[test]
    fn sql_builders_support_schema_qualified_table() {
        let f = build_key_filter("tenant_id", &["acme".to_owned()]);
        assert_eq!(
            count_sql("public.bookmarks", &f),
            "SELECT count(*) FROM \"public\".\"bookmarks\" WHERE \"tenant_id\"::text = ANY(ARRAY['acme']::text[])"
        );
        assert_eq!(
            staging_create_sql("public.bookmarks"),
            "CREATE TEMP TABLE __autumn_move_staging (LIKE \"public\".\"bookmarks\")"
        );
        assert_eq!(
            staging_insert_sql("public.bookmarks"),
            "INSERT INTO \"public\".\"bookmarks\" SELECT * FROM __autumn_move_staging ON CONFLICT DO NOTHING"
        );
    }

    #[test]
    fn reset_sequence_sql_quotes_and_guards() {
        let sql = reset_sequence_sql("bookmarks", "id");
        // Quoted table (case/schema preserved) + raw column passed to
        // pg_get_serial_sequence; a raw `Bookmarks` would fold to `bookmarks`,
        // and a quoted `"id"` would be taken literally — either returns NULL.
        assert!(
            sql.contains("pg_get_serial_sequence('\"bookmarks\"', 'id')"),
            "{sql}"
        );
        // max()/FROM still use quoted identifiers.
        assert!(
            sql.contains("SELECT max(\"id\") INTO mx FROM \"bookmarks\""),
            "{sql}"
        );
        // No-op guards: only when a sequence exists, and empty table is safe.
        assert!(sql.contains("IF seq IS NOT NULL"), "{sql}");
        assert!(
            sql.contains("setval(seq, COALESCE(mx, 1), mx IS NOT NULL)"),
            "{sql}"
        );

        // Schema-qualified table is quoted per-part for pg_get_serial_sequence
        // (and for max()/FROM); the column stays raw.
        let sql = reset_sequence_sql("public.bookmarks", "id");
        assert!(
            sql.contains("pg_get_serial_sequence('\"public\".\"bookmarks\"', 'id')"),
            "{sql}"
        );
    }

    #[test]
    fn validate_identifier_rejects_injection() {
        for ok in ["bookmarks", "tenant_id", "_t", "t1", "public.bookmarks"] {
            assert!(is_valid_schema_or_simple(ok), "{ok} should be valid");
        }
        for bad in ["", "1table", "drop table", "a;b", "a-b", "a.b.c"] {
            assert!(!is_valid_schema_or_simple(bad), "{bad} should be invalid");
        }
    }

    // Mirror of validate_identifier's logic (without process exit) so the
    // predicate is unit-testable.
    fn is_valid_schema_or_simple(value: &str) -> bool {
        let parts: Vec<&str> = value.split('.').collect();
        if parts.is_empty() || parts.len() > 2 {
            return false;
        }
        parts.iter().all(|p| is_valid_simple_identifier(p))
    }

    #[test]
    fn dest_contains_source_handles_subset_superset_and_multiplicity() {
        let v = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        // Empty source is vacuously contained.
        assert!(dest_contains_source(&v(&[]), &v(&[])));
        assert!(dest_contains_source(&v(&[]), &v(&["a"])));

        // Exact match and superset both pass (destination may hold extra rows
        // written after the dry-run copy).
        assert!(dest_contains_source(&v(&["a", "b"]), &v(&["a", "b"])));
        assert!(dest_contains_source(&v(&["a", "b"]), &v(&["a", "b", "c"])));

        // A source row missing on the destination fails (would block deletion).
        assert!(!dest_contains_source(&v(&["a", "b"]), &v(&["a"])));
        assert!(!dest_contains_source(&v(&["a"]), &v(&[])));

        // Multiplicity is respected: two of a needs two of a on the destination.
        assert!(!dest_contains_source(&v(&["a", "a"]), &v(&["a"])));
        assert!(dest_contains_source(&v(&["a", "a"]), &v(&["a", "a", "a"])));
    }
}

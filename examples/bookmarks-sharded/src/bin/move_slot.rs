//! Worked example: move one or more tenants' data from a source shard to a
//! destination shard, safely (issue #1209 §3b).
//!
//! This is the *data movement* half of resharding. It does NOT change routing —
//! you do that only AFTER this tool verifies the copy (see the runbook in
//! `docs/guide/sharding.md`). Order matters:
//! copy → verify → re-route the moved tenants → delete from source.
//!
//! Re-route by PINNING each moved tenant in the directory (`_autumn_shard_
//! directory`), not by remapping a hash slot in `autumn.toml`: a slot is shared
//! by every tenant that hashes to it, so remapping it for a single-tenant move
//! also reroutes co-tenants whose rows were not copied, making their data
//! disappear. Remap a slot only when copying every key in that slot.
//!
//! For the given tenant key(s) it:
//!   1. SELECTs their `bookmarks` rows from the source and INSERTs them into
//!      the destination. The shard-local `BIGSERIAL` id is intentionally not
//!      copied (the destination assigns fresh ids), so the move never collides
//!      on the primary key; `created_at` IS preserved.
//!   2. VERIFYs the move: row counts and an id-independent content checksum
//!      (over tenant_id,url,title,tag) must match on both shards.
//!   3. Deletes the rows from the source shard only with `--confirm`, and only
//!      after verification passes.
//!
//! Usage:
//!   move_slot --from <SRC_URL> --to <DST_URL> --tenant <KEY> [--tenant <KEY> ...] [--confirm]
//!
//! Example (docker-compose stack; move tenant "acme" from shard0 → shard1).
//! Run WITHOUT `--confirm` first: this copies and verifies but leaves the
//! source rows in place, so traffic still routing to shard0 keeps reading and
//! writing them.
//!   cargo run --bin move_slot -- \
//!     --from postgres://autumn:autumn@localhost:5443/bookmarks_shard0 \
//!     --to   postgres://autumn:autumn@localhost:5444/bookmarks_shard1 \
//!     --tenant acme
//!
//! Only AFTER you have cut the slot/route over to shard1 (edit `autumn.toml`
//! or the tenant directory and redeploy) re-run the same command with
//! `--confirm` to delete the now-orphaned rows from the source shard:
//!   cargo run --bin move_slot -- \
//!     --from postgres://autumn:autumn@localhost:5443/bookmarks_shard0 \
//!     --to   postgres://autumn:autumn@localhost:5444/bookmarks_shard1 \
//!     --tenant acme --confirm

use diesel::prelude::*;
use diesel::sql_types::{Array, Text};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

// Self-contained schema: bins do not share the app crate's `schema` module.
diesel::table! {
    bookmarks (id) {
        id -> Int8,
        tenant_id -> Text,
        url -> Text,
        title -> Text,
        tag -> Text,
        created_at -> Timestamp,
    }
}

/// The movable columns of a bookmark — everything except the shard-local id.
#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = bookmarks)]
struct MovableBookmark {
    tenant_id: String,
    url: String,
    title: String,
    tag: String,
    created_at: chrono::NaiveDateTime,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct ChecksumRow {
    #[diesel(sql_type = Text)]
    checksum: String,
}

struct Args {
    from: String,
    to: String,
    tenants: Vec<String>,
    confirm: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut from = None;
    let mut to = None;
    let mut tenants = Vec::new();
    let mut confirm = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--from" => from = it.next(),
            "--to" => to = it.next(),
            "--tenant" => {
                tenants.push(it.next().ok_or("--tenant needs a value")?);
            }
            "--confirm" => confirm = true,
            "-h" | "--help" => {
                eprintln!(
                    "move_slot --from <SRC_URL> --to <DST_URL> \
                     --tenant <KEY> [--tenant <KEY> ...] [--confirm]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let from = from.ok_or("--from is required")?;
    let to = to.ok_or("--to is required")?;
    if tenants.is_empty() {
        return Err("at least one --tenant is required".to_owned());
    }
    Ok(Args {
        from,
        to,
        tenants,
        confirm,
    })
}

/// Count + id-independent content checksum for the tenant set on one shard.
async fn snapshot(
    conn: &mut AsyncPgConnection,
    tenants: &[String],
) -> Result<(i64, String), diesel::result::Error> {
    let count = diesel::sql_query("SELECT count(*) AS n FROM bookmarks WHERE tenant_id = ANY($1)")
        .bind::<Array<Text>, _>(tenants)
        .get_result::<CountRow>(conn)
        .await?
        .n;

    // Hash the sorted concatenation of the movable columns, so the checksum is
    // independent of the shard-local ids (which differ after the copy).
    let checksum = diesel::sql_query(
        "SELECT COALESCE(md5(string_agg(r, '|' ORDER BY r)), '') AS checksum \
         FROM (SELECT tenant_id || chr(31) || url || chr(31) || title || chr(31) || tag AS r \
               FROM bookmarks WHERE tenant_id = ANY($1)) s",
    )
    .bind::<Array<Text>, _>(tenants)
    .get_result::<ChecksumRow>(conn)
    .await?
    .checksum;

    Ok((count, checksum))
}

#[tokio::main]
async fn main() {
    let args = parse_args().unwrap_or_else(|e| {
        eprintln!("✗ {e}\n  See --help.");
        std::process::exit(2);
    });

    eprintln!(
        "🍂 move_slot: {} tenant(s) {:?}\n   from: {}\n   to:   {}",
        args.tenants.len(),
        args.tenants,
        args.from,
        args.to
    );

    let mut src = AsyncPgConnection::establish(&args.from)
        .await
        .unwrap_or_else(|e| fail(&format!("connect to source failed: {e}")));
    let mut dst = AsyncPgConnection::establish(&args.to)
        .await
        .unwrap_or_else(|e| fail(&format!("connect to destination failed: {e}")));

    // ── 1. Copy source → destination ──────────────────────────────────────
    eprintln!("→ Copying rows…");
    let rows: Vec<MovableBookmark> = bookmarks::table
        .filter(bookmarks::tenant_id.eq_any(&args.tenants))
        .select(MovableBookmark::as_select())
        .load(&mut src)
        .await
        .unwrap_or_else(|e| fail(&format!("reading source rows failed: {e}")));

    if rows.is_empty() {
        eprintln!("  Nothing to move (no rows for those tenants on the source).");
        return;
    }

    // A single multi-row INSERT is atomic: either every row lands or none do.
    let inserted = diesel::insert_into(bookmarks::table)
        .values(&rows)
        .execute(&mut dst)
        .await
        .unwrap_or_else(|e| fail(&format!("inserting into destination failed: {e}")));
    eprintln!("  Inserted {inserted} row(s) into the destination.");

    // ── 2. Verify counts + checksum on both shards ────────────────────────
    eprintln!("→ Verifying…");
    let (src_count, src_sum) = snapshot(&mut src, &args.tenants)
        .await
        .unwrap_or_else(|e| fail(&format!("source verification query failed: {e}")));
    let (dst_count, dst_sum) = snapshot(&mut dst, &args.tenants)
        .await
        .unwrap_or_else(|e| fail(&format!("destination verification query failed: {e}")));

    eprintln!("   source: count={src_count} checksum={src_sum}");
    eprintln!("   dest:   count={dst_count} checksum={dst_sum}");

    if src_count != dst_count || src_sum != dst_sum {
        fail("verification FAILED: destination does not match source. No rows deleted.");
    }
    eprintln!("✓ Verified: destination matches source.");

    // ── 3. Delete from source (only with --confirm) ───────────────────────
    if !args.confirm {
        eprintln!(
            "✓ Copy verified but source rows were KEPT (no --confirm).\n  \
             Next steps:\n    \
             1. Route these tenants to the destination shard, then deploy so new\n       \
             writes land there. Prefer PINNING each tenant in the directory\n       \
             (INSERT INTO _autumn_shard_directory (tenant_key, shard_name) …) so\n       \
             ONLY the copied tenants move. Do NOT remap the hash slot in\n       \
             autumn.toml for a single-tenant move: a slot is shared by every\n       \
             tenant that hashes to it, and remapping reroutes those co-tenants\n       \
             too — but their rows were not copied, so their data goes missing.\n       \
             (Remap a slot only when copying every key in that slot.)\n    \
             2. Re-run with --confirm to delete the now-stale source rows."
        );
        return;
    }

    eprintln!("→ Deleting rows from source (--confirm)…");
    let deleted =
        diesel::delete(bookmarks::table.filter(bookmarks::tenant_id.eq_any(&args.tenants)))
            .execute(&mut src)
            .await
            .unwrap_or_else(|e| fail(&format!("deleting source rows failed: {e}")));
    eprintln!(
        "✓ Done. Removed {deleted} source row(s); the destination now owns these tenants.\n  \
         Ensure the slot map in autumn.toml routes them to the destination shard."
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("✗ {msg}");
    std::process::exit(1);
}

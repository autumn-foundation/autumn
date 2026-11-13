//! Ledger profiling harness: statements and buffers per data-capsule
//! export/import on a production-shaped schema.
//!
//! Compiled only with `--features ledger-profile`. Needs a Postgres with `pg_stat_statements`; set `LEDGER_PG_URL` to a server
//! URL without database name (`postgres://u@host:port`). Seeded and
//! deterministic. See `docs/perf/capsule-statements/README.md`.

use autumn_web::gdpr::portability::{CapsuleModel, PgCapsuleStore, export_subject, import_capsule};
use diesel::Connection as _;
use diesel::PgConnection;
use diesel::connection::SimpleConnection as _;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;

const TABLES: usize = 30;
const ROWS: usize = 20_000;
const SUBJECTS: usize = 200;

fn schema() -> String {
    let mut s = String::from(
        "CREATE TABLE users (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT);\n\
         INSERT INTO users (email, name) SELECT 'u'||g||'@x.io', CASE WHEN g%5=0 THEN NULL ELSE 'n'||g END \
         FROM generate_series(1,200) g;\n",
    );
    for t in 0..TABLES {
        // Zipf-like skew: subject 1 owns ~30% of every table.
        s += &format!(
            "CREATE TABLE t{t} (id BIGSERIAL PRIMARY KEY, owner_id BIGINT NOT NULL REFERENCES users(id), \
             seq INT NOT NULL, body TEXT, amount NUMERIC(12,2), note TEXT, at TIMESTAMPTZ NOT NULL);\n\
             INSERT INTO t{t} (owner_id, seq, body, amount, note, at) \
             SELECT CASE WHEN g%10<3 THEN 1 ELSE 1+((g*7919)%{SUBJECTS}) END, g, \
             CASE WHEN g%4=0 THEN NULL ELSE md5(g::text) END, (g%1000)/10.0, \
             CASE WHEN g%3=0 THEN NULL ELSE 'note '||g END, \
             timestamptz '2026-01-01' + (g||' minutes')::interval FROM generate_series(1,{ROWS}) g;\n\
             ANALYZE t{t};\n"
        );
    }
    s
}

fn models() -> Vec<CapsuleModel> {
    let mut m = vec![CapsuleModel::new("users", "id")];
    for t in 0..TABLES {
        m.push(CapsuleModel::new(format!("t{t}"), "owner_id").belongs_to("owner_id", "users"));
    }
    m
}

/// Announce a phase boundary and block until the driver acknowledges it by
/// creating `$LEDGER_GATE/<mark>.ack` (after resetting or snapshotting
/// `pg_stat_statements`), so no query of the next phase runs before that.
async fn gate(mark: &str) {
    eprintln!("LEDGER-MARK {mark}");
    let Ok(dir) = std::env::var("LEDGER_GATE") else {
        return;
    };
    let ack = std::path::Path::new(&dir).join(format!("{mark}.ack"));
    while !ack.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(2).build().expect("pool")
}

#[tokio::test]
#[ignore = "profiling harness: needs LEDGER_PG_URL (Postgres with pg_stat_statements)"]
async fn capsule_export_import_profile() {
    let Ok(base) = std::env::var("LEDGER_PG_URL") else {
        return;
    };
    let mut admin = PgConnection::establish(&format!("{base}/postgres")).expect("admin");
    for sql in [
        "DROP DATABASE IF EXISTS ledger_src",
        "DROP DATABASE IF EXISTS ledger_dst",
        "CREATE DATABASE ledger_src",
        "CREATE DATABASE ledger_dst",
    ] {
        admin.batch_execute(sql).expect("db");
    }
    let ddl = schema();
    PgConnection::establish(&format!("{base}/ledger_src"))
        .expect("src")
        .batch_execute(&ddl)
        .expect("seed");
    // Target: same schema, no rows.
    let mut dst = PgConnection::establish(&format!("{base}/ledger_dst")).expect("dst");
    dst.batch_execute(&ddl).expect("schema");
    dst.batch_execute(
        &(0..TABLES)
            .map(|t| format!("TRUNCATE t{t} RESTART IDENTITY;"))
            .chain(["TRUNCATE users RESTART IDENTITY CASCADE;".to_owned()])
            .collect::<String>(),
    )
    .expect("truncate");

    let models = models();
    let source = PgCapsuleStore::new(pool(&format!("{base}/ledger_src")));
    let target = PgCapsuleStore::new(pool(&format!("{base}/ledger_dst")));

    // Measured window: the caller resets/snapshots pg_stat_statements around
    // the `LEDGER-MARK` lines.
    gate("export-start").await;
    let capsule = export_subject(&models, &source, "1").await.expect("export");
    gate("export-end").await;
    let rows: usize = models.iter().map(|m| capsule.records(&m.table).len()).sum();
    eprintln!("LEDGER-ROWS {rows}");
    gate("import-start").await;
    import_capsule(&capsule, &models, &target)
        .await
        .expect("import");
    gate("import-end").await;
}

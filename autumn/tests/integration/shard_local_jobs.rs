//! Issue #3072, AC2: with shard-local job tables, `enqueue_in_tx` and the
//! shard's data write commit or roll back together, and a worker for that
//! shard runs the job.

#[cfg(all(feature = "db", feature = "test-support", not(feature = "sqlite")))]
mod shard_local_job_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use autumn_web::config::{AutumnConfig, ShardConfig};
    use autumn_web::job;
    use autumn_web::prelude::*;
    use autumn_web::test::{TestApp, TestDb};
    use diesel::prelude::*;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

    /// The job migrations, applied to the control database only. The shard
    /// gets its job table from the runtime.
    const CONTROL_JOB_SCHEMA: [&str; 5] = [
        include_str!("../../migrations/20260513000000_create_job_queue/up.sql"),
        include_str!("../../migrations/20260519000000_add_trace_context_to_jobs/up.sql"),
        include_str!("../../migrations/20260610000000_add_job_uniqueness_concurrency/up.sql"),
        include_str!("../../migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql"),
        include_str!("../../migrations/20260628000000_add_queue_to_jobs/up.sql"),
    ];

    const JOB_NAME: &str = "shard_local_welcome";

    static WELCOMED: AtomicUsize = AtomicUsize::new(0);

    /// The welcome jobs that ran.
    fn welcomed() -> usize {
        AtomicUsize::load(&WELCOMED, Ordering::SeqCst)
    }

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct WelcomeArgs {
        account: String,
    }

    #[job(name = "shard_local_welcome")]
    async fn shard_local_welcome(_state: AppState, _args: WelcomeArgs) -> AutumnResult<()> {
        AtomicUsize::fetch_add(&WELCOMED, 1, Ordering::SeqCst);
        Ok(())
    }

    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    /// A fresh database on the shared container.
    async fn fresh_database(db: &TestDb, name: &str) -> (String, Pool<AsyncPgConnection>) {
        let mut admin = db.pool().get().await.expect("admin connection");
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
            .await
            .expect("drop database");
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .expect("create database");
        let url = match db.url().rsplit_once('/') {
            Some((base, _)) => format!("{base}/{name}"),
            None => panic!("unexpected url {}", db.url()),
        };
        let pool = Pool::builder(AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url))
            .max_size(4)
            .build()
            .expect("pool");
        (url, pool)
    }

    async fn count(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
        let mut conn = pool.get().await.expect("connection");
        diesel::sql_query(sql)
            .get_result::<CountRow>(&mut *conn)
            .await
            .map_or(-1, |row| row.n)
    }

    /// Write an account and enqueue its job in one shard transaction, then
    /// commit or roll back.
    async fn sign_up(pool: &Pool<AsyncPgConnection>, account: &str, commit: bool) {
        let mut pooled = pool.get().await.expect("shard connection");
        let conn: &mut AsyncPgConnection = &mut pooled;
        conn.batch_execute("BEGIN").await.expect("begin");
        diesel::sql_query("INSERT INTO accounts (name) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(account)
            .execute(conn)
            .await
            .expect("insert account");
        job::enqueue_in_tx(
            JOB_NAME,
            WelcomeArgs {
                account: account.to_owned(),
            },
            conn,
        )
        .await
        .expect("enqueue_in_tx on the shard connection");
        let end = if commit { "COMMIT" } else { "ROLLBACK" };
        conn.batch_execute(end).await.expect("end transaction");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn enqueue_in_tx_commits_and_rolls_back_with_the_shard_write() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        AtomicUsize::store(&WELCOMED, 0, Ordering::SeqCst);

        let db = TestDb::shared().await;
        let (control_url, control) = fresh_database(db, "shard_local_jobs_control").await;
        let (shard_url, shard) = fresh_database(db, "shard_local_jobs_shard0").await;
        {
            let mut conn = control.get().await.expect("control connection");
            for sql in CONTROL_JOB_SCHEMA {
                conn.batch_execute(sql).await.expect("control job schema");
            }
            let mut conn = shard.get().await.expect("shard connection");
            conn.batch_execute("CREATE TABLE accounts (name TEXT PRIMARY KEY)")
                .await
                .expect("accounts table");
        }

        let mut config = AutumnConfig::default();
        config.jobs.backend = "postgres".into();
        config.jobs.workers = 1;
        config.jobs.postgres.shard_local = true;
        config.database.primary_url = Some(control_url);
        // `config` first: it replaces the whole config, shards included.
        let _client = TestApp::new()
            .config(config)
            .with_db(control.clone())
            .with_shards(vec![ShardConfig {
                name: "shard0".to_owned(),
                primary_url: shard_url,
                ..Default::default()
            }])
            .jobs(jobs![shard_local_welcome])
            .build();

        // The runtime makes the job table on the shard.
        let mut ready = false;
        for _ in 0..100 {
            if count(&shard, "SELECT COUNT(*) AS n FROM autumn_jobs").await >= 0 {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "the shard must get an autumn_jobs table");

        // Rollback: neither the account nor the job exists.
        sign_up(&shard, "rolled-back", false).await;
        assert_eq!(count(&shard, "SELECT COUNT(*) AS n FROM accounts").await, 0);
        assert_eq!(
            count(&shard, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
            0,
            "a rolled-back enqueue leaves no job"
        );

        // Commit: both exist, on the shard and not on the control database.
        sign_up(&shard, "committed", true).await;
        assert_eq!(count(&shard, "SELECT COUNT(*) AS n FROM accounts").await, 1);
        assert_eq!(
            count(&shard, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
            1,
            "the job row commits with the account row"
        );
        assert_eq!(
            count(&control, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
            0,
            "the job is shard-local"
        );

        // A worker for the shard runs the job.
        for _ in 0..200 {
            if welcomed() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(welcomed(), 1, "the shard worker ran the job once");
    }
}

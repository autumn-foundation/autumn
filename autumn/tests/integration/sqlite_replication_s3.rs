//! The S3 leg of continuous replication (#1628), proved against a real
//! S3-compatible endpoint rather than a mock.
//!
//! `sqlite_replication.rs` proves the *loop* (ship → destroy → point-in-time
//! restore → row equality) over a filesystem destination, with no container, in
//! the ordinary test lane. This file proves the part that file can't: that the
//! `SigV4` signing, addressing, `ListObjectsV2` pagination and error mapping in
//! `replication::s3` actually interoperate with an S3 endpoint.
//!
//! Requires Docker (testcontainers `minio`), so it is `#[ignore]`d. CI's
//! Docker-dependent sweep runs every `#[ignore]`d test in this consolidated
//! binary, so it runs there with no workflow change; locally:
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests -- --ignored replication_s3
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::replication::destination::ReplicaDestination;
use autumn_web::replication::s3::{S3Credentials, S3Destination, S3Settings};
use autumn_web::replication::{
    ReplicationSettings, ReplicationStatus, Replicator, restore, segment,
};
use autumn_web::time::ClockSource;
use chrono::{DateTime, TimeZone as _, Utc};
use diesel::connection::SimpleConnection as _;
use diesel::sql_types::Text;
use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

#[derive(QueryableByName, Debug)]
struct Value {
    #[diesel(sql_type = Text)]
    v: String,
}

fn open_app_db(path: &std::path::Path) -> SqliteConnection {
    let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open sqlite");
    conn.batch_execute(
        "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; PRAGMA busy_timeout = 5000;",
    )
    .expect("pragmas");
    conn
}

fn read_values(path: &std::path::Path) -> Vec<String> {
    let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open restored");
    sql_query("SELECT v FROM t ORDER BY id")
        .load::<Value>(&mut conn)
        .expect("select")
        .into_iter()
        .map(|row| row.v)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers: minio)"]
async fn replicates_to_and_restores_from_a_real_s3_endpoint() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::minio::MinIO;

    let minio = MinIO::default()
        .start()
        .await
        .expect("start MinIO — is Docker running?");
    let host = minio.get_host().await.expect("host");
    let port = minio.get_host_port_ipv4(9000).await.expect("port");
    let endpoint = format!("http://{host}:{port}");

    // The whole replication + restore path is blocking, so it runs on a blocking
    // thread — exactly how the framework drives it in production.
    let result = tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("app.db");
        let mut conn = open_app_db(&db);
        conn.batch_execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL); \
             INSERT INTO t (v) VALUES ('alpha'), ('beta');",
        )
        .expect("seed");

        // MinIO's default root credentials, as shipped by testcontainers-modules.
        let credentials = || S3Credentials {
            access_key_id: "minioadmin".to_owned(),
            secret_access_key: "minioadmin".to_owned(),
        };
        let settings = |bucket: &str| S3Settings {
            bucket: bucket.to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: Some(endpoint.clone()),
            force_path_style: true,
        };

        // MinIO does not auto-create buckets, so the target bucket is created
        // out of band first (see `create_bucket_via_http`).
        create_bucket_via_http(&endpoint, "replicas");
        let destination: Arc<dyn ReplicaDestination> =
            Arc::new(S3Destination::new(settings("replicas"), credentials()).expect("destination"));
        assert!(
            destination.describe().contains("replicas"),
            "describe must name the bucket without leaking credentials: {}",
            destination.describe()
        );
        assert!(!destination.describe().contains("minioadmin"));

        let root = segment::root_prefix(Some("db"), "prod");
        let status = Arc::new(ReplicationStatus::new(destination.describe()));
        let clock = StepClock::new();
        let mut replicator = Replicator::new(
            ReplicationSettings {
                database_path: db.clone(),
                root: root.clone(),
                sync_interval: Duration::from_secs(1),
                snapshot_interval: Duration::from_secs(3600),
                max_wal_bytes: 16 * 1024 * 1024,
                retention: Duration::from_secs(7 * 24 * 3600),
                verify_interval: None,
            },
            Arc::clone(&destination),
            Arc::clone(&status),
        )
        .with_clock(Arc::clone(&clock) as Arc<dyn ClockSource>);

        clock.set(at(0));
        let first = replicator.tick().expect("first tick against MinIO");
        assert!(first.snapshot_taken);
        // The opening tick checkpoints *before* it snapshots, so the base is
        // never behind the WAL — and everything written so far is already inside
        // the database file it just copied, leaving nothing to ship. The local
        // suite pins the same expectation in `prime`.
        assert_eq!(
            first.segments, 0,
            "a generation opens from a freshly checkpointed WAL, so it has nothing to ship"
        );

        sql_query("INSERT INTO t (v) VALUES ('gamma')")
            .execute(&mut conn)
            .expect("insert");
        clock.set(at(10));
        assert_eq!(replicator.tick().expect("second tick").segments, 1);

        // Listing must come back through the real ListObjectsV2 path.
        let keys = destination
            .list(&segment::generations_prefix(&root))
            .expect("list");
        assert!(
            keys.iter()
                .any(|k| k.ends_with(segment::SNAPSHOT_META_OBJECT)),
            "the commit marker must be listed: {keys:?}"
        );

        // Destroy the machine.
        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db.display())));
        }

        let output = dir.path().join("restored.db");
        let outcome =
            restore::restore(destination.as_ref(), &root, None, &output).expect("restore");
        assert!(outcome.frames_replayed > 0);
        assert_eq!(read_values(&output), ["alpha", "beta", "gamma"]);

        // A point-in-time restore against the same endpoint.
        let earlier = dir.path().join("earlier.db");
        restore::restore(destination.as_ref(), &root, Some(at(5)), &earlier).expect("pitr");
        assert_eq!(read_values(&earlier), ["alpha", "beta"]);
    })
    .await;
    result.expect("blocking replication task");
}

/// A fixture instant, deliberately in the past, so the point-in-time leg of this
/// test does not depend on how long `MinIO` took to answer.
fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0)
        .single()
        .expect("timestamp")
}

/// A clock this test steps by hand. The replicator stamps each artifact from its
/// own clock at the moment that artifact's contents are fenced, so the instants a
/// point-in-time restore selects on come from here.
struct StepClock(std::sync::Mutex<DateTime<Utc>>);

impl StepClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(at(0))))
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().expect("clock") = now;
    }
}

impl ClockSource for StepClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

/// Create a bucket with a bare signed PUT. `S3Destination` deliberately refuses
/// an empty object key (it is an object store, not a bucket manager), so the
/// bucket itself is created out of band with the same credentials.
fn create_bucket_via_http(endpoint: &str, bucket: &str) {
    use autumn_web::sigv4;

    let now = Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let host = endpoint
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_owned();
    let headers = {
        let mut headers: Vec<sigv4::Header> = vec![
            ("host".to_owned(), host),
            (
                "x-amz-content-sha256".to_owned(),
                sigv4::EMPTY_PAYLOAD_SHA256.to_owned(),
            ),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        headers
    };
    let path = format!("/{bucket}");
    let canonical =
        sigv4::canonical_request("PUT", &path, "", &headers, sigv4::EMPTY_PAYLOAD_SHA256);
    let scope = sigv4::credential_scope(&date, "us-east-1", "s3");
    let signature = sigv4::signature(
        "minioadmin",
        &date,
        "us-east-1",
        "s3",
        &amz_date,
        &scope,
        &canonical,
    );
    let authorization = sigv4::authorization_header("minioadmin", &scope, &headers, &signature);

    let response = reqwest::blocking::Client::new()
        .put(format!("{endpoint}{path}"))
        .header("x-amz-date", &amz_date)
        .header("x-amz-content-sha256", sigv4::EMPTY_PAYLOAD_SHA256)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .send()
        .expect("create bucket");
    assert!(
        response.status().is_success() || response.status() == 409,
        "creating the bucket failed: {}",
        response.status()
    );
}

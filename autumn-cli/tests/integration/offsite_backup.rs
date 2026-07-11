//! Integration test for offsite S3 backups (issue #1619, AC #8).
//!
//! Proves the full disaster-recovery loop against real containers:
//! seed → `autumn db backup --upload` → destroy the local artifact + drop the
//! seeded rows → `autumn db restore offsite:dev/latest` → row-level equality.
//!
//! Requires Docker (via testcontainers, `postgres` + `minio` modules) AND
//! `pg_dump`/`pg_restore` on `PATH`, so it is `#[ignore]`d and only runs when
//! explicitly requested:
//!
//! ```text
//! cargo test -p autumn-cli --test cli_tests -- --ignored --nocapture offsite
//! ```
//!
//! The test drives the real `autumn` binary as a subprocess with a per-child
//! working directory and environment (never mutating this process's cwd or env),
//! so it has no process-wide side effects and lives in the consolidated test
//! binary per the workspace test-layout guidelines.

use std::path::Path;
use std::process::Command;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, Option<i32>) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn run_autumn_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?} failed (exit={code:?})\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

// ─── Minimal SigV4 bucket-creation helper (test-only) ────────────────────────
//
// The `autumn` binary has no library target, so an integration test cannot reach
// its internal S3 client — the bucket must be created out-of-band. This tiny
// signer only needs to `PUT /{bucket}` once. It independently cross-checks the
// binary's own SigV4 (both must interoperate with the same MinIO for the test to
// pass).

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(data);
    m.finalize().into_bytes().into()
}

async fn create_bucket(endpoint: &str, access: &str, secret: &str, region: &str, bucket: &str) {
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(b"");
    let url = url::Url::parse(endpoint).unwrap();
    let host_str = url.host_str().unwrap().to_owned();
    let host = url
        .port()
        .map_or_else(|| host_str.clone(), |p| format!("{host_str}:{p}"));
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical =
        format!("PUT\n/{bucket}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let signature = hex::encode(hmac(&k, sts.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed_headers}, \
         Signature={signature}"
    );
    let resp = reqwest::Client::new()
        .put(format!("{endpoint}/{bucket}"))
        .header("x-amz-date", &amz_date)
        .header("x-amz-content-sha256", &payload_hash)
        .header("authorization", authorization)
        .send()
        .await
        .expect("create bucket request");
    // 200 OK or 409 (already owned) are both fine.
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 409,
        "unexpected create-bucket status: {}",
        resp.status()
    );
}

/// Sign and send an arbitrary S3 request (test-only `SigV4`), returning the
/// response. `canonical_query` must already be canonical (sorted + encoded).
/// Used to observe the stored object out-of-band (list keys, HEAD for the `ETag`).
async fn signed_request(
    method: reqwest::Method,
    url: &str,
    canonical_path: &str,
    canonical_query: &str,
    access: &str,
    secret: &str,
    region: &str,
) -> reqwest::Response {
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(b"");
    let parsed = url::Url::parse(url).unwrap();
    let host_str = parsed.host_str().unwrap().to_owned();
    let host = parsed
        .port()
        .map_or_else(|| host_str.clone(), |p| format!("{host_str}:{p}"));
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "{}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        method.as_str()
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let signature = hex::encode(hmac(&k, sts.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed_headers}, \
         Signature={signature}"
    );
    let full = if canonical_query.is_empty() {
        url.to_owned()
    } else {
        format!("{url}?{canonical_query}")
    };
    reqwest::Client::new()
        .request(method, full)
        .header("x-amz-date", &amz_date)
        .header("x-amz-content-sha256", &payload_hash)
        .header("authorization", authorization)
        .send()
        .await
        .expect("signed request")
}

/// Deterministic, incompressible bytes: a SHA-256 keystream, so a Postgres custom
/// dump of this data does NOT shrink under compression and reliably exceeds the
/// multipart part size (proving the multipart path, not a single PUT).
fn incompressible_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut counter: u64 = 0;
    while out.len() < len {
        let mut h = Sha256::new();
        h.update(b"autumn-multipart-seed");
        h.update(counter.to_le_bytes());
        out.extend_from_slice(&h.finalize());
        counter += 1;
    }
    out.truncate(len);
    out
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers: postgres+minio) and pg_dump/pg_restore on PATH"]
async fn offsite_backup_upload_then_restore_round_trips() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::minio::MinIO;
    use testcontainers_modules::postgres::Postgres;
    use tokio_postgres::NoTls;

    // ── Containers ───────────────────────────────────────────────────────────
    let pg = Postgres::default()
        .start()
        .await
        .expect("start Postgres — is Docker running?");
    let pg_host = pg.get_host().await.unwrap();
    let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{pg_host}:{pg_port}/postgres");

    let minio = MinIO::default()
        .start()
        .await
        .expect("start MinIO — is Docker running?");
    let minio_host = minio.get_host().await.unwrap();
    let minio_port = minio.get_host_port_ipv4(9000).await.unwrap();
    let endpoint = format!("http://{minio_host}:{minio_port}");
    // testcontainers-modules MinIO default root credentials.
    let access = "minioadmin";
    let secret = "minioadmin";
    let bucket = "autumn-offsite-it";

    create_bucket(&endpoint, access, secret, "us-east-1", bucket).await;

    // ── Seed a known table ───────────────────────────────────────────────────
    let (client, conn) = tokio_postgres::connect(&db_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "DROP TABLE IF EXISTS offsite_rt; \
             CREATE TABLE offsite_rt (id INT PRIMARY KEY, name TEXT NOT NULL); \
             INSERT INTO offsite_rt VALUES (1,'alpha'),(2,'beta'),(3,'gamma');",
        )
        .await
        .unwrap();

    // ── Project dir with an offsite destination ──────────────────────────────
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("autumn.toml"),
        format!(
            "[backup.offsite]\n\
             prefix = \"db\"\n\
             [backup.offsite.s3]\n\
             bucket = \"{bucket}\"\n\
             region = \"us-east-1\"\n\
             endpoint = \"{endpoint}\"\n\
             force_path_style = true\n\
             access_key_id_env = \"AUTUMN_OFFSITE_KEY\"\n\
             secret_access_key_env = \"AUTUMN_OFFSITE_SECRET\"\n"
        ),
    )
    .unwrap();

    let envs = [
        ("AUTUMN_DATABASE__URL", db_url.as_str()),
        ("AUTUMN_OFFSITE_KEY", access),
        ("AUTUMN_OFFSITE_SECRET", secret),
    ];

    // ── Backup + upload ──────────────────────────────────────────────────────
    let (_out, stderr) = run_autumn_ok(dir, &["db", "backup", "--upload"], &envs);
    assert!(
        stderr.contains("Offsite upload verified"),
        "upload should report verification: {stderr}"
    );

    // ── List proves the run is offsite ───────────────────────────────────────
    let (list_out, list_err) = run_autumn_ok(dir, &["db", "offsite", "list"], &envs);
    let listing = format!("{list_out}{list_err}");
    assert!(
        listing.contains("control.dump"),
        "offsite list should show the control artifact: {listing}"
    );

    // ── Destroy local artifact + the seeded rows ─────────────────────────────
    std::fs::remove_dir_all(dir.join("backups")).ok();
    client
        .batch_execute("DELETE FROM offsite_rt;")
        .await
        .unwrap();
    let before: i64 = client
        .query_one("SELECT COUNT(*) FROM offsite_rt", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(before, 0, "rows should be gone before restore");

    // ── Restore from offsite (dev profile → no --force needed) ───────────────
    let (_o, restore_err) = run_autumn_ok(dir, &["db", "restore", "offsite:dev/latest"], &envs);
    assert!(
        restore_err.contains("Restore complete"),
        "restore should complete: {restore_err}"
    );

    // ── Row-level equality with the seed ─────────────────────────────────────
    let rows = client
        .query("SELECT id, name FROM offsite_rt ORDER BY id", &[])
        .await
        .unwrap();
    let got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        got,
        vec![
            (1, "alpha".to_owned()),
            (2, "beta".to_owned()),
            (3, "gamma".to_owned()),
        ],
        "restored rows must equal the seed"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers: postgres+minio) and pg_dump/pg_restore on PATH"]
#[allow(clippy::too_many_lines)]
async fn offsite_backup_uploads_large_artifact_via_multipart() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::minio::MinIO;
    use testcontainers_modules::postgres::Postgres;
    use tokio_postgres::NoTls;

    // ── Containers ───────────────────────────────────────────────────────────
    let pg = Postgres::default()
        .start()
        .await
        .expect("start Postgres — is Docker running?");
    let pg_host = pg.get_host().await.unwrap();
    let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{pg_host}:{pg_port}/postgres");

    let minio = MinIO::default()
        .start()
        .await
        .expect("start MinIO — is Docker running?");
    let minio_host = minio.get_host().await.unwrap();
    let minio_port = minio.get_host_port_ipv4(9000).await.unwrap();
    let endpoint = format!("http://{minio_host}:{minio_port}");
    let access = "minioadmin";
    let secret = "minioadmin";
    let region = "us-east-1";
    let bucket = "autumn-offsite-mp-it";

    create_bucket(&endpoint, access, secret, region, bucket).await;

    // ── Seed incompressible bytea LARGER than the real 64 MiB multipart
    //    threshold so the dump takes the multipart path via the genuine default
    //    threshold — NOT by an env override lowering it (Low 3: the part-size env
    //    tunes only chunking, never the threshold). 72 rows × 1 MiB ≈ 72 MiB of
    //    random data survives custom-format compression and exceeds 64 MiB. ─────
    let (client, conn) = tokio_postgres::connect(&db_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(
            "DROP TABLE IF EXISTS offsite_big; \
             CREATE TABLE offsite_big (id INT PRIMARY KEY, blob BYTEA NOT NULL);",
        )
        .await
        .unwrap();
    // ~1 MiB/row, sized comfortably over the 64 MiB multipart threshold. The seed
    // count is bound once here and reused by the post-restore assertion so the two
    // can never drift (P2 #23).
    let seed_rows: i32 = 72;
    for id in 0..seed_rows {
        let blob = incompressible_bytes(1024 * 1024);
        client
            .execute(
                "INSERT INTO offsite_big (id, blob) VALUES ($1, $2)",
                &[&id, &blob],
            )
            .await
            .unwrap();
    }

    // ── Project dir with an offsite destination ──────────────────────────────
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("autumn.toml"),
        format!(
            "[backup.offsite]\n\
             prefix = \"db\"\n\
             [backup.offsite.s3]\n\
             bucket = \"{bucket}\"\n\
             region = \"{region}\"\n\
             endpoint = \"{endpoint}\"\n\
             force_path_style = true\n\
             access_key_id_env = \"AUTUMN_OFFSITE_KEY\"\n\
             secret_access_key_env = \"AUTUMN_OFFSITE_SECRET\"\n"
        ),
    )
    .unwrap();

    let envs = [
        ("AUTUMN_DATABASE__URL", db_url.as_str()),
        ("AUTUMN_OFFSITE_KEY", access),
        ("AUTUMN_OFFSITE_SECRET", secret),
        // Tune ONLY the part size to 5 MiB (S3's minimum) so the >64 MiB dump
        // splits into many small parts. This does NOT lower the threshold —
        // multipart is triggered by the real 64 MiB default on the large fixture.
        ("AUTUMN_OFFSITE_MULTIPART_PART_SIZE_BYTES", "5242880"),
    ];

    // ── Backup + upload (multipart) ──────────────────────────────────────────
    let (_out, stderr) = run_autumn_ok(dir, &["db", "backup", "--upload"], &envs);
    assert!(
        stderr.contains("Offsite upload verified"),
        "multipart upload should verify: {stderr}"
    );

    // ── Find the uploaded control.dump key via a signed ListObjectsV2 ─────────
    let list_query = format!(
        "list-type=2&prefix={}",
        // prefix "db/" — encode the slash for the query.
        "db%2F"
    );
    let list_resp = signed_request(
        reqwest::Method::GET,
        &format!("{endpoint}/{bucket}"),
        &format!("/{bucket}"),
        &list_query,
        access,
        secret,
        region,
    )
    .await;
    assert!(list_resp.status().is_success(), "list status");
    let list_body = list_resp.text().await.unwrap();
    // Grab the control.dump <Key> from the listing.
    let dump_key = list_body
        .split("<Key>")
        .filter_map(|seg| seg.split("</Key>").next())
        .find(|k| k.ends_with("control.dump"))
        .expect("control.dump key present in listing")
        .to_owned();

    // ── HEAD it: a multipart object's ETag is `<md5>-<partcount>`. Assert the
    //    `-<n>` suffix with n >= 2, proving a multipart (not single-PUT) upload
    //    with multiple parts. The exact count depends on the dump's byte size, so
    //    we assert the shape rather than a brittle fixed number. ───────────────
    let encoded_key = dump_key
        .split('/')
        .map(urlencode_segment)
        .collect::<Vec<_>>()
        .join("/");
    let head_resp = signed_request(
        reqwest::Method::HEAD,
        &format!("{endpoint}/{bucket}/{encoded_key}"),
        &format!("/{bucket}/{encoded_key}"),
        "",
        access,
        secret,
        region,
    )
    .await;
    assert!(head_resp.status().is_success(), "head status");
    let etag = head_resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned();
    let part_count: u32 = etag
        .rsplit_once('-')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or_else(|| {
            panic!("multipart ETag should carry a -<partcount> suffix, got {etag:?}")
        });
    assert!(
        part_count >= 2,
        "a >64 MiB dump at a 5 MiB part size must upload as multiple parts, got {part_count} ({etag:?})"
    );

    // ── And the DR loop still works end-to-end (restore equality). ───────────
    std::fs::remove_dir_all(dir.join("backups")).ok();
    client
        .batch_execute("DELETE FROM offsite_big;")
        .await
        .unwrap();
    let (_o, restore_err) = run_autumn_ok(dir, &["db", "restore", "offsite:dev/latest"], &envs);
    assert!(
        restore_err.contains("Restore complete"),
        "restore of the multipart artifact should complete: {restore_err}"
    );
    let count: i64 = client
        .query_one("SELECT COUNT(*) FROM offsite_big", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        count,
        i64::from(seed_rows),
        "all seeded rows must be restored from the multipart object"
    );
}

/// AWS-URI-encode one path segment (unreserved pass through, else `%XX`).
fn urlencode_segment(seg: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

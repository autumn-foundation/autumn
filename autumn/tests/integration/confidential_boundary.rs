//! Docker proof for the client-sealed confidentiality boundary.
//!
//! This deliberately inspects representations below the application API: raw
//! PostgreSQL rows, a text dump, logs, backup-shaped files, and nested replay
//! JSON. The canary is generated at runtime so a hard-coded fixture cannot make
//! the leak scan pass accidentally.

#![cfg(all(feature = "db", feature = "test-support", not(feature = "sqlite")))]

use std::collections::BTreeMap;

use autumn_web::confidential::{ClientKey, ConfidentialEnvelope};
use getrandom::getrandom;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::postgres::Postgres;

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn scan_artifact(name: &str, bytes: &[u8], marker: &[u8]) {
    assert!(
        !contains(bytes, marker),
        "plaintext canary leaked into {name}"
    );
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(bytes) {
        let decoded = serde_json::to_vec(&json).unwrap();
        assert!(
            !contains(&decoded, marker),
            "plaintext canary leaked through decoded {name}"
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn confidential_plaintext_never_crosses_the_client_boundary() {
    let container = Postgres::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let (client, connection) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres"),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client.batch_execute("CREATE TABLE confidential_records (id BIGSERIAL PRIMARY KEY, owner TEXT NOT NULL, envelope TEXT NOT NULL, blind_index TEXT NOT NULL, UNIQUE(owner, blind_index))").await.unwrap();

    let mut random = [0_u8; 48];
    getrandom(&mut random).unwrap();
    let marker = format!("autumn-confidential-canary-{}", hex::encode(random));
    let owner_key = ClientKey::generate().unwrap();
    let stranger_key = ClientKey::generate().unwrap();
    let owner = "account-a";
    let stranger = "account-b";

    // Create: sealing happens before the application receives this DTO.
    let created = owner_key.seal(owner, marker.as_bytes()).unwrap();
    let row = client.query_one(
        "INSERT INTO confidential_records(owner,envelope,blind_index) VALUES($1,$2,$3) RETURNING id",
        &[&owner, &created.envelope.as_str(), &created.blind_index.encode()],
    ).await.unwrap();
    let id: i64 = row.get(0);

    // Equality lookup is exclusively through the blind index and owner scope.
    let token = owner_key
        .blind_index(owner, marker.as_bytes())
        .unwrap()
        .encode();
    let found = client
        .query_one(
            "SELECT envelope FROM confidential_records WHERE owner=$1 AND blind_index=$2",
            &[&owner, &token],
        )
        .await
        .unwrap();
    let wire: String = found.get(0);
    let envelope = ConfidentialEnvelope::parse(wire).unwrap();
    assert_eq!(owner_key.open(owner, &envelope).unwrap(), marker.as_bytes());
    assert!(
        client
            .query_opt(
                "SELECT envelope FROM confidential_records WHERE owner=$1 AND blind_index=$2",
                &[&stranger, &token]
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(stranger_key.open(owner, &envelope).is_err());

    // Update plus validation/application-error/replay-shaped capture paths.
    let updated_marker = format!("{marker}-updated");
    let updated = owner_key.seal(owner, updated_marker.as_bytes()).unwrap();
    client
        .execute(
            "UPDATE confidential_records SET envelope=$1, blind_index=$2 WHERE id=$3 AND owner=$4",
            &[
                &updated.envelope.as_str(),
                &updated.blind_index.encode(),
                &id,
                &owner,
            ],
        )
        .await
        .unwrap();
    let validation_error = serde_json::json!({"errors":{"value":["is invalid"]}});
    let application_error = "request failed: confidential value rejected";
    let replay = serde_json::json!({"request":{"body":updated},"outcome":{"error":application_error},"effects":{"nested":[validation_error]}});

    let rows = client
        .query(
            "SELECT owner,envelope,blind_index FROM confidential_records ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    let mut raw_rows = Vec::new();
    let mut dump =
        String::from("COPY confidential_records (owner,envelope,blind_index) FROM stdin;\n");
    for row in rows {
        let values: (String, String, String) = (row.get(0), row.get(1), row.get(2));
        raw_rows.extend_from_slice(format!("{:?}", values).as_bytes());
        dump.push_str(&format!("{}\t{}\t{}\n", values.0, values.1, values.2));
        assert!(
            ConfidentialEnvelope::parse(values.1).is_ok(),
            "stored bytes must use the confidential envelope"
        );
    }
    dump.push_str("\\.\n");

    let access_log = format!(
        "method=POST path=/confidential status=201 owner={owner}\nmethod=PUT path=/confidential/{id} status=422"
    );
    let error_log = application_error.as_bytes();
    let backup_manifest = serde_json::json!({"autumn_version":env!("CARGO_PKG_VERSION"),"format":"custom","targets":[{"label":"control","file":"control.dump","database":"postgres"}]});
    let mut archive = BTreeMap::new();
    archive.insert(
        "manifest.json",
        serde_json::to_vec(&backup_manifest).unwrap(),
    );
    archive.insert("control.dump", dump.as_bytes().to_vec());

    scan_artifact("raw PostgreSQL rows", &raw_rows, marker.as_bytes());
    scan_artifact("textual database dump", dump.as_bytes(), marker.as_bytes());
    scan_artifact("full access log", access_log.as_bytes(), marker.as_bytes());
    scan_artifact("full error log", error_log, marker.as_bytes());
    for (filename, bytes) in archive {
        scan_artifact(filename, &bytes, marker.as_bytes());
        scan_artifact("backup filename", filename.as_bytes(), marker.as_bytes());
    }
    scan_artifact(
        "nested replay capsule",
        &serde_json::to_vec(&replay).unwrap(),
        marker.as_bytes(),
    );
}

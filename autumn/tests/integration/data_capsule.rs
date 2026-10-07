//! Portable data capsules (issue #1811): export, verify, view, re-import.
//!
//! These tests use the in-memory store, so they need no database. The
//! Postgres round trip is in `data_capsule_pg`.

use std::path::Path;

use autumn_web::gdpr::portability::{
    CapsuleModel, CapsuleSigner, DataCapsule, DataCapsuleError, FieldSpec, MemoryCapsuleStore,
    export_subject, import_capsule, verify_dir,
};
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
use autumn_web::security::SigningSecretConfig;
use serde_json::{Value, json};

const SECRET: &[u8] = b"capsule-test-secret-0123456789abcdef";

fn signer() -> CapsuleSigner {
    CapsuleSigner::new(SECRET)
}

fn registry() -> GdprRegistry {
    GdprRegistry::new()
        .register(ModelRegistration::hard_delete("users"))
        .register(ModelRegistration::hard_delete("posts"))
        .register(ModelRegistration::anonymize("comments"))
        .capsule(CapsuleModel::new("users", "id"))
        .capsule(CapsuleModel::new("posts", "author_id").belongs_to("author_id", "users"))
        .capsule(
            CapsuleModel::new("comments", "author_id")
                .belongs_to("author_id", "users")
                .belongs_to("post_id", "posts"),
        )
}

fn empty_store() -> MemoryCapsuleStore {
    MemoryCapsuleStore::new()
        .table(
            "users",
            vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("email", "text"),
                FieldSpec::new("bio", "text").nullable(),
            ],
        )
        .table(
            "posts",
            vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("author_id", "bigint"),
                FieldSpec::new("title", "text"),
                FieldSpec::new("meta", "jsonb").nullable(),
            ],
        )
        .table(
            "comments",
            vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("author_id", "bigint"),
                FieldSpec::new("post_id", "bigint"),
                FieldSpec::new("body", "text"),
            ],
        )
}

fn seeded_store() -> MemoryCapsuleStore {
    let store = empty_store();
    store.insert(
        "users",
        json!({"id": 1, "email": "ada@example.com", "bio": "<b>hi</b>"}),
    );
    store.insert(
        "users",
        json!({"id": 2, "email": "bob@example.com", "bio": null}),
    );
    store.insert(
        "posts",
        json!({"id": 10, "author_id": 1, "title": "Hello & welcome", "meta": {"tags": ["a", "b"]}}),
    );
    store.insert(
        "posts",
        json!({"id": 11, "author_id": 2, "title": "Not Ada's", "meta": null}),
    );
    store.insert(
        "comments",
        json!({"id": 100, "author_id": 1, "post_id": 10, "body": "First!"}),
    );
    store.insert(
        "comments",
        json!({"id": 101, "author_id": 1, "post_id": 11, "body": "Nice post, Bob"}),
    );
    store
}

async fn export_ada(store: &MemoryCapsuleStore) -> DataCapsule {
    export_subject(registry().capsule_models(), store, "1")
        .await
        .expect("export must succeed")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read file")
}

// ── Registry ────────────────────────────────────────────────────────────────

#[test]
fn registry_holds_capsule_models_next_to_gdpr_registrations() {
    let registry = registry();
    let tables: Vec<&str> = registry
        .capsule_models()
        .iter()
        .map(|m| m.table.as_str())
        .collect();
    assert_eq!(tables, ["users", "posts", "comments"]);
    assert_eq!(registry.registered_tables().len(), 3);
    let posts = &registry.capsule_models()[1];
    assert_eq!(posts.primary_key, "id");
    assert_eq!(posts.subject_column, "author_id");
    assert_eq!(posts.relationships[0].column, "author_id");
    assert_eq!(posts.relationships[0].target, "users");
}

// ── Export ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn export_takes_only_the_subject_rows_of_each_model() {
    let capsule = export_ada(&seeded_store()).await;
    assert_eq!(capsule.manifest.subject, "1");
    assert_eq!(capsule.records("users").len(), 1);
    assert_eq!(capsule.records("posts").len(), 1);
    assert_eq!(capsule.records("comments").len(), 2);
    assert_eq!(capsule.records("posts")[0]["title"], "Hello & welcome");
}

#[tokio::test]
async fn manifest_describes_models_fields_and_relationships() {
    let capsule = export_ada(&seeded_store()).await;
    let comments = capsule
        .manifest
        .models
        .iter()
        .find(|m| m.table == "comments")
        .expect("comments model");
    assert_eq!(comments.record_count, 2);
    assert_eq!(comments.file, "records/comments.json");
    let names: Vec<&str> = comments.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["id", "author_id", "post_id", "body"]);
    let targets: Vec<&str> = comments
        .relationships
        .iter()
        .map(|r| r.target.as_str())
        .collect();
    assert_eq!(targets, ["users", "posts"]);
}

#[tokio::test]
async fn export_rejects_an_unsafe_table_name() {
    let models = [CapsuleModel::new("users; DROP TABLE users", "id")];
    let err = export_subject(&models, &seeded_store(), "1")
        .await
        .expect_err("unsafe name must fail");
    assert!(matches!(err, DataCapsuleError::InvalidName(_)), "{err:?}");
}

// ── Archive layout ──────────────────────────────────────────────────────────

#[tokio::test]
async fn write_dir_produces_records_manifest_signature_and_viewer() {
    let capsule = export_ada(&seeded_store()).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    capsule.write_dir(&root, &signer()).expect("write");

    for file in [
        "manifest.json",
        "signature.json",
        "records/users.json",
        "records/posts.json",
        "records/comments.json",
        "viewer/index.html",
        "viewer/posts/index.html",
        "viewer/manifest.json",
    ] {
        assert!(root.join(file).is_file(), "{file} must exist");
    }
    let manifest: Value = serde_json::from_str(&read(&root.join("manifest.json"))).unwrap();
    assert_eq!(manifest["format"], "autumn-data-capsule");
    assert_eq!(manifest["format_version"], 1);
    assert!(manifest["files"]["records/posts.json"].is_string());
    assert!(manifest["files"]["viewer/index.html"].is_string());
}

#[tokio::test]
async fn write_dir_refuses_a_directory_that_is_not_empty() {
    let capsule = export_ada(&seeded_store()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.txt"), "user data").unwrap();
    let err = capsule
        .write_dir(dir.path(), &signer())
        .expect_err("non-empty dir");
    assert!(matches!(err, DataCapsuleError::NotEmpty(_)), "{err:?}");
    assert_eq!(read(&dir.path().join("keep.txt")), "user data");
}

#[tokio::test]
async fn write_dir_removes_a_partial_capsule_on_error() {
    let mut capsule = export_ada(&seeded_store()).await;
    capsule.manifest.models[0].table = "bad name".to_owned();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    let err = capsule.write_dir(&root, &signer()).expect_err("bad name");
    assert!(matches!(err, DataCapsuleError::InvalidName(_)), "{err:?}");
    assert!(
        !root.exists(),
        "a failed write must leave no partial capsule"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn capsule_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let capsule = export_ada(&seeded_store()).await;
    let dir = tempfile::tempdir().unwrap();
    // An empty directory that the caller made with a wide mode.
    let root = dir.path().join("capsule");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    capsule.write_dir(&root, &signer()).unwrap();

    for d in ["", "records", "viewer", "viewer/posts"] {
        assert_eq!(mode(&root.join(d)), 0o700, "dir {d:?}");
    }
    for f in [
        "manifest.json",
        "signature.json",
        "records/users.json",
        "viewer/index.html",
    ] {
        assert_eq!(mode(&root.join(f)), 0o600, "file {f}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn write_dir_refuses_a_symlinked_directory() {
    let capsule = export_ada(&seeded_store()).await;
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let link = dir.path().join("capsule");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let err = capsule.write_dir(&link, &signer()).expect_err("symlink");
    assert!(matches!(err, DataCapsuleError::InvalidName(_)), "{err:?}");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
}

// ── Integrity ───────────────────────────────────────────────────────────────

async fn written() -> (tempfile::TempDir, std::path::PathBuf) {
    let capsule = export_ada(&seeded_store()).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    capsule.write_dir(&root, &signer()).expect("write");
    (dir, root)
}

#[tokio::test]
async fn verify_accepts_an_unchanged_capsule() {
    let (_dir, root) = written().await;
    let report = verify_dir(&root, &signer()).expect("verify");
    assert!(report.files_checked >= 8, "{report:?}");
}

#[tokio::test]
async fn verify_detects_a_changed_record_file() {
    let (_dir, root) = written().await;
    let path = root.join("records/posts.json");
    let tampered = read(&path).replace("Hello & welcome", "Hello & goodbye");
    std::fs::write(&path, tampered).unwrap();
    let err = verify_dir(&root, &signer()).expect_err("tamper");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_detects_a_changed_manifest() {
    let (_dir, root) = written().await;
    let path = root.join("manifest.json");
    std::fs::write(
        &path,
        read(&path).replace("\"subject\": \"1\"", "\"subject\": \"2\""),
    )
    .unwrap();
    let err = verify_dir(&root, &signer()).expect_err("tamper");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_detects_an_added_file() {
    let (_dir, root) = written().await;
    std::fs::write(root.join("viewer/evil.js"), "alert(1)").unwrap();
    let err = verify_dir(&root, &signer()).expect_err("extra file");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_detects_a_removed_file() {
    let (_dir, root) = written().await;
    std::fs::remove_file(root.join("records/comments.json")).unwrap();
    let err = verify_dir(&root, &signer()).expect_err("missing file");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_rejects_a_capsule_signed_with_another_key() {
    let (_dir, root) = written().await;
    let other = CapsuleSigner::new(b"another-secret-0123456789abcdefgh");
    let err = verify_dir(&root, &other).expect_err("wrong key");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn read_dir_does_not_load_a_tampered_capsule() {
    let (_dir, root) = written().await;
    std::fs::write(root.join("records/users.json"), "[]").unwrap();
    let err = DataCapsule::read_dir(&root, &signer()).expect_err("tampered");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_rejects_an_unknown_signature_algorithm() {
    let (_dir, root) = written().await;
    let path = root.join("signature.json");
    std::fs::write(&path, read(&path).replace("HMAC-SHA256", "none")).unwrap();
    let err = verify_dir(&root, &signer()).expect_err("algorithm");
    assert!(
        matches!(err, DataCapsuleError::UnsupportedFormat(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn verify_rejects_a_missing_signature() {
    let (_dir, root) = written().await;
    std::fs::remove_file(root.join("signature.json")).unwrap();
    let err = verify_dir(&root, &signer()).expect_err("no signature");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

/// Change the manifest, then sign it again with the correct key.
fn resign(root: &Path, change: impl FnOnce(&mut Value)) {
    let mut manifest: Value = serde_json::from_str(&read(&root.join("manifest.json"))).unwrap();
    change(&mut manifest);
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let signature = json!({"algorithm": "HMAC-SHA256", "signature": signer().sign(&bytes)});
    std::fs::write(root.join("manifest.json"), &bytes).unwrap();
    std::fs::write(root.join("signature.json"), signature.to_string()).unwrap();
}

#[tokio::test]
async fn read_dir_rejects_a_record_count_that_does_not_agree() {
    let (_dir, root) = written().await;
    resign(&root, |m| m["models"][0]["record_count"] = json!(5));
    let err = DataCapsule::read_dir(&root, &signer()).expect_err("count");
    assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_rejects_a_signed_manifest_with_an_unsafe_path() {
    for bad in ["../outside.json", "a/b/c/d", "/etc/passwd", ".hidden"] {
        let (_dir, root) = written().await;
        resign(&root, |m| m["files"][bad] = json!("00"));
        let err = verify_dir(&root, &signer()).expect_err(bad);
        assert!(
            matches!(err, DataCapsuleError::InvalidName(_)),
            "{bad}: {err:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn verify_rejects_a_symlink_before_it_reads_a_file() {
    // A link to /dev/zero would make a read run without end. Verify must
    // find the link first.
    let (_dir, root) = written().await;
    let file = root.join("records/users.json");
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &file).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        verify_dir(&root, &signer())
    })
    .await
    .expect("verify must not read the link");
    assert!(
        matches!(result, Err(DataCapsuleError::Integrity(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn excluded_columns_are_not_in_the_capsule() {
    let models = [CapsuleModel::new("users", "id").exclude("bio")];
    let capsule = export_subject(&models, &seeded_store(), "1").await.unwrap();
    assert!(capsule.records("users")[0].get("bio").is_none());
    let users = capsule.manifest.model("users").unwrap();
    assert!(users.fields.iter().all(|f| f.name != "bio"));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    capsule.write_dir(&root, &signer()).unwrap();
    assert!(!read(&root.join("viewer/users/index.html")).contains("&lt;b&gt;hi"));
}

#[tokio::test]
async fn export_rejects_an_exclusion_that_names_no_column() {
    // A typo must not let the real column into the capsule.
    let models = [CapsuleModel::new("users", "id").exclude("boi")];
    let err = export_subject(&models, &seeded_store(), "1")
        .await
        .expect_err("unknown exclusion");
    assert!(matches!(err, DataCapsuleError::InvalidInput(_)), "{err:?}");
    assert!(err.to_string().contains("boi"), "{err}");
}

#[tokio::test]
async fn export_rejects_blob_and_relationship_columns_that_name_no_column() {
    // A typo must not give a capsule without its blobs, its links, or its
    // records.
    for (model, typo) in [
        (CapsuleModel::new("users", "idd"), "idd"),
        (CapsuleModel::new("users", "id").primary_key("uid"), "uid"),
        (CapsuleModel::new("users", "id").blob("avatarr"), "avatarr"),
        (
            CapsuleModel::new("posts", "author_id").belongs_to("authr_id", "users"),
            "authr_id",
        ),
    ] {
        let err = export_subject(&[model], &seeded_store(), "1")
            .await
            .expect_err("unknown column");
        assert!(matches!(err, DataCapsuleError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains(typo), "{err}");
    }
}

#[tokio::test]
async fn every_viewer_link_points_at_a_file_and_an_anchor() {
    let (_dir, root) = written().await;
    let mut pages = vec![root.join("viewer/index.html")];
    for table in ["users", "posts", "comments"] {
        pages.push(root.join("viewer").join(table).join("index.html"));
    }
    let mut checked = 0;
    for page in &pages {
        let html = read(page);
        for href in html.split("href=\"").skip(1) {
            let href = &href[..href.find('"').unwrap()];
            let (file, anchor) = href.split_once('#').unwrap_or((href, ""));
            let target = page.parent().unwrap().join(file);
            assert!(target.is_file(), "{} -> {href}", page.display());
            if !anchor.is_empty() {
                assert!(
                    read(&target).contains(&format!("id=\"{anchor}\"")),
                    "{} -> {href}",
                    page.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 10, "only {checked} links");
}

#[test]
fn signer_needs_a_configured_secret() {
    let err = CapsuleSigner::from_config(&SigningSecretConfig::default())
        .expect_err("no secret must fail");
    assert!(
        matches!(err, DataCapsuleError::MissingSigningSecret),
        "{err:?}"
    );
}

#[test]
fn signer_accepts_a_previous_secret_for_verification() {
    let config = SigningSecretConfig {
        secret: Some("new-secret-0123456789abcdefghijkl".to_owned()),
        previous_secrets: vec![String::from_utf8(SECRET.to_vec()).unwrap()],
    };
    let rotated = CapsuleSigner::from_config(&config).expect("signer");
    let message = b"manifest";
    let old_signature = signer().sign(message);
    assert!(rotated.verify(message, &old_signature));
    assert_ne!(rotated.sign(message), old_signature);
}

// ── Viewer ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn viewer_is_offline_and_escapes_values() {
    let (_dir, root) = written().await;
    for page in [
        "viewer/index.html",
        "viewer/users/index.html",
        "viewer/posts/index.html",
    ] {
        let html = read(&root.join(page));
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "{page}"
        );
        assert!(!html.contains("<script"), "{page} must have no script");
        assert!(html.contains("Content-Security-Policy"), "{page}");
    }
    let users = read(&root.join("viewer/users/index.html"));
    assert!(users.contains("&lt;b&gt;hi&lt;/b&gt;"), "{users}");
    assert!(!users.contains("<b>hi</b>"));
}

#[tokio::test]
async fn viewer_links_follow_relationships_both_ways() {
    let (_dir, root) = written().await;
    let comments = read(&root.join("viewer/comments/index.html"));
    assert!(comments.contains("id=\"r-100\""), "{comments}");
    assert!(
        comments.contains("href=\"../posts/index.html#r-10\""),
        "{comments}"
    );
    assert!(
        comments.contains("href=\"../users/index.html#r-1\""),
        "{comments}"
    );
    // Post 11 belongs to Bob, so it is not in Ada's capsule: no dead link.
    assert!(!comments.contains("#r-11\""), "{comments}");

    let posts = read(&root.join("viewer/posts/index.html"));
    assert!(
        posts.contains("href=\"../comments/index.html#r-100\""),
        "{posts}"
    );

    let index = read(&root.join("viewer/index.html"));
    assert!(index.contains("href=\"posts/index.html\""), "{index}");
}

// ── Import ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_is_lossless_at_field_level() {
    let source = seeded_store();
    let first = export_ada(&source).await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    first.write_dir(&root, &signer()).unwrap();
    let loaded = DataCapsule::read_dir(&root, &signer()).expect("read");

    let target = empty_store();
    let summary = import_capsule(&loaded, registry().capsule_models(), &target)
        .await
        .expect("import");
    assert_eq!(summary.records, 4);

    let second = export_ada(&target).await;
    for table in ["users", "posts", "comments"] {
        assert_eq!(first.records(table), second.records(table), "{table}");
        assert_eq!(loaded.records(table), second.records(table), "{table}");
    }
    let second_dir = dir.path().join("again");
    second.write_dir(&second_dir, &signer()).unwrap();
    for table in ["users", "posts", "comments"] {
        let file = format!("records/{table}.json");
        assert_eq!(
            read(&root.join(&file)),
            read(&second_dir.join(&file)),
            "{file}"
        );
    }
}

#[tokio::test]
async fn import_writes_parents_before_children() {
    let capsule = export_ada(&seeded_store()).await;
    let target = empty_store();
    let summary = import_capsule(&capsule, registry().capsule_models(), &target)
        .await
        .expect("import");
    let order: Vec<&str> = summary.tables.iter().map(|t| t.table.as_str()).collect();
    assert_eq!(order, ["users", "posts", "comments"]);
}

#[tokio::test]
async fn import_rejects_a_table_the_app_did_not_register() {
    let capsule = export_ada(&seeded_store()).await;
    let only_users = [CapsuleModel::new("users", "id")];
    let err = import_capsule(&capsule, &only_users, &empty_store())
        .await
        .expect_err("unregistered table");
    assert!(matches!(err, DataCapsuleError::UnknownTable(_)), "{err:?}");
}

#[tokio::test]
async fn import_rejects_a_relationship_cycle() {
    let models = [
        CapsuleModel::new("users", "id").belongs_to("id", "comments"),
        CapsuleModel::new("posts", "author_id").belongs_to("author_id", "users"),
        CapsuleModel::new("comments", "author_id").belongs_to("post_id", "posts"),
    ];
    let capsule = export_subject(&models, &seeded_store(), "1").await.unwrap();
    let err = import_capsule(&capsule, &models, &empty_store())
        .await
        .expect_err("cycle");
    assert!(
        matches!(err, DataCapsuleError::RelationshipCycle(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn import_fails_and_writes_nothing_on_a_key_conflict() {
    let capsule = export_ada(&seeded_store()).await;
    let target = empty_store();
    target.insert(
        "comments",
        json!({"id": 101, "author_id": 9, "post_id": 9, "body": "x"}),
    );
    let err = import_capsule(&capsule, registry().capsule_models(), &target)
        .await
        .expect_err("conflict");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    assert!(target.rows("users").is_empty(), "import must be atomic");
}

#[tokio::test]
async fn read_dir_rejects_an_unknown_format_version() {
    let capsule = export_ada(&seeded_store()).await;
    let mut future = capsule;
    future.manifest.format_version = 99;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    future.write_dir(&root, &signer()).unwrap();
    let err = DataCapsule::read_dir(&root, &signer()).expect_err("version");
    assert!(
        matches!(err, DataCapsuleError::UnsupportedFormat(_)),
        "{err:?}"
    );
}

// ── Blobs ───────────────────────────────────────────────────────────────────

#[cfg(feature = "storage")]
mod blobs {
    use std::time::Duration;

    use autumn_web::gdpr::portability::{collect_blobs, restore_blobs};
    use autumn_web::storage::{
        Blob, BlobFuture, BlobMeta, BlobStore, BlobStoreError, ByteStream, LocalBlobStore,
        local::SigningKey,
    };
    use bytes::Bytes;
    use sha2::{Digest as _, Sha256};

    use super::*;

    fn blob_store(root: &Path) -> LocalBlobStore {
        LocalBlobStore::new(
            "local",
            root.to_path_buf(),
            "/_blobs",
            Duration::from_secs(60),
            SigningKey::new(b"blob-key".to_vec()),
            vec![],
        )
        .unwrap()
    }

    fn models() -> Vec<CapsuleModel> {
        vec![
            CapsuleModel::new("users", "id")
                .blob("avatar")
                .blob("cv_key"),
        ]
    }

    fn store() -> MemoryCapsuleStore {
        let store = MemoryCapsuleStore::new().table(
            "users",
            vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("avatar", "jsonb").nullable(),
                FieldSpec::new("cv_key", "text").nullable(),
            ],
        );
        store.insert(
            "users",
            json!({
                "id": 1,
                "avatar": {"provider_id": "local", "key": "avatars/ada.png",
                           "content_type": "image/png", "byte_size": 4},
                "cv_key": "docs/ada-cv.txt",
            }),
        );
        store
    }

    #[tokio::test]
    async fn blobs_travel_with_the_capsule_and_restore_by_key() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        source_blobs
            .put("docs/ada-cv.txt", "text/plain", Bytes::from_static(b"cv"))
            .await
            .unwrap();

        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs)
            .await
            .expect("collect");
        assert_eq!(capsule.manifest.blobs.len(), 2);

        let root = tmp.path().join("capsule");
        capsule.write_dir(&root, &signer()).unwrap();
        let sha = &capsule.manifest.blobs[0].sha256;
        assert!(root.join("blobs").join(sha).is_file());

        let loaded = DataCapsule::read_dir(&root, &signer()).unwrap();
        let target_blobs = blob_store(&tmp.path().join("b"));
        let restored = restore_blobs(&loaded, &target_blobs).await.unwrap();
        assert_eq!(restored, 2);
        assert_eq!(
            target_blobs.get("avatars/ada.png").await.unwrap(),
            Bytes::from_static(b"\x89PNG")
        );
        let head = target_blobs.head("avatars/ada.png").await.unwrap().unwrap();
        assert_eq!(head.content_type, "image/png");
        assert_eq!(
            target_blobs.get("docs/ada-cv.txt").await.unwrap(),
            Bytes::from_static(b"cv")
        );

        let users = read(&root.join("viewer/users/index.html"));
        assert!(
            users.contains(&format!("href=\"../../blobs/{sha}\" download")),
            "{users}"
        );
    }

    #[tokio::test]
    async fn restore_never_overwrites_a_different_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();
        // The CV blob is missing in the source: export skips it.
        assert_eq!(capsule.manifest.blobs.len(), 1);

        let target_blobs = blob_store(&tmp.path().join("b"));
        target_blobs
            .put("avatars/ada.png", "image/png", Bytes::from_static(b"other"))
            .await
            .unwrap();
        let err = restore_blobs(&capsule, &target_blobs)
            .await
            .expect_err("different bytes under the same key");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
        assert_eq!(
            target_blobs.get("avatars/ada.png").await.unwrap(),
            Bytes::from_static(b"other")
        );

        // The same bytes are not a conflict.
        let same = blob_store(&tmp.path().join("c"));
        same.put(
            "avatars/ada.png",
            "image/png",
            Bytes::from_static(b"\x89PNG"),
        )
        .await
        .unwrap();
        assert_eq!(restore_blobs(&capsule, &same).await.unwrap(), 1);

        // The same bytes with a different MIME type are a conflict too.
        let other_type = blob_store(&tmp.path().join("d"));
        other_type
            .put(
                "avatars/ada.png",
                "text/plain",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let err = restore_blobs(&capsule, &other_type)
            .await
            .expect_err("same bytes, different MIME type");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
        let head = other_type.head("avatars/ada.png").await.unwrap().unwrap();
        assert_eq!(head.content_type, "text/plain");
    }

    /// A blob store where another writer takes `raced` just before the
    /// conditional write, `loses_mime` keeps no MIME type, `vanishing` reports
    /// a taken key that is gone again, and `plain` has no conditional create.
    #[derive(Default)]
    struct Odd {
        raced: Option<&'static str>,
        loses_mime: Option<&'static str>,
        vanishing: Option<&'static str>,
        replaced_after: Option<&'static str>,
        stale_meta: Option<&'static str>,
        /// An S3-like etag (not a SHA-256) that changes on every `head`.
        shifting_etag: Option<&'static str>,
        heads: std::sync::atomic::AtomicUsize,
        plain: bool,
    }

    struct OddStore {
        inner: LocalBlobStore,
        odd: Odd,
    }

    impl BlobStore for OddStore {
        fn provider_id(&self) -> &str {
            self.inner.provider_id()
        }
        fn put<'a>(
            &'a self,
            key: &'a str,
            content_type: &'a str,
            bytes: Bytes,
        ) -> BlobFuture<'a, Blob> {
            self.inner.put(key, content_type, bytes)
        }
        fn put_if_absent<'a>(
            &'a self,
            key: &'a str,
            content_type: &'a str,
            bytes: Bytes,
        ) -> BlobFuture<'a, Option<Blob>> {
            Box::pin(async move {
                if self.odd.plain {
                    return Err(BlobStoreError::Unsupported("plain".into()));
                }
                if Some(key) == self.odd.vanishing {
                    return Ok(None);
                }
                if Some(key) == self.odd.raced {
                    self.inner
                        .put(key, "text/plain", Bytes::from_static(b"theirs"))
                        .await?;
                }
                let created = self.inner.put_if_absent(key, content_type, bytes).await?;
                if Some(key) == self.odd.replaced_after {
                    self.inner
                        .put(key, "text/plain", Bytes::from_static(b"theirs"))
                        .await?;
                }
                Ok(created)
            })
        }
        fn put_stream<'a>(
            &'a self,
            key: &'a str,
            content_type: &'a str,
            data: ByteStream<'a>,
        ) -> BlobFuture<'a, Blob> {
            self.inner.put_stream(key, content_type, data)
        }
        fn get<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Bytes> {
            self.inner.get(key)
        }
        fn delete<'a>(&'a self, key: &'a str) -> BlobFuture<'a, ()> {
            self.inner.delete(key)
        }
        fn head<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
            Box::pin(async move {
                let meta = self.inner.head(key).await?;
                Ok(meta.map(|mut meta| {
                    if Some(key) == self.odd.loses_mime {
                        "application/octet-stream".clone_into(&mut meta.content_type);
                    }
                    // The metadata of the bytes before a replacement.
                    if Some(key) == self.odd.stale_meta {
                        meta.etag = Some(hex::encode(Sha256::digest(b"old bytes")));
                    }
                    if Some(key) == self.odd.shifting_etag {
                        let n = self
                            .odd
                            .heads
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        meta.etag = Some(format!("\"{n:032x}\""));
                    }
                    meta
                }))
            })
        }
        fn presigned_url<'a>(
            &'a self,
            key: &'a str,
            expires_in: Duration,
        ) -> BlobFuture<'a, String> {
            self.inner.presigned_url(key, expires_in)
        }
    }

    #[tokio::test]
    async fn export_refuses_blob_metadata_of_other_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = blob_store(&tmp.path().join("a"));
        inner
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let blobs = OddStore {
            inner,
            odd: Odd {
                stale_meta: Some("avatars/ada.png"),
                ..Odd::default()
            },
        };
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        let err = collect_blobs(&mut capsule, &blobs)
            .await
            .expect_err("bytes and metadata of two versions");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn export_refuses_a_blob_whose_etag_changes_during_the_read() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = blob_store(&tmp.path().join("a"));
        inner
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let blobs = OddStore {
            inner,
            odd: Odd {
                shifting_etag: Some("avatars/ada.png"),
                ..Odd::default()
            },
        };
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        let err = collect_blobs(&mut capsule, &blobs)
            .await
            .expect_err("a new version on every read");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn restore_refuses_an_existing_blob_that_changes_during_the_check() {
        let tmp = tempfile::tempdir().unwrap();
        let source = blob_store(&tmp.path().join("a"));
        source
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source).await.unwrap();

        // The target has the same bytes and MIME type, but a new version on
        // every read: the check cannot know which version it saw.
        let inner = blob_store(&tmp.path().join("b"));
        inner
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let target = OddStore {
            inner,
            odd: Odd {
                shifting_etag: Some("avatars/ada.png"),
                ..Odd::default()
            },
        };
        let err = restore_blobs(&capsule, &target)
            .await
            .expect_err("a new version on every read");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn restore_does_not_overwrite_a_blob_written_during_the_import() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        source_blobs
            .put("docs/ada-cv.txt", "text/plain", Bytes::from_static(b"cv"))
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();

        // The check finds both keys free. Then another writer takes the
        // second key before the import writes it.
        let target = OddStore {
            inner: blob_store(&tmp.path().join("b")),
            odd: Odd {
                raced: Some("docs/ada-cv.txt"),
                ..Odd::default()
            },
        };
        let err = restore_blobs(&capsule, &target)
            .await
            .expect_err("lost race");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
        assert_eq!(
            target.get("docs/ada-cv.txt").await.unwrap(),
            Bytes::from_static(b"theirs")
        );
        // The blob that this import wrote before the conflict is gone again.
        assert!(matches!(
            target.get("avatars/ada.png").await,
            Err(BlobStoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn restore_fails_when_the_store_does_not_keep_the_mime_type() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        source_blobs
            .put("docs/ada-cv.txt", "text/plain", Bytes::from_static(b"cv"))
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();

        let target = OddStore {
            inner: blob_store(&tmp.path().join("b")),
            odd: Odd {
                loses_mime: Some("docs/ada-cv.txt"),
                ..Odd::default()
            },
        };
        let err = restore_blobs(&capsule, &target)
            .await
            .expect_err("MIME type not kept");
        assert!(matches!(err, DataCapsuleError::Blob(_)), "{err:?}");
        // Nothing that this import wrote stays.
        for key in ["avatars/ada.png", "docs/ada-cv.txt"] {
            assert!(
                matches!(target.get(key).await, Err(BlobStoreError::NotFound(_))),
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn rollback_keeps_a_blob_that_another_writer_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        source_blobs
            .put("docs/ada-cv.txt", "text/plain", Bytes::from_static(b"cv"))
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();

        // The import writes the avatar, another writer replaces it, then the
        // second blob fails and the import rolls back.
        let target = OddStore {
            inner: blob_store(&tmp.path().join("b")),
            odd: Odd {
                replaced_after: Some("avatars/ada.png"),
                loses_mime: Some("docs/ada-cv.txt"),
                ..Odd::default()
            },
        };
        restore_blobs(&capsule, &target)
            .await
            .expect_err("second blob fails");
        assert_eq!(
            target.get("avatars/ada.png").await.unwrap(),
            Bytes::from_static(b"theirs"),
            "rollback must not delete a blob that is not this import's"
        );
    }

    #[tokio::test]
    async fn restore_reports_a_blob_replaced_right_after_its_create() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put("docs/ada-cv.txt", "text/plain", Bytes::from_static(b"cv"))
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();

        // Another writer replaces the blob with other bytes of the same MIME
        // type just after the import creates it.
        let target = OddStore {
            inner: blob_store(&tmp.path().join("b")),
            odd: Odd {
                replaced_after: Some("docs/ada-cv.txt"),
                ..Odd::default()
            },
        };
        let err = restore_blobs(&capsule, &target)
            .await
            .expect_err("bytes changed");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
        assert_eq!(
            target.get("docs/ada-cv.txt").await.unwrap(),
            Bytes::from_static(b"theirs")
        );
    }

    #[tokio::test]
    async fn restore_needs_a_store_with_a_conditional_create() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();

        let plain = OddStore {
            inner: blob_store(&tmp.path().join("b")),
            odd: Odd {
                plain: true,
                ..Odd::default()
            },
        };
        let err = restore_blobs(&capsule, &plain)
            .await
            .expect_err("no conditional create");
        assert!(matches!(err, DataCapsuleError::NotConfigured(_)), "{err:?}");

        // A store that says "taken" for a key that is free again: the blob
        // is not there, so the import must not report success.
        let vanishing = OddStore {
            inner: blob_store(&tmp.path().join("c")),
            odd: Odd {
                vanishing: Some("avatars/ada.png"),
                ..Odd::default()
            },
        };
        let err = restore_blobs(&capsule, &vanishing)
            .await
            .expect_err("blob is not there");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn two_keys_with_the_same_bytes_keep_their_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        for key in ["avatars/ada.png", "docs/ada-cv.txt"] {
            source_blobs
                .put(key, "image/png", Bytes::from_static(b"same"))
                .await
                .unwrap();
        }
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();
        assert_eq!(capsule.manifest.blobs.len(), 2);
        let root = tmp.path().join("capsule");
        capsule.write_dir(&root, &signer()).unwrap();
        let loaded = DataCapsule::read_dir(&root, &signer()).unwrap();
        let target_blobs = blob_store(&tmp.path().join("b"));
        restore_blobs(&loaded, &target_blobs).await.unwrap();
        for key in ["avatars/ada.png", "docs/ada-cv.txt"] {
            assert_eq!(
                target_blobs.get(key).await.unwrap(),
                Bytes::from_static(b"same"),
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn import_without_a_blob_store_rejects_a_capsule_with_blobs() {
        use std::sync::Arc;

        use autumn_web::gdpr::portability::CapsuleService;

        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put("avatars/ada.png", "image/png", Bytes::from_static(b"png"))
            .await
            .unwrap();
        let source = CapsuleService::new(models(), Arc::new(store()), signer())
            .with_blob_store(Arc::new(source_blobs));
        let root = tmp.path().join("capsule");
        let report = source.export_to("1", &root).await.unwrap();
        assert_eq!(report.blobs, 1);

        let empty = MemoryCapsuleStore::new().table(
            "users",
            vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("avatar", "jsonb").nullable(),
                FieldSpec::new("cv_key", "text").nullable(),
            ],
        );
        let target = CapsuleService::new(models(), Arc::new(empty), signer());
        let err = target.import_from(&root).await.expect_err("no blob store");
        assert!(matches!(err, DataCapsuleError::NotConfigured(_)), "{err:?}");
    }

    #[tokio::test]
    async fn export_without_a_blob_store_rejects_records_with_blob_keys() {
        use std::sync::Arc;

        use autumn_web::gdpr::portability::CapsuleService;

        let tmp = tempfile::tempdir().unwrap();
        let service = CapsuleService::new(models(), Arc::new(store()), signer());
        let root = tmp.path().join("capsule");
        let err = service
            .export_to("1", &root)
            .await
            .expect_err("records point at blobs");
        assert!(matches!(err, DataCapsuleError::NotConfigured(_)), "{err:?}");
        assert!(!root.exists(), "no capsule without its blobs");
    }

    #[tokio::test]
    async fn import_points_blob_handles_at_the_target_store() {
        use std::sync::Arc;

        use autumn_web::gdpr::portability::CapsuleService;

        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put("avatars/ada.png", "image/png", Bytes::from_static(b"png"))
            .await
            .unwrap();
        let source = CapsuleService::new(models(), Arc::new(store()), signer())
            .with_blob_store(Arc::new(source_blobs));
        let root = tmp.path().join("capsule");
        source.export_to("1", &root).await.unwrap();

        // The target store has another provider id.
        let target_blobs = Arc::new(
            LocalBlobStore::new(
                "target",
                tmp.path().join("b"),
                "/_blobs",
                Duration::from_secs(60),
                SigningKey::new(b"blob-key".to_vec()),
                vec![],
            )
            .unwrap(),
        );
        let records = Arc::new(MemoryCapsuleStore::new().table(
            "users",
            vec![
                FieldSpec::new("id", "bigint"),
                FieldSpec::new("avatar", "jsonb").nullable(),
                FieldSpec::new("cv_key", "text").nullable(),
            ],
        ));
        let target = CapsuleService::new(models(), records.clone(), signer())
            .with_blob_store(target_blobs.clone());
        target.import_from(&root).await.expect("import");

        let head = target_blobs.head("avatars/ada.png").await.unwrap().unwrap();
        let rows = records.rows("users");
        let avatar = &rows[0]["avatar"];
        assert_eq!(avatar["provider_id"], "target", "{avatar}");
        assert_eq!(avatar["etag"], json!(head.etag), "{avatar}");
        assert_eq!(avatar["key"], "avatars/ada.png");
        // A plain key string stays as it is.
        assert_eq!(rows[0]["cv_key"], "docs/ada-cv.txt");
    }

    #[tokio::test]
    async fn a_failed_record_import_removes_the_blobs_it_wrote() {
        use std::sync::Arc;

        use autumn_web::gdpr::portability::CapsuleService;

        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put("avatars/ada.png", "image/png", Bytes::from_static(b"png"))
            .await
            .unwrap();
        let source = CapsuleService::new(models(), Arc::new(store()), signer())
            .with_blob_store(Arc::new(source_blobs));
        let root = tmp.path().join("capsule");
        source.export_to("1", &root).await.unwrap();

        // The target has the record already: the record import conflicts
        // after the blobs are written.
        let target_blobs = Arc::new(blob_store(&tmp.path().join("b")));
        let target = CapsuleService::new(models(), Arc::new(store()), signer())
            .with_blob_store(target_blobs.clone());
        let err = target
            .import_from(&root)
            .await
            .expect_err("record conflict");
        assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
        assert!(
            matches!(
                target_blobs.get("avatars/ada.png").await,
                Err(BlobStoreError::NotFound(_))
            ),
            "a failed import leaves no blob"
        );
    }

    #[tokio::test]
    async fn verify_detects_a_changed_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let source_blobs = blob_store(&tmp.path().join("a"));
        source_blobs
            .put(
                "avatars/ada.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            )
            .await
            .unwrap();
        source_blobs
            .put("docs/ada-cv.txt", "text/plain", Bytes::from_static(b"cv"))
            .await
            .unwrap();
        let mut capsule = export_subject(&models(), &store(), "1").await.unwrap();
        collect_blobs(&mut capsule, &source_blobs).await.unwrap();
        let root = tmp.path().join("capsule");
        capsule.write_dir(&root, &signer()).unwrap();
        let sha = capsule.manifest.blobs[0].sha256.clone();
        std::fs::write(root.join("blobs").join(sha), b"evil").unwrap();
        let err = verify_dir(&root, &signer()).expect_err("blob tamper");
        assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
    }
}

// ── Service (shared by the actuator and the CLI) ────────────────────────────

mod service {
    use std::sync::Arc;

    use autumn_web::AppState;
    use autumn_web::gdpr::portability::{CapsuleDirectory, CapsuleService};

    use super::*;

    fn service(store: MemoryCapsuleStore) -> CapsuleService {
        CapsuleService::new(
            registry().capsule_models().to_vec(),
            Arc::new(store),
            signer(),
        )
    }

    #[tokio::test]
    async fn export_import_and_verify_through_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("capsule");
        let report = service(seeded_store())
            .export_to("1", &root)
            .await
            .expect("export");
        assert_eq!(report.subject, "1");
        assert_eq!(report.records, 4);
        assert_eq!(report.blobs, 0);

        let target = service(empty_store());
        assert_eq!(target.verify(&root).await.expect("verify").records, 4);
        let summary = target.import_from(&root).await.expect("import");
        assert_eq!(summary.records, 4);
    }

    #[tokio::test]
    async fn export_rejects_an_empty_subject() {
        let dir = tempfile::tempdir().unwrap();
        let err = service(seeded_store())
            .export_to("", &dir.path().join("c"))
            .await
            .expect_err("empty subject");
        assert!(matches!(err, DataCapsuleError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn capsule_path_accepts_only_a_plain_name_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(empty_store()).with_dir(dir.path());
        assert_eq!(svc.capsule_path("ada-1").unwrap(), dir.path().join("ada-1"));
        for bad in ["../x", "a/b", "", ".hidden", "a\\b"] {
            assert!(
                matches!(svc.capsule_path(bad), Err(DataCapsuleError::InvalidName(_))),
                "{bad}"
            );
        }
        let no_dir = service(empty_store());
        assert!(matches!(
            no_dir.capsule_path("ada-1"),
            Err(DataCapsuleError::NotConfigured(_))
        ));
    }

    #[test]
    fn capsule_names_are_safe_and_do_not_hold_the_subject() {
        // Another user can list the capsule directory: a name must not show
        // who asked for an export.
        let (a, b) = (
            CapsuleService::capsule_name(),
            CapsuleService::capsule_name(),
        );
        assert_ne!(a, b, "each export gets its own name");
        for name in [&a, &b] {
            assert!(name.starts_with("capsule-"), "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{name}"
            );
        }
    }

    #[test]
    fn from_state_needs_registered_capsule_models() {
        let err = CapsuleService::from_state(&AppState::for_test()).expect_err("no models");
        assert!(matches!(err, DataCapsuleError::NotConfigured(_)), "{err:?}");
    }

    #[test]
    fn from_state_prefers_an_installed_service() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test()
            .with_extension(service(empty_store()))
            .with_extension(CapsuleDirectory::new(dir.path()));
        let svc = CapsuleService::from_state(&state).expect("installed service");
        assert_eq!(svc.models().len(), 3);
        assert_eq!(svc.dir(), Some(dir.path()));
    }
}

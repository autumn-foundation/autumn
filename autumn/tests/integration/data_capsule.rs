//! Portable data capsules (issue #1811): export, verify, view, re-import.
//!
//! These tests use the in-memory store, so they need no database. The
//! Postgres round trip is in `data_capsule_pg`.

use std::path::Path;

use autumn_web::gdpr::portability::{
    CapsuleError, CapsuleModel, CapsuleSigner, DataCapsule, FieldSpec, MemoryCapsuleStore,
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
    assert!(matches!(err, CapsuleError::InvalidName(_)), "{err:?}");
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
    assert!(matches!(err, CapsuleError::NotEmpty(_)), "{err:?}");
    assert_eq!(read(&dir.path().join("keep.txt")), "user data");
}

#[tokio::test]
async fn write_dir_removes_a_partial_capsule_on_error() {
    let mut capsule = export_ada(&seeded_store()).await;
    capsule.manifest.models[0].table = "bad name".to_owned();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    let err = capsule.write_dir(&root, &signer()).expect_err("bad name");
    assert!(matches!(err, CapsuleError::InvalidName(_)), "{err:?}");
    assert!(
        !root.exists(),
        "a failed write must leave no partial capsule"
    );
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
    assert!(matches!(err, CapsuleError::Integrity(_)), "{err:?}");
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
    assert!(matches!(err, CapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_detects_an_added_file() {
    let (_dir, root) = written().await;
    std::fs::write(root.join("viewer/evil.js"), "alert(1)").unwrap();
    let err = verify_dir(&root, &signer()).expect_err("extra file");
    assert!(matches!(err, CapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_detects_a_removed_file() {
    let (_dir, root) = written().await;
    std::fs::remove_file(root.join("records/comments.json")).unwrap();
    let err = verify_dir(&root, &signer()).expect_err("missing file");
    assert!(matches!(err, CapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn verify_rejects_a_capsule_signed_with_another_key() {
    let (_dir, root) = written().await;
    let other = CapsuleSigner::new(b"another-secret-0123456789abcdefgh");
    let err = verify_dir(&root, &other).expect_err("wrong key");
    assert!(matches!(err, CapsuleError::Integrity(_)), "{err:?}");
}

#[tokio::test]
async fn read_dir_does_not_load_a_tampered_capsule() {
    let (_dir, root) = written().await;
    std::fs::write(root.join("records/users.json"), "[]").unwrap();
    assert!(DataCapsule::read_dir(&root, &signer()).is_err());
}

#[test]
fn signer_needs_a_configured_secret() {
    let err = CapsuleSigner::from_config(&SigningSecretConfig::default())
        .expect_err("no secret must fail");
    assert!(matches!(err, CapsuleError::MissingSigningSecret), "{err:?}");
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
    let order: Vec<&str> = summary.tables.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(order, ["users", "posts", "comments"]);
}

#[tokio::test]
async fn import_rejects_a_table_the_app_did_not_register() {
    let capsule = export_ada(&seeded_store()).await;
    let only_users = [CapsuleModel::new("users", "id")];
    let err = import_capsule(&capsule, &only_users, &empty_store())
        .await
        .expect_err("unregistered table");
    assert!(matches!(err, CapsuleError::UnknownTable(_)), "{err:?}");
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
    assert!(matches!(err, CapsuleError::RelationshipCycle(_)), "{err:?}");
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
    assert!(matches!(err, CapsuleError::Conflict(_)), "{err:?}");
    assert!(target.rows("users").is_empty(), "import must be atomic");
}

#[tokio::test]
async fn read_dir_rejects_an_unknown_format_version() {
    let capsule = export_ada(&seeded_store()).await;
    let mut future = capsule.clone();
    future.manifest.format_version = 99;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("capsule");
    future.write_dir(&root, &signer()).unwrap();
    let err = DataCapsule::read_dir(&root, &signer()).expect_err("version");
    assert!(matches!(err, CapsuleError::UnsupportedFormat(_)), "{err:?}");
}

// ── Blobs ───────────────────────────────────────────────────────────────────

#[cfg(feature = "storage")]
mod blobs {
    use std::time::Duration;

    use autumn_web::gdpr::portability::{collect_blobs, restore_blobs};
    use autumn_web::storage::{BlobStore, LocalBlobStore, local::SigningKey};
    use bytes::Bytes;

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
            users.contains(&format!("href=\"../../blobs/{sha}\"")),
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
        assert!(matches!(err, CapsuleError::Conflict(_)), "{err:?}");
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
        assert!(matches!(err, CapsuleError::Integrity(_)), "{err:?}");
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
        assert_eq!(target.verify(&root).expect("verify").records, 4);
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
        assert!(matches!(err, CapsuleError::InvalidName(_)), "{err:?}");
    }

    #[test]
    fn capsule_path_accepts_only_a_plain_name_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(empty_store()).with_dir(dir.path());
        assert_eq!(svc.capsule_path("ada-1").unwrap(), dir.path().join("ada-1"));
        for bad in ["../x", "a/b", "", ".hidden", "a\\b"] {
            assert!(
                matches!(svc.capsule_path(bad), Err(CapsuleError::InvalidName(_))),
                "{bad}"
            );
        }
        let no_dir = service(empty_store());
        assert!(matches!(
            no_dir.capsule_path("ada-1"),
            Err(CapsuleError::NotConfigured(_))
        ));
    }

    #[test]
    fn capsule_names_are_safe_for_any_subject() {
        let name = CapsuleService::capsule_name("../../etc/passwd x");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{name}"
        );
        assert!(name.starts_with("capsule-"), "{name}");
    }

    #[test]
    fn from_state_needs_registered_capsule_models() {
        let err = CapsuleService::from_state(&AppState::for_test()).expect_err("no models");
        assert!(matches!(err, CapsuleError::NotConfigured(_)), "{err:?}");
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

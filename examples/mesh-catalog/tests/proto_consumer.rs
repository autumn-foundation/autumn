//! A protobuf consumer outside the workspace reads the catalog's JSON.
//!
//! `proto_consumer/` holds Rust that buffa generates from the committed
//! export, `examples/mesh-storefront/contracts/proto/`. The consumer parses
//! the JSON that the real endpoints send, through the protobuf JSON mapping.
//! `the_generated_consumer_matches_the_proto_export` makes the code again and
//! compares it. Set `UPDATE_GOLDEN=1` to write it again.

use std::path::{Path, PathBuf};

use autumn_web::test::TestApp;

/// The generated messages, one module per protobuf package.
#[allow(
    unused_imports,
    dead_code,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod autumn {
    pub mod catalog {
        include!("proto_consumer/autumn.catalog.mod.rs");
    }
    pub mod mesh_catalog {
        pub mod events {
            include!("proto_consumer/autumn.mesh_catalog.events.mod.rs");
        }
    }
}

use autumn::catalog::{CreateItemRequest, Item};

fn proto_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mesh-storefront/contracts/proto")
}

fn generated_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/proto_consumer")
}

#[tokio::test]
async fn a_buffa_consumer_decodes_the_item_that_get_item_sends() {
    let client = TestApp::new().routes(mesh_catalog::routes()).build();
    let response = client.get("/items/7").send().await;
    response.assert_status(200);
    let item: Item = serde_json::from_str(&response.text()).expect("protobuf JSON");
    assert_eq!(item.id, "7");
    assert_eq!(item.name, "Item 7");
    assert_eq!(item.price_cents, 1299);
    assert_eq!(item.subtitle, None, "JSON null reads as no value");
}

#[tokio::test]
async fn create_item_accepts_the_request_a_buffa_consumer_writes() {
    let request = CreateItemRequest {
        name: "Lamp".to_owned(),
        price_cents: 4200,
        request_id: "r-1".to_owned(),
        note: Some("desk".to_owned()),
        ..CreateItemRequest::default()
    };
    let body = serde_json::to_value(&request).expect("protobuf JSON");
    let client = TestApp::new().routes(mesh_catalog::routes()).build();
    let response = client.post("/items").json(&body).send().await;
    response.assert_status(200);
    let item: Item = serde_json::from_str(&response.text()).expect("protobuf JSON");
    assert_eq!((item.name.as_str(), item.price_cents), ("Lamp", 4200));
}

#[test]
fn a_buffa_consumer_decodes_the_item_repriced_event() {
    let sent = mesh_catalog::ItemRepriced {
        id: "7".to_owned(),
        price_cents: 999,
    };
    let json = serde_json::to_string(&sent).expect("serializes");
    let read: autumn::mesh_catalog::events::ItemRepriced =
        serde_json::from_str(&json).expect("protobuf JSON");
    assert_eq!((read.id.as_str(), read.price_cents), ("7", 999));
}

/// Every `.proto` file under `dir`, relative and with `/`, sorted.
fn proto_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("proto dir")
        .map(|e| e.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            proto_files(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "proto") {
            let relative = path.strip_prefix(root).expect("under root");
            let parts: Vec<String> = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(parts.join("/"));
        }
    }
}

/// The Rust that buffa generates from the committed export, by file name.
fn generate() -> Vec<(String, String)> {
    use buffa::Message as _;
    use prost::Message as _;

    let root = proto_dir();
    let mut files = Vec::new();
    proto_files(&root, &root, &mut files);
    let set = protox::compile(&files, [&root]).expect("protox compiles the export");
    let set = buffa_descriptor::generated::descriptor::FileDescriptorSet::decode_from_slice(
        &set.encode_to_vec(),
    )
    .expect("descriptor set");
    let mut config = buffa_codegen::CodeGenConfig::default();
    config.generate_json = true;
    config.generate_views = false;
    buffa_codegen::generate(&set.file, &files, &config)
        .expect("buffa generates")
        .into_iter()
        .map(|f| (f.name, f.content))
        .collect()
}

#[test]
fn the_generated_consumer_matches_the_proto_export() {
    let dir = generated_dir();
    let generated = generate();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (name, content) in &generated {
            std::fs::write(dir.join(name), content).expect("write");
        }
    }
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("generated dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut names: Vec<String> = generated.iter().map(|(n, _)| n.clone()).collect();
    names.sort();
    assert_eq!(
        on_disk, names,
        "run with UPDATE_GOLDEN=1 and review the diff"
    );
    for (name, content) in &generated {
        let committed = std::fs::read_to_string(dir.join(name)).expect("read");
        assert_eq!(
            &committed, content,
            "tests/proto_consumer/{name} is not what buffa generates from the proto export. \
             Run `UPDATE_GOLDEN=1 cargo test -p mesh-catalog --test proto_consumer` and review \
             the diff."
        );
    }
}

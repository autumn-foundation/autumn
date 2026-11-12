//! Tests for `autumn contracts export --avro`.
//!
//! The release-1 export is a golden tree under
//! `tests/golden/contracts/avro/release-1/`. Set `UPDATE_GOLDEN=1` to write
//! it again, then review the diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/contracts")
}

/// Run `autumn contracts` with `args`. Return the exit code and stderr.
fn autumn(args: &[&std::ffi::OsStr]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .arg("contracts")
        .args(args)
        .output()
        .expect("run autumn");
    (
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Export `from` as Avro to `out`. `extra` adds flags.
fn export_avro(from: &Path, out: &Path, extra: &[&str]) -> (i32, String) {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "export".as_ref(),
        "--from".as_ref(),
        from.as_os_str(),
        "--avro".as_ref(),
        "--out".as_ref(),
        out.as_os_str(),
    ];
    args.extend(extra.iter().map(std::ffi::OsStr::new));
    autumn(&args)
}

fn read_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let key = path
                    .strip_prefix(root)
                    .expect("under root")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(key, std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn release_1() -> PathBuf {
    golden_dir().join("release-1.json")
}

#[test]
fn the_release_1_avro_export_matches_its_golden_tree() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");
    let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints"]);
    assert_eq!(code, 0, "{stderr}");
    let golden = golden_dir().join("avro/release-1");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let _ = std::fs::remove_dir_all(&golden);
        for (path, bytes) in read_tree(&out) {
            let file = golden.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(file, bytes).expect("write");
        }
    }
    assert_eq!(
        read_tree(&out),
        read_tree(&golden),
        "the release-1 Avro export differs from its golden tree. If the change is \
         intended, run `UPDATE_GOLDEN=1 cargo test -p autumn-cli --test cli_tests \
         contracts_avro` and review the diff."
    );
}

#[test]
fn the_example_avro_export_is_the_release_1_events() {
    let example =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/mesh-storefront/contracts/avro");
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");
    let (code, stderr) = export_avro(&release_1(), &out, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        read_tree(&example),
        read_tree(&out),
        "examples/mesh-storefront/contracts/avro differs from the release-1 export. \
         Export it again from the same source."
    );
}

#[test]
fn two_avro_exports_of_one_source_give_the_same_bytes() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let mut trees = Vec::new();
    for n in 1..=2 {
        let out = scratch.path().join(format!("avro-{n}"));
        let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints"]);
        assert_eq!(code, 0, "{stderr}");
        trees.push(read_tree(&out));
    }
    assert!(!trees[0].is_empty());
    assert_eq!(trees[0], trees[1]);
}

#[test]
fn avro_check_passes_when_up_to_date_and_fails_on_a_change() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");

    let (code, stderr) = export_avro(&release_1(), &out, &["--check"]);
    assert_eq!(code, 1, "nothing exported yet: {stderr}");
    assert!(
        stderr.contains("catalog.item_repriced.avsc: is missing"),
        "{stderr}"
    );
    assert!(!out.exists(), "--check writes nothing");

    let (code, stderr) = export_avro(&release_1(), &out, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stderr) = export_avro(&release_1(), &out, &["--check"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("up to date"), "{stderr}");

    let file = out.join("catalog.item_repriced.avsc");
    std::fs::write(&file, "{}\n").expect("edit");
    let (code, stderr) = export_avro(&release_1(), &out, &["--check"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("catalog.item_repriced.avsc: differs"),
        "{stderr}"
    );
}

#[test]
fn avro_check_reads_crlf_files_as_lf() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");
    let (code, stderr) = export_avro(&release_1(), &out, &[]);
    assert_eq!(code, 0, "{stderr}");
    for key in read_tree(&out).keys() {
        let file = out.join(key);
        let text = std::fs::read_to_string(&file).expect("read");
        std::fs::write(&file, text.replace('\n', "\r\n")).expect("write");
    }
    let (code, stderr) = export_avro(&release_1(), &out, &["--check"]);
    assert_eq!(code, 0, "a CRLF checkout is up to date: {stderr}");
}

#[test]
fn avro_export_removes_a_stale_schema_it_wrote_and_keeps_other_files() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");
    let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints"]);
    assert_eq!(code, 0, "{stderr}");

    let marked = |name: &str, key: &str| {
        format!(r#"{{"type": "record", "name": "{name}", "{key}": "x", "fields": []}}"#)
    };
    let stale = out.join("old.topic.avsc");
    std::fs::write(&stale, marked("Old", "autumn.topic")).expect("write");
    let stale_body = out.join("endpoints/catalog.old.request.avsc");
    std::fs::write(&stale_body, marked("Old", "autumn.endpoint")).expect("write");
    let mine = out.join("mine.avsc");
    std::fs::write(&mine, r#"{"type": "record", "name": "Mine", "fields": []}"#).expect("write");
    // A marked schema in another directory is not the export's.
    let kept = out.join("released/v1/old.topic.avsc");
    std::fs::create_dir_all(kept.parent().expect("parent")).expect("mkdir");
    std::fs::write(&kept, marked("Old", "autumn.topic")).expect("write");

    let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints", "--check"]);
    assert_eq!(code, 1, "{stderr}");
    for name in ["old.topic.avsc", "catalog.old.request.avsc"] {
        assert!(
            stderr.contains(&format!("{name}: the export no longer writes it")),
            "{name}: {stderr}"
        );
    }
    assert!(!stderr.contains("released"), "{stderr}");
    assert!(
        stale.exists() && stale_body.exists(),
        "--check removes nothing"
    );

    let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stale.exists(), "a schema the export no longer writes goes");
    assert!(
        !stale_body.exists(),
        "a body schema the export no longer writes goes"
    );
    assert!(
        stderr.contains("old.topic.avsc"),
        "each removal is named: {stderr}"
    );
    assert!(mine.exists(), "a file without the marker stays");
    assert!(kept.exists(), "a schema in another directory stays");
}

#[test]
fn an_export_without_endpoints_names_why_the_body_schemas_go() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");
    let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stderr) = export_avro(&release_1(), &out, &["--check"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("Pass `--endpoints` to keep them"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn avro_export_does_not_write_through_a_link() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("avro");
    let elsewhere = scratch.path().join("elsewhere");
    std::fs::create_dir_all(&out).expect("mkdir");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    std::os::unix::fs::symlink(&elsewhere, out.join("endpoints")).expect("link");
    let (code, stderr) = export_avro(&release_1(), &out, &["--endpoints"]);
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("is a link"), "{stderr}");
    assert_eq!(std::fs::read_dir(&elsewhere).expect("read").count(), 0);
}

#[test]
fn a_refused_avro_export_exits_4_and_writes_nothing() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(release_1()).expect("read")).expect("json");
    let mut bundle = bundle;
    bundle["events"][0]["shape"]["serialized"][1]["wire_type"] =
        serde_json::json!({"kind": "integer", "bits": 64, "signed": false});
    bundle["events"][0]["shape"]["deserialized"][1]["wire_type"] =
        serde_json::json!({"kind": "integer", "bits": 64, "signed": false});
    let from = scratch.path().join("bundle.json");
    std::fs::write(&from, serde_json::to_vec(&bundle).expect("json")).expect("write");
    let out = scratch.path().join("avro");
    let (code, stderr) = export_avro(&from, &out, &[]);
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("`price_cents`"), "{stderr}");
    assert!(stderr.contains("integer(u64)"), "{stderr}");
    assert!(!out.exists(), "a refusal writes nothing");
}

#[test]
fn avro_flags_are_checked_by_the_parser() {
    let from = release_1();
    let cases: &[&[&str]] = &[
        &["--avro"],
        &["--avro", "--proto", "--out", "x"],
        &["--endpoints", "--out", "x"],
        &["--avro", "--lock", "x.lock", "--out", "x"],
        &["--check", "--out", "x"],
    ];
    for extra in cases {
        let mut args: Vec<&std::ffi::OsStr> =
            vec!["export".as_ref(), "--from".as_ref(), from.as_os_str()];
        args.extend(extra.iter().map(std::ffi::OsStr::new));
        let (code, stderr) = autumn(&args);
        assert_eq!(code, 2, "{extra:?}: {stderr}");
    }
}

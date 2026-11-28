//! `autumn schema-org` through the real binary: the exit codes of `audit`,
//! and the output of `list` and `show`.

use std::path::Path;
use std::process::{Command, Output};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run(args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("schema-org")
        .args(args)
        .output()
        .expect("failed to run autumn schema-org")
}

fn page(dir: &Path, name: &str, json: &str) -> String {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("<html><head><script type=\"application/ld+json\">{json}</script></head></html>"),
    )
    .expect("write page");
    path.display().to_string()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn audit_passes_a_complete_page_and_fails_a_broken_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = page(
        dir.path(),
        "good.html",
        r#"{"@context":"https://schema.org","@type":"Organization","name":"Acme",
           "url":"https://acme.test","logo":"https://acme.test/l.png",
           "address":"1 Main St","email":"hi@acme.test","telephone":"+1 555",
           "sameAs":["https://social.test/acme"]}"#,
    );
    let out = run(&["audit", &good]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("0 errors, 0 warnings"),
        "{}",
        stdout(&out)
    );

    let broken = page(
        dir.path(),
        "broken.html",
        r#"{"@context":"https://schema.org","@type":"Recipe","name":"x"}"#,
    );
    let out = run(&["audit", &broken, "--format", "json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json report");
    assert_eq!(report["failed"], true);
    assert!(
        report["errors"].as_u64().is_some_and(|n| n >= 1),
        "{report}"
    );
}

#[test]
fn audit_strict_fails_on_warnings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = page(
        dir.path(),
        "typo.html",
        r#"{"@context":"https://schema.org","@type":"Thing","nmae":"x"}"#,
    );
    assert!(run(&["audit", &path]).status.success());
    assert_eq!(run(&["audit", &path, "--strict"]).status.code(), Some(1));
}

#[test]
fn audit_reads_a_directory_and_rejects_a_missing_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("posts")).expect("mkdir");
    page(
        dir.path(),
        "posts/a.html",
        r#"{"@context":"https://schema.org","@type":"Thing","name":"a"}"#,
    );
    page(
        dir.path(),
        "index.html",
        r#"{"@context":"https://schema.org","@type":"Thing","name":"b"}"#,
    );
    let out = run(&["audit", &dir.path().display().to_string()]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("2 pages"), "{}", stdout(&out));

    let bad = dir.path().join("latin1.html");
    std::fs::write(&bad, [0x3c, 0x70, 0x3e, 0xe9]).expect("write");
    let out = run(&["audit", &bad.display().to_string(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json report");
    assert_eq!(report["failed"], true, "{report}");
    assert_eq!(
        report["pages"][0]["findings"][0]["rule"], "unreadable",
        "{report}"
    );

    let out = run(&["audit", "/no/such/dir"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such file"));
}

#[test]
fn list_and_show_print_the_vocabulary() {
    let out = run(&["list"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("* Recipe"), "{}", stdout(&out));

    let out = run(&["show", "product"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).starts_with("Product  (Thing > Product)"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("offers_price:money"),
        "{}",
        stdout(&out)
    );

    let out = run(&["show", "Prodcut"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Did you mean `Product`"));
}

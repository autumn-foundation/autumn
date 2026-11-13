//! Tests for `autumn contracts export --proto`.
//!
//! The release-1 export is a golden tree under
//! `tests/golden/contracts/proto/release-1/`. Set `UPDATE_GOLDEN=1` to write
//! it again, then review the diff.
//!
//! The `buf_` tests need the `buf` binary (on `PATH`, or in `BUF`). They are
//! `#[ignore]`d, and `.github/workflows/wire-contracts.yml` runs them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/contracts")
}

/// Run `autumn contracts` with `args`. Return the exit code, stdout and
/// stderr.
fn autumn(args: &[&std::ffi::OsStr]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .arg("contracts")
        .args(args)
        .output()
        .expect("run autumn");
    (
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Export `from` (a bundle file or a descriptor directory) as proto to
/// `out`, with the lock at `lock`. `extra` adds flags.
fn export_proto(from: &Path, out: &Path, lock: &Path, extra: &[&str]) -> (i32, String) {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "export".as_ref(),
        "--from".as_ref(),
        from.as_os_str(),
        "--proto".as_ref(),
        "--out".as_ref(),
        out.as_os_str(),
        "--lock".as_ref(),
        lock.as_os_str(),
    ];
    args.extend(extra.iter().map(std::ffi::OsStr::new));
    let (code, _, stderr) = autumn(&args);
    (code, stderr)
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

#[test]
fn the_release_1_proto_export_matches_its_golden_tree() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("proto");
    let lock = out.join("proto.lock");
    let (code, stderr) = export_proto(&golden_dir().join("release-1.json"), &out, &lock, &[]);
    assert_eq!(code, 0, "{stderr}");
    let golden = golden_dir().join("proto/release-1");
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
        "the release-1 proto export differs from its golden tree. If the change is \
         intended, run `UPDATE_GOLDEN=1 cargo test -p autumn-cli --test cli_tests \
         contracts_proto` and review the diff."
    );
}

#[test]
fn the_golden_proto_tree_matches_the_example_export() {
    let example =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/mesh-storefront/contracts");
    let mut committed = read_tree(&example.join("proto"));
    committed.insert(
        "proto.lock".to_owned(),
        std::fs::read(example.join("proto.lock")).expect("example lock"),
    );
    assert_eq!(
        committed,
        read_tree(&golden_dir().join("proto/release-1")),
        "examples/mesh-storefront/contracts/proto and the release-1 golden differ. Export \
         both again from the same source."
    );
}

#[test]
fn two_proto_exports_of_one_source_give_the_same_bytes() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let mut trees = Vec::new();
    for n in 1..=2 {
        let out = scratch.path().join(format!("proto-{n}"));
        let lock = scratch.path().join(format!("proto-{n}.lock"));
        let (code, stderr) = export_proto(&golden_dir().join("release-1.json"), &out, &lock, &[]);
        assert_eq!(code, 0, "{stderr}");
        trees.push((read_tree(&out), std::fs::read(&lock).expect("lock")));
    }
    assert_eq!(trees[0], trees[1]);
}

#[test]
fn proto_check_passes_when_up_to_date_and_fails_on_a_missing_or_stale_lock() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("proto");
    let lock = scratch.path().join("proto.lock");
    let release_1 = golden_dir().join("release-1.json");

    let (code, stderr) = export_proto(&release_1, &out, &lock, &["--check"]);
    assert_eq!(code, 1, "no lock yet: {stderr}");
    assert!(stderr.contains("the lock file is missing"), "{stderr}");
    assert!(!out.exists(), "--check writes nothing");

    let (code, stderr) = export_proto(&release_1, &out, &lock, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stderr) = export_proto(&release_1, &out, &lock, &["--check"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("up to date"), "{stderr}");

    // A field the lock does not have yet.
    let current = golden_dir().join("new_optional_request_field/current.json");
    let (code, stderr) = export_proto(&current, &out, &lock, &["--check"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("adds, moves or reserves"), "{stderr}");
    assert!(stderr.contains("differs from the export"), "{stderr}");

    // A file someone changed by hand.
    let file = out.join("autumn/catalog/catalog.proto");
    std::fs::write(&file, "// edited\n").expect("edit");
    let (code, stderr) = export_proto(&release_1, &out, &lock, &["--check"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("catalog.proto: differs"), "{stderr}");
}

#[test]
fn proto_export_keeps_numbers_and_removes_a_stale_service_file() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("proto");
    let lock = scratch.path().join("proto.lock");
    let (code, stderr) = export_proto(&golden_dir().join("release-1.json"), &out, &lock, &[]);
    assert_eq!(code, 0, "{stderr}");
    let stale = out.join("autumn/old/old.proto");
    std::fs::create_dir_all(stale.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &stale,
        "// Generated by `autumn contracts export --proto`. Do not edit.\nsyntax = \"proto3\";\n",
    )
    .expect("write");
    let before = std::fs::read_to_string(&lock).expect("lock");

    let current = golden_dir().join("new_optional_request_field/current.json");
    let (code, stderr) = export_proto(&current, &out, &lock, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stale.exists(),
        "a file the export no longer writes is removed"
    );
    let after = std::fs::read_to_string(&lock).expect("lock");
    for line in before.lines().filter(|l| l.contains(" = ")) {
        assert!(after.contains(line), "`{line}` moved:\n{after}");
    }
}

#[test]
fn proto_check_reads_crlf_files_as_lf() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("proto");
    let lock = scratch.path().join("proto.lock");
    let release_1 = golden_dir().join("release-1.json");
    let (code, stderr) = export_proto(&release_1, &out, &lock, &[]);
    assert_eq!(code, 0, "{stderr}");
    let mut files: Vec<PathBuf> = read_tree(&out).keys().map(|k| out.join(k)).collect();
    files.push(lock.clone());
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read");
        std::fs::write(&file, text.replace('\n', "\r\n")).expect("write");
    }
    let (code, stderr) = export_proto(&release_1, &out, &lock, &["--check"]);
    assert_eq!(code, 0, "a CRLF checkout is up to date: {stderr}");
}

#[test]
fn proto_export_keeps_a_proto_file_it_did_not_write() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("proto");
    let lock = scratch.path().join("proto.lock");
    let mine = out.join("autumn/mine/mine.proto");
    std::fs::create_dir_all(mine.parent().expect("parent")).expect("mkdir");
    std::fs::write(&mine, "syntax = \"proto3\";\n").expect("write");
    let (code, stderr) = export_proto(&golden_dir().join("release-1.json"), &out, &lock, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(mine.exists(), "a file without the export header stays");
}

#[test]
fn the_default_lock_is_beside_the_output_directory() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let out = scratch.path().join("contracts/proto");
    let (code, _, stderr) = autumn(&[
        "export".as_ref(),
        "--from".as_ref(),
        golden_dir().join("release-1.json").as_os_str(),
        "--proto".as_ref(),
        "--out".as_ref(),
        out.as_os_str(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        scratch.path().join("contracts/proto.lock").is_file(),
        "{stderr}"
    );
}

#[test]
fn the_wire_contracts_workflow_runs_both_buf_tests_by_full_name() {
    let workflow = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/wire-contracts.yml"),
    )
    .expect("workflow");
    for name in [
        "buf_lint_and_buf_build_pass_on_each_proto_export",
        "buf_breaking_finds_no_break_that_contracts_check_approved",
    ] {
        assert!(
            workflow.contains(&format!("integration::contracts_proto::{name}")),
            "wire-contracts.yml must run `{name}` with `--ignored --exact`"
        );
    }
    assert!(workflow.contains("--ignored --exact"));
}

#[test]
fn proto_export_without_out_is_refused() {
    let (code, _, stderr) = autumn(&[
        "export".as_ref(),
        "--from".as_ref(),
        golden_dir().join("release-1.json").as_os_str(),
        "--proto".as_ref(),
    ]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--out"), "{stderr}");
}

/// The `buf` binary.
fn buf() -> PathBuf {
    std::env::var_os("BUF").map_or_else(|| PathBuf::from("buf"), PathBuf::from)
}

/// Run `buf` in `dir`. Returns its exit code and its output.
fn run_buf(dir: &Path, args: &[&std::ffi::OsStr]) -> (i32, String) {
    let out = Command::new(buf())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "run buf ({}): {e}. Install buf or set BUF.",
                buf().display()
            )
        });
    (
        out.status.code().expect("exit code"),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Every golden proto tree.
fn proto_trees() -> Vec<PathBuf> {
    let mut trees: Vec<PathBuf> = std::fs::read_dir(golden_dir().join("proto"))
        .expect("golden proto dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.join("buf.yaml").is_file())
        .collect();
    trees.sort();
    trees
}

/// Copy `tree` to `to`. A tree with the googleapis dependency gets the test
/// stub of `google/api` in its place, so buf needs no network. buf does not
/// lint the stub.
fn offline_copy(tree: &Path, to: &Path) {
    let copy = |from: &Path, into: &Path| {
        for (path, bytes) in read_tree(from) {
            let file = into.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(file, bytes).expect("write");
        }
    };
    copy(tree, to);
    let config = std::fs::read_to_string(to.join("buf.yaml")).expect("buf.yaml");
    if config.contains("deps:") {
        let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/googleapis-stub");
        copy(&stub, to);
        let config = config
            .replace("deps:\n  - buf.build/googleapis/googleapis\n", "")
            .replace("lint:\n", "lint:\n  ignore:\n    - google\n");
        std::fs::write(to.join("buf.yaml"), config).expect("buf.yaml");
    }
}

#[test]
#[ignore = "requires buf"]
fn buf_lint_and_buf_build_pass_on_each_proto_export() {
    let trees = proto_trees();
    assert!(trees.len() >= 7, "{trees:?}");
    for golden in trees {
        let scratch = tempfile::tempdir().expect("tempdir");
        let tree = scratch.path();
        offline_copy(&golden, tree);
        let (code, text) = run_buf(tree, &["lint".as_ref()]);
        assert_eq!(code, 0, "buf lint {}:\n{text}", golden.display());
        let image = tree.join("image.binpb");
        let (code, text) = run_buf(tree, &["build".as_ref(), "-o".as_ref(), image.as_os_str()]);
        assert_eq!(code, 0, "buf build {}:\n{text}", golden.display());
    }
}

/// The two releases of a golden case: bundle files, or descriptor
/// directories that the export joins.
fn release_pair(case: &str) -> (PathBuf, PathBuf) {
    let dir = golden_dir().join(case);
    if dir.join("current.json").is_file() {
        (
            golden_dir().join("release-1.json"),
            dir.join("current.json"),
        )
    } else {
        (dir.join("deployed"), dir.join("current"))
    }
}

/// Run `autumn contracts check` for a case and return its exit code.
fn check_exit(deployed: &Path, current: &Path, scratch: &Path) -> i32 {
    let bundle = |from: &Path, name: &str| -> PathBuf {
        if from.is_file() {
            return from.to_path_buf();
        }
        let out = scratch.join(name);
        let (code, _, stderr) = autumn(&[
            "export".as_ref(),
            "--from".as_ref(),
            from.as_os_str(),
            "--out".as_ref(),
            out.as_os_str(),
        ]);
        assert_eq!(code, 0, "{stderr}");
        out
    };
    let deployed = bundle(deployed, "deployed.json");
    let current = bundle(current, "current.json");
    let (code, _, _) = autumn(&[
        "check".as_ref(),
        "--against".as_ref(),
        deployed.as_os_str(),
        "--current".as_ref(),
        current.as_os_str(),
    ]);
    code
}

/// Each two-release case: the exit code of `autumn contracts check`, whether
/// `buf breaking` (category `WIRE_JSON`) finds a break, and why the two
/// differ, when they do.
///
/// The check approves a case with exit 0: safe in any deploy order. buf must
/// find no break in a case the check approves, with no exception. buf can
/// approve a case that the check refuses (exit 1, or exit 3 for one deploy
/// order): proto3 has no required fields, and buf sees no routes and no
/// read-sets. Each such case names its reason.
const CASES: &[(&str, i32, bool, &str)] = &[
    ("unchanged", 0, false, ""),
    ("new_optional_request_field", 0, false, ""),
    ("removed_unread_response_field", 0, false, ""),
    (
        "new_required_request_field",
        3,
        false,
        "required fields: proto3 has no required, so a new required field is not a break",
    ),
    (
        "removed_read_response_field",
        3,
        false,
        "read-sets: the removed number is reserved, and buf cannot see that a caller reads it",
    ),
    (
        "new_response_field_read_by_new_caller",
        3,
        false,
        "read-sets: a new caller reads a field that the deployed callee does not send yet",
    ),
    (
        "path_changed",
        1,
        false,
        "routes: the path is in a comment, and buf does not compare comments",
    ),
    (
        "path_changed_old_route_kept",
        3,
        false,
        "routes: the path is in a comment, and buf does not compare comments",
    ),
    (
        "method_changed",
        1,
        false,
        "routes: the method is in a comment, and buf does not compare comments",
    ),
    (
        "endpoint_removed_from_router",
        1,
        false,
        "routes: WIRE_JSON does not check a removed rpc",
    ),
    (
        "nested_required_field_removed",
        3,
        false,
        "required fields: the removed number is reserved, and proto3 has no required",
    ),
    ("unit_to_empty_object", 1, true, ""),
    (
        "user_no_body_response",
        3,
        false,
        "read-sets: the removed number is reserved, and buf cannot see that a caller reads it",
    ),
    (
        "shared_type_identity",
        3,
        false,
        "read-sets: the removed number is reserved, and buf cannot see that a caller reads it",
    ),
    ("wire_type_representation_changed", 1, true, ""),
    ("response_field_type_changed", 1, true, ""),
];

#[test]
#[ignore = "requires buf"]
fn buf_breaking_finds_no_break_that_contracts_check_approved() {
    let mut listed: Vec<&str> = CASES.iter().map(|c| c.0).collect();
    listed.sort_unstable();
    let mut on_disk: Vec<String> = std::fs::read_dir(golden_dir())
        .expect("golden dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.join("current.json").is_file() || p.join("current").is_dir())
        .map(|p| p.file_name().expect("name").to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    assert_eq!(listed, on_disk, "each golden case needs a row in CASES");

    let mut wrong = Vec::new();
    for (case, want_exit, want_break, why) in CASES {
        let scratch = tempfile::tempdir().expect("tempdir");
        let (deployed, current) = release_pair(case);
        let lock = scratch.path().join("proto.lock");
        let before = scratch.path().join("before");
        let after = scratch.path().join("after");
        let (code, stderr) = export_proto(&deployed, &before, &lock, &[]);
        assert_eq!(code, 0, "{case}: {stderr}");
        let (code, stderr) = export_proto(&current, &after, &lock, &[]);
        assert_eq!(code, 0, "{case}: {stderr}");

        let exit = check_exit(&deployed, &current, scratch.path());
        let (code, text) = run_buf(
            &after,
            &[
                "breaking".as_ref(),
                "--against".as_ref(),
                before.as_os_str(),
            ],
        );
        // buf exits 100 for a break. Any other failure is not a verdict.
        assert!(
            code == 0 || code == 100,
            "{case}: buf breaking failed:\n{text}"
        );
        let breaks = code == 100;
        let approved = exit == 0;
        if approved && breaks {
            wrong.push(format!(
                "{case}: the check approved it (exit 0), but buf finds a break:\n{text}"
            ));
        }
        if (exit, breaks) != (*want_exit, *want_break) {
            wrong.push(format!(
                "{case}: want check exit {want_exit} and buf break {want_break}, got exit \
                 {exit} and buf break {breaks}:\n{text}"
            ));
        }
        let agree = approved != breaks;
        if agree != why.is_empty() {
            wrong.push(format!(
                "{case}: the check and buf disagree, so name the reason, or agree, so name \
                 none (reason: `{why}`)"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
}

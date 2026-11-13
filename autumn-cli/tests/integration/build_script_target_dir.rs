use std::fs;

/// Compile `candidate_target_dirs` from `src` and run it with the given
/// env. Return the printed candidates.
fn run_candidate_target_dirs(
    src: &str,
    dir: &std::path::Path,
    cargo_target_dir: Option<&str>,
    out_dir: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    // Windows checkouts may use CRLF.
    let src = src.replace("\r\n", "\n");
    let start = src
        .find("fn candidate_target_dirs()")
        .expect("candidate_target_dirs present");
    let body = &src[start..];
    let end = body.find("\n}\n").expect("function end") + 3;
    let program = format!(
        "{}\nfn main() {{ for c in candidate_target_dirs() {{ println!(\"{{}}\", c.display()); }} }}\n",
        &body[..end]
    );
    let main_rs = dir.join("main.rs");
    fs::write(&main_rs, program).unwrap();
    let exe = dir.join(if cfg!(windows) { "probe.exe" } else { "probe" });
    let status =
        std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition", "2021", "-o"])
            .arg(&exe)
            .arg(&main_rs)
            .status()
            .expect("rustc runs");
    assert!(status.success(), "probe failed to compile");
    let mut cmd = std::process::Command::new(&exe);
    cmd.env_remove("CARGO_TARGET_DIR").env("OUT_DIR", out_dir);
    if let Some(v) = cargo_target_dir {
        cmd.env("CARGO_TARGET_DIR", v);
    }
    let out = cmd.output().expect("probe runs");
    assert!(
        out.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(std::path::PathBuf::from)
        .collect()
}

/// A relative `CARGO_TARGET_DIR` must not be a candidate. The build
/// script runs in the member directory. A relative path could match a
/// stale member-local binary (issue #3108). An absolute value stays first.
#[test]
fn build_scripts_skip_relative_cargo_target_dir() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("src/templates/build.rs.tmpl")];
    for name in [
        "blog",
        "bookmarks",
        "bookmarks-distributed",
        "cms",
        "reddit-clone",
        "saas",
        "teams",
        "todo-app",
        "wiki",
    ] {
        files.push(root.join("../examples").join(name).join("build.rs"));
    }
    files.push(root.join("src/starters/cms/build.rs"));
    files.push(root.join("src/starters/saas/build.rs"));

    let tmp = tempfile::tempdir().unwrap();
    // Absolute on every OS: build all paths from the temp dir.
    let ws = tmp.path().join("ws");
    let out_dir = ws.join("target/debug/build/pkg-1/out");
    let abs_target = tmp.path().join("abs").join("target");
    let expected = [ws.join("target"), ws.clone()];
    let mut seen = std::collections::HashSet::new();
    for file in files {
        let src =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        // Identical function text gives an identical probe; run it once.
        let src = src.replace("\r\n", "\n");
        let start = src.find("fn candidate_target_dirs()").unwrap();
        if !seen.insert(src[start..].split("\n}\n").next().unwrap().to_owned()) {
            continue;
        }
        let name = file.display();
        let rel = run_candidate_target_dirs(&src, tmp.path(), Some("target"), &out_dir);
        assert_eq!(rel, expected, "{name}: relative value kept");
        let none = run_candidate_target_dirs(&src, tmp.path(), None, &out_dir);
        assert_eq!(none, expected, "{name}: unset differs");
        let abs = run_candidate_target_dirs(&src, tmp.path(), abs_target.to_str(), &out_dir);
        assert_eq!(abs[0], abs_target, "{name}: absolute lost");
        assert_eq!(abs[1..], expected, "{name}: OUT_DIR fallback lost");
    }
}

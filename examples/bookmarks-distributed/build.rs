mod build_support;

fn main() {
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=static/css/input.css");
    println!("cargo:rerun-if-changed=tailwind.config.js");
    println!("cargo:rerun-if-changed=static/css/autumn.css");
    println!("cargo:rerun-if-env-changed=AUTUMN_REQUIRE_TAILWIND");
    println!("cargo:rerun-if-env-changed=PATH");
    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-env-changed=PATHEXT");

    let tailwind = find_tailwind_cli();
    // Watch the path `find_tailwind_cli` actually resolved (wherever
    // `CARGO_TARGET_DIR`/workspace nesting really put it), not a
    // package-relative guess -- `cargo:rerun-if-changed` on a path that can
    // never resolve to where the binary really lives makes Cargo treat the
    // build script as permanently dirty, so it (and the Tailwind rebuild)
    // reruns on every single `cargo build` even with nothing changed. When
    // nothing was found yet, fall back to the primary candidate's expected
    // path, so the first `autumn setup` afterwards is still picked up
    // without an unrelated source edit. Mirrors
    // `autumn-cli/src/templates/build.rs.tmpl` (issue #2694, Codex review on
    // #3107 rounds 2-3).
    match &tailwind {
        Some(path) => println!("cargo:rerun-if-changed={}", path.display()),
        None => {
            if let Some(expected) = expected_tailwind_path() {
                println!("cargo:rerun-if-changed={}", expected.display());
            }
        }
    }

    let Some(tailwind) = tailwind else {
        handle_tailwind_unavailable("Tailwind CSS CLI not found");
        return;
    };

    let output = std::process::Command::new(&tailwind)
        .args([
            "-i",
            "static/css/input.css",
            "-o",
            "static/css/autumn.css",
            "--content",
            "src/**/*.rs",
            "--minify",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            handle_tailwind_unavailable(&format!(
                "Tailwind CSS CLI exited with status {}: {stderr}",
                output.status
            ));
        }
        Err(error) => {
            handle_tailwind_unavailable(&format!("failed to run Tailwind CSS CLI: {error}"));
        }
    }
}

fn handle_tailwind_unavailable(reason: &str) {
    let require_tailwind = build_support::require_tailwind_from_env();
    match build_support::tailwind_failure_action(require_tailwind) {
        build_support::TailwindFailureAction::FailBuild => {
            panic!("{reason}; AUTUMN_REQUIRE_TAILWIND is set");
        }
        build_support::TailwindFailureAction::SkipRegeneration => {
            println!("cargo:warning={reason}; skipping static/css/autumn.css regeneration");
        }
    }
}

fn find_tailwind_cli() -> Option<std::path::PathBuf> {
    // 1. Check the workspace target directory (from `autumn setup`), trying
    //    each candidate and picking the platform's binary name.
    for target_dir in candidate_target_dirs() {
        let local = target_dir.join("autumn").join(tailwind_bin_name());
        if local.exists() {
            return Some(local);
        }
    }

    // 2. Check PATH
    which("tailwindcss")
}

/// Where `autumn setup` is expected to install the CLI, going by the FIRST
/// (most likely correct) candidate target directory -- used only to still
/// pick up a first `autumn setup` run when nothing was found yet; see the
/// call site in `main`.
fn expected_tailwind_path() -> Option<std::path::PathBuf> {
    Some(
        candidate_target_dirs()
            .into_iter()
            .next()?
            .join("autumn")
            .join(tailwind_bin_name()),
    )
}

fn tailwind_bin_name() -> &'static str {
    if cfg!(windows) {
        "tailwindcss.exe"
    } else {
        "tailwindcss"
    }
}

/// Candidate Cargo target directories to look for `autumn setup`'s download
/// in, most-likely first. Mirrors `candidate_target_dirs` in
/// `autumn-cli/src/templates/build.rs.tmpl`: `CARGO_TARGET_DIR` (when the
/// user set it as an env var) first, then `OUT_DIR`'s ancestors at both
/// offset 4 and 5 (`cargo build` vs `cargo build --target <triple>`, which
/// inserts an extra path segment).
fn candidate_target_dirs() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        candidates.push(std::path::PathBuf::from(dir));
    }

    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        let out_dir = std::path::PathBuf::from(out_dir);
        candidates.extend(
            [out_dir.ancestors().nth(4), out_dir.ancestors().nth(5)]
                .into_iter()
                .flatten()
                .map(std::path::Path::to_path_buf),
        );
    }

    candidates
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.exists() {
            return Some(candidate);
        }
        #[cfg(target_os = "windows")]
        {
            let candidate_exe = dir.join(format!("{binary}.exe"));
            if candidate_exe.exists() {
                return Some(candidate_exe);
            }
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=static/css/input.css");
    println!("cargo:rerun-if-changed=tailwind.config.js");
    // The CLI's own install path, so the first build after `autumn setup`
    // actually reruns. Without these the script can be skipped indefinitely
    // after a first build that found no Tailwind — it produced no CSS
    // output, nothing it tracks has changed since, and the site stays
    // unstyled until an unrelated source edit happens to trigger it. See
    // issue #2694.
    // Resolved via the same OUT_DIR-ancestors logic find_tailwind_cli uses
    // below -- a bare relative "target/autumn/tailwindcss" literal is wrong
    // for a workspace member (Cargo runs the build script with the PACKAGE
    // dir as CWD, not the workspace root `autumn setup` installs into), so
    // it would never exist and leave the build dirty forever regardless of
    // platform (Codex review on #3107, round 2).
    if let Some(path) = expected_tailwind_install_path() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=PATH");
    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-env-changed=PATHEXT");

    let Some(tailwind) = find_tailwind_cli() else {
        println!(
            "cargo:warning=Tailwind CSS CLI not found — CSS will not be compiled. \
             Run `autumn setup` or install tailwindcss manually."
        );
        return;
    };

    let status = std::process::Command::new(&tailwind)
        .args([
            "-i",
            "static/css/input.css",
            "-o",
            "static/css/autumn.css",
            "--content",
            "src/**/*.rs",
            "--minify",
        ])
        .status()
        .expect("Failed to run Tailwind CLI");

    assert!(status.success(), "Tailwind CSS compilation failed");
}

fn find_tailwind_cli() -> Option<std::path::PathBuf> {
    if let Some(local) = expected_tailwind_install_path()
        && local.exists()
    {
        return Some(local);
    }

    if let Some(path) = which("tailwindcss") {
        return Some(path);
    }

    None
}

/// Where `autumn setup` installs the Tailwind CLI: the first ancestor of
/// `OUT_DIR` literally named `target`, plus `autumn/<bin name>`. Walking by
/// name (rather than a fixed `ancestors().nth(4)` offset) still finds the
/// real workspace target dir regardless of how deep `OUT_DIR` nests (e.g. an
/// extra `<target-triple>` segment from an explicit `--target`).
fn expected_tailwind_install_path() -> Option<std::path::PathBuf> {
    let out_dir = std::env::var("OUT_DIR").ok()?;
    let out_path = std::path::PathBuf::from(out_dir);
    let bin_name = if cfg!(windows) {
        "tailwindcss.exe"
    } else {
        "tailwindcss"
    };
    out_path
        .ancestors()
        .find(|a| a.file_name().and_then(|n| n.to_str()) == Some("target"))
        .map(|target_dir| target_dir.join("autumn").join(bin_name))
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

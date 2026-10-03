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
    // Only the platform-correct path: watching both unconditionally
    // means the OTHER platform's file is permanently absent, which
    // Cargo always treats as changed -- making every build dirty
    // forever, on every platform (Codex review on #3107).
    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-changed=target/autumn/tailwindcss.exe");
    #[cfg(not(target_os = "windows"))]
    println!("cargo:rerun-if-changed=target/autumn/tailwindcss");
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
    // 1. Check workspace target directory (from `autumn setup`).
    //    OUT_DIR is <workspace>/target/<profile>/build/<pkg>/out —
    //    walk up to the target dir and look for autumn/tailwindcss.
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        let out_path = std::path::PathBuf::from(out_dir);
        // ancestors: out → <pkg> → build → <profile> → target
        if let Some(target_dir) = out_path.ancestors().nth(4) {
            let bin_name = if cfg!(windows) {
                "tailwindcss.exe"
            } else {
                "tailwindcss"
            };
            let local = target_dir.join("autumn").join(bin_name);
            if local.exists() {
                return Some(local);
            }
        }
    }

    // 2. Check PATH
    if let Some(path) = which("tailwindcss") {
        return Some(path);
    }

    None
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

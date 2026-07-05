fn main() {
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=static/css/input.css");
    println!("cargo:rerun-if-changed=tailwind.config.js");
    println!("cargo:rerun-if-changed=target/autumn/tailwindcss");
    println!("cargo:rerun-if-env-changed=PATH");
    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-env-changed=PATHEXT");

    if let Some(tailwind) = find_tailwind_cli() {
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

        if !status.success() {
            panic!("Tailwind CSS compilation failed");
        }
    } else {
        // Output a warning ONLY if this is not part of a documentation build
        // or a test where we don't care about CSS.
        // Usually, `autumn dev` will complain if it is actually required.
        // As per DX audit, removing the warning or making it quieter is better,
        // we'll just not emit a warning for missing tailwind when it's optional.
    }
}

fn find_tailwind_cli() -> Option<std::path::PathBuf> {
    // 1. Check local download (from `autumn setup`)
    let local = std::path::PathBuf::from("target/autumn/tailwindcss");
    if local.exists() {
        return Some(local);
    }

    // 2. Check PATH
    if let Some(path) = which("tailwindcss") {
        return Some(path);
    }

    // 3. Return None to treat this as optional.
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

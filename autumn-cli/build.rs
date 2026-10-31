fn main() {
    // `autumn a11y verify` reads `[target.'…']` keys for the host.
    for (var, name) in [
        ("TARGET", "AUTUMN_HOST_TARGET"),
        ("CARGO_CFG_TARGET_ENV", "AUTUMN_TARGET_ENV"),
        ("CARGO_CFG_TARGET_VENDOR", "AUTUMN_TARGET_VENDOR"),
    ] {
        let value = std::env::var(var).unwrap_or_default();
        println!("cargo:rustc-env={name}={value}");
    }
    // Windows default main-thread stack is 1 MB. The Clap-derived parser for
    // this CLI now exceeds that limit with 25+ subcommands. Request 8 MB to
    // match the Linux/macOS default and prevent stack overflow at startup.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "windows" {
        let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if env == "msvc" {
            println!("cargo:rustc-link-arg=/STACK:8388608");
        } else {
            // MinGW / GNU ld
            println!("cargo:rustc-link-arg=-Wl,--stack,8388608");
        }
    }
}

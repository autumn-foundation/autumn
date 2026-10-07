### Fixed

- **docs:** the README quickstart now installs with
  `cargo install autumn-cli --version 0.8.0 --locked`. Without `--locked`,
  cargo re-resolves dependencies and picked `uuid 1.27.0` (rustc 1.89+), so the
  first step failed on the advertised Rust 1.88.0 MSRV. The scaffolded project
  README carries the same command, and `scripts/clean-room-msrv.sh` reruns the
  journey on the MSRV toolchain with a pristine `CARGO_HOME`.

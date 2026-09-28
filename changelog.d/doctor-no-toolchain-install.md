### Fixed

- **cli:** `autumn doctor` no longer makes rustup install the project's pinned
  toolchain. Doctor runs its checks at the same time, and two of them started
  `rustc` and `cargo` through rustup's shim. On a machine without the pinned
  toolchain, the concurrent installs could leave it half-installed: the next
  `cargo` command failed with "the 'cargo.exe' binary ... is not applicable to
  the toolchain", or doctor hung. Doctor now reports the missing toolchain in
  the `rust_toolchain` check and suggests `rustup toolchain install`. When
  `cargo metadata` cannot name the target directory, the Tailwind check looks
  under `CARGO_TARGET_DIR` or `./target`. If the binary is not there, the check
  is not evaluated, so doctor does not report it missing from a guess.

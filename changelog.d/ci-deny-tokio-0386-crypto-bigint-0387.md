### Security

- Bumped `tokio` to 1.54.1 (RUSTSEC-2026-0386, `AsyncFd` I/O-safety unsoundness) and recorded a dated, reasoned `cargo-deny` waiver for RUSTSEC-2026-0387 (`crypto-bigint` 0.4.9 via `aws-sigv4`'s unused SigV4a path; no 0.4.x fix exists).

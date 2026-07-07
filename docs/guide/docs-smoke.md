# Docs Smoke Procedure

This docs-smoke procedure certifies the public first-run path for the published
Autumn 0.5.x line. It must use Rust 1.88.0+ and the published `autumn-cli` and
`autumn-web` crates unless the release is explicitly in a pre-publish rehearsal.

Run it in a clean temporary directory, not inside the Autumn checkout:

```bash
rustc --version
cargo install autumn-cli --version 0.6.0

mkdir autumn-docs-smoke
cd autumn-docs-smoke
autumn new smoke-app
cd smoke-app
autumn setup
cargo run
```

In a second terminal, verify the first route and framework endpoints:

```bash
curl -fsS http://127.0.0.1:3000/
curl -fsS http://127.0.0.1:3000/hello/world
curl -fsS http://127.0.0.1:3000/health
```

Passing output:

- `/` returns `Welcome to smoke-app!`.
- `/hello/world` returns `Hello, world!`.
- `/health` returns:

  ```json
  { "status": "ok", "version": "0.6.0" }
  ```

Do not add `[patch.crates-io]`, path dependencies, or `cargo install --path`
when certifying the published-user path. Those are contributor-mode shortcuts,
not first-run documentation proof.

## Pre-Publish Rehearsal

Before the `autumn-cli` or `autumn-web` release has reached crates.io, the same
commands may be rehearsed against the workspace only if the release notes record
why the smoke is temporary:

```text
docs-smoke: workspace-prepublish rehearsal because autumn-cli/autumn-web 0.5.0
were not yet available on crates.io.
```

That rehearsal does not certify the release. The published docs-smoke must be
rerun with `cargo install autumn-cli --version 0.6.0` and no workspace patches
before announcing the release.

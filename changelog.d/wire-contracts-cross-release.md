### Added

- **wire contracts:** `autumn contracts export` writes a sorted, byte-stable
  bundle of every wire descriptor in the build, from a dedicated pass that a
  build cache cannot skip. `autumn contracts check --against
  <bundle|url|git-ref>` compares the current build with a deployed release in
  both rolling-deploy windows. It fails on a removed response field that a
  deployed caller reads, a new required request field that a deployed caller
  does not send, and a changed path or method. When a change is safe in one
  deploy order only, it names that order and exits 2. `#[contract_checked]`
  and `wire_client!` now also write descriptors, so the bundle holds each
  caller's read-set. See `docs/guide/wire-contracts.md`, "Across releases".

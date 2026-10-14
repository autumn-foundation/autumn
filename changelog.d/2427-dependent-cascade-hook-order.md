### Breaking Changes

- **Breaking:** **repository:** `delete_many` now fires each parent's
  `before_delete` before any child hook, as `delete_by_id` does (issue #2427,
  [migration guide](docs/migrations/next.md)). `DependentAction` gains a hidden
  `Probe` variant, and `RuntimeDependentSpec` gains a `probe` field.

### Changed

- **repository:** a dependent cascade now probes every `restrict` before any
  hook fires (issue #2427). The probe reaches grandchildren and deeper, across a
  parent's sibling associations and across all parents of a `delete_many` batch.
  A blocked delete fires no hook. The probe reads each `destroy` child set once
  more. A `restrict` that an earlier sibling leg would have removed now blocks.
- **repository:** a batch root that is also another root's descendant still
  fires its hooks once.

### Added

- **repository:** `position(...)` now works with `dependent(...)`, in the
  repository attribute and on the model. The old compile error is gone.

### Testing

- **repository:** integration tests pin the cascade hook order and prove a
  cascaded child's `after_delete_commit` runs once, after commit (issue #2427).

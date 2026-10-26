### Fixed

- **`#[model]`:** every generated `NewX` implements `Normalize` again (issue
  #2692). 0.8.0 removed the impl when the model has no `#[normalize]` columns,
  so `NewX::normalize()` and a `T: Normalize` bound did not compile. The impl
  does nothing for such a model. The repository insert path still does not
  clone the input for it (#2634).

### Breaking Changes

- **Breaking:** **`#[model]`:** if you wrote your own `impl Normalize for NewX`
  for a model with no `#[normalize]` columns, remove it (issue #2692). It now
  conflicts with the generated impl (`E0119`). If it changed values, move the
  logic to `#[normalize(...)]` on the field or to a `before_create` hook
  ([migration guide](docs/migrations/next.md)).

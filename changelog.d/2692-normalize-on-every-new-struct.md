### Fixed

- **`#[model]`:** every generated `NewX` implements `Normalize` again (issue
  #2692). 0.8.0 removed the impl when the model has no `#[normalize]` columns,
  so `NewX::normalize()` and a `T: Normalize` bound stopped compiling. The impl
  does nothing for such a model. The repository insert path still does not
  clone the input for it (#2634). If you added your own `impl Normalize for
  NewX` to work around this, remove it: it now conflicts with the generated
  impl.

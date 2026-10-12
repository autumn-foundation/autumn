### Fixed

- **cache:** an unrelated call such as `Config::load()` in a `#[cached]`
  function no longer makes the read `derived` (issue #2362). `load`, `count`,
  `list`, `page`, `all` and `first` no longer name a dependency. A capitalised
  call with one of them marks the read `undetermined`, so `autumn cache audit
  --strict` reports it. `find_all`, `find_by_id` and `exists_by_id` still name
  the model.

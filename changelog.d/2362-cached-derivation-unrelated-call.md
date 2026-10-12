### Fixed

- **cache:** an unrelated call such as `Config::load()` in a `#[cached]`
  function no longer makes the read `derived` (issue #2362). `load`, `count`,
  `list`, `page`, `all` and `first` no longer name a dependency. A read with
  nothing else recovered is `undetermined`, so `autumn cache audit --strict`
  reports it. `find_all`, `find_by_id` and `exists_by_id` still name the model.
  `Post::count(db)` no longer derives `Post`: declare `reads(Post)`.

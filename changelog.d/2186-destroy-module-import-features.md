### Fixed

- **cli:** `autumn destroy scaffold` keeps the `storage` and `markdown`
  features when your code imports the feature module
  (`use autumn_web::storage;` or `use autumn_web::markdown as md;`).
  Before this fix, `destroy` removed the feature and the build failed
  (issue #2186).

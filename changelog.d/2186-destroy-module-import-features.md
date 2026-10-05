### Fixed

- **cli:** `autumn destroy scaffold` keeps the `storage` and `markdown`
  features when your code imports the module (`use autumn_web::storage;`,
  `use autumn_web::markdown as md;`). Before, it removed the feature and the
  build failed (issue #2186).

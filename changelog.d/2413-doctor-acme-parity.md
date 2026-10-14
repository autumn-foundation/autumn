### Fixed

- **doctor:** `autumn doctor --strict` now fails a non-string `cache_dir` or
  `contact_email`, a non-table `[server.tls] acme`, and a non-list `domains`
  (issue #2413). Each message names the bad value. Before, doctor passed
  configs that the server refuses to start on.

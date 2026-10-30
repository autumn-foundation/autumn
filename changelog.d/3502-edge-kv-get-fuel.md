### Fixed

- **edge:** a `kv_get` answer now costs fuel by value size, and unread host
  answers are capped at 64 MiB (issue #3502). A guest that loops on a large
  value now falls through to the origin.

### Fixed

- **autumn-edge:** the reference host now charges fuel for each `kv_get`
  answer, by value size (issue #3502). It also caps unread host answers at
  64 MiB. A capsule that loops on a large value now falls through to the
  origin.

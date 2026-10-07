### Fixed

- **etag:** `If-None-Match` parsing now keeps a comma inside a quoted tag
  (issue #3080). Before, a tag such as `"a,b"` never got a `304`, and a
  different tag `"a"` got a false `304`. A malformed list member now never
  matches. `EtagLayer` now parses each `If-None-Match` field alone.

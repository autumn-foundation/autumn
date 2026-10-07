### Fixed

- **etag:** The `If-None-Match` parser no longer splits a quoted tag at a
  comma (issue #3080). A response with the `ETag` `"a,b"` now gets a `304`
  when the request sends `"a,b"`. A response with the `ETag` `"a"` no longer
  gets a false `304` for that request. A malformed member, such as `"abc`,
  does not match. `*` matches only when no `If-None-Match` field has a tag.

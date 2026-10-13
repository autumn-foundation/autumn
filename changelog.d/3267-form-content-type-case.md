### Fixed

- **forms:** `ChangesetForm`, `NestedChangesetForm` and the `_method` override
  now accept a mixed-case `Content-Type` such as
  `Application/X-WWW-Form-Urlencoded` (issue #3267). They returned `415`, or
  skipped the override, while the CSRF layer accepted the same request.

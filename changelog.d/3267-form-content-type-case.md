### Fixed

- **forms:** Form gates now match `Content-Type` without regard to case
  (issue #3267). `ChangesetForm` and `NestedChangesetForm` returned `415` for
  `Application/X-WWW-Form-Urlencoded`. The `_method` override did nothing for
  it. The CSRF layer accepted the same request. The captcha and inbound-mail
  gates now also accept leading whitespace.

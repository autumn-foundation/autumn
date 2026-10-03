### Changed

- **billing:** the webhook endpoint declared in `autumn.toml` must now allow
  request bodies of at least 4 MiB (`max_body_bytes = 4194304`); boot fails
  with the line to add when it allows less. The webhook default (1 MiB) rejects
  a provider event that carries many invoice lines with a 400, which the
  provider retries unchanged, so the event was never applied. The billing
  guide, the crate README and rustdoc, and the `autumn plugin add
  autumn-billing` output now include the setting. A larger limit is allowed.

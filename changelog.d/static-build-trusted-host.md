### Fixed

- **static_gen:** a release `autumn build` (the `prod` profile) renders its
  static pages and agent documents again. Its in-process renders carry no
  `Host`, and the trusted-host check refused them with `400`; a build render
  now passes, while a live request without a `Host` is still refused.

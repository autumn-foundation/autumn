### Added

- **slug:** [no-plugin] `slugify_with_suffix(input, entropy, len)` builds a slug
  with a short random tail (`hello-world-k3x9q`), so posts with the same title
  get distinct URLs without probing the database. The tail is drawn from the
  injected `Entropy`, so a seeded simulation mints the same slugs on every run.

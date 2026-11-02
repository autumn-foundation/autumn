### Changed

- **metrics:** [no-plugin] a held `Counter`, `Gauge` or `Histogram` handle now resolves its
  series once and reuses it, so recording through a handle no longer hashes its
  label set on every call. Behaviour is unchanged, including when
  `max_labels_per_series` is lowered after the handle was built.

### Changed

- **web:** `Query<T>` decoding stores a single-valued key's value inline instead of in a one-element `Vec`, cutting allocations per decode by ~14% (no behavior change).

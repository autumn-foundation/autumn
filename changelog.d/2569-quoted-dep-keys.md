### Fixed

- **sqlite-unification gate recognizes quoted dependency keys (#2569):**
  `scripts/check-sqlite-unification.sh` anchored every dependency-edge rule on
  an unquoted TOML key, so a quoted key — `"autumn-web" = { …,
  features = ["sqlite"] }`, `[dependencies."autumn-web"]`, or
  `"autumn-web".features = [ … ]`, all spellings cargo accepts — sailed
  through the gate and could flip the `sqlite` backend for the whole graph
  undetected. Key-quoting is now stripped (key portion only; value quotes
  stay significant) before the rules run, in `feed()`'s output for entries
  and on section headers. Pinned by three new self-test cases (32/32).

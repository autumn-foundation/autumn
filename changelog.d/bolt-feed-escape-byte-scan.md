### Changed

- **feed:** `Feed::render` XML-escapes text with a byte-table scan instead of decoding every `char`; output is identical. Callgrind on the 30-entry `feed_render` bench: 338,241 → 255,656 instructions per render (-24.4%).

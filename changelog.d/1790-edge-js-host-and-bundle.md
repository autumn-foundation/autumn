### Added

- **edge:** a web-standard JavaScript edge host, `autumn-edge/js/autumn-edge-host.mjs`
  ([ADR-0024](docs/adr/0024-web-standard-js-edge-host.md)). It runs a capsule
  on Cloudflare Workers, Deno, Bun or Node with the reference host's WASI
  surface, determinism and response checks. It forwards every decline to the
  origin unchanged. It reads an async KV store (a Workers KV binding) through
  replay-on-miss, so a capsule needs no preloaded snapshot. The edge-greeting
  conformance suite (Tier J) requires the same outcome as the reference host
  over 10,000 generated requests.
- **cli:** `autumn edge bundle --target cloudflare --origin <URL>` writes a
  Worker that `wrangler deploy` deploys: the capsule, the host, a generated
  `worker.mjs`, and a `wrangler.toml` that later bundles keep. It refuses a
  capsule that imports something the host cannot link. CI runs the bundle in
  workerd against the real origin.

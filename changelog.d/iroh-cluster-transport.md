### Added

- **iroh:** one iroh endpoint per process, dialed by public key (issue
  #3818). Build with the `iroh` feature and set `[iroh] enabled = true`. The
  secret key file is created `0600`. Relay and address lookup are off by
  default; `relay = "n0"` or `"custom"` and `address_lookup = "n0-dns"` turn
  them on. `allowed_peers` limits who may connect. `autumn doctor` has a new
  `iroh` check. See `docs/guide/iroh.md`.
- **cluster:** `[cluster] transport = "iroh"` runs the cluster on the
  `[iroh]` endpoint. Seeds are `<endpoint-id>` or `<endpoint-id>@<ip:port>`.
  One connection carries frames both ways, so a node behind NAT gets every
  push back. A frame whose sender names another endpoint id is dropped, and
  counted under the new `reason="sender"` label of
  `autumn_cluster_frames_rejected_total`.

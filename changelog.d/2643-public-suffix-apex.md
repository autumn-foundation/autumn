### Fixed

- **custom domains:** apex detection uses the Public Suffix List (issue
  #2643, [ADR-0026](docs/adr/0026-public-suffix-apex-detection.md)).
  `clientco.co.uk`, `customer.com.au` and `tenant.co.jp` now get A/AAAA
  instructions, not a CNAME that a DNS provider refuses. If no ingress
  addresses are set, they get the "set `ingress_ipv4` / `ingress_ipv6`" error.
  `autumn doctor` warns when a registered domain gets no DNS instructions.

### Fixed

- **dns-01:** cleanup is best-effort again — every zone's record removal is
  attempted even after one fails, and the failures are reported together with
  the failing zones named, instead of abandoning the remaining records after
  the first error.
- **dns-01 (Route 53):** zone lookup now follows the `ListHostedZonesByName`
  pagination cursor while the names it points at can still match the
  candidate, so accounts with more hosted zones than fit one `maxitems=10`
  page no longer fail with "no Route 53 hosted zone found". It stops once the
  cursor moves past the candidate name, and the "no zone found" error still
  names the `hosted_zone_id` escape hatch.
- **`autumn doctor`:** `--online` reachability probes (port 80/443 + DNS
  points-here) now check a representative covered hostname
  (`doctor-check.<domain>`) for wildcard entries instead of the bare apex, so
  wildcard-only deployments (wildcard `A`/`AAAA`, no apex record) stop
  reporting permanent false warnings. The `_acme-challenge` delegation probe
  still checks the base domain, where the wildcard's challenge record lives.

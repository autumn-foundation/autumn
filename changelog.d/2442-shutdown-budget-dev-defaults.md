### Fixed

- **Shutdown budget (issue #2442):** `autumn serve` / `autumn dev` resolved
  the graceful-stop budget from a hard-coded `(prestop_grace_secs,
  shutdown_timeout_secs) = (5, 30)` seed, so an unconfigured `dev`-profile
  project was credited a 35-second drain budget the app never uses — the
  runtime's dev smart defaults are `0` and `1`. The resolver now seeds from
  the selected profile's own smart defaults (read from the same source the
  runtime's config loader applies, so the two cannot drift again): `dev`
  seeds `(0, 1)`, `prod` and custom profiles keep `(5, 30)`. The TOML and
  environment layers are unchanged and still override the seed in order.

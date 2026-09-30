### Fixed

- **Capacity contract (issue #2445):** the scheduled false-positive probe ran
  3 no-op rebuilds, not the required 20. The `noop_rebuilds`
  `workflow_dispatch` input declares a default of 20, but `inputs` is empty on
  the `schedule` trigger, so `${{ inputs.noop_rebuilds || '3' }}` silently
  fell through to the literal `3` every Monday — the only automated run of
  the probe, and a sample size the workflow's own comment says cannot
  distinguish a 15%-flake gate from a 0% one. The fallback is now `'20'`, and
  a `repo_hygiene` pin asserts every `inputs.<name> || '<fallback>'` fallback
  in `capacity-contract.yml` equals the input's declared default, so the
  scheduled behavior can never drift from the manual one again.

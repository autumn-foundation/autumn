### Fixed

- **`deploy maintenance on` against a never-deployed host (#2280):** the shared
  flag's parent (`{app_dir}/shared`) only comes into existence during
  `prepare-dirs` on a deploy, and the scp-backed `WriteFile` op does not create
  destination parents — so the first `maintenance on` failed on a fresh host and
  the `AppliedSharedOnly` success path (keyed on exactly that host shape, "no
  promoted release") was unreachable. The fan-out now runs a
  `maintenance-prepare-shared-flag-dir` `mkdir -p` ahead of the shared write,
  mirroring the existing live-flag dir preparation, while keeping the
  amendment-A2 ordering (the shared flag is still written first). No behavior
  change for the `off` path: `rm -f` never needed the dir.

### Fixed

- **sqlite target coverage guard only credits `run:` commands (#2574):**
  `sqlite_test_targets_are_ci_named` (in
  `autumn-cli/tests/integration/repo_hygiene.rs`) built its "commands" from
  every non-comment YAML line, so step *metadata* — e.g. a step literally
  named `cargo test -p autumn-web --features sqlite --test <target>` —
  satisfied the guard while no job ran the target. Command collection now
  tracks `run:`-block membership by indentation (inline `run:`, block
  `run: |`/`>`, bare `run:`, and `- run:` forms), and only lines inside a
  `run:` scalar become commands; `\`-continuation joining is unchanged. Two
  new unit tests pin the behavior (`name:`/`env:` decoys credit nothing;
  commented-out invocations stay ignored), and the guard still passes on the
  unmodified workflow set.

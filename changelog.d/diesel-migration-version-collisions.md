### Added

- **Prevent Diesel migration version collisions before they happen:** Diesel
  records applied migrations *by version* (the leading `YYYYMMDDHHMMSS`
  directory prefix). If two differently-named migrations ever share one
  version, the framework already resolves it safely at startup by
  auto-substituting a version for the losing one, so both still apply — but
  a generated substitute is still a worse outcome than never colliding in
  the first place. `autumn migrate new <name>` creates
  `migrations/<version>_<name>/{up,down}.sql` with a version guaranteed free
  across the working tree, every local and remote-tracking git branch this
  checkout has fetched, and the framework's own compiled-in migrations —
  use it (or `autumn generate migration`) instead of hand-creating a
  migration directory. `autumn migrate check-collisions` is the CI-time
  backstop for a collision `migrate new` could not see (a branch pushed
  after, a teammate's concurrent PR): it fails when a version this checkout
  introduces is already claimed by a different directory on the default
  branch, another pushed branch, or the framework's own migrations, and is
  wired into this repo's own CI (`migration-collisions` job) as the
  reference implementation downstream apps can copy.

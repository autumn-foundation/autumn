### Fixed

- **graph:** close the served-surface gaps in the architecture graph
  (issue #2520). The census now lists OpenAPI routes, framework asset
  routes, and a worker's probe and actuator routes. `autumn graph` uses
  the app's config loader and role.
- **graph:** `--check` names a changed secondary mount and a changed
  declared plugin mount. Declared plugin mounts keep their auth in
  `completeness.declared_mounts`.
- **graph:** SQL scan ignores quoted text, reads each statement of a
  batch, and reads `COPY` and `TABLE`. A cron job records its timezone.
- **graph:** repository edges match the model by name and table. Git
  dependency paths no longer leak the build user's home directory.
- **graph:** a job's access is set for each edge. A job that reads one
  table and writes another no longer shows both as writes. A call such as
  `repo.update(..)` on a repository parameter counts as a write.

### Changed

- **graph:** run `autumn graph show` once and commit the result. The
  `limits` text, the census and the job access changed, so an older
  committed graph fails `--check` one time. The descriptor types in
  `autumn::graph` are for the macros. Do not build them by hand.

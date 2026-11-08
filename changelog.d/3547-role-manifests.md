### Added

- **release:** `autumn release init --target kubernetes` writes one workload
  per role (issue #3547). The Helm chart loops over a `roles:` map in
  `values.yaml`: each role gets a Deployment with `AUTUMN_ROLE`, a PDB, and an
  optional HPA and `AUTUMN_JOBS__PIN`. Only a role that serves HTTP gets a
  Service, an Ingress, an Argo Rollout or a Flagger Canary. The Kustomize base
  lists one component per role. `--target fly` writes one `[processes]` entry
  per role. The role set comes from `[roles.*]`, `--split-workers` or the new
  `--roles`. For a split app, `autumn.production.toml.example` (the image
  config) gets the `[roles.*]` presets and a shared `[jobs] backend`. See [Kubernetes](docs/guide/kubernetes.md#one-workload-per-role).
- **deploy:** `[[deploy.groups]]` puts a role on a group of fleet hosts. Each
  release dir gets a `role.env` with `AUTUMN_ROLE`, so a rollback runs the old
  release with its own role. Preflight fails an undeclared role, and a split
  role on the `local` jobs backend. See
  [Fleet deploys](docs/guide/fleet-deploys.md#per-group-roles).
- **doctor:** the `manifest_roles` row warns when a deploy manifest names a
  role that `autumn.toml` does not declare.

### Changed

- **release:** a regenerated Kubernetes output differs from earlier output
  ([migration guide](docs/migrations/next.md)). A chart from an earlier
  release keeps working.
  - Object names are `<name>-<role>`. An upgrade replaces the Deployment and
    the Service. An Ingress outside the chart must use the new Service name.
  - The Helm `replicaCount` moved to `roles.<name>.replicaCount`. The chart
    refuses the old key.
  - The Kustomize `deployment.yaml`, `service.yaml` and `pdb.yaml` moved from
    `base/` to `components/<role>/`. Overlay patches must use the new names.

### Breaking Changes

- **Breaking:** `DeployConfig` has the public field `groups`. A struct
  literal must set it ([migration guide](docs/migrations/next.md)).

### Fixed

- **setup:** `autumn setup` and `autumn assets` now retry a download that
  gets a 5xx or 429 answer, not only a dropped connection. They try five
  times, with a wait of 2, 4, 8 and 16 s, so a short outage of a release
  host does not stop the quickstart.

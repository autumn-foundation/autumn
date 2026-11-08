# Plan: one workload per role in release manifests and fleet deploys (issue #3547)

Part of #3540. Uses named roles (#3543) and route groups (#3544).

## Goal

`autumn release init` writes one workload per role. `autumn deploy` can put
a role on a group of hosts. `autumn doctor` warns when a manifest names a
role that `autumn.toml` does not declare.

## The role set

One function picks the roles for every target. It reads `[roles.*]` from
`autumn.toml`.

| Input | Roles |
|---|---|
| `--roles a,b` | `a`, `b`. Each must be a built-in role or a `[roles.<name>]` preset. |
| `--split-workers`, or one or more presets | `web`, `worker`, then each preset in name order. |
| Nothing | `combined`. |

Each role carries its name, a DNS-safe name and `serves_http` (from its
base). Two roles with the same DNS-safe name are an error.

## Brainstorming

1. Helm: one `roles:` map in `values.yaml`. The templates `range` over it.
   The chart stays generic. The generator writes only the values.
2. Helm: per role `enabled`, `servesHttp`, `replicaCount`, `resources`,
   `autoscaling`, `env`, `pin` and `ingress`. Shared values stay at the top.
3. Helm: only roles with `servesHttp` get a Service, an Ingress, a Rollout
   or a Flagger Canary. A worker role keeps a plain Deployment.
4. Helm: a role sets `AUTUMN_ROLE` and, when `pin` is set,
   `AUTUMN_JOBS__PIN`.
5. Selector labels add `app.kubernetes.io/component: <role>`. The PDB and
   the HPA are per role. The PodMonitor stays one object and copies the
   component label to the metrics.
6. Kustomize: `base/` lists one component per role in
   `components/<role>/`. Each component holds its own Deployment, PDB and,
   for an HTTP role, its Service and Ingress.
7. Fly: one `[processes]` entry per role. The process sets `AUTUMN_ROLE`
   with `env`. `[http_service]` names only the HTTP processes.
8. Fleet: `[[deploy.groups]]` with `role` and `hosts`. The env file of each
   host gets `AUTUMN_ROLE`. Groups keep their order. The rollout order is
   group order, then host order.
9. Doctor: one check reads the Helm `roles:` keys, `AUTUMN_ROLE` in the
   Kustomize, Fly and Compose files, and `[[deploy.groups]] role`.
10. CI: a golden copy of the Kubernetes output for an app with `web`,
    `worker` and one preset. The manifest script lints that copy.

## Reverse brainstorming

Question: "How can this put a role on the wrong pods, or lose traffic?"

| Way to fail | Guard |
|---|---|
| A worker gets a Service and an Ingress, and user requests get 404. | Only `servesHttp` roles get them. Golden and `helm template` checks. |
| Two roles share a selector, so a Service sends traffic to workers. | The component label is in every selector. Test. |
| A role name with `_` breaks a Kubernetes name. | The DNS-safe name. Collisions are an error. Test. |
| A long role name pushes an object name past 63 characters. | The Helm helper cuts the release part, and fails when the role is too long. |
| A manifest names a role that `autumn.toml` lost. The pod does not boot. | Doctor warns. Test. |
| A fleet group runs `worker` on the `local` jobs backend. Jobs never run. | Preflight fails, with the boot rule. Test. |
| A host is in two groups and gets two roles. | Error. Test. |
| `groups` with `host` or `hosts` makes the order unclear. | Error. Test. |
| The default output changes for an app with no roles. | One `combined` role. Same probes and timing. Test. |
| The generated chart and the golden copy drift. | The golden test compares bytes. `UPDATE_GOLDEN=1` writes it. |
| A preset changes base and the chart keeps `servesHttp`. | Doctor warns on the mismatch. |

## Six thinking hats

- **White.** The chart has one Deployment and no `AUTUMN_ROLE`. A fleet
  runs `combined` on every host. `ProcessRole`, `RolePreset` and
  `resolve_role` exist. A worker binds a probe-only listener on the app
  port.
- **Red.** An operator wants `kubectl get deploy` to show one line per role.
  Hand-written manifests feel fragile.
- **Black.** Object names change from `<release>-<chart>` to
  `<release>-<chart>-<role>`. An upgrade replaces the Deployment. Note it in
  the changelog. Helm has no YAML parser in the CLI, so doctor reads the
  `roles:` keys with a small line scan.
- **Yellow.** One chart serves every topology. `autumn doctor` catches drift
  between code and manifests.
- **Green.** Later: Compose per named role; per-role Fly VM sizes; a role
  column in `autumn deploy status`.
- **Blue.** Spec, then red tests, then code, then docs, the golden copy and
  CI.

## Spec

`verification/role_manifests.rs` models the role set and the fleet groups:

- Each manifest role is declared.
- A role gets a Service only when it serves HTTP.
- The role set has no duplicate names.
- Each fleet host has exactly one role, and the rollout order is the group
  order.

## Tests (red first)

- release: the role set for each input; DNS names; collisions; an unknown
  `--roles` entry.
- release: Helm `values.yaml` and the Kustomize tree for `web`, `worker` and
  `critical` match the golden copy.
- release: a `combined`-only app gets one role and the same probes.
- release: Fly writes `[processes]` for a split app, and nothing new for a
  `combined` app.
- deploy: groups parse, flatten in order, and refuse a blank role, an
  empty group, a duplicate host and a mix with `host` or `hosts`.
- deploy: each host env file carries its group role.
- deploy: preflight fails an undeclared role, and a split role on `local`.
- doctor: warns on an undeclared manifest role; passes a declared one.
- CI: `helm lint`, `helm template | kubeconform` and
  `kustomize build | kubeconform` on the golden copy.

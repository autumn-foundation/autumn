# Retention enforcement gaps (#2396)

Issue #2396 lists findings 1-12. #3288 and #3314 fixed 1-5. This plan covers the rest.

## Brainstorm

| Finding | Options | Choice |
|---|---|---|
| Archive on one replica | Split task by enforcement. Or run the purge on all replicas in one task. | Two tasks. Fleet for shared data, per-replica for the archive. |
| Hold off Postgres | Skip the TTL cap when held. Or report the gap. | Report the gap. The cap runs before the registry exists. |
| `job_history` on Redis | Purge Redis lists. Or report `NOT enforced`. | Report. A Redis purge is a new feature. |
| Symlinked archive | Refuse. Or resolve the link. | Resolve. Reads already follow the link. |
| Archive owner | Best-effort `chown`. Or `chown` and refuse on failure. | Refuse on failure. A kept stale entry is safer than a re-owned log. |
| Purge guard | Check in the CLI only. Or also in the app. | Both. The app knows the real profile. |
| Standalone hook index | Reuse the old version. Or add a new one. | New version. Versions must be unique. |

## Reverse brainstorm: how could this fix make things worse?

- A per-replica task purges twice on one host. Harmless: the purge is idempotent.
- A purge refused for a custom loader with no profile. Safe default. `--force` lifts it.
- `chown` fails for a non-root service. The purge fails loudly and the report shows it.
- Two migrations create the same index. `IF NOT EXISTS` makes the second a no-op.
- An inherited `AUTUMN_DB_RETENTION_FORCE` forces a purge. The CLI sets or removes it on every run.

## Six hats

- White: the facts are in the code and the issue comments.
- Red: an operator who sees `rows_removed: 0` trusts it. That trust is the risk.
- Black: new tasks change `/actuator/tasks`. Docs and tests name them.
- Yellow: each fix is small and local.
- Green: a later change can add a Redis purge and a SQLite sweep.
- Blue: red tests first, then green, then refactor, then review.

## Not in this change

- Finding 6 (clock skew): `expires_at` already uses the database clock (#2875). The `updated_at`
  column is also the compare-and-swap token, so it needs a new column. The skew is minutes
  against a window of days.
- Finding 10 (SQLite `commit_hooks` sweep): needs a SQLite build and its own tests.
- Redis purge for `job_history`.
- Web-role replicas do not run tasks, so they do not purge their archive. The guide says so.

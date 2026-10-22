### Fixed

- **jobs:** a stale job attempt no longer publishes a terminal tracked status
  (issue #2583). On `sqlite`, `postgres` and `redis`, the worker writes the
  tracked `succeeded` or `failed` status only after the queue accepts its
  claim-checked ack. Before, an attempt that lost its claim could write
  `failed` for a job that was still running. The store then refused the
  result of the new owner. See [tracked jobs](docs/guide/jobs.md#tracked-status-follows-the-queue-ack).

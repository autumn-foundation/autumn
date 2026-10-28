# Running behind PgBouncer / RDS Proxy

A connection pooler sits between the app and Postgres. It shares a small
number of server connections between many clients. This guide tells you which
Autumn features work behind each pooler mode.

## Pooler modes

| Mode | A client keeps one server session for | Examples |
|------|---------------------------------------|----------|
| Session | The full client connection | PgBouncer `pool_mode = session` |
| Transaction | One transaction | PgBouncer `pool_mode = transaction`, Supavisor port 6543, the Neon `-pooler` host |
| Proxy with pinning | One transaction, until the client sets session state, then the full connection | AWS RDS Proxy |

Session mode works with every Autumn feature. Transaction mode does not keep
session state between transactions. The table below shows what that breaks.

## Compatibility matrix

| Autumn feature | Session mode | Transaction mode | RDS Proxy |
|----------------|--------------|------------------|-----------|
| Repository reads and writes, `Db`, transactions | Works | Works | Works |
| Transaction advisory locks (scheduler tick table, counter cache, positions) | Works | Works | Works |
| Session advisory locks: `Lock` ([distributed locks](distributed-locks.md)), migration lock, ISR coordinator, `autumn-search` Postgres backend | Works | **Broken.** The unlock can run on a different server session, so the lock leaks. | Works, but pins the connection |
| `database.statement_timeout` (a `SET` on each checkout) | Works | **Broken.** The `SET` applies to a different session, or to another client. | Works, but pins every connection |
| Prepared statements (diesel-async caches them) | Works | Works only with PgBouncer 1.21 or later and `max_prepared_statements` above `0` | Works. Some forms pin the connection. |
| `LISTEN` (shard directory invalidation) | Works | **Broken.** Notifications do not arrive. | Pins the connection |

Pinning keeps one server connection for one client. This removes the pooling
benefit for that client.

## Recommended setup

1. Use session mode when you can. Then you need no other change.
2. Run `autumn migrate` with a direct URL, not the pooler URL. The migration
   lock is a session advisory lock. For the same reason, set
   `database.auto_migrate = false` when `database.url` is a pooler URL.
3. In transaction mode, do not set `database.statement_timeout`. Set the
   timeout on the database role:

   ```sql
   ALTER ROLE app SET statement_timeout = '5s';
   ```

4. In transaction mode, do not use `Lock` on the pooled URL. Do not use a
   shard directory listener on it.
5. With PgBouncer in transaction mode, set `max_prepared_statements` (for
   example `200`) in `pgbouncer.ini`.

## Boot warning

At boot, Autumn examines `database.url`, `database.primary_url`,
`database.replica_url`, and the `primary_url` and `replica_url` of each
`[[database.shards]]` entry. When a URL looks like a pooler, the app logs a
warning that names the key, for example `database.shards[1].primary_url`.
These URL parts start the warning:

| URL part | Pooler |
|----------|--------|
| `pgbouncer=true` in the query | PgBouncer |
| Port `6432` | PgBouncer |
| Host `*.proxy-*.rds.amazonaws.com` | RDS Proxy |
| Host `*.pooler.supabase.com` on port `6543` | Supavisor (transaction mode) |
| Host `*-pooler.*.neon.tech` | the Neon pooler |

The check is a heuristic. It cannot find the pool mode. When your pooler is
in session mode, or the warning is wrong, turn it off:

```toml
[database]
warn_on_pooler = false
```

The environment variable is `AUTUMN_DATABASE__WARN_ON_POOLER=false`.

## Related

- [Distributed locks](distributed-locks.md)
- [Scheduled tasks across replicas](scheduled-multi-replica.md)
- [Cloud-native Autumn](cloud-native.md#replication-lag-and-read-your-own-writes)

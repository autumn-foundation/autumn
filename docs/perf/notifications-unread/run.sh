#!/usr/bin/env bash
# usage: run.sh <hist> [with_index]  -> prints the workload report. Needs PG env vars, pg_stat_statements preloaded.
set -euo pipefail
d=$(dirname "$0")
psql -q -v hist="$1" -f "$d/fixture.sql" >/dev/null
[ "${2:-}" = with_index ] && psql -q -c "CREATE INDEX idx_notifications_unread ON notifications (tenant_id, recipient_id, id) WHERE read_at IS NULL" -c "VACUUM ANALYZE notifications" >/dev/null
psql -q -f "$d/workload.sql"
psql -q -Atc "select 'sizes: table='||pg_size_pretty(pg_table_size('notifications'))||' idx_shipped='||pg_size_pretty(pg_relation_size('idx_notifications_tenant_recipient'))||' idx_unread='||coalesce(pg_size_pretty(pg_relation_size(to_regclass('idx_notifications_unread'))),'-')"

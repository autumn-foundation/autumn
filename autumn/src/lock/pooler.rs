//! Connection-pooler hint (issue #3053).
//!
//! A transaction-mode pooler (`PgBouncer`, RDS Proxy, and others) can give each
//! transaction a different server session. Session advisory locks do not
//! work there: the lock stays on a session that the app no longer owns. The
//! advisory [`Lock`](crate::lock::Lock) and the migration lock use session
//! advisory locks.
//!
//! A client cannot ask a pooler for its mode. So this module reads the
//! database target for well-known pooler ports and host names, and the pool
//! builder logs one warning per target. The check is a hint: a pooler on a
//! plain host name and port is not found, and a multi-host URL is not read.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// A pooler that the database target points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolerHint {
    /// `PgBouncer`: port 6432, a `pgbouncer` host, or `pgbouncer=true`.
    PgBouncer,
    /// Amazon RDS Proxy: a `*.proxy-*.rds.amazonaws.com` host.
    RdsProxy,
    /// Supabase Supavisor in transaction mode: a `*.pooler.supabase.com`
    /// host on port 6543. Port 5432 there is session mode, which is safe.
    Supabase,
    /// Neon pooler: a `*-pooler.*` host on `neon.tech`.
    Neon,
    /// Port 6543, the usual transaction-pooler port, on another host.
    PoolerPort(u16),
}

impl std::fmt::Display for PoolerHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PgBouncer => f.write_str("PgBouncer"),
            Self::RdsProxy => f.write_str("Amazon RDS Proxy"),
            Self::Supabase => f.write_str("Supabase pooler"),
            Self::Neon => f.write_str("Neon pooler"),
            Self::PoolerPort(port) => write!(f, "a pooler port ({port})"),
        }
    }
}

/// The parts of a target that the hint reads.
struct Endpoint {
    host: String,
    port: Option<u16>,
    pgbouncer_flag: bool,
}

/// Read host, port and the `pgbouncer` flag from a URL or from a libpq
/// keyword/value string. Query parameters `host` and `port` win over the
/// authority, as in libpq.
fn endpoint(target: &str) -> Option<Endpoint> {
    if let Ok(url) = url::Url::parse(target) {
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return None;
        }
        let mut endpoint = Endpoint {
            host: url.host_str().unwrap_or_default().to_owned(),
            port: url.port(),
            pgbouncer_flag: false,
        };
        for (key, value) in url.query_pairs() {
            match &*key {
                "host" => endpoint.host = value.into_owned(),
                "port" => endpoint.port = value.parse().ok(),
                "pgbouncer" => endpoint.pgbouncer_flag = value == "true",
                _ => {}
            }
        }
        return Some(endpoint);
    }
    let pairs = crate::pg_conn_str::keyword_value_pairs(target)?;
    let mut endpoint = Endpoint {
        host: String::new(),
        port: None,
        pgbouncer_flag: false,
    };
    for (key, value) in pairs {
        match key.as_str() {
            "host" => endpoint.host = value,
            "port" => endpoint.port = value.parse().ok(),
            _ => {}
        }
    }
    Some(endpoint)
}

/// Find a well-known pooler in a `PostgreSQL` target. Returns `None` for other
/// targets and for a value that does not parse.
pub fn pooler_hint(target: &str) -> Option<PoolerHint> {
    let Endpoint {
        host,
        port,
        pgbouncer_flag,
    } = endpoint(target)?;
    let host = host.to_ascii_lowercase();
    if host.contains(".proxy-") && host.ends_with(".rds.amazonaws.com") {
        return Some(PoolerHint::RdsProxy);
    }
    if host.ends_with(".pooler.supabase.com") {
        return (port == Some(6543)).then_some(PoolerHint::Supabase);
    }
    if host.ends_with(".neon.tech") && host.split('.').next()?.ends_with("-pooler") {
        return Some(PoolerHint::Neon);
    }
    if port == Some(6432) || host.contains("pgbouncer") || pgbouncer_flag {
        return Some(PoolerHint::PgBouncer);
    }
    if port == Some(6543) {
        return Some(PoolerHint::PoolerPort(6543));
    }
    None
}

/// Log one warning when `target` points to a well-known pooler. Each target
/// warns once per process. Returns `true` when this call logged.
pub fn warn_if_pooled(target: &str) -> bool {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let Some(pooler) = pooler_hint(target) else {
        return false;
    };
    let redacted = crate::db_url::redact_target(target);
    let first = WARNED
        .get_or_init(Mutex::default)
        .lock()
        .map_or(true, |mut seen| seen.insert(redacted.clone()));
    if first {
        tracing::warn!(
            target = %redacted,
            pooler = %pooler,
            "the database target points to a connection pooler ({pooler}). In transaction \
             mode, session advisory locks are not safe: `Lock` and the migration lock can \
             leak or lose their lock. Use `LeaseLock` for \
             mutual exclusion, and run migrations on a direct connection. See \
             docs/guide/distributed-locks.md, \"Connection poolers\"."
        );
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_postgres_is_not_a_pooler() {
        assert_eq!(pooler_hint("postgres://u:p@localhost:5432/app"), None);
        assert_eq!(pooler_hint("postgres://u:p@db.internal/app"), None);
    }

    #[test]
    fn pgbouncer_port_and_host_are_poolers() {
        assert_eq!(
            pooler_hint("postgres://u:p@db:6432/app"),
            Some(PoolerHint::PgBouncer)
        );
        assert_eq!(
            pooler_hint("postgres://u:p@pgbouncer.internal:5432/app"),
            Some(PoolerHint::PgBouncer)
        );
        assert_eq!(
            pooler_hint("postgres://u:p@db:5432/app?pgbouncer=true"),
            Some(PoolerHint::PgBouncer)
        );
    }

    #[test]
    fn libpq_forms_are_read() {
        assert_eq!(
            pooler_hint("host=db port=6432 user=app password=secret dbname=app"),
            Some(PoolerHint::PgBouncer)
        );
        assert_eq!(
            pooler_hint("postgres:///app?host=db&port=6432"),
            Some(PoolerHint::PgBouncer)
        );
        assert_eq!(pooler_hint("host=db port=5432 dbname=app"), None);
    }

    #[test]
    fn managed_poolers_are_detected() {
        assert_eq!(
            pooler_hint("postgres://u:p@app.proxy-abc123.us-east-1.rds.amazonaws.com:5432/app"),
            Some(PoolerHint::RdsProxy)
        );
        assert_eq!(
            pooler_hint("postgres://u:p@aws-0-eu-west-1.pooler.supabase.com:6543/postgres"),
            Some(PoolerHint::Supabase)
        );
        assert_eq!(
            pooler_hint("postgres://u:p@ep-cool-name-123456-pooler.us-east-2.aws.neon.tech/app"),
            Some(PoolerHint::Neon)
        );
    }

    #[test]
    fn supabase_session_mode_is_not_flagged() {
        assert_eq!(
            pooler_hint("postgres://u:p@aws-0-eu-west-1.pooler.supabase.com:5432/postgres"),
            None
        );
    }

    #[test]
    fn port_6543_elsewhere_is_a_generic_pooler_port() {
        assert_eq!(
            pooler_hint("postgres://u:p@db.internal:6543/app"),
            Some(PoolerHint::PoolerPort(6543))
        );
    }

    #[test]
    fn non_postgres_and_garbage_are_not_poolers() {
        assert_eq!(pooler_hint("sqlite://app.db"), None);
        assert_eq!(pooler_hint("not a url"), None);
        assert_eq!(pooler_hint(""), None);
    }

    #[test]
    fn hint_names_the_pooler() {
        assert!(PoolerHint::PgBouncer.to_string().contains("PgBouncer"));
        assert!(PoolerHint::RdsProxy.to_string().contains("RDS Proxy"));
        assert!(PoolerHint::PoolerPort(6543).to_string().contains("6543"));
    }

    #[test]
    fn each_target_warns_once() {
        let target = "postgres://u:p@warn-once-test:6432/app";
        assert!(warn_if_pooled(target), "the first build warns");
        assert!(
            !warn_if_pooled(target),
            "a second pool on one target does not"
        );
        assert!(!warn_if_pooled("postgres://u:p@direct-test:5432/app"));
    }
}

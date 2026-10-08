//! Connection pooler detection (issue #3065).
//!
//! A transaction-mode pooler (`PgBouncer`, Supavisor, the Neon pooler) can
//! give each transaction a different server session. Session advisory locks,
//! a session `SET statement_timeout` and prepared statements then do not work
//! as expected. RDS Proxy pins the session instead, which removes the
//! pooling benefit. [`detect_pooler`] finds a pooler from the
//! database URL, so the app can warn at boot. See
//! `docs/guide/connection-poolers.md`.

/// A pooler found in a database URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolerHint {
    /// The pooler product.
    pub kind: PoolerKind,
    /// The part of the URL that shows the pooler.
    pub reason: &'static str,
}

/// Known pooler products.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolerKind {
    /// `PgBouncer`.
    PgBouncer,
    /// AWS RDS Proxy.
    RdsProxy,
    /// Supabase Supavisor.
    Supavisor,
    /// The Neon connection pooler (`PgBouncer`).
    NeonPooler,
}

/// Find a connection pooler from a Postgres URL or keyword/value string.
///
/// This is a heuristic. It returns `None` when the URL shows no pooler.
#[must_use]
pub fn detect_pooler(database_url: &str) -> Option<PoolerHint> {
    let target = Target::parse(database_url)?;
    let host = target.host.to_ascii_lowercase();
    let hint = |kind, reason| Some(PoolerHint { kind, reason });
    if target.pgbouncer_flag {
        return hint(PoolerKind::PgBouncer, "`pgbouncer=true` in the URL");
    }
    if host.ends_with(".rds.amazonaws.com") && host.contains(".proxy-") {
        return hint(PoolerKind::RdsProxy, "an RDS Proxy host name");
    }
    // Port 5432 on this host is session mode, which works with every
    // feature. Port 6543 is transaction mode.
    if host.ends_with(".pooler.supabase.com") && target.port == Some(6543) {
        return hint(
            PoolerKind::Supavisor,
            "a Supabase pooler host name on port 6543 (transaction mode)",
        );
    }
    if host.ends_with(".neon.tech")
        && host
            .split('.')
            .next()
            .is_some_and(|label| label.ends_with("-pooler"))
    {
        return hint(PoolerKind::NeonPooler, "a Neon `-pooler` host name");
    }
    if target.port == Some(6432) {
        return hint(PoolerKind::PgBouncer, "port 6432, the PgBouncer default");
    }
    None
}

impl PoolerKind {
    /// The product name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PgBouncer => "PgBouncer",
            Self::RdsProxy => "RDS Proxy",
            Self::Supavisor => "Supavisor",
            Self::NeonPooler => "the Neon pooler",
        }
    }
}

/// The parts of a connection string that show a pooler.
struct Target {
    host: String,
    port: Option<u16>,
    pgbouncer_flag: bool,
}

impl Target {
    fn parse(database_url: &str) -> Option<Self> {
        if crate::pg_conn_str::is_url(database_url) {
            let url = url::Url::parse(database_url).ok()?;
            Some(Self {
                host: url.host_str().unwrap_or_default().to_owned(),
                port: url.port(),
                pgbouncer_flag: url
                    .query_pairs()
                    .any(|(key, value)| key == "pgbouncer" && value == "true"),
            })
        } else {
            let pairs = crate::pg_conn_str::keyword_value_pairs(database_url)?;
            let value = |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.as_str())
            };
            Some(Self {
                host: value("host").unwrap_or_default().to_owned(),
                port: value("port").and_then(|port| port.parse().ok()),
                pgbouncer_flag: false,
            })
        }
    }
}

/// Boot warnings for every configured database URL that looks like a
/// pooler. Empty when `database.warn_on_pooler` is `false`.
#[must_use]
pub fn pooler_warnings(config: &crate::config::DatabaseConfig) -> Vec<String> {
    if !config.warn_on_pooler {
        return Vec::new();
    }
    let primary_key = if config.primary_url.is_some() {
        "database.primary_url"
    } else {
        "database.url"
    };
    let mut urls = vec![
        (primary_key.to_owned(), config.effective_primary_url()),
        (
            "database.replica_url".to_owned(),
            config.replica_url.as_deref(),
        ),
    ];
    // Shard checkouts use the same session settings and prepared statements.
    for (index, shard) in config.shards.iter().enumerate() {
        urls.push((
            format!("database.shards[{index}].primary_url"),
            Some(shard.primary_url.as_str()),
        ));
        urls.push((
            format!("database.shards[{index}].replica_url"),
            shard.replica_url.as_deref(),
        ));
    }
    urls.into_iter()
        .filter_map(|(key, url)| Some((key, detect_pooler(url?)?)))
        .map(|(key, hint)| {
            format!(
                "{key} looks like {} ({}). In transaction mode, session advisory locks \
             (`Lock`, migrations, ISR), the per-checkout `SET statement_timeout` and \
             prepared statements do not work as expected. See \
             docs/guide/connection-poolers.md. Set database.warn_on_pooler = false to \
             silence this warning.",
                hint.kind.name(),
                hint.reason
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(url: &str) -> Option<PoolerKind> {
        detect_pooler(url).map(|hint| hint.kind)
    }

    #[test]
    fn detects_pgbouncer_by_default_port() {
        assert_eq!(
            kind("postgres://app:pw@db.internal:6432/app"),
            Some(PoolerKind::PgBouncer)
        );
        assert_eq!(
            kind("host=db.internal port=6432 dbname=app"),
            Some(PoolerKind::PgBouncer)
        );
    }

    #[test]
    fn detects_pgbouncer_by_query_flag() {
        assert_eq!(
            kind("postgres://app@db:5432/app?pgbouncer=true"),
            Some(PoolerKind::PgBouncer)
        );
    }

    #[test]
    fn detects_managed_poolers_by_host() {
        assert_eq!(
            kind("postgres://app@my-proxy.proxy-abc123.us-east-1.rds.amazonaws.com:5432/app"),
            Some(PoolerKind::RdsProxy)
        );
        assert_eq!(
            kind("postgres://app@aws-0-eu-west-1.pooler.supabase.com:6543/postgres"),
            Some(PoolerKind::Supavisor)
        );
        assert_eq!(
            kind("postgres://app@ep-cool-name-123-pooler.us-east-2.aws.neon.tech/neondb"),
            Some(PoolerKind::NeonPooler)
        );
    }

    #[test]
    fn plain_postgres_is_not_a_pooler() {
        assert_eq!(kind("postgres://app@localhost:5432/app"), None);
        assert_eq!(kind("postgres://app@localhost/app"), None);
        assert_eq!(
            kind("postgres://app@db.abc123.us-east-1.rds.amazonaws.com/app"),
            None
        );
        assert_eq!(kind("host=localhost dbname=app"), None);
        assert_eq!(
            kind("postgres://app@aws-0-eu-west-1.pooler.supabase.com:5432/postgres"),
            None,
            "Supavisor session mode works with every feature"
        );
        assert_eq!(kind("not a url"), None);
    }

    #[test]
    fn hint_names_the_reason() {
        let hint = detect_pooler("postgres://db:6432/app").unwrap();
        assert!(hint.reason.contains("6432"), "{}", hint.reason);
    }

    fn db(primary: &str, replica: Option<&str>) -> crate::config::DatabaseConfig {
        crate::config::DatabaseConfig {
            url: Some(primary.to_owned()),
            replica_url: replica.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn warns_once_per_pooled_url_and_names_the_guide() {
        let warnings = pooler_warnings(&db(
            "postgres://db:6432/app",
            Some("postgres://ro.proxy-x.eu-west-1.rds.amazonaws.com/app"),
        ));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("PgBouncer"), "{}", warnings[0]);
        assert!(warnings[0].contains("database.url"), "{}", warnings[0]);
        assert!(warnings[1].contains("RDS Proxy"), "{}", warnings[1]);
        assert!(
            warnings[1].contains("database.replica_url"),
            "{}",
            warnings[1]
        );
        for warning in &warnings {
            assert!(warning.contains("docs/guide/connection-poolers.md"));
            assert!(warning.contains("advisory lock"));
            assert!(!warning.contains("6432/app"), "URL must not be logged");
        }
    }

    #[test]
    fn warns_for_pooled_shard_urls_with_the_indexed_key() {
        let mut config = db("postgres://db:5432/app", None);
        config.shards = vec![
            crate::config::ShardConfig {
                name: "us".to_owned(),
                primary_url: "postgres://us-db:5432/app".to_owned(),
                ..Default::default()
            },
            crate::config::ShardConfig {
                name: "eu".to_owned(),
                primary_url: "postgres://eu-pool:6432/app".to_owned(),
                replica_url: Some("postgres://eu-ro.pooler.supabase.com:6543/app".to_owned()),
                ..Default::default()
            },
        ];
        let warnings = pooler_warnings(&config);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].contains("database.shards[1].primary_url"),
            "{}",
            warnings[0]
        );
        assert!(
            warnings[1].contains("database.shards[1].replica_url"),
            "{}",
            warnings[1]
        );
        assert!(warnings.iter().all(|w| !w.contains("shards[0]")));
    }

    #[test]
    fn no_warning_when_disabled_or_direct() {
        let mut config = db("postgres://db:6432/app", None);
        config.warn_on_pooler = false;
        assert!(pooler_warnings(&config).is_empty());
        assert!(pooler_warnings(&db("postgres://db:5432/app", None)).is_empty());
    }
}

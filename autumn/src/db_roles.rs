//! One database role per boundary (#3550).
//!
//! A boundary in `boundaries.toml` can have its own Postgres role. The app
//! connects as that role with `[database.roles.<boundary>]`:
//!
//! ```toml
//! [database.roles.marketing]
//! user = "marketing"          # on the host and database of `database.url`
//! password = "..."            # or AUTUMN_DATABASE__ROLES__MARKETING__PASSWORD
//!
//! [database.roles.billing]
//! url = "postgres://billing:secret@db/app"
//! pool_size = 4
//! ```
//!
//! Code in the boundary gets the pool of its role with
//! [`AppState::pool_for`](crate::state::AppState::pool_for), or a connection
//! with the [`BoundaryDb`](crate::db::BoundaryDb) extractor. `autumn db grants`
//! gives each role access to its own tables only.
//!
//! An app with no `[database.roles]` keeps one role. Then `pool_for` gives the
//! primary pool.
//!
//! See `docs/guide/database-boundaries.md`.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::config::Env;

/// A boundary of the app, as a type.
///
/// ```rust
/// use autumn_web::db_roles::Boundary;
///
/// pub struct Marketing;
///
/// impl Boundary for Marketing {
///     const NAME: &'static str = "marketing";
/// }
/// ```
pub trait Boundary: Send + Sync + 'static {
    /// The boundary name, as in `boundaries.toml` and
    /// `[database.roles.<name>]`.
    const NAME: &'static str;
}

/// `[database.roles.<boundary>]`: how the boundary connects.
///
/// Set `url`, or set `user` (and `password`) to use the host and database of
/// the primary URL.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseRoleConfig {
    /// A full Postgres URL for the role.
    #[serde(default)]
    pub url: Option<String>,
    /// The role name, on the primary URL.
    #[serde(default)]
    pub user: Option<String>,
    /// The password for `user`.
    #[serde(default)]
    pub password: Option<secrecy::SecretString>,
    /// The pool size. When unset, `database.pool_size`.
    #[serde(default)]
    pub pool_size: Option<usize>,
}

impl fmt::Debug for DatabaseRoleConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseRoleConfig")
            .field(
                "url",
                &self.url.as_deref().map(crate::db_url::redact_target),
            )
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("pool_size", &self.pool_size)
            .finish()
    }
}

impl DatabaseRoleConfig {
    /// The URL that the role connects with.
    ///
    /// `url` is used as it is. `user` replaces the user and the password of
    /// `primary`. A role with no password gets no password: the password of
    /// the primary URL belongs to another role.
    ///
    /// # Errors
    ///
    /// Returns a message when both or neither of `url` and `user` are set,
    /// when `user` is set and `primary` is not a `postgres://` URL, or when
    /// the result does not name Postgres.
    pub fn effective_url(&self, primary: Option<&str>) -> Result<String, String> {
        let url = match (&self.url, &self.user) {
            (Some(_), Some(_)) => return Err("set `url` or `user`, not both".to_owned()),
            (None, None) => return Err("set `url` or `user`".to_owned()),
            (Some(url), None) => url.clone(),
            (None, Some(user)) => with_user(primary, user, self.password.as_ref())?,
        };
        if crate::config::DatabaseBackend::detect(&url)
            != Some(crate::config::DatabaseBackend::Postgres)
        {
            return Err(format!(
                "a database role needs a Postgres URL, got {:?}",
                crate::db_url::redact_target(&url)
            ));
        }
        Ok(url)
    }
}

/// `primary` with the user and the password of a role.
fn with_user(
    primary: Option<&str>,
    user: &str,
    password: Option<&secrecy::SecretString>,
) -> Result<String, String> {
    let primary = primary.ok_or("`user` needs a Postgres `database.url`; set `url` instead")?;
    let not_url = || "`user` needs a postgres:// `database.url`; set `url` instead".to_owned();
    // A Postgres URI, read by hand: a generic URL parser refuses a host list
    // (`db1:5432,db2:5432`) and a URI with no host.
    let (scheme, rest) = ["postgres://", "postgresql://"]
        .into_iter()
        .find_map(|scheme| primary.strip_prefix(scheme).map(|rest| (scheme, rest)))
        .ok_or_else(not_url)?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (userinfo, hosts) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let decode = |text: &str| {
        percent_encoding::percent_decode_str(text)
            .decode_utf8_lossy()
            .into_owned()
    };
    // A `user` or `password` in the query wins over the user part, so the
    // role would connect as the owner. Remove them. The other parameters
    // stay byte for byte: Postgres does not read `+` as a space. The driver
    // decodes a name, so `%75ser` is `user`.
    let mut owner = decode(userinfo.split_once(':').map_or(userinfo, |(u, _)| u));
    let mut pairs: Vec<String> = Vec::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match decode(key).as_str() {
            "" | "password" | "passfile" => {}
            "user" => owner = decode(value),
            _ => pairs.push(pair.to_owned()),
        }
    }
    // With no database, Postgres uses the user name. Keep the database of
    // the primary URL when the user changes.
    let has_dbname = pairs
        .iter()
        .any(|pair| decode(pair.split_once('=').map_or(pair, |(k, _)| k)) == "dbname");
    let pin_database = matches!(path, "" | "/") && !has_dbname && !owner.is_empty();
    let password = password.map(secrecy::ExposeSecret::expose_secret);
    let mut path = path.to_owned();
    let authority = if hosts.is_empty() {
        // A socket URI (`postgres:///app?host=/run/postgresql`) has no user
        // part: the credentials go in the query.
        if pin_database {
            pairs.push(format!("dbname={}", encode(&owner)));
        }
        pairs.push(format!("user={}", encode(user)));
        if let Some(password) = password {
            pairs.push(format!("password={}", encode(password)));
        }
        String::new()
    } else {
        if pin_database {
            path = format!("/{}", encode(&owner));
        }
        let credentials = password.map_or_else(
            || encode(user),
            |password| format!("{}:{}", encode(user), encode(password)),
        );
        format!("{credentials}@{hosts}")
    };
    let query = pairs.join("&");
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    Ok(format!("{scheme}{authority}{path}{query}"))
}

/// Percent-encode a URL part. Letters, digits and `-._~` stay.
fn encode(text: &str) -> String {
    const PART: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(text, PART).to_string()
}

/// A role name as it appears in an env var: upper case, with `-` as `_`.
#[must_use]
pub fn env_name(name: &str) -> String {
    name.to_ascii_uppercase().replace('-', "_")
}

/// Apply the env overrides of `[database.roles]`.
///
/// `AUTUMN_DATABASE__ROLES` is a comma list of names. It declares roles that
/// `autumn.toml` does not. Each role then reads
/// `AUTUMN_DATABASE__ROLES__<NAME>__URL`, `__USER`, `__PASSWORD` and
/// `__POOL_SIZE`. `<NAME>` is the name in upper case, with `-` as `_`.
pub fn apply_env_overrides(roles: &mut BTreeMap<String, DatabaseRoleConfig>, env: &dyn Env) {
    if let Ok(names) = env.var("AUTUMN_DATABASE__ROLES") {
        for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            roles.entry(name.to_owned()).or_default();
        }
    }
    let read = |key: &str| env.var(key).ok().map(|v| v.trim().to_owned());
    for (name, role) in roles.iter_mut() {
        let upper = env_name(name);
        if let Some(v) = read(&format!("AUTUMN_DATABASE__ROLES__{upper}__URL")) {
            role.url = (!v.is_empty()).then_some(v);
        }
        if let Some(v) = read(&format!("AUTUMN_DATABASE__ROLES__{upper}__USER")) {
            role.user = (!v.is_empty()).then_some(v);
        }
        // A password keeps its spaces: they can be part of it.
        if let Ok(v) = env.var(&format!("AUTUMN_DATABASE__ROLES__{upper}__PASSWORD")) {
            role.password = (!v.is_empty()).then(|| secrecy::SecretString::from(v));
        }
        let pool_key = format!("AUTUMN_DATABASE__ROLES__{upper}__POOL_SIZE");
        if let Some(v) = read(&pool_key) {
            if v.is_empty() {
                role.pool_size = None;
            } else if let Ok(size) = v.parse() {
                role.pool_size = Some(size);
            } else {
                eprintln!("Warning: {pool_key}={v:?} is not valid, ignoring");
            }
        }
    }
}

/// The largest connection count of all role pools together.
#[must_use]
pub fn max_connections(database: &crate::config::DatabaseConfig) -> usize {
    database
        .roles
        .values()
        .map(|r| r.pool_size.unwrap_or(database.pool_size))
        .sum()
}

/// Check the name and the URL of each role.
///
/// # Errors
///
/// Returns a message that names the bad key.
pub fn validate(
    roles: &BTreeMap<String, DatabaseRoleConfig>,
    primary: Option<&str>,
) -> Result<(), String> {
    let mut env_names: BTreeMap<String, &str> = BTreeMap::new();
    for (name, role) in roles {
        let key = format!("database.roles.{name}");
        if let Some(other) = env_names.insert(env_name(name), name) {
            return Err(format!(
                "{key}: `{other}` and `{name}` have one env name, \
                 AUTUMN_DATABASE__ROLES__{}__*; rename one",
                env_name(name)
            ));
        }
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "{key}: a role name must match [a-z0-9_-], because it is a boundary name"
            ));
        }
        if role.pool_size == Some(0) {
            return Err(format!("{key}.pool_size must be 1 or more"));
        }
        role.effective_url(primary)
            .map_err(|message| format!("{key}: {message}"))?;
    }
    Ok(())
}

/// The pools of `[database.roles]`, by boundary name. The app state holds
/// one when the config declares a role.
#[cfg(feature = "db")]
#[derive(Clone, Default)]
pub struct RolePools {
    pools: BTreeMap<String, crate::db::Pool<crate::db::RuntimeConnection>>,
    warned: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

#[cfg(feature = "db")]
impl RolePools {
    /// The pools, by boundary name.
    #[must_use]
    pub fn new(pools: BTreeMap<String, crate::db::Pool<crate::db::RuntimeConnection>>) -> Self {
        Self {
            pools,
            warned: std::sync::Arc::default(),
        }
    }

    /// The pool of `name`. A missing name logs one warning, then gives
    /// `None`: the boundary must not fall back to the owner role.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<crate::db::Pool<crate::db::RuntimeConnection>> {
        if let Some(pool) = self.pools.get(name) {
            return Some(pool.clone());
        }
        let first = self
            .warned
            .lock()
            .map_or(true, |mut warned| warned.insert(name.to_owned()));
        if first {
            tracing::warn!(
                boundary = name,
                "no [database.roles.{name}] entry; the boundary gets no database connection"
            );
        }
        None
    }

    /// The boundary names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.pools.keys().map(String::as_str)
    }

    /// Whether no role is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }
}

#[cfg(feature = "db")]
impl fmt::Debug for RolePools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RolePools")
            .field("roles", &self.pools.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;

    struct MapEnv(BTreeMap<&'static str, &'static str>);

    impl Env for MapEnv {
        fn var(&self, key: &str) -> Result<String, std::env::VarError> {
            self.0
                .get(key)
                .map(|v| (*v).to_owned())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    fn role(toml_text: &str) -> DatabaseRoleConfig {
        toml::from_str(toml_text).expect("role config")
    }

    const PRIMARY: &str = "postgres://owner:ownerpw@db.internal:5432/app?sslmode=require";

    #[test]
    fn url_is_used_as_it_is() {
        let r = role(r#"url = "postgres://m:pw@other/app""#);
        assert_eq!(
            r.effective_url(Some(PRIMARY)).unwrap(),
            "postgres://m:pw@other/app"
        );
    }

    #[test]
    fn user_replaces_the_user_and_password_of_the_primary_url() {
        let r = role("user = \"marketing\"\npassword = \"s3cret\"");
        assert_eq!(
            r.effective_url(Some(PRIMARY)).unwrap(),
            "postgres://marketing:s3cret@db.internal:5432/app?sslmode=require"
        );
    }

    #[test]
    fn user_with_no_password_drops_the_owner_password() {
        let r = role(r#"user = "marketing""#);
        assert_eq!(
            r.effective_url(Some(PRIMARY)).unwrap(),
            "postgres://marketing@db.internal:5432/app?sslmode=require"
        );
    }

    #[test]
    fn user_drops_the_credentials_in_the_query() {
        let r = role("user = \"marketing\"\npassword = \"pw\"");
        assert_eq!(
            r.effective_url(Some(
                "postgres://db/app?user=owner&sslmode=require&password=opw&passfile=/x"
            ))
            .unwrap(),
            "postgres://marketing:pw@db/app?sslmode=require"
        );
        assert_eq!(
            r.effective_url(Some("postgres://db/app?user=owner"))
                .unwrap(),
            "postgres://marketing:pw@db/app"
        );
        assert_eq!(
            r.effective_url(Some(
                "postgres://db/app?%75ser=owner&%70assword=opw&sslmode=require"
            ))
            .unwrap(),
            "postgres://marketing:pw@db/app?sslmode=require"
        );
    }

    #[test]
    fn the_implicit_database_of_the_primary_url_stays() {
        let r = role(r#"user = "marketing""#);
        assert_eq!(
            r.effective_url(Some("postgres://owner:pw@db")).unwrap(),
            "postgres://marketing@db/owner"
        );
        assert_eq!(
            r.effective_url(Some("postgres://owner@db/?dbname=app"))
                .unwrap(),
            "postgres://marketing@db/?dbname=app"
        );
    }

    #[test]
    fn a_socket_url_gets_the_credentials_in_the_query() {
        let r = role("user = \"marketing\"\npassword = \"p w&\"");
        assert_eq!(
            r.effective_url(Some("postgres:///app?host=/run/postgresql&user=owner"))
                .unwrap(),
            "postgres:///app?host=/run/postgresql&user=marketing&password=p%20w%26"
        );
        // No database: the one of the owner stays.
        assert_eq!(
            r.effective_url(Some("postgres://?host=/run/postgresql&user=owner"))
                .unwrap(),
            "postgres://?host=/run/postgresql&dbname=owner&user=marketing&password=p%20w%26"
        );
    }

    #[test]
    fn a_host_list_stays() {
        let r = role("user = \"marketing\"\npassword = \"pw\"");
        assert_eq!(
            r.effective_url(Some("postgres://owner:opw@db1:5432,db2:5432/app"))
                .unwrap(),
            "postgres://marketing:pw@db1:5432,db2:5432/app"
        );
        assert_eq!(
            r.effective_url(Some("postgresql://o@[::1]:5432/app"))
                .unwrap(),
            "postgresql://marketing:pw@[::1]:5432/app"
        );
    }

    #[test]
    fn other_query_parameters_stay_as_they_are() {
        let r = role(r#"user = "m""#);
        assert_eq!(
            r.effective_url(Some(
                "postgres://o@db/app?options=-c%20search_path%3Dapp&application_name=a+b"
            ))
            .unwrap(),
            "postgres://m@db/app?options=-c%20search_path%3Dapp&application_name=a+b"
        );
    }

    #[test]
    fn a_literal_percent_sign_survives_the_round_trip() {
        let r = role("user = \"m%x\"\npassword = \"secret%40suffix\"");
        let url = r.effective_url(Some("postgres://o@h/app")).unwrap();
        assert_eq!(url, "postgres://m%25x:secret%2540suffix@h/app");
        let parsed = url::Url::parse(&url).unwrap();
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .unwrap()
                .into_owned()
        };
        assert_eq!(decode(parsed.username()), "m%x");
        assert_eq!(decode(parsed.password().unwrap()), "secret%40suffix");
    }

    #[test]
    fn a_password_is_percent_encoded() {
        let r = role("user = \"m\"\npassword = \"p@ss/w:rd\"");
        assert_eq!(
            r.effective_url(Some("postgres://o@h/app")).unwrap(),
            "postgres://m:p%40ss%2Fw%3Ard@h/app"
        );
    }

    #[test]
    fn both_or_neither_of_url_and_user_is_an_error() {
        let both = role("url = \"postgres://a@h/d\"\nuser = \"m\"");
        assert!(
            both.effective_url(Some(PRIMARY))
                .unwrap_err()
                .contains("not both")
        );
        let neither = DatabaseRoleConfig::default();
        assert!(
            neither
                .effective_url(Some(PRIMARY))
                .unwrap_err()
                .contains("`url` or `user`")
        );
    }

    #[test]
    fn user_needs_a_postgres_url() {
        let r = role(r#"user = "m""#);
        assert!(r.effective_url(None).unwrap_err().contains("database.url"));
        assert!(
            r.effective_url(Some("host=db user=o dbname=app"))
                .unwrap_err()
                .contains("postgres://")
        );
        assert!(
            r.effective_url(Some("sqlite://app.db"))
                .unwrap_err()
                .contains("postgres://")
        );
        let sqlite = role(r#"url = "sqlite://app.db""#);
        assert!(sqlite.effective_url(None).unwrap_err().contains("Postgres"));
    }

    #[test]
    fn debug_hides_the_secrets() {
        let r = role("url = \"postgres://m:topsecret@h/app\"\npassword = \"hunter2\"");
        let text = format!("{r:?}");
        assert!(!text.contains("topsecret"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<DatabaseRoleConfig>("usr = \"m\"").is_err());
    }

    #[test]
    fn env_declares_and_overrides_roles() {
        let mut roles = BTreeMap::new();
        roles.insert("core".to_owned(), role(r#"user = "core""#));
        let env = MapEnv(BTreeMap::from([
            ("AUTUMN_DATABASE__ROLES", "marketing, ad-sync"),
            ("AUTUMN_DATABASE__ROLES__CORE__PASSWORD", " core pw "),
            ("AUTUMN_DATABASE__ROLES__MARKETING__USER", "marketing"),
            ("AUTUMN_DATABASE__ROLES__MARKETING__POOL_SIZE", "3"),
            (
                "AUTUMN_DATABASE__ROLES__AD_SYNC__URL",
                "postgres://ad@h/app",
            ),
        ]));
        apply_env_overrides(&mut roles, &env);
        assert_eq!(
            roles.keys().map(String::as_str).collect::<Vec<_>>(),
            ["ad-sync", "core", "marketing"]
        );
        assert_eq!(
            roles["core"].password.as_ref().unwrap().expose_secret(),
            " core pw "
        );
        assert_eq!(roles["marketing"].user.as_deref(), Some("marketing"));
        assert_eq!(roles["marketing"].pool_size, Some(3));
        assert_eq!(roles["ad-sync"].url.as_deref(), Some("postgres://ad@h/app"));
    }

    #[test]
    fn validate_checks_names_and_urls() {
        let mut roles = BTreeMap::new();
        roles.insert("marketing".to_owned(), role(r#"user = "m""#));
        assert!(validate(&roles, Some(PRIMARY)).is_ok());

        roles.insert("Bad Name".to_owned(), role(r#"user = "m""#));
        let err = validate(&roles, Some(PRIMARY)).unwrap_err();
        assert!(err.contains("database.roles.Bad Name"), "{err}");
        roles.remove("Bad Name");

        roles.insert("billing".to_owned(), DatabaseRoleConfig::default());
        let err = validate(&roles, Some(PRIMARY)).unwrap_err();
        assert!(err.contains("database.roles.billing"), "{err}");
        roles.remove("billing");

        roles.insert("zero".to_owned(), role("user = \"z\"\npool_size = 0"));
        let err = validate(&roles, Some(PRIMARY)).unwrap_err();
        assert!(err.contains("pool_size"), "{err}");
    }

    #[test]
    fn max_connections_adds_every_role_pool() {
        let mut database = crate::config::DatabaseConfig {
            pool_size: 10,
            ..Default::default()
        };
        assert_eq!(max_connections(&database), 0);
        database.roles.insert("a".into(), role("user = \"a\""));
        database
            .roles
            .insert("b".into(), role("user = \"b\"\npool_size = 3"));
        assert_eq!(max_connections(&database), 13);
    }

    #[test]
    fn database_config_reads_the_roles_table() {
        let config: crate::config::AutumnConfig = toml::from_str(
            "[database]\nurl = \"postgres://o@h/app\"\n\n[database.roles.marketing]\nuser = \"marketing\"\n",
        )
        .unwrap();
        assert_eq!(
            config.database.roles["marketing"].user.as_deref(),
            Some("marketing")
        );
        assert!(config.database.validate().is_ok());
        let bad: crate::config::AutumnConfig = toml::from_str(
            "[database]\nurl = \"postgres://o@h/app\"\n\n[database.roles.marketing]\n",
        )
        .unwrap();
        assert!(bad.database.validate().is_err());
    }

    #[test]
    fn two_names_with_one_env_name_are_an_error() {
        let mut roles = BTreeMap::new();
        roles.insert("ad-sync".to_owned(), role(r#"user = "a""#));
        roles.insert("ad_sync".to_owned(), role(r#"user = "b""#));
        let err = validate(&roles, Some(PRIMARY)).unwrap_err();
        assert!(err.contains("AUTUMN_DATABASE__ROLES__AD_SYNC__"), "{err}");
    }

    #[test]
    fn validate_does_not_print_the_password() {
        let mut roles = BTreeMap::new();
        roles.insert("m".to_owned(), role("url = \"mysql://m:topsecret@h/app\""));
        let err = validate(&roles, None).unwrap_err();
        assert!(!err.contains("topsecret"), "{err}");
    }

    #[cfg(all(feature = "db", not(feature = "sqlite")))]
    mod pools {
        use super::*;
        use crate::state::AppState;

        struct Marketing;
        impl Boundary for Marketing {
            const NAME: &'static str = "marketing";
        }

        struct Billing;
        impl Boundary for Billing {
            const NAME: &'static str = "billing";
        }

        fn database() -> crate::config::DatabaseConfig {
            let mut database = crate::config::DatabaseConfig {
                url: Some(crate::test_urls::unreachable("db-roles")),
                pool_size: 7,
                connect_timeout_secs: 1,
                ..Default::default()
            };
            database.roles.insert(
                "marketing".into(),
                role("user = \"marketing\"\npool_size = 3"),
            );
            database
        }

        fn size(pool: Option<crate::db::Pool<crate::db::RuntimeConnection>>) -> Option<usize> {
            pool.map(|p| p.status().max_size)
        }

        #[test]
        fn with_no_roles_every_boundary_gets_the_primary_pool() {
            let primary = crate::db::create_pool(&database()).unwrap().unwrap();
            let state = AppState::for_test().with_pool(primary);
            assert_eq!(size(state.pool_for::<Marketing>()), Some(7));
            assert_eq!(size(state.pool_for::<Billing>()), Some(7));
        }

        #[test]
        fn a_role_pool_serves_its_boundary_only() {
            let database = database();
            let primary = crate::db::create_pool(&database).unwrap().unwrap();
            let state = AppState::for_test().with_pool(primary);
            let pools = crate::db::create_role_pools(&database).unwrap();
            assert_eq!(pools.names().collect::<Vec<_>>(), ["marketing"]);
            state.insert_extension(pools);
            assert_eq!(size(state.pool_for::<Marketing>()), Some(3));
            // No fallback to the owner pool.
            assert_eq!(size(state.pool_for::<Billing>()), None);
            assert_eq!(size(state.pool_for::<Billing>()), None);
        }

        #[test]
        fn a_url_role_needs_no_primary_url() {
            let mut database = crate::config::DatabaseConfig {
                connect_timeout_secs: 1,
                ..Default::default()
            };
            database.roles.insert(
                "marketing".into(),
                role(&format!(
                    "url = \"{}\"\npool_size = 3",
                    crate::test_urls::unreachable("db-roles-url")
                )),
            );
            database.validate().unwrap();
            let state = AppState::for_test();
            state.insert_extension(crate::db::create_role_pools(&database).unwrap());
            assert_eq!(size(state.pool_for::<Marketing>()), Some(3));
        }

        #[test]
        fn role_pools_are_reported_to_the_extractor() {
            use crate::db::DbState as _;
            let state = AppState::for_test();
            assert!(!state.boundary_roles_configured());
            state.insert_extension(crate::db::create_role_pools(&database()).unwrap());
            assert!(state.boundary_roles_configured());
        }

        #[test]
        fn a_bad_role_url_is_a_pool_error() {
            let mut database = database();
            database
                .roles
                .insert("billing".into(), DatabaseRoleConfig::default());
            let err = crate::db::create_role_pools(&database).unwrap_err();
            assert!(
                err.to_string().starts_with("database.roles.billing:"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn boundary_db_refuses_a_boundary_with_no_role() {
            use axum::extract::FromRequestParts as _;
            let database = database();
            let primary = crate::db::create_pool(&database).unwrap().unwrap();
            let state = AppState::for_test().with_pool(primary);
            state.insert_extension(crate::db::create_role_pools(&database).unwrap());
            let (mut parts, ()) = axum::http::Request::builder()
                .body(())
                .unwrap()
                .into_parts();
            let Err(err) =
                crate::db::BoundaryDb::<Billing>::from_request_parts(&mut parts, &state).await
            else {
                panic!("a boundary with no role must not get a connection");
            };
            assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
            assert!(err.to_string().contains("`billing`"), "{err}");
        }
    }
}

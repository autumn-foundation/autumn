//! The per-release registry of machine-applyable app-code migrations
//! (issue #1629).
//!
//! This is the app-code sibling of `autumn_web::config::DEPRECATED_CONFIG_KEYS`:
//! a static, reviewable table describing what each breaking release did to the
//! *user's own Rust source*, so `autumn upgrade` can apply the mechanical part
//! instead of every maintainer hand-editing call sites out of a prose guide.
//!
//! # Adding an entry
//!
//! One [`AppMigration`] per documented breaking change, including the ones no
//! codemod can touch — a [`Rewrite::GuideOnly`] entry is what makes the upgrade
//! summary *link* the guide section rather than staying silent about a break
//! the user still has to handle. `scripts/check-migration-guides.sh` reads the
//! [`id`](AppMigration::id) strings out of this file, so the guide's
//! `**Automation:**` label and the shipped codemod cannot drift apart.

/// How much trust a migration's rewrite earns, mirroring the labels the
/// migration guides carry (issue #1629).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Safe by construction — a rename or import move. Rewritten sites are
    /// reported as a count plus the diff.
    Auto,
    /// Rewritten, but each site is listed individually in the summary for a
    /// human to read before committing.
    Review,
    /// Never rewritten. The summary links the exact guide section.
    Manual,
}

impl Confidence {
    /// The lowercase label used in guides, `--json` output, and the gate.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Review => "review",
            Self::Manual => "manual",
        }
    }
}

/// How the renamed function is called — the property that tells the framework
/// API apart from every same-named API that is *not* being renamed.
///
/// This matters because names collide inside Autumn itself: 0.6.0 renamed the
/// generated repository constructor `with_pool`, while `AppState::with_pool`
/// and `AuthzContext::with_pool` are current, unrenamed builder methods. The
/// two are distinguishable without type inference, because a constructor takes
/// no `self` and a builder does: `Repo::with_pool(pool)` can only be the
/// former and `state.with_pool(pool)` can only be the latter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallForm {
    /// `Type::name(args)` — an associated function, no `self` parameter. Only
    /// the `::` call form is rewritten.
    AssociatedFunction,
    /// `value.name(args)` — a method taking `self`. Only the `.` call form is
    /// rewritten.
    Method,
}

/// The naming shape the framework gives the type a renamed function lives on.
///
/// `#[repository]` emits its concrete type as `Pg{trait}`, and every trait the
/// scaffold generates is `{Model}Repository`, so a generated repository is
/// `PgPostRepository`, `PgSubredditRepository`, and so on. Matching the whole
/// shape rather than just the `Pg` prefix keeps an app's own `PgCache` out of
/// the rewrite.
///
/// A hand-named trait that breaks the convention (`PostStore` -> `PgPostStore`)
/// does not match and is *reported* under `manual` — a site to apply by hand,
/// never a silent miss and never a wrong edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverShape {
    pub prefix: &'static str,
    pub suffix: &'static str,
}

impl ReceiverShape {
    /// Whether `ident` is a name the framework would have generated.
    #[must_use]
    pub fn matches(&self, ident: &str) -> bool {
        ident.len() >= self.prefix.len() + self.suffix.len()
            && ident.starts_with(self.prefix)
            && ident.ends_with(self.suffix)
    }
}

/// What a migration mechanically does to app source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewrite {
    /// Rename a function *at its call sites*. Only the name changes, so the
    /// replacement never spans a line break.
    ///
    /// A site matches only when the call *form* and the exact top-level
    /// argument count both agree with the renamed function's real signature —
    /// a rename changes neither, and insisting on both is what keeps a
    /// same-named API on another type from being rewritten.
    CallRename {
        from: &'static str,
        to: &'static str,
        form: CallForm,
        /// Exact number of top-level arguments the function takes.
        args: usize,
        /// Naming shape the receiver must have — see [`ReceiverShape`].
        receiver: Option<ReceiverShape>,
    },
    /// Nothing is rewritten; the change is reported with its guide link so the
    /// user still sees it in the upgrade summary.
    GuideOnly,
}

/// One release's machine-applyable (or explicitly guide-only) app-code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppMigration {
    /// Stable identifier, `<version>-<slug>`. Named in the guide's
    /// `**Automation:**` label and in `--json` output; never reused.
    pub id: &'static str,
    /// The release that introduced the break. A migration applies when the
    /// app's recorded version is *older* than this and the target is at least
    /// this.
    pub version: &'static str,
    /// One-line description, shown in the upgrade summary.
    pub title: &'static str,
    /// Confidence label — see [`Confidence`].
    pub confidence: Confidence,
    /// Repo-relative path plus anchor of the guide section documenting it.
    pub guide: &'static str,
    /// The mechanical rewrite, if any.
    pub rewrite: Rewrite,
}

/// The canonical registry of app-code migrations, oldest release first.
pub static APP_MIGRATIONS: &[AppMigration] = &[
    // 0.4.0 and 0.5.0 ship no codemod — every break in those releases is
    // semantic or configuration-level. They are registered anyway: without an
    // entry, an app upgrading 0.3.x -> 0.6.0 would be told about the two 0.6.0
    // changes and nothing else, which reads as "those are the only breaks in
    // range". A `GuideOnly` entry is how the summary names the change and links
    // its guide section. `MIGRATION_GUIDE_FLOOR` in
    // `scripts/check-migration-guides.sh` is 0.4.0, so that is where this
    // starts too.
    AppMigration {
        id: "0.4.0-security-production-signing-secret",
        version: "0.4.0",
        title: "a production signing secret is required",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.4.0.md#security-production-signing-secret-is-required",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.4.0-storage-s3-plugin-crate",
        version: "0.4.0",
        title: "`storage-s3` moved to the `autumn-storage-s3` plugin crate",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.4.0.md#storage-storage-s3-moved-to-a-plugin-crate",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.4.0-authorization-policy-required",
        version: "0.4.0",
        title: "repository APIs require a policy in production",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.4.0.md#authorization-repository-apis-require-a-policy-in-production",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.4.0-mail-deliver-later-durable",
        version: "0.4.0",
        title: "production `deliver_later` must be durable or acknowledged",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.4.0.md#mail-production-deliver_later-must-be-durable-or-acknowledged",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.5.0-security-forwarded-header-trust",
        version: "0.5.0",
        title: "forwarded-header trust is declared once, centrally",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.5.0.md#security-forwarded-header-trust-is-now-declared-once",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.5.0-deprecated-configuration-keys",
        version: "0.5.0",
        title: "deprecated configuration keys",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.5.0.md#deprecated-configuration-keys",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.5.0-client-identity-extractors",
        version: "0.5.0",
        title: "read client identity through `ClientAddr` / `ClientHost` / `ClientScheme`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.5.0.md#reading-client-identity-clientaddr-clienthost-clientscheme",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.6.0-tenancy-jwt-secret-secretstring",
        version: "0.6.0",
        title: "`TenancyConfig::jwt_secret` is now a `secrecy::SecretString`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.6.0.md#security-tenancyconfigjwt_secret-is-now-a-secrecysecretstring",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.6.0-repository-with-pool-untracked",
        version: "0.6.0",
        title: "repository constructor `with_pool` is renamed to `with_pool_untracked`",
        confidence: Confidence::Auto,
        guide: "docs/migrations/0.6.0.md#repository-with_pool-is-renamed-to-with_pool_untracked",
        rewrite: Rewrite::CallRename {
            from: "with_pool",
            to: "with_pool_untracked",
            // `pub fn with_pool_untracked(pool) -> Self` — an associated
            // function taking exactly the pool. `AppState::with_pool(pool)`
            // and `AuthzContext::with_pool(pool)` are builder *methods* that
            // still carry the old name and must not be touched.
            form: CallForm::AssociatedFunction,
            args: 1,
            receiver: Some(ReceiverShape {
                prefix: "Pg",
                suffix: "Repository",
            }),
        },
    },
];

/// The full registry.
pub fn app_migrations() -> &'static [AppMigration] {
    APP_MIGRATIONS
}

/// A `major.minor.patch` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Whether this is a pre-release of the triple (`0.7.0-rc.1`).
    ///
    /// A flag rather than the identifier, because the only comparison that has
    /// to be right is pre-release against *stable*: every registry version is a
    /// released one, so `rc.1` vs `rc.2` never decides which migrations apply.
    /// Two pre-releases of the same triple therefore compare equal, and when
    /// they are two workspace members racing to set the floor either answer
    /// selects the same migrations.
    pub prerelease: bool,
}

impl Version {
    /// A released version.
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: false,
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            // `0.7.0-rc.1` precedes `0.7.0`, so an app on the candidate is
            // *behind* the release and still needs its migrations. Derived
            // ordering would have put `false` first and inverted this.
            .then_with(|| other.prerelease.cmp(&self.prerelease))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.prerelease {
            f.write_str("-pre")?;
        }
        Ok(())
    }
}

/// Parse a Cargo dependency requirement into the *lowest* version it admits.
///
/// `"0.5"`, `"^0.5.0"` and `"=0.5.0"` all floor at `0.5.0` — the question this
/// answers is "what is the app definitely already past?", and the floor is the
/// only honest answer to that. A requirement with no single floor (`"*"`, a
/// multi-comparator range, a git branch name) returns `None` so the caller can
/// ask for `--from` instead of rewriting against a guessed release.
pub fn parse_version_req(req: &str) -> Option<Version> {
    /// A `*`/`x`/`X` segment: Cargo's wildcard requirement, floored at zero.
    fn is_wildcard(part: &str) -> bool {
        matches!(part, "*" | "x" | "X")
    }

    let req = req.trim();
    // A comma is Cargo's comparator separator: `>=0.4, <0.6` pins a range whose
    // floor says nothing about which migrations the app has already taken.
    if req.is_empty() || req.contains(',') {
        return None;
    }
    // An upper bound is not a floor. `autumn-web = "<0.6"` says nothing about
    // which release the app is on — stripping the `<` would read it as 0.6.0
    // and skip the very 0.6.0 codemod a 0.5.x app needs. Ask for `--from`.
    if req.starts_with('<') {
        return None;
    }
    // Build metadata is not part of the version; a pre-release suffix is, and
    // it sorts *below* the release, so it is remembered rather than dropped.
    let without_build = req
        .trim_start_matches(['=', '^', '~', '>'])
        .trim()
        .split('+')
        .next()?;
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, false), |(core, _)| (core, true));
    let core = core.trim();
    if core.is_empty() {
        return None;
    }
    let mut parts = core.split('.');
    // An omitted segment is Cargo's "any value here", whose floor is zero, and
    // an explicit wildcard segment says the same thing: `0.5.*` and `0.5` pin
    // the same floor. Only the *major* wildcard is refused — a bare `*` accepts
    // every release, so reading it as 0.0.0 would silently select every
    // migration ever written instead of asking for `--from`.
    let first = parts.next()?;
    if is_wildcard(first) {
        return None;
    }
    let major = first.parse::<u64>().ok()?;
    let mut wildcarded = false;
    let mut segment = |part: Option<&str>| -> Option<u64> {
        match part {
            None => Some(0),
            Some(part) if is_wildcard(part) => {
                wildcarded = true;
                Some(0)
            }
            // `1.*.3` is not a requirement Cargo accepts, and reading the `3`
            // after a wildcard would invent precision the user did not write.
            Some(_) if wildcarded => None,
            Some(part) => part.parse::<u64>().ok(),
        }
    };
    let minor = segment(parts.next())?;
    let patch = segment(parts.next())?;
    // A fourth segment is not semver; refuse rather than silently ignore it.
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        prerelease,
        ..Version::new(major, minor, patch)
    })
}

/// Where the migration guides live, for a reader who is not standing in the
/// Autumn repository.
///
/// The registry stores repo-relative paths because that is what the release
/// gate checks against the tree. Printed verbatim by an installed CLI running
/// in someone's app, `docs/migrations/0.6.0.md` names a file in *their*
/// project — a dead end at exactly the point the report says to go read
/// something. The human report renders this prefix instead.
///
/// Pinned to the default branch rather than to each release's tag: guides are
/// append-only historical documents, the branch always resolves, and a
/// migration registered for a release that has not been tagged yet — the case
/// `docs/migrations/next.md` exists for — would otherwise link to a 404.
pub const GUIDE_BASE_URL: &str = "https://github.com/autumn-foundation/autumn/blob/trunk-dev/";

/// A repo-relative guide location as a URL anyone can open.
#[must_use]
pub fn guide_url(guide: &str) -> String {
    format!("{GUIDE_BASE_URL}{guide}")
}

/// The migrations that apply when upgrading `from` → `to`.
///
/// A migration applies when its release is newer than what the app records and
/// no newer than the target: `from < version <= to`. An app already on the
/// release that broke the API has, by definition, already dealt with the break.
/// Registry order is preserved, so the result is oldest release first.
pub fn migrations_between(from: Version, to: Version) -> Vec<&'static AppMigration> {
    select_between(APP_MIGRATIONS, from, to)
}

/// [`migrations_between`] over an explicit registry, so the selection rule is
/// testable against a table with more than one release in it.
pub fn select_between(
    registry: &'static [AppMigration],
    from: Version,
    to: Version,
) -> Vec<&'static AppMigration> {
    registry
        .iter()
        .filter(|migration| {
            parse_version_req(migration.version)
                .is_some_and(|version| version > from && version <= to)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version::new(major, minor, patch)
    }

    /// A pre-release of the triple, e.g. `0.7.0-rc.1`.
    fn pre(major: u64, minor: u64, patch: u64) -> Version {
        Version {
            prerelease: true,
            ..Version::new(major, minor, patch)
        }
    }

    #[test]
    fn parses_bare_and_operator_prefixed_requirements() {
        assert_eq!(parse_version_req("0.6.0"), Some(v(0, 6, 0)));
        assert_eq!(parse_version_req("=0.5.0"), Some(v(0, 5, 0)));
        assert_eq!(parse_version_req("^0.5.1"), Some(v(0, 5, 1)));
        assert_eq!(parse_version_req(">=0.4"), Some(v(0, 4, 0)));
        assert_eq!(parse_version_req("~0.5"), Some(v(0, 5, 0)));
        assert_eq!(parse_version_req("  0.6.0  "), Some(v(0, 6, 0)));
    }

    #[test]
    fn parses_partial_requirements_as_their_lowest_admitted_version() {
        // `autumn-web = "0.5"` admits 0.5.0 and up; the app is *at least* on
        // 0.5.0, so a 0.5.0 migration is already applied and must not re-run.
        assert_eq!(parse_version_req("0.5"), Some(v(0, 5, 0)));
        assert_eq!(parse_version_req("1"), Some(v(1, 0, 0)));
    }

    #[test]
    fn ignores_build_metadata_but_keeps_the_prerelease() {
        assert_eq!(parse_version_req("0.7.0+build.5"), Some(v(0, 7, 0)));
        assert_eq!(parse_version_req("0.7.0-rc.1"), Some(pre(0, 7, 0)));
        assert_eq!(parse_version_req("0.7.0-rc.1+build.5"), Some(pre(0, 7, 0)));
    }

    #[test]
    fn a_prerelease_sorts_below_its_release() {
        assert!(pre(0, 7, 0) < v(0, 7, 0), "0.7.0-rc.1 precedes 0.7.0");
        assert!(v(0, 6, 0) < pre(0, 7, 0), "and still follows 0.6.0");
        assert_ne!(pre(0, 7, 0), v(0, 7, 0));
    }

    #[test]
    fn an_app_on_a_release_candidate_still_needs_that_releases_migrations() {
        // Upgrading 0.7.0-rc.1 -> 0.7.0: the candidate is *behind* the release,
        // so the release's migrations apply. Treating the two as equal selected
        // nothing.
        let ids = ids(&select_between(FIXTURE_REGISTRY, pre(0, 7, 0), v(0, 7, 0)));
        assert_eq!(ids, vec!["0.7.0-c"]);
    }

    #[test]
    fn targeting_a_release_candidate_does_not_select_the_release() {
        // `--to 0.7.0-rc.1` must not pull in migrations registered for the
        // finished 0.7.0.
        let ids = ids(&select_between(FIXTURE_REGISTRY, v(0, 6, 0), pre(0, 7, 0)));
        assert!(ids.is_empty(), "0.7.0 is not reached yet, got {ids:?}");
    }

    #[test]
    fn rejects_unparsable_requirements() {
        // A wildcard, a multi-comparator range, and a git/branch style pin
        // carry no single lower bound this command can trust — better to ask
        // for `--from` than to guess and rewrite against the wrong release.
        assert_eq!(parse_version_req("*"), None);
        assert_eq!(parse_version_req(">=0.4, <0.6"), None);
        // An upper bound is not a floor: reading `<0.6` as 0.6.0 would skip
        // the 0.6.0 codemod on an app actually resolved to 0.5.x.
        assert_eq!(parse_version_req("<0.6"), None);
        assert_eq!(parse_version_req("<=0.6.0"), None);
        assert_eq!(parse_version_req(""), None);
        assert_eq!(parse_version_req("trunk-dev"), None);
        // A wildcard *major* is the bare `*` in another spelling.
        assert_eq!(parse_version_req("x"), None);
        assert_eq!(parse_version_req("X.1"), None);
        // Precision after a wildcard is not a requirement Cargo accepts, and
        // honouring the `3` would invent a floor the user did not write.
        assert_eq!(parse_version_req("1.*.3"), None);
    }

    #[test]
    fn a_wildcard_segment_floors_at_zero() {
        // Cargo's wildcard requirements. `0.5.*` and `0.5` are the same
        // requirement, so they have to parse to the same floor — and `0.*` is
        // `0` written out, which this has always accepted as 0.0.0. Only the
        // major position is refused, because `*` alone accepts every release.
        assert_eq!(parse_version_req("0.5.*"), Some(Version::new(0, 5, 0)));
        assert_eq!(parse_version_req("0.5.x"), Some(Version::new(0, 5, 0)));
        assert_eq!(parse_version_req("^1.2.*"), Some(Version::new(1, 2, 0)));
        assert_eq!(parse_version_req("0.*"), parse_version_req("0"));
        assert_eq!(parse_version_req("0.x"), Some(Version::new(0, 0, 0)));
    }

    #[test]
    fn selects_migrations_newer_than_the_recorded_version() {
        let ids: Vec<_> = migrations_between(v(0, 5, 0), v(0, 6, 0))
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(
            ids.contains(&"0.6.0-repository-with-pool-untracked"),
            "0.5.0 -> 0.6.0 must include the 0.6.0 rename, got {ids:?}"
        );
    }

    #[test]
    fn excludes_migrations_the_app_is_already_past() {
        let ids: Vec<_> = migrations_between(v(0, 6, 0), v(0, 7, 0))
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(
            ids.is_empty(),
            "an app already on 0.6.0 needs no 0.6.0 migration, got {ids:?}"
        );
    }

    #[test]
    fn excludes_migrations_newer_than_the_target() {
        let selected = migrations_between(v(0, 4, 0), v(0, 5, 0));
        assert!(
            selected.iter().all(|m| m.version == "0.5.0"),
            "targeting 0.5.0 selects 0.5.0 and nothing newer, got {:?}",
            selected.iter().map(|m| m.id).collect::<Vec<_>>()
        );
        assert!(
            !selected.is_empty(),
            "0.5.0 is ahead of 0.4.0 and its changes must be reported"
        );
    }

    #[test]
    fn selection_is_empty_when_target_is_not_newer() {
        assert!(migrations_between(v(0, 6, 0), v(0, 6, 0)).is_empty());
        assert!(migrations_between(v(0, 6, 0), v(0, 5, 0)).is_empty());
    }

    #[test]
    fn selection_is_ordered_oldest_release_first() {
        let selected = migrations_between(v(0, 0, 0), v(9, 9, 9));
        let mut previous = v(0, 0, 0);
        for migration in &selected {
            let current = parse_version_req(migration.version)
                .unwrap_or_else(|| panic!("registry version {} must parse", migration.version));
            assert!(
                current >= previous,
                "registry must stay sorted oldest-first: {current} after {previous}"
            );
            previous = current;
        }
        assert_eq!(selected.len(), APP_MIGRATIONS.len());
    }

    /// A registry with three releases in it — the shipped one has a single
    /// release, so the multi-release half of the selection rule is otherwise
    /// unreachable.
    static FIXTURE_REGISTRY: &[AppMigration] = &[
        AppMigration {
            id: "0.5.0-a",
            version: "0.5.0",
            title: "a",
            confidence: Confidence::Manual,
            guide: "docs/migrations/0.5.0.md#a",
            rewrite: Rewrite::GuideOnly,
        },
        AppMigration {
            id: "0.6.0-b",
            version: "0.6.0",
            title: "b",
            confidence: Confidence::Auto,
            guide: "docs/migrations/0.6.0.md#b",
            rewrite: Rewrite::CallRename {
                from: "old",
                to: "new",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        },
        AppMigration {
            id: "0.7.0-c",
            version: "0.7.0",
            title: "c",
            confidence: Confidence::Review,
            guide: "docs/migrations/0.7.0.md#c",
            rewrite: Rewrite::CallRename {
                from: "older",
                to: "newer",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: None,
            },
        },
    ];

    fn ids(selected: &[&'static AppMigration]) -> Vec<&'static str> {
        selected.iter().map(|m| m.id).collect()
    }

    #[test]
    fn a_multi_release_range_selects_every_release_it_spans_in_order() {
        assert_eq!(
            ids(&select_between(FIXTURE_REGISTRY, v(0, 4, 0), v(0, 7, 0))),
            vec!["0.5.0-a", "0.6.0-b", "0.7.0-c"],
        );
    }

    #[test]
    fn a_multi_release_range_excludes_both_ends_correctly() {
        // Already on 0.5.0, upgrading to 0.6.0: only 0.6.0 is ahead.
        assert_eq!(
            ids(&select_between(FIXTURE_REGISTRY, v(0, 5, 0), v(0, 6, 0))),
            vec!["0.6.0-b"],
        );
        // Skipping a release still picks up the one in the middle.
        assert_eq!(
            ids(&select_between(FIXTURE_REGISTRY, v(0, 5, 0), v(0, 7, 0))),
            vec!["0.6.0-b", "0.7.0-c"],
        );
        assert!(select_between(FIXTURE_REGISTRY, v(0, 7, 0), v(0, 7, 0)).is_empty());
    }

    #[test]
    fn registry_entries_are_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for migration in app_migrations() {
            assert!(
                seen.insert(migration.id),
                "duplicate migration id {}",
                migration.id
            );
            assert!(
                parse_version_req(migration.version).is_some(),
                "{} has an unparsable version {}",
                migration.id,
                migration.version
            );
            assert!(
                migration.id.starts_with(migration.version),
                "{} must be prefixed with its release version",
                migration.id
            );
            assert!(
                migration.guide.starts_with("docs/migrations/") && migration.guide.contains('#'),
                "{} must link a guide section, got {}",
                migration.id,
                migration.guide
            );
            match migration.rewrite {
                Rewrite::CallRename { from, to, .. } => {
                    assert_ne!(from, to, "{} renames nothing", migration.id);
                    assert_ne!(
                        migration.confidence,
                        Confidence::Manual,
                        "{} rewrites code, so it cannot be labelled manual",
                        migration.id
                    );
                }
                Rewrite::GuideOnly => assert_eq!(
                    migration.confidence,
                    Confidence::Manual,
                    "{} rewrites nothing, so it must be labelled manual",
                    migration.id
                ),
            }
        }
    }

    #[test]
    fn no_two_renames_in_one_release_chain_into_each_other() {
        // `rewrite_source_for_releases` iterates release by release, so a chain
        // that crosses releases resolves in one run. A chain *within* one
        // release would need a second pass that does not happen — so it is
        // forbidden rather than half-supported.
        for migration in app_migrations() {
            let Rewrite::CallRename { to, .. } = migration.rewrite else {
                continue;
            };
            for other in app_migrations() {
                let Rewrite::CallRename { from, .. } = other.rewrite else {
                    continue;
                };
                assert!(
                    from != to || migration.version != other.version,
                    "{} renames to `{to}`, which {} renames again in the same release",
                    migration.id,
                    other.id
                );
            }
        }
    }

    #[test]
    fn the_first_shipped_codemod_is_the_with_pool_rename() {
        let migration = app_migrations()
            .iter()
            .find(|m| m.id == "0.6.0-repository-with-pool-untracked")
            .expect("the with_pool rename is the first shipped codemod (issue #1629)");
        assert_eq!(migration.confidence, Confidence::Auto);
        assert_eq!(
            migration.rewrite,
            Rewrite::CallRename {
                from: "with_pool",
                to: "with_pool_untracked",
                form: CallForm::AssociatedFunction,
                args: 1,
                receiver: Some(ReceiverShape {
                    prefix: "Pg",
                    suffix: "Repository",
                }),
            }
        );
    }

    #[test]
    fn the_receiver_shape_matches_what_the_repository_macro_emits() {
        let shape = ReceiverShape {
            prefix: "Pg",
            suffix: "Repository",
        };
        // `#[repository]` on `trait PostRepository` emits `PgPostRepository`.
        assert!(shape.matches("PgPostRepository"));
        assert!(shape.matches("PgSubredditRepository"));
        assert!(shape.matches("PgRepository"), "a trait named `Repository`");
        // An app type that merely starts with `Pg`, or merely ends in
        // `Repository`, is not something the macro generated.
        assert!(!shape.matches("PgCache"));
        assert!(!shape.matches("PgConnectionPool"));
        assert!(!shape.matches("PostRepository"), "that is the trait");
        assert!(!shape.matches("Cache"));
        assert!(!shape.matches("Pg"));
    }

    #[test]
    fn confidence_labels_match_the_guide_vocabulary() {
        assert_eq!(Confidence::Auto.label(), "auto");
        assert_eq!(Confidence::Review.label(), "review");
        assert_eq!(Confidence::Manual.label(), "manual");
    }

    #[test]
    fn versions_order_by_major_then_minor_then_patch() {
        assert!(v(0, 5, 9) < v(0, 6, 0));
        assert!(v(1, 0, 0) > v(0, 9, 9));
        assert_eq!(v(0, 6, 1), v(0, 6, 1));
    }
}

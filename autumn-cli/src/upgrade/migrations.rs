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
    // No shipped migration earns this label yet (#2234); only tests construct it.
    #[cfg_attr(not(test), allow(dead_code))]
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
    // 0.4.0 and 0.5.0 ship no codemod. `GuideOnly` entries make the summary name and link every
    // break in range, not only the ones with a rewrite. This starts at 0.4.0 to match
    // `MIGRATION_GUIDE_FLOOR` in `scripts/check-migration-guides.sh`.
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
        // `review`, not `auto` (#2234): the receiver check is textual, not name resolution, so
        // every rewritten site is also listed for a human to read.
        confidence: Confidence::Review,
        guide: "docs/migrations/0.6.0.md#repository-with_pool-is-renamed-to-with_pool_untracked",
        rewrite: Rewrite::CallRename {
            from: "with_pool",
            to: "with_pool_untracked",
            // An associated function taking only the pool. `AppState::with_pool(pool)` and
            // `AuthzContext::with_pool(pool)` are builder methods that keep the old name.
            form: CallForm::AssociatedFunction,
            args: 1,
            receiver: Some(ReceiverShape {
                prefix: "Pg",
                suffix: "Repository",
            }),
        },
    },
    // 0.7.0 ships no codemod: each break needs a value, a client wire change or a new impl
    // shape. `GuideOnly` entries make the summary name and link them.
    AppMigration {
        id: "0.7.0-routing-route-seo-field",
        version: "0.7.0",
        title: "`Route` and `StaticRouteMeta` gained an `seo` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.7.0.md#routing-route-and-staticroutemeta-gained-an-seo-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.7.0-api-lock-version-required",
        version: "0.7.0",
        title: "a `lock_version` model requires the version on JSON `PUT`/`PATCH`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.7.0.md#json-api-a-lock_version-model-now-requires-the-version-on-putpatch",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.7.0-app-layers-erased",
        version: "0.7.0",
        title: "app-wide layers are registered against the erased ingress service",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.7.0.md#app-wide-layers-are-now-erased",
        rewrite: Rewrite::GuideOnly,
    },
    // Invisible in Rust: `auto_migrate_in_production = true` under a custom profile did nothing
    // on 0.6.x and applies migrations at startup on 0.7.0.
    AppMigration {
        id: "0.7.0-migrate-profile-agnostic",
        version: "0.7.0",
        title: "`auto_migrate_in_production` is honoured on any non-`dev` profile",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.7.0.md#startup-migrations-are-now-profile-agnostic-1903",
        rewrite: Rewrite::GuideOnly,
    },
    // Still compiles: `Query<T>` can now return 400s, no longer resolves map duplicates
    // last-value-wins, and sorts `Vec<(String, String)>` by key. Nothing in the build warns.
    AppMigration {
        id: "0.7.0-query-decoding",
        version: "0.7.0",
        title: "`Query<T>` decodes structure, rejects duplicate scalar keys, and sorts pair vectors",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.7.0.md#queryt-decoding-1972",
        rewrite: Rewrite::GuideOnly,
    },
    // 0.8.0 ships no codemod: no break below (one per section of the guide's *Breaking changes*)
    // is rename-level, so each is `GuideOnly`.
    AppMigration {
        id: "0.8.0-http-client-error-non-exhaustive",
        version: "0.8.0",
        title: "`http_client::ClientError` is `#[non_exhaustive]` and gains `SimNetwork`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#http_client-clienterror-is-non_exhaustive-and-gains-simnetwork-2967",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-consent-banner-csrf-cookie-option",
        version: "0.8.0",
        title: "`inject_consent_banner` takes `csrf_cookie_name: Option<&str>`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#consent-inject_consent_banner-takes-csrf_cookie_name-optionstr",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-sim-types-private",
        version: "0.8.0",
        title: "`sim::SimClock` and `sim::SimApp` are no longer public",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#sim-simclock-and-simapp-are-no-longer-public-2967",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-macros-split",
        version: "0.8.0",
        title: "`autumn-macros` no longer holds `model`, `repository` and `service`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#macros-autumn-macros-no-longer-holds-the-database-macros-2809",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-widget-classes-prefixed",
        version: "0.8.0",
        title: "built-in widget CSS classes are now `autumn-`-prefixed",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#widgets-widget-css-classes-are-now-autumn--prefixed-2354",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-commentable-author-name-checked",
        version: "0.8.0",
        title: "`#[commentable(by = ...)]` checks `author_name` against the author model",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#commentable-author_name-is-checked-against-the-author-model-2288",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-authorize-aliased-compile-error",
        version: "0.8.0",
        title: "aliased and ambiguous `#[authorize]` shapes are a compile error",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#authorize-aliased-authorize-and-ambiguous-attribute-shapes-are-now-a-compile-error",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-static-get-feature-flag-compile-error",
        version: "0.8.0",
        title: "`#[feature_flag]` on `static_get` is a compile error",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#static_get-feature_flag-is-now-a-compile-error",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-repository-owner-requires-policy",
        version: "0.8.0",
        title: "`owner = column` beside `api = \"...\"` requires `policy`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#repository-owner--column-next-to-api---now-requires-policy",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-lifecycle-unsound-graph",
        version: "0.8.0",
        title: "an unsound `#[lifecycle]` graph is a compile error",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#lifecycle-an-unsound-lifecycle-graph-is-now-a-compile-error",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-openapi-schema-skip-serializing-if",
        version: "0.8.0",
        title: "`#[serde(skip_serializing_if)]` on an `OpenApiSchema` type needs `#[serde(default)]`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#openapischema-serdeskip_serializing_if-now-needs-serdedefault",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-audit-event-metadata",
        version: "0.8.0",
        title: "`AuditEvent` gains a `metadata` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#audit-auditevent-gains-a-metadata-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-openapi-parameter-description",
        version: "0.8.0",
        title: "`openapi::Parameter` gains a `description` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#openapi-parameter-gains-a-description-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-autumn-billing-customer-tenant-scoped",
        version: "0.8.0",
        title: "`autumn-billing`'s `Customer.user_id` is tenant-scoped under tenancy",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#autumn-billing-customeruser_id-is-tenant-scoped-under-tenancy",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-tls-mtls-members",
        version: "0.8.0",
        title: "`TlsConfig`, `TlsError` and `SecurityDump` gain mTLS members",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#tls-tlsconfig-tlserror-and-securitydump-gain-mtls-members-1640",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-ssg-manifest-non-exhaustive",
        version: "0.8.0",
        title: "`ManifestEntry` / `StaticManifest` are `#[non_exhaustive]`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#ssg-manifestentry--staticmanifest-are-non_exhaustive-and-generated-pages-carry-their-declared-content-type",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-capsule-execute-replay-fixtures",
        version: "0.8.0",
        title: "`capsule::execute` takes `ReplayFixtures`; the capsule format is version 3",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#failure-capsules-capsuleexecute-takes-replayfixtures-and-the-capsule-format-is-version-3",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-config-replication-field",
        version: "0.8.0",
        title: "`AutumnConfig` gains a `replication` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#config-autumnconfig-gains-a-replication-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-config-job-sqlite-field",
        version: "0.8.0",
        title: "`JobConfig` gains a `sqlite` field and `SchedulerBackend` a `Sqlite` variant",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#config-jobconfig-gains-a-sqlite-field-and-schedulerbackend-gains-a-sqlite-variant",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-media-roomstore-heartbeat",
        version: "0.8.0",
        title: "`RoomStore` gains a required `heartbeat` method",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#media-rooms-roomstore-gains-a-required-heartbeat-method",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-media-rooms-require-session",
        version: "0.8.0",
        title: "media room create and join require an authenticated session",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#media-rooms-create-and-join-require-an-authenticated-session",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-admin-experiment-changed-at",
        version: "0.8.0",
        title: "`ExperimentChange::changed_at` is now `NaiveDateTime`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#admin-plugin-experimentchangechanged_at-is-now-naivedatetime",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-capacity-contract-metadata-fields",
        version: "0.8.0",
        title: "three capacity-contract metadata structs gain fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#capacity-contracts-three-metadata-structs-gain-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-job-admin-record-blocked-field",
        version: "0.8.0",
        title: "`JobAdminRecord` gains a `blocked_on_concurrency` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#jobs-jobadminrecord-gains-a-blocked_on_concurrency-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-scrub-refuses-delete-trigger",
        version: "0.8.0",
        title: "`autumn db scrub` refuses a `DELETE` trigger or rule on a table it empties",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#scrub-a-delete-trigger-or-rule-on-a-table-autumn-db-scrub-empties-is-refused",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-acme-renewal-dns-recovery",
        version: "0.8.0",
        title: "`AcmeRenewalTask` gains `dns` and `recovery`; `AcmeConfig` gains `dns`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#acme-acmerenewaltask-gains-dns-and-recovery-acmeconfig-gains-dns",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-plugin-conformance-config-non-exhaustive",
        version: "0.8.0",
        title: "`plugin_conformance::ConformanceConfig` is `#[non_exhaustive]`; build it with its constructors",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#plugin-conformance-conformanceconfig-is-non_exhaustive",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.8.0-validator-0-21",
        version: "0.8.0",
        title: "`#[derive(Validate)]` needs `validator = \"0.21\"` in the app's own manifest",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.8.0.md#dependencies-validator-moves-to-021",
        rewrite: Rewrite::GuideOnly,
    },
    // 0.9.0 ships no codemod: no break below (one per section of the guide's *Breaking changes*)
    // is rename-level, so each is `GuideOnly`.
    AppMigration {
        id: "0.9.0-commentable-soft-delete-argument",
        version: "0.9.0",
        title: "Commentable: the runtime helpers take `soft_delete: Option<bool>`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#commentable-the-runtime-helpers-take-soft_delete-optionbool",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-notifications-push-tenant-id",
        version: "0.9.0",
        title: "Notifications and push: tables get a `tenant_id` column",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#notifications-and-push-tables-get-a-tenant_id-column",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-db-feature-postgres-split",
        version: "0.9.0",
        title: "Features: `db` has no Postgres client; enable `postgres`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#features-db-has-no-postgres-client-enable-postgres",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-job-tracking-retry-token",
        version: "0.9.0",
        title: "Jobs: `JobTrackingStore::reset_for_retry` takes a `RetryToken`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#jobs-jobtrackingstorereset_for_retry-takes-a-retrytoken",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-job-info-timeout",
        version: "0.9.0",
        title: "Jobs: `JobInfo` has a new `timeout` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#jobs-jobinfo-has-a-new-timeout-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-schema-foreign-key-actions",
        version: "0.9.0",
        title: "Schema: `ForeignKey` has `on_delete` and `on_update` fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#schema-foreignkey-has-on_delete-and-on_update-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-config-cost-field",
        version: "0.9.0",
        title: "Config: `AutumnConfig` gains a `cost` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#config-autumnconfig-gains-a-cost-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-route-info-layer-guarded",
        version: "0.9.0",
        title: "Routes: `ScopedGroup` and `RouteInfo` gain a `layer_guarded` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#routes-scopedgroup-and-routeinfo-gain-a-layer_guarded-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-config-fault-injection-field",
        version: "0.9.0",
        title: "Config: `AutumnConfig` gains a `fault_injection` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#config-autumnconfig-gains-a-fault_injection-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-admission-criticality-fields",
        version: "0.9.0",
        title: "Admission: `Route`, `ServerConfig` and `HttpClientConfig` have new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#admission-route-serverconfig-and-httpclientconfig-have-new-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-http-client-retries-idempotent-only",
        version: "0.9.0",
        title: "HTTP client: `retries(n)` no longer retries `POST` and `PATCH`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#http-client-retriesn-no-longer-retries-post-and-patch",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-scheduler-ticks-table",
        version: "0.9.0",
        title: "Scheduler: the Postgres backend needs the `autumn_scheduler_ticks` table",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#scheduler-the-postgres-backend-needs-the-autumn_scheduler_ticks-table",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-idempotency-store-async",
        version: "0.9.0",
        title: "Idempotency: `IdempotencyStore` is async",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#idempotency-idempotencystore-is-async",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-idempotency-in-flight-ttl-60s",
        version: "0.9.0",
        title: "Idempotency: the in-flight lock expires after 60 s",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#idempotency-the-in-flight-lock-expires-after-60-s",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-metrics-request-duration-histogram",
        version: "0.9.0",
        title: "Metrics: `autumn_http_request_duration_seconds` is now a histogram",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#metrics-autumn_http_request_duration_seconds-is-now-a-histogram",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-telemetry-sample-ratio",
        version: "0.9.0",
        title: "Telemetry: `TelemetryConfig` and `OtlpTraceRuntime` have a new field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#telemetry-telemetryconfig-and-otlptraceruntime-have-a-new-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-ws-autumn-types",
        version: "0.9.0",
        title: "WebSockets: `ws::WebSocket` and `ws::WebSocketUpgrade` are Autumn types",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#websockets-wswebsocket-and-wswebsocketupgrade-are-autumn-types",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-query-budget-associated-fn",
        version: "0.9.0",
        title: "Query budgets: an associated function handed the handle is reported",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#query-budgets-an-associated-function-handed-the-handle-is-reported-2316",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-query-budget-container-method",
        version: "0.9.0",
        title: "Query budgets: a method that borrows a container of handles is reported",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#query-budgets-a-method-that-borrows-a-container-of-handles-is-reported-2316",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-prod-strict-config",
        version: "0.9.0",
        title: "Config: the `prod` profile enables `strict_config` and new protections",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#config-the-prod-profile-enables-strict_config-and-new-protections",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-admission-limit-non-exhaustive",
        version: "0.9.0",
        title: "Capacity: `AdmissionLimit` is `#[non_exhaustive]` and has a new variant",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#capacity-admissionlimit-is-non_exhaustive-and-has-a-new-variant",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-health-config-fields",
        version: "0.9.0",
        title: "Config: `HealthConfig` gains four public fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#config-healthconfig-gains-four-public-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-tenancy-job-config-fields",
        version: "0.9.0",
        title: "Config: `TenancyConfig`, `JobConfig` and `JobPostgresConfig` have new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#config-tenancyconfig-jobconfig-and-jobpostgresconfig-have-new-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-tenant-propagating-body-bulkhead",
        version: "0.9.0",
        title: "Tenancy: `TenantPropagatingBody` has a `db_bulkhead` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#tenancy-tenantpropagatingbody-has-a-db_bulkhead-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-ready-pings-primary",
        version: "0.9.0",
        title: "Probes: `/ready` pings the primary database",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#probes-ready-pings-the-primary-database",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-circuit-breaker-slow-call-fields",
        version: "0.9.0",
        title: "Resilience: `CircuitBreakerPolicy` and `CircuitBreakerPolicyConfig` have slow-call fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#resilience-circuitbreakerpolicy-and-circuitbreakerpolicyconfig-have-slow-call-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-media-plugin-opt-in-primitives",
        version: "0.9.0",
        title: "Media: `MediaPlugin` installs only the primitives you enable",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#media-mediaplugin-installs-only-the-primitives-you-enable",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-media-room-store-session-max",
        version: "0.9.0",
        title: "Media: `RoomStore::heartbeat` and `RoomStore::roster` take a `session_max` limit",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#media-roomstoreheartbeat-and-roomstoreroster-take-a-session_max-limit",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-pg-flag-store-get-before-load",
        version: "0.9.0",
        title: "Feature flags: `PgFlagStore::get` errors before the first load",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#feature-flags-pgflagstoreget-errors-before-the-first-load",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-agent-operable-stricter-analyser",
        version: "0.9.0",
        title: "Agent authority: the analyser refuses more handler shapes",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#agent-authority-the-analyser-refuses-more-handler-shapes",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-http-client-timeouts-config-fields",
        version: "0.9.0",
        title: "Config: `HttpClientConfig` and `RequestTimeoutsConfig` get new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#config-httpclientconfig-and-requesttimeoutsconfig-get-new-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-ledger-raw-sql-writes-refused",
        version: "0.9.0",
        title: "Ledger: raw-SQL framework writes to a ledgered table are refused",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#ledger-raw-sql-framework-writes-to-a-ledgered-table-are-refused-2319",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-ledger-more-writes-refused",
        version: "0.9.0",
        title: "Ledger: more writes to a ledgered table are refused",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#ledger-more-writes-to-a-ledgered-table-are-refused-2319",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-shadow-stats-comparisons-abandoned",
        version: "0.9.0",
        title: "Shadow: `ShadowStats` gains `comparisons_abandoned`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#shadow-shadowstats-gains-comparisons_abandoned",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-capsule-effect-variants",
        version: "0.9.0",
        title: "Capsules: new `CacheEffect` and `EffectSeam` variants, new `JobEffect` and `CapsuleEffects` fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#capsules-new-cacheeffect-and-effectseam-variants-new-jobeffect-and-capsuleeffects-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-api-doc-mcp-task",
        version: "0.9.0",
        title: "MCP: `ApiDoc` gains a `mcp_task` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#mcp-apidoc-gains-a-mcp_task-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-api-doc-mcp-core",
        version: "0.9.0",
        title: "MCP: `ApiDoc` gains a `mcp_core` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#mcp-apidoc-gains-a-mcp_core-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-static-route-meta-content-type",
        version: "0.9.0",
        title: "`StaticRouteMeta` has a `content_type` field",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#staticroutemeta-has-a-content_type-field",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-data-flow-model-path-version",
        version: "0.9.0",
        title: "Data flow: manifest rows have a crate version in `model_path`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#data-flow-manifest-rows-have-a-crate-version-in-model_path",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-repository-cascade-probe-delete-many-order",
        version: "0.9.0",
        title: "Repository: dependent cascades probe first and `delete_many` hooks parents first",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#repository-dependent-cascades-probe-first-and-delete_many-hooks-parents-first",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-cache-coherence-mutation-fields",
        version: "0.9.0",
        title: "Cache coherence: `MutationDescriptor` and `Mutation` have new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#cache-coherence-mutationdescriptor-and-mutation-have-new-fields-2364",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-route-idempotency-off",
        version: "0.9.0",
        title: "Idempotency: `RouteIdempotency` has an `Off` variant",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#idempotency-routeidempotency-has-an-off-variant",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-idempotency-bearer-token-slot",
        version: "0.9.0",
        title: "Idempotency: bearer-token requests get their own cache slot",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#idempotency-bearer-token-requests-get-their-own-cache-slot",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-graph-served-app-state",
        version: "0.9.0",
        title: "Graph: `served()` takes the app's `AppState`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#graph-served-takes-the-apps-appstate",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-billing-write-tie",
        version: "0.9.0",
        title: "Billing: `Write` has a `Tie` variant",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#billing-write-has-a-tie-variant",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-model-new-normalize-impl",
        version: "0.9.0",
        title: "`#[model]`: every `NewX` implements `Normalize` again",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#model-every-newx-implements-normalize-again",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-constela-retention-bounds",
        version: "0.9.0",
        title: "Constela: tighter bounds on what a document keeps",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#constela-tighter-bounds-on-what-a-document-keeps",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-api-doc-webmcp-fields",
        version: "0.9.0",
        title: "WebMCP: `ApiDoc` gains `webmcp_tool` and `webmcp_exclude` fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#webmcp-apidoc-gains-webmcp_tool-and-webmcp_exclude-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.9.0-agent-authority-webmcp-fields",
        version: "0.9.0",
        title: "Agent authority: manifest rows gain WebMCP fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.9.0.md#agent-authority-manifest-rows-gain-webmcp-fields",
        rewrite: Rewrite::GuideOnly,
    },
    // 0.10.0 ships no codemod: no break below (one per section of the guide's *Breaking changes*)
    // is rename-level, so each is `GuideOnly`.
    AppMigration {
        id: "0.10.0-features-optional-modules",
        version: "0.10.0",
        title: "Features: 16 modules are now optional",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#features-16-modules-are-now-optional",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-features-widget-families",
        version: "0.10.0",
        title: "Features: four widget families are now optional",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#features-four-widget-families-are-now-optional",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-features-jwt-push",
        version: "0.10.0",
        title: "Features: `jwt` and `push` are now optional",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#features-jwt-and-push-are-now-optional",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-billing-event-claim-split",
        version: "0.10.0",
        title: "Billing: `EventClaim::Duplicate` is now `Applied` and `InFlight`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#billing-eventclaimduplicate-is-now-applied-and-inflight",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-widgets-class-prefix",
        version: "0.10.0",
        title: "Widgets: alert, badge and wizard classes have the `autumn-` prefix",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#widgets-alert-badge-and-wizard-classes-have-the-autumn--prefix",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-widgets-nav-menu-popover",
        version: "0.10.0",
        title: "Widgets: `NavMenu` is a popover menu",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#widgets-navmenu-is-a-popover-menu",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-job-tracking-update-record",
        version: "0.10.0",
        title: "Jobs: `JobTrackingStore` has a required `update_record` method",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#jobs-jobtrackingstore-has-a-required-update_record-method",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-ingress-futures-unboxed",
        version: "0.10.0",
        title: "Middleware: ingress service futures are not boxed",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#middleware-ingress-service-futures-are-not-boxed",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-csrf-extractors-infallible",
        version: "0.10.0",
        title: "Security: the CSRF extractors cannot reject",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#security-the-csrf-extractors-cannot-reject",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-sqlite-repository-alias",
        version: "0.10.0",
        title: "Repositories: a SQLite build defines `Sqlite<Trait>`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#repositories-a-sqlite-build-defines-sqlitetrait",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-fleet-config-fields",
        version: "0.10.0",
        title: "Database: `DatabaseFleetConfig` has four new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#database-databasefleetconfig-has-four-new-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-autocomplete-config-fields",
        version: "0.10.0",
        title: "Widgets: `AutocompleteConfig` has five new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#widgets-autocompleteconfig-has-five-new-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-unknown-autumn-role",
        version: "0.10.0",
        title: "Config: an unknown `AUTUMN_ROLE` stops the boot",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#config-an-unknown-autumn_role-stops-the-boot",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-role-type",
        version: "0.10.0",
        title: "Config: `state.role()` returns `&Role`; `config.role` is a `RoleName`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#config-staterole-returns-role-configrole-is-a-rolename",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-route-group-fields",
        version: "0.10.0",
        title: "Routing: `Route` and `AutumnConfig` have new fields",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#routing-route-and-autumnconfig-have-new-fields",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-msrv-1-97",
        version: "0.10.0",
        title: "Toolchain: Rust 1.97.0 or newer",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#toolchain-rust-1970-or-newer",
        rewrite: Rewrite::GuideOnly,
    },
    AppMigration {
        id: "0.10.0-reqwest-0-13",
        version: "0.10.0",
        title: "HTTP client: `reqwest` 0.13",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.10.0.md#http-client-reqwest-013",
        rewrite: Rewrite::GuideOnly,
    },
];

/// The full registry.
pub fn app_migrations() -> &'static [AppMigration] {
    APP_MIGRATIONS
}

/// A `major.minor.patch` version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The pre-release identifier, if this is a pre-release of the triple:
    /// `Some("rc.1")` for `0.7.0-rc.1`.
    ///
    /// Kept verbatim so the report can echo the version that was actually asked
    /// for — `0.7.0-rc.1` and `0.7.0-beta.2` are different releases, and a
    /// machine-readable record that flattens both to `0.7.0-pre` is not a
    /// record of anything.
    ///
    /// *Ordering* still only distinguishes pre-release from stable. That is the
    /// one comparison that has to be right: every registry version is a
    /// released one, so `rc.1` vs `rc.2` never decides which migrations apply,
    /// and when two workspace members race to set the floor either answer
    /// selects the same set.
    pub prerelease: Option<String>,
}

impl Version {
    /// A released version.
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: None,
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            // `0.7.0-rc.1` precedes `0.7.0`: an app on the candidate still needs the release's
            // migrations. A derived `Ord` would sort `None` first and invert this.
            .then_with(|| other.prerelease.is_some().cmp(&self.prerelease.is_some()))
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
        if let Some(prerelease) = &self.prerelease {
            write!(f, "-{prerelease}")?;
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
    // An upper bound is not a floor: reading `<0.6` as 0.6.0 would skip the 0.6.0 codemod a
    // 0.5.x app needs. Ask for `--from`.
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
    let (core, prerelease) = without_build.split_once('-').map_or_else(
        || (without_build, None),
        |(core, identifier)| (core, Some(identifier.to_owned())),
    );
    let core = core.trim();
    if core.is_empty() {
        return None;
    }
    let mut parts = core.split('.');
    // An omitted or wildcard segment floors at zero: `0.5.*` and `0.5` pin the same floor. A
    // wildcard major is refused: reading `*` as 0.0.0 would select every migration ever written.
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
            // Cargo rejects `1.*.3`; reading the `3` would invent precision the user never wrote.
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
pub fn migrations_between(from: &Version, to: &Version) -> Vec<&'static AppMigration> {
    select_between(APP_MIGRATIONS, from, to)
}

/// [`migrations_between`] over an explicit registry, so the selection rule is
/// testable against a table with more than one release in it.
pub fn select_between(
    registry: &'static [AppMigration],
    from: &Version,
    to: &Version,
) -> Vec<&'static AppMigration> {
    registry
        .iter()
        .filter(|migration| {
            parse_version_req(migration.version)
                .is_some_and(|version| version > *from && version <= *to)
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
    /// A pre-release of the triple. The identifier is kept verbatim, so tests
    /// that care about it can name it and the rest can use the default.
    fn pre(major: u64, minor: u64, patch: u64) -> Version {
        pre_id(major, minor, patch, "rc.1")
    }

    fn pre_id(major: u64, minor: u64, patch: u64, identifier: &str) -> Version {
        Version {
            prerelease: Some(identifier.to_owned()),
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
        // The identifier is kept verbatim so the report can echo the version that was asked for.
        assert_eq!(
            parse_version_req("0.7.0-beta.2"),
            Some(pre_id(0, 7, 0, "beta.2"))
        );
        assert_eq!(
            parse_version_req("0.7.0-rc.1")
                .map(|v| v.to_string())
                .as_deref(),
            Some("0.7.0-rc.1")
        );
        assert_eq!(
            parse_version_req("0.7.0-beta.2")
                .map(|v| v.to_string())
                .as_deref(),
            Some("0.7.0-beta.2")
        );
    }

    #[test]
    fn prereleases_of_one_triple_are_interchangeable_for_selection() {
        // Every registry version is a release, so ordering only separates pre-release from stable.
        // Display keeps `rc.1` and `beta.2` distinct; comparison does not.
        assert_eq!(pre_id(0, 7, 0, "rc.1"), pre_id(0, 7, 0, "rc.1"));
        assert!(pre_id(0, 7, 0, "beta.2") < v(0, 7, 0));
        assert_eq!(
            pre_id(0, 7, 0, "rc.1").cmp(&pre_id(0, 7, 0, "beta.2")),
            std::cmp::Ordering::Equal,
            "two pre-releases of one triple are interchangeable for selection"
        );
    }

    #[test]
    fn a_prerelease_sorts_below_its_release() {
        assert!(pre(0, 7, 0) < v(0, 7, 0), "0.7.0-rc.1 precedes 0.7.0");
        assert!(v(0, 6, 0) < pre(0, 7, 0), "and still follows 0.6.0");
        assert_ne!(pre(0, 7, 0), v(0, 7, 0));
    }

    #[test]
    fn an_app_on_a_release_candidate_still_needs_that_releases_migrations() {
        // Upgrading 0.7.0-rc.1 -> 0.7.0: the candidate is behind the release, so the release's
        // migrations apply.
        let ids = ids(&select_between(
            FIXTURE_REGISTRY,
            &pre(0, 7, 0),
            &v(0, 7, 0),
        ));
        assert_eq!(ids, vec!["0.7.0-c"]);
    }

    #[test]
    fn targeting_a_release_candidate_does_not_select_the_release() {
        // `--to 0.7.0-rc.1` must not pull in migrations registered for the
        // finished 0.7.0.
        let ids = ids(&select_between(
            FIXTURE_REGISTRY,
            &v(0, 6, 0),
            &pre(0, 7, 0),
        ));
        assert!(ids.is_empty(), "0.7.0 is not reached yet, got {ids:?}");
    }

    #[test]
    fn rejects_unparsable_requirements() {
        // No single lower bound: ask for `--from` rather than rewrite against a guessed release.
        assert_eq!(parse_version_req("*"), None);
        assert_eq!(parse_version_req(">=0.4, <0.6"), None);
        // An upper bound is not a floor: reading `<0.6` as 0.6.0 would skip the 0.6.0 codemod.
        assert_eq!(parse_version_req("<0.6"), None);
        assert_eq!(parse_version_req("<=0.6.0"), None);
        assert_eq!(parse_version_req(""), None);
        assert_eq!(parse_version_req("trunk-dev"), None);
        // A wildcard *major* is the bare `*` in another spelling.
        assert_eq!(parse_version_req("x"), None);
        assert_eq!(parse_version_req("X.1"), None);
        // Cargo rejects precision after a wildcard.
        assert_eq!(parse_version_req("1.*.3"), None);
    }

    #[test]
    fn a_wildcard_segment_floors_at_zero() {
        // `0.5.*` is `0.5` and `0.*` is `0`, so each pair parses to the same floor.
        assert_eq!(parse_version_req("0.5.*"), Some(Version::new(0, 5, 0)));
        assert_eq!(parse_version_req("0.5.x"), Some(Version::new(0, 5, 0)));
        assert_eq!(parse_version_req("^1.2.*"), Some(Version::new(1, 2, 0)));
        assert_eq!(parse_version_req("0.*"), parse_version_req("0"));
        assert_eq!(parse_version_req("0.x"), Some(Version::new(0, 0, 0)));
    }

    #[test]
    fn selects_migrations_newer_than_the_recorded_version() {
        let ids: Vec<_> = migrations_between(&v(0, 5, 0), &v(0, 6, 0))
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
        // An app on 0.6.0 needs none of 0.6.0's migrations but all of 0.7.0's. Assert the
        // boundary, not an empty result, so the test survives new 0.7.0 entries.
        let selected = migrations_between(&v(0, 6, 0), &v(0, 7, 0));
        let ids: Vec<_> = selected.iter().map(|m| m.id).collect();
        assert!(
            selected.iter().all(|m| m.version == "0.7.0"),
            "0.6.0 -> 0.7.0 must select only 0.7.0 migrations, got {ids:?}"
        );
        assert!(
            !ids.is_empty(),
            "0.7.0 registers migrations, so this range is not empty — if it is, \
             the range filter dropped them"
        );
    }

    #[test]
    fn excludes_migrations_newer_than_the_target() {
        let selected = migrations_between(&v(0, 4, 0), &v(0, 5, 0));
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
        assert!(migrations_between(&v(0, 6, 0), &v(0, 6, 0)).is_empty());
        assert!(migrations_between(&v(0, 6, 0), &v(0, 5, 0)).is_empty());
    }

    #[test]
    fn selection_is_ordered_oldest_release_first() {
        let selected = migrations_between(&v(0, 0, 0), &v(9, 9, 9));
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
            ids(&select_between(FIXTURE_REGISTRY, &v(0, 4, 0), &v(0, 7, 0))),
            vec!["0.5.0-a", "0.6.0-b", "0.7.0-c"],
        );
    }

    #[test]
    fn a_multi_release_range_excludes_both_ends_correctly() {
        // Already on 0.5.0, upgrading to 0.6.0: only 0.6.0 is ahead.
        assert_eq!(
            ids(&select_between(FIXTURE_REGISTRY, &v(0, 5, 0), &v(0, 6, 0))),
            vec!["0.6.0-b"],
        );
        // Skipping a release still picks up the one in the middle.
        assert_eq!(
            ids(&select_between(FIXTURE_REGISTRY, &v(0, 5, 0), &v(0, 7, 0))),
            vec!["0.6.0-b", "0.7.0-c"],
        );
        assert!(select_between(FIXTURE_REGISTRY, &v(0, 7, 0), &v(0, 7, 0)).is_empty());
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
        // `rewrite_source_for_releases` resolves chains across releases in one run, but a chain
        // within one release would need a second pass that does not happen.
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
        // `review`, not `auto` (#2234): receiver identification is textual, not name resolution.
        assert_eq!(migration.confidence, Confidence::Review);
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
        // A type that only starts with `Pg` or only ends in `Repository` is not generated.
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

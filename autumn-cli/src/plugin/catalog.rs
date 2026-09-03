//! The install catalog: what `autumn plugin list` shows and what
//! `autumn plugin add` knows how to mount.
//!
//! Two sources feed the catalog, exactly as issue #1606 scopes discovery
//! ("crates.io plus the existing naming convention"):
//!
//! - **First-party** plugins ship as a static table compiled into the CLI.
//!   They are released in lockstep with `autumn-web`, so the CLI's own version
//!   *is* the version to install, and the mount snippet can be written down
//!   once and compile-gated in CI.
//! - **Community** plugins are discovered on crates.io through the documented
//!   `autumn-plugin-<name>` naming convention (see `docs/plugins.md`). Their
//!   mount is *derived* from the same convention and printed for the user to
//!   apply, never spliced in: nothing here can verify a third-party crate
//!   really exposes `<Name>Plugin`, and a wrong guess would leave the app not
//!   compiling.

/// One installable first-party plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// crates.io name, e.g. `autumn-admin-plugin`.
    pub crate_name: &'static str,
    /// One-line description shown by `autumn plugin list`. Kept byte-identical
    /// to the crate's own `Cargo.toml` `description` (pinned by a unit test)
    /// so the listing can never quietly drift from what crates.io shows.
    pub summary: &'static str,
    /// Snippet spliced into the `AppBuilder` chain, already indented to sit
    /// under the builder-opening `autumn_web::app()` line. Its first line is
    /// the `// added by ...` marker.
    pub mount: &'static str,
    /// The builder call this plugin mounts through: `.plugin(` for a
    /// `Plugin`, `.with_blob_store(` for `autumn-storage-s3`.
    pub mount_call: &'static str,
    /// The fully-qualified path this command's own mount writes. Trusted only
    /// *inside* a [`Self::mount_call`] argument: on its own it is just a type
    /// name, so `fn configure(_: autumn_admin_plugin::AdminPlugin) {}` would
    /// otherwise read as a mount and suppress the real one.
    pub mount_arg: &'static str,
    /// The constructor call a mount must contain (`AdminPlugin::new(`).
    /// Trusted anywhere in code, because a plugin built into a variable and
    /// mounted as `.plugin(configured)` is still a mount — and splicing a
    /// second, default-constructed one over it would win the duplicate check
    /// and silently discard the user's configuration.
    pub constructor: &'static str,
    /// Config keys the plugin needs before the app can serve it.
    pub config_keys: &'static [&'static str],
    /// Follow-up steps printed after a successful install.
    pub post_install: &'static [&'static str],
}

/// Every first-party plugin in this workspace, in `plugin list` order.
///
/// No entry adds `features = [...]` to the app's own `autumn-web` dependency.
/// Each plugin crate already depends on `autumn-web` with the features its
/// mount needs, and Cargo unifies features across the graph — so one dependency
/// line is genuinely enough, and writing features into the user's manifest
/// would duplicate a fact the plugin crate already owns. The `plugin-install`
/// CI gate compiles every one of these mounts into a fresh scaffold, so if a
/// plugin ever drops a feature its mount needs, that gate is what fails.
///
/// Four of the five implement `Plugin` and mount with `.plugin(...)`.
/// `autumn-storage-s3` is the exception: it exposes a `BlobStore`, not a
/// `Plugin`, and `S3BlobStore::from_config` is `async` — but `.await` is legal
/// inside a method-call argument in an `async fn`, so its mount is still one
/// splice into the same builder chain, with no second anchor and no statement
/// rewriting.
pub const FIRST_PARTY: &[CatalogEntry] = &[
    CatalogEntry {
        crate_name: "autumn-admin-plugin",
        summary: "Out-of-the-box admin panel plugin for autumn-web applications",
        mount: concat!(
            "        // added by `autumn plugin add autumn-admin-plugin`\n",
            "        .plugin(autumn_admin_plugin::AdminPlugin::new())\n",
        ),
        mount_call: ".plugin(",
        mount_arg: "autumn_admin_plugin::AdminPlugin",
        constructor: "AdminPlugin::new(",
        config_keys: &["[database]\nprimary_url = \"postgres://localhost/my_app\""],
        post_install: &[
            "Register a model with `autumn generate admin <Model>` — the panel mounts at /admin but starts with no models.",
            "Sign in with a session carrying the `admin` role; override with `AdminPlugin::new().require_role(...)`.",
            "The jobs dashboard at /admin/jobs works without a database; model screens need a configured pool.",
        ],
    },
    CatalogEntry {
        crate_name: "autumn-cache-redis",
        summary: "Redis-backed shared cache plugin for autumn-web applications",
        mount: concat!(
            "        // added by `autumn plugin add autumn-cache-redis`\n",
            "        .plugin(autumn_cache_redis::RedisCachePlugin::new())\n",
        ),
        mount_call: ".plugin(",
        mount_arg: "autumn_cache_redis::RedisCachePlugin",
        constructor: "RedisCachePlugin::new(",
        config_keys: &[
            "[cache]\nbackend = \"redis\"\n\n[cache.redis]\nurl = \"redis://127.0.0.1:6379\"",
        ],
        post_install: &[
            "Until `[cache] backend = \"redis\"` is set the plugin is a no-op and the in-process Moka cache stays in use.",
        ],
    },
    CatalogEntry {
        crate_name: "autumn-media-plugin",
        summary: "Live-streaming media plugin (broadcast + rooms) for autumn-web applications",
        mount: concat!(
            "        // added by `autumn plugin add autumn-media-plugin`\n",
            "        .plugin(autumn_media_plugin::MediaPlugin::new())\n",
        ),
        mount_call: ".plugin(",
        mount_arg: "autumn_media_plugin::MediaPlugin",
        constructor: "MediaPlugin::new(",
        config_keys: &[
            "[media]\nroom_max_participants = 6\n\n[media.mediamtx]\napi_base = \"http://127.0.0.1:9997\"",
        ],
        post_install: &[
            "Both primitives are off by default — chain `.with_broadcast()` and/or `.with_rooms()` to enable them.",
            "Broadcast needs a reachable MediaMTX origin and an ffmpeg binary; see autumn-media-plugin/README.md.",
        ],
    },
    CatalogEntry {
        crate_name: "autumn-search",
        summary: "Keyword and vector search plugin for autumn-web applications: mark a model searchable, get an index that stays in sync",
        mount: concat!(
            "        // added by `autumn plugin add autumn-search`\n",
            "        .plugin(autumn_search::SearchPlugin::new())\n",
        ),
        mount_call: ".plugin(",
        mount_arg: "autumn_search::SearchPlugin",
        constructor: "SearchPlugin::new(",
        config_keys: &["[search]\nengine = \"postgres\""],
        post_install: &[
            "Mark a model with `#[searchable]`, then register it: `SearchPlugin::new().postgres().index::<Model>()`.",
            "Keep the index in sync by giving the model's `#[repository]` a `SearchSyncHooks` alias.",
            "Backfill an existing table with `autumn search reindex`.",
        ],
    },
    CatalogEntry {
        crate_name: "autumn-storage-s3",
        summary: "S3-compatible blob storage plugin for autumn-web applications",
        mount: concat!(
            "        // added by `autumn plugin add autumn-storage-s3`\n",
            "        .with_blob_store({\n",
            "            // `load_lenient_unknown_roots` (not `load`) so a config section\n",
            "            // belonging to another plugin — `[media]`, `[search]` — is a warning\n",
            "            // here rather than a hard failure: this load only needs `[storage]`.\n",
            "            let config = autumn_web::config::AutumnConfig::load_lenient_unknown_roots()\n",
            "                .expect(\"autumn.toml must load before the S3 blob store can be built\");\n",
            "            autumn_storage_s3::S3BlobStore::from_config(&config.storage.s3)\n",
            "                .await\n",
            "                .expect(\"`[storage.s3]` must set `bucket` and `region`\")\n",
            "        })\n",
        ),
        mount_call: ".with_blob_store(",
        mount_arg: "autumn_storage_s3::S3BlobStore",
        constructor: "S3BlobStore::from_config(",
        config_keys: &[
            "[storage]\nbackend = \"s3\"\n\n[storage.s3]\nbucket = \"my-bucket\"\nregion = \"us-east-1\"",
        ],
        post_install: &[
            "Set `[storage.s3] bucket` and `region` before starting the app — the mount panics with that message until you do.",
            "Credentials come from the standard AWS chain, or name env vars with `access_key_id_env` / `secret_access_key_env`.",
        ],
    },
];

/// The first-party entry named `name`, if there is one.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static CatalogEntry> {
    FIRST_PARTY.iter().find(|entry| entry.crate_name == name)
}

/// The documented third-party crate-name prefix (`docs/plugins.md`).
pub const COMMUNITY_PREFIX: &str = "autumn-plugin-";

/// Whether `name` follows the documented third-party naming convention **and**
/// is a legal crates.io crate name.
///
/// The charset check is load-bearing, not cosmetic: this predicate is the only
/// gate on a name that is then interpolated into a crates.io URL *and* written
/// as a bare TOML key. `autumn-plugin-x/../../crates/serde` would otherwise be
/// URL-normalised into a request for a different crate and then written as a
/// key containing `/` and `.`, which no longer parses — the one outcome
/// `plugin add` promises is impossible.
///
/// The bare prefix with nothing after it is not a crate name either.
#[must_use]
pub fn is_community_name(name: &str) -> bool {
    name.strip_prefix(COMMUNITY_PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// The `<Name>Plugin` struct a community crate is expected to expose, derived
/// from the documented convention (`autumn-plugin-live-feed` →
/// `LiveFeedPlugin`). `None` when `name` does not follow the convention.
#[must_use]
pub fn community_struct_name(name: &str) -> Option<String> {
    if !is_community_name(name) {
        return None;
    }
    let rest = name.strip_prefix(COMMUNITY_PREFIX)?;
    let mut out = String::with_capacity(rest.len() + "Plugin".len());
    for segment in rest.split('-').filter(|s| !s.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        return None;
    }
    out.push_str("Plugin");
    Some(out)
}

/// The Rust module path for a crate name (`autumn-plugin-live-feed` →
/// `autumn_plugin_live_feed`).
#[must_use]
pub fn module_path_for(crate_name: &str) -> String {
    crate_name.replace('-', "_")
}

/// The `.plugin(...)` line a community crate is expected to mount with.
/// `None` when the struct name cannot be derived.
///
/// Printed, never spliced: nothing here can check that a third-party crate
/// really follows the convention, and a wrong guess would leave the app not
/// compiling — the one outcome issue #1606 rules out.
#[must_use]
pub fn community_mount_snippet(crate_name: &str) -> Option<String> {
    let struct_name = community_struct_name(crate_name)?;
    Some(format!(
        "        .plugin({}::{struct_name}::new())",
        module_path_for(crate_name)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Workspace members that are not *installable* plugin crates: the
    /// framework itself, its macros, this CLI, the schema core, the edge
    /// runtime, and the pinned plugin-surface reference (issue #1601) — none of
    /// which appear in `docs/plugins.md`'s first-party plugin table.
    ///
    /// `autumn-plugin-reference` really is a `Plugin`, which is the point of
    /// it: it exists so CI compiles the declared stable plugin surface on every
    /// change. It is never published, so `autumn plugin add` could not install
    /// it if it were listed. `every_excluded_member_is_unpublishable_or_core`
    /// below keeps that from becoming a way to hide a real plugin.
    const NON_PLUGIN_MEMBERS: &[&str] = &[
        "autumn",
        "autumn-macros",
        "autumn-cli",
        "autumn-schema-core",
        "autumn-edge",
        "autumn-plugin-reference",
    ];

    /// Crates in [`NON_PLUGIN_MEMBERS`] that ARE published, and so are excluded
    /// for being core rather than for being uninstallable.
    const PUBLISHED_NON_PLUGIN_MEMBERS: &[&str] = &[
        "autumn",
        "autumn-macros",
        "autumn-cli",
        "autumn-schema-core",
        "autumn-edge",
    ];

    /// The exclusion list above is the one way a genuinely installable
    /// first-party plugin could be dropped from `autumn plugin list` without
    /// the coverage test noticing. So every entry has to earn its place: it is
    /// either one of the named core crates, or it is `publish = false` and
    /// therefore not installable at all.
    #[test]
    fn every_excluded_member_is_unpublishable_or_core() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        for member in NON_PLUGIN_MEMBERS {
            if PUBLISHED_NON_PLUGIN_MEMBERS.contains(member) {
                continue;
            }
            let manifest = workspace.join(member).join("Cargo.toml");
            let content = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
            let table: toml::Table = toml::from_str(&content).expect("parse manifest");
            let publish = table
                .get("package")
                .and_then(|p| p.get("publish"))
                .and_then(toml::Value::as_bool);
            assert_eq!(
                publish,
                Some(false),
                "{member} is excluded from the plugin catalog but is publishable; either list it \
                 in the catalog or add it to PUBLISHED_NON_PLUGIN_MEMBERS with a reason"
            );
        }
    }

    /// AC #1: the listing covers **all** first-party plugins in this
    /// workspace, not just the three `plugin add` is spelled out for.
    ///
    /// The expected set is ENUMERATED from the workspace manifest rather than
    /// hardcoded, so a sixth first-party plugin crate added later fails this
    /// test instead of quietly never appearing in `autumn plugin list`.
    #[test]
    fn catalog_covers_every_first_party_plugin_in_the_workspace() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).expect("manifest");
        let table: toml::Table = toml::from_str(&manifest).expect("parse workspace manifest");
        let members = table["workspace"]["members"]
            .as_array()
            .expect("members array");

        let expected: Vec<String> = members
            .iter()
            .filter_map(toml::Value::as_str)
            // Top-level crates only: `examples/*`, `benchmarks/*` and
            // `example-e2e` are apps, not plugin crates.
            .filter(|member| !member.contains('/') && *member != "example-e2e")
            .filter(|member| !NON_PLUGIN_MEMBERS.contains(member))
            .map(std::borrow::ToOwned::to_owned)
            .collect();
        assert!(
            expected.len() >= 5,
            "the workspace enumeration found too few plugin crates: {expected:?}"
        );

        let names: Vec<&str> = FIRST_PARTY.iter().map(|e| e.crate_name).collect();
        for member in &expected {
            assert!(
                names.contains(&member.as_str()),
                "catalog is missing the first-party plugin crate {member}"
            );
        }
    }

    /// The catalog is a hand-written table, so it can drift from the crates it
    /// describes. Pin it to the manifests: every entry must name a crate that
    /// exists in this workspace, and carry that crate's own description.
    #[test]
    fn catalog_entries_match_their_crate_manifests() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        for entry in FIRST_PARTY {
            let manifest = workspace.join(entry.crate_name).join("Cargo.toml");
            let content = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
            let table: toml::Table = toml::from_str(&content).expect("parse manifest");
            let description = table["package"]["description"]
                .as_str()
                .expect("description")
                .to_owned();
            assert_eq!(
                entry.summary, description,
                "{} summary drifted from its Cargo.toml description",
                entry.crate_name
            );
        }
    }

    /// Idempotency (AC #4) is decided by these three fields, so a mount that
    /// does not contain its own call, type path and constructor could never
    /// detect the mount it just wrote.
    #[test]
    fn every_mount_contains_its_own_detection_markers() {
        for entry in FIRST_PARTY {
            for (label, needle) in [
                ("mount_call", entry.mount_call),
                ("mount_arg", entry.mount_arg),
                ("constructor", entry.constructor),
            ] {
                assert!(
                    entry.mount.contains(needle),
                    "{}: {label} {needle:?} not in its own mount",
                    entry.crate_name
                );
            }
            assert!(
                entry
                    .mount
                    .contains(&format!("autumn plugin add {}", entry.crate_name)),
                "{}: mount is missing its marker comment",
                entry.crate_name
            );
        }
    }

    /// The type path is only trusted inside a mount call, and the constructor
    /// anywhere — so each must be the shape the other is not.
    #[test]
    fn detection_markers_have_the_right_shapes() {
        for entry in FIRST_PARTY {
            assert!(
                entry.mount_arg.contains("::") && !entry.mount_arg.contains('('),
                "{}: mount_arg should be a bare type path",
                entry.crate_name
            );
            assert!(
                entry.constructor.ends_with('('),
                "{}: constructor should be a call",
                entry.crate_name
            );
            assert!(
                entry.mount_call.starts_with('.') && entry.mount_call.ends_with('('),
                "{}: mount_call should be a builder call",
                entry.crate_name
            );
        }
    }

    #[test]
    fn lookup_finds_first_party_entries() {
        assert_eq!(
            lookup("autumn-admin-plugin").map(|e| e.crate_name),
            Some("autumn-admin-plugin")
        );
    }

    #[test]
    fn lookup_rejects_unknown_names() {
        assert!(lookup("autumn-plugin-nope").is_none());
        assert!(lookup("serde").is_none());
    }

    #[test]
    fn community_names_follow_the_documented_prefix() {
        assert!(is_community_name("autumn-plugin-live-feed"));
        assert!(is_community_name("autumn-plugin-audit_log2"));
        assert!(!is_community_name("autumn-admin-plugin"));
        assert!(!is_community_name("autumn-plugin-"));
        assert!(!is_community_name("serde"));
    }

    /// The prefix check is the ONLY gate on a name that is interpolated into
    /// a crates.io URL and written as a bare TOML key, so it must also reject
    /// anything that is not a legal crate name. `autumn-plugin-x/../../crates/
    /// serde` would otherwise be URL-normalised into a request for a
    /// different crate and then written as a key that no longer parses.
    #[test]
    fn community_names_reject_anything_that_is_not_a_crate_name() {
        for hostile in [
            "autumn-plugin-x/../../crates/serde",
            "autumn-plugin-x?q=1",
            "autumn-plugin-x y",
            "autumn-plugin-x\"\n[dependencies]\nevil = \"1",
            "autumn-plugin-../serde",
        ] {
            assert!(!is_community_name(hostile), "{hostile:?} must be rejected");
            assert!(community_mount_snippet(hostile).is_none(), "{hostile:?}");
        }
    }

    /// `docs/plugins.md`: third-party crates are `autumn-plugin-<name>` and
    /// expose `<Name>Plugin`.
    #[test]
    fn community_struct_name_follows_the_documented_convention() {
        assert_eq!(
            community_struct_name("autumn-plugin-live-feed").as_deref(),
            Some("LiveFeedPlugin")
        );
        assert_eq!(
            community_struct_name("autumn-plugin-audit").as_deref(),
            Some("AuditPlugin")
        );
        assert_eq!(community_struct_name("autumn-plugin-").as_deref(), None);
        assert_eq!(community_struct_name("autumn-admin-plugin"), None);
    }

    #[test]
    fn module_path_replaces_hyphens_with_underscores() {
        assert_eq!(
            module_path_for("autumn-plugin-live-feed"),
            "autumn_plugin_live_feed"
        );
        assert_eq!(
            module_path_for("autumn-admin-plugin"),
            "autumn_admin_plugin"
        );
    }

    #[test]
    fn community_mount_snippet_is_a_plugin_call() {
        let snippet = community_mount_snippet("autumn-plugin-live-feed").expect("snippet");
        assert!(
            snippet.contains(".plugin(autumn_plugin_live_feed::LiveFeedPlugin::new())"),
            "{snippet}"
        );
        assert!(community_mount_snippet("autumn-plugin-").is_none());
    }
}

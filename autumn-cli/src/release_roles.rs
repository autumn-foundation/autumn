//! The roles that `autumn release init` writes a workload for.
//!
//! One role set feeds every target: the Helm values, the Kustomize
//! components and the Fly process groups. The rules:
//!
//! - `--roles a,b`: exactly these roles. Each one is a built-in role or a
//!   `[roles.<name>]` preset in `autumn.toml`.
//! - `--split-workers`, or one or more presets: `web`, `worker`, then each
//!   preset in name order.
//! - Else: `combined` alone.
//!
//! `verification/role_manifests.rs` models these rules.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use autumn_web::config::{ProcessRole, RoleName, RolePreset, resolve_role};

/// The longest object name the Kubernetes output uses. Flagger adds
/// `-primary` or `-canary`, and the result must stay within 63.
pub const MAX_OBJECT_NAME: usize = 55;

/// Names that YAML 1.1 reads as a boolean or null. Helm uses YAML 1.1.
const YAML_WORDS: [&str; 10] = [
    "y", "n", "yes", "no", "on", "off", "true", "false", "null", "nil",
];

/// One role that gets its own workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRole {
    /// The role name, as `AUTUMN_ROLE` takes it.
    pub name: String,
    /// The name in Kubernetes object names: `_` becomes `-`.
    pub dns: String,
    /// Whether the role serves user HTTP routes. Its base decides.
    pub serves_http: bool,
}

impl ManifestRole {
    fn new(name: &str, base: ProcessRole) -> Self {
        Self {
            name: name.to_owned(),
            dns: name.replace('_', "-"),
            serves_http: base.serves_http(),
        }
    }
}

/// Read the `[roles.*]` presets from `autumn.toml` in `dir`.
///
/// No file, or no `[roles]` table, gives an empty map.
///
/// # Errors
///
/// Returns a message when the file does not parse, or a preset is not valid.
pub fn read_presets(dir: &Path) -> Result<BTreeMap<String, RolePreset>, String> {
    let path = dir.join("autumn.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(BTreeMap::new());
    };
    let table: toml::Table =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(roles) = table.get("roles") else {
        return Ok(BTreeMap::new());
    };
    let presets: BTreeMap<String, RolePreset> = roles
        .clone()
        .try_into()
        .map_err(|e| format!("{} [roles]: {e}", path.display()))?;
    autumn_web::config::validate_role_presets(&presets).map_err(|e| e.to_string())?;
    Ok(presets)
}

/// The config the image needs to run the workload roles. The image copies
/// the production example to `/app/autumn.toml`, so the example must declare
/// each `[roles.<name>]` preset and a jobs backend that every process shares.
///
/// A `combined` app alone gets no text.
///
/// # Errors
///
/// Returns a message when `autumn.toml` does not parse.
pub fn runtime_role_config(dir: &Path, roles: &[ManifestRole]) -> Result<String, String> {
    if roles.len() == 1 && roles[0].name == "combined" {
        return Ok(String::new());
    }
    let path = dir.join("autumn.toml");
    let table: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(_) => toml::Table::new(),
    };
    // The whole `[jobs]` table of the project: queues, pins and the backend
    // settings. A split role on the in-process `local` queue does not boot.
    let mut jobs = table
        .get("jobs")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let backend = jobs
        .get("backend")
        .and_then(toml::Value::as_str)
        .filter(|backend| *backend != "local")
        .unwrap_or("postgres")
        .to_owned();
    jobs.insert("backend".to_owned(), toml::Value::String(backend.clone()));
    // The Redis URL can hold a password. The env var sets it.
    if let Some(redis) = jobs.get_mut("redis").and_then(toml::Value::as_table_mut) {
        redis.remove("url");
    }
    let mut out = String::from(
        "\n# Process roles. Each workload sets AUTUMN_ROLE to one of them. A split\n\
         # topology needs a jobs backend that every process shares.\n",
    );
    if backend == "redis" {
        out.push_str("# Set the Redis URL with AUTUMN_JOBS__REDIS__URL.\n");
    }
    let mut wrapper = toml::Table::new();
    wrapper.insert("jobs".to_owned(), toml::Value::Table(jobs));
    if let Some(presets) = table.get("roles") {
        wrapper.insert("roles".to_owned(), presets.clone());
    }
    out.push_str(&toml::to_string(&wrapper).map_err(|e| e.to_string())?);
    Ok(out)
}

/// The role set for the manifests.
///
/// # Errors
///
/// Returns a message when an explicit role is not declared, or when two roles
/// get the same Kubernetes name.
pub fn manifest_roles(
    presets: &BTreeMap<String, RolePreset>,
    explicit: &[String],
    split: bool,
) -> Result<Vec<ManifestRole>, String> {
    let mut roles: Vec<ManifestRole> = Vec::new();
    if explicit.is_empty() {
        if split || !presets.is_empty() {
            roles.push(ManifestRole::new("web", ProcessRole::Web));
            roles.push(ManifestRole::new("worker", ProcessRole::Worker));
            for (name, preset) in presets {
                roles.push(ManifestRole::new(name, preset.base));
            }
        } else {
            roles.push(ManifestRole::new("combined", ProcessRole::Combined));
        }
    } else {
        for raw in explicit {
            let name: RoleName = raw.parse().map_err(|e| format!("--roles {raw:?}: {e}"))?;
            let role = resolve_role(&name, presets).map_err(|e| format!("--roles: {e}"))?;
            if roles.iter().any(|r| r.name == role.name()) {
                continue;
            }
            roles.push(ManifestRole::new(role.name(), role.base()));
        }
    }
    for (i, role) in roles.iter().enumerate() {
        if YAML_WORDS.contains(&role.name.as_str()) {
            return Err(format!(
                "role {:?}: YAML reads this name as a boolean or null; rename the role",
                role.name
            ));
        }
        if !role.dns.ends_with(|c: char| c.is_ascii_alphanumeric()) {
            return Err(format!(
                "role {:?} must end in a letter or a digit to be a Kubernetes name",
                role.name
            ));
        }
        if let Some(other) = roles[..i].iter().find(|r| r.dns == role.dns) {
            return Err(format!(
                "roles {:?} and {:?} get the same Kubernetes name {:?}; rename one",
                other.name, role.name, role.dns
            ));
        }
        if role.dns.len() > MAX_OBJECT_NAME / 2 {
            return Err(format!(
                "role {:?} is too long for a Kubernetes object name; use at most {} \
                 characters",
                role.name,
                MAX_OBJECT_NAME / 2
            ));
        }
    }
    Ok(roles)
}

/// The object name of `role`: the app name, cut to fit, then the role.
#[must_use]
pub fn object_name(app: &str, role: &ManifestRole) -> String {
    let room = MAX_OBJECT_NAME.saturating_sub(role.dns.len() + 1);
    let app: String = app.chars().take(room).collect();
    format!("{}-{}", app.trim_end_matches('-'), role.dns)
}

/// The Helm `roles:` entries, indented under `roles:`.
#[must_use]
pub fn helm_roles_yaml(roles: &[ManifestRole]) -> String {
    let mut out = String::new();
    for role in roles {
        let replicas = if role.serves_http { 2 } else { 1 };
        let _ = writeln!(
            out,
            "  {name}:\n    enabled: true\n    servesHttp: {http}\n    replicaCount: {replicas}\n    \
             pin: []\n    env: []\n    resources: {{}}\n    autoscaling:\n      enabled: false\n      \
             minReplicas: {replicas}\n      maxReplicas: 10\n      \
             targetCPUUtilizationPercentage: 80",
            name = role.name,
            http = role.serves_http,
        );
        if role.serves_http {
            out.push_str(
                "    ingress:\n      enabled: false\n      className: \"\"\n      annotations: {}\n      \
                 # - host: shop.example.com\n      #   paths: [{path: /, pathType: Prefix}]\n      \
                 hosts: []\n      tls: []\n",
            );
        }
    }
    out
}

/// The Fly `[processes]` block and the `[http_service] processes` line.
///
/// A `combined` app alone gets neither, so its `fly.toml` does not change.
#[must_use]
pub fn fly_blocks(roles: &[ManifestRole], project_name: &str) -> (String, String) {
    if roles.len() == 1 && roles[0].name == "combined" {
        return (String::new(), String::new());
    }
    let mut processes = String::from(
        "# One process group per role. Each sets AUTUMN_ROLE. Only the groups that\n\
         # serve HTTP get [http_service]. Scale each group on its own, for example\n\
         # `fly scale count web=2 worker=1`.\n[processes]\n",
    );
    for role in roles {
        let _ = writeln!(
            processes,
            "  {name} = \"env AUTUMN_ROLE={name} /usr/local/bin/{project_name}\"",
            name = role.name
        );
    }
    // Fly balances [http_service] over its groups with no path routing. Two
    // HTTP roles that mount other route groups would get each other's
    // requests, so only one group gets it.
    let http: Vec<&ManifestRole> = roles.iter().filter(|role| role.serves_http).collect();
    let public = http
        .iter()
        .find(|role| matches!(role.name.as_str(), "web" | "combined"))
        .or_else(|| http.first());
    for role in &http {
        if public.is_some_and(|p| p.name != role.name) {
            let _ = writeln!(
                processes,
                "# {name} serves HTTP too. [http_service] sends traffic only to {public}. Give\n\
                 # {name} its own [[services]] block, or its own Fly app.",
                name = role.name,
                public = public.map_or("", |p| p.name.as_str()),
            );
        }
    }
    processes.push('\n');
    let line = public.map_or_else(String::new, |p| format!("  processes = [\"{}\"]\n", p.name));
    (processes, line)
}

/// Fill the role placeholders of a rendered template. A template without
/// them does not change.
#[must_use]
pub fn fill_role_blocks(content: &str, roles: &[ManifestRole], project_name: &str) -> String {
    let (processes, http_processes) = fly_blocks(roles, project_name);
    let components = roles.iter().fold(String::new(), |mut out, role| {
        let _ = writeln!(out, "  - ../components/{}", role.dns);
        out
    });
    let content = replace_line(content, "{{helm_roles}}", &helm_roles_yaml(roles));
    let content = replace_line(&content, "{{kustomize_components}}", &components);
    let content = replace_line(&content, "{{fly_processes}}", &processes);
    content.replace("{{fly_http_processes}}", &http_processes)
}

/// The public host of a role in the Kustomize output. Overlays replace it.
fn kustomize_host(app: &str, role: &ManifestRole) -> String {
    if matches!(role.name.as_str(), "web" | "combined") {
        format!("{app}.example.com")
    } else if role.serves_http {
        format!("{}.{app}.example.com", role.dns)
    } else {
        format!("{app}.example.com")
    }
}

/// Replace a placeholder that fills its own line, with its line end. A
/// Windows checkout gives the templates CRLF line ends, so both count.
fn replace_line(text: &str, placeholder: &str, with: &str) -> String {
    text.replace(&format!("{placeholder}\r\n"), with)
        .replace(&format!("{placeholder}\n"), with)
}

/// Fill the role placeholders of one Kustomize component template.
fn fill_component(template: &str, role: &ManifestRole, app: &str, resources: &str) -> String {
    replace_line(template, "{{component_resources}}", resources)
        .replace("{{role_object}}", &object_name(app, role))
        .replace("{{role_host}}", &kustomize_host(app, role))
        .replace("{{replicas}}", if role.serves_http { "2" } else { "1" })
        .replace(
            "{{worker_note}}",
            if role.serves_http {
                ""
            } else {
                " It serves only the probes and the actuator."
            },
        )
        .replace("{{role}}", &role.name)
}

/// The Kustomize component of each role, as `(path, template)`. The release
/// renderer then fills the app placeholders.
#[must_use]
pub fn kustomize_files(roles: &[ManifestRole], app: &str) -> Vec<(String, String)> {
    use crate::release::templates;
    let mut files = Vec::new();
    for role in roles {
        let dir = format!("deploy/kustomize/components/{}", role.dns);
        let mut parts = vec![
            ("deployment.yaml", templates::KUSTOMIZE_DEPLOYMENT),
            ("pdb.yaml", templates::KUSTOMIZE_PDB),
        ];
        if role.serves_http {
            parts.push(("service.yaml", templates::KUSTOMIZE_SERVICE));
            parts.push(("ingress.yaml", templates::KUSTOMIZE_INGRESS));
        }
        let resources = parts.iter().fold(String::new(), |mut out, (name, _)| {
            let _ = writeln!(out, "  - {name}");
            out
        });
        files.push((
            format!("{dir}/kustomization.yaml"),
            fill_component(templates::KUSTOMIZE_COMPONENT, role, app, &resources),
        ));
        for (name, template) in parts {
            files.push((
                format!("{dir}/{name}"),
                fill_component(template, role, app, &resources),
            ));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presets(toml_text: &str) -> BTreeMap<String, RolePreset> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("autumn.toml"), toml_text).unwrap();
        read_presets(dir.path()).unwrap()
    }

    fn names(roles: &[ManifestRole]) -> Vec<&str> {
        roles.iter().map(|r| r.name.as_str()).collect()
    }

    /// The rules of `verification/role_manifests.rs`, on every subset of
    /// four presets: each role is declared, names are unique, and only a base
    /// that serves HTTP is marked so.
    #[test]
    fn manifest_roles_match_the_model() {
        let all = [
            ("critical", "worker"),
            ("marketing", "web"),
            ("bulk", "worker"),
            ("edge", "combined"),
        ];
        for mask in 0..16_u8 {
            let mut text = String::new();
            for (i, (name, base)) in all.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    let _ = writeln!(text, "[roles.{name}]\nbase = \"{base}\"");
                }
            }
            let p = presets(&text);
            for split in [false, true] {
                let roles = manifest_roles(&p, &[], split).unwrap();
                for (i, role) in roles.iter().enumerate() {
                    let name: RoleName = role.name.parse().unwrap();
                    let resolved = resolve_role(&name, &p).expect("declared");
                    assert_eq!(role.serves_http, resolved.serves_http(), "{}", role.name);
                    assert!(roles[..i].iter().all(|r| r.name != role.name), "{roles:?}");
                }
                let expected = if split || mask != 0 { 2 + p.len() } else { 1 };
                assert_eq!(roles.len(), expected, "{roles:?}");
            }
        }
    }

    /// The image config keeps the whole `[jobs]` table of the project, but not
    /// the Redis URL: it can hold a password.
    #[test]
    fn runtime_config_keeps_the_jobs_settings_but_not_the_redis_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[jobs]\nbackend = \"redis\"\nqueues = [\"default\", \"critical\"]\n\
             [jobs.redis]\nurl = \"redis://user:secret@cache:6379\"\nkey_prefix = \"shop:\"\n\
             [roles.critical]\nbase = \"worker\"\npin = [\"critical\"]\n",
        )
        .unwrap();
        let p = read_presets(dir.path()).unwrap();
        let roles = manifest_roles(&p, &[], false).unwrap();
        let text = runtime_role_config(dir.path(), &roles).unwrap();
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("AUTUMN_JOBS__REDIS__URL"), "{text}");
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["jobs"]["backend"].as_str(), Some("redis"));
        assert_eq!(table["jobs"]["redis"]["key_prefix"].as_str(), Some("shop:"));
        assert!(table["jobs"]["redis"].get("url").is_none());
        assert_eq!(table["jobs"]["queues"].as_array().map(Vec::len), Some(2));
        assert!(table["roles"]["critical"]["pin"].is_array());
    }

    #[test]
    fn runtime_config_replaces_a_local_jobs_backend() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[jobs]\nbackend = \"local\"\nworkers = 4\n",
        )
        .unwrap();
        let roles = manifest_roles(&BTreeMap::new(), &[], true).unwrap();
        let text = runtime_role_config(dir.path(), &roles).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["jobs"]["backend"].as_str(), Some("postgres"));
        assert_eq!(table["jobs"]["workers"].as_integer(), Some(4));
    }

    #[test]
    fn no_presets_and_no_split_gives_combined() {
        let roles = manifest_roles(&BTreeMap::new(), &[], false).unwrap();
        assert_eq!(names(&roles), ["combined"]);
        assert!(roles[0].serves_http);
    }

    #[test]
    fn split_gives_web_and_worker() {
        let roles = manifest_roles(&BTreeMap::new(), &[], true).unwrap();
        assert_eq!(names(&roles), ["web", "worker"]);
        assert!(roles[0].serves_http);
        assert!(!roles[1].serves_http);
    }

    #[test]
    fn presets_add_web_worker_and_each_preset() {
        let p = presets(
            "[roles.marketing]\nbase = \"web\"\n\n[roles.critical]\nbase = \"worker\"\n\
             pin = [\"critical\"]\n",
        );
        let roles = manifest_roles(&p, &[], false).unwrap();
        assert_eq!(names(&roles), ["web", "worker", "critical", "marketing"]);
        assert!(!roles[2].serves_http, "critical has a worker base");
        assert!(roles[3].serves_http, "marketing has a web base");
    }

    #[test]
    fn explicit_roles_are_canonical_and_deduplicated() {
        let p = presets("[roles.critical]\nbase = \"worker\"\n");
        let explicit = ["jobs", "http", "critical", "worker"].map(str::to_owned);
        let roles = manifest_roles(&p, &explicit, false).unwrap();
        assert_eq!(names(&roles), ["worker", "web", "critical"]);
    }

    #[test]
    fn explicit_undeclared_role_is_an_error() {
        let err = manifest_roles(&BTreeMap::new(), &["critical".to_owned()], false).unwrap_err();
        assert!(err.contains("critical"), "{err}");
        assert!(err.contains("no roles are declared"), "{err}");
    }

    #[test]
    fn an_underscore_becomes_a_dash_in_the_dns_name() {
        let p = presets("[roles.bulk_jobs]\nbase = \"worker\"\n");
        let roles = manifest_roles(&p, &[], false).unwrap();
        assert_eq!(roles[2].name, "bulk_jobs");
        assert_eq!(roles[2].dns, "bulk-jobs");
    }

    #[test]
    fn two_roles_with_one_dns_name_are_an_error() {
        let p =
            presets("[roles.bulk_jobs]\nbase = \"worker\"\n[roles.bulk-jobs]\nbase = \"worker\"\n");
        let err = manifest_roles(&p, &[], false).unwrap_err();
        assert!(err.contains("same Kubernetes name"), "{err}");
    }

    #[test]
    fn a_long_role_name_is_an_error() {
        let name = format!("r{}", "a".repeat(30));
        let p = presets(&format!("[roles.{name}]\nbase = \"worker\"\n"));
        let err = manifest_roles(&p, &[], false).unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    #[test]
    fn an_invalid_preset_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[roles.critical]\nbase = \"web\"\njob_workers = true\n",
        )
        .unwrap();
        let err = read_presets(dir.path()).unwrap_err();
        assert!(err.contains("job_workers"), "{err}");
    }

    #[test]
    fn no_autumn_toml_gives_no_presets() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_presets(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn object_names_fit_flagger_suffixes() {
        let role = ManifestRole::new("marketing", ProcessRole::Web);
        assert_eq!(object_name("shop", &role), "shop-marketing");
        let long = object_name(&"a".repeat(53), &role);
        assert!(long.len() <= MAX_OBJECT_NAME, "{long}");
        assert!(long.ends_with("-marketing"), "{long}");
    }
    fn split_roles() -> Vec<ManifestRole> {
        vec![
            ManifestRole::new("web", ProcessRole::Web),
            ManifestRole::new("worker", ProcessRole::Worker),
            ManifestRole::new("bulk_jobs", ProcessRole::Worker),
        ]
    }

    #[test]
    fn helm_roles_set_serves_http_from_the_base() {
        let yaml = helm_roles_yaml(&split_roles());
        assert!(
            yaml.contains("  web:\n    enabled: true\n    servesHttp: true\n"),
            "{yaml}"
        );
        assert!(
            yaml.contains("  worker:\n    enabled: true\n    servesHttp: false\n"),
            "{yaml}"
        );
        assert!(yaml.contains("  bulk_jobs:\n"), "{yaml}");
        assert_eq!(
            yaml.matches("ingress:").count(),
            1,
            "only web gets an ingress key"
        );
    }

    #[test]
    fn fly_blocks_list_every_role_and_only_http_roles_serve() {
        let (processes, http) = fly_blocks(&split_roles(), "shop");
        assert!(processes.contains("[processes]\n"), "{processes}");
        assert!(processes.contains("  web = \"env AUTUMN_ROLE=web /usr/local/bin/shop\"\n"));
        assert!(processes.contains("  worker = \"env AUTUMN_ROLE=worker /usr/local/bin/shop\"\n"));
        assert!(processes.contains("  bulk_jobs = \"env AUTUMN_ROLE=bulk_jobs"));
        assert_eq!(http, "  processes = [\"web\"]\n");
    }

    #[test]
    fn fly_gives_http_service_to_one_group_only() {
        let roles = vec![
            ManifestRole::new("marketing", ProcessRole::Web),
            ManifestRole::new("web", ProcessRole::Web),
            ManifestRole::new("worker", ProcessRole::Worker),
        ];
        let (processes, http) = fly_blocks(&roles, "shop");
        assert_eq!(http, "  processes = [\"web\"]\n");
        assert!(
            processes.contains("# marketing serves HTTP too."),
            "{processes}"
        );
    }

    #[test]
    fn yaml_words_and_a_trailing_dash_are_refused() {
        for name in ["on", "no", "jobs_"] {
            let p = presets(&format!("[roles.{name}]\nbase = \"worker\"\n"));
            let err = manifest_roles(&p, &[], false).unwrap_err();
            assert!(err.contains(name), "{err}");
        }
    }

    #[test]
    fn fly_blocks_are_empty_for_combined() {
        let roles = manifest_roles(&BTreeMap::new(), &[], false).unwrap();
        assert_eq!(fly_blocks(&roles, "shop"), (String::new(), String::new()));
    }

    /// A Windows checkout gives the templates CRLF line ends.
    #[test]
    fn placeholders_on_crlf_lines_are_filled() {
        let template = "roles:\r\n{{helm_roles}}\r\ncomponents:\r\n{{kustomize_components}}\r\n\
                        a\r\n{{fly_processes}}\r\n[http_service]\r\n{{fly_http_processes}}  x\r\n";
        let out = fill_role_blocks(template, &split_roles(), "shop");
        assert!(!out.contains("{{"), "{out}");
        assert!(out.contains("  web:\n"), "{out}");
        for (name, text) in kustomize_files(&split_roles(), "shop") {
            assert!(!text.contains("{{component_resources}}"), "{name}");
        }
        let crlf_component = crate::release::templates::KUSTOMIZE_COMPONENT
            .replace("\r\n", "\n")
            .replace('\n', "\r\n");
        let filled = fill_component(&crlf_component, &split_roles()[0], "shop", "  - a.yaml\n");
        assert!(!filled.contains("{{component_resources}}"), "{filled}");
        assert!(!filled.contains("{{role"), "{filled}");
        assert!(filled.contains("  - a.yaml\n"), "{filled}");
    }

    #[test]
    fn fill_role_blocks_leaves_other_templates_alone() {
        let text = "a: 1\n";
        assert_eq!(fill_role_blocks(text, &split_roles(), "shop"), text);
    }

    #[test]
    fn kustomize_component_per_role_with_service_only_for_http() {
        // A Windows checkout gives the templates CRLF line ends.
        let files: Vec<(String, String)> = kustomize_files(&split_roles(), "shop")
            .into_iter()
            .map(|(name, text)| (name, text.replace("\r\n", "\n")))
            .collect();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"deploy/kustomize/components/web/service.yaml"));
        assert!(names.contains(&"deploy/kustomize/components/web/ingress.yaml"));
        assert!(names.contains(&"deploy/kustomize/components/worker/deployment.yaml"));
        assert!(!names.contains(&"deploy/kustomize/components/worker/service.yaml"));
        assert!(!names.contains(&"deploy/kustomize/components/worker/ingress.yaml"));
        assert!(names.contains(&"deploy/kustomize/components/bulk-jobs/pdb.yaml"));
        let deployment = &files
            .iter()
            .find(|(n, _)| n == "deploy/kustomize/components/bulk-jobs/deployment.yaml")
            .unwrap()
            .1;
        assert!(
            deployment.contains("name: shop-bulk-jobs\n"),
            "{deployment}"
        );
        assert!(deployment.contains("- name: AUTUMN_ROLE\n              value: bulk_jobs\n"));
        assert!(deployment.contains("app.kubernetes.io/component: bulk_jobs"));
        assert!(!deployment.contains("{{role"), "{deployment}");
        let component = &files
            .iter()
            .find(|(n, _)| n == "deploy/kustomize/components/worker/kustomization.yaml")
            .unwrap()
            .1;
        assert!(component.contains("kind: Component"));
        assert!(
            component.contains("resources:\n  - deployment.yaml\n  - pdb.yaml\n"),
            "{component}"
        );
    }
}

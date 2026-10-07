use super::*;
use autumn_web::slo::{SliKind, SloConfig};
use serde::Deserialize as _;

fn config(
    name: &str,
    objective: f64,
    sli: SliKind,
    route: Option<&str>,
    ms: Option<u64>,
) -> SloConfig {
    SloConfig {
        name: name.to_owned(),
        objective,
        sli,
        route: route.map(str::to_owned),
        threshold_ms: ms,
        description: None,
    }
}

/// The fixture behind the golden files: one SLO of each shape.
fn fixture_configs() -> Vec<SloConfig> {
    let mut checkout = config(
        "checkout",
        99.5,
        SliKind::Availability,
        Some("/api/orders"),
        None,
    );
    checkout.description = Some("Customers can place orders.".to_owned());
    vec![
        config("availability", 99.9, SliKind::Availability, None, None),
        checkout,
        config(
            "orders-latency",
            99.0,
            SliKind::Latency,
            Some("/api/orders"),
            Some(250),
        ),
    ]
}

fn fixture_options() -> GenerateOptions {
    GenerateOptions {
        app: "shop".to_owned(),
        selector: Some("job=\"shop\"".to_owned()),
        prometheus_url: DEFAULT_PROMETHEUS_URL.to_owned(),
    }
}

fn fixture() -> Generated {
    let slos = slo::validate(&fixture_configs()).expect("valid fixture");
    generate(&slos, &fixture_options()).expect("generate")
}

fn file<'a>(generated: &'a Generated, name: &str) -> &'a str {
    &generated
        .files
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("{name} not generated"))
        .contents
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/slo")
}

#[test]
fn generated_files_match_the_golden_files() {
    let generated = fixture();
    let dir = golden_dir();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(&dir).expect("create golden dir");
        for file in &generated.files {
            std::fs::write(dir.join(file.name), &file.contents).expect("write golden");
        }
    }
    for file in &generated.files {
        let path = dir.join(file.name);
        let golden = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing golden {}: {e}", path.display()));
        assert_eq!(
            file.contents, golden,
            "{} differs from its golden file. If the change is intended, run \
             `UPDATE_GOLDEN=1 cargo test -p autumn-cli slo::tests` and review the diff.",
            file.name
        );
    }
}

#[test]
fn the_file_set_is_fixed() {
    let names: Vec<_> = fixture().files.iter().map(|f| f.name).collect();
    assert_eq!(
        names,
        vec![
            "prometheus-rules.yaml",
            "prometheus-rule.yaml",
            "grafana-dashboard.json",
            "argo-analysis-template.yaml",
            "flagger-metric-templates.yaml",
            "helm-values.yaml",
        ]
    );
}

#[test]
fn output_is_deterministic() {
    assert_eq!(fixture(), fixture());
}

#[test]
fn availability_for_all_routes_reads_the_response_counter() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    assert_eq!(
        error_ratio(&slos[0], &["job=\"shop\""], "5m"),
        "(sum(rate(autumn_http_responses_total{status=\"5xx\",job=\"shop\"}[5m])) or vector(0))\n\
         /\n\
         sum(rate(autumn_http_responses_total{job=\"shop\"}[5m]))"
    );
}

#[test]
fn availability_for_one_route_reads_the_histogram_count() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    assert_eq!(
        error_ratio(&slos[1], &[""], "1h"),
        "(sum(rate(autumn_http_request_duration_seconds_count{route=\"/api/orders\",status_class=\"5xx\"}[1h])) or vector(0))\n\
         /\n\
         sum(rate(autumn_http_request_duration_seconds_count{route=\"/api/orders\"}[1h]))"
    );
}

#[test]
fn latency_reads_the_bucket_at_the_threshold() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let expr = error_ratio(&slos[2], &[], "30m");
    assert!(
        expr.contains(
            "autumn_http_request_duration_seconds_bucket{route=\"/api/orders\",le=\"0.25\"}[30m]"
        ),
        "{expr}"
    );
    assert!(expr.starts_with("1 - ("), "{expr}");
}

#[test]
fn every_bucket_bound_renders_the_exporter_le_label() {
    let expected = [
        "0.001", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10",
    ];
    for (ms, le) in slo::LATENCY_BUCKETS_MS.iter().zip(expected) {
        assert_eq!(format_decimal(*ms, 3), le);
    }
}

#[test]
fn burn_thresholds_are_exact_for_99_9() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let thresholds: Vec<_> = BURN_WINDOWS
        .iter()
        .map(|w| burn_threshold(&slos[0], w.factor_tenths))
        .collect();
    assert_eq!(thresholds, vec!["0.0144", "0.006", "0.001"]);
    let rules = file(&fixture(), "prometheus-rules.yaml").to_owned();
    assert!(
        rules.contains(
            "autumn_slo:sli_error:ratio_rate1h{app=\"shop\",slo=\"availability\"} > 0.0144"
        )
    );
    assert!(
        rules.contains(
            "autumn_slo:sli_error:ratio_rate3d{app=\"shop\",slo=\"availability\"} > 0.001"
        )
    );
}

#[test]
fn rule_files_are_valid_yaml_with_three_alerts_per_slo() {
    let generated = fixture();
    let rules: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "prometheus-rules.yaml")).expect("yaml");
    let groups = rules["groups"].as_sequence().expect("groups");
    assert_eq!(groups.len(), 3);
    for group in groups {
        let rules = group["rules"].as_sequence().expect("rules");
        let alerts = rules.iter().filter(|r| r.get("alert").is_some()).count();
        let records = rules.iter().filter(|r| r.get("record").is_some()).count();
        assert_eq!(alerts, BURN_WINDOWS.len());
        assert_eq!(records, RECORDING_WINDOWS.len() + 1);
    }
    let crd: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "prometheus-rule.yaml")).expect("yaml");
    assert_eq!(crd["kind"], "PrometheusRule");
    assert_eq!(crd["spec"]["groups"], rules["groups"]);
}

/// Every recording rule that another file reads exists in the rule file.
#[test]
fn consumers_read_only_defined_recording_rules() {
    let generated = fixture();
    let rules = file(&generated, "prometheus-rules.yaml");
    let defined: std::collections::BTreeSet<&str> = rules
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- record: "))
        .collect();
    let dashboard: serde_json::Value =
        serde_json::from_str(file(&generated, "grafana-dashboard.json")).expect("json");
    let mut used = Vec::new();
    for panel in dashboard["panels"].as_array().expect("panels") {
        for target in panel["targets"].as_array().into_iter().flatten() {
            let expr = target["expr"].as_str().expect("expr");
            used.extend(
                expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
                    .filter(|t| t.starts_with("autumn_slo:"))
                    .map(str::to_owned),
            );
        }
    }
    for alert_line in rules
        .lines()
        .filter(|l| l.contains("autumn_slo:sli_error") && l.contains('>'))
    {
        used.extend(
            alert_line
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
                .filter(|t| t.starts_with("autumn_slo:"))
                .map(str::to_owned),
        );
    }
    assert!(!used.is_empty());
    for name in used {
        assert!(
            defined.contains(name.as_str()),
            "{name} is not a recording rule"
        );
    }
}

#[test]
fn the_dashboard_has_four_panels_per_slo() {
    let dashboard: serde_json::Value =
        serde_json::from_str(file(&fixture(), "grafana-dashboard.json")).expect("json");
    assert_eq!(dashboard["uid"], "autumn-slo-shop");
    assert_eq!(dashboard["panels"].as_array().expect("panels").len(), 12);
    let ids: std::collections::BTreeSet<_> = dashboard["panels"]
        .as_array()
        .expect("panels")
        .iter()
        .map(|p| p["id"].as_u64().expect("id"))
        .collect();
    assert_eq!(ids.len(), 12, "panel ids are unique");
}

#[test]
fn argo_queries_select_the_canary_and_divide_by_the_budget() {
    let generated = fixture();
    let argo: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "argo-analysis-template.yaml")).expect("yaml");
    assert_eq!(argo["kind"], "AnalysisTemplate");
    assert_eq!(argo["metadata"]["name"], "shop-slo");
    let metrics = argo["spec"]["metrics"].as_sequence().expect("metrics");
    assert_eq!(metrics.len(), 3);
    let query = metrics[0]["provider"]["prometheus"]["query"]
        .as_str()
        .expect("query");
    assert!(
        query.contains("version=\"{{args.canary-hash}}\",job=\"shop\""),
        "{query}"
    );
    assert!(query.ends_with("/ 0.001"), "{query}");
    assert_eq!(
        metrics[0]["successCondition"],
        "len(result) == 0 || isNaN(result[0]) || result[0] <= 14.4"
    );
    let checkout_query = metrics[1]["provider"]["prometheus"]["query"]
        .as_str()
        .expect("query");
    assert!(checkout_query.ends_with("/ 0.005"), "{checkout_query}");
}

#[test]
fn flagger_has_one_template_per_slo_and_the_helm_values_name_them() {
    let generated = fixture();
    let docs: Vec<serde_yaml::Value> =
        serde_yaml::Deserializer::from_str(file(&generated, "flagger-metric-templates.yaml"))
            .map(|d| serde_yaml::Value::deserialize(d).expect("yaml"))
            .collect();
    let names: Vec<_> = docs
        .iter()
        .map(|d| d["metadata"]["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["shop-availability", "shop-checkout", "shop-orders-latency"]
    );
    for doc in &docs {
        let query = doc["spec"]["query"].as_str().expect("query");
        assert!(query.contains("[{{ interval }}]"), "{query}");
        assert!(query.contains("pod=~\"{{ target }}-"), "{query}");
    }
    let values: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "helm-values.yaml")).expect("yaml");
    assert_eq!(values["analysis"]["templateName"], "shop-slo");
    assert_eq!(values["analysis"]["maxBurnRate"], 14.4);
    let templates: Vec<_> = values["analysis"]["metricTemplates"]
        .as_sequence()
        .expect("list")
        .iter()
        .map(|v| v.as_str().expect("name").to_owned())
        .collect();
    assert_eq!(templates, names);
}

#[test]
fn no_slo_is_an_error_with_an_example() {
    let error = generate(&[], &fixture_options()).expect_err("no slo");
    assert!(error.contains("[[slo]]"), "{error}");
}

#[test]
fn the_app_name_is_made_kubernetes_safe() {
    assert_eq!(kube_name("My_App").as_deref(), Ok("my-app"));
    for bad in ["", "1app", "app!", "app-", &"a".repeat(51)] {
        assert!(kube_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_bad_selector_is_rejected() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    for bad in ["{job=\"x\"}", "job", "a=\"b\"\nc"] {
        let mut options = fixture_options();
        options.selector = Some(bad.to_owned());
        assert!(generate(&slos, &options).is_err(), "{bad:?}");
    }
}

#[test]
fn no_selector_gives_bare_series() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let mut options = fixture_options();
    options.selector = None;
    let generated = generate(&slos, &options).expect("generate");
    let rules = file(&generated, "prometheus-rules.yaml");
    assert!(
        rules.contains("sum(rate(autumn_http_responses_total[5m]))"),
        "{rules}"
    );
}

#[test]
fn a_long_object_name_is_rejected() {
    let slos = slo::validate(&[config(
        &"a".repeat(40),
        99.0,
        SliKind::Availability,
        None,
        None,
    )])
    .expect("valid");
    let mut options = fixture_options();
    options.app = "b".repeat(30);
    let error = generate(&slos, &options).expect_err("too long");
    assert!(error.contains("63"), "{error}");
}

#[test]
fn a_low_objective_warns_that_the_fast_page_cannot_fire() {
    let slos =
        slo::validate(&[config("a", 90.0, SliKind::Availability, None, None)]).expect("valid");
    let generated = generate(&slos, &fixture_options()).expect("generate");
    assert_eq!(generated.warnings.len(), 1);
    assert!(
        generated.warnings[0].contains("never"),
        "{:?}",
        generated.warnings
    );
    assert!(fixture().warnings.is_empty());
}

fn args(dir: &Path, check: bool) -> GenerateArgs {
    GenerateArgs {
        out_dir: dir.to_path_buf(),
        app: Some("shop".to_owned()),
        selector: Some("job=\"shop\"".to_owned()),
        prometheus_url: DEFAULT_PROMETHEUS_URL.to_owned(),
        check,
    }
}

#[test]
fn execute_writes_then_check_passes_then_detects_drift() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("deploy/slo");
    let configs = fixture_configs();

    let missing = execute(&configs, "ignored", &args(&dir, true)).expect_err("missing files");
    assert!(missing.contains("prometheus-rules.yaml"), "{missing}");

    let (outcome, _) = execute(&configs, "ignored", &args(&dir, false)).expect("write");
    let Outcome::Written(paths) = outcome else {
        panic!("expected written files");
    };
    assert_eq!(paths.len(), 6);
    assert_eq!(
        std::fs::read_to_string(dir.join("helm-values.yaml")).expect("read"),
        file(&fixture(), "helm-values.yaml")
    );

    let (outcome, _) = execute(&configs, "ignored", &args(&dir, true)).expect("check");
    assert_eq!(outcome, Outcome::UpToDate);

    std::fs::write(dir.join("grafana-dashboard.json"), "{}").expect("edit");
    let drift = execute(&configs, "ignored", &args(&dir, true)).expect_err("drift");
    assert!(drift.contains("grafana-dashboard.json"), "{drift}");
    assert!(!drift.contains("helm-values.yaml"), "{drift}");
}

#[test]
fn execute_reports_a_bad_slo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bad = vec![config("a", 100.0, SliKind::Availability, None, None)];
    let error = execute(&bad, "shop", &args(tmp.path(), false)).expect_err("bad");
    assert!(error.starts_with("[[slo]] a:"), "{error}");
}

#[test]
fn execute_falls_back_to_the_default_app() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut a = args(tmp.path(), false);
    a.app = None;
    execute(&fixture_configs(), "my_store", &a).expect("write");
    let values = std::fs::read_to_string(tmp.path().join("helm-values.yaml")).expect("read");
    assert!(values.contains("\"my-store-slo\""), "{values}");
}

#[test]
fn default_app_prefers_deploy_app_name_then_package_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"pkg-name\"\nversion = \"0.1.0\"\n",
    )
    .expect("write");
    let mut config = autumn_web::config::AutumnConfig::default();
    assert_eq!(default_app(&config, tmp.path()), "pkg-name");
    config.deploy = Some(autumn_web::config::DeployConfig {
        app_name: Some("deploy-name".to_owned()),
        ..Default::default()
    });
    assert_eq!(default_app(&config, tmp.path()), "deploy-name");
    assert_eq!(
        default_app(
            &autumn_web::config::AutumnConfig::default(),
            &tmp.path().join("none")
        ),
        "app"
    );
}

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
    checkout.description = Some("Customers can place \"orders\".".to_owned());
    vec![
        config("availability", 99.9, SliKind::Availability, None, None),
        checkout,
        config(
            "orders-latency",
            99.0,
            SliKind::Latency,
            Some("/api/orders/{id}"),
            Some(250),
        ),
        // All routes, on a whole-second bucket.
        config("latency", 95.0, SliKind::Latency, None, Some(1_000)),
    ]
}

fn fixture_options() -> GenerateOptions {
    GenerateOptions {
        app: "shop".to_owned(),
        selector: Some("job=\"shop\"".to_owned()),
        prometheus_url: DEFAULT_PROMETHEUS_URL.to_owned(),
        out_dir: DEFAULT_OUT_DIR.to_owned(),
        rule_labels: Vec::new(),
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

/// The `slo_scope` of the fixture selector.
fn shop_scope() -> String {
    format!("{:08x}", fnv1a(r#"job="shop""#) & 0xffff_ffff)
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
fn latency_counts_slow_non_5xx_requests_as_bad() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let expr = error_ratio(&slos[2], &[], "30m");
    let count = r#"autumn_http_request_duration_seconds_count{route="/api/orders/{id}",status_class!="5xx"}[30m]"#;
    let bucket = r#"autumn_http_request_duration_seconds_bucket{route="/api/orders/{id}",status_class!="5xx",le="0.25"}[30m]"#;
    assert_eq!(
        expr,
        format!("(\n  sum(rate({count}))\n  -\n  sum(rate({bucket}))\n)\n/\nsum(rate({count}))")
    );
    // All routes: requests that match no route are not traffic.
    let all = error_ratio(&slos[3], &[], "5m");
    assert!(all.contains(r#"route!="_unmatched""#), "{all}");
}

#[test]
fn whole_second_bounds_match_the_prometheus_3_le_form() {
    assert_eq!(le_matcher(250), r#"le="0.25""#);
    assert_eq!(le_matcher(1_000), r#"le=~"1(\\.0)?""#);
    assert_eq!(le_matcher(10_000), r#"le=~"10(\\.0)?""#);
    assert_eq!(le_matcher(2_500), r#"le="2.5""#);
}

#[test]
fn long_windows_sum_the_recorded_5m_rates() {
    let rules = file(&fixture(), "prometheus-rules.yaml").to_owned();
    assert!(rules.contains("- record: autumn_slo:sli_bad:rate5m"));
    assert!(rules.contains("- record: autumn_slo:sli_total:rate5m"));
    assert!(rules.contains(
        &format!(
            r#"sum_over_time(autumn_slo:sli_bad:rate5m{{app="shop",slo_scope="{}",job="shop",slo="availability"}}[30d])"#,
            shop_scope()
        )
    ));
    assert!(!rules.contains("[30d]))"), "no raw rate over 30 days");
    assert!(!rules.contains("[3d]))"), "no raw rate over 3 days");
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
    assert!(rules.contains(
        &format!(
            r#"autumn_slo:sli_error:ratio_rate1h{{app="shop",slo_scope="{}",job="shop",slo="availability"}} > 0.0144"#,
            shop_scope()
        )
    ));
    assert!(rules.contains(
        &format!(
            r#"autumn_slo:sli_error:ratio_rate3d{{app="shop",slo_scope="{}",job="shop",slo="availability"}} > 0.001"#,
            shop_scope()
        )
    ));
}

#[test]
fn rule_files_are_valid_yaml_with_three_alerts_per_slo() {
    let generated = fixture();
    let rules: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "prometheus-rules.yaml")).expect("yaml");
    let groups = rules["groups"].as_sequence().expect("groups");
    assert_eq!(groups.len(), 4);
    for group in groups {
        let rules = group["rules"].as_sequence().expect("rules");
        let alerts = rules.iter().filter(|r| r.get("alert").is_some()).count();
        let records = rules.iter().filter(|r| r.get("record").is_some()).count();
        assert_eq!(alerts, BURN_WINDOWS.len());
        // The objective, two 5m rates, and one ratio per window.
        assert_eq!(records, RECORDING_WINDOWS.len() + 3);
        // The equality matcher of --selector labels every rule.
        for rule in rules {
            assert_eq!(rule["labels"]["job"], "shop", "{rule:?}");
        }
    }
    let crd: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "prometheus-rule.yaml")).expect("yaml");
    assert_eq!(crd["kind"], "PrometheusRule");
    assert_eq!(crd["spec"]["groups"], rules["groups"]);
}

/// `--rule-label` lets a Prometheus `ruleSelector` find the `PrometheusRule`.
#[test]
fn rule_labels_go_on_the_prometheus_rule_object_only() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let mut options = fixture_options();
    options.rule_labels = vec![
        "release=kube-prometheus-stack".to_owned(),
        "example.com/team=payments".to_owned(),
    ];
    let generated = generate(&slos, &options).expect("generate");
    let crd: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "prometheus-rule.yaml")).expect("yaml");
    let labels = &crd["metadata"]["labels"];
    assert_eq!(labels["release"], "kube-prometheus-stack");
    assert_eq!(labels["example.com/team"], "payments");
    assert_eq!(labels["app.kubernetes.io/managed-by"], "autumn");
    // Sorted, so the output does not depend on the flag order.
    let text = file(&generated, "prometheus-rule.yaml");
    assert!(
        text.find("example.com/team").expect("team") < text.find("\"release\":").expect("release"),
        "{text}"
    );
    options.rule_labels.reverse();
    assert_eq!(generate(&slos, &options).expect("generate"), generated);
    // The plain rule file has no Kubernetes object, so no labels.
    assert!(!file(&generated, "prometheus-rules.yaml").contains("kube-prometheus-stack"));
}

/// A YAML 1.1 reader (Kubernetes) reads a bare `on`, `yes` or `no` key as a
/// boolean, so user label keys are quoted.
#[test]
fn user_label_keys_are_quoted() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let mut options = fixture_options();
    options.selector = Some(r#"on="prod""#.to_owned());
    options.rule_labels = vec!["yes=1".to_owned()];
    let generated = generate(&slos, &options).expect("generate");
    for name in ["prometheus-rules.yaml", "prometheus-rule.yaml"] {
        let text = file(&generated, name);
        assert!(text.contains(r#""on": "prod""#), "{name}: {text}");
        assert!(!text.contains("\n          on:"), "{name}");
    }
    let crd = file(&generated, "prometheus-rule.yaml");
    assert!(crd.contains(r#""yes": "1""#), "{crd}");
}

#[test]
fn a_bad_rule_label_is_rejected() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    for bad in [
        "release",
        "=x",
        "release=a b",
        "release=-x",
        "Bad Key=x",
        "/x=y",
        "a/b/c=d",
        &format!("release={}", "x".repeat(64)),
        &format!("{}=x", "k".repeat(64)),
        "app.kubernetes.io/managed-by=me",
        "app.kubernetes.io/name=other",
        "release=a,release=b",
    ] {
        let mut options = fixture_options();
        options.rule_labels = bad.split(',').map(str::to_owned).collect();
        assert!(generate(&slos, &options).is_err(), "{bad:?}");
    }
    let mut options = fixture_options();
    options.rule_labels = vec!["release=".to_owned()];
    assert!(generate(&slos, &options).is_ok(), "an empty value is valid");
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
    let uid = dashboard["uid"].as_str().expect("uid");
    assert!(
        uid.starts_with("autumn-slo-shop-") && uid.len() <= 40,
        "{uid}"
    );
    assert_eq!(dashboard["title"], "shop SLOs (job=\"shop\")");
    assert_eq!(dashboard["panels"].as_array().expect("panels").len(), 16);
    let ids: std::collections::BTreeSet<_> = dashboard["panels"]
        .as_array()
        .expect("panels")
        .iter()
        .map(|p| p["id"].as_u64().expect("id"))
        .collect();
    assert_eq!(ids.len(), 16, "panel ids are unique");
}

#[test]
fn long_app_names_get_distinct_dashboard_uids() {
    let a = dashboard_uid(&format!("{}-one", "x".repeat(40)), "");
    let b = dashboard_uid(&format!("{}-two", "x".repeat(40)), "");
    assert_eq!(a.len(), 40);
    assert_ne!(a, b);
    assert_eq!(dashboard_uid("shop", ""), "autumn-slo-shop");
    // Two rule sets for one app get two dashboards.
    assert_ne!(
        dashboard_uid("shop", r#"namespace="prod""#),
        dashboard_uid("shop", r#"namespace="staging""#)
    );
}

#[test]
fn argo_queries_select_the_canary_and_divide_by_the_budget() {
    let generated = fixture();
    let argo: serde_yaml::Value =
        serde_yaml::from_str(file(&generated, "argo-analysis-template.yaml")).expect("yaml");
    assert_eq!(argo["kind"], "AnalysisTemplate");
    assert_eq!(argo["metadata"]["name"], "shop-slo");
    let metrics = argo["spec"]["metrics"].as_sequence().expect("metrics");
    assert_eq!(metrics.len(), 4);
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
        vec![
            "shop-availability",
            "shop-checkout",
            "shop-orders-latency",
            "shop-latency",
        ]
    );
    for doc in &docs {
        let query = doc["spec"]["query"].as_str().expect("query");
        assert!(query.contains("[5m]"), "{query}");
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
    for bad in [
        "{job=\"x\"}",
        "job",
        "a=\"b\"\nc",
        "job=shop",
        "job=\"shop\" # x",
        "job=\"unterminated",
        "job=\"a\"env=\"b\"",
        "slo=\"x\"",
        "route=\"/x\"",
        "job=\"{{args.x}}\"",
        "1job=\"x\"",
        "severity=\"x\"",
        "slo_scope=\"x\"",
        "job=\"a\",job=\"b\"",
        "job=\"a\u{2028}b\"",
        // PromQL decodes these (Go escapes); the YAML label would not match.
        r#"team="a\nb""#,
        r#"team="a\tb""#,
        r#"team="a\x41""#,
        r#"team="a\u0041""#,
        r#"path=~"a\.b""#,
        r#"team="a\""#,
    ] {
        let mut options = fixture_options();
        options.selector = Some(bad.to_owned());
        assert!(generate(&slos, &options).is_err(), "{bad:?}");
    }
}

#[test]
fn the_selector_is_parsed_and_normalized() {
    let parsed =
        parse_selector(r#" job="shop", path=~"a{2}" ,env!="dev",x!~"a\"b" "#).expect("valid");
    let rendered: Vec<_> = parsed.iter().map(Matcher::render).collect();
    assert_eq!(
        rendered,
        vec![
            r#"job="shop""#,
            r#"path=~"a{2}""#,
            r#"env!="dev""#,
            r#"x!~"a\"b""#
        ]
    );
    assert_eq!(parse_selector("").expect("empty"), Vec::new());
}

#[test]
fn only_quote_and_backslash_escapes_are_kept_and_decoded() {
    let parsed = parse_selector(r#"team="a\"b\\c",path=~"x\\.y""#).expect("valid");
    assert_eq!(parsed[0].value, r#"a\"b\\c"#);
    assert_eq!(unescape(&parsed[0].value), r#"a"b\c"#);
    assert_eq!(parsed[1].render(), r#"path=~"x\\.y""#);
}

#[test]
fn rule_sets_with_different_selectors_never_share_series() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let render = |selector: Option<&str>| {
        let mut options = fixture_options();
        options.selector = selector.map(str::to_owned);
        file(
            &generate(&slos, &options).expect("generate"),
            "prometheus-rules.yaml",
        )
        .to_owned()
    };
    let scope_of = |rules: &str| {
        rules
            .lines()
            .find_map(|l| l.trim().strip_prefix("slo_scope: "))
            .expect("scope")
            .to_owned()
    };
    let prod = render(None);
    let staging = render(Some(r#"namespace="staging""#));
    assert_ne!(scope_of(&prod), scope_of(&staging));
    // Every consumer of a recorded series matches the scope.
    for rules in [&prod, &staging] {
        let scope = scope_of(rules);
        for line in rules
            .lines()
            .filter(|l| l.contains("autumn_slo:") && l.contains('{'))
        {
            assert!(line.contains(&format!("slo_scope={scope}")), "{line}");
        }
    }
}

#[test]
fn equality_values_are_unescaped_for_labels() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let mut options = fixture_options();
    options.selector = Some(r#"team="a\"b""#.to_owned());
    let rules = file(
        &generate(&slos, &options).expect("generate"),
        "prometheus-rules.yaml",
    )
    .to_owned();
    assert!(
        rules.contains(r#""team": "a\"b""#),
        "the YAML label is unescaped"
    );
    assert!(
        rules.contains(r#"team="a\"b""#),
        "the PromQL matcher stays escaped"
    );
}

#[test]
fn the_comment_dir_is_relative_and_normalized() {
    assert_eq!(comment_dir("deploy/slo"), "deploy/slo");
    assert_eq!(comment_dir("./deploy/slo/"), "deploy/slo");
    assert_eq!(comment_dir("/home/ci/work/deploy/slo"), "<out-dir>");
    assert_eq!(comment_dir("C:\\work\\slo"), "<out-dir>");
    // Rooted on every OS, not only where `Path::is_absolute` says so.
    assert_eq!(comment_dir("\\work\\slo"), "<out-dir>");
    assert_eq!(comment_dir("./"), ".");
}

#[test]
fn the_helm_values_comment_follows_the_out_dir() {
    let slos = slo::validate(&fixture_configs()).expect("valid");
    let mut options = fixture_options();
    options.out_dir = "ops/slo/".to_owned();
    let generated = generate(&slos, &options).expect("generate");
    assert!(file(&generated, "helm-values.yaml").contains("-f ops/slo/helm-values.yaml"));
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
}

#[test]
fn slos_that_read_the_histogram_warn_about_it() {
    let warnings = fixture().warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("SLO checkout, orders-latency, latency reads the"),
        "{warnings:?}"
    );
    let slos =
        slo::validate(&[config("a", 99.9, SliKind::Availability, None, None)]).expect("valid");
    assert!(
        generate(&slos, &fixture_options())
            .expect("generate")
            .warnings
            .is_empty()
    );
}

fn args(dir: &Path, check: bool) -> GenerateArgs {
    GenerateArgs {
        out_dir: dir.to_path_buf(),
        app: Some("shop".to_owned()),
        selector: Some("job=\"shop\"".to_owned()),
        prometheus_url: DEFAULT_PROMETHEUS_URL.to_owned(),
        rule_labels: Vec::new(),
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
        std::fs::read_to_string(dir.join("prometheus-rules.yaml")).expect("read"),
        file(&fixture(), "prometheus-rules.yaml")
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

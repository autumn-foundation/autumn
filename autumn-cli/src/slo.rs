//! `autumn slo generate`: SLO-as-code (issue #3069).
//!
//! Reads the `[[slo]]` tables in `autumn.toml` and writes:
//!
//! | File | Use |
//! |------|-----|
//! | `prometheus-rules.yaml` | Prometheus rule file: recording rules and multiwindow burn-rate alerts |
//! | `prometheus-rule.yaml` | The same rules as a prometheus-operator `PrometheusRule` |
//! | `grafana-dashboard.json` | Grafana dashboard: SLI, error budget left, burn rate |
//! | `argo-analysis-template.yaml` | Argo Rollouts `AnalysisTemplate` |
//! | `flagger-metric-templates.yaml` | Flagger `MetricTemplate`s |
//! | `helm-values.yaml` | Values that connect the Helm chart to the templates |
//!
//! The output is deterministic: no timestamps, and a fixed order. Thresholds
//! come from integer ppm math, so they are exact.
//!
//! The queries read these series:
//!
//! - `autumn_http_responses_total{status="5xx"}`: availability for all routes.
//! - `autumn_http_request_duration_seconds_{count,bucket}`: availability for
//!   one route, and latency. This histogram comes from issue #3064.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use autumn_web::slo::{self, BURN_WINDOWS, PPM, ROLLBACK_BURN_TENTHS, Sli, Slo, format_decimal};
use serde::Serialize;

/// The windows for which a recording rule exists.
pub const RECORDING_WINDOWS: [&str; 6] = ["5m", "30m", "1h", "6h", "3d", "30d"];

/// The window of the rollout analysis queries (Argo Rollouts).
const ANALYSIS_WINDOW: &str = "5m";

/// The default output directory.
pub const DEFAULT_OUT_DIR: &str = "deploy/slo";

/// The default Prometheus address in the analysis templates.
pub const DEFAULT_PROMETHEUS_URL: &str = "http://prometheus.monitoring.svc:9090";

/// Settings for one generator run.
#[derive(Debug, Clone)]
pub struct GenerateOptions {
    /// Application name. It goes into labels and Kubernetes object names.
    pub app: String,
    /// Extra label matchers for every query, for example `job="shop"`.
    pub selector: Option<String>,
    /// Prometheus address for the analysis templates.
    pub prometheus_url: String,
}

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// The file name in the output directory.
    pub name: &'static str,
    /// The file contents.
    pub contents: String,
}

/// The result of a generator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// The files, in a fixed order.
    pub files: Vec<GeneratedFile>,
    /// Problems that do not stop the run.
    pub warnings: Vec<String>,
}

/// Generate every SLO artifact.
///
/// # Errors
///
/// Returns an error when there is no SLO, when the app name is not a valid
/// Kubernetes name, or when the selector is not valid.
pub fn generate(slos: &[Slo], options: &GenerateOptions) -> Result<Generated, String> {
    if slos.is_empty() {
        return Err(
            "autumn.toml has no [[slo]] tables. Add one, for example:\n\n\
             [[slo]]\nname = \"availability\"\nobjective = 99.9\nsli = \"availability\"\n"
                .to_owned(),
        );
    }
    let app = kube_name(&options.app)?;
    let selector = check_selector(options.selector.as_deref())?;
    for slo in slos {
        let name = metric_template_name(&app, slo);
        if name.len() > 63 {
            return Err(format!(
                "the object name {name:?} is longer than 63 characters; \
                 use a shorter --app or SLO name"
            ));
        }
    }
    let ctx = Context {
        app,
        selector,
        prometheus_url: options.prometheus_url.clone(),
    };
    let files = vec![
        GeneratedFile {
            name: "prometheus-rules.yaml",
            contents: render_rule_file(slos, &ctx),
        },
        GeneratedFile {
            name: "prometheus-rule.yaml",
            contents: render_prometheus_rule(slos, &ctx),
        },
        GeneratedFile {
            name: "grafana-dashboard.json",
            contents: render_dashboard(slos, &ctx),
        },
        GeneratedFile {
            name: "argo-analysis-template.yaml",
            contents: render_argo(slos, &ctx),
        },
        GeneratedFile {
            name: "flagger-metric-templates.yaml",
            contents: render_flagger(slos, &ctx),
        },
        GeneratedFile {
            name: "helm-values.yaml",
            contents: render_helm_values(slos, &ctx),
        },
    ];
    Ok(Generated {
        files,
        warnings: warnings(slos),
    })
}

struct Context {
    app: String,
    selector: Option<String>,
    prometheus_url: String,
}

/// Convert a name to a DNS-1123 label: lowercase, `_` to `-`.
///
/// # Errors
///
/// Returns an error when the result is empty, too long, or has other
/// characters.
pub fn kube_name(raw: &str) -> Result<String, String> {
    let name = raw.trim().to_ascii_lowercase().replace('_', "-");
    let valid = !name.is_empty()
        && name.len() <= 50
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(name)
    } else {
        Err(format!(
            "app name {raw:?} is not a valid Kubernetes name; use --app with 1 to 50 \
             lowercase letters, digits or '-', starting with a letter"
        ))
    }
}

fn check_selector(selector: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if raw.contains(['{', '}', '\n', '\r']) || !raw.contains('=') {
        return Err(format!(
            "--selector {raw:?} is not valid; give label matchers without braces, \
             for example 'job=\"shop\"'"
        ));
    }
    Ok(Some(raw.trim_end_matches(',').to_owned()))
}

fn warnings(slos: &[Slo]) -> Vec<String> {
    let mut out = Vec::new();
    for slo in slos {
        // A burn rate above 1/budget needs an error ratio above 100 %.
        let fast = u64::from(slo.budget_ppm()) * u64::from(BURN_WINDOWS[0].factor_tenths) / 10;
        if fast >= u64::from(PPM) {
            out.push(format!(
                "SLO {}: the error budget is so large that the {}x burn-rate page can never \
                 fire. Use a higher objective, or rely on the slower alerts.",
                slo.name,
                format_decimal(u64::from(BURN_WINDOWS[0].factor_tenths), 1),
            ));
        }
    }
    out
}

// ── PromQL ───────────────────────────────────────────────────────────────────

const RESPONSES: &str = "autumn_http_responses_total";
const DURATION_COUNT: &str = "autumn_http_request_duration_seconds_count";
const DURATION_BUCKET: &str = "autumn_http_request_duration_seconds_bucket";

/// The name of the recording rule for one window.
#[must_use]
pub fn recording_name(window: &str) -> String {
    format!("autumn_slo:sli_error:ratio_rate{window}")
}

/// Join label matchers into a series selector.
fn series(metric: &str, matchers: &[&str]) -> String {
    let parts: Vec<&str> = matchers.iter().copied().filter(|m| !m.is_empty()).collect();
    if parts.is_empty() {
        metric.to_owned()
    } else {
        format!("{metric}{{{}}}", parts.join(","))
    }
}

/// The error ratio of `slo` over `window`, as a Prometheus query.
///
/// `extra` holds label matchers added to every series.
#[must_use]
pub fn error_ratio(slo: &Slo, extra: &[&str], window: &str) -> String {
    let route = slo.route().map(|r| format!("route=\"{r}\""));
    let route = route.as_deref().unwrap_or("");
    match &slo.sli {
        Sli::Availability { route: None } => {
            let mut bad = vec!["status=\"5xx\""];
            bad.extend_from_slice(extra);
            format!(
                "(sum(rate({}[{window}])) or vector(0))\n/\nsum(rate({}[{window}]))",
                series(RESPONSES, &bad),
                series(RESPONSES, extra),
            )
        }
        Sli::Availability { route: Some(_) } => {
            let mut bad = vec![route, "status_class=\"5xx\""];
            bad.extend_from_slice(extra);
            let mut all = vec![route];
            all.extend_from_slice(extra);
            format!(
                "(sum(rate({}[{window}])) or vector(0))\n/\nsum(rate({}[{window}]))",
                series(DURATION_COUNT, &bad),
                series(DURATION_COUNT, &all),
            )
        }
        Sli::Latency { threshold_ms, .. } => {
            let le = format!("le=\"{}\"", format_decimal(*threshold_ms, 3));
            let mut good = vec![route, le.as_str()];
            good.extend_from_slice(extra);
            let mut all = vec![route];
            all.extend_from_slice(extra);
            format!(
                "1 - (\n  sum(rate({}[{window}]))\n  /\n  sum(rate({}[{window}]))\n)",
                series(DURATION_BUCKET, &good),
                series(DURATION_COUNT, &all),
            )
        }
    }
}

fn ratio(ppm: u64) -> String {
    format_decimal(ppm, 6)
}

/// The error ratio, as text, at which `slo` burns at `factor_tenths`.
fn burn_threshold(slo: &Slo, factor_tenths: u32) -> String {
    // budget_ppm * factor_tenths / 10 / 1e6, exact.
    format_decimal(u64::from(slo.budget_ppm()) * u64::from(factor_tenths), 7)
}

fn sli_text(slo: &Slo) -> String {
    let scope = slo
        .route()
        .map_or_else(|| "all routes".to_owned(), |route| format!("route {route}"));
    match &slo.sli {
        Sli::Availability { .. } => format!("non-5xx responses, {scope}"),
        Sli::Latency { threshold_ms, .. } => {
            format!("requests faster than {threshold_ms} ms, {scope}")
        }
    }
}

/// A YAML double-quoted scalar. JSON string syntax is valid YAML.
fn yaml_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Write `text` as a YAML block scalar body at `indent` spaces.
fn block(out: &mut String, indent: usize, text: &str) {
    for line in text.lines() {
        let _ = writeln!(out, "{:indent$}{line}", "");
    }
}

fn header(out: &mut String) {
    out.push_str(
        "# Generated by `autumn slo generate` from the [[slo]] tables in autumn.toml.\n\
         # Do not edit. Run the command again after you change an SLO.\n",
    );
}

// ── Prometheus rules ─────────────────────────────────────────────────────────

/// Write the `groups:` list at `indent` spaces.
fn render_groups(out: &mut String, slos: &[Slo], ctx: &Context, indent: usize) {
    let pad = " ".repeat(indent);
    let _ = writeln!(out, "{pad}groups:");
    let selector = ctx.selector.as_deref().unwrap_or("");
    for slo in slos {
        let labels = |out: &mut String, extra: &[(&str, &str)]| {
            let _ = writeln!(out, "{pad}        labels:");
            let _ = writeln!(out, "{pad}          app: {}", yaml_str(&ctx.app));
            let _ = writeln!(out, "{pad}          slo: {}", yaml_str(&slo.name));
            for (key, value) in extra {
                let _ = writeln!(out, "{pad}          {key}: {}", yaml_str(value));
            }
        };
        let _ = writeln!(
            out,
            "{pad}  - name: {}",
            yaml_str(&format!("autumn-slo-{}-{}", ctx.app, slo.name))
        );
        let _ = writeln!(out, "{pad}    rules:");
        let _ = writeln!(out, "{pad}      - record: autumn_slo:objective:ratio");
        let _ = writeln!(
            out,
            "{pad}        expr: vector({})",
            ratio(u64::from(slo.objective_ppm))
        );
        labels(out, &[]);
        for window in RECORDING_WINDOWS {
            let _ = writeln!(out, "{pad}      - record: {}", recording_name(window));
            let _ = writeln!(out, "{pad}        expr: |-");
            block(out, indent + 10, &error_ratio(slo, &[selector], window));
            labels(out, &[]);
        }
        let own = format!("app=\"{}\",slo=\"{}\"", ctx.app, slo.name);
        for window in BURN_WINDOWS {
            let threshold = burn_threshold(slo, window.factor_tenths);
            let factor = format_decimal(u64::from(window.factor_tenths), 1);
            let _ = writeln!(out, "{pad}      - alert: AutumnSloErrorBudgetBurn");
            let _ = writeln!(out, "{pad}        expr: |-");
            let expr = format!(
                "{long}{{{own}}} > {threshold}\nand\n{short}{{{own}}} > {threshold}",
                long = recording_name(window.long),
                short = recording_name(window.short),
            );
            block(out, indent + 10, &expr);
            labels(
                out,
                &[
                    ("severity", window.severity),
                    ("long_window", window.long),
                    ("short_window", window.short),
                ],
            );
            let _ = writeln!(out, "{pad}        annotations:");
            let _ = writeln!(
                out,
                "{pad}          summary: {}",
                yaml_str(&format!(
                    "{} SLO {} burns its error budget at {factor}x or more",
                    ctx.app, slo.name
                ))
            );
            let objective = format_decimal(u64::from(slo.objective_ppm), 4);
            let mut description = format!(
                "Objective {objective}% ({}). The error ratio is above {threshold} over \
                 {} and {}.",
                sli_text(slo),
                window.long,
                window.short,
            );
            if let Some(extra) = &slo.description {
                description.push(' ');
                description.push_str(extra);
            }
            let _ = writeln!(
                out,
                "{pad}          description: {}",
                yaml_str(&description)
            );
        }
    }
}

fn render_rule_file(slos: &[Slo], ctx: &Context) -> String {
    let mut out = String::new();
    header(&mut out);
    render_groups(&mut out, slos, ctx, 0);
    out
}

fn object_labels(out: &mut String, app: &str) {
    out.push_str("  labels:\n");
    let _ = writeln!(out, "    app.kubernetes.io/name: {}", yaml_str(app));
    out.push_str("    app.kubernetes.io/managed-by: autumn\n");
}

fn render_prometheus_rule(slos: &[Slo], ctx: &Context) -> String {
    let mut out = String::new();
    header(&mut out);
    out.push_str("apiVersion: monitoring.coreos.com/v1\nkind: PrometheusRule\nmetadata:\n");
    let _ = writeln!(out, "  name: {}", yaml_str(&format!("{}-slo", ctx.app)));
    object_labels(&mut out, &ctx.app);
    out.push_str("spec:\n");
    render_groups(&mut out, slos, ctx, 2);
    out
}

// ── Rollout analysis ─────────────────────────────────────────────────────────

/// The Flagger `MetricTemplate` name for `slo`.
#[must_use]
pub fn metric_template_name(app: &str, slo: &Slo) -> String {
    format!("{app}-{}", slo.name)
}

/// The burn rate of `slo` as a Prometheus query: error ratio / budget.
fn burn_rate(slo: &Slo, extra: &[&str], window: &str) -> String {
    format!(
        "(\n{}\n)\n/ {}",
        indent_lines(&error_ratio(slo, extra, window), 2),
        ratio(u64::from(slo.budget_ppm()))
    )
}

fn indent_lines(text: &str, by: usize) -> String {
    text.lines()
        .map(|line| format!("{:by$}{line}", ""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn max_burn_text() -> String {
    format_decimal(u64::from(ROLLBACK_BURN_TENTHS), 1)
}

fn render_argo(slos: &[Slo], ctx: &Context) -> String {
    let mut out = String::new();
    header(&mut out);
    out.push_str(
        "# The Helm chart sets AUTUMN_DEPLOY_VERSION to the pod-template hash, so the\n\
         # `version` label tells canary pods from stable pods.\n",
    );
    out.push_str("apiVersion: argoproj.io/v1alpha1\nkind: AnalysisTemplate\nmetadata:\n");
    let _ = writeln!(out, "  name: {}", yaml_str(&format!("{}-slo", ctx.app)));
    object_labels(&mut out, &ctx.app);
    out.push_str("spec:\n  args:\n    - name: canary-hash\n    - name: prometheus-address\n");
    let _ = writeln!(out, "      value: {}", yaml_str(&ctx.prometheus_url));
    out.push_str("  metrics:\n");
    let canary = "version=\"{{args.canary-hash}}\"";
    let selector = ctx.selector.as_deref().unwrap_or("");
    let max = max_burn_text();
    for slo in slos {
        let _ = writeln!(out, "    - name: {}", yaml_str(&slo.name));
        out.push_str("      interval: 1m\n      failureLimit: 1\n");
        let _ = writeln!(
            out,
            "      successCondition: {}",
            yaml_str(&format!(
                "len(result) == 0 || isNaN(result[0]) || result[0] <= {max}"
            ))
        );
        out.push_str(
            "      provider:\n        prometheus:\n          address: \"{{args.prometheus-address}}\"\n          query: |-\n",
        );
        block(
            &mut out,
            12,
            &burn_rate(slo, &[canary, selector], ANALYSIS_WINDOW),
        );
    }
    out
}

fn render_flagger(slos: &[Slo], ctx: &Context) -> String {
    let mut out = String::new();
    header(&mut out);
    out.push_str(
        "# Each query returns the canary burn rate. The Helm chart fails the canary\n\
         # above analysis.maxBurnRate.\n",
    );
    let canary = "namespace=\"{{ namespace }}\",pod=~\"{{ target }}-[0-9a-zA-Z]+(-[0-9a-zA-Z]+)\"";
    let selector = ctx.selector.as_deref().unwrap_or("");
    for (index, slo) in slos.iter().enumerate() {
        if index > 0 {
            out.push_str("---\n");
        }
        out.push_str("apiVersion: flagger.app/v1beta1\nkind: MetricTemplate\nmetadata:\n");
        let _ = writeln!(
            out,
            "  name: {}",
            yaml_str(&metric_template_name(&ctx.app, slo))
        );
        object_labels(&mut out, &ctx.app);
        out.push_str("spec:\n  provider:\n    type: prometheus\n");
        let _ = writeln!(out, "    address: {}", yaml_str(&ctx.prometheus_url));
        out.push_str("  query: |-\n");
        block(
            &mut out,
            4,
            &burn_rate(slo, &[canary, selector], "{{ interval }}"),
        );
    }
    out
}

fn render_helm_values(slos: &[Slo], ctx: &Context) -> String {
    let mut out = String::new();
    header(&mut out);
    let _ = writeln!(
        out,
        "# Use it with: helm upgrade --install {app} deploy/helm -f {DEFAULT_OUT_DIR}/helm-values.yaml",
        app = ctx.app
    );
    out.push_str("analysis:\n");
    let _ = writeln!(
        out,
        "  templateName: {}",
        yaml_str(&format!("{}-slo", ctx.app))
    );
    let _ = writeln!(out, "  maxBurnRate: {}", max_burn_text());
    out.push_str("  metricTemplates:\n");
    for slo in slos {
        let _ = writeln!(
            out,
            "    - {}",
            yaml_str(&metric_template_name(&ctx.app, slo))
        );
    }
    out
}

// ── Grafana dashboard ────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Dashboard {
    uid: String,
    title: String,
    tags: Vec<&'static str>,
    timezone: &'static str,
    schema_version: u32,
    version: u32,
    editable: bool,
    refresh: &'static str,
    time: TimeRange,
    templating: Templating,
    panels: Vec<Panel>,
}

#[derive(Serialize)]
struct TimeRange {
    from: &'static str,
    to: &'static str,
}

#[derive(Serialize)]
struct Templating {
    list: Vec<Variable>,
}

#[derive(Serialize)]
struct Variable {
    name: &'static str,
    label: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    query: &'static str,
}

#[derive(Serialize)]
struct DataSource {
    #[serde(rename = "type")]
    kind: &'static str,
    uid: &'static str,
}

const DATASOURCE: DataSource = DataSource {
    kind: "prometheus",
    uid: "${datasource}",
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Panel {
    id: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    grid_pos: GridPos,
    #[serde(skip_serializing_if = "Option::is_none")]
    datasource: Option<DataSource>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    targets: Vec<Target>,
    #[serde(skip_serializing_if = "Option::is_none")]
    field_config: Option<FieldConfig>,
}

#[derive(Serialize)]
struct GridPos {
    h: u32,
    w: u32,
    x: u32,
    y: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    ref_id: &'static str,
    expr: String,
    legend_format: &'static str,
    datasource: DataSource,
}

#[derive(Serialize)]
struct FieldConfig {
    defaults: FieldDefaults,
}

#[derive(Serialize)]
struct FieldDefaults {
    unit: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    decimals: Option<u32>,
}

const fn target(ref_id: &'static str, expr: String, legend_format: &'static str) -> Target {
    Target {
        ref_id,
        expr,
        legend_format,
        datasource: DATASOURCE,
    }
}

/// The four panels of one SLO row, starting at grid row `y`.
fn slo_panels(slo: &Slo, ctx: &Context, y: u32, first_id: u32) -> [Panel; 4] {
    let own = format!("app=\"{}\",slo=\"{}\"", ctx.app, slo.name);
    let objective = format_decimal(u64::from(slo.objective_ppm), 4);
    let budget = format!("(1 - autumn_slo:objective:ratio{{{own}}})");
    let burn = |window: &str| format!("{}{{{own}}} / {budget}", recording_name(window));
    let stat = |id, title: &str, x, expr, legend, decimals| Panel {
        id,
        kind: "stat",
        title: title.to_owned(),
        description: None,
        grid_pos: GridPos {
            h: 8,
            w: 6,
            x,
            y: y + 1,
        },
        datasource: Some(DATASOURCE),
        targets: vec![target("A", expr, legend)],
        field_config: Some(FieldConfig {
            defaults: FieldDefaults {
                unit: "percentunit",
                decimals: Some(decimals),
            },
        }),
    };
    let mut sli = stat(
        first_id + 1,
        "SLI (30d)",
        0,
        format!("1 - {}{{{own}}}", recording_name("30d")),
        "SLI",
        3,
    );
    sli.description.clone_from(&slo.description);
    [
        Panel {
            id: first_id,
            kind: "row",
            title: format!("{} — objective {objective}% ({})", slo.name, sli_text(slo)),
            description: None,
            grid_pos: GridPos {
                h: 1,
                w: 24,
                x: 0,
                y,
            },
            datasource: None,
            targets: Vec::new(),
            field_config: None,
        },
        sli,
        stat(
            first_id + 2,
            "Error budget left (30d)",
            6,
            format!("1 - {}", burn("30d")),
            "budget left",
            1,
        ),
        Panel {
            id: first_id + 3,
            kind: "timeseries",
            title: "Burn rate".to_owned(),
            description: Some(format!(
                "1 spends the budget in exactly 30 days. A page fires at {}x (1h and 5m) \
                 and at 6x (6h and 30m).",
                format_decimal(u64::from(BURN_WINDOWS[0].factor_tenths), 1)
            )),
            grid_pos: GridPos {
                h: 8,
                w: 12,
                x: 12,
                y: y + 1,
            },
            datasource: Some(DATASOURCE),
            targets: vec![
                target("A", burn("1h"), "1h"),
                target("B", burn("6h"), "6h"),
                target("C", burn("3d"), "3d"),
            ],
            field_config: Some(FieldConfig {
                defaults: FieldDefaults {
                    unit: "none",
                    decimals: None,
                },
            }),
        },
    ]
}

fn render_dashboard(slos: &[Slo], ctx: &Context) -> String {
    let panels = slos
        .iter()
        .zip(0u32..)
        .flat_map(|(slo, index)| slo_panels(slo, ctx, index * 9, index * 4 + 1))
        .collect();
    let dashboard = Dashboard {
        uid: format!("autumn-slo-{}", ctx.app).chars().take(40).collect(),
        title: format!("{} SLOs", ctx.app),
        tags: vec!["autumn", "slo"],
        timezone: "browser",
        schema_version: 39,
        version: 1,
        editable: true,
        refresh: "1m",
        time: TimeRange {
            from: "now-7d",
            to: "now",
        },
        templating: Templating {
            list: vec![Variable {
                name: "datasource",
                label: "Data source",
                kind: "datasource",
                query: "prometheus",
            }],
        },
        panels,
    };
    let mut text = serde_json::to_string_pretty(&dashboard).unwrap_or_default();
    text.push('\n');
    text
}

// ── Command ──────────────────────────────────────────────────────────────────

/// Arguments of `autumn slo generate`.
#[derive(Debug, Clone)]
pub struct GenerateArgs {
    /// Output directory.
    pub out_dir: PathBuf,
    /// Application name override.
    pub app: Option<String>,
    /// Extra label matchers.
    pub selector: Option<String>,
    /// Prometheus address for the analysis templates.
    pub prometheus_url: String,
    /// Compare with the files on disk; write nothing.
    pub check: bool,
}

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Files were written.
    Written(Vec<PathBuf>),
    /// `--check` found no drift.
    UpToDate,
}

/// Generate the files and write or check them.
///
/// # Errors
///
/// Returns an error for a bad SLO, a bad option, an I/O failure, or (with
/// `check`) a file on disk that is missing or different.
pub fn execute(
    configs: &[slo::SloConfig],
    default_app: &str,
    args: &GenerateArgs,
) -> Result<(Outcome, Vec<String>), String> {
    let slos = slo::validate(configs).map_err(|e| e.to_string())?;
    let options = GenerateOptions {
        app: args.app.clone().unwrap_or_else(|| default_app.to_owned()),
        selector: args.selector.clone(),
        prometheus_url: args.prometheus_url.clone(),
    };
    let generated = generate(&slos, &options)?;
    if args.check {
        let drifted: Vec<String> = generated
            .files
            .iter()
            .filter(|file| {
                std::fs::read_to_string(args.out_dir.join(file.name))
                    .map_or(true, |on_disk| on_disk != file.contents)
            })
            .map(|file| args.out_dir.join(file.name).display().to_string())
            .collect();
        if drifted.is_empty() {
            return Ok((Outcome::UpToDate, generated.warnings));
        }
        return Err(format!(
            "these SLO files are missing or out of date:\n  {}\nRun `autumn slo generate` \
             and commit the result.",
            drifted.join("\n  ")
        ));
    }
    std::fs::create_dir_all(&args.out_dir)
        .map_err(|e| format!("cannot create {}: {e}", args.out_dir.display()))?;
    let mut written = Vec::new();
    for file in &generated.files {
        let path = args.out_dir.join(file.name);
        std::fs::write(&path, &file.contents)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        written.push(path);
    }
    Ok((Outcome::Written(written), generated.warnings))
}

/// The default app name: `[deploy] app_name`, else the package name.
fn default_app(config: &autumn_web::config::AutumnConfig, project: &Path) -> String {
    if let Some(name) = config
        .deploy
        .as_ref()
        .and_then(|d| d.app_name.as_deref())
        .filter(|n| !n.trim().is_empty())
    {
        return name.to_owned();
    }
    std::fs::read_to_string(project.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            table
                .get("package")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "app".to_owned())
}

/// Run `autumn slo generate`.
pub fn run_generate(args: &GenerateArgs) {
    let config = match autumn_web::config::AutumnConfig::load_lenient_unknown_roots() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error: failed to load configuration: {e}");
            std::process::exit(1);
        }
    };
    let app = default_app(&config, Path::new("."));
    match execute(&config.slo, &app, args) {
        Ok((outcome, warnings)) => {
            for warning in warnings {
                eprintln!("Warning: {warning}");
            }
            match outcome {
                Outcome::Written(paths) => {
                    for path in paths {
                        println!("wrote {}", path.display());
                    }
                }
                Outcome::UpToDate => println!("SLO files are up to date."),
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests;

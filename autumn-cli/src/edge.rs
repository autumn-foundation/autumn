//! `autumn edge serve` and `autumn edge ttfb` (issue #1790).
//!
//! `serve` runs the reference edge node: the capsule in front of a remote
//! origin. `ttfb` measures time to first byte at the node and at the origin
//! and compares the bytes. The logic is in `autumn_edge::node`; this module
//! reads the arguments and prints.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use autumn_edge::InMemoryEdgeKv;
use autumn_edge::gateway::{EdgeGateway, Lane, check_response_header};
use autumn_edge::host::EdgeArtifact;
use autumn_edge::node::{self, AccessEntry, EdgeNode, HttpOrigin, TrustedProxy, ttfb};
use autumn_edge::reexports::http::{HeaderName, HeaderValue};

/// Options for `autumn edge serve`.
pub struct ServeOptions<'a> {
    pub capsule: &'a str,
    pub origin: &'a str,
    pub listen: &'a str,
    pub kv: Option<&'a str>,
    pub probe_path: &'a str,
    pub no_probe: bool,
    pub response_headers: &'a [String],
    pub trusted_proxies: &'a [String],
    pub quiet: bool,
}

/// Options for `autumn edge ttfb`.
pub struct TtfbOptions<'a> {
    pub edge: &'a str,
    pub origin: &'a str,
    pub paths: &'a [String],
    pub rounds: usize,
    pub min_reduction: f64,
    pub divergence_only: bool,
}

/// Exit code for a setup error, or a failed probe.
const EXIT_FAIL: i32 = 1;
/// Exit code for a probe that could not run.
const EXIT_ERROR: i32 = 2;

/// `name: value` as a header pair the gateway accepts.
fn parse_response_header(raw: &str) -> Result<(HeaderName, HeaderValue), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| format!("`{raw}` is not `name: value`"))?;
    let name = HeaderName::from_bytes(name.trim().as_bytes())
        .map_err(|_| format!("`{}` is not a header name", name.trim()))?;
    check_response_header(&name)?;
    let value = HeaderValue::from_str(value.trim())
        .map_err(|_| format!("the value of `{name}` is not a valid header value"))?;
    Ok((name, value))
}

/// The `--trusted-proxy` values.
fn parse_trusted_proxies(raw: &[String]) -> Result<Vec<TrustedProxy>, String> {
    raw.iter()
        .map(|proxy| TrustedProxy::parse(proxy).map_err(|err| err.to_string()))
        .collect()
}

/// A JSON object of string values, as the `kv` store.
fn parse_kv(json: &str) -> Result<BTreeMap<String, String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|err| format!("not JSON: {err}"))?;
    let object = value
        .as_object()
        .ok_or("must be a JSON object of string values")?;
    object
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|text| (key.clone(), text.to_owned()))
                .ok_or_else(|| format!("the value of `{key}` must be a string"))
        })
        .collect()
}

/// A finite number of percent, for `--min-reduction`.
pub fn parse_percent(raw: &str) -> Result<f64, String> {
    raw.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("`{raw}` is not a finite number"))
}

/// The lane as a short word for the access log.
fn lane_label(lane: Option<Lane>) -> String {
    match lane {
        Some(Lane::Edge) => "edge".to_owned(),
        Some(Lane::Fallthrough(reason)) => format!("origin ({reason})"),
        Some(Lane::OriginOnly) => "origin (origin_only)".to_owned(),
        None => "node_error".to_owned(),
    }
}

/// Print `message` and exit with `code`.
fn fail(code: i32, message: &str) -> ! {
    eprintln!("\u{2717} {message}");
    std::process::exit(code);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new()
        .unwrap_or_else(|err| fail(EXIT_ERROR, &format!("could not start the runtime: {err}")))
}

/// Run `autumn edge serve`. Exits 1 on a setup error, 2 when the runtime
/// does not start. Stops on Ctrl-C or SIGTERM.
pub fn serve(options: &ServeOptions<'_>) {
    let wasm = std::fs::read(options.capsule).unwrap_or_else(|err| {
        fail(
            EXIT_FAIL,
            &format!(
                "cannot read the capsule `{}`: {err}\n  Run `autumn build` first, or give \
                 the path with --capsule.",
                options.capsule
            ),
        )
    });
    let artifact = EdgeArtifact::from_bytes(&wasm).unwrap_or_else(|err| {
        fail(
            EXIT_FAIL,
            &format!("`{}` is not a capsule: {err}", options.capsule),
        )
    });
    let origin = HttpOrigin::new(options.origin)
        .unwrap_or_else(|err| fail(EXIT_FAIL, &format!("--origin: {err}")));

    let mut configured = Vec::new();
    for raw in options.response_headers {
        configured.push(
            parse_response_header(raw)
                .unwrap_or_else(|err| fail(EXIT_FAIL, &format!("--response-header: {err}"))),
        );
    }

    let trusted = parse_trusted_proxies(options.trusted_proxies)
        .unwrap_or_else(|err| fail(EXIT_FAIL, &format!("--trusted-proxy: {err}")));

    let runtime = runtime();
    runtime.block_on(async {
        let mut headers = Vec::new();
        if !options.no_probe {
            headers = node::origin_static_headers(options.origin, options.probe_path)
                .await
                .unwrap_or_else(|err| {
                    fail(
                        EXIT_FAIL,
                        &format!(
                            "could not read the static headers from the origin: {err}\n  \
                             Start the origin first, or pass --no-probe and set them with \
                             --response-header."
                        ),
                    )
                });
            // A header given on the command line replaces the probed one.
            headers.retain(|(name, _)| !configured.iter().any(|(given, _)| given == name));
        }
        headers.extend(configured);
        let header_count = headers.len();

        let gateway = EdgeGateway::new(Arc::new(artifact), origin).with_response_headers(headers);
        let (gateway, kv_state) = with_kv_file(gateway, options.kv);

        let mut edge_node = EdgeNode::new(gateway).with_trusted_proxies(trusted);
        if !options.quiet {
            edge_node = edge_node.with_access_log(|entry: &AccessEntry| {
                println!(
                    "{} {} {} {} {:.1}ms",
                    entry.method,
                    entry.path,
                    entry.status,
                    lane_label(entry.lane),
                    entry.elapsed.as_secs_f64() * 1000.0
                );
            });
        }

        let listener = tokio::net::TcpListener::bind(options.listen)
            .await
            .unwrap_or_else(|err| {
                fail(
                    EXIT_FAIL,
                    &format!("cannot listen on {}: {err}", options.listen),
                )
            });
        let address = listener
            .local_addr()
            .map_or_else(|_| options.listen.to_owned(), |addr| addr.to_string());
        println!(
            "\u{1F342} Edge node on http://{address} \u{2192} origin {} \
             (capsule {}, {} KB; kv: {kv_state}; {header_count} response header(s))",
            options.origin,
            options.capsule,
            wasm.len() / 1024,
        );
        run_until_stopped(listener, edge_node).await;
    });
}

/// Give the gateway the `kv` capability from `--kv`. Returns the gateway and
/// a short state for the banner.
fn with_kv_file(
    gateway: EdgeGateway<HttpOrigin>,
    kv: Option<&str>,
) -> (EdgeGateway<HttpOrigin>, String) {
    let Some(path) = kv else {
        return (gateway, "off".to_owned());
    };
    let store = load_kv(Path::new(path))
        .unwrap_or_else(|err| fail(EXIT_FAIL, &format!("--kv {path}: {err}")));
    let count = store.len();
    let kv = store
        .into_iter()
        .fold(InMemoryEdgeKv::new(), |kv, (key, value)| {
            kv.with(key, value)
        });
    (gateway.with_kv(Arc::new(kv)), format!("{count} key(s)"))
}

/// Serve until Ctrl-C or SIGTERM, then give open requests [`DRAIN`].
async fn run_until_stopped(listener: tokio::net::TcpListener, edge_node: EdgeNode<HttpOrigin>) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = node::serve(listener, edge_node, async {
        let _ = stopped.await;
    });
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result,
        () = shutdown_signal() => {
            let _ = stop.send(());
            println!("Stopping: open requests have {} s to finish.", DRAIN.as_secs());
            tokio::time::timeout(DRAIN, server).await.unwrap_or(Ok(()))
        }
    };
    if let Err(err) = result {
        fail(EXIT_FAIL, &format!("the edge node stopped: {err}"));
    }
}

/// How long open requests can run after a stop signal.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

/// Ctrl-C, or SIGTERM on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn load_kv(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let json = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    parse_kv(&json)
}

fn millis(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Run `autumn edge ttfb`. Exits 0 on a pass, 1 on a fail, 2 on an error.
pub fn ttfb(options: &TtfbOptions<'_>) {
    let probe = ttfb::Probe {
        edge: options.edge.to_owned(),
        origin: options.origin.to_owned(),
        paths: options.paths.to_vec(),
        rounds: options.rounds,
    };
    let report = runtime()
        .block_on(ttfb::measure(&probe))
        .unwrap_or_else(|err| fail(EXIT_ERROR, &err.to_string()));

    println!(
        "TTFB: {} request(s) per side\n\n             median        p90",
        report.edge.samples.len()
    );
    for (side, summary) in [("edge", &report.edge), ("origin", &report.origin)] {
        println!(
            "  {side:<8} {:>9.1} ms {:>9.1} ms",
            millis(summary.median()),
            millis(summary.p90())
        );
    }
    let minimum = if options.divergence_only {
        "not checked".to_owned()
    } else {
        format!("minimum {:.1}%", options.min_reduction)
    };
    println!(
        "\n  reduction  {:.1}% ({minimum})\n  divergences {}",
        report.reduction_percent(),
        report.divergences.len()
    );
    for divergence in report.divergences.iter().take(10) {
        println!("    {divergence}");
    }

    match judge(&report, options.min_reduction, options.divergence_only) {
        Ok(()) => println!("\n\u{2713} pass"),
        Err(reason) => fail(EXIT_FAIL, &format!("fail: {reason}")),
    }
}

/// `Ok` when the report passes. `Err` gives the reason.
fn judge(report: &ttfb::Report, min_reduction: f64, divergence_only: bool) -> Result<(), String> {
    if !report.divergences.is_empty() {
        return Err(format!(
            "{} pair(s) diverged; the edge and the origin must send the same bytes",
            report.divergences.len()
        ));
    }
    if !divergence_only && !report.passes(min_reduction) {
        return Err(format!(
            "the median TTFB reduction is {:.1}%; the minimum is {:.1}%",
            report.reduction_percent(),
            min_reduction
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_edge::FallthroughReason;

    #[test]
    fn a_response_header_is_name_colon_value() {
        let (name, value) = parse_response_header("X-Frame-Options: DENY").unwrap();
        assert_eq!(name, "x-frame-options");
        assert_eq!(value, "DENY");
        let (_, value) = parse_response_header("csp:default-src 'self'; img-src *").unwrap();
        assert_eq!(value, "default-src 'self'; img-src *");
    }

    #[test]
    fn a_bad_or_refused_response_header_is_an_error() {
        assert!(parse_response_header("no colon").is_err());
        assert!(parse_response_header("bad name: x").is_err());
        let err = parse_response_header("set-cookie: a=b").unwrap_err();
        assert!(err.contains("set-cookie"), "{err}");
        assert!(parse_response_header("content-length: 3").is_err());
    }

    #[test]
    fn a_trusted_proxy_flag_is_an_address_or_a_range() {
        assert_eq!(
            parse_trusted_proxies(&["10.0.0.0/8".into(), "::1".into()])
                .unwrap()
                .len(),
            2
        );
        let err = parse_trusted_proxies(&["nope".into()]).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn kv_is_a_json_object_of_strings() {
        let kv = parse_kv(r#"{"greeting":"hi","release":"0.8"}"#).unwrap();
        assert_eq!(kv["greeting"], "hi");
        assert_eq!(kv.len(), 2);
        assert!(parse_kv(r#"{"n":1}"#).unwrap_err().contains("`n`"));
        assert!(parse_kv("[1]").is_err());
        assert!(parse_kv("not json").is_err());
    }

    fn report(edge_ms: u64, origin_ms: u64, divergences: usize) -> ttfb::Report {
        ttfb::Report {
            edge: ttfb::Summary {
                samples: vec![std::time::Duration::from_millis(edge_ms)],
            },
            origin: ttfb::Summary {
                samples: vec![std::time::Duration::from_millis(origin_ms)],
            },
            divergences: vec!["GET /: body differs".to_owned(); divergences],
        }
    }

    #[test]
    fn judge_needs_zero_divergence_and_the_reduction() {
        assert!(judge(&report(10, 100, 0), 50.0, false).is_ok());
        let slow = judge(&report(80, 100, 0), 50.0, false).unwrap_err();
        assert!(
            slow.contains("reduction is 20.0%") && slow.contains("50.0%"),
            "{slow}"
        );
        let diverged = judge(&report(10, 100, 2), 50.0, false).unwrap_err();
        assert!(diverged.contains("2 pair(s) diverged"), "{diverged}");
    }

    #[test]
    fn divergence_only_skips_the_reduction_but_not_the_bytes() {
        assert!(judge(&report(80, 1, 0), 50.0, true).is_ok());
        assert!(judge(&report(1, 80, 1), 50.0, true).is_err());
    }

    #[test]
    fn the_lane_label_names_the_lane_and_the_reason() {
        assert_eq!(lane_label(Some(Lane::Edge)), "edge");
        assert_eq!(
            lane_label(Some(Lane::Fallthrough(FallthroughReason::UnknownRoute))),
            "origin (unknown_route)"
        );
        assert_eq!(lane_label(Some(Lane::OriginOnly)), "origin (origin_only)");
        assert_eq!(lane_label(None), "node_error");
    }
}

//! Isolated integration test: allocation gate for trusted-proxy resolution
//! (`autumn_web::security::ProxyResolver`).
//!
//! Its own binary for the same reason `config_alloc_gate` is:
//! `allocation-counter` installs a process-wide counting `#[global_allocator]`,
//! a process-wide side effect per CLAUDE.md's isolated-test rules.
//!
//! # What is measured
//!
//! Behind a proxy (`trust_forwarded_headers = true`) the resolver reads the
//! client address out of the request's `X-Forwarded-For` chain, and in
//! hop-count mode the host and scheme resolvers each read the chain again to
//! decide whether the peer is trusted. Every one of those reads only parses
//! the chain, so a request carrying it on one header line (the usual shape)
//! must not pay for a copy of it:
//!
//! - `resolve_client_addr` returns a `Copy` `IpAddr` and allocates nothing.
//! - `resolve_client_host` / `resolve_client_scheme` allocate only the owned
//!   `String` their public signature returns.
//! - A chain split over several header lines still costs one joined `String`
//!   per read, since the lines must read as one comma-separated list.
//!
//! Before the chain was borrowed, every read collected the header lines into a
//! `Vec` and joined them into a fresh `String`, two allocations a read even for
//! a single line. Allocations per call; the `before` column is this gate run
//! against the code it replaced:
//!
//! | call                            | mode     | before | after |
//! |---------------------------------|----------|--------|-------|
//! | `resolve_client_addr`           | hops = 1 |      2 |     0 |
//! | `resolve_client_addr`           | ranges   |      2 |     0 |
//! | `resolve_client_host`           | hops = 1 |      3 |     1 |
//! | `resolve_client_host`           | ranges   |      1 |     1 |
//! | `resolve_client_scheme`         | hops = 1 |      3 |     1 |
//! | `resolve_client_scheme`         | ranges   |      1 |     1 |
//! | `resolve_client_addr`, 2 lines  | hops = 1 |      2 |     1 |
//!
//! A failure here means a refactor put a copy of the chain back on the read
//! path; re-measure and re-derive rather than raising a ceiling.

use std::hint::black_box;
use std::net::{IpAddr, SocketAddr};

use autumn_web::security::{ProxyResolver, TrustedProxiesConfig};
use axum::extract::ConnectInfo;
use axum::http::Request;

/// Enough calls that a per-call allocation cannot hide in noise.
const CALLS: u64 = 1_000;

/// Behind exactly one ALB with a dynamic IP: peel one hop from the right.
fn hops_mode() -> ProxyResolver {
    ProxyResolver::from_config(&TrustedProxiesConfig {
        ranges: Vec::new(),
        trusted_hops: Some(1),
        trust_forwarded_headers: true,
    })
}

/// Behind proxies in a known range: walk the chain skipping trusted IPs.
fn ranges_mode() -> ProxyResolver {
    ProxyResolver::from_config(&TrustedProxiesConfig {
        ranges: vec!["10.0.0.0/8".to_owned()],
        trusted_hops: None,
        trust_forwarded_headers: true,
    })
}

/// A request as the ALB at `10.0.1.200` forwards it, with the chain on as many
/// `X-Forwarded-For` lines as `xff_lines` holds.
fn proxied_request(xff_lines: &[&str]) -> Request<()> {
    let mut builder = Request::builder()
        .header("host", "internal.cluster.local")
        .header("x-forwarded-host", "app.example")
        .header("x-forwarded-proto", "https");
    for line in xff_lines {
        builder = builder.header("x-forwarded-for", *line);
    }
    let mut req = builder.body(()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 1, 200], 443))));
    req
}

/// Allocations made by `f` over [`CALLS`] calls, after a warm-up that pays
/// every first-sight cost (lazy statics and the like). The total, not a
/// per-call quotient, so a stray allocation on some calls cannot round away.
fn over_calls(label: &str, mut f: impl FnMut()) -> u64 {
    for _ in 0..10 {
        f();
    }
    let info = allocation_counter::measure(|| {
        for _ in 0..CALLS {
            f();
        }
    });
    println!(
        "{label}: {} allocations over {CALLS} calls",
        info.count_total
    );
    info.count_total
}

#[test]
fn resolving_the_client_addr_does_not_copy_a_one_line_chain() {
    let req = proxied_request(&["192.0.2.1, 10.0.1.200"]);
    let client: IpAddr = "192.0.2.1".parse().unwrap();

    for (mode, resolver) in [("hops", hops_mode()), ("ranges", ranges_mode())] {
        // The answer itself is unchanged: the real client, not the ALB.
        assert_eq!(resolver.resolve_client_addr(&req), Some(client), "{mode}");
        let total = over_calls(&format!("resolve_client_addr ({mode})"), || {
            black_box(resolver.resolve_client_addr(black_box(&req)));
        });
        assert_eq!(
            total, 0,
            "resolve_client_addr ({mode}) must read a one-line X-Forwarded-For in place"
        );
    }
}

#[test]
fn resolving_host_and_scheme_allocates_only_the_returned_string() {
    let req = proxied_request(&["192.0.2.1, 10.0.1.200"]);

    for (mode, resolver) in [("hops", hops_mode()), ("ranges", ranges_mode())] {
        assert_eq!(
            resolver.resolve_client_host(&req).as_deref(),
            Some("app.example"),
            "{mode}"
        );
        assert_eq!(
            resolver.resolve_client_scheme(&req).as_deref(),
            Some("https"),
            "{mode}"
        );

        let host = over_calls(&format!("resolve_client_host ({mode})"), || {
            black_box(resolver.resolve_client_host(black_box(&req)));
        });
        assert_eq!(
            host, CALLS,
            "resolve_client_host ({mode}) must allocate only the host it returns"
        );
        let scheme = over_calls(&format!("resolve_client_scheme ({mode})"), || {
            black_box(resolver.resolve_client_scheme(black_box(&req)));
        });
        assert_eq!(
            scheme, CALLS,
            "resolve_client_scheme ({mode}) must allocate only the scheme it returns"
        );
    }
}

#[test]
fn a_chain_split_over_two_lines_costs_one_joined_string() {
    // A proxy that appends its own `X-Forwarded-For` line instead of extending
    // the existing one: the lines read as one chain, which has to be built.
    let req = proxied_request(&["192.0.2.1", "10.0.1.200"]);
    let resolver = hops_mode();
    assert_eq!(
        resolver.resolve_client_addr(&req),
        Some("192.0.2.1".parse().unwrap())
    );
    let total = over_calls("resolve_client_addr (hops, two lines)", || {
        black_box(resolver.resolve_client_addr(black_box(&req)));
    });
    assert_eq!(
        total, CALLS,
        "two X-Forwarded-For lines must cost exactly the joined chain"
    );
}

//! The simulated network (issue #2967).
//!
//! A [`SimNet`] replaces the real network for outbound calls made through
//! [`crate::http_client::Client`]. It does three things:
//!
//! - **Hosts.** [`SimNet::host`] maps a host name to an in-process
//!   [`axum::Router`]. A call to `http://payments/charge` is served by the
//!   router named `payments`. A host with no router falls back to the app's
//!   http mocks ([`crate::test::TestApp::http_mock`]). A host with neither is
//!   an error. No call reaches the real network.
//! - **Seeded faults.** [`SimNet::latency`] delays each attempt by a seeded
//!   amount of virtual time. [`SimNet::drop_rate`] drops a seeded share of
//!   attempts. The client's retry policy applies: attempts and backoff, 429
//!   `Retry-After`, 502-504 retries and the per-attempt `request_timeout`. A
//!   drop is retried like a real connect or timeout error.
//! - **Partitions.** [`SimNet::partition`] cuts a host off until
//!   [`SimNet::heal`]. Call them while the test runs.
//!
//! Every decision comes from a stream seeded from the sim seed, and
//! [`SimNet::events`] records each attempt. Two runs with the same seed record
//! equal event logs.
//!
//! ```rust,ignore
//! let net = SimNet::new()
//!     .host("payments", payments_router())
//!     .latency(Duration::from_millis(5), Duration::from_millis(80))
//!     .drop_rate(0.1);
//! sim.net(net.clone());
//! sim.build(TestApp::new().routes(routes![checkout]));
//! net.partition("payments");
//! ```
//!
//! # Scope
//!
//! Only calls through `http_client::Client` built from the app state (the
//! `Client` extractor or `Client::from_state`) see the network. A client made
//! with `Client::new()` does not. Needs the `http-client` feature.
//!
//! As with http mocks, a sim call skips the process-global circuit breaker,
//! the SSRF address checks, `pin_to` and redirect following. A host router's
//! `3xx` comes back as the response.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::entropy::{Entropy, SeededEntropy};

/// Salt `XOR`ed into the mount seed for the network stream, so it is
/// independent of the other seeded streams.
pub(crate) const NET_STREAM_SALT: u64 = 0x4E37_5EED_4E37_5EED;

/// What the network did to one attempt.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetFault {
    /// The attempt was delivered.
    None,
    /// The attempt was lost after its latency. The client retries it like a
    /// connect or timeout error.
    Dropped,
    /// The host was partitioned, so the attempt failed at once.
    Partitioned,
    /// The latency passed the client's `request_timeout`. The event's latency
    /// is the timeout, the time that actually passed. The client retries it.
    TimedOut,
}

impl std::fmt::Display for NetFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "delivered",
            Self::Dropped => "dropped",
            Self::Partitioned => "partitioned",
            Self::TimedOut => "timed out",
        })
    }
}

/// One attempt through the network, in order.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetEvent {
    /// The attempt number, starting at 0.
    pub seq: u64,
    /// The host of the attempt.
    pub host: String,
    /// The virtual time the attempt took.
    pub latency: Duration,
    /// What the network did.
    pub fault: NetFault,
}

/// A seeded virtual network for outbound HTTP. See the [module docs](self).
///
/// Cheap to clone: clones share one network, so a test keeps a clone to
/// partition hosts and read events after it hands one to
/// [`Sim::net`](crate::sim::Sim::net).
#[derive(Clone, Default)]
pub struct SimNet {
    inner: Arc<Mutex<NetState>>,
}

struct NetState {
    hosts: BTreeMap<String, axum::Router>,
    partitioned: BTreeSet<String>,
    drop_rate: f64,
    latency: (Duration, Duration),
    stream: Arc<dyn Entropy>,
    events: Vec<NetEvent>,
}

impl Default for NetState {
    fn default() -> Self {
        Self {
            hosts: BTreeMap::new(),
            partitioned: BTreeSet::new(),
            drop_rate: 0.0,
            latency: (Duration::ZERO, Duration::ZERO),
            stream: SeededEntropy::shared(NET_STREAM_SALT),
            events: Vec::new(),
        }
    }
}

impl std::fmt::Debug for SimNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("SimNet")
            .field("hosts", &state.hosts.keys().collect::<Vec<_>>())
            .field("partitioned", &state.partitioned)
            .field("drop_rate", &state.drop_rate)
            .field("latency", &state.latency)
            .finish_non_exhaustive()
    }
}

impl SimNet {
    /// An empty network: no hosts, no latency, no drops.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve calls to `host` with `router`. The name matches as a URL host
    /// does: `Payments` serves `http://payments/`.
    #[must_use]
    pub fn host(self, host: impl Into<String>, router: axum::Router) -> Self {
        self.lock()
            .hosts
            .insert(canonical_host(&host.into()), router);
        self
    }

    /// Delay each attempt by a seeded time in `[min, max]`, to the
    /// nanosecond. A `max` below `min` is raised to `min`.
    #[must_use]
    pub fn latency(self, min: Duration, max: Duration) -> Self {
        self.lock().latency = (min, max.max(min));
        self
    }

    /// Drop this share of attempts, from `0.0` (none) to `1.0` (all). The value
    /// is clamped, and `NaN` is `0.0`.
    #[must_use]
    pub fn drop_rate(self, rate: f64) -> Self {
        self.lock().drop_rate = super::chaos::clamp_prob(rate);
        self
    }

    /// Cut `host` off. Each attempt to reach it fails at once until
    /// [`heal`](Self::heal).
    pub fn partition(&self, host: impl Into<String>) {
        self.lock().partitioned.insert(canonical_host(&host.into()));
    }

    /// Reconnect a partitioned `host`.
    pub fn heal(&self, host: &str) {
        self.lock().partitioned.remove(&canonical_host(host));
    }

    /// Every attempt that finished its latency, in the order they finished. An
    /// attempt dropped during its latency is not recorded.
    #[must_use]
    pub fn events(&self) -> Vec<NetEvent> {
        self.lock().events.clone()
    }

    /// Restart the decision stream from `seed`. Called at each mount.
    pub(crate) fn reseed(&self, seed: u64) {
        self.lock().stream = SeededEntropy::shared(seed ^ NET_STREAM_SALT);
    }

    /// Serve `host` with `router` from now on (a mounted replica, #3067).
    pub(crate) fn insert_host(&self, host: &str, router: axum::Router) {
        self.lock().hosts.insert(canonical_host(host), router);
    }

    /// Stop serving `host` (a killed replica). Calls to it then fail.
    pub(crate) fn remove_host(&self, host: &str) {
        self.lock().hosts.remove(&canonical_host(host));
    }

    /// The router that serves `host`, if any.
    pub(crate) fn service(&self, host: &str) -> Option<axum::Router> {
        self.lock().hosts.get(&canonical_host(host)).cloned()
    }

    /// Send one attempt to `host`: record it, wait its latency, and return the
    /// fault, if any. A latency above `timeout` becomes a
    /// [`NetFault::TimedOut`] that waits only the timeout.
    pub(crate) async fn transmit(
        &self,
        host: &str,
        timeout: Option<Duration>,
    ) -> Result<(), NetFault> {
        let (latency, fault) = {
            let state = self.lock();
            // Two draws per attempt, always, so the stream maps one-to-one
            // onto attempts whatever the configuration.
            let latency_draw = state.stream.next_u64();
            let drop_draw = state.stream.next_u64();
            let (latency, fault) = if state.partitioned.contains(&canonical_host(host)) {
                (Duration::ZERO, NetFault::Partitioned)
            } else if super::chaos::unit_from_draw(drop_draw) < state.drop_rate {
                (
                    sample_latency(state.latency, latency_draw),
                    NetFault::Dropped,
                )
            } else {
                (sample_latency(state.latency, latency_draw), NetFault::None)
            };
            let (latency, fault) = match timeout {
                Some(limit) if fault != NetFault::Partitioned && latency > limit => {
                    (limit, NetFault::TimedOut)
                }
                _ => (latency, fault),
            };
            drop(state);
            (latency, fault)
        };
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        // Record the attempt only once its latency has passed. An attempt
        // dropped before then (a crash, a cancelled request) never reached the
        // host, so it leaves no event. Its draws are still spent, so later
        // attempts replay the same way.
        {
            let mut state = self.lock();
            let seq = state.events.len() as u64;
            state.events.push(NetEvent {
                seq,
                host: host.to_owned(),
                latency,
                fault,
            });
        }
        match fault {
            NetFault::None => Ok(()),
            fault => Err(fault),
        }
    }

    fn lock(&self) -> MutexGuard<'_, NetState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A latency in `[min, max]`, to the nanosecond, from `draw`. Bounds past
/// `u64::MAX` nanoseconds (about 584 years) are capped there.
fn sample_latency((min, max): (Duration, Duration), draw: u64) -> Duration {
    let min_ns = u64::try_from(min.as_nanos()).unwrap_or(u64::MAX);
    let max_ns = u64::try_from(max.as_nanos()).unwrap_or(u64::MAX);
    let span = max_ns.saturating_sub(min_ns).saturating_add(1);
    Duration::from_nanos(min_ns.saturating_add(draw % span))
}

/// `host` as a URL writes it: lower case, with an international name in
/// punycode and an IPv6 address in brackets. A request's host is always in
/// this form, so a name registered any other way would never match.
fn canonical_host(host: &str) -> String {
    url::Host::parse(host).map_or_else(|_| host.to_ascii_lowercase(), |parsed| parsed.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_host_name_matches_as_a_url_host_does() {
        use super::canonical_host;
        assert_eq!(canonical_host("Payments"), "payments");
        assert_eq!(
            canonical_host("PAYMENTS.Example.COM"),
            "payments.example.com"
        );
        assert_eq!(canonical_host("[::1]"), "[::1]");
        assert_eq!(canonical_host("10.0.0.1"), "10.0.0.1");
        let url = url::Url::parse("http://Bücher.Example/").unwrap();
        assert_eq!(canonical_host("Bücher.Example"), url.host_str().unwrap());
    }

    use super::{Duration, NetFault, SimNet, sample_latency};

    #[test]
    fn latency_stays_in_range() {
        let range = (Duration::from_millis(5), Duration::from_millis(9));
        for draw in 0..100 {
            let latency = sample_latency(range, draw);
            assert!(latency >= range.0 && latency <= range.1, "{latency:?}");
        }
        let fixed = (Duration::from_millis(7), Duration::from_millis(7));
        assert_eq!(sample_latency(fixed, u64::MAX), Duration::from_millis(7));
    }

    #[test]
    fn sub_millisecond_latency_is_kept() {
        let fixed = (Duration::from_micros(500), Duration::from_micros(500));
        assert_eq!(sample_latency(fixed, 12_345), Duration::from_micros(500));
        let range = (Duration::from_micros(1500), Duration::from_micros(2500));
        for draw in [0, 1, 999, 1000, 1_000_000, u64::MAX] {
            let latency = sample_latency(range, draw);
            assert!(latency >= range.0 && latency <= range.1, "{latency:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_latency_past_the_timeout_is_recorded_as_a_timeout() {
        let net = SimNet::new().latency(Duration::from_secs(60), Duration::from_secs(60));
        let limit = Duration::from_secs(30);
        let start = tokio::time::Instant::now();
        assert_eq!(
            net.transmit("h", Some(limit)).await,
            Err(NetFault::TimedOut)
        );
        assert_eq!(start.elapsed(), limit, "only the timeout passes");
        let events = net.events();
        assert_eq!(events[0].fault, NetFault::TimedOut);
        assert_eq!(events[0].latency, limit);
        assert_eq!(
            net.transmit("h", None).await,
            Ok(()),
            "no timeout, no fault"
        );
    }

    #[test]
    fn max_below_min_is_raised() {
        let net = SimNet::new().latency(Duration::from_millis(9), Duration::from_millis(1));
        assert_eq!(
            net.lock().latency,
            (Duration::from_millis(9), Duration::from_millis(9))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn partition_fails_at_once_and_heal_restores() {
        let net = SimNet::new();
        net.partition("a");
        assert_eq!(net.transmit("a", None).await, Err(NetFault::Partitioned));
        net.heal("a");
        assert_eq!(net.transmit("a", None).await, Ok(()));
        let faults: Vec<_> = net.events().iter().map(|event| event.fault).collect();
        assert_eq!(faults, vec![NetFault::Partitioned, NetFault::None]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_attempt_dropped_during_its_latency_leaves_no_event() {
        let net = SimNet::new().latency(Duration::from_millis(10), Duration::from_millis(10));
        let outcome = crate::sim::crash_at(0, net.transmit("h", None)).await;
        assert!(outcome.is_crashed());
        assert!(net.events().is_empty(), "the host was never reached");
        assert_eq!(net.transmit("h", None).await, Ok(()));
        assert_eq!(net.events().len(), 1);
        assert_eq!(net.events()[0].seq, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn same_seed_same_decisions() {
        let run = |seed| async move {
            let net = SimNet::new()
                .drop_rate(0.5)
                .latency(Duration::ZERO, Duration::from_millis(30));
            net.reseed(seed);
            for _ in 0..32 {
                let _ = net.transmit("h", None).await;
            }
            net.events()
        };
        assert_eq!(run(1).await, run(1).await);
        assert_ne!(run(1).await, run(2).await);
    }
}

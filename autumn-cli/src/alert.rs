//! `autumn alert test` — fire a synthetic operator alert through each configured
//! delivery channel to verify wiring before a real incident (issue #1630).
//!
//! Follows the `autumn webhook sim` precedent: it exercises the *same* channel
//! implementations the runtime installs (built through
//! [`autumn_web::alerts::native_transport_channels`] plus the generic signed
//! webhook), so a green `autumn alert test` proves the exact delivery path a
//! live alert would take. Only the outbound-HTTP transports (`PagerDuty`, Slack,
//! Discord, and the signed webhook) are exercised — email delivery is validated
//! by `autumn doctor` and a real mail send, since it needs a configured mailer.

use std::sync::Arc;

use autumn_web::alerts::{self, Alert, AlertChannel, AlertCondition, WebhookAlertChannel};
use autumn_web::http::Client;

/// Run `autumn alert test [--channel <name>]`.
///
/// Loads the effective configuration, builds every configured outbound-HTTP
/// alert channel, and delivers a synthetic test alert through each, reporting
/// per-channel success or an actionable error. Exits non-zero if any configured
/// channel fails to deliver, or if `--channel` names a channel that is not
/// configured.
pub fn run_test(channel_filter: Option<&str>) {
    let config = match autumn_web::config::AutumnConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to load configuration: {e}");
            std::process::exit(1);
        }
    };
    let client = Client::from_config(&config.http.client);
    let mut channels = configured_http_channels(&config.alerts, &client);

    if config
        .alerts
        .email
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        println!(
            "Note: an [alerts] email destination is configured; `autumn alert test` does not \
             exercise email delivery (it needs a configured mailer). Verify it with `autumn \
             doctor` and a real send."
        );
    }

    // Filter to a single channel when requested; a name that is not configured
    // is an actionable error (issue #1630: unknown/unconfigured provider name).
    if let Some(want) = channel_filter {
        let configured: Vec<&'static str> = channels.iter().map(|c| c.name()).collect();
        channels.retain(|c| c.name() == want);
        if channels.is_empty() {
            eprintln!(
                "Error: no configured alert channel named '{want}'. Configured channels: {configured:?}. \
                 Supported names: pagerduty, slack, discord, webhook."
            );
            std::process::exit(1);
        }
    }

    if channels.is_empty() {
        println!(
            "No outbound alert delivery channels are configured. Set a PagerDuty routing key \
             ([alerts] pagerduty_routing_key), a Slack/Discord webhook URL ([alerts] \
             slack_webhook_url / discord_webhook_url), or a signed webhook ([alerts] webhook_url \
             + webhook_secret). See docs/guide/operator-alerts.md"
        );
        return;
    }

    let alert = Alert::trigger(AlertCondition::DeadLetteredJob, "alert-test:synthetic")
        .title("Autumn test alert")
        .summary(
            "Synthetic alert fired by `autumn alert test` to verify operator-alert channel \
             wiring. If you are seeing this, delivery works.",
        )
        .detail("source", "autumn alert test")
        .build();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: failed to start async runtime: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "Firing a synthetic test alert through {} channel(s)…",
        channels.len()
    );
    let mut any_failed = false;
    runtime.block_on(async {
        for channel in &channels {
            match channel.deliver(&alert).await {
                Ok(()) => println!("  \u{2705} {}: delivered", channel.name()),
                Err(error) => {
                    any_failed = true;
                    println!("  \u{274c} {}: {error}", channel.name());
                }
            }
        }
    });

    if any_failed {
        std::process::exit(1);
    }
}

/// Build the outbound-HTTP alert channels the runtime would install from
/// `config`: the native providers (`PagerDuty` / Slack / Discord) plus the
/// generic signed webhook. Prints a note when a webhook URL is set without the
/// signing secret (alert webhooks are always signed, so it is skipped).
fn configured_http_channels(
    config: &autumn_web::alerts::AlertConfig,
    client: &Client,
) -> Vec<Arc<dyn AlertChannel>> {
    let mut channels = alerts::native_transport_channels(config, client);
    if let Some(url) = config
        .webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match config
            .webhook_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(secret) => channels.push(Arc::new(WebhookAlertChannel::new(
                client.clone(),
                url.to_owned(),
                Some(secret.to_owned()),
            ))),
            None => eprintln!(
                "Note: [alerts] webhook_url is set but webhook_secret is not; alert webhooks are \
                 always signed, so the generic webhook channel is skipped."
            ),
        }
    }
    channels
}

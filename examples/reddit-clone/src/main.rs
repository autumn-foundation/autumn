// Reddit Clone — an Autumn example showcasing the full feature set:
//
//   Route macros        -> #[get], #[post], #[delete], routes![], #[autumn_web::main]
//   Hybrid rendering    -> #[static_get] pre-rendered about page
//   Database            -> Diesel async Postgres, Db extractor, embedded migrations
//   Model macro         -> #[autumn_web::model] with #[id], #[indexed], #[validate], #[default]
//   Repository macro    -> #[autumn_web::repository] with derived queries & REST API generation
//   Mutation hooks      -> before_create / before_update lifecycle hooks
//   Authentication      -> Session cookies, bcrypt hashing, session.rotate_id()
//   Transactional Email -> Mailer extractor + #[mailer] welcome email template
//   Mail previews       -> dev-only /_autumn/mail preview registration
//   Authorization       -> #[secured] macro for route protection
//   CSRF protection     -> CsrfToken extractor in forms
//   Validation          -> #[validate(length(min, max))] on model fields
//   Scheduled tasks     -> #[scheduled(every = "15m")] hot-rank recalculator
//   WebSockets          -> #[ws] live feed with Channels + durable app-db relay + pluggable bus
//   Background Jobs     -> #[job] onboarding + post-publication side effects
//   Idempotency keys    -> POST/PUT/DELETE deduplication via Idempotency-Key header
//   Profiles            -> autumn.toml + autumn-dev.toml dev overrides
//   Actuator            -> /health, /actuator/health, /actuator/info, /actuator/tasks
//   HTML stack          -> Maud templates, htmx interactivity, Tailwind CSS
//   Runtime config      -> ConfigRegistry + RuntimeConfigService; live-tunable posts_per_page
//                          and registration_open without a restart (see src/config.rs)
//   Feature flags       -> AppBuilder::with_flag_store, Flags extractor, fragment + handler gating,
//                          25% rollout of new_ui_preview (see src/feature_flags.rs,
//                          routes/posts.rs front_page). Toggle live: autumn flags enable post_awards
//   A/B experiments     -> Experiments extractor + feed_layout 50/50 split (compact vs. card)
//                          (see src/experiments.rs, routes/posts.rs front_page)
//   Error reporting     -> ErrorReporter hook — structured tracing event per panic/5xx
//                          (see src/error_reporter.rs); swap for Sentry SDK in production
//   Signed webhooks     -> SignedWebhook extractor verifies Stripe-Signature before handler runs;
//                          handles Reddit Gold/Premium subscription events
//                          (see routes/webhooks.rs, [[security.webhooks.endpoints]] in autumn.toml)
//   Outbound HTTP       -> autumn_web::http::Client extractor for traced, retried outbound calls
//                          (link-preview deferred: tracked in #1238 + #1239 for 0.5.0)
//
// Run with:   cargo run -p reddit-clone   (first dev boot applies reddit migrations and
//                                          starts the job runtime + durable live-feed relay)
// Front page: http://localhost:3000
// WebSocket:  ws://localhost:3000/ws/feed
// API test:   curl http://localhost:3000/api/posts
//             curl http://localhost:3000/api/subreddits

use autumn_web::actuator::{HealthCheckOutput, HealthIndicator, HealthStatus};
use autumn_web::config::AutumnConfig;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::webhook_outbound::{InMemoryOutboundWebhookStore, OutboundWebhookPlugin};
use reddit_clone::error_reporter::StructuredReporter;
use reddit_clone::models::Post;
use reddit_clone::policies::PostPolicy;
use reddit_clone::{experiments, live_events, repositories, routes, tasks};
use std::collections::HashMap;
use std::sync::Arc;

/// Example custom health indicator: verifies the live-feed relay is reachable.
///
/// In a real app this would ping Redis, an SMTP server, or a payment gateway.
/// This demo always returns `UP` — swap in real connectivity logic as needed.
struct LiveFeedRelayIndicator;

impl HealthIndicator for LiveFeedRelayIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            // In production: attempt a lightweight ping to the Redis pub/sub
            // channel used by the live-feed relay and return Down on failure.
            let mut details = HashMap::new();
            details.insert("backend".to_string(), serde_json::json!("in_process"));
            HealthCheckOutput {
                status: HealthStatus::Up,
                details,
            }
        })
    }
}

#[cfg(feature = "embed-assets")]
static EMBEDDED_STATIC: autumn_web::include_dir::Dir = autumn_web::embed_static!();

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    let webhook_store = Arc::new(InMemoryOutboundWebhookStore::new());
    let webhook_plugin = OutboundWebhookPlugin::new(webhook_store);

    // Feature flags: InMemoryFlagStore in dev, PgFlagStore when a DB URL is present.
    // Pre-configured: new_ui_preview at 25% rollout, post_awards off.
    // Toggle live without restart: autumn flags enable post_awards
    let app_config = AutumnConfig::load().unwrap_or_default();
    let flag_store = reddit_clone::feature_flags::build_store(&app_config);

    // A/B experiments: feed_layout 50/50 compact vs. card (see src/experiments.rs).
    // Swap InMemoryExperimentStore for PgExperimentStore in production so
    // assignments survive restarts and you can conclude experiments from the DB.
    let experiment_svc = experiments::setup();

    let app = autumn_web::app()
        .migrations(autumn_web::migrate::FRAMEWORK_MIGRATIONS)
        .migrations(MIGRATIONS)
        .with_flag_store(flag_store)
        .with_error_reporter(StructuredReporter)
        .state_initializer(move |state| {
            state.insert_extension(experiment_svc);
        })
        .routes(routes![
            routes::posts::front_page,
            routes::about::about,
            routes::partials::nav_auth,
            routes::auth::register_form,
            routes::auth::register,
            routes::auth::login_form,
            routes::auth::login,
            routes::auth::logout,
            routes::auth::profile,
            routes::avatars::avatar_form,
            routes::avatars::upload_avatar,
            routes::subreddits::list,
            routes::subreddits::create_form,
            routes::subreddits::create,
            routes::subreddits::show,
            routes::posts::submit_form,
            routes::posts::submit_to_sub_form,
            routes::posts::submit,
            routes::posts::show_by_id,
            routes::posts::show,
            routes::posts::edit_form,
            routes::posts::update,
            routes::posts::manage_tags,
            routes::posts::delete_post,
            routes::comments::create,
            routes::comments::list_comments,
            routes::votes::upvote,
            routes::votes::downvote,
            routes::live::live_feed_health,
            routes::live::live_feed,
            routes::live::subreddit_feed,
            routes::live::subreddit_viewers,
            routes::live::subreddit_viewer_stream,
            routes::live::posts_stream,
            routes::live::subreddit_posts_stream,
            repositories::subreddit_api_list,
            repositories::subreddit_api_get,
            repositories::post_api_list,
            repositories::post_api_get,
            // Signed inbound webhook intake (see routes/webhooks.rs + autumn.toml).
            routes::webhooks::stripe_webhook,
            // Dev-only error routes for smoke-testing the dev error overlay.
            // These return 404 in production (profile guard is in ErrorPageFilter).
            routes::errors::trigger_error,
            routes::errors::trigger_panic,
            routes::errors::trigger_404,
        ])
        .mail_previews(routes::auth::mail_previews())
        .policy::<Post, _>(PostPolicy)
        .static_routes(static_routes![routes::about::about])
        .tasks(tasks![
            tasks::recalculate_hot_ranks,
            tasks::prune_live_feed_events
        ])
        .jobs(reddit_clone::jobs::registered_jobs())
        .listeners(reddit_clone::listeners::registered_listeners())
        .plugin(webhook_plugin)
        .plugin(live_events::LiveFeedPlugin::new())
        // Custom health indicator — visible at GET /actuator/health under "live_feed_relay".
        // Gates /ready (IndicatorGroup::Readiness by default), so a degraded relay
        // will block rolling deploys until it recovers.
        .health_indicator("live_feed_relay", Arc::new(LiveFeedRelayIndicator))
        .idempotent();

    #[cfg(feature = "embed-assets")]
    let app = app.embedded_static(&EMBEDDED_STATIC);

    app.run().await;
}

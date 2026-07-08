mod access_log;
mod after_commit_integration;
mod api_versioning_integration;
#[cfg(feature = "openapi")]
mod api_versioning_openapi;
mod api_versioning_unit;
mod app_builder;
mod authorization_integration;
mod auto_broadcast;
mod bot_protection_pipeline;
mod boundary_hooks_integration;
#[cfg(feature = "cache-moka")]
mod cache_stampede;
#[cfg(feature = "ws")]
mod chaos_channels;
#[cfg(feature = "ws")]
mod chaos_channels_concurrent_loom;
#[cfg(feature = "ws")]
mod chaos_channels_loom;
#[cfg(feature = "ws")]
mod chaos_channels_proptest;
#[cfg(feature = "ws")]
mod chaos_channels_subscribe_loom;
mod chaos_job_client_loom;
mod chaos_metrics_compute_percentiles_proptest;
mod chaos_metrics_leak;
mod chaos_metrics_leak_loom;
mod chaos_metrics_loom;
mod chaos_rate_limit_fuzz;
mod chaos_rate_limit_loom;
mod chaos_session_loom;
mod chaos_state_loom;
mod circuit_breaker_integration;
mod clock_integration;
mod compile_fail;
mod compression_middleware;
mod config_deprecation;
mod config_runtime_drift;
mod custom_layer;
mod db_telemetry_tests;
#[cfg(feature = "db")]
mod directory_shard_router;
#[cfg(all(feature = "embed-assets", feature = "i18n"))]
mod embed_assets_integration;
#[cfg(feature = "db")]
mod encryption_columns;
#[cfg(feature = "db")]
mod encryption_repository;
mod error_reporting;
mod events_integration;
#[cfg(feature = "db")]
mod experiments_pg_integration;
mod extractors;
mod factory_integration;
mod feature_flags_integration;
mod form_for_derive;
mod form_search_widgets;
#[cfg(all(feature = "maud", feature = "cache-moka"))]
mod fragment_cache_integration;
mod graceful_shutdown_contract;
mod health_indicator_integration;
mod hooks_lifecycle;
#[cfg(feature = "htmx")]
mod htmx_serving;
#[cfg(feature = "i18n")]
mod i18n_integration;
mod idempotency_middleware;
#[cfg(feature = "inbound-mail")]
mod inbound_mail_integration;
mod inline_broadcast_prefetch;
mod inspector_integration;
mod isr_coordination;
mod job_tracking_route;
mod job_tracking_stores_integration;
#[cfg(all(feature = "ws", feature = "maud", feature = "htmx", feature = "db"))]
mod live_broadcast;
mod load_shed;
#[cfg(feature = "mail")]
mod mail;
#[cfg(feature = "mail")]
mod mail_layout;
#[cfg(feature = "mail")]
mod mail_macro;
#[cfg(feature = "mail")]
mod mail_recorder_integration;
#[cfg(feature = "mail")]
mod mail_unsubscribe;
#[cfg(feature = "maud")]
mod maud_render;
#[cfg(feature = "mcp")]
mod mcp_endpoint;
#[cfg(feature = "mcp")]
mod mcp_plugin;
#[cfg(all(feature = "db", feature = "mcp"))]
mod mcp_repository;
#[cfg(feature = "mcp")]
mod mcp_streaming;
mod middleware_introspection;
mod middleware_pipeline;
#[cfg(feature = "openapi")]
mod openapi;
mod pagination;
mod path_helpers;
#[cfg(feature = "db")]
mod pg_tls;
#[cfg(feature = "db")]
mod preload_scoping;
mod problem_details;
mod rate_limit_pipeline;
mod rate_limit_principal;
#[cfg(feature = "redis")]
mod rate_limit_redis_integration;
mod raw_router_escape_hatch;
#[cfg(feature = "db")]
mod read_your_writes_routing;
#[cfg(feature = "db")]
mod repository_authorization;
#[cfg(feature = "db")]
mod repository_bulk_operations;
#[cfg(feature = "db")]
mod repository_find_in_batches;
#[cfg(feature = "db")]
mod repository_from_shard;
#[cfg(all(feature = "db", feature = "openapi"))]
mod repository_openapi;
#[cfg(feature = "db")]
mod repository_replica_routing;
#[cfg(feature = "db")]
mod repository_scope_meta;
#[cfg(feature = "db")]
mod repository_search;
mod request_timeout;
mod route_macro;
mod routes_macro;
mod scheduled_coordination;
mod schema_drift_guard;
mod scoped_tokens;
mod security;
mod seo;
#[cfg(feature = "db")]
mod shard_map_guard;
#[cfg(feature = "db")]
mod sharding_across_tenants;
mod sharding_commit_hooks;
#[cfg(feature = "db")]
mod sharding_integration;
mod signed_webhooks;
mod static_serving;
#[cfg(feature = "storage")]
mod storage_local_integration;
#[cfg(feature = "maud")]
mod stories;
#[cfg(feature = "system-tests")]
mod system_test_api;
#[cfg(feature = "db")]
mod tenancy;
mod tenancy_unit;
mod test_app_integration;
mod test_db_integration;
mod time_zone_integration;
mod transactional_test_integration;
mod tx_isolation_retry_integration;
mod webhook_outbound;
#[cfg(feature = "maud")]
mod widget_css_coverage;
mod widgets_modal;
mod widgets_tabs;
#[cfg(feature = "ws")]
mod ws_integration;

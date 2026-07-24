#[cfg(feature = "maud")]
mod a11y;
mod access_log;
mod acting_as_integration;
mod after_commit_integration;
mod alerts;
mod api_versioning_integration;
#[cfg(feature = "openapi")]
mod api_versioning_openapi;
mod api_versioning_unit;
mod app_builder;
mod authorization_integration;
mod auto_broadcast;
mod bot_protection_pipeline;
mod boundary_hooks_integration;
#[cfg(feature = "ws")]
mod broadcast_recorder;
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
mod commit_hook_drain;
mod compile_fail;
mod compression_middleware;
mod config_deprecation;
mod config_runtime_drift;
mod custom_layer;
mod db_telemetry_tests;
#[cfg(feature = "db")]
mod directory_shard_router;
#[cfg(feature = "db")]
mod distributed_lock;
mod download;
mod duplicate_route_detection;
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
mod factory_fake;
mod factory_integration;
mod fake_generators;
mod feature_flags_integration;
mod feed;
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
mod job_recorder_integration;
mod job_tracking_route;
mod job_tracking_stores_integration;
#[cfg(all(feature = "ws", feature = "maud", feature = "htmx", feature = "db"))]
mod live_broadcast;
mod load_shed;
#[cfg(feature = "mail")]
mod mail;
#[cfg(feature = "mail")]
mod mail_css_inline;
#[cfg(feature = "mail")]
mod mail_layout;
#[cfg(feature = "mail")]
mod mail_macro;
#[cfg(feature = "mail")]
mod mail_recorder_integration;
#[cfg(feature = "mail")]
mod mail_suppression;
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
mod mcp_schema_derive;
#[cfg(feature = "mcp")]
mod mcp_streaming;
mod middleware_introspection;
mod middleware_pipeline;
#[cfg(feature = "db")]
mod migrate_checksum_proptest;
#[cfg(feature = "db")]
mod model_field_attrs;
#[cfg(feature = "maud")]
mod negotiate;
#[cfg(all(feature = "db", feature = "test-support"))]
mod nested_form_atomic_save;
#[cfg(feature = "maud")]
mod nested_form_order_example;
mod notifications;
#[cfg(feature = "offline-sync")]
mod offline_sync_conformance;
#[cfg(feature = "offline-sync")]
mod offline_sync_engine;
#[cfg(feature = "offline-sync")]
mod offline_sync_pg;
#[cfg(feature = "offline-sync")]
mod offline_sync_store;
#[cfg(feature = "openapi")]
mod openapi;
mod pagination;
mod pagination_cursor_proptest;
mod path_helpers;
mod payload_version_integration;
#[cfg(feature = "db")]
mod pg_tls;
#[cfg(feature = "db")]
mod preload_scoping;
mod problem_details;
#[cfg(feature = "redis")]
mod process_role_worker_gating;
mod query_count_asserts;
#[cfg(feature = "redis")]
mod queue_dedicated_capacity;
mod range;
mod rate_limit_pipeline;
mod rate_limit_principal;
#[cfg(feature = "redis")]
mod rate_limit_redis_integration;
mod raw_router_escape_hatch;
#[cfg(feature = "db")]
mod read_your_writes_routing;
#[cfg(feature = "db")]
mod repository_audit_actor;
#[cfg(feature = "db")]
mod repository_authorization;
#[cfg(feature = "db")]
mod repository_bulk_operations;
#[cfg(feature = "db")]
mod repository_dependent_destroy;
#[cfg(feature = "db")]
mod repository_find_in_batches;
#[cfg(feature = "db")]
mod repository_find_or_create_by;
#[cfg(feature = "db")]
mod repository_from_shard;
#[cfg(feature = "db")]
mod repository_grouped_aggregates;
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
mod server_timing;
#[cfg(feature = "db")]
mod shard_across_tenants_no_shard_set;
#[cfg(feature = "db")]
mod shard_map_guard;
#[cfg(feature = "db")]
mod sharding_across_tenants;
mod sharding_commit_hooks;
#[cfg(feature = "db")]
mod sharding_integration;
mod signed_webhooks;
mod sim_advance_to;
mod sim_clock_drain;
mod sim_deterministic_ids;
mod sim_job_clock;
mod sim_rate_limit_clock;
mod sim_strict_wall_clock;
mod sim_test_smoke;
#[cfg(feature = "ws")]
mod sse_replay;
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
mod tenant_cell_quota;
mod tenant_cell_unit;
mod test_app_integration;
mod test_db_integration;
mod throttle_route;
mod time_zone_integration;
#[cfg(feature = "tls")]
mod tls_serving;
mod transactional_test_integration;
mod tx_isolation_retry_integration;
#[cfg(feature = "db")]
mod validate_merged_model;
#[cfg(feature = "db")]
mod validate_on_update_blind;
mod validate_patch_option_ip;
mod webhook_outbound;
#[cfg(feature = "maud")]
mod widget_css_coverage;
#[cfg(feature = "maud")]
mod widgets_alert;
#[cfg(feature = "maud")]
mod widgets_avatar;
#[cfg(feature = "maud")]
mod widgets_badge;
#[cfg(feature = "maud")]
mod widgets_charts;
#[cfg(feature = "maud")]
mod widgets_infinite_feed;
mod widgets_modal;
mod widgets_tabs;
#[cfg(feature = "maud")]
mod widgets_toast;
#[cfg(feature = "ws")]
mod ws_integration;

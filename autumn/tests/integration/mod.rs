#[cfg(feature = "maud")]
mod a11y;
mod access_log;
#[cfg(feature = "acme")]
mod acme_dns01;
#[cfg(feature = "acme")]
mod acme_end_to_end;
#[cfg(feature = "acme")]
mod acme_fake_ca;
#[cfg(feature = "acme-pebble")]
mod acme_pebble;
mod acting_as_integration;
mod after_commit_integration;
#[cfg(feature = "mcp")]
mod agent_authority;
mod alerts;
mod api_versioning_integration;
#[cfg(feature = "openapi")]
mod api_versioning_openapi;
mod api_versioning_unit;
mod app_builder;
mod app_metrics_facade;
#[cfg(feature = "db")]
mod auth_lockout_race;
mod authorization_integration;
mod auto_broadcast;
mod bot_protection_pipeline;
mod boundary_hooks_integration;
#[cfg(feature = "ws")]
mod broadcast_recorder;
#[cfg(all(feature = "db", feature = "cache-moka"))]
mod cache_coherence;
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
mod cluster_two_node;
#[cfg(feature = "db")]
mod commentable;
mod commit_hook_drain;
mod compile_fail;
mod compression_middleware;
mod config_deprecation;
mod config_runtime_drift;
mod custom_layer;
#[cfg(feature = "db")]
mod data_classification;
mod db_telemetry_tests;
#[cfg(feature = "db")]
mod directory_shard_router;
#[cfg(feature = "db")]
mod distributed_lock;
mod download;
mod duplicate_route_detection;
// The origin-side half of the edge capsule (#1790). `cache-moka` supplies the
// concrete `Cache` the `CacheEdgeKv` adapter is proven against; the wasm half
// of the parity claim lives in the example crate's conformance suite, which
// needs the `wasm32-wasip1` target and must never be swept in here.
#[cfg(all(feature = "edge", feature = "cache-moka"))]
mod edge_native;
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
#[cfg(feature = "db")]
mod export_csv_list_count_profile;
mod extractors;
mod factory_fake;
mod factory_integration;
#[cfg(feature = "reporting")]
mod failure_capsule_capture;
#[cfg(all(
    feature = "reporting",
    feature = "db",
    feature = "test-support",
    not(feature = "sqlite")
))]
mod failure_capsule_db;
#[cfg(all(feature = "reporting", feature = "http-client", feature = "cache-moka"))]
mod failure_capsule_effects;
#[cfg(all(
    feature = "reporting",
    feature = "db",
    feature = "test-support",
    not(feature = "sqlite")
))]
mod failure_capsule_end_to_end;
#[cfg(all(
    feature = "reporting",
    feature = "db",
    feature = "test-support",
    not(feature = "sqlite")
))]
mod failure_capsule_overhead;
#[cfg(all(feature = "reporting", feature = "db", not(feature = "sqlite")))]
mod failure_capsule_replay;
mod fake_generators;
mod feature_flags_integration;
mod feed;
mod form_for_derive;
mod form_search_widgets;
#[cfg(all(feature = "maud", feature = "cache-moka"))]
mod fragment_cache_integration;
mod framework_retention;
#[cfg(feature = "db")]
mod framework_retention_pg;
mod graceful_shutdown_contract;
mod health_indicator_integration;
mod hooks_lifecycle;
#[cfg(feature = "htmx")]
mod htmx_serving;
#[cfg(feature = "i18n")]
mod i18n_integration;
mod idempotency_middleware;
mod idempotency_tenant_scope;
mod idempotency_token_principal;
mod impersonation;
#[cfg(feature = "db")]
mod impersonation_versioned_db;
#[cfg(feature = "inbound-mail")]
mod inbound_mail_integration;
mod ingress_named_futures;
mod inline_broadcast_prefetch;
mod inspector_integration;
mod isr_coordination;
mod job_recorder_integration;
mod job_tracking_route;
mod job_tracking_stores_integration;
#[cfg(all(feature = "ws", feature = "maud", feature = "htmx", feature = "db"))]
mod live_broadcast;
mod live_state;
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
#[cfg(feature = "mcp")]
mod mcp_structured_query;
mod middleware_introspection;
mod middleware_pipeline;
mod middleware_stack_depth;
mod middleware_stack_order;
#[cfg(feature = "db")]
mod migrate_checksum_proptest;
#[cfg(feature = "db")]
mod model_counter_cache;
#[cfg(feature = "db")]
mod model_field_attrs;
#[cfg(feature = "db")]
mod model_votable;
#[cfg(feature = "maud")]
mod negotiate;
#[cfg(all(feature = "db", feature = "test-support"))]
mod nested_form_atomic_save;
#[cfg(feature = "maud")]
mod nested_form_order_example;
mod notifications;
mod nul_byte_input;
#[cfg(feature = "offline-sync")]
mod offline_sync_conformance;
#[cfg(feature = "offline-sync")]
mod offline_sync_engine;
#[cfg(feature = "offline-sync")]
mod offline_sync_gc_tombstones_batching_perf;
#[cfg(feature = "offline-sync")]
mod offline_sync_pg;
#[cfg(feature = "offline-sync")]
mod offline_sync_push_batching_perf;
#[cfg(feature = "offline-sync")]
mod offline_sync_store;
#[cfg(feature = "openapi")]
mod openapi;
#[cfg(feature = "openapi")]
mod openapi_export;
mod pagination;
mod pagination_cursor_proptest;
mod path_helpers;
mod payload_version_integration;
#[cfg(feature = "pdf")]
mod pdf;
#[cfg(feature = "db")]
mod pg_tls;
mod plugin_contract;
#[cfg(feature = "db")]
mod position_repository_integration;
#[cfg(feature = "db")]
mod preload_scoping;
mod problem_details;
#[cfg(feature = "redis")]
mod process_role_worker_gating;
// The capability-sandboxed plugin lane (#1609). Gated on `plugin-sandbox` (the
// runtime) and `test-support` (the shared WAT escape corpus), neither of which
// the Docker sweep's feature set enables — so the ignored timing benchmark in
// here is never picked up by that bare `--ignored` run.
#[cfg(all(feature = "plugin-sandbox", feature = "test-support"))]
mod plugin_sandbox;
// The grown capability vocabulary (#1632): the adversarial corpus for KV,
// outbound HTTP, DB, jobs, render hooks, quotas and the audit surface.
#[cfg(all(feature = "plugin-sandbox", feature = "test-support"))]
mod plugin_sandbox_capabilities;
mod push_end_to_end;
mod push_router;
#[cfg(feature = "db")]
mod push_send_many_subscription_lookup_profile;
mod query_count_asserts;
mod query_structured;
#[cfg(feature = "redis")]
mod queue_dedicated_capacity;
mod queue_pinning;
mod range;
mod rate_limit_pipeline;
mod rate_limit_principal;
#[cfg(feature = "redis")]
mod rate_limit_redis_integration;
mod raw_router_escape_hatch;
#[cfg(feature = "db")]
mod read_your_writes_routing;
// Postgres tier of the bitemporal, tamper-evident record ledger (issue #1699).
// The Docker-free golden test lives in `tests/sqlite_ledger.rs`; this proves the
// Postgres fork (jsonb snapshot cast, Timestamptz binds, COALESCE unique index).
#[cfg(feature = "db")]
mod ledger_postgres;
// Ledger findings/fix harness for the generated `ledger_as_of`/`ledger_diff`
// read path: profiles the unbounded full-chain read against a deep,
// production-shaped chain and (after the fix) the bounded replacement.
#[cfg(feature = "db")]
mod ledger_as_of_deep_chain_profile;
#[cfg(feature = "db")]
mod repository_audit_actor;
#[cfg(feature = "db")]
mod repository_authorization;
#[cfg(feature = "db")]
mod repository_bulk_operations;
#[cfg(feature = "db")]
mod repository_commit_hooks_claim_ack_profile;
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
#[cfg(feature = "db")]
mod retention;
#[cfg(all(feature = "markdown", feature = "maud"))]
mod rich_text;
mod route_macro;
mod routes_macro;
mod scheduled_coordination;
mod schema_drift_guard;
mod scoped_tokens;
#[cfg(feature = "db")]
mod search_index_definition;
mod secured_route;
mod security;
mod seo;
mod server_timing;
#[cfg(feature = "http-client")]
mod shadow_mirror;
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
mod sim_chaos_clock_skew_monotonic;
mod sim_clock_drain;
mod sim_delayed_enqueue;
mod sim_deterministic_ids;
mod sim_fault_plan;
mod sim_fault_plan_pg;
mod sim_job_clock;
mod sim_llm_stub;
mod sim_monotonic_clock;
mod sim_rate_limit_clock;
mod sim_retry_storm;
mod sim_strict_wall_clock;
mod sim_test_smoke;
#[cfg(feature = "db")]
mod sqlite_replication;
#[cfg(all(feature = "db", feature = "http-client"))]
mod sqlite_replication_s3;
#[cfg(feature = "db")]
mod sqlite_replication_wal;
#[cfg(feature = "ws")]
mod sse_replay;
mod static_serving;
mod step_up_route;
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
mod tls_app_surface;
#[cfg(feature = "tls")]
mod tls_serving;
#[cfg(feature = "tls")]
mod tls_support;
mod transactional_test_integration;
#[cfg(all(feature = "db", feature = "i18n"))]
mod translatable_model;
#[cfg(feature = "i18n")]
mod translatable_request;
mod tx_isolation_retry_integration;
#[cfg(feature = "db")]
mod validate_merged_model;
#[cfg(feature = "db")]
mod validate_on_update_blind;
mod validate_patch_option_ip;
mod webhook_outbound;
#[cfg(feature = "db")]
mod webhook_outbound_dispatch_fanout_profile;
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
#[cfg(feature = "maud")]
mod widgets_reaction_controls;
mod widgets_tabs;
#[cfg(feature = "maud")]
mod widgets_toast;
#[cfg(feature = "ws")]
mod ws_integration;

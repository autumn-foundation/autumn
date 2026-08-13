mod a11y_verify;
mod api_scaffold;
mod cloud_native_scaffold;
mod console;
mod db;
mod db_pull;
mod deploy;
mod generate_lock_version_postgres;
mod generate_references_postgres;
mod generate_tauri_mobile;
mod generate_tauri_mobile_offline;
mod i18n_check;
mod lifecycle_check;
mod manifest_posture;
mod migrate_down;
#[cfg(feature = "sqlite")]
mod migrate_sqlite;
mod offsite_backup;
mod replay;
mod repo_hygiene;
mod scaffold_belongs_to;
mod scaffold_bulk_delete;
mod scaffold_csv_export;
mod scaffold_form_for;
mod scaffold_lock_version;
mod scaffold_nested_resources;
mod scaffold_rich_text;
mod scaffold_search;
mod scaffold_sort_filter;
mod scaffold_validation;
mod schema_migrate;
#[cfg(feature = "sqlite")]
mod schema_migrate_sqlite;
mod schema_pull;
#[cfg(feature = "sqlite")]
mod schema_pull_sqlite;
mod seed_model_linking;
#[cfg(unix)]
mod serve;
mod tauri_mobile_thin_client;
mod test_command;
mod webhook_sim;

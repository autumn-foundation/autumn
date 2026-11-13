require_relative "boot"

require "rails"
require "active_model/railtie"
require "active_record/railtie"
require "action_controller/railtie"
require "action_view/railtie"

Bundler.require(*Rails.groups)

module BenchRails
  class Application < Rails::Application
    config.load_defaults 8.1

    config.eager_load = true
    config.enable_reloading = false
    config.hosts.clear
    config.logger = ActiveSupport::TaggedLogging.logger($stdout)
    config.log_level = :warn
    # The JSON API is called without a CSRF token by design (`null_session`),
    # so the per-request "Can't verify CSRF token authenticity" warning is noise.
    config.action_controller.log_warning_on_csrf_failure = false
    config.public_file_server.enabled = ENV["RAILS_SERVE_STATIC_FILES"].present?
    config.secret_key_base = ENV.fetch("SECRET_KEY_BASE")

    # The benchmark database is shared with other frameworks, and Ecto also
    # keeps its migration ledger in `schema_migrations` (with a different
    # column layout), so Rails tracks its migrations in a table of its own.
    config.active_record.schema_migrations_table_name = "rails_schema_migrations"
  end
end

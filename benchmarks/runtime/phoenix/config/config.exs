import Config

config :benchmark,
  ecto_repos: [Benchmark.Repo]

config :benchmark, BenchmarkWeb.Endpoint,
  url: [host: "localhost"],
  adapter: Bandit.PhoenixAdapter,
  render_errors: [
    formats: [html: BenchmarkWeb.ErrorHTML, json: BenchmarkWeb.ErrorJSON],
    layout: false
  ],
  pubsub_server: Benchmark.PubSub

config :benchmark, Benchmark.Repo, pool_size: 20

config :phoenix, :json_library, Jason

config :logger, level: :warning

import_config "#{config_env()}.exs"

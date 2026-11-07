defmodule Benchmark.MixProject do
  use Mix.Project

  def project do
    [
      app: :benchmark,
      version: "0.1.0",
      elixir: "~> 1.19",
      elixirc_paths: elixirc_paths(Mix.env()),
      start_permanent: Mix.env() == :prod,
      listeners: [Phoenix.CodeReloader],
      deps: deps()
    ]
  end

  def application do
    [
      mod: {Benchmark.Application, []},
      extra_applications: [:logger, :runtime_tools]
    ]
  end

  defp elixirc_paths(:test), do: ["lib", "test/support"]
  defp elixirc_paths(_), do: ["lib"]

  # phoenix_live_view is the HEEx template engine (Phoenix.Component) for the
  # server-rendered pages; the app mounts no LiveViews and no socket.
  defp deps do
    [
      {:phoenix, "~> 1.8.16"},
      {:phoenix_ecto, "~> 4.7"},
      {:ecto_sql, "~> 3.14"},
      {:postgrex, "~> 0.22"},
      {:phoenix_html, "~> 4.4"},
      {:phoenix_live_view, "~> 1.2"},
      {:jason, "~> 1.4"},
      {:bandit, "~> 1.14"}
    ]
  end
end

defmodule BenchmarkWeb.PostController do
  use BenchmarkWeb, :controller
  alias Benchmark.{Post, Repo}

  def index(conn, _params) do
    render(conn, :index, posts: Post.recent(Repo))
  end

  def show(conn, %{"id" => id}) do
    case Repo.get(Post, id) do
      nil -> conn |> put_status(404) |> text("not found")
      post -> render(conn, :show, post: post)
    end
  end
end

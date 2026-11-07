defmodule BenchmarkWeb.Router do
  use BenchmarkWeb, :router

  pipeline :browser do
    plug :accepts, ["html"]
  end

  pipeline :api do
    plug :accepts, ["json"]
  end

  scope "/", BenchmarkWeb do
    pipe_through :browser
    get "/posts",      PostController, :index
    get "/posts/:id",  PostController, :show
    get "/health",     HealthController, :index
  end

  scope "/api", BenchmarkWeb do
    pipe_through :api
    get    "/posts/protected", PostApiController, :protected
    get    "/posts",           PostApiController, :index
    get    "/posts/:id",       PostApiController, :show
    post   "/posts",           PostApiController, :create
    patch  "/posts/:id",       PostApiController, :update
    delete "/posts/:id",       PostApiController, :delete
  end
end

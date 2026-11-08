FROM elixir:1.19.7-otp-28-slim AS builder
ENV MIX_ENV=prod
WORKDIR /app

RUN mix local.hex --force && mix local.rebar --force

COPY mix.exs mix.lock ./
RUN mix deps.get --only prod --check-locked && mix deps.compile

COPY config/config.exs config/prod.exs config/runtime.exs config/
COPY lib ./lib
RUN mix compile --warnings-as-errors && mix release

# Same Debian release (trixie) as the builder, so the ERTS in the release
# links against the libssl3t64 / libtinfo6 / libstdc++6 it was built with.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3t64 libncurses6 libstdc++6 curl \
    && rm -rf /var/lib/apt/lists/*

ENV LANG=C.UTF-8 LC_ALL=C.UTF-8

WORKDIR /app
COPY --from=builder /app/_build/prod/rel/benchmark ./

ENV PHX_SERVER=true
ENV PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD curl -sf http://localhost:${PORT}/health || exit 1

CMD ["/app/bin/benchmark", "start"]

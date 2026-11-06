FROM rust:1.97-bookworm AS builder
WORKDIR /workspace
ENV LIBSQLITE3_SYS_USE_PKG_CONFIG=1

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

# The workspace root manifest resolves autumn-web's `workspace = true` deps.
# Cargo reads every path dependency's manifest, optional ones included, so
# each crate autumn-web names by path is copied.
COPY Cargo.toml Cargo.lock /workspace/
COPY autumn /workspace/autumn
COPY autumn-edge /workspace/autumn-edge
COPY autumn-macros /workspace/autumn-macros
COPY autumn-macros-model /workspace/autumn-macros-model
COPY autumn-macros-repository /workspace/autumn-macros-repository
COPY autumn-macros-support /workspace/autumn-macros-support
COPY autumn-schema-core /workspace/autumn-schema-core

COPY benchmarks/runtime/autumn /workspace/benchmarks/runtime/autumn/

WORKDIR /workspace/benchmarks/runtime/autumn
# The app is its own workspace; start from the root lockfile so it builds
# against the same dependency versions the framework is tested with.
RUN cp /workspace/Cargo.lock Cargo.lock
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libpq5 libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /workspace/benchmarks/runtime/autumn/target/release/bench-autumn /app/bench-autumn
COPY --from=builder /workspace/benchmarks/runtime/autumn /app/bench-autumn-src

ENV AUTUMN_MANIFEST_DIR=/app/bench-autumn-src
ENV AUTUMN_SERVER__HOST=0.0.0.0
ENV AUTUMN_SERVER__PORT=8080

EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD curl -sf http://localhost:8080/health || exit 1

CMD ["/app/bench-autumn"]

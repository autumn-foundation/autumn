FROM rust:1.97-bookworm AS builder
WORKDIR /workspace

# Minimal stub workspace to allow -p build without pulling in the whole repo.
RUN cargo new --bin bench-loco-stub
COPY Cargo.toml /workspace/bench-loco/
COPY src /workspace/bench-loco/src
COPY migrations /workspace/bench-loco/migrations
WORKDIR /workspace/bench-loco
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 libpq5 curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /workspace/bench-loco/target/release/bench-loco /app/bench-loco
COPY config /app/config

ENV LOCO_APP_PORT=8080
ENV RUST_LOG=warn
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD curl -sf http://localhost:${LOCO_APP_PORT}/_health || exit 1

CMD ["/app/bench-loco", "-e", "production", "start"]

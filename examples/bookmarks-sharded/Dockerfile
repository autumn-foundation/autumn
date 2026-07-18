# Must satisfy the workspace MSRV (rust-version in the root Cargo.toml).
FROM rust:1.88-bookworm AS builder
WORKDIR /workspace

COPY . .
RUN cargo build --release -p bookmarks-sharded --bin bookmarks-sharded --bin migrate

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libpq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /workspace/target/release/bookmarks-sharded /app/bookmarks-sharded
COPY --from=builder /workspace/target/release/migrate /app/bookmarks-sharded-migrate
COPY --from=builder /workspace/examples/bookmarks-sharded /app/examples/bookmarks-sharded

CMD ["/app/bookmarks-sharded"]

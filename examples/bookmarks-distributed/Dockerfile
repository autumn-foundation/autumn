FROM rust:1.86-bookworm AS builder
WORKDIR /workspace

COPY . .
RUN cargo build --release -p bookmarks-distributed --bin bookmarks-distributed --bin bookmarks-distributed-migrate

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libpq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /workspace/target/release/bookmarks-distributed /app/bookmarks-distributed
COPY --from=builder /workspace/target/release/bookmarks-distributed-migrate /app/bookmarks-distributed-migrate
COPY --from=builder /workspace/examples/bookmarks-distributed /app/examples/bookmarks-distributed

CMD ["/app/bookmarks-distributed"]

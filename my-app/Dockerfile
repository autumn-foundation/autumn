FROM rust:1.88.0-bookworm AS builder

WORKDIR /app

ARG TAILWIND_VERSION=v4.1.4

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p target/autumn \
    && curl -fsSL \
        -o target/autumn/tailwindcss \
        "https://github.com/tailwindlabs/tailwindcss/releases/download/${TAILWIND_VERSION}/tailwindcss-linux-x64" \
    && chmod +x target/autumn/tailwindcss

COPY Cargo.toml autumn.toml build.rs tailwind.config.js ./
COPY src ./src
COPY static ./static
COPY migrations ./migrations

RUN cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 autumn

WORKDIR /app

COPY --from=builder /app/target/release/my-app /usr/local/bin/my-app
COPY --from=builder /app/autumn.toml /app/autumn.toml
COPY --from=builder /app/static /app/static
COPY --from=builder /app/migrations /app/migrations

ENV AUTUMN_PROFILE=prod
EXPOSE 3000

USER autumn

CMD ["my-app"]

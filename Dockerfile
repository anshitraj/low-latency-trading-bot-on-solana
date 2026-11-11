# syntax=docker/dockerfile:1
# Multi-stage build: Rust API + static Mini App -> slim runtime image.

FROM rust:1.92-bookworm AS rust
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY tests tests
COPY benchmarks benchmarks
COPY migrations migrations
COPY fixtures fixtures
RUN cargo build --release -p trader-api -p trader-cli

FROM node:24-bookworm-slim AS web
WORKDIR /web
RUN corepack enable
COPY openapi.yaml /openapi.yaml
COPY apps/telegram-webapp/package.json apps/telegram-webapp/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY apps/telegram-webapp ./
RUN sed -i 's#../../openapi.yaml#/openapi.yaml#' package.json && pnpm build

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app trader
WORKDIR /app
COPY --from=rust /src/target/release/trader-api /usr/local/bin/trader-api
COPY --from=rust /src/target/release/trader-cli /usr/local/bin/trader-cli
COPY --from=web /web/out /app/miniapp
ENV MINIAPP_DIR=/app/miniapp BIND_ADDR=0.0.0.0:8080
USER trader
EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --retries=4 CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 && printf 'GET /v1/health HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q '\"ok\"' <&3"]
ENTRYPOINT ["/usr/local/bin/trader-api"]

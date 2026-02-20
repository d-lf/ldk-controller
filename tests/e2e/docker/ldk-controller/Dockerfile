# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependency compilation first
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin ldk-controller

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /var/lib/ldk-controller ldk-controller
WORKDIR /var/lib/ldk-controller

COPY --from=builder /app/target/release/ldk-controller /usr/local/bin/ldk-controller

USER ldk-controller
ENTRYPOINT ["/usr/local/bin/ldk-controller"]

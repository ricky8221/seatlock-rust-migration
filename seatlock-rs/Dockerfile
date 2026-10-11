# syntax=docker/dockerfile:1
# Multi-arch (amd64 / arm64): on Apple Silicon this builds and runs natively on ARM64, the same
# architecture family as Graviton.
FROM rust:1.90-slim-bookworm AS build
# Cargo features to build with, e.g. FEATURES=mimalloc or FEATURES=jemalloc (allocator experiments).
ARG FEATURES=""
# jemalloc's build script needs make.
RUN apt-get update && apt-get install -y --no-install-recommends make && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Cargo.lock is committed after the first local `cargo build`; the glob keeps the very first
# image build working before it exists.
COPY Cargo.toml Cargo.lock* ./
# Cache dependencies in their own layer.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release ${FEATURES:+--features "$FEATURES"} && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release ${FEATURES:+--features "$FEATURES"}

# debian-slim (not distroless) on purpose: bench/sampler.py reads /proc/1/status via `docker exec
# sh`, the same way it does for the Java image. Swap to distroless for production.
FROM debian:bookworm-slim
RUN useradd --system --uid 10001 app
COPY --from=build /src/target/release/seatlock-rs /usr/local/bin/seatlock-rs
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/seatlock-rs"]

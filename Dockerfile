# syntax=docker/dockerfile:1

# ---- Build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Abhängigkeiten zuerst auflösen und cachen (Layer bleibt stabil, solange sich
# Cargo.toml/Cargo.lock nicht ändern). rusqlite baut SQLite gebündelt (C-Compiler
# ist im rust-Image vorhanden).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Eigentliche Quellen bauen.
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- Runtime stage ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/llmux /usr/local/bin/llmux
# Beispiel-Config als Vorlage mitliefern (die echte Config wird per Volume gemountet).
COPY config/llmux.example.yaml /app/config/llmux.example.yaml

ENV LLMUX_CONFIG=/app/config/llmux.yaml \
    LLMUX_DB=/app/data/llmux.sqlite \
    RUST_LOG=llmux=info

EXPOSE 3456
VOLUME ["/app/data"]
ENTRYPOINT ["llmux"]

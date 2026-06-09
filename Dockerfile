# syntax=docker/dockerfile:1

# ---- Dashboard build stage ----
# Baut das Astro-Dashboard nach dist/dashboard; wird unten ins Rust-Binary eingebettet
# (#20). Zur Laufzeit ist kein Node nötig.
FROM node:22-bookworm-slim AS dashboard
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY astro.config.mjs tsconfig.json ./
COPY dashboard ./dashboard
RUN npm run build

# ---- Build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Abhängigkeiten zuerst auflösen und cachen (Layer bleibt stabil, solange sich
# Cargo.toml/Cargo.lock nicht ändern). rusqlite baut SQLite gebündelt (C-Compiler
# ist im rust-Image vorhanden).
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Eigentliche Quellen + gebautes Dashboard (rust-embed bettet dist/dashboard ein).
# config/ wird gebraucht: example-/demo-Config sind via include_str! ins Binary eingebettet.
COPY src ./src
COPY config ./config
COPY --from=dashboard /app/dist/dashboard ./dist/dashboard
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

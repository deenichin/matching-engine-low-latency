# ---- build ----
FROM rust:1-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --workspace

# ---- runtime ----
FROM debian:bookworm-slim

RUN useradd --create-home --uid 1000 app \
    && mkdir -p /run/engine \
    && chown app:app /run/engine

COPY --from=builder /build/target/release/engine /usr/local/bin/engine
COPY risk.toml /home/app/risk.toml

USER app
WORKDIR /home/app

# Order-entry and market-data sockets both live under /run/engine — see
# docker-compose.yml's named volume and SPEC.md §3.
ENTRYPOINT ["/usr/local/bin/engine"]

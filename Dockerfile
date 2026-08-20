# ---- build ----
FROM rust:1-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

# ---- runtime ----
FROM debian:bookworm-slim

RUN useradd --create-home --uid 1000 app

COPY --from=builder /build/target/release/onebalance-task /usr/local/bin/app

USER app
WORKDIR /home/app

ENTRYPOINT ["/usr/local/bin/app"]
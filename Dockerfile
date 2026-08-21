# ---- build ----
# Pinned, not the floating `rust:1-slim` tag: that tag resolves to whatever
# the latest 1.x patch is on the day of the build, which makes the build
# non-reproducible by definition -- a reviewer running this later gets
# whatever Docker Hub is serving that day, which may match neither this
# project's development toolchain nor any other build of this same image.
# 1.91.1 is what every stage of this project has been built and verified
# against on the host; pinning here is about reproducibility as a
# principle, not just matching one session's toolchain.
FROM rust:1.91.1-slim AS builder

# rust:*-slim images don't install clippy/rustfmt by default -- both are
# required for ./check.sh (`docker compose run --rm dev ./check.sh`, see
# below). Installed before the source is copied in, so this layer is
# cached independently of code changes.
RUN rustup component add clippy rustfmt

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --workspace

# Everything needed to run the test suite and both benchmark suites inside
# this same stage too (`docker compose run --rm dev ...`, SPEC §9/PLAN.md
# stage 8) -- copied after the release build above so a config-file change
# doesn't invalidate that (expensive) compile cache layer. check.sh needs
# its own execute bit (preserved by COPY from the build context); risk.toml
# and risk-bench.toml are the two configs cargo run/bench default to
# loading by relative path; recordings/ is what
# crates/bin/tests/replay.rs's byte-identical test reads.
COPY check.sh ./
COPY risk.toml risk-bench.toml ./
COPY recordings ./recordings

# ---- runtime ----
# `bookworm` is Debian 12's release codename, not a floating "latest" or
# "stable" alias -- the OS major version this stage builds on is fixed.
# Left unpinned to a specific digest deliberately, not by oversight: unlike
# the builder stage above, nothing here compiles any of this project's
# code, so the only drift risk is routine package-security updates within
# the same Debian 12 release, a materially smaller and differently-shaped
# risk than the builder stage's floating-Rust-minor-version problem. Not
# worth the added maintenance burden (a digest going stale needs manual
# refreshing to keep receiving those same security updates) for a stage
# this thin.
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

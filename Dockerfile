# syntax=docker/dockerfile:1
#
# Multi-stage build for Telemetry (logs/traces vhost demux over two vendored library crates).
#   - builder: rust:1.96-slim (Debian trixie).
#   - runtime: debian:trixie-slim (matching glibc), non-root, ca-certificates.
#
# The two surfaces embed their templates + static CSS via include_str! at COMPILE time, so the
# runtime image carries only the single statically-templated binary — no assets to ship. sqlx uses
# rustls (no OpenSSL) and the audit emitter is hand-rolled HTTP/1.1 over a raw TCP socket, so the
# binary depends only on glibc — no libssl. ca-certificates is kept because the audit emitter posts
# to Watchtower. The HEALTHCHECK uses the built-in `telemetry healthcheck` subcommand, so the image
# needs no curl.

FROM rust:1.96-slim AS builder
WORKDIR /build

# Bring the whole self-contained crate (the binary + the two vendored surface crates under
# crates/) and build the release binary. The surfaces' static/ + templates/ are needed at build
# time for their include_str! embeds.
COPY Cargo.toml ./
COPY src ./src
COPY crates ./crates
RUN cargo build --release --bin telemetry \
    && strip target/release/telemetry

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (no shell, no home writes needed).
RUN useradd --system --uid 10001 --user-group --no-create-home telemetry
COPY --from=builder /build/target/release/telemetry /usr/local/bin/telemetry

USER telemetry
ENV BIND_ADDR=0.0.0.0:9100
# :9100 fronts the logs/traces/vitals subdomains; :8300 is the second listener serving the SAME
# demux so the estate's hardcoded `http://vitals:8300` metric POSTs reach the vitals arm.
EXPOSE 9100 8300

# Dependency-free liveness probe -> GET /healthz on the loopback, exit 0/1.
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["telemetry", "healthcheck"]

CMD ["telemetry"]

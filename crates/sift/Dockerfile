# syntax=docker/dockerfile:1
#
# Multi-stage build for Sift (log aggregation + search).
#   - builder: rust:1.96-slim (Debian trixie).
#   - runtime: debian:trixie-slim (matching glibc), non-root, ca-certificates.
#
# Like keyward/inkwell, Sift links NO OpenSSL: sqlx uses `rustls` and everything else is pure
# Rust, so the binary depends only on glibc — no libssl in either stage. The container
# HEALTHCHECK uses the built-in `sift healthcheck` subcommand, so no extra HTTP tool is needed.

FROM rust:1.96-slim AS builder
WORKDIR /build

# Cache the dependency graph first: build a throwaway lib/bin against the real manifest so
# `cargo build` only recompiles our crate when src/ changes, not the whole tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --bin sift \
    && rm -rf src

# Now build the real binary. static/ + templates/ are include_str!'d into the binary, so they
# must be present at compile time.
COPY src ./src
COPY static ./static
COPY templates ./templates
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --bin sift \
    && strip target/release/sift

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (no shell, no home writes needed).
RUN useradd --system --uid 10001 --user-group --no-create-home sift
COPY --from=builder /build/target/release/sift /usr/local/bin/sift

USER sift
# Default in-container binds; overridable at runtime. 9100 = dashboard/ingest, 5514 = syslog.
ENV BIND_ADDR=0.0.0.0:9100
ENV SIFT_SYSLOG_ADDR=0.0.0.0:5514
EXPOSE 9100
EXPOSE 5514/udp
EXPOSE 5514/tcp

# Dependency-free liveness probe -> GET /healthz on the loopback, exit 0/1.
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["sift", "healthcheck"]

ENTRYPOINT ["sift"]

# OSTARA backend — multi-stage Docker build.
#
#   docker build -t ostara .
#   docker run -p 3000:3000 -e AIS_API_KEY=... ostara
#
# Secrets are NEVER baked into the image: pass them as environment variables
# (or the host's secret manager). See .env.example for the full list — every
# key is optional and feeds degrade gracefully without them.

# ---- Stage 1: compile -------------------------------------------------------
# Note: no dummy-crate dependency-caching trick here. It caused a real
# outage (the cached dummy binary shipped instead of the app). A single
# COPY + build is obviously correct; the ~10 min build cost is acceptable.
FROM rust:1.99-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN cargo build --release

# ---- Stage 2: slim runtime --------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates: the app validates TLS itself (rustls native roots) for
# upstream HTTPS calls — the system store must exist.
# libssl3: the binary links OpenSSL (via a dependency) — without it the
# container exits immediately on start ("machine never started").
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/ostara /app/ostara
# Static UI (Vercel serves it in the split deployment; kept here so the
# image also runs standalone via its ServeDir fallback) and the on-disk
# registries + snapshots the API reads at runtime.
COPY web ./web
COPY data ./data

RUN useradd --create-home --shell /usr/sbin/nologin appuser \
    && chown -R appuser:appuser /app
USER appuser

# Container hosts (fly.io, Railway, Render) inject $PORT; the app honors it
# (PORT > OSIRIS_PORT > 3000 — see src/config.rs).
EXPOSE 3000
CMD ["./ostara"]

# OSTARA backend — multi-stage Docker build.
#
#   docker build -t ostara .
#   docker run -p 3000:3000 -e AIS_API_KEY=... ostara
#
# Secrets are NEVER baked into the image: pass them as environment variables
# (or the host's secret manager). See .env.example for the full list — every
# key is optional and feeds degrade gracefully without them.

# ---- Stage 1: compile -------------------------------------------------------
FROM rust:1.99-bookworm AS builder
WORKDIR /app

# Dependency caching: build a dummy crate first so this layer is reused
# until Cargo.toml/Cargo.lock change; the real sources compile in the next.
# (The [[bench]] target needs its file to exist for the manifest to parse,
# so stub benches/parse_bench.rs too — `cargo build` won't compile it.)
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src benches \
    && printf 'fn main() { println!("placeholder"); }\n' > src/main.rs \
    && printf '' > src/lib.rs \
    && printf 'fn main() {}\n' > benches/parse_bench.rs \
    && cargo build --release \
    && rm -rf src benches

COPY src ./src
RUN cargo build --release

# ---- Stage 2: slim runtime --------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates: the app validates TLS itself (rustls native roots) for
# upstream HTTPS calls — the system store must exist.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
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

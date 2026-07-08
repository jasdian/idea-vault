# syntax=docker/dockerfile:1

# =============================================================================
# idea-vault — single Rust crate (axum + Askama + HTMX + SSE), SQLite index,
# local Ollama client. Multi-stage build (mcp-server pattern) + cargo-chef
# dependency caching. Bundled SQLite so the runtime needs no system libsqlite3;
# rustls (no OpenSSL) so the runtime needs no libssl. Runtime =
# debian:bookworm-slim, non-root, with curl for the healthcheck and a writable
# /data mountpoint for the rebuildable index volume.
#
# The app is compiled as a fully STATIC musl binary
# (x86_64-unknown-linux-musl), so it depends on no host glibc — this is why the
# newer-glibc builder (rust:slim tracks Debian trixie) can pair with the older
# bookworm runtime without a `GLIBC_2.xx not found` error. rustls + bundled
# SQLite (no OpenSSL/libsqlite3) is what makes a clean static link possible.
#
# NOTE: this builds once the crate is scaffolded (Cargo.toml + src/ +
# templates/ exist). Until then it is the deployment contract, not a working
# build. See docs/12-deployment.md.
# =============================================================================

# ---- base with cargo-chef -------------------------------------------------
FROM rust:1.91-slim AS chef
WORKDIR /app
# musl-tools provides musl-gcc, which the cc crate uses to compile bundled
# SQLite for the musl target; pkg-config is a harmless safety net. Add the
# static musl target so every stage below can build against it.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl \
    && cargo install cargo-chef --locked

# ---- plan: hash only the dependency manifests ------------------------------
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Askama compiles templates into the binary at build time, so templates/ must
# be in the context; static/ is rust-embed'ed and needed at compile time too.
# (No migrations/ dir: the SQLite index is a rebuildable derivation of vault/**
# — schema DDL lives in src/index/schema.rs, recovery is reindex, ADR-0002.)
COPY templates ./templates
COPY static ./static
RUN cargo chef prepare --recipe-path recipe.json

# ---- build: compile deps (cached), then the app ---------------------------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# This layer is cached until Cargo.toml/Cargo.lock change. Cook + build for the
# static musl target so the emitted binary carries no glibc dependency.
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl --bin idea-vault

# ---- runtime: minimal, non-root -------------------------------------------
FROM debian:bookworm-slim AS runtime

# UID/GID are build args (default 1000) so the SAME uid owns both the
# bind-mounted vault (owned by the host user) and the fresh named index
# volume. Rebuild with --build-arg APP_UID=$(id -u) if your host uid != 1000.
ARG APP_UID=1000
ARG APP_GID=1000

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g ${APP_GID} app \
    && useradd -u ${APP_UID} -g ${APP_GID} -M -s /usr/sbin/nologin -d /app app

WORKDIR /app

# Pre-create + chown the mountpoints BEFORE dropping privileges. Docker copies
# this ownership onto a freshly-created named volume (the cosmic/zomboid
# volume-ownership gotcha) so the non-root process can write index.db. /claude
# is the claude-code state home (docker-compose.claude.yml mounts a named
# volume there and sets HOME=/claude); without the chown a fresh claude-state
# volume is root-owned and the non-root `user:` cannot write CLI state.
RUN mkdir -p /data /vault /claude && chown -R ${APP_UID}:${APP_GID} /data /vault /claude

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/idea-vault /usr/local/bin/idea-vault
# If you do NOT embed static assets with rust-embed, ship them instead:
# COPY --from=builder /app/static /app/static

ENV IDEA_VAULT_BIND=0.0.0.0:3000 \
    IDEA_VAULT_VAULT_DIR=/vault \
    IDEA_VAULT_INDEX_PATH=/data/index.db \
    IDEA_VAULT_OLLAMA_URL=http://ollama:11434 \
    RUST_LOG=info

USER app
EXPOSE 3000

# /admin/health already exists (docs/09-web-ui.md R11). curl is in the image.
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=5 \
    CMD curl -fsS http://localhost:3000/admin/health || exit 1

ENTRYPOINT ["idea-vault"]

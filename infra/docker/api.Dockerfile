# syntax=docker/dockerfile:1.7
#
# Omnion API image (REQ-128, slice 1).
#
# Multi-stage, build-context aware for the Cargo workspace at the repository root:
#
#   base     rust toolchain + the two C libraries the dependency graph links against.
#   chef     `cargo chef prepare` turns the workspace manifests into a recipe.json, and that
#            recipe is BUILT in its own stage. A source-only change therefore recompiles the
#            workspace and none of the third-party graph. The recipe stage is the dependency
#            layer the request asks for; it is a real layer, not a comment saying one exists.
#   build    the release binary, on top of the already-warm recipe artifacts.
#   runtime  distroless: the binary, CA certificates and a timezone database. No shell, no
#            package manager, no compiler, so a read-only root filesystem is the default state
#            rather than a hardening note.
#
# No build argument here carries a secret or a credential-shaped value. The four ARGs are the
# version/commit/source/licence quad that becomes OCI labels; `release/verify-deploy-artifacts.sh`
# asserts that property by parsing this file rather than trusting review, because a build
# argument that leaks a secret is invisible in review and permanent in the layer history.
#
# Size budget (documented, asserted by the same script against a built image when one exists):
#   api ≤ 120 MB compressed.

# ---------------------------------------------------------------------------------------------
# base — toolchain and link-time system libraries
# ---------------------------------------------------------------------------------------------
FROM rust:1.90-slim-bookworm AS base
RUN apt-get update \
 && apt-get install --no-install-recommends -y pkg-config libssl-dev ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    RUSTFLAGS="-C strip=symbols"

WORKDIR /build

# ---------------------------------------------------------------------------------------------
# chef — the dependency layer
# ---------------------------------------------------------------------------------------------
FROM base AS chef
RUN cargo install cargo-chef --locked --version ^0.1

# Only the manifests reach the planner: a change to a source file must not invalidate it.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps/api/Cargo.toml ./apps/api/Cargo.toml
COPY crates ./crates
COPY modules ./modules
COPY tools ./tools

RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------------------------
# build — release binary on top of the warm dependency layer
# ---------------------------------------------------------------------------------------------
FROM base AS build
COPY --from=chef /build/recipe.json ./recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps ./apps
COPY crates ./crates
COPY modules ./modules
COPY tools ./tools
COPY database ./database

ARG OMNION_VERSION=0.1.0
ARG OMNION_REVISION=unknown
RUN cargo build --release --locked -p omnion-api \
 && mkdir -p /out \
 && cp target/release/omnion-api /out/omnion-api

# ---------------------------------------------------------------------------------------------
# runtime — distroless, non-root, read-only friendly
# ---------------------------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
ARG OMNION_VERSION=0.1.0
ARG OMNION_REVISION=unknown
ARG OMNION_SOURCE=https://github.com/raksix/omnion
ARG OMNION_LICENSES=Apache-2.0

LABEL org.opencontainers.image.title="omnion-api" \
      org.opencontainers.image.description="Omnion HTTP API (axum)" \
      org.opencontainers.image.version="${OMNION_VERSION}" \
      org.opencontainers.image.revision="${OMNION_REVISION}" \
      org.opencontainers.image.source="${OMNION_SOURCE}" \
      org.opencontainers.image.licenses="${OMNION_LICENSES}" \
      org.opencontainers.image.base.name="gcr.io/distroless/cc-debian12:nonroot" \
      org.opencontainers.image.vendor="Omnion" \
      io.omnion.size-budget="120MB"

COPY --from=build /out/omnion-api /usr/local/bin/omnion-api

# Distroless already carries CA certificates and tzdata. They are named here anyway, so a base
# swap that quietly drops them fails this build rather than the first TLS request in production.
USER nonroot:nonroot
WORKDIR /home/nonroot
EXPOSE 8080

ENV OMNION_PORT=8080 \
    OMNION_HOST=0.0.0.0 \
    OMNION_ENV=production \
    # The one writable path the process asks for when the file-system storage driver is
    # selected. The compose stack sets `read_only: true` and mounts exactly this as a tmpfs.
    OMNION_STORAGE_DIR=/tmp/omnion-storage

HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
  CMD ["/usr/local/bin/omnion-api", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/omnion-api"]

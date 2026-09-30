# syntax=docker/dockerfile:1.7
#
# Omnion CLI image (REQ-128, slice 1).
#
# The CLI is the small-company bootstrap path: `docker compose up` needs something that can run
# `omnion migrate` and `omnion setup` before the panel is reachable, and on a host with no Rust
# toolchain the image is that something. It is therefore a RUNTIME image here, not just a build
# target — the release pipeline builds the same binary for aarch64/amd64 as a separate
# `release/` artifact, and this file is what puts it on a host that has Docker and nothing else.
#
# Size budget: cli ≤ 40 MB compressed.

# ---------------------------------------------------------------------------------------------
# build — static binary on the musl target
# ---------------------------------------------------------------------------------------------
FROM rust:1.90-slim-bookworm AS build
RUN apt-get update \
 && apt-get install --no-install-recommends -y pkg-config libssl-dev musl-tools ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV CARGO_TERM_COLOR=never CARGO_INCREMENTAL=0

WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY modules ./modules
COPY tools ./tools
COPY database ./database

ARG OMNION_VERSION=0.1.0
ARG OMNION_REVISION=unknown
# `--fully-static` is what makes the runtime scratch stage possible: there is no libc to resolve
# at run time, so the image needs no base distribution at all.
RUN rustup target add x86_64-unknown-linux-musl \
 && cargo build --release --locked -p omnion-cli --bin omnion \
 && strip target/x86_64-unknown-linux-musl/release/omnion \
 && mkdir -p /out \
 && cp target/x86_64-unknown-linux-musl/release/omnion /out/omnion \
 && /out/omnion --version

# ---------------------------------------------------------------------------------------------
# runtime — the binary and nothing else
# ---------------------------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
ARG OMNION_VERSION=0.1.0
ARG OMNION_REVISION=unknown
ARG OMNION_SOURCE=https://github.com/raksix/omnion
ARG OMNION_LICENSES=Apache-2.0

LABEL org.opencontainers.image.title="omnion-cli" \
      org.opencontainers.image.description="Omnion command line (setup, doctor, migrate)" \
      org.opencontainers.image.version="${OMNION_VERSION}" \
      org.opencontainers.image.revision="${OMNION_REVISION}" \
      org.opencontainers.image.source="${OMNION_SOURCE}" \
      org.opencontainers.image.licenses="${OMNION_LICENSES}" \
      org.opencontainers.image.base.name="gcr.io/distroless/cc-debian12:nonroot" \
      io.omnion.size-budget="40MB"

COPY --from=build /out/omnion /usr/local/bin/omnion
USER nonroot:nonroot
WORKDIR /home/nonroot

# The CLI reads `OMNION_DATABASE_URL` and `OMNION_REDIS_URL` from the environment; it never
# takes a credential as an argument, because an argument is visible in `ps` to every process on
# the host. `omnion setup` prompts for the values it needs.
ENV OMNION_ENV=production

ENTRYPOINT ["/usr/local/bin/omnion"]
CMD ["--help"]

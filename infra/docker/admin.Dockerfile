# syntax=docker/dockerfile:1.7
#
# Omnion admin panel image (REQ-128, slice 1).
#
# Same recipe as the renderer below, because they are the same application: a Next.js app in a
# pnpm workspace, built with `output: standalone`, served by the Node runtime as an unprivileged
# user. Keeping one file with two targets rather than two files means the two images cannot drift
# on the parts that matter (the install layer, the non-root user, the health probe) — a drift that
# would only ever show up as "the renderer is mysteriously larger and runs as root".
#
# Size budget: admin ≤ 250 MB, web ≤ 250 MB.

# ---------------------------------------------------------------------------------------------
# deps — the pnpm install layer, cached on the lockfile alone
# ---------------------------------------------------------------------------------------------
FROM node:22-bookworm-slim AS deps
ENV PNPM_HOME=/pnpm PATH=/pnpm:$PATH COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN corepack enable && corepack prepare pnpm@11.7.0 --activate
WORKDIR /build
# Manifests first: a source change must not reinstall the world.
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml turbo.json ./
COPY apps/admin/package.json ./apps/admin/package.json
COPY apps/web/package.json ./apps/web/package.json
COPY packages ./packages
COPY themes ./themes
RUN pnpm install --frozen-lockfile

# ---------------------------------------------------------------------------------------------
# build — `next build` with standalone output
# ---------------------------------------------------------------------------------------------
FROM deps AS build
ARG OMNION_API_URL=http://127.0.0.1:8080
ARG NEXT_TELEMETRY_DISABLED=1
ENV NEXT_TELEMETRY_DISABLED=1 OMNION_API_URL=${OMNION_API_URL}
COPY apps/admin ./apps/admin
RUN pnpm --filter @omnion/admin build

# `output: standalone` is what makes the runtime image small, and it is the only reason the
# dependency layer above is worth caching: without it the image would ship the whole workspace.
RUN test -f apps/admin/.next/standalone/server.js \
 || { echo "apps/admin/.next/standalone/server.js is missing — next.config needs output: 'standalone'"; exit 1; }

# ---------------------------------------------------------------------------------------------
# runtime — Node, non-root, read-only friendly
# ---------------------------------------------------------------------------------------------
FROM node:22-bookworm-slim AS admin-runtime
ARG OMNION_VERSION=0.1.0
ARG OMNION_REVISION=unknown
ARG OMNION_SOURCE=https://github.com/raksix/omnion
ARG OMNION_LICENSES=Apache-2.0

LABEL org.opencontainers.image.title="omnion-admin" \
      org.opencontainers.image.description="Omnion admin panel (Next.js standalone)" \
      org.opencontainers.image.version="${OMNION_VERSION}" \
      org.opencontainers.image.revision="${OMNION_REVISION}" \
      org.opencontainers.image.source="${OMNION_SOURCE}" \
      org.opencontainers.image.licenses="${OMNION_LICENSES}" \
      io.omnion.size-budget="250MB"

ENV NODE_ENV=production \
    NEXT_TELEMETRY_DISABLED=1 \
    PORT=3000 \
    HOSTNAME=0.0.0.0 \
    # The admin panel forwards `/api/*` to this origin at the EDGE, so the browser never holds a
    # cross-origin session and no CORS rule is needed. The value is baked at BUILD time because
    # Next inlines it into the rewrites table.
    OMNION_API_URL=http://api:8080

WORKDIR /app
COPY --from=build /build/apps/admin/.next/standalone ./
# `static` and `public` sit outside `standalone` in a Next build; without them the page loads
# HTML and then 404s every chunk, which looks like a broken deploy rather than a missing copy.
# The admin panel has no `public/` directory today, so an empty one is created rather than
# `COPY`ing a path that does not exist and failing the build on it.
COPY --from=build /build/apps/admin/.next/static ./apps/admin/.next/static
RUN mkdir -p apps/admin/public

RUN groupadd --system --gid 1001 omnion \
 && useradd --system --uid 1001 --gid omnion --home-dir /app --shell /usr/sbin/nologin omnion \
 && chown -R omnion:omnion /app
USER omnion
EXPOSE 3000

# `next start` on the standalone server. `--hostname` is explicit because a container that binds
# 127.0.0.1 is unreachable from outside it and the failure looks like a broken image, not a
# misconfiguration.
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:'+(process.env.PORT||3000)+'/').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"

CMD ["node", "apps/admin/server.js", "--hostname", "0.0.0.0", "--port", "3000"]

# ---------------------------------------------------------------------------------------------
# web — the public renderer, same recipe, different app
# ---------------------------------------------------------------------------------------------
FROM deps AS web-build
ARG OMNION_API_URL=http://api:8080
ENV NEXT_TELEMETRY_DISABLED=1 OMNION_API_URL=${OMNION_API_URL}
COPY apps/web ./apps/web
RUN pnpm --filter omnion-web build
RUN test -f apps/web/.next/standalone/server.js \
 || { echo "apps/web/.next/standalone/server.js is missing — next.config needs output: 'standalone'"; exit 1; }

FROM node:22-bookworm-slim AS web-runtime
ARG OMNION_VERSION=0.1.0
ARG OMNION_REVISION=unknown
ARG OMNION_SOURCE=https://github.com/raksix/omnion
ARG OMNION_LICENSES=Apache-2.0

LABEL org.opencontainers.image.title="omnion-web" \
      org.opencontainers.image.description="Omnion public renderer (Next.js standalone)" \
      org.opencontainers.image.version="${OMNION_VERSION}" \
      org.opencontainers.image.revision="${OMNION_REVISION}" \
      org.opencontainers.image.source="${OMNION_SOURCE}" \
      org.opencontainers.image.licenses="${OMNION_LICENSES}" \
      io.omnion.size-budget="250MB"

ENV NODE_ENV=production \
    NEXT_TELEMETRY_DISABLED=1 \
    PORT=3000 \
    HOSTNAME=0.0.0.0 \
    OMNION_API_URL=http://api:8080

WORKDIR /app
COPY --from=web-build /build/apps/web/.next/standalone ./
COPY --from=web-build /build/apps/web/.next/static ./apps/web/.next/static
COPY --from=web-build /build/apps/web/public ./apps/web/public
RUN groupadd --system --gid 1001 omnion \
 && useradd --system --uid 1001 --gid omnion --home-dir /app --shell /usr/sbin/nologin omnion \
 && chown -R omnion:omnion /app
USER omnion
EXPOSE 3000
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:'+(process.env.PORT||3000)+'/').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["node", "apps/web/server.js", "--hostname", "0.0.0.0", "--port", "3000"]

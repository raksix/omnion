#!/usr/bin/env bash
# Runner for the `builder-help-dialog` proven-to-fail mutations.
#
# It is a wrapper rather than an inline command so the cron job's script field can hold a bare
# filename (Hermes resolves `~/.hermes/scripts/<name>`, and a whole `node …` command in that field
# fails every tick with "Script not found").
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/apps/admin"

# `env -i` because pm2's inherited environment makes node die with a fake SIGABRT on this box.
exec env -i HOME=/root \
  PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  node --experimental-strip-types features/workflows/builder-help-dialog.mutation.ts

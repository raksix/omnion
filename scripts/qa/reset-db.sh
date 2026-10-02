#!/usr/bin/env bash
# Omnion QA — reset the dedicated QA database.
#
# The walkthrough always runs against an empty database so that the first-run wizard (and every
# empty state) is exercised on every pass. Never point this at the development database.
set -euo pipefail

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa}"

docker exec "$CONTAINER" psql -U omnion -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

echo "[qa] ${DB} reset (container ${CONTAINER})"

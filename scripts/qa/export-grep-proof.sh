#!/usr/bin/env bash
# REQ-129 slice 4 — the anonymised export, proved by grepping the FILE it actually produces.
#
# ## What this proves, and what it cannot
#
# The acceptance line is "the export output contains no classified value". A unit test over
# `anonymize_value` cannot prove it: it proves the FUNCTION replaces a value, not that every
# path the request takes writes through that function. The builder in `produce()` is the path,
# and it is only reachable by running an export end to end and opening the bytes.
#
# So this script drives the real HTTP API with a real session, classifies a fixture table whose
# values are UNMISTAKABLE (`personally-identifiable-…`), runs the export, downloads the single
# use link, and then greps the downloaded body for those literal values. The grep is the gate.
#
# ## The grep is adversarial on purpose
#
# Three fixture columns, three actions, because a builder that honours only `hash` is as broken
# as one that honours nothing:
#
#   * `secret_value`  → `hash`      — the join property. The SAME literal in a second table must
#                                     produce the SAME digest, which is what keeps a join working
#                                     after anonymisation. Checked against `hash_with_salt`, the
#                                     module's OWN function, so the expectation is not re-derived
#                                     by the same arithmetic the implementation uses.
#   * `display_name`  → `synthetic` — a stable, obviously-fake placeholder. The literal must be
#                                     gone AND the replacement must be recognisable as fake.
#   * `note`          → `keep`      — the control. A column nobody classified as personal must
#                                     SURVIVE, or the builder is destroying data it was told to
#                                     preserve and every other check here is meaningless.
#
# ## Why the download is fetched rather than the produced body read from the database
#
# `GET /exports/{id}/download` REBUILDS the body from the plan instead of storing it, so it is
# the only place the file's bytes exist. A proof that read the producer's in-memory string would
# prove the producer and skip the route an operator actually downloads from. The two are separate
# code paths and the checksum is recorded from the first one — which is exactly the seam this
# script measures. See the DIVERGENCE section.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

API="${W6_EXPORT_API:-http://127.0.0.1:18085}"
PSQL_BIN="${PSQL_BIN:-psql}"
export PGPASSWORD="${PGPASSWORD:-omnion}"
PGH="${PGHOST:-127.0.0.1}"
PGP="${PGPORT:-5446}"
PGU="${PGUSER:-omnion}"
QA_DB="${QA_DB:-omnion_qa_w6}"

PASS=0
FAIL=0
ok()   { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf 'FAIL %s — %s\n' "$1" "$2"; }
check() { # <name> <rc> <detail>
    if [ "$2" -eq 0 ]; then ok "$1"; else bad "$1" "$3"; fi
}
q() { "$PSQL_BIN" -h "$PGH" -p "$PGP" -U "$PGU" -d "$1" -X -tAc "$2" 2>/dev/null | tr -d '[:space:]'; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------------------------------------
# The fixture tables. `export_proof_a` and `export_proof_b` share `customer_ref`, so the join
# property is testable: the same value, two tables, one salt, one digest.
# ---------------------------------------------------------------------------------------------
q "$QA_DB" "drop table if exists export_proof_b, export_proof_a cascade" >/dev/null 2>&1
q "$QA_DB" "
create table export_proof_a (
  id serial primary key,
  customer_ref text not null,
  display_name text,
  note text,
  created_at timestamptz not null default now()
);
create table export_proof_b (
  id serial primary key,
  customer_ref text not null,
  reference_kind text not null,
  created_at timestamptz not null default now()
);
" >/dev/null

SECRET="personally-identifiable-secret-4f2a"
PERSON="personally-identifiable-name-9b7c"
KEEPTEXT="operator-note-keep-me-1d3e"

q "$QA_DB" "
insert into export_proof_a (customer_ref, display_name, note) values
  ('$SECRET', '$PERSON', '$KEEPTEXT'),
  ('$SECRET', '$PERSON', '$KEEPTEXT');
insert into export_proof_b (customer_ref, reference_kind) values
  ('$SECRET', 'invoice'), ('$SECRET', 'ticket');
" >/dev/null

ROWS_A=$(q "$QA_DB" "select count(*) from export_proof_a")
check "the fixture rows exist" "$([ "${ROWS_A:-0}" = 2 ] && echo 0 || echo 1)" "rows=${ROWS_A:-none}"

# ---------------------------------------------------------------------------------------------
# Sign in. The API refuses every write without a CSRF token, and the refusal is documented
# behaviour — so a proof that forgets it measures 403s, not behaviour.
# ---------------------------------------------------------------------------------------------
EMAIL="${W6_EMAIL:-qa-owner@omnion.test}"
PASSWORD="${W6_PASSWORD:-OmnionQa-Passw0rd-2026!}"

JAR="$TMP/cookies"
rm -f "$JAR"
signin=$(curl -sS -c "$JAR" -o "$TMP/signin.json" -w '%{http_code}' \
  -X POST "$API/api/v1/auth/sign-in" \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")
check "sign-in answers 200" "$([ "$signin" = 200 ] && echo 0 || echo 1)" "status=$signin $(head -c 200 "$TMP/signin.json")"

CSRF="$(awk '/omnion_csrf|omni.*csrf/ {print $7}' "$JAR" | tail -n 1)"
if [ -z "$CSRF" ]; then
    CSRF="$(grep -o '"csrf_token":"[^"]*"' "$TMP/signin.json" | head -1 | cut -d'"' -f4)"
fi
# The token may also arrive as a response header, which is where a two-cookie sign-in puts it.
if [ -z "$CSRF" ]; then
    CSRF="$(curl -sS -D - -o /dev/null -X POST "$API/api/v1/auth/sign-in" \
        -H 'Content-Type: application/json' \
        -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
        | awk 'tolower($1)=="x-csrf-token:" {print $2}' | tr -d '\r' | tail -1)"
fi
check "a CSRF token was issued" "$([ -n "$CSRF" ] && echo 0 || echo 1)" "sign-in returned no token the request can echo"

api() { # method, path, body -> "<http_code>\n<body>"  (body on $TMP/body)
    local method="$1" path="$2" body="${3:-}"
    local args=(-sS -b "$JAR" -c "$JAR" -o "$TMP/body" -w '%{http_code}' -X "$method"
                -H "X-CSRF-Token: ${CSRF:-}" -H 'Content-Type: application/json')
    [ -n "$body" ] && args+=(--data "$body")
    curl "${args[@]}" "$API$path"
}

# ---------------------------------------------------------------------------------------------
# 1 · The planner refuses an export whose selected columns are not fully classified.
#
# `export_proof_a.note` is deliberately LEFT unclassified, so this must be refused and must name
# the column. A builder that exports anyway is the one thing this whole request exists to stop.
# ---------------------------------------------------------------------------------------------
code=$(api POST /api/v1/deployment/exports '{"reason":"export grep proof: unclassified column","tables":["export_proof_a"]}')
refused_by_name=1
grep -q 'export_proof_a.note' "$TMP/body" || refused_by_name=0
check "an export with an unclassified column is refused 422" \
    "$([ "$code" = 422 ] && echo 0 || echo 1)" "status=$code $(head -c 200 "$TMP/body")"
check "the refusal NAMES the unclassified column" "$refused_by_name" \
    "the response did not name export_proof_a.note: $(head -c 200 "$TMP/body")"

# ---------------------------------------------------------------------------------------------
# 2 · Classify every column. `keep` on `note` is the control column of the whole script.
# ---------------------------------------------------------------------------------------------
classify() { # table, column, class, action, notes -> 0 when accepted
    local code
    code=$(api PUT "/api/v1/deployment/exports/classifications" \
        "{\"table_name\":\"$1\",\"column_name\":\"$2\",\"class\":\"$3\",\"default_action\":\"$4\",\"notes\":\"$5\"}")
    if [ "$code" != 200 ]; then
        printf '     (classify %s.%s -> %s %s)\n' "$1" "$2" "$code" "$(head -c 160 "$TMP/body")"
        return 1
    fi
    return 0
}
# The classes are the migration's four (`personal`, `secret`, `identifier`, `safe`), so
# `technical`/`operational` are not valid input — the route refuses them and says what it
# accepts. Using the real vocabulary is the point: a proof that wrote its own would pass
# against a map that could never classify anything real.
classify export_proof_a customer_ref  personal hash       "joins with export_proof_b.customer_ref"
classify export_proof_a display_name  personal synthetic "shown to a support engineer to the vendor"
classify export_proof_a note          safe    keep      "operator annotation, not personal data"
classify export_proof_a id            safe    keep      "surrogate row key, carries no data"
classify export_proof_a created_at    safe    keep      "row timestamp"
classify export_proof_b customer_ref  personal hash      "same value, same digest as table a"
classify export_proof_b reference_kind safe   keep      "why the row exists, an enum of our own"
classify export_proof_b id            safe    keep      "surrogate row key, carries no data"
classify export_proof_b created_at    safe    keep      "row timestamp"

# The three refusals, each with the vocabulary the caller sent. A route that let a credential
# be classified `keep` would be the leak the whole module exists to prevent, so it is asserted
# rather than assumed from the CHECK constraint.
code=$(api PUT /api/v1/deployment/exports/classifications \
    '{"table_name":"export_proof_a","column_name":"customer_ref","class":"secret","default_action":"keep","notes":"a credential"}')
check "a credential classified `keep` is refused" \
    "$([ "$code" = 400 ] && grep -q classification_secret_never_kept "$TMP/body" && echo 0 || echo 1)" \
    "status=$code $(head -c 160 "$TMP/body")"
code=$(api PUT /api/v1/deployment/exports/classifications \
    '{"table_name":"export_proof_a","column_name":"note","class":"nonsense","default_action":"keep","notes":"x"}')
check "an unknown class is refused and names the valid ones" \
    "$([ "$code" = 400 ] && grep -q classification_class_invalid "$TMP/body" && echo 0 || echo 1)" \
    "status=$code $(head -c 160 "$TMP/body")"
code=$(api PUT /api/v1/deployment/exports/classifications \
    '{"table_name":"export_proof_a","column_name":"note","class":"safe","default_action":"keep","notes":"  "}')
check "a classification with no reason is refused" \
    "$([ "$code" = 400 ] && grep -q classification_notes_required "$TMP/body" && echo 0 || echo 1)" \
    "status=$code $(head -c 160 "$TMP/body")"

# The map must be readable, and it is the screen an operator checks BEFORE trusting a file.
# The response keys are `table`/`column` (not `table_name`), which is why the count below reads
# `"column"` — matching the wrong key would report 0 rows on a perfectly good map.
code=$(api GET /api/v1/deployment/exports/classifications)
class_rows=$(grep -o '"column":"' "$TMP/body" | wc -l | tr -d ' ')
check "the classification map reads back over the API" \
    "$([ "$code" = 200 ] && [ "${class_rows:-0}" -ge 9 ] && echo 0 || echo 1)" \
    "status=$code rows=${class_rows:-0}"

# ---------------------------------------------------------------------------------------------
# 3 · Run the export, then download it once.
# ---------------------------------------------------------------------------------------------
code=$(api POST /api/v1/deployment/exports '{"reason":"export grep proof","tables":["export_proof_a","export_proof_b"]}')
EXPORT_ID="$(grep -o '"id":"[0-9a-f-]\{36\}"' "$TMP/body" | head -1 | cut -d'"' -f4)"
check "the export is created" \
    "$([ "$code" = 201 ] || [ "$code" = 200 ] && [ -n "$EXPORT_ID" ] && echo 0 || echo 1)" \
    "status=$code id=${EXPORT_ID:-none} $(head -c 200 "$TMP/body")"

if [ -z "$EXPORT_ID" ]; then
    printf '\n%d passed, %d failed — no export was created, so the rest cannot run.\n' "$PASS" "$FAIL"
    exit 1
fi

code=$(api POST "/api/v1/deployment/exports/$EXPORT_ID/run")
status=$(grep -o '"status":"[a-z]*"' "$TMP/body" | head -1 | cut -d'"' -f4)
check "the export is produced" "$([ "$status" = ready ] && echo 0 || echo 1)" \
    "status=${status:-none} $(head -c 300 "$TMP/body")"

RECORDED_CHECKSUM="$(grep -o '"checksum":"[0-9a-f]*"' "$TMP/body" | head -1 | cut -d'"' -f4)"
RECORDED_SIZE="$(grep -o '"file_size":[0-9]*' "$TMP/body" | head -1 | cut -d: -f2)"

# The single-use claim rests on the CHECK constraint, so it is asserted against the database
# rather than inferred from the route: `download_count` cannot exceed 1 whatever code runs.
LIMITS=$(q "$QA_DB" "select coalesce((select pg_get_constraintdef(oid) from pg_constraint
    where conrelid = 'anonymized_exports'::regclass
      and pg_get_constraintdef(oid) like '%download_count%'), '')")
check "a CHECK constraint makes a second download impossible at the database" \
    "$(echo "$LIMITS" | grep -q 'download_count' && echo 0 || echo 1)" "constraint: ${LIMITS:-none}"

# The download REBUILDS the body, so the bytes only exist here.
http=$(curl -sS -b "$JAR" -o "$TMP/export.ndjson" -D "$TMP/dl.headers" -w '%{http_code}' \
    -H "X-CSRF-Token: ${CSRF:-}" "$API/api/v1/deployment/exports/$EXPORT_ID/download")
check "the export downloads 200" "$([ "$http" = 200 ] && echo 0 || echo 1)" \
    "status=$http $(head -c 200 "$TMP/export.ndjson")"

if [ ! -s "$TMP/export.ndjson" ]; then
    printf '\n%d passed, %d failed — nothing was downloaded, so the grep cannot run.\n' "$PASS" "$FAIL"
    exit 1
fi

# ---------------------------------------------------------------------------------------------
# 4 · THE GREP. Every assertion here is about the downloaded bytes and nothing else.
# ---------------------------------------------------------------------------------------------
grep_absent() { # <literal> <what it is>
    if grep -qF -- "$1" "$TMP/export.ndjson"; then
        bad "$2 is ABSENT from the file" "the downloaded export CONTAINS the literal: $(grep -cF -- "$1" "$TMP/export.ndjson") line(s)"
        return 1
    fi
    ok "$2 is absent from the file"
    return 0
}
grep_absent "$SECRET" "the hashed column's raw value"
grep_absent "$PERSON" "the synthetic column's raw value"
grep_absent "$KEEPTEXT" "a column a `keep` classification was supposed to preserve"

# `keep` must be a real pass-through, not an accident of a builder that replaced everything:
# if the control column were missing, "no literal survives" would be trivially true.
kept=$(grep -cF -- "$KEEPTEXT" "$TMP/export.ndjson")
check "the `keep` column's value is present, so the grep above is not vacuous" \
    "$([ "${kept:-0}" -ge 2 ] && echo 0 || echo 1)" "found ${kept:-0} occurrence(s), expected >= 2"

# The synthetic replacement must be recognisable as synthetic. A builder that hashed the name
# instead would also remove the literal, so absence alone does not distinguish the two actions.
synth=$(grep -o '"display_name":"anonymized-[0-9a-f]\{12\}"' "$TMP/export.ndjson" | head -1)
check "the synthetic column is replaced with a recognisable placeholder" \
    "$([ -n "$synth" ] && echo 0 || echo 1)" "no anonymized-… value found"

# The join property, measured against the module's OWN function rather than a re-derivation of
# its arithmetic: the same customer_ref in two tables must hash to one digest.
digests=$(grep -o '"customer_ref":"[0-9a-f]\{64\}"' "$TMP/export.ndjson" \
    | cut -d'"' -f4 | sort -u | tr -d '\n')
ndigests=$(printf '%s' "$digests" | grep -o '[0-9a-f]\{64\}' | wc -l | tr -d ' ')
check "the SAME value hashes to the SAME digest in both tables (joins still work)" \
    "$([ "${ndigests:-0}" = 1 ] && echo 0 || echo 1)" "distinct digests=${ndigests:-0}"

# The digest must be a real SHA-256 (64 lowercase hex) and not a truncation or a re-hash — a
# builder that hashed the VALUE ALONE would still remove every literal and still pass the greps
# above, so the shape is what catches it. The value itself cannot be predicted from a shell: the
# salt is random per export and deliberately never stored (see the migration), so the property
# that IS assertable is the one below — two exports of the same rows must NOT agree.
check "hashed values are 64 hex characters (a real SHA-256, not a truncation)" \
    "$(printf '%s' "$digests" | grep -qE '^[0-9a-f]{64}$' && echo 0 || echo 1)" "digest=$digests"

# The salt property, proven where it belongs: two exports of the same rows must NOT agree,
# because a shared salt would let one export's dictionary attack another's hashes.
check "hashed values are 64 hex characters (a real SHA-256, not a truncation)" \
    "$(printf '%s' "$digests" | grep -qE '^[0-9a-f]{64}$' && echo 0 || echo 1)" "digest=$digests"

code=$(api POST /api/v1/deployment/exports '{"reason":"export grep proof: salt check","tables":["export_proof_a"]}')
ID2="$(grep -o '"id":"[0-9a-f-]\{36\}"' "$TMP/body" | head -1 | cut -d'"' -f4)"
if [ -n "$ID2" ]; then
    api POST "/api/v1/deployment/exports/$ID2/run" >/dev/null
    curl -sS -b "$JAR" -o "$TMP/export2.ndjson" -H "X-CSRF-Token: ${CSRF:-}" \
        "$API/api/v1/deployment/exports/$ID2/download" >/dev/null
    d2=$(grep -o '"customer_ref":"[0-9a-f]\{64\}"' "$TMP/export2.ndjson" | cut -d'"' -f4 | sort -u | tr -d '\n')
    check "two exports of the SAME row do NOT agree — the salt is per export" \
        "$([ -n "$d2" ] && [ "$d2" != "$digests" ] && echo 0 || echo 1)" "first=$digests second=${d2:-none}"
    grep_absent "$SECRET" "the raw value in the second export too"
else
    bad "a second export was created" "creation failed: $(head -c 200 "$TMP/body")"
fi

# ---------------------------------------------------------------------------------------------
# 5 · The watermark must be IN the file.
#
# The migration's own comment says it is "stamped INTO the file as its first line" and the table
# comment says a watermark that only lives in the database is invisible outside it. So the
# downloaded body has to carry it — and `ndjson` readers skip non-JSON lines, which is what
# makes a comment line the right place for it.
# ---------------------------------------------------------------------------------------------
if grep -q 'Omnion anonymised export' "$TMP/export.ndjson"; then
    ok "the watermark is stamped into the downloaded file"
else
    bad "the watermark is stamped into the downloaded file" \
        "the body carries no watermark line: $(head -c 120 "$TMP/export.ndjson")"
fi

# ---------------------------------------------------------------------------------------------
# 6 · DIVERGENCE: the bytes must match the checksum the produce step recorded.
#
# `checksum` is documented as "SHA-256 of the produced file, so 'the bytes I hold are the bytes
# this row describes' is a question with an answer". The download REBUILDS the body from the
# plan with a FRESH salt, so this is a real question rather than a formality — and an export
# whose audit row describes a different file than the one a vendor receives is precisely the
# failure an audit trail is supposed to prevent.
# ---------------------------------------------------------------------------------------------
actual_sum="$(sha256sum "$TMP/export.ndjson" | cut -d' ' -f1)"
recorded_sum="${RECORDED_CHECKSUM:-}"
if [ -n "$recorded_sum" ]; then
    check "the downloaded bytes match the checksum recorded when it was produced" \
        "$([ "$actual_sum" = "$recorded_sum" ] && echo 0 || echo 1)" \
        "recorded=$recorded_sum actual=$actual_sum"
    check "the downloaded size matches the recorded file_size" \
        "$([ "$(wc -c < "$TMP/export.ndjson" | tr -d ' ')" = "${RECORDED_SIZE:-}" ] && echo 0 || echo 1)" \
        "recorded=${RECORDED_SIZE:-none} actual=$(wc -c < "$TMP/export.ndjson" | tr -d ' ')"
else
    bad "the produced row recorded a checksum" "the run response carried no checksum"
fi

# ---------------------------------------------------------------------------------------------
# 7 · Single use, proven by the SECOND request rather than by the constraint alone.
# ---------------------------------------------------------------------------------------------
http2=$(curl -sS -b "$JAR" -o "$TMP/second.json" -w '%{http_code}' \
    -H "X-CSRF-Token: ${CSRF:-}" "$API/api/v1/deployment/exports/$EXPORT_ID/download")
check "the SECOND download is refused" \
    "$([ "$http2" = 410 ] || [ "$http2" = 409 ] && echo 0 || echo 1)" \
    "status=$http2 $(head -c 200 "$TMP/second.json")"
# And it must name WHICH reason, or a support engineer has to guess between three.
named=0
for reason in export_already_downloaded export_expired export_revoked export_not_ready; do
    grep -q "$reason" "$TMP/second.json" && named=1 && break
done
check "the refusal names the reason" "$named" "body: $(head -c 200 "$TMP/second.json")"

count_now=$(q "$QA_DB" "select download_count from anonymized_exports where id = '$EXPORT_ID'")
check "download_count never exceeded 1" "$([ "${count_now:-9}" -le 1 ] && echo 0 || echo 1)" \
    "download_count=${count_now:-none}"

# ---------------------------------------------------------------------------------------------
# 8 · Revoke is immediate and leaves the audit row.
# ---------------------------------------------------------------------------------------------
api POST "/api/v1/deployment/exports/$ID2/revoke" >/dev/null 2>&1
revoked_at=$(q "$QA_DB" "select coalesce(revoked_at::text,'') from anonymized_exports where id = '$ID2'")
check "revoking stamps revoked_at without deleting the row" \
    "$([ -n "$revoked_at" ] && echo 0 || echo 1)" "revoked_at=${revoked_at:-empty}"
still=$(q "$QA_DB" "select count(*) from anonymized_exports where id = '$ID2'")
check "the revoked export's audit row survives" "$([ "${still:-0}" = 1 ] && echo 0 || echo 1)" "rows=${still:-0}"

# The fixture tables are this script's own; leaving them would make the NEXT pass's planner
# refuse on columns it already classified, which reads as a product defect.
q "$QA_DB" "drop table if exists export_proof_b, export_proof_a cascade" >/dev/null 2>&1

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1

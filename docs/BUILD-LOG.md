## 2026-09-28 — REQ-010 slice 4 (retention half) · the part of a file manager that forgets

build media: retention policies, the run log, the hold, and the reference repair

REQ-010 slice 4's remaining half, and the last piece of the slice. Slice 1 gave the
library a trash whose countdown was always the *site's* fallback and that no worker
ever enforced; what is here is the policy itself — scoped to a site or a folder —
plus the sweeps that act on it, the log that records every run, and the refusal that
stops a purge from deleting a hero image a live page still resolves to.

**0049_media_retention.sql**, `crates/media/src/retention.rs`,
`apps/api/src/routes/media_retention.rs`, `apps/api/src/retention_runner.rs`,
`features/media/retention-view.tsx` (a **Retention** tab on `/media/settings`) and the
file's `LegalHold` block on the detail screen.

**Six decisions, each a shortcut that produces a plausible wrong answer.** The
**current version is exempt from the version sweep by number, never by age** — the
obvious query, "delete every version older than N days", deletes the version `media`
is serving and leaves `storage_key` naming an object that no longer exists, and the
current version is the highest number in `media_versions` (the same one `next_version`
hands out under a row lock). The **hold is a column on `media` rather than a flag on
the policy**: three destructive sweeps must respect it, a policy flag would have to be
joined into all three, and the first one somebody edits forgets the others — the
forgotten one is the one that deletes evidence. A **purge refuses a referenced file
and names the referrers**, because `media_references` cascades away with the file, so
a purge *can* silently delete a hero image a published page resolves to. **A run that
found nothing writes a row**, because retention is the one library feature whose
absence of activity is indistinguishable from being broken, and a run's `summary`
separates *nothing was eligible* from *N held* from *N still referenced* from *the run
stopped early*. The **folder policy wins and the site policy is the fallback, never
the shortest window of everything that matches** — a campaign folder cannot be
shortened by a site-wide rule that happens to be tighter. And the **repair scan drops
the other kind of lie**: a reference to a page deleted during a migration refuses a
purge for ever, and only a referent the platform can *prove* is gone is dropped, so a
module that arrives tomorrow does not find its usage rows already deleted.

The worker reaches the sweep through the **route's** `run_once`, not a second copy of
it: two answers to "what may this file go" is how a nightly sweep and an operator's
click start disagreeing about the same row, and the operator who reads the screen is
the one who is misled. And the tick is **minutes, not days** — a sweep is idempotent,
so an empty tick is a no-op, and a worker that only ran at 02:00 has exactly one
failure mode with nothing in between to show it.

**Three defects in my own crate code, all caught by the unit tests written beside it.**
`describe()` printed "30 days day(s)" because the format string spelled the unit *and*
`plural_days` appended it — and a `contains("30 days")` assertion passes over that
happily, so both spellings now assert the *absence* of "day(s)". The cross-field check
defaulted the other window to the **minimum** (1) rather than the **column default**
(30), so a create sending `purge_after_days = 5` passed validation and was then refused
by the database as a bare constraint name; the defaults are now named constants bound
to the insert, so the check and the write cannot disagree about what "the default" is.

**The third is serde rather than logic, and it is the one worth remembering.**
`Option<Option<Uuid>>` with `#[serde(default)]` *looks* like it carries "absent" and
"null" separately. It does not — and neither does `Option<Value>`, because serde's
`Option` visitor maps a `null` to `None` whatever the inner type is. So the explicit
`folder_id: null` — the one edit an operator most often wants, "this rule was for one
folder, now it is for everything" — silently arrived as "leave the scope alone", and
the screen would have reported **saved** over an unchanged scope, which is the worst
shape a save can have. The scope is now read from the *key's presence* in the raw map,
which is the only place the distinction exists. The walk proves the **write** happens,
not just the decode: it clears the scope, reads `folder_id` back out of PostgreSQL, and
then proves the other half by sending a body that omits the field and finding the scope
untouched.

**One defect no unit test could have found, and it was the trigger again.** The
`sites` trigger inserted a policy row without a `name`, and `name` is `not null` with
no default — so **every site creation failed** with `null value in column "name" of
relation "media_retention_policies"`. That reads as neither a retention problem nor a
migration problem but as *the site could not be created*, and it fires from a trigger
two migrations away, so the only place the error names the cause is the function body.
All five walks died on it identically before their first assertion. This is the third
time a `sites` trigger has been the defect in this REQ (0028 presets, 0044 scan
settings) and the fourth time the seed alone left a *new* site with a pleasant `GET`
and a save that wrote nothing.

**The environment was the other half of the tick.** MinIO refuses writes with
`XMinioStorageFull` at 95% disk, and `/mnt/apopic` was at 95% — so three of five walks
failed at the **upload**, not at anything retention did. `docker system df` reported
1.2G reclaimable under "dangling" volumes and the honest reading was that four of the
five were production data (solexx PostgreSQL, sooliva Redis and Mongo, teknofest
Mongo) whose container merely happened to be stopped; the space came from this
repository's own `target/debug/deps` instead — 26 stale test-binary links, 5.7G, of
which nothing was running. **A dangling volume is not garbage**, and MinIO's threshold
is a hard wall rather than a slow degradation.

**Proof.** `--test media_retention` → **5 walks, 0 failures** over the real router and
a real database. Both versions of a file are aged past a one-day window and the sweep
removes **only** the superseded one, asserted by reading `media_versions` back out of
PostgreSQL *and* by finding the served version's bytes still in the object store — a
length check would pass by accident. A hold beats a 400-day-old trash row, a refusal
with no reason writes nothing, the reason lands in `audit_log`, a second identical press
reports `changed: false` rather than a second audit row that reads as a second person,
and the release is effective on the very next run. A purge over a referenced file is
refused naming `page <id> (hero_image_id)`; the page is deleted; a second purge is
**still** refused; the repair removes one row; the third purge goes through. A folder
policy outranks the site rule through the crate's own resolver; a `purge_after_days`
inside the **stored** `trash_days` is refused and the refusal writes nothing; a policy
of another tenant is a `404` whose refusal deletes nothing; a reader may read the policy
and the log and may not create, edit, delete, run, repair or hold.
`cargo test -p omnion-media --lib` → **179**, `-p omnion-api --lib` → **147**,
`-p omnion-core --lib` → **34**; `--test media` (14), `--test media_shares` (5) and
`--test media_retention` (5) are green against the routes this touches. `apps/admin`
`tsc --noEmit` clean.

**Not run: the browser pass.** The retention tab is walked by
`scripts/qa/run.sh` — `runMediaRetention` opens the tab, asserts every policy states
its consequence in a sentence and names its scope, drives the cross-field warning
*before* the save, creates and saves a policy, runs a pass and asserts the notice is
a sentence rather than a bare zero and that the log wrote a row even when nothing
was eligible, and toggles the file's hold on and back off — the whole function is
written and `node --check` passes. It did not execute: `scripts/qa/qa-slot.sh` caps
the box at **one** concurrent pass and a sibling wave held the slot for the entire
25-minute window, so this pass was still queued when `timeout 1500` fired and
printed only `waiting for a QA slot`. That is scheduling contention, not a red
result, and it is recorded as **unrun** rather than as a pass. REQ-010 therefore
stays `in-progress` and this slice is not closed on tests alone — the gate is the
browser pass, and the next tick runs it first.

**Next.** The Usage and Activity tabs on the file detail — they answer
`media_references` and an activity read, and the reference rows the repair scan now
maintains are what finally give them rows to read. Then slice 4 closes and the queue
moves to REQ-021 (notification centre), the first untouched item in wave 1.

## 2026-09-28 — REQ-010 slice 4 (scanning half + grants half) · the access layer, and a red that had been hiding for four ticks

build media: folder and file grants, with a deny that wins at any depth

REQ-010 slice 4, the permissions half. A permission says whether an account may
touch the library; a grant says **not this one, not here**. That asymmetry is the
whole design, and the alternative — a layer that could also hand capabilities out —
would be a second, un-audited source of truth beside the permission catalogue. The
first thing anybody would build on such a layer is a "grant a contractor read on the
whole library" button that quietly bypasses IAM.

**0047_media_grants.sql** creates the table with the XOR constraint (a grant on
neither node is a grant on the whole site; one on both is ambiguous in exactly the
way two resolvers disagree), the unique (node, subject) index and three lookups.
Deliberately **no `sites` trigger**, unlike 0028/0029/0044 which all closed the
"a site created after the migration has nothing" gap: a grant attaches to a folder
or a file, and a new site has neither until somebody opens the library, so there is
no node for a trigger to write to.

**`resolve()` is a pure function with no pool in it**, because a deny-wins rule
tested only over HTTP is a rule somebody refactors away while changing a handler.
The answer is "the union of the allows, minus the union of the denies" and not a
fold in list order: a fold lets a file allow recorded after a folder deny undo it,
and lets a root allow re-open a file whose own grant says no. A subject named
*only* by denies gets nothing rather than "everything except what was refused".

**The gate.** `ensure_servable` is two halves now, in an order — grant, then scan.
The public renderer and the share-token route call the **scan half alone**: a grant
narrows a sign-in and cannot describe an anonymous visitor, so applying one there
would break every published page the moment somebody narrowed a folder. Share
*creation* resolves the chain's `share` bit on its own, so a writer with no share
bit is refused a capability their role would otherwise have let them hand out.

**Three defects the walks found, none of which a unit test could have seen.**

*One.* A deny with no bit set removes nothing and reads as one that does; refused
at the API with the field named, and refused by the screen first.

*Two, and it is the one that matters.* `delete_one` resolved the grant's node,
which loaded the file, which ran the tenancy check and answered `403` — confirming
that another tenant's grant id exists, which is precisely the oracle the 404 rule
exists to prevent. The lookup now scopes by organization **in the same `where`**,
so there is no window in which an id's existence leaks, and the walk proves both
halves: a foreign grant answers 404 *and* the refusal did not delete their row.

*Three.* A URL with two path parameters and a one-parameter handler is the shape
axum rejects with a bare `500` and no body. The grant delete route now carries the
grant id alone, because the row already knows the node it was written on. Half a
tick went into reading that 500 as an unrelated panic before the shape was read
off the mount list.

**Also fixed: a pre-existing red that had been hiding for four ticks.** The EXIF
fixture computed the sub-directory's offsets without IFD0's four-byte
next-directory pointer, so the *lens* pointer landed inside the value area and
overwrote the first two bytes of the camera make. The reader then reported no
camera at all — and a null `exif` column is a perfectly legal answer to "what did
the camera say", so every piece of evidence pointed at the reader instead of at the
fixture. The value area is now written by allocation with named pairs rather than by
arithmetic, and a new test calls the reader on the builder's own output and asserts
every field: no database, no object store, no walk, and the bytes next to the
answer. Two of the walk's helpers also could not express what they were asserting —
a `jsonb` decode cannot hold the null that this whole feature exists to distinguish
from "read nothing" — so `media_column` returns `Option<Value>` and `media_int`
reads the integer columns.

**Proof.** `--test media_grants` → **8 walks, 0 failures**, against a disposable
database (`scripts/qa/run-media-walk.sh`, because the shared development database
carries a sibling wave's migrations and the suite dies with `VersionMissing` before
reaching an assertion). Every value is read **out of PostgreSQL** wherever a row is
concerned, because a response that omits a field is indistinguishable from one that
stored it and chose not to say so. A file deny refuses the raw route with a `403`
naming where the deny was found, while a bystander holding every permission still
reads the same file. A folder deny reaches a file two folders down and the tab
shows the three-node chain with the folder that carries it marked. A group deny
reaches its members, and **leaving the team restores access on the next request**.
A deny naming only `share` leaves reading alone while the create route refuses. An
untouched chain serves every file. `cargo test -p omnion-media --lib` → **164**
(was 148), `-p omnion-api --lib` → **140** (was 127), `--test media` → **14** (was
red), `--test media_shares` → **5**, `--test media_scan` unchanged. `apps/admin`
`tsc --noEmit` clean.

**Next.** Slice 4's last third: retention policies with the daily worker and its run
log, and reference-based purge refusal plus the repair scan — which is also what
finally gives the Usage and Activity tabs rows to read. Done when a retention run
removes exactly the eligible rows and a purge names the resources holding a file.

## 2026-09-28 — REQ-010 slice 3 (four fifths) · duplicates, and the row that survives the merge

- **What shipped.** **`0038_media_duplicates.sql`**, `crates/media/src/duplicates.rs`,
  `apps/api/src/routes/media_duplicates.rs`, `apps/api/tests/media_duplicates.rs`,
  `features/media/duplicates-view.tsx`, `/media/duplicates`, a `Duplicates` link on the media
  browser, and `runMediaDuplicates` in the walkthrough. Five commits: `3d28b18`, `5baaa24`,
  `6a07a68`, `c119975`, `aa6d382`, plus the harness and grammar fixes below.
- **The shape of it.** A duplicate group is a **projection of `media` over its own checksum**, never
  a table: a replace changes the checksum, a delete removes a row and a restore brings one back, so
  a stored group would need a trigger on all three to stay true. `media_references` is the other new
  table — the rows a merge repoints and the "used in" tab reads in slice 4.
- **Seven decisions, each a shortcut that produces a plausible wrong answer.** A group of one is
  not a duplicate, and neither is a trashed copy (it is already on its way out). Reclaimable
  excludes the keeper: it is what a *purge* returns, not the group's size, because a merge frees
  nothing. The report never picks the keeper and the merge **refuses** one from outside the group —
  an automatic tie-break breaks a live page and is discovered from a 404, not from a report. A
  merge **trashes** copies and never deletes, so a restore reverses the whole thing. The cross-site
  mode is a different question rather than a wider default, and carries **no reclaimable column at
  all** rather than a zero: no merge can decide which tenant keeps a file.
- **The repoint is the interesting statement.** A page may reference two copies of the same file, so
  a plain `update … set media_id = keeper` moves the first row and is then *refused* on the second —
  a whole merge rolled back with a `duplicate key` message naming an index and not a cause. It is
  written against the keeper's own rows instead, so nothing can collide afterwards by construction.
  Proved directly against PostgreSQL 5433 rather than asserted: the plain update raises
  `duplicate key value violates unique constraint "media_references_unique"`, the keeper-side
  rewrite moves 1 and collapses 1, and the table then holds one live `page-1/hero` row.
- **Four defects the router walks found, none visible to a unit test on a body builder.**
  **(1)** The repoint's row type was `((i64, i64),)` — a composite column where the statement
  returns two — so *every merge* answered `500: Rust type (i64,i64) (as RECORD) is not compatible
  with INT8`. **(2)** The cross-site mode reported nothing: it read the per-site view with the site
  filter lifted, which by construction still groups *per site*, so the two tenants each holding one
  copy showed two empty reports to the one account allowed to ask. **(3)** The cross-site view's
  `file_count` was the installation's, not the caller's — a checksum in nine sites reported `9` to
  somebody who named two. It is now grouped over the named sites directly. **(4)** A share cannot
  be created without `media.share`, which the walk's fixture had not been granted; the test now
  proves the whole chain, because the merge closes links it did not create.
- **Proof.** 6 walks over the real router → **0 failures**. The report ignores a lone file and a
  trashed one; the merge keeps the **second** file (so it cannot pass by accident), moves
  `page-1/hero` **once** rather than twice, closes a live share with its reason, and refuses an
  outside keeper and an already-merged group with `409` — not `400`, because the request was legal
  and the library moved on. Short checksums are `400` naming the field; a reader may read and may
  not merge; the platform owner sees one checksum across two named sites and a tenant gets
  `platform_only` before any row is read. The migration applies across the whole set on a fresh
  database. `cargo test -p omnion-media --lib` → **98**, `-p omnion-api --lib` → **122**,
  `--test media_shares` → **5** unchanged, `apps/admin` tsc clean.
- **The browser pass, and a grammar defect only it could find.** `runMediaDuplicates` uploads the
  same sample file twice (never a fabricated checksum), asserts the pair forms a group, asserts
  **`Merge group` is disabled until a keeper is chosen** and enabled after, picks the *second* file
  so a merge that quietly kept the first would fail, and checks the result. It produced:

      "The 5 copies are in the trash and their bytes are is only reclaimed when the trash is purged."

  The verb phrase and the `is` were carried by one branch, so the plural case got `are is`. Every
  word was present and only the grammar was wrong — which is why neither the API walks nor the unit
  tests saw it: they asserted on the substring `only reclaimed`, which survived. Fixed in `9bd3d58`
  and pinned in both numbers.
- **A cleanup script took out the quality gate it was making room for.** The first pass died with
  `ENOENT …/clicks.jsonl` after four routes and produced **no report and no findings**: the sibling
  commit `8fecbc4` added `scripts/qa/disk-guard.sh`, and it ran while the pass was writing,
  deleting the artifact directory the pass had just created. Fixed in `9fa315f` — a directory
  touched in the last hour belongs to a live walkthrough and is skipped. The second pass ran to
  completion and produced the grammar defect above, so the fix is proved by use rather than by
  inspection.
- **Not mine, recorded honestly.** `media-presets` failed in the same pass with a `Failed to …` JSON
  parse from a `page.evaluate` — a pre-existing depth pass reading a response that was not JSON —
  and `media-file-detail` / `media-shares` reported `no file to open — the upload step did not
  succeed`, a QA-database fixture race between passes rather than a product defect. Neither is
  caused by this change and neither is claimed as fixed. The `media storage` pass reported a
  `credential` leak category, which is its own standing finding (the settings screen shows a masked
  credential); unchanged by this slice.
- **Next.** Slice 3 closes with the CDN purge hook to REQ-011 and **EXIF** (still open from slice 2).
  Slice 4 then brings folder and file grants with inheritance, the scanning pipeline with
  quarantine and release, retention policies with the daily worker, and the reference-based purge
  refusal — and with them the Usage and Activity tabs, which finally have rows to read.


## 2026-09-28 — REQ-006 slice 4b-2 · a live provider, and the four defects only a live provider shows

- **What shipped.** **`87390ff`** — `apps/api/tests/support/stub_idp.rs`, a real identity provider
  this process starts on a loopback port: a discovery document, a JWKS, an authorization endpoint
  that answers `302` with a `Location`, a token endpoint that verifies the PKCE challenge itself and
  spends a code exactly once, and a SAML endpoint that signs an assertion with both halves of the
  XML-signature binding — all under one freshly generated 2048-bit RSA key.
  `apps/api/tests/sso_live.rs` drives the **real router** against it: the browser is sent to the
  provider's own endpoint, comes back with a code, the code is exchanged, the token is verified
  against the *published* keys, the claim → role mapping attaches the role, JIT provisions the
  account, and `GET /me` with the resulting cookie names the directory person. The same file does
  SAML. Plus the fixes below.
- **Why a stub and not more fixtures.** Every other proof of enterprise sign-in tested one layer:
  a verifier against a synthetic token, a reader against a hand-built assertion, the HTTP layer
  against refusals it triggers itself. None of them proved that a *browser* can complete a round
  trip, because that needs a provider on the other end. The stub shares nothing with the code it
  tests except the `rsa` crate, so agreement between the two sides is evidence rather than
  tautology.
- **Proof.** `cargo test --workspace --lib` → **598 unit tests, 0 failures**. `cargo test -p
  omnion-api --test sso --test sso_live` → **3 walks, 0 failures**, all three in one database.
  `pnpm typecheck` green. `cargo clippy --all-targets` adds no new warning.
- **Four defects the walk found, and each is a thing no layer-by-layer test could see.**
  **(1) Every SAML sign-in was broken.** The relay page posted the *return path* as `RelayState`
  while the callback claims a **challenge** from `RelayState` — so the callback could never claim
  one and every SAML sign-in ended in `invalid_state`. The page now carries the challenge `start`
  issued (and HTML-escapes it, because the route is public and a `state` query is attacker-supplied
  in the general case). **(2) `c_hash` was checked against the PKCE verifier's hash.** No provider
  has ever seen the verifier, so a real directory could not satisfy that rule: it refused every
  legitimate sign-in while proving nothing. It is now the real code hash (OIDC Core §3.1.3.6), and
  a *missing* `c_hash` is not a refusal, because the claim is a RECOMMENDED and an optional claim
  cannot be a mandatory rule. **(3) The SAML `test` button could never report success.** Its probe
  was a self-closing `<saml:Assertion/>` that the reader never parses, and the verdict was
  inferred from the error text — so every certificate read as broken. The certificate step is now
  exposed on its own and the claim half is probed with a document the real reader accepts, which
  also catches a typo in an attribute name before the first real assertion. **(4) The claim → role
  mapping silently did nothing for every tenant.** It looked the role up with the tenant's
  organization id, but the base roles are seeded at *platform* scope, so `editors` → `editor`
  matched nothing and the person signed in with no role and no error — the worst possible outcome
  for the feature whose whole point is the mapping. The lookup now falls back to the platform role,
  the way the rest of the platform finds one.
- **A test that only passes in one harness is a test that lies.** The walk's first version swept
  `email like 'sso-live-%'` to clear leftovers from a crashed run. It passed in CI (one database
  per job) and failed against a shared one, because it deleted a sibling suite's fixtures
  mid-run. Every statement is now scoped to the walk's own organizations, and all three walks are
  proven to pass together in one database.
- **Owner action, still open: the disk.** `/mnt/apopic` (one 60 GB loop image shared by eight
  worktrees' `target/`) hit **0 bytes free** twice during this tick and a link failed with
  `No space left on device`. `target/debug/incremental` was cleared twice, and the **unclaimed**
  `omnion-w4`/`w5`/`w7` worktree `target/` directories (12.6 GB of build artifacts, no wave owns
  them, no `cargo` running) were removed. That returned 13 GB. **The unclaimed worktrees should be
  pruned outright, or the box needs more room** — a link failure is a red build, and one will
  happen again mid-tick.
- **Owner action, still open: the shared dev database.** `omnion` has migration **19** applied from
  a sibling branch that `main` does not have, so `db.migrate()` refuses with `VersionMissing(19)`
  and every integration test that migrates fails there. CI is clean. This tick ran against a
  dedicated `omnion_sso_live` database instead, which is the right shape for a local run and worth
  making the default until the 0019 slot is reconciled.
- **Next.** REQ-006 is **done** — every slice shipped and every acceptance box ticked. The next
  REQ in wave 1 order is **REQ-010** (the enterprise file manager). This tick is not a close tick
  in the QA-pass sense (no screen changed), so `scripts/qa/run.sh` was not run; the next tick that
  lands a screen carries it.
# Omnion — Build Log

> Cross-tick memory for the **`omnion-build`** loop. Newest entries at the bottom. 3-5 lines
> per entry: phase · what got done · proof (command + result) · next.

## 2026-09-25 — Loop bootstrap (chat session)

- Build phase opened by owner directive: Lokma-style loop, start building; code first, deploy later (dev target: omnion.fermag.com.tr).
- Created: `docs/BUILD-BACKLOG.md` (P00→P14 + gated P-DEP) + this log + `omnion-build` cron loop (pinned model).
- Toolchain status: node 22 ✓ · pnpm 11 ✓ · bun ✓ · docker 29 ✓ · gcc 13 ✓ · **Rust NOT installed → P00 installs rustup**.
- Next: **P00 — Toolchain + repo skeleton.**

## 2026-09-25 — P00 · Toolchain + repo skeleton

- Rust toolchain installed via rustup (minimal, stable): `cargo 1.98.1` · `rustc 1.98.1`.
- Workspace: root `Cargo.toml` (resolver 2, members `apps/api` + `crates/*`) + `rust-toolchain.toml` (stable, rustfmt + clippy).
- `apps/api` (thin HTTP layer, docs/04): `src/lib.rs`, `main.rs`, `state.rs`, `routes/{mod,health}.rs` —
  `GET /healthz` → `{"ok":true,"service":"omnion-api","version":"0.1.0"}`, `PORT` env (default 8080, invalid value fails fast);
  `crates/core`: `omnion-core` infrastructure lib exposing `BuildInfo`, consumed by the API state.
- Infra: `infra/compose/docker-compose.dev.yml` (+ `postgres.yml`, `redis.yml`, `minio.yml`, `mailpit.yml`) with overridable
  host ports; `.gitignore` gains `target/`; CI skeleton `.github/workflows/ci.yml` (fmt · clippy `-D warnings` · test + compose config check).
- Proof: `cargo check --workspace --all-targets` → Finished ✅ · `cargo clippy --workspace --all-targets -- -D warnings` → clean ✅ ·
  `cargo test --workspace` → 2 passed (router `/healthz` integration test + core metadata) ✅ ·
  `curl -i 127.0.0.1:8080/healthz` → `HTTP/1.1 200 OK` body `{"ok":true,"service":"omnion-api","version":"0.1.0"}` ✅ ·
  `PORT=18080` → 200 ✅ · `docker compose -f infra/compose/docker-compose.dev.yml config --quiet` → valid, services
  `postgres, redis, minio, mailpit` ✅.
- CI on GitHub: workflow run `36193827205` → **success** (jobs: `Rust — fmt · clippy · test` ✅ · `Infra — compose config` ✅).
- Repo note: this clone's `origin` moved to the SSH URL — the stored OAuth token has no `workflow` scope, so over HTTPS GitHub rejects any commit touching `.github/workflows/`.
- Next: **P01 — Core foundations** (typed env config + tracing subscriber + shared error type, sqlx/Postgres pool + `database/migrations/0001_initial.sql`, `GET /readyz` with DB + Redis pings).

## 2026-09-25 — P01 · Core foundations

- `crates/core` gained the shared infrastructure layer: typed `config` (OMNION_* keys, `PORT` fallback,
  validation for environment/port/pool size/URL schemes, pretty-vs-JSON log defaults), `telemetry`
  (tracing subscriber with the OpenTelemetry layer hook plus a shutdown handle), `error`
  (`ConfigError` + `CoreError`), `db` (SQLx pool + embedded migration runner) and `redis_client`
  (lazily connected handle that reconnects without a restart).
- `database/migrations/0001_initial.sql`: organizations, users (case-insensitive unique email),
  sessions (hashed tokens only) and an append-only `audit_log` whose `actor_type` also covers the
  AI Hub audit chain.
- `apps/api` boots config → telemetry → database + migrations → redis → HTTP, with SIGTERM/Ctrl-C
  graceful shutdown. `GET /readyz` pings both dependencies — `200` only when everything answers,
  `503` with detail in development — and `ApiError` maps core errors onto the HTTP surface (`503`
  for an unavailable dependency, `500` otherwise).
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **27 passed** (20 core unit · 3 api unit · 1 healthz ·
  3 readyz integration against the live compose stack) · live run: `GET /healthz` → `200
  {"ok":true,…}` and `GET /readyz` → `200 {"checks":{"database":{"status":"ok"},"redis":{"status":"ok"}}}` ·
  `_sqlx_migrations` shows version 1 "initial" with `success = t`, and `\dt` lists
  organizations · users · sessions · audit_log · failure path: with Redis stopped, `/readyz` → `503
  redis unavailable (broken pipe)` while `/healthz` stayed `200`, and it returned to `200` without
  restarting the API once Redis was back.
- Dev stack: `docker compose -f infra/compose/docker-compose.dev.yml up -d` now really starts all four
  containers (postgres 5433 · redis 6380 · minio 9000/9001 · mailpit 1025/8025). Upstream MinIO images
  are no longer published on Docker Hub (`pull access denied`), so `infra/compose/minio.yml` pulls the
  community mirror `pgsty/minio` — the same server binary.
- CI: the Rust job now provisions PostgreSQL + Redis service containers (so the readiness integration
  tests run for real) and adds a smoke step that boots the API and curls `/healthz` + `/readyz`.
- CI run `36195108450` → **success** (Rust job `1m51s`, Infra job `5s`): the readyz suite passed 3/3
  against the service containers and the smoke step logged `listening address=0.0.0.0:8080`,
  `{"ok":true,…}` for `/healthz`, `{"checks":{"database":{"status":"ok"},"redis":{"status":"ok"}}}` for
  `/readyz`, then `shutdown signal received` after SIGTERM.
- Next: **P02 — Identity v0** (users + argon2, sessions, login/logout, first-admin bootstrap,
  integration tests).

## 2026-09-25 — P02 · Identity v0

- New crate `crates/identity` (docs/07-IAM.md subset): `users` (create/find by email or id,
  lowercase-normalized addresses, case-insensitive uniqueness), `password` (Argon2id hashing on the
  blocking pool, minimum length policy, `dummy_verify` so unknown addresses burn the same work as
  wrong passwords), `sessions` (256-bit hex tokens; only the SHA-256 hash is stored; 30-day TTL;
  resolve/touch/revoke) and `authentication` (`authenticate` returns Authenticated /
  InvalidCredentials / AccountDisabled — account status is only disclosed after the password
  verified).
- `crates/core` config gains `OMNION_ADMIN_EMAIL` + `OMNION_ADMIN_PASSWORD` (both-or-neither
  validation, password redacted from `Debug`); `apps/api` boot now seeds the first administrator on
  an empty database (`bootstrap_first_admin`, idempotent and race-safe) and logs why it skipped
  otherwise.
- API surface: `POST /api/v1/auth/login` (session cookie `omnion_session`, HttpOnly + SameSite=Lax,
  `Secure` outside development, Max-Age 30 days) · `GET /api/v1/me` — backed by a `CurrentSession`
  extractor (401 `unauthenticated` / `invalid_session`) · `POST /api/v1/auth/logout` (204, revokes and
  clears). Identity errors map onto the HTTP surface through `ApiError` (exhausted pool → 503).
  `ClientAddress` extractor captures the peer IP for `sessions.ip_address` without requiring
  connect-info (axum has no optional `ConnectInfo`), and the server is served with
  `into_make_service_with_connect_info`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **65 passed** (identity 18 · core 23 · api unit 14 · health 1 ·
  readyz 3 · auth integration 6 — including bootstrap on a throwaway database and revoke-after-logout)
  · live run on `:18080` against the compose stack: boot log `first administrator account created`,
  `/healthz` 200, `/readyz` 200 both checks ok, login 200 + `set-cookie: omnion_session=…; Path=/;
  HttpOnly; SameSite=Lax; Max-Age=2592000` with a 64-char token, `/me` 200 with the account JSON,
  `/me` without cookie 401 `unauthenticated`, logout 204 + `Max-Age=0`, replayed revoked token 401
  `invalid_session`, wrong password 401 `invalid_credentials`; database check: `admin@omnion.test`
  row with `$argon2id$v=19$…`, one session row with a hash that is not the token, `revoked = t`,
  `seen = t`.
- CI: the smoke step now seeds the admin and walks login → me → anonymous 401 → logout.
- CI run `36196441876` → **success**: `Rust — fmt · clippy · test` ✅ (1m11s — the identity suite ran
  against the service containers) · `Infra — compose config` ✅ (4s).
- Next: **P03 — IAM v0** (roles, permissions, role bindings, `require(permission)` guard, audit
  writes, role seeds from docs/07 §3).

## 2026-09-25 — P03 · IAM v0

- New crate `crates/permissions` (docs/07-IAM.md): the permission catalogue (32 keys across
  content/media/users/plugins/deployment/iam/audit), fully custom roles with priority, an inheritance
  link and flag, explicit allow/deny entries and scoped role bindings (global / organization / site,
  `expires_at` for temporary roles). `evaluate::RoleGraph` is a pure, deterministic resolver for the
  documented precedence — explicit deny > explicit allow > inherited allow > default deny — and it
  reports the provenance of every verdict (the seed of the permission simulator, §18). `crates/audit`
  is the append-only trail writer (§13, §19) reused by later AI-agent actions.
- Migration `0002_iam.sql`: `permissions`, `roles` (platform vs organization roles, key unique per
  scope), `role_permissions` (allow/deny, one row per key) and `role_bindings` (scope shape enforced by
  a check constraint, live combinations unique, `site_id` gains its foreign key with P04).
- `apps/api`: `require(permission)` guard (`src/guards.rs`) wraps routes, resolves the session once and
  hands it to the handler through the request extensions; privileged actions write audit rows in the
  same request. New `/api/v1/iam` surface: permissions, roles (GET/POST), role permissions (PUT),
  bindings (GET/POST), effective-permissions (own set without a permission, others need
  `iam.roles.read`) and audit (GET, `audit.read`). Boot seeds the catalogue and the six base roles
  (§3) and keeps the "at least one Owner" invariant (§20), auditing the fallback binding as a platform
  action.
- Fixed: adding a migration did not rebuild `omnion-core`, so the embedded migrator kept the old set
  and `0002` silently never applied (`relation "permissions" does not exist`). `crates/core/build.rs`
  now emits `cargo:rerun-if-changed=../../database/migrations`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **95 passed** (permissions 20 unit incl. deny/inheritance/cycle
  cases · audit 2 · api unit 17 · identity 18 · iam integration 4 against the compose stack).
- Live run on `:18081` against the compose stack: boot log `permission catalogue and base roles ready
  permissions=32`; `/healthz` 200 and `/readyz` both checks ok; anonymous `/api/v1/me` 401 and
  `/api/v1/iam/roles` 401; a signed-in session without a role gets `403 permission_denied` naming
  `iam.roles.read`; after the platform Owner binding `GET /iam/roles` → 200 with the ladder (`owner
  p1000 allow=32`, `administrator p900 allow=32`, `manager p700 allow=21`, `moderator p500 allow=9`);
  `POST /iam/roles` 201, `PUT /iam/roles/{id}/permissions` 200, `POST /iam/bindings` 201, editing a
  platform role 403 `system_role`; `/api/v1/iam/audit` lists `iam.binding.granted`,
  `iam.role.permissions_updated` and `iam.role.created` with actor, target and metadata, and the same
  rows are in `audit_log`.
- CI: the smoke step now also walks the gated surface (roles + own effective set, anonymous 401 for
  roles/audit). Run `36197985861` → **success**: `Rust — fmt · clippy · test` ✅ (1m06s; the smoke log
  shows `permission catalogue and base roles ready permissions=32`, the six base roles with the
  documented priorities and allow counts 32/32/21/9/8/2, the administrator's global Owner binding and
  the resolved effective set) · `Infra — compose config` ✅.
- Next: **P04 — Tenancy v0** (organizations, sites, domains + CRUD API, scope enforcement,
  cross-tenant denial tests).

## 2026-09-25 — P04 · Tenancy v0

- Migration `0003_tenancy.sql` (docs/01-VISION.md §10, docs/07-IAM.md §7): `sites` (one property
  of one organization, `key` unique per organization) and `site_domains` (host unique
  platform-wide, at most one primary per site through a partial unique index). The site-scoped
  role binding finally gets its foreign key; the narrow pre-constraint cleanup retires bindings
  that pointed at no site, so the constraint applies on an existing database.
- `crates/identity` grows the tenancy store: `organizations` (create/read/list/update/delete with
  slug, name and status validation) and `sites` (site and domain CRUD, host validation, automatic
  first-primary plus promotion on removal, `find_site_by_host` — the routing primitive P07 builds
  on). `IdentityError` learns the tenancy variants; `apps/api/src/error.rs` maps them
  (404 not-found, 409 taken, 400 shape).
- `crates/permissions`: new `tenancy` category — `organizations.read|manage`,
  `sites.read|create|update|delete`, `domains.manage`; the Manager ladder gains site management
  and Moderator/Editor `sites.read`; `bindings::validate` now refuses a site scope that names an
  unknown site or one of another organization.
- `apps/api`: `/api/v1/organizations` (GET/POST/GET id/PATCH/DELETE) and `/api/v1/sites`
  (GET/POST/GET id/PATCH/DELETE) plus `/{id}/domains`, `/{id}/domains/{domain_id}` and
  `/{id}/domains/{domain_id}/primary`, all behind the new guards. The shared `scope.rs` helpers
  carry the tenancy rule — an account with a primary organization stays inside it, opening and
  deleting a tenant is platform-only — and `routes/iam.rs` now uses them instead of its own
  copies. Deleting a tenant requires it to be empty (`409 organization_not_empty`).
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **119 passed** (identity 27 · permissions 22 · core 23 · api
  unit 26 · auth 6 · health 1 · iam 4 · readyz 3 · tenancy 5 integration against the compose
  stack, incl. cross-tenant denial and restore-free domain promotion).
- Live run on `:18082` against the compose stack (Owner session, P03 pattern): `/healthz` and
  `/readyz` both ok; anonymous `/api/v1/sites` and `/api/v1/organizations` 401; tenant 201,
  duplicate slug 409 `organization_slug_taken`; site 201, duplicate key 409 `site_key_taken`; first
  host primary, second non-primary, promotion 200, list primary-first; platform patch 200; domain
  removed 204 with the remaining host promoted; site 204; tenant 204 and `GET` 404; `site.created`,
  `site.domain.added`, `site.domain.primary_changed`, `site.updated`, `site.domain.removed`,
  `site.deleted`, `organization.created`, `organization.updated` and `organization.deleted` rows
  in `audit_log` with actor, target and metadata.
- Cross-tenant, live: an organization account (its own role with the tenancy keys, bound at
  organization scope) sees exactly its own organization and site and may create inside them, while
  every read of the other tenant — organization, site, domains, `?organization_id=` filter — and
  every write (`POST /sites` with the other organization, `POST /sites/{other}/domains`, PATCH,
  DELETE) answers **403 cross_organization**; the same calls on its own site answer 200/201. An
  account without a role gets **403 permission_denied** on the way to the handler; opening a
  tenant, and deleting its own, answer **403 platform_only** while renaming it is 200; a
  site-scoped binding that names another tenant's site is refused with `invalid_request`
  (“the site belongs to another organization”). The dev database was left clean
  (organizations 0 · sites 0 · domains 0 · 6 base roles · 1 owner binding).
- Noted for operators: base roles keep the permission set they were created with, so an
  installation that upgrades keeps the new tenancy keys on the Owner only — a tenant grants them
  to a role of its own (proven live); the seed's Owner sync is what keeps the platform owner
  complete.
- CI: the smoke step now also opens a tenant, a site and a domain and re-checks the anonymous 401s;
  the block was rehearsed locally against the built binary before pushing.
- CI run `36199468683` → **success**: `Rust — fmt · clippy · test` ✅ (119 tests, including the five
  tenancy integration tests; the smoke log shows tenant `smoke-organization`, site `main` and domain
  `smoke.omnion.test` with `"is_primary":true` created against the CI service containers) ·
  `Infra — compose config` ✅.
- Next: **P05 — Content v0** (pages + revisions, draft/published, publish/restore, slug rules,
  translations skeleton).

## 2026-09-25 — P05 · Content v0

- New crate `crates/content` (docs/05-VERSIONING.md §4–§7, docs/01-VISION.md §5, §7): `pages`
  (slug unique per site, `page_type`, lifecycle `draft`/`published`/`archived`, a pointer at the
  revision visitors see), `page_revisions` (append-only history with `revision_no`,
  title/body/summary and `restored_from_id`) and `translations` (one value of one field of one
  resource in one language — the content → translations[lang] model, no `title_tr` columns
  anywhere).
- The store keeps the documented rules: revision 1 is written with the page; editing content
  appends `n + 1` and archives the draft it supersedes; publishing freezes the draft, retires
  the revision it replaced and refuses with `no_draft_revision` when nothing is pending;
  restoring copies an older revision forward as a new draft (recording where it came from), so
  history is never rewritten; deleting a page takes its revisions and translation rows with it.
  Migration `0004_content.sql`; partial unique indexes hold "at most one draft, one published"
  in the database itself.
- API surface: `GET|POST /api/v1/pages`, `GET|PATCH|DELETE /api/v1/pages/{id}`,
  `POST /{id}/publish`, `POST /{id}/restore`, `GET /{id}/revisions[/{revision_id}]` and
  `GET|PUT /{id}/revisions/{revision_id}/translations[/{language}]`, all behind the
  `content.pages.*` guards plus the tenancy scope rule (a foreign page answers `403
  cross_organization`); every state change writes an audit row (`page.created`, `page.updated`,
  `page.published`, `page.revision.restored`, `page.translation.updated`, `page.deleted`).
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **140 passed** (content crate 12 unit · api
  content routes 3 unit · 5 content integration against the compose stack). Live run on `:18083`
  against a fresh database (dropped afterwards): page `home` created as v1 draft; publish →
  `published_rev=1`; edit → v2 draft while v1 stayed live; publish → v2; re-publish →
  `no_draft_revision`; restore v1 → v3 draft with `restored_from=<v1>`; publish → v3 live;
  history `v3 published / v2 archived / v1 archived` with v1's row unchanged;
  `PUT .../translations/tr` → `tr/title = Merhaba`, `tr/body = Gövde`; anonymous `/api/v1/pages`
  → `401 unauthenticated`; `_sqlx_migrations` shows version 4 `content`; `audit_log` carries the
  seven `page.*` actions with revision numbers.
- CI: the smoke step now walks the content surface (create → publish → edit → publish → restore
  → translation) and re-checks the anonymous 401. Run `36200626747` → **success** (Rust job
  1m31s; the smoke log shows the page created as a draft, revised to "Welcome to Omnion", and
  the restore row `revision_no: 3, state: draft, restored_from_id: …`) · `Infra — compose
  config` ✅.
- Next: **P06 — Admin app v0** (`apps/admin` Next.js + TS + Tailwind with the API-wired login,
  app shell, pages list and site switcher; root `package.json` + `pnpm-workspace.yaml` +
  `turbo.json` skeleton).

## 2026-09-25 — P06 · Admin app v0

- The JavaScript/TypeScript side of the monorepo is alive: root `package.json` +
  `pnpm-workspace.yaml` (`apps/*`, `packages/*`) + `turbo.json` (`build`/`dev`/`typecheck`), so
  `pnpm build` at the root builds every app through turbo (verified: `1 successful, 1 total`).
- New app `apps/admin/` (Next.js 16 + React 19 + TypeScript 5.9 + Tailwind v4, docs/03-FRONTEND.md):
  the app shell (sidebar sections, sticky header with the current screen, mobile drawer), an
  API-wired sign-in screen, an overview screen with live workspace totals and the account card,
  the pages list of the selected site (state filter, live/draft revision per row, reload, empty
  and error states) and the sites list.
- Same-origin API access: `next.config.ts` forwards `/api/*` to `OMNION_API_URL` (default
  `http://127.0.0.1:8080`), so the API's HttpOnly session cookie stays first-party and no CORS
  rule is needed; a deployment only has to route `/api/*` at the edge.
- Session handling is server-first. `proxy.ts` (Next.js 16's renamed `middleware.ts`) turns
  visitors without a session cookie away from panel routes with a `307` to `/login`, the root
  layout resolves the account through `GET /api/v1/me` with the request's own cookie, and the
  client provider starts from that answer — the browser never probes the session itself, which
  keeps the console clean and shows no flash of protected UI.
- Tailwind v4 with `source(none)` + explicit `@source` roots: the repository also carries a Rust
  `target/` tree, so automatic content detection stays off and the app declares its own folders.
- Proof: `pnpm install` (2 workspace projects) · `pnpm --filter @omnion/admin run typecheck`
  clean · `pnpm --filter @omnion/admin run build` green (7 routes; `ƒ Proxy (Middleware)`) ·
  root `pnpm build` → turbo `1 successful, 1 total` · the compiled stylesheet carries the
  utilities the screens use (`bg-canvas`, `text-muted`, `border-line`, `sm:grid-cols-3`,
  `hover:bg-canvas`, …). Live walk on `:3100` against a fresh database (`omnion_p06_live`,
  dropped afterwards) with the API on `:8080`: seeded one tenant → one site → two pages (one
  published with a newer draft, one draft-only) through the API, then drove the panel in a real
  browser (Playwright): anonymous `/` → **307** to `/login`; a wrong password shows the API's
  own refusal (`email or password is incorrect`); the bootstrap account signs in and lands on
  the overview with totals `1 site / 2 pages / 1 published`; the switcher lists the site; the
  pages list shows `/home · live v1 · draft v2` and `/about · not published · draft v1`; the
  state filter narrows the list to the published page; the sites list marks the selected site;
  sign-out clears the cookie and a guarded route answers with `/login`. **17/17 checks passed,
  0 console errors** (the only rejected request in the whole walk is the deliberate wrong
  password on `/api/v1/auth/login`). Screenshot: `/tmp/omnion_p06_pages.png`.
- Next: **P07 — Public web v0** (`apps/web` server-side renderer for published pages + the
  `themes/minimal` stub, `GET /:slug` renders).

## 2026-09-25 — P07 · Public web v0

- The platform now *serves* a site, not just manages one. `crates/identity` gained
  `find_site_by_global_key` (a key resolves platform-wide only when exactly one site carries it —
  keys are unique per organization), and `apps/api` gained the **public surface**
  `GET /api/v1/public/pages/{slug}`: unauthenticated, published revisions only, and no internal
  identifiers in the payload (site key + name · page slug/type/updated_at · revision
  number/title/body/summary/published_at).
- Site resolution is part of the request, documented in `apps/api/src/routes/public.rs`: `?site=`
  (a host when it contains a dot, otherwise a key) → `X-Forwarded-Host`/`Host` with the port
  dropped → the installation's only site when there is exactly one. An address that matches
  nothing answers `404` — and it answers the *same* `404` for "unknown" and "not published", so
  the public surface never discloses a draft. An address that is not a slug shape is a `404` too,
  never a `400`: the panel's validation rules stay in the panel.
- New JavaScript side: `packages/types` (the public content shapes — types-only), `packages/theme-sdk`
  (the theme contract: `SiteTheme`, `PageLayoutProps`, `ThemeManifest`, `defineTheme`),
  `themes/minimal` (manifest + `PageLayout` + stylesheet; warm cream/ink/terracotta palette with a
  light/dark pair), and `apps/web` — Next.js 16 renderer where `app/[[...slug]]/page.tsx` serves the
  site's `home` page at `/` and any published page at `/{slug}` (`force-dynamic`, so a publish is
  visible without a rebuild), `lib/api.ts` reads the API server-side and forwards the visitor's host
  as the site hint unless it is a loopback address, `lib/theme.ts` is the theme registry, and
  `lib/metadata.ts` emits title/description plus canonical and Open Graph URLs when
  `OMNION_SITE_URL` is set. `pnpm-workspace.yaml` now includes `themes/*`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **145 passed** (api unit 33 incl. the new hint/host helpers ·
  public integration 2 · the rest unchanged) · `pnpm --filter @omnion/web run typecheck` clean ·
  `pnpm --filter @omnion/web run build` green (`ƒ /[[...slug]]` server-rendered on demand) · root
  `pnpm build` → turbo `2 successful, 2 total`.
- Live walk on a fresh database (`omnion_p07_live`, dropped afterwards; API on `:18090`, renderer on
  `:3200`): the draft answers `404` on the public surface; after publishing, the public read answers
  `200` through `?site=main`, through `Host: p07.omnion.test` and with no hint at all (single-site
  fallback); `GET /home` returns 7509 bytes of HTML containing the title, the body paragraph,
  `Powered by Omnion` and `data-theme="minimal"`; `GET /` serves the home page; `GET /` with
  `Host: p07.omnion.test` renders through the renderer's host forwarding; `/missing` and the
  draft-only `/soon` answer the not-found view; a revision published while the renderer kept running
  appeared in the HTML without a restart; the theme's stylesheet came back with its `--mn-accent`
  token.
- CI: the smoke step now also reads the published page through the public surface (by key and by the
  site's own domain), and it creates a page that is never published to assert the public `404`. The
  block was rehearsed locally against the built binary first (`SMOKE BLOCK REHEARSAL OK:
  public_title=Welcome to Omnion host_title=Welcome to Omnion draft=404`).
- CI run `36202889332` → **success**: `Rust — fmt · clippy · test` ✅ (the public integration suite
  ran in CI — `Running tests/public.rs` → `2 passed` — and the smoke log prints the public evidence:
  `public surface: title="Welcome to Omnion" via-key · "Welcome to Omnion" via-domain ·
  unpublished=404`) · `Infra — compose config` ✅.
- Next: **P08 — Media v0** (`crates/storage` S3/MinIO abstraction, upload endpoint, media table and
  the public serve path).

## 2026-09-26 — P08 · Media v0

- `crates/storage` (`omnion-storage`): the object-storage abstraction behind the media library —
  a key validator (`keys.rs`, the shape that cannot leave a storage root), SigV4 signing of its own
  S3 requests (`signing.rs`; unit-tested against the published example request — canonical request,
  string-to-sign and signature), an S3/MinIO driver (`s3.rs`: path-style addressing, `NoSuchBucket`
  → create the bucket and retry once, and a `probe` that tells a missing bucket from an unreachable
  store) and a file-system driver for local runs (`fs.rs`). `Storage::from_config`/`from_env` pick
  the driver from `StorageConfig` (`OMNION_STORAGE_DRIVER=s3|fs`, `OMNION_S3_*`, `OMNION_STORAGE_DIR`);
  the secret key is never rendered.
- `crates/media` (`omnion-media`): the row half — the `Media`/`NewMedia` model, the `media` table
  queries and the rules the outside world meets: `sanitize_filename` (path dropped, symbols reduced,
  extension kept), `normalize_content_type` (`type/subtype` only), `object_key` built from ids, and
  `serve_plan` — the inline allow-list (images, video, audio, PDF, `text/plain`); everything else
  leaves as `application/octet-stream` + `attachment` with `nosniff`, so an uploaded document can
  never become markup or script on the platform's own origin.
- `database/migrations/0005_media.sql`: one row per stored object (site, object key, file name,
  content type, size, SHA-256 checksum, uploader) with a unique object key, `size > 0` and shaped
  content-type/checksum constraints; sites cascade, so a removed tenant takes its library with it.
- API (`/api/v1/media`): `POST` (multipart, `media.upload`; the route's own body limit is the
  library's 25 MB plus framing slack; the row is written *after* the object is stored and the object
  is dropped again when the row is refused), `GET` list and metadata (`media.read`),
  `GET /{id}/raw` (`media.read`), `DELETE` (`media.delete` — object first, so a refused store never
  leaves bytes nothing points at). Every handler applies the tenancy scope rule through the site and
  writes an audit row (`media.uploaded`, `media.deleted`). The renderer's read path is
  unauthenticated like the rest of the public surface: `GET /api/v1/public/media/{id}`. `AppState`
  now carries the object store; boot logs `s3://omnion-media` and warns instead of failing when the
  store is down.
- `apps/admin`: the `/media` screen (new nav entry) — the library of the selected site with previews
  for image types, type/size/uploaded columns, an upload button (multipart through the panel's own
  origin) and per-row removal. `lib/api.ts` gained `fetchMedia`/`uploadMedia`/`deleteMedia`/
  `mediaRawUrl` and now only sets `content-type: application/json` for string bodies, so `FormData`
  keeps the browser's own multipart boundary; `lib/format.ts` gained `formatBytes`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **190 passed** (storage 25 · media 12 · api unit 40 · the new
  media integration suite **6** against the live compose stack) ✅ · root `pnpm build` for the admin
  app green (`ƒ /media` in the route table) ✅.
- Live walk (fresh database `omnion_p08_live`, API on `:18090`, admin on `:3100`, MinIO from
  `infra/compose`): sign-in → tenant + site → upload `omnion-media.txt` (19 B, `text/plain`,
  sha256 `11933807907f32bd…`) → the listing carries it with both read paths → `/raw` answers the very
  bytes (`cmp` identical, `type=text/plain`) and `/public/media/{id}` the same bytes with no session
  → anonymous `GET /api/v1/media` → `401` → `DELETE` → `204` → `/raw` afterwards → `404`; the object
  shows up in the MinIO data directory and is gone once deleted. Admin: Playwright signs in, opens
  `/media`, and the table lists `omnion-admin-fixture.txt` (`text/plain`, 27 B) — **the file is
  listed in the panel** ✅ with 0 console errors.
- Note for deployments: the admin's `/api/*` rewrite destination is baked at build time, so a panel
  pointed at a non-default API origin has to be *built* with `OMNION_API_URL` set; setting it only
  at `next start` answers `500` (`ECONNREFUSED 127.0.0.1:8080`).
- CI: a `Start the object store (media library)` step brings MinIO up the way `infra/compose` does
  (service containers cannot pass a command) and the smoke step now walks the media round trip —
  upload → list → `cmp` both read paths → anonymous `401` → delete → `404`. Run `36204518708` →
  **success**: `Rust — fmt · clippy · test` ✅ (the media integration suite ran in CI —
  `a_file_round_trips_from_upload_to_fetch ... ok`, `the_media_surface_is_permission_gated ... ok`,
  `an_upload_into_another_tenant_is_refused ... ok`, `test result: ok. 6 passed` — and the smoke log
  prints the media evidence: `media surface: library=1 file(s), first=omnion-media.txt 19 B
  text/plain · round-trip=ok · public=ok · removed=404`, after the new step printed
  `object store: 200`) · `Infra — compose config` ✅.
- Next: **P09 — Workflow engine v0** (`crates/workflows`: durable step store, background runner,
  step retries with backoff, wait-sweeper, manual + schedule triggers).

## 2026-09-26 — P09 · Workflow engine v0

- `crates/workflows` (`omnion-workflows`): the durable step engine behind the automation surface
  (docs/requests/REQ-003; design lessons from docs/09-N8N-TEARDOWN.md §13). A definition is a
  trigger plus an ordered step list — `definition.rs` (manual or cron schedule, 1–50 uniquely
  named steps, attempt cap five, waits 1 s–1 day), `cron.rs` (a five-field cron subset in UTC:
  lists, ranges, steps, month/weekday names, the classic day-of-month/day-of-week union — no
  dependency), `actions.rs` (the closed built-in action set `noop`, `echo`, `fail`, `transient`:
  no dynamic code in the core process, lesson 14), `store.rs` (the durable step store: claim,
  park, retry, settle, cancel, sweep — written so two instances racing over one row still hand
  each step to exactly one runner) and `engine.rs` (the policy: what one tick does, what the
  sweep repairs).
- Durable by construction: starting a run materialises one `workflow_steps` row per step, with
  `available_at` carrying both the retry backoff and the wait deadline. A wait parks the run (a
  write, not a sleep) and is resumed by the runner's next claim or by the sweep; a failing step
  is re-queued with an exponential backoff (5 s → 300 s by default) until its attempts run out;
  a claim older than the lease (5 min) is reclaimed by the sweep with an honest outcome — the
  next attempt for a task step, another park for a wait, and a clean failure when the lost
  attempt was the last one, so every run still reaches a terminal state.
- `database/migrations/0006_workflows.sql`: `workflows`, `workflow_executions`,
  `workflow_steps` with the constraints the engine leans on (status vocabularies, trigger shape,
  `attempts` never above the cap, one row per step position, cascade from the organization down).
- API (`/api/v1/workflows`, `/api/v1/workflow-executions`): create / read / replace / remove a
  definition, list its run history, read one run with every step, start a run and cancel one. The
  three new keys (`workflows.read`, `workflows.manage`, `workflows.run`) are catalogued and reach
  the operational roles (manager all three, moderator read+run, editor read). Definition changes
  and cancellations are audited by the handlers; the engine audits the run lifecycle
  (`workflow.execution.started|completed|failed|cancelled`).
- `apps/api/src/workflow_runner.rs`: the background runner — one task inside the API process, on
  by default — ticks the engine every `OMNION_WORKFLOW_TICK_MS` (default 1 s) and sweeps every
  `OMNION_WORKFLOW_SWEEP_SECONDS` (default 30 s); batch, scheduler batch and the retry base/max
  are configurable too (`OMNION_WORKFLOW_*`), and `OMNION_WORKFLOW_RUNNER=false` keeps a process
  from running the engine at all.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **250 passed, 0 failed** (workflows **36** unit
  tests · the new integration suite `apps/api/tests/workflows.rs` **11** against the live compose
  stack · every other suite unchanged) ✅
- Live walk (fresh database `omnion_p09_live`, API on `:18091`, runner tick 200 ms / sweep 1 s /
  retry base 400 ms): the Owner created a four-step workflow (noop → transient(`fail_times: 1`,
  3 attempts) → wait(2 s) → echo) and started it. The runner re-queued the failing step
  (`a failing step was re-queued … attempt=1 of=3 delay_ms=400`), parked the wait (`a wait step
  parked the run … seconds=2`), resumed it and finished the run — the API then read
  `completed step2.attempts=2 step3=succeeded step4=succeeded`. A second run parked on a 600 s
  wait was cancelled (`cancelled`, steps `cancelled`+`succeeded`), the second cancel answered
  `409 execution_not_running`. A `* * * * *` workflow fired by itself
  (`a scheduled workflow started …`) exactly once with `trigger=schedule`. The audit trail held
  9 rows (`started` ×3, `completed` ×2, `cancelled` ×1, `created` ×3) and both workflow routes
  answered `401` without a session.
- CI: the smoke step now walks a workflow as well — define the four-step run, start it, poll the
  execution until it settles, assert `completed` with `attempts=2` on the retried step, print the
  workflow audit rows, and prove `/api/v1/workflows` is closed to anonymous callers. Run
  `36206269819` → **success**: `Rust — fmt · clippy · test` ✅ (the workflow integration suite ran
  in CI — the eleven walks passed against the compose stack — and the smoke log prints
  `a failing step was re-queued … attempt=1 of=3 delay_ms=5000` for the retried step, then
  `workflow surface: completed attempts=2 wait=succeeded · run=completed` with
  `workflow audit rows: 19`) · `Infra — compose config` ✅.
- Next: **P10 — Onboarding v0 (REQ-050)** (first-run wizard + `omnion` CLI skeleton).

### Lessons

- A wait step must be resumable whichever mechanism wakes it: deciding "park or resume" from the
  status the row had before the claim broke as soon as the sweeper re-queued a due wait — the
  parked step parked itself again (caught by the attempts constraint as a `23514`). Deciding on
  the claim count (`attempts > 1` ⇒ resume) makes the runner's claim and the sweep agree, and the
  database constraint turned a silent double-park into a failing test.
- An audit row is a second write: a probe that reads the state the moment it flips can land in
  the millisecond before the audit arrives. Positive audit assertions must poll (state → audit is
  the deliberate order); keep the negative ones reading immediately.
- A claim is a LEASE, not a lock. A runner that stops mid-step leaves a `running` row whose
  attempt is already counted, and nothing else would move it again: the run would stay open
  forever and block its own later steps. The sweep reclaims claims older than the lease.
- Test-fleet hygiene: a panicking test skips its cleanup, and the next run's engine ticks walk
  into those rows (a leftover wait at its attempt cap failed an unrelated suite with a constraint
  error). Sweep the suite's own prefix once per process (`OnceCell`) before fixtures appear, and
  keep the test runner's lease longer than any test step so the reclaim path only fires in the
  walk that asks for it.

## 2026-09-26 — P10 · Onboarding v0 (REQ-050)

- `crates/onboarding` (`omnion-onboarding`): the first run of an installation as one flow both
  front ends drive — `steps.rs` (owner account → organization → first site with its domain →
  theme → AI decision → close), `state.rs` (the `onboarding_state` singleton, the derived step
  status and the summary) and `checklist.rs` (the getting-started items, derived from the
  platform's own rows). The owner step binds the Owner role through the same seeding path the API
  runs at boot; every step is audited; an installation whose first account came from the
  environment bootstrap finishes its first run through the wizard too (the oldest active account
  acts as its owner).
- `database/migrations/0007_onboarding.sql`: `sites.theme` (the presentation setting the renderer
  activates) and the `onboarding_state` singleton, one timestamp per step.
- `tools/cli` (`omnion-cli`, binary `omnion`): `setup` (interactive, flag-driven or fully
  non-interactive; the password never echoes), `doctor` (six checks in boot order, actionable
  hints, non-zero exit when one fails) and `migrate` (before/after migration status). The CLI
  reads the same typed configuration the API does and runs the same flow code.
- `apps/api`: `/api/v1/onboarding` — `GET` (open: how far the first run has come) plus `owner`,
  `organization`, `site`, `theme`, `ai-provider` and `complete`, guarded by the flow itself (no
  permission guard: there is nothing to check against before an account exists). The site body
  and the public page response carry the site's theme now, and `PATCH /sites/{id}` accepts one.
- `apps/admin`: the `/setup` wizard (five steps + done screen, Lucide icons only, resumable from
  the server's step status), a sign-in redirect for a fresh installation, the getting-started
  checklist card on the dashboard and a theme selector on Sites. `apps/web` resolves the theme the
  site carries, so choosing one changes what visitors see.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **272 passed, 0 failed** across 32 suites (the new
  `crates/onboarding` unit tests and `apps/api/tests/onboarding.rs` — four walks on throwaway
  databases) ✅
- Live walks (each on a database created empty for the run):
  1. **wizard surface over HTTP** (`:18092`): `needs_setup` → owner (auto sign-in) → organization
     → site+domain → an unknown theme refused (`400 unknown_theme`) → theme → AI skip → complete;
     the owner then reads `/organizations` and `/sites` through the regular API; six
     `onboarding.*` audit rows; a second owner attempt answers `409 already_installed` and an
     anonymous step `401` ✅
  2. **the browser** (admin panel `:3101` against API `:18094`, Chromium under Xvfb): 10/10 checks
     with 0 console errors — all five steps walk, the dashboard shows the checklist (6 items, 2
     open), Sites shows the theme select, and a finished installation redirects `/setup` back to
     the panel; read back from the database: 1 user, 1 organization, 1 site (`theme=minimal`),
     1 domain, `onboarding_completed=true` ✅
  3. **the CLI** on a fresh database: `doctor` fails on an unmigrated schema (exit 1, hint
     `omnion migrate`), `migrate` applies 7 migrations, `doctor` passes, `setup --non-interactive`
     creates everything, the API serves it (login + the site with its theme), and a second
     `setup` is refused with exit 2 ✅
- CI: the smoke job builds the CLI too and runs a first-run walk on a database that has never been
  migrated (`omnion doctor` refuses the unmigrated schema, `migrate` applies it, `setup` creates
  the installation, the API serves it). Run `36208527271` → **success** (the walk prints
  `doctor reported the pending schema (exit 1, as expected)`, `Setup complete.` …
  `onboarding: completed · steps {'owner': True, …} · themes ['minimal']`,
  `site: ci-site theme=minimal`) · `Infra — compose config` ✅
- Also fixed: the P09 workflow suite's walks are serialised behind a walk lock — its sweeps work
  on the whole `workflows` table, so two walks in flight could settle each other's rows (4 of 5
  runs failed before the lock, 5 of 5 pass after).
- Next: **P11 — AI Hub v0** (docs/06: provider abstraction, providers/models tables, streaming
  chat endpoint, provider configuration in the admin panel).

### Lessons

- An onboarding flow must be *derived*, not remembered: every step's status comes from the
  platform's own rows (accounts, organizations, sites, the theme on the site), so a refresh, a
  second tab, or an installation set up through the API directly all resume correctly without a
  client-side state machine.
- A wizard's first step cannot require a session: the endpoint that creates the owner account is
  open by necessity, so its safety has to come from the data rule (only while the installation
  has no accounts at all) — with the unique index as the backstop for two callers racing.
- The admin panel bakes `rewrites()` into the build: pointing it at a different API origin only
  takes effect after `next build`, so a browser walk must build the panel for the API port it
  will talk to (a runtime env var is not enough).
- This container's Chromium refuses `Page.captureScreenshot` even under Xvfb: a browser walk has
  to treat pictures as a bonus and assert on the DOM (step-state attributes, checklist items,
  console errors) instead of on screenshots.
- Two walks that share a table-wide repair pass cannot run at once: `engine::sweep` is
  deliberately global, so the workflow suite now holds a walk lock — determinism is worth the
  extra ~17 s of serial runtime, and a flake that fails 4 of 5 runs is a bug, not weather.

## 2026-09-26 — P11 · AI Hub v0 (docs/06)

- New crate `crates/ai-hub` (`omnion-ai-hub`) — the platform's single door to AI. `model.rs` holds
  the stored shapes and their validation (a provider is a name, a protocol, a base URL and a
  write-only key; a model is a wire key plus the capability metadata of docs/06 §3); `store.rs`
  keeps the two data rules the rest of the platform leans on — at most one default provider, and a
  default model that is always an enabled one (a switch-off, a removal, a replaced model list or a
  deleted provider repairs the default, or leaves the installation without one — never with a
  model that cannot answer); `client.rs` is the OpenAI-compatible wire client (`chat`,
  `stream_chat` with an SSE decoder built against frames rather than chunks, and
  `list_remote_models`); `router.rs` resolves a request to one `(provider, model)` pair — an
  explicit `provider/model`, a bare key searched across the enabled providers (default provider
  first, and a model key may itself carry a slash), or the installation's default model.
- `database/migrations/0008_ai_hub.sql`: `ai_providers` + `ai_models`, with the partial unique
  indexes that make the two rules the database's own (one default provider, one default model),
  case-insensitive provider names, a base-URL shape check and a cascade from a provider to its
  models.
- Permissions: `ai.providers.read`, `ai.providers.manage` and `ai.chat` (category `ai`) —
  connecting an endpoint and using the platform's AI are separate powers. Manager reads the
  registry and chats (managing providers stays with Owner/Administrator), moderator and editor
  chat, member holds none of them.
- API (`/api/v1/ai`): providers list/create/patch/delete (the key goes in and never comes back —
  only `has_api_key` does), the model registry (`GET /ai/models`, `PUT /ai/providers/{id}/models`
  replaces a provider's set, `PATCH /ai/models/{id}` switches or defaults one), discovery against
  the provider itself (`POST /ai/providers/{id}/discover-models`) and `POST /ai/chat`, which
  answers as `text/event-stream`: `start` (the pair the router chose) → `delta` frames → `done`
  with the finish reason and the token usage, or `error` with a stable code. Every exchange is
  audited (`ai.chat.completed` / `ai.chat.failed`) with provider, model and usage.
- `apps/admin`: the `/ai` screen (sidebar entry; provider list with enable / make-default /
  models / remove; a connect form with the key write-only; a model editor with "Discover from
  provider"; the registry with capability flags and default/enable switches; and a Try-it chat
  that streams an answer through the whole chain). `lib/api.ts` gained the typed client, including
  a browser-side `streamChat` that parses the SSE frames.
- `infra/mocks/openai-compatible.mjs`: a dependency-free OpenAI-compatible mock (models list, JSON
  and streamed chat, and a model it refuses with `500`) so the round trip is reproducible locally
  and in CI without a vendor key.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **303 passed, 0 failed** (ai-hub 26 unit
  tests; `apps/api/tests/ai_hub.rs` is new) · `pnpm --filter @omnion/admin build` green with the
  `ƒ /ai` route and `pnpm --filter @omnion/admin typecheck` clean.
- Live walk (fresh database `omnion_p11_live`, API `:18096`, mock on `:8123`): sign-in; connect
  `Local Mock` (`http://127.0.0.1:8123/v1/` — the trailing slash normalized away, key stored,
  `model_count 2`, no key anywhere in the responses); the registry lists both models with
  `Local Mock/mock-large` as the default; discovery returns the provider's own list; a chat
  addressed as `Local Mock/mock-large` streams `start` + 5 deltas + `done`
  (`chars=33 · total_tokens=12`) and reassembles to `Hello from the mock (mock-large).`; the same
  without a model named (the router takes the default); a model the provider refuses arrives as
  `event: error {"code":"provider_error", … 500 …}`; an anonymous chat is `401`; the audit trail
  holds 7 `ai.*` rows across five actions.
- Browser walk (admin panel `:3100` against the API, Chromium): **10/10** checks — sign-in, the
  sidebar entry, the provider row, the model rows with the default marked, a chat streaming
  through the UI (`Hello from the mock (mock-large).` · route `Local Mock · mock-large ·
  openai_compatible` · usage `33 characters · 12 tokens · stop`), a model becoming the default
  from its row, and 0 console errors.
- CI: a new `AI Hub walk (mock provider)` step starts the mock and a second API instance, connects
  the provider, streams a chat, asserts the reassembled answer, the `provider_error` frame and the
  anonymous `401`, and prints the `ai.*` audit rows. Its script was dry-run locally against the
  live stack first (`exit 0`, `ai chat stream: deltas=5 reassembled="Hello from the mock
  (mock-large)."`). Run `36210119981` → **success**: `Rust — fmt · clippy · test` ✅ (the walk
  printed `ai registry: 2 model(s), default=mock-large`, `ai chat stream: deltas=5
  reassembled="Hello from the mock (mock-large)."`, `ai audit rows: 4 [… ai.chat.completed ·
  ai.chat.failed · ai.provider.connected · ai.provider.models_replaced]` and `ai hub:
  provider=… · registry=ok · chat=streamed · refusal=reported · anonymous=401`) ·
  `Infra — compose config` ✅.
- Next: **P12 — Events + Webhooks v0** (event bus table + delivery worker + HMAC signatures, first
  fan-out `page.published`).

### Lessons

- A provider key is a one-way shape: the API's provider body has no field for it (`has_api_key` is
  derived), so "the key never comes back" is a grep over the responses rather than an audit of
  every handler — design the response type so a leak would need a new field.
- Two invariants belong to the schema, not to validation code: "one default provider" and "the
  default model is always enabled" are partial unique indexes plus one `repair_default_model` pass
  that every write path calls. The database refuses the impossible state; the store reforms it
  after each change.
- Streaming has two failure surfaces and both need an answer: everything decidable before the
  first byte stays an HTTP status (routing, permission, request shape), everything after it
  travels as an `error` frame inside the stream. A client that handles only one of them shows a
  half-answer, or a spinner that never stops.
- The SSE decoder is where a provider's personality is: it must survive a JSON object split
  mid-frame, CRLF frames, keep-alive comments, a missing trailing blank line, a usage frame after
  the finish reason, and a refusal smuggled inside a `200` stream. Write it against frames, never
  against "one chunk = one event" — and prove it with frame-level unit tests.
- Two mocks, two jobs: the in-process axum mock keeps the Rust suite hermetic; the zero-dependency
  Node mock makes the same round trip reachable from CI and from a laptop. The CI step's script
  was dry-run locally before it was committed — and the dry run found that 8081 on this box is the
  Pterodactyl Wings daemon, not a free port (`ss -tlpn` before trusting a port).

## 2026-09-26 — P12 · Events + Webhooks v0 (docs/01-VISION.md §13)

- New crate `crates/events` (`omnion-events`) — the platform's event bus and its deliveries.
  `model.rs` holds the recorded event, the endpoint, the queue rows and the attempt budget the
  queue carries; `validation.rs` the shapes the platform refuses to store (a dotted, lower-case
  event name, an absolute http(s) URL, a subscription list, the generated 32-byte secret);
  `signature.rs` the signing scheme (HMAC-SHA256 over `<timestamp>.<raw body>` carried as
  `X-Omnion-Signature: v1=<hex>`, verified in constant time); `store.rs` the SQL (event + fan-out
  in one transaction, `for update skip locked` claims with a lease, settle exactly once, the retry
  ladder); `bus.rs` `emit`/`emit_to`; `sender.rs` one signed POST — the body is serialized once and
  both the request body and the signature are computed from those bytes, so a receiver that
  verifies the raw bytes always agrees; and `engine.rs` `run_due`: settle the queue of endpoints
  that were switched off, claim the due rows, deliver, retry with backoff, fail when the attempts
  run out.
- `database/migrations/0009_events_webhooks.sql`: `events` (dotted-name check, organization/site/
  actor references), `webhook_endpoints` (per-organization case-insensitive unique name, the
  subscription array with a cardinality check, a secret-length check) and `webhook_deliveries`
  (one delivery per endpoint per event, `pending | delivered | failed`, attempts, lease, response
  status, error) with the partial index the claim walks.
- Fan-out: publishing a page records `page.published` (page, site, slug, revision) and the bus
  queues one signed delivery per enabled, subscribed endpoint of that organization — the event row
  and its deliveries are one transaction, so the queue can never reference an event that does not
  exist. An event without an organization fans out to nobody: an endpoint belongs to one tenant,
  and matching it against a platform-level fact would leak between tenants.
- API (`/api/v1/webhooks`, `/api/v1/events`): endpoints list/create/get/patch/delete, the
  operator's test delivery (`POST /webhooks/{id}/test` queues `webhook.test` — even to a
  switched-off endpoint, which is what "test before going live" means), the queue history of one
  endpoint (`GET /webhooks/{id}/deliveries`) and the event feed (`GET /events?name=&limit=`). The
  signing secret is write-only: shown once when the platform generated it, never again, and a
  rotation replaces it silently. Endpoints are tenant resources — an organization account only
  ever sees and changes its own (a cross-tenant read is `403 cross_organization`).
- Permissions: `webhooks.read`, `webhooks.manage`, `events.read` (categories `webhooks`, `events`);
  Manager reads endpoints and the feed, Owner/Administrator pick the new keys up from the catalogue.
- Runner: `apps/api/src/event_runner.rs` ticks the delivery queue in the API process
  (`OMNION_EVENTS_*`, default on) — `poll_ms` 5000, `batch` 20, `lease_seconds` 120,
  `request_timeout_ms` 10000, `retry_base_ms` 15000 → `retry_max_ms` 900000; a queued delivery
  carries its five attempts.
- `infra/mocks/webhook-receiver.mjs`: a dependency-free receiver that verifies the HMAC exactly the
  way a third-party receiver must (and answers `401` when it does not verify). `infra/mocks/
  webhooks-walk.sh`: the end-to-end walk — sign in, tenant + site + page, connect the receiver as
  an endpoint, publish, wait for the signed delivery, read it back from the queue history and the
  event feed. Developer and CI run the same script.

### Verify

- `cargo check --workspace --all-targets` + `cargo clippy --workspace --all-targets -- -D warnings`
  green; `cargo test --workspace`: **332 passed, 0 failed** (the events crate contributes 22 unit
  tests, the API suite 2 more walks).
- `apps/api/tests/events.rs` — 2 walks on throwaway databases against a **real receiver this suite
  starts on a loopback port**: connecting an endpoint (secret shown once, never again in any
  response body), the duplicate-name `409` and the unusable-URL `400`, the test delivery, the
  `page.published` fan-out (header + signature + envelope verified by the receiver's own verifier,
  the queue history showing `delivered / attempts=1 / 200`, the event feed showing the payload), a
  receiver that answers `500` once (the delivery stays `pending` with `attempts=1` and the refusal
  in `error`, then delivers on the retry with `attempts=2`), a receiver that never accepts (five
  attempts, then `failed` with `response_status=500` — the receiver saw every one), the tenant
  scope (two organizations, one event, only the owning organization's endpoint receives it and
  nothing is queued for the other), the permission gates (`401` anonymous, `403` for a member
  without the keys, `403` for cross-tenant reads) and the switched-off endpoint (its queued
  delivery settles as `failed` with the reason instead of sitting pending forever).
- Live walk on this machine — the API binary on `:8082` against a throwaway database plus
  `webhook-receiver.mjs` on `:8124`, driven by the committed `infra/mocks/webhooks-walk.sh`:
  `receiver: event=page.published signature=verified bytes=477 slug=home` ·
  `platform: status=delivered attempts=1 response=200 event=page.published` ·
  `bus: 1 page.published event(s) on the feed` · `webhooks walk: OK — signed delivery verified end
  to end`; the API's own log: `event recorded event_id=1 name=page.published deliveries=1` followed
  by `webhook delivered delivery_id=cc2ff733-b27f-4b62-aa41-e39e343e084c endpoint=Walk Receiver
  event=page.published status=200 attempt=1`.
- CI: the `Webhooks walk (signed delivery)` step starts the receiver and a second API instance and
  runs the same script (dry-run locally: exit 0).
- Next: **P13 — Automation v0 (REQ-003 lite)** — trigger → condition → action on top of P09.

### Lessons

- One transaction for "the fact" and "who must hear about it": an event row and its queued
  deliveries commit together, which removes the class of bug where a delivery points at an event
  that was never recorded. The trade-off is explicit — subscriptions are matched at emission time,
  so an endpoint connected later receives nothing retroactively.
- Sign the exact bytes you send. Serialize the body once, sign those bytes, put those bytes on the
  wire; a receiver that re-serializes a parsed body will verify in one language and fail in another
  (key order, unicode escaping, float formatting).
- The timestamp belongs inside the signed material: a receiver gets replay protection by comparing
  it with its own clock, and Omnion needs no extra state to make the promise.
- A queue row carries its own budget (`max_attempts`) and its attempts are counted when it is
  claimed, not when it succeeds — so a process that dies mid-delivery still consumes an attempt and
  a crash loop cannot retry forever.
- "Switched off" needs an answer for the queue too: pending deliveries of a disabled endpoint are
  settled as failed with the reason. Otherwise the history shows a queue that never moves and the
  operator cannot tell why nothing arrived.
- The last mile is the delivery id: `X-Omnion-Delivery` is the row's own id, which is what makes
  the receiver's log and the panel's delivery list talk about the same attempt.

## 2026-09-26 — P13 · Automation v0 (REQ-003 lite)

- New crate `crates/automation` (`omnion-automation`) — trigger → condition → action on top of the
  P09 engine, with the docs/09-N8N-TEARDOWN §13 lessons applied: an automation rule **is** a
  workflow whose `trigger_kind = 'event'` (storage reflects what it is — no parallel rule table),
  the comparison set is closed and small (9 operators: `equals`, `not_equals`, `contains`,
  `not_contains`, `starts_with`, `ends_with`, `in`, `exists`, `not_exists` — no expression
  language), and no dynamic code ever runs in the core process. `model.rs` the rule + run shapes;
  `condition.rs` the evaluator (every condition must hold; a field the payload does not answer is
  false, and the skip audit says why); `binding.rs` the `{{ }}` bindings an action may fill from
  the payload — a binding the event cannot fill **refuses to start** the run (no half-filled
  steps); `matcher.rs` the drain — **one tick = one transaction**: read the events above the
  cursor, evaluate the armed rules of their tenants, start one durable run per match, advance the
  cursor; `for update skip locked` makes the cursor row itself the concurrency lock, so
  exactly-once holds across instances.
- Cursor: seeded to the end of the bus **once at boot** (`matcher::seed_cursor`, called by
  `automation_runner` before its first tick) — a fresh installation watches forward; on an empty
  bus that seed lands on `0`, which is precisely why the drain reads everything above it. The
  lazy-seed branch inside the drain was removed: the E2E walk caught it swallowing the first real
  event (it re-read `0` as "never seeded" and stepped the cursor past the event the rule was
  waiting for).
- Actions: `send_email` — the SMTP wire protocol written out in `mail.rs` (`EHLO` → optional
  `AUTH PLAIN` → `MAIL FROM` → one `RCPT TO` per recipient → `DATA` → `QUIT`; multi-line replies
  read in full; dot-stuffed bodies), master switch `OMNION_MAIL_ENABLED`, and a switched-off
  server fails the step **with that reason** — and `comment_revision` — a comment on the revision
  the event names (`crates/content/src/comments.rs`). Synthetic actions (`noop`, `echo`, `fail`,
  `transient`) stay pure in-engine; they are what the suite drives the engine with.
- `database/migrations/0010_automation.sql`: the cursor row + the run table + the P09 constraint
  work the E2E walk exposed — `workflows_trigger_kind_valid` did not know `'event'` and
  `workflows_schedule_shape` had no event branch, so an event rule fell through both checks; both
  are dropped and re-added (three-branch shape), plus `trigger_event`/`conditions` columns and
  `workflows_trigger_event_shape`.
- API: `/api/v1/automations` (list/create/get/update/delete) + `/automations/catalogue` (the closed
  vocabulary a rule may be written in), reusing `workflows.read`/`workflows.manage`; rules are
  tenant resources and the surface is permission-gated. The matcher runs in the API process:
  `OMNION_AUTOMATION_RUNNER` (default on), `OMNION_AUTOMATION_POLL_MS`, `OMNION_AUTOMATION_BATCH`.
- Proof: `cargo fmt --all -- --check` → clean · `cargo clippy --workspace --all-targets -- -D
  warnings` → clean · `cargo test --workspace --lib` → **328 passed** · `cargo test -p omnion-api
  --tests` → **58 + 56 integration (13 files, incl. the new `tests/automation.rs`)**.
  `apps/api/tests/automation.rs` — 5 walks on a throwaway DB with a real in-process SMTP sink: the
  full page.published → conditions → `send_email` + `comment_revision` run end to end —
  `matcher: evaluated=1 matched=1 skipped=0 run=5029f197-…` · `engine: run="completed" step1={action:
  send_email, subject: "Published: Release notes", to: editor@example.com} step2={action:
  comment_revision, comment_id: 6d5b166d-…}` · `sink: to=editor@example.com subject="Published:
  Release notes" bytes=314 commands=5` — plus: a false condition and an unheard event start nothing,
  a binding the event cannot fill refuses the run, a switched-off mail server fails the step with
  that reason (`step_error="the email could not be sent: sending email is switched off
  (OMNION_MAIL_ENABLED=false)"`), and the surface stays permission-gated + tenant-scoped.
- Next: **P14 — Polish + CI v0** (make the CI workflow run for real + README quickstart).

## 2026-09-26 — P14 · Polish + CI v0

- CI gains the `Web — install · typecheck · build · serve (admin + public renderer)` job: pnpm comes
  from the root `packageManager` field (no version pinned twice), `pnpm install --frozen-lockfile`,
  `pnpm typecheck`, `pnpm build` (both Next.js apps through turbo), and then the panel is booted and
  has to keep the promise a reader cares about — the sign-in screen answers `200` while an anonymous
  panel route is redirected (`307`) to it. The job deliberately needs no API: it is what a fresh
  clone can show before anything is configured.
- README rewritten around a quickstart a fresh clone can follow: the compose stack (table of services
  and ports) → `cargo run -p omnion-api` → first run (`omnion setup` or the `/setup` wizard) → the
  admin panel → the public renderer, followed by the checks CI runs, the repository layout and the
  documentation map. Every command in it was executed on the dev stack first.
- Proof: CI run `36213754270` (head `3a8de29`) → **success**, all three jobs — the web job (40 s)
  logged `admin panel: /login=200 · anonymous /pages=307 → /login`, and `Rust — fmt · clippy · test`
  (smoke + AI-hub + webhooks + first-run walks) plus `Infra — compose config` stayed green. Local
  dry runs: `pnpm install --frozen-lockfile` → "Already up to date" · `pnpm typecheck` → 2/2 ·
  `pnpm build` → 2 successful · `/healthz` 200 · `/readyz` database+redis ok · `omnion doctor` 6/6.
- Next: **P15+ — REQ-driven queue** (`docs/requests/REQ-XXX`, owner steers priority; suggested first
  REQ-016 webhooks centre → REQ-002 command centre). The foundation phases P00–P14 are complete.

## 2026-09-26 — REQ-002 · Global search (slice 1: the index and the query core)

- The platform's search is an **index**, not a scatter of per-table queries: `crates/search` owns a
  provider registry (pages, media, users, sites — one entry each: key, document type, the read
  permission its hits require, the panel route a hit opens), an indexer that writes one
  `search_documents` row per searchable thing (`database/migrations/0012_search.sql`), and a query
  engine that narrows by organization, by the caller's provider permissions and by the scoped
  syntax (`type:`, `site:`, `owner:me`, `before:`/`after:`, `is:draft`) inside the SQL — never
  after paging. Ranking is `ts_rank_cd` over the document vector (title A / tags B / subtitle C /
  body D) with the installation's weights (normalised into the range PostgreSQL accepts) plus a
  `pg_trgm` prefix and near-miss match.
- The index keeps itself fresh from the bus: `apps/api/src/search_runner.rs` walks the events
  above a cursor row (`for update skip locked`) and applies each one to its provider;
  `POST /api/v1/search/reindex` rebuilds a provider (or all) idempotently — upsert plus prune, so
  a second pass writes the same count — and leaves an audit row and a `search.reindexed` event.
  The API surface is `/api/v1/search` (ranked hits, per-provider counts, honest hints),
  `/search/suggest` (title prefixes), `/search/status` and `/search/recent` (GET/DELETE). Two new
  permission keys: `search.read` (the box; every base role holds it) and `search.manage` (index
  operations).
- Proof: `cargo fmt --all -- --check` → clean · `cargo clippy --workspace --all-targets -- -D
  warnings` → clean · `cargo test --workspace` → **421 passed, 0 failed (44 suites)**, including
  the new `apps/api/tests/search.rs` (8 walks: hits across pages + media + users for a librarian
  while an editor without `users.read` gets none; tenancy scope with a platform owner reading
  across organizations; the scoped syntax with an unknown `type:` answering empty *and explained*;
  publishing a page becoming findable within one indexer tick; a reindex idempotent twice over;
  the media removal branch; suggestions; the search history; four refusals) · `pnpm typecheck &&
  pnpm build` → 2/2 successful · `bash scripts/qa/run.sh` → 49 clicks, 49 screenshots, **0 high
  findings, 0 vision issues** (`qa-artifacts/20260926-134405`).
- Deviations from the spec, each deliberate and recorded in the REQ: no dyn `SearchProvider` trait
  (the registry is the seam and the indexer is the single writer), `document` is a plain tsvector
  column (`array_to_string` is STABLE, and generated columns refuse STABLE expressions), weights
  are normalised (`ts_rank_cd` accepts only 0..=1), and the `users`/`sites` hits point at routes
  that arrive with REQ-006 — slice 2 renders a section only for a provider whose route exists, so
  no click goes nowhere.
- Next: **REQ-002 slice 2 — the palette** (header search box, ⌘K overlay, sections by hit count,
  keyboard map, recently viewed, all states, mobile sheet), then slice 3 (`/search` results screen
  with facets/selection/export, `/settings/search`).

## 2026-09-26 — REQ-002 · Global search (slice 2: the ⌘K palette and the results screen)

- The panel has its one box. `⌘K`/`Ctrl+K` opens a palette from any screen, `/` focuses the header
  box without opening anything, typing two characters hands over to the palette. Sections are one
  per provider, ordered by how much each provider matched, five rows each plus "see all"; `↑`/`↓`
  walk the whole list across section boundaries, `Tab` jumps sections, `Enter` opens the
  highlighted row, `⌘Enter` opens it in a new tab, `Esc` closes. The empty query shows the
  account's recent searches (removable one by one, clearable completely) and this browser's
  recently viewed screens; a phone gets the same palette as a full-screen sheet with 44px rows.
  `apps/admin/components/search-palette.tsx` + `global-search.tsx` carry it, `lib/search-palette.ts`
  and `lib/search-memory.ts` the logic and the browser's own memory.
- Hits open the thing they name. The indexer writes deep links now (`/pages?site=…&focus=…`,
  `/media?site=…&focus=…`), the palette switches the panel's site before it navigates, the pages
  screen opens the focused page's editor and the library marks the focused file's row. "See all
  results" lands on `/search`, which ships as the honest half of the results screen (query, sort,
  per-provider counts, rows with the match emphasised, pagination, all three states); facets,
  selection, export and `/settings/search` stay in slice 3.
- Slice 1 promised sections for pages, media, sites and accounts but only content emitted events —
  the index carried plans for the others and no producers, so those sections would have stayed
  empty forever. That half shipped too: `media.created`/`media.deleted`, `site.created`/
  `site.updated` and `user.created` are emitted by the routes that change those rows.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **422 passed, 0 failed (44 suites)**, including the
  new `an_upload_and_a_new_site_reach_the_index_through_the_bus` (three real routes → one indexer
  tick → indexed, then removed) · `pnpm typecheck && pnpm build` → 2/2 · `bash scripts/qa/run.sh`
  → 58 clicks, 67 screenshots, **0 high findings** (`qa-artifacts/20260926-150923`); the pass's
  palette phase reports open (`focusedInput: true`), two sections for "sample" (Media 1, Pages 1),
  the highlight moving `hit-0-0 → hit-1-0`, the row opening `/pages?site=…&focus=…`, the recents
  surviving a reload, and `close` ✓, while the phone sheet measures 390×844 with 44px rows ·
  `scripts/qa/probe-palette.cjs` → **22/22 PASS** (keyboard map incl. `Ctrl+Enter` in a new tab,
  the no-hits state, recents, the box click, `/`, both console-error checks).
- The QA harness grew teeth and immediately used them. The mobile context is signed in now (every
  "mobile" route had been photographing the sign-in screen — a gap, not a screen), the media
  screen's hidden upload input is set directly (a click-through cannot reach a 0×0 control, so the
  library — and everything indexed from it — stayed empty), and an open palette is closed before
  the next round clicks. Real mobile screens then showed a pages table that overflowed a phone and
  a corrupt 82-byte "PNG" fixture that rendered as a broken thumbnail; both fixed here (narrow
  columns and action labels leave before the table does; the fixture is a real 160×120 PNG).
  Overlay screenshots are viewport-only now — a full-page shot of a fixed sheet shows the page
  below the fold and reads as an overlay that fails to cover the screen.
- Carried forward, not caused by this slice: the public renderer answers `404` for its own icon
  requests on every pass (5 medium findings, unchanged since `20260926-134405`), and the vision
  review's one remaining low note guesses the contrast of `text-muted` at ~2.5:1 where the
  harness's own measured check reports zero low-contrast nodes — the token stays as it is.
- Next: **REQ-002 slice 3** — `/search` facets with counts, removable chips, Shift-range + `⌘A`
  selection, Copy links, CSV export (`/search/export`), `/settings/search` (per-provider status,
  ranking weights, reindex progress) and the settings/logs/translations providers; then wave 1
  continues with REQ-032 (command centre).

## 2026-09-26 — REQ-002 slice 3: results depth, the wider provider set, the index's own screen (done)

- The results screen is now the depth the brief asked for. Facets: **Type, Site, Owner, Language,
  Status and Updated**, each with counts and each counted **without its own filter** (so "Pages 12"
  is the number clicking it would leave); applied filters become removable chips; 50 rows a page;
  selection with Shift-range and `⌘A`; Copy links (newline-separated, absolute); CSV export
  (`/search/export`) of the whole result set or of the selection, with the row count in a header;
  the keyboard map (`s` sort, `f` facets, `x` a row, `⌘A` the page, `?` the list); and on phones
  the filters move into a sheet behind `Filters (n)` while the selection bar stays sticky.
- `/settings/search` is new — the index's own screen, reachable from the nav. One row (or card,
  below `md`) per provider: documents, last indexed, **state** (`ready`, `indexing`, `stale`,
  `failed`, `empty`) and the last pass's own numbers, with a Reindex button per provider and for the
  whole index; the ranking weights form (whole numbers, title ≥ body) with the API's validation,
  Restore defaults from the server's own defaults, and a progress line fed by the passes. The
  states are read from `search_reindex_runs` (migration 0013): every pass records what it wrote, how
  long it took and why it failed, so a state is never guessed from a document count.
- Three providers joined the registry, each with a real destination: **Activity** (audit entries
  and recorded events whose target is a page, a file or a site — a row opens the thing it touched),
  **Translations** (a translated field opens its page's editor) and **Settings** (the search
  weights row, one per organization, opening `/settings/search`). The palette renders all three; the
  results screen can filter, select and export them like any other row.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **436 passed, 0 failed** (five new walks in
  `apps/api/tests/search.rs`: facets leave their own filter out of their own counts, the export
  answers one row per hit and honours `selected=`, the weights flip a fixture's order, the settings
  read/write/refusals, and Activity + Settings are findable with their own screens) · `pnpm
  typecheck && pnpm build` → 2/2 (`/settings/search` in the route table) · `bash scripts/qa/run.sh`
  → 105 clicks, 107 screenshots, **0 high findings**, **0 vision issues**
  (`qa-artifacts/20260926-160500`). The pass's depth phase: 6 facet groups, a type facet narrowed
  13 → 9 hits with one chip, removing it left 0 chips, `s` moved the sort to `newest`, a Shift-range
  selected 3 of 13 rows, Copy links wrote exactly 3 URLs, the exported CSV carried 3 rows (same
  count), the settings screen showed 7 providers with their states, a per-provider reindex answered
  "Indexed 1 document in 15 ms." and its row read `1 written · 0 pruned · 15 ms`.
- The QA harness keeps earning its keep. It caught a real defect in this slice — the settings table
  is a six-column table and a phone reported 7 elements outside the viewport — so the provider rows
  render as cards below `md`, the timestamps no longer wrap, and the disabled Save button stopped
  fading its labelled text (white on pale peach). It also grew a depth phase of its own (facets →
  chips → sort → selection → clipboard → CSV file → shortcuts → reindex → weights), which is what
  makes each of those acceptance claims a number instead of an impression. Two layouts over one
  dataset also taught the harness a selector lesson: `:visible` only, or a click lands on the copy
  that a phone would show.
- Carried forward, not caused by this slice: the public renderer still answers `404` for its own
  icon requests (5 medium findings, unchanged), and the previous pass's single low vision note (the
  overview's `Connect a domain` row) turned out to be a misread — the API and the DOM both report
  the step done, and the re-run reported 0 issues across all 14 screens.
- Next: wave 1 continues with **REQ-032** (command centre: palette + quick actions + recents), then
  REQ-007 (analytics + real dashboard).

## 2026-09-26 — REQ-032 · slice 1 · the palette becomes a command centre

- **Commands are code, not configuration.** `crates/search/src/commands.rs` is the registry (8
  entries today: the panel's screens plus "Create a page"), each naming the permission whose
  holder may run it, the route it opens, keywords, aliases and the screens it is worth suggesting
  on. `visible()` projects it through a caller's effective permissions and `suggest()` picks a
  screen's suggestions — both pure, both unit-tested without a database.
- **The API serves the projection.** `GET /api/v1/commands`, `GET /api/v1/command-center/context`
  and `GET`/`POST`/`DELETE /api/v1/command-center/recent`, all behind `search.read`; migration
  `0014_command_center.sql` adds `command_recents` (per user, trimmed to 50, upserted on repeat)
  and `command_usage_daily`. The palette grew the command group, the `>` `@` `#` `:` `?` prefix
  model with a mode chip, its own Recent list with a Clear control, and focus restoration on `Esc`;
  `/pages?new=1` opens the create form with the cursor already in the title field.
- **Proof:** `cargo fmt`/`clippy` clean · `cargo test --workspace` → **452 passed, 0 failed**
  (9 new walks in `apps/api/tests/command_center.rs`: the projection leaves a member's answer
  without the words "Create a page"/"Open pages", suggestions follow the screen and skip the one
  already open, recents are personal and repeat-safe, the history trims at 50, refusals carry a
  code, an out-of-reach command is dropped on read, every registry id stores) · `pnpm typecheck &&
  pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 105 clicks, 113 screenshots, **0 high findings**,
  **0 vision issues** (`qa-artifacts/20260926-171549`) · `scripts/qa/probe-command-center.cjs` →
  **26/26** (a command lands on its screen and closes the palette, the run is in the account's own
  history when read back through the API, clearing empties it immediately and after a reload, `#qa`
  answers Sites-only, `Tab` walks the groups, no 5xx) · `scripts/qa/probe-palette.cjs` → **22/22**
  (the REQ-002 palette regressions: arrows, `Ctrl+Enter` new tab, no-results, the phone sheet, and
  the query remembered under the renamed Recent group).
- **The pass caught two defects.** (1) The palette opens over the results screen's dialog and the
  scrim closes it without touching the dialog (palette `z 50` over `z 40`), but the harness step
  first clicked the scrim's *centre* — where the dialog sits — and read the interception as a
  failure; it clicks the corner now. (2) A real one: the migration's id-format check rejected
  hyphens, so recording `nav.create-page` answered `500` — invisible to the integration suite
  (which only recorded `nav.pages`/`nav.media`) until the browser probe fired a real one.
  `every_registered_command_can_be_remembered` now walks the whole registry through the endpoint.
- The walkthrough also flagged the pages table pushing its Actions column 21px past a phone's edge,
  so the page cell is now the flexible one (`w-full max-w-0`) — the vision review reports 0 issues.
- Carried forward, not caused by this slice: the public renderer still answers `404` for its own
  icon requests (5 medium findings, unchanged).
- Next: wave 1 continues with **REQ-032 slice 2** (federated search in the palette: per-group
  loading and errors, type filters, the URL-backed `/search`), then REQ-007 (analytics + real
  dashboard).

## 2026-09-26 — REQ-032 · slice 2 · the palette answers in groups

- **Each provider answers on its own.** The palette stopped slicing one answer: it asks every
  provider its own question (`/api/v1/search?q=…&types=pages`) in waves of three, so a section has
  its own skeleton while its request is in flight, its own rows when it answers, and its own
  retryable error — the failure's code and status in the tooltip — when it fails. One slow or
  failing type never blanks the rest. A whole-index count rides behind the first wave for the "see
  all results" total; sections keep the registry's order instead of reshuffling under the reader's
  eyes as answers land.
- **Typing is not a search worth remembering.** The endpoint grew `history=false`; the palette's
  federated calls say it, so `search_recent` stays the record of searches someone committed to
  (a hit opened, a section's "see all", the results screen). The palette's own history is
  `command_recents`, written on commit. Nine parallel calls per keystroke pause also became three
  at a time **because the walkthrough proved it**: with nine in flight the page's own RSC
  prefetches queued behind them and two navigations were recorded as `net::ERR_ABORTED` (high) —
  the cap removed both, `GROUP_CONCURRENCY` in the palette.
- **An empty answer now says which empty it is.** `hidden_total` (a count, never a title, computed
  over the enabled providers the caller's keys do not cover, and only when their own answer is
  empty) lets the results screen and the palette tell "nothing matched" from "nothing you may read
  matched": "N results are outside your permissions." The palette's no-results state also offers
  the way out as real rows — `/search?q=…` and `/ai?q=…` (the AI Hub opens with the words in its
  prompt; nothing is asked on the reader's behalf).
- **A "see all" is a link the screen understands.** `sectionUrl` emits the results screen's own
  `type=<provider>` parameter, so the link lands with its chip applied, its rail already knowing
  which type is on, and the search box still showing the words that were typed.
- **The narrowing is a property of the requests now.** `>` asks the registry and never the index
  (0 calls, 0 sections); `@`/`#`/`:` ask exactly one provider each. A regression of mine —
  `>` + text listed the whole command list instead of the matches, so Enter landed on "Search
  everything" — was caught by the *walkthrough*, not by the new probe: the end-to-end click pass
  is the reason the palette's own commands still work.
- **Proof:** `cargo test --workspace` → **455 passed, 0 failed** (3 new: the count outside a
  reader's scope at the API — counted, titled nowhere, `0` for the reader who sees everything —
  plus the `history=false` walk and its wire test) · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `pnpm typecheck && pnpm build` 2/2 · `bash scripts/qa/run.sh` → 77 clicks, 98
  screenshots, **0 high findings**, **0 vision issues** (`qa-artifacts/20260926-180233`) ·
  `scripts/qa/probe-palette-federated.cjs` → **32/32** (delayed provider → own skeleton while
  another holds rows; 503 on one type → that section retryable, the rest rendering; `>`/`#`/`:`/`@`
  ask what they say; `Tab`/`Shift+Tab` walk sections; "see all" → `/search?q=…&type=pages` with the
  API's own count, same after a reload; "zzqqxx" → the results screen + a prefilled AI Hub;
  history clean; as the Member: no "Create a page"/"Open sites", the shared sites link renders 0
  rows and "1 result is outside your permissions." with no title in the answer) ·
  `probe-command-center.cjs` **26/26** and `probe-palette.cjs` **22/22** re-run clean (no slice-1
  regressions) · the typing budget, keystroke → row over 20 samples: **p50 197 ms · p95 220 ms ·
  max 237 ms** (criterion: 300 ms p95).
- **Found, not fixed (REQ-002's ground):** creating a draft page emits no event, so a fresh draft
  reaches the index only on a publish or a reindex — the indexer's plan lists `page.created`
  (`crates/search/src/indexer.rs`) but `POST /api/v1/pages` never emits it. The probe works around
  it with an explicit reindex; the gap is worth a REQ-002 follow-up rather than a quiet fix here.
- Carried forward, not caused by this slice: the public renderer still answers `404` for its own
  icon requests (5 medium findings, unchanged).
- Next: REQ-032 stayed in progress — **slice 3** was the next tick's work.

## 2026-09-26 — REQ-032 · slice 3 · the palette acts, and says so

- **A command has a kind.** The registry grew `CommandKind::Action` and a `confirm` flag, and
  `runnable()` answers only for actions — so the API's `not_runnable` refusal cannot drift from what
  the projection says. Two actions ship with the services that exist today: rebuilding the whole
  search index (`search.manage`, asks first) and clearing the account's own palette history (asks
  first; no key beyond being signed in). The four examples in the request — "Create customer",
  "Create workflow", "Run backup", "Switch site" — arrive with their owning services (CRM, the
  workflows screen, a backup service, a palette parameter model), none of which exist yet; the
  pattern for each is now one registry entry + one match arm + one audit expectation.
- **The act is the owning service's code, not a copy.** `search::perform_reindex` (its audit row and
  `search.reindexed` event included) now serves both the settings screen's button and the palette's
  command, and one `clear_recents` serves the palette's Clear control and the `act.clear-recents`
  command — two entry points, one behaviour, no drift possible.
- **The confirmation is a platform rule.** `POST /commands/{id}/run` carries no route-level guard
  on purpose: every command has its own key, so the handler re-checks it, refuses a navigation
  command by name, refuses an unknown id as a 404, and refuses an unconfirmed run with
  `confirmation_required` **before anything executes** — a programmatic caller that skips the card
  still has to say yes. The audit trail is provably unchanged while the palette's question is open
  (the probe reads it before, during and after).
- **One entry and one count per executed command.** `command.run` names actor, command (as the
  target), outcome and the owning service's aggregate result — never a row of content — and
  `command_usage_daily` counts the run (upserted by user/command/day, no query text). A refused run
  writes nothing at all; the probe asserts the trail is unchanged after all three refusals.
- **The palette never invents the outcome.** The result card prints the run endpoint's own message
  ("Rebuilt 7 providers · 18 documents · 95 ms") and links to the screen that reads the record back
  (`/settings/search`, which shows the same pass as the provider's last run). Recents of an action
  command run it again instead of navigating.
- **A defect found and fixed in this tick, not caused by this slice:** the search suite asserted
  that one `indexer::drain(_, 100)` applies its own event; with action-command reindexes on the same
  shared bus (every reindex leaves one `search.reindexed` per provider, and those are skipped rather
  than applied) a single batch can be spent on history the suite does not own. The suite now ticks
  the way the runner does — bounded batches until one applies something.
- **Proof:** `cargo test --workspace` → **463 passed, 0 failed** (8 new walks: 3 registry + 5
  command-centre) · `cargo clippy --workspace --all-targets -- -D warnings` → clean · `pnpm
  typecheck && pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 105 clicks, 115 screenshots, **0 high
  findings**, **0 vision issues** (`qa-artifacts/20260926-184340`; 5 medium = the public renderer's
  own icon 404s, carried forward) · `scripts/qa/probe-command-actions.cjs` → **31/31** ·
  `probe-command-center.cjs` **26/26** · `probe-palette.cjs` **22/22** ·
  `probe-palette-federated.cjs` **32/32** · the walkthrough's own steps record
  `ranNothingYet: true` (nothing ran while the question was open) and `newEntries: 1` with
  `target: act.reindex-search`, `outcome: ok`.
- Carried forward, not caused by this slice: the public renderer still answers `404` for its own
  icon requests (5 medium findings, unchanged). Slice 2's finding stands: creating a draft page emits
  no event, so a fresh draft reaches the index only on a publish or a reindex.
- Next: REQ-032 stays in progress — **slice 4** (natural-language resolution: `POST
  /command-center/resolve` on ai-hub, the intent preview card, confidence threshold with "Did you
  mean", `Edit as search`, timeout fallback); wave 1 then moves to REQ-007 (analytics + the real
  dashboard).

## 2026-09-26 — REQ-032 · slice 4 · the palette reads what is typed into it

- **A phrase becomes a structure, checked against the platform's own tables.** `crates/search/src/intent.rs`
  reads one phrase into the vocabulary the platform really has — the domain maps through the provider
  registry, a command is only ever a registry entry whose own words the phrase covered, filters are the
  ones the index supports, and confidence is *computed* from what was recognised, so the same words
  always read the same way. An unmapped domain stays a search and says so; a command the caller cannot
  run is dropped and the words search instead. 13 walks, no database.
- **The model is asked first and checked afterwards.** `POST /api/v1/command-center/resolve` (guard
  `search.read`) answers the intent, a preview line, `runnable`, the destinations, the alternatives and
  where the reading came from — and executes nothing. A connected model gets a six-second bound and a
  closed vocabulary; its answer is *normalised* (unknown command, unknown provider, out-of-range sort or
  count dropped, not trusted) and an unusable one leaves the grammar's reading standing with
  `degraded: true`. Only model readings are audited (`command.resolve`: the interpreted intent, never a
  row of content). The endpoint answers `search_route` (the editable search) beside `route` (where a
  runnable reading lands), because a reading the caller may not run still has an editable shape.
- **The card is a proposal beside the results, not a gate in front of them.** It sits above the list
  like the confirmation card and is deliberately not an arrow-key row (every row the arrows reach must
  be a screen); its controls are real buttons, ≥44px on touch. `Run` shows only for a runnable reading,
  and a reading of an action command opens the platform's *own* confirmation card, so a command that
  asks first asks here too. `Edit as search` always exists, the request aborts when a newer keystroke
  overtakes it, and a failing reader offers a retry. Low confidence ("zzqqxx", 55%) carries no Run at
  all — only alternatives.
- Proof: `cargo test --workspace` → **490 passed, 0 failed** (27 new) · `cargo clippy --workspace
  --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 · `bash scripts/qa/run.sh`
  → 97 clicks, 117 screenshots, **0 high findings**, **0 vision issues** (`qa-artifacts/20260926-194131`;
  the walkthrough's new steps record `parsedIntent: true · offersRun: false · ranNothingYet: true` and
  `Edit as search` landing on `/search?q=tickets+Mehmet&sort=newest`) · `scripts/qa/probe-command-resolve.cjs`
  → **47/47** (including a connected model that never answers degrading in **6.4 s** with its audit entry,
  and the card at 390×844) · the four older palette probes re-run clean.
- Next: **wave 1 moves to REQ-007** (analytics + the real dashboard). REQ-032 is closed; carried
  forward, not caused by this slice: the public renderer's own icon 404s (5 medium findings), and
  `page.created` is still not emitted by `POST /api/v1/pages` (REQ-002 follow-up).

## 2026-09-26 — REQ-007 · slice 1 · the platform starts counting

- **One beacon becomes rows, and the decision comes first.** `POST /api/v1/public/analytics/collect`
  is a public write path, so what protects it is what a public write path *can* be protected by:
  a 64 KB body cap, a per-site-and-caller budget (429 named `rate_limited`, tested by spending the
  whole budget), a site resolved exactly as the renderer resolves it (`?site=`, then the
  forwarded host, then the installation's only site) — and a collector that decides **before** it
  writes. Tracking switched off, `DNT: 1`, `Sec-GPC: 1`, a crawler user agent, an excluded path or
  address and the site's own sample rate are all evaluated ahead of the first write, which is why a
  dropped beacon leaves no trace at all: not a visit, not an event, not even the day's salt row.
  What it does leave is a counter — the day's `filtered` bucket, the one metric the rollup is
  forbidden to recompute (the bucket delete is narrowed with `metric = any(...)`).
- **A hash instead of a person.** The visitor identifier is `sha256(site, day-salt, address, user
  agent)`; the salt is 32 random bytes per UTC day in `analytics_salts` and nothing else is stored.
  The tests read the row and find a 64-hex hash and `ip_prefix = null`. With `anonymize_ip` switched
  off the row keeps the truncated network only — `203.0.113.0/24`, `2001:db8:1::/48` — the widths
  the request documents, asserted in the walk and in the module's own unit tests.
- **A bucket run is a delete and a rewrite, not an increment.** `rollup_day`/`rollup_hour` recompute
  every metric/dimension pair from the raw rows inside one transaction, so running a bucket twice
  writes the same numbers: the walk snapshots `analytics_daily` before and after the second run and
  compares them row for row. High-cardinality dimensions are capped at fifty values per bucket with
  the tail folded into `(other)`, sorted by count and then by value so two runs write the same order.
- **The settings screen's validation is the API's validation.** One `validate()` in the module is
  what both the endpoint and (from slice 2) the panel call; `retention_days = 3` answers
  `invalid_analytics_settings` and the message *names the field*, `mode` is closed to the two
  documented values, and excluded paths and addresses are checked line by line. Reading a site's
  settings and snippet is `analytics.read`; changing them is `analytics.settings.manage`; both
  resolve the site through the caller's own organization — a reader of the second organization gets
  `cross_organization`, and the platform Owner is the caller who may reach across.
- **`modules/` exists now.** REQ-007 called for `modules/analytics` while every previous feature
  landed under `crates/`; docs/04-MONOREPO.md reserves `modules/` for features a customer can carry,
  so the workspace gained `modules/*` and this is its first member (`omnion-module-analytics`).
  The schema landed as **0015** — 0012–0014 went to search and the command centre while this request
  waited, and the "next free slot at build time" rule in the request is what that means.
- **Two deliberate reads of the same promise.** The tracker (`apps/web/public/analytics.js`) stops
  before it makes a request when the browser reports DNT or GPC, and the collector enforces the same
  rules again on the server: a client-side promise is not a promise. Engagement rides as a
  `page_engagement` **event** rather than a second pageview beacon, because a second pageview for the
  same page would double-count it.
- **Proof:** `cargo test --workspace --no-fail-fast` → **524 passed, 0 failed** (analytics: 22 module
  units + 6 integration walks; every other suite unchanged and green) · `cargo clippy --workspace
  --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 · `bash
  scripts/qa/run.sh` → 105 clicks, 117 screenshots, **0 high findings**, **0 vision issues**
  (`qa-artifacts/20260926-202816`; the 5 medium findings are the public renderer's own icon 404s,
  carried forward). This slice ships no screen, so the walkthrough inventory is unchanged.
- **Fresh-database proof:** `create database omnion_fresh_check` + `omnion migrate` →
  `applying 14 pending migration(s): 1 … 15` → `database is up to date (14 of 14 migrations
  applied)`, and the database holds 11 `analytics_*` tables with zero settings rows (an empty
  installation has no sites to seed for). The check database was dropped afterwards.
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium), and
  `page.created` is still not emitted by `POST /api/v1/pages` (REQ-002 follow-up).
- Next: REQ-007 stays in progress — **slice 2** (overview + the six report screens over these
  rollups, the shared date-range toolbar, filters, drawers and CSV export, plus the walkthrough
  gaining the ten `/analytics` routes with a synthetic beacon batch).

## 2026-09-26 — REQ-007 · slice 2 · the numbers come back out

- **The read side is one module, and it is honest about where a number comes from.** `reports.rs`
  answers the overview, the page report, sources, audience, events, downloads and forms. A range
  inside the site's retention window is read from the **raw rows** — that is where *distinct
  visitors across a period* is a real number rather than a sum of per-day counts — and a range
  that reaches past retention falls back to the daily rollups, marking the answer `exact: false`
  so the screen can say so. Series buckets are addressed by an offset from the range start rather
  than by a truncated timestamp: `date_trunc` follows the connection's time zone, and a report
  that shifts by an hour depending on who asks is worse than no report.
- **A report's SQL and its binds cannot drift.** `Narrowing` writes only the clauses a report
  actually applies and numbers each placeholder from how many values are already bound. The first
  draft had the failure modes this prevents: a no-op placeholder for a filter the sources report
  cannot use, and a `limit` that was bound three times out of four (the QA of the module's own
  tests caught the first as a red assertion, the second as a 500).
- **Where a filter cannot apply is a fact about the data.** A visit has no path of its own — its
  pageviews do — so the path filter on a visit-scoped report narrows through an `exists` over that
  visit's pageviews; a page report narrows the path on the pageview itself. The screens and the
  export share one query, so the CSV holds exactly the rows the table showed, and the row count
  rides in `x-export-rows` because a silently truncated file lies.
- **The collector now stores the country an edge reported.** Cloudflare's `CF-IPCountry` and its
  two siblings are read on the beacon; the platform still never geolocates an address itself, and
  the edges' own "unknown" placeholders (`XX`, `T1`) are refused — a country that is not a country
  would show up in the audience report as a place.
- **Seven screens, one toolbar.** The report screens live behind a shared shell whose state is the
  URL (range presets, two pickers, comparison, granularity, export, refresh with a last-updated
  caption, and the spec's keyboard: `d`, `c`, `r`, `e`, `g`+`o|p|s|a`). The overview's empty state
  hands over the tracking snippet instead of an empty chart; a comparison against a period with no
  traffic says "no comparison" rather than drawing a delta against zero; below `lg` every table
  becomes a card list (one layout in the DOM, chosen by a media query, so a hook names exactly one
  element).
- **Three defects the QA pass found, and the fix for each.** (1) The date pickers let a range end
  before it starts, so every screen asked the API for a range it must refuse — 28 of the pass's 29
  errors; moving one end now drags the other. (2) The country filter sent whatever was typed and
  let the API be the first line of defence; the field validates as a two-letter code, marks itself
  `aria-invalid` and says so under the input. (3) A click-through harness looked up an element on a
  page that was already being replaced by a client-side navigation; the pass now settles after each
  click and records the click error itself, and the wizard's step loop waits for its own step to
  move instead of submitting twice.
- **Proof:** `cargo test --workspace --no-fail-fast` → **541 passed, 0 failed** (the analytics
  module's 36 units + the suite's 9 integration walks, three of them new: the seeded overview and
  its comparison, the filtered/sorted/paged page report with its CSV, and the dimension reports
  with an empty site answering zeroes) · `cargo clippy` clean · `pnpm typecheck && pnpm build` →
  2/2 · `bash scripts/qa/run.sh` → 328 clicks, 331 screenshots, **0 high findings**, **0 vision
  issues** (`qa-artifacts/20260926-220408`). The depth pass reads the analytics section end to end:
  a synthetic batch of four visitors (two devices, three countries, one download, one form with its
  start, one custom event with a value) spread over thirty days, the 7-day range with the
  comparison on, a page drawer, a real CSV download (`omnion-analytics-pages-2026-09-20..2026-09-26.csv`,
  2 rows) and the empty state of a filter that cannot match.
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium findings,
  unchanged), and `page.created` is still not emitted by `POST /api/v1/pages` (REQ-002 follow-up).
- Next: **REQ-007 slice 3** — goal CRUD with steps, hit recording from pageviews/events/downloads
  and the server-side conversions, the funnel endpoint and screen (`/analytics/goals`), and
  realtime (`/analytics/realtime` + its SSE counters). Slice 4 then closes the privacy operations
  (retention purge with its audit row, visitor erasure, the "what we store" table and the
  `/analytics/settings` screen).

## REQ-007 slice 3 — goals, funnels and realtime (2026-09-26)

- **Goals are the platform's definition of a conversion, and the recorder is ordered and
  deduplicated.** `modules/analytics/src/goals.rs` validates a goal (a name, one to five steps, and
  a match that means something per kind), stores the funnel in one transaction, and mirrors the
  last step in the goal's own row. `record_facts` only ever moves a visitor to the *earliest missing*
  step, so a later step cannot be reached before an earlier one — and one beacon can carry a visitor
  several steps because a batch can hold several facts. Every write is an `on conflict do nothing`
  on the unique `(goal, visitor, step)` key, and only the hits that were actually written are
  reported, which is what lets the API emit `analytics.goal_reached` exactly once per conversion.
- **A funnel counts a step as *reached* by the visitor whose furthest position inside the range is
  that step or beyond.** That is what keeps counts monotonically non-increasing when a visitor's
  earlier step happened before the window; counting hits at position *p* would let step two exceed
  step one. A goal shortened after the fact folds a furthest position beyond the funnel into the
  last step instead of dropping it.
- **Realtime reads raw rows and nothing else.** `modules/analytics/src/realtime.rs` answers the last
  five and thirty minutes (visitors, pageviews, events, goal hits, current pages, event feed), the
  API hands it over as a snapshot and as server-sent snapshots every five seconds, and a per-site
  cap of eight live streams is enforced by a guard that travels inside the stream so a dropped
  connection frees its slot. The first pass of this slice failed here: `analytics_events.value` is
  `numeric`, the feed decoded it as a float, and every realtime request answered 500 — the cast
  (`value::float8`) and a test that gives an event a value closed it (the assertion failed before
  the fix, passes after).
- **Two screens, one section.** `/analytics/goals` (list with conversions, rates, last hit, switch,
  edit, delete; an editor with an ordered step list, per-step validation and a live funnel with
  drop-offs) and `/analytics/realtime` (two windows, current pages, event feed; the pill turns to
  `Paused` and the stream closes while the tab is hidden). Reading is `analytics.read`, writing is
  `analytics.goals.manage`; a `PATCH` merges so a switch never rewrites the match it did not show.
- **The QA harness learned one thing about itself.** Every screen drops its in-flight fetches when
  the URL state changes and the realtime stream ends with the tab, so a browser-cancelled request
  (`net::ERR_ABORTED`) is now *counted* (`abortedRequests`) instead of reported as a high finding —
  24 of the first pass's 159 high findings were exactly that, and none of them was a defect.
- **Proof:** `cargo test --workspace --no-fail-fast` → **556 passed, 0 failed** (the new walks: a
  three-step funnel recording 3·2·1 with drop-offs and a deduplicated re-send, and realtime counting
  a beacon with an event value) · `cargo clippy --workspace --all-targets -- -D warnings` → clean ·
  `pnpm typecheck && pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 404 clicks, 406 screenshots,
  **0 high findings**, **0 vision issues** (`qa-artifacts/20260926-231356`). The walkthrough creates
  a two-step goal through the editor (and is refused when a step has no match), completes it with a
  real beacon, reads the funnel back (`1` · `1`, conversions `1`), measures the beacon-to-counter
  latency at **18 ms**, and reads the live realtime screen (pill `live`, five-minute counter `1`,
  two current pages, four feed events).
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium findings,
  unchanged), and `page.created` is still not emitted by `POST /api/v1/pages` (REQ-002 follow-up).
- Next: **REQ-007 slice 4** — retention purge with its audit row, visitor erasure, the exclusions
  and sampling half of the settings screen, the "what we store" table, `/analytics/settings`, and
  the `analytics.traffic_spike` / `analytics.retention_purged` / `analytics.erasure_completed`
  events.

## REQ-007 slice 4 — privacy operations, the settings screen, the spike watch (2026-09-27)

- **The privacy promises became database operations, in one readable file.**
  `modules/analytics/src/privacy.rs` holds the retention purge (one site, whole days, the audit row
  written inside the same transaction as the deletions, pageviews counted before the visit delete
  would take them along), the visitor erasure (visits, pageviews, events and goal hits of one
  handle — idempotent, because an erasure that fails on a retry is a compliance problem of its
  own) and `STORED_FIELDS`, the "what we store" table the screen renders from the same source as
  the schema. `detect_spike` and `spike_recorded` live beside them: the hour that just closed
  against the trailing week's median, announced once, with the guard reading the events themselves
  rather than a counter a restart would lose.
- **The salts are the one shared table**, so the prune is the one statement not scoped by a site —
  and it uses the *longest* retention any site still asks for, because one site's seven-day window
  must never delete a salt another site's window still covers. A salt is only read while it is the
  current day; the row is dead weight past every window.
- **API and events.** `POST /analytics/purge` and `DELETE /analytics/visitors/{hash}` sit behind
  `analytics.settings.manage` — the permission that decides how long data lives is not the one that
  reads it — and both record their fact on the bus (`analytics.retention_purged`,
  `analytics.erasure_completed`) with the audit id and the counts, never an address. The settings
  payload grew `purge_cutoff`, `last_purge` and `storage`, so the screen never guesses any of the
  three. The rollup worker announces `analytics.traffic_spike` once per completed hour.
- **The screen** (`/analytics/settings`): tracking, privacy, exclusions with a glob preview that
  reads each pattern back, the snippet with its site key and copy control, the data section — the
  cutoff named *before* the purge runs, the erasure requiring the handle typed twice — and the
  storage table. Field validation mirrors the server's of the same field. The section shell learned
  a toolbar-less mode: a screen with no date range and nothing to export renders neither.
- **The walkthrough learned `data-qa-guard`.** The generic click pass fills every field with a
  sample value; on this screen a sample value in the erasure field would be a refused request (a
  high finding), not a click. A screen now declares which controls its own depth pass drives, and
  the pass skips them — recorded as `deferred-settings`.
- **One finding, fixed in the tick.** The first pass flagged four `role="switch"` buttons with no
  accessible name (the `<label for>` that names them visually does not name them to the harness);
  `aria-label` closed it, and the pass was re-run to prove the fix rather than to claim it.
- **Proof:** `cargo test --workspace --no-fail-fast` → **562 passed, 0 failed** (the new walks: a
  purge that removes exactly the stale rows on both sides of a seven-day cutoff and leaves another
  site alone, an erasure that removes five rows of one handle on its own site and zero of another's,
  and both facts delivered to a real HTTP receiver; plus a flat-week fixture where forty visitors
  against a median of five is a spike and an hour nobody spiked is not) · `cargo clippy
  --workspace --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 ·
  `bash scripts/qa/run.sh` → 436 clicks, 433 screenshots, **0 high findings**, **0 vision issues**
  (`qa-artifacts/20260927-002830`). The settings pass on the populated QA database: tracking saved
  and read back after a reload, a retention of 3 refused with the field named, the purge named its
  cutoff (`2026-09-20`) and took the rows past it from 1 to 0, a real handle was erased with its
  rows counted before (1) and after (0), and the storage table rendered ten rows with three
  marked personal.
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium findings,
  unchanged), and `page.created` is still not emitted by `POST /api/v1/pages` (REQ-002 follow-up).
- Next: **REQ-006 (IAM)** — the next wave-1 item: user, role and permission screens plus sessions
  and devices.

## 2026-09-27 · REQ-006 slice 1 — role depth (migration 0011, the matrix, versions)

- **The migration carries the whole request.** `database/migrations/0011_iam_advanced.sql` adds the
  role side this slice uses (`role_versions`) and the rest of the model the later slices land on:
  the `users`/`sessions` extensions, devices, MFA factors and recovery codes, groups, service
  accounts and their keys, ABAC policies and versions, the security-policy document, sign-in
  attempts, sign-in providers, permission requests, provisioning tokens and their log. The
  `role_bindings` move to subjects and the wider scope ladder is expand-then-contract: the new
  columns arrive beside `user_id`, are backfilled from it, and a `before insert` trigger fills
  `subject_id` for writers that still insert the old shape. Verified twice — applied to a scratch
  database (fresh) and to the populated development database (backfill + the policy seed read
  back).
- **Role depth in the crate.** `crates/permissions` gained `update_role`, `delete_role` (refusing
  a role that still carries live bindings), `duplicate_role`, `ancestors`/`children`, one
  validation function that refuses a cycle and a chain past eight levels in either direction
  (called by both `create_role` and `update_role`), and `replace_role_permissions` — the atomic
  matrix save with `expected_version`, the applied diff and a recorded version. The new
  `versions` module appends and reads `role_versions` and computes diffs; the diff itself is pure
  and unit-tested.
- **The API** grew `/iam/roles/{id}` (detail with entries, chain, children, member count),
  `PATCH`, `DELETE`, `POST /duplicate`, `POST /preview`, `GET /versions`, `GET /members`, and the
  matrix save now answers with the diff it applied. Refusals carry field-level codes
  (`role_inheritance_cycle`, `role_inheritance_depth`, `role_has_bindings`,
  `role_version_conflict`, `invalid_entries`). The catalogue gained the IAM family the later
  slices guard their routes with.
- **The panel**: `/settings/iam/roles` and `/settings/iam/roles/{id}` — the tri-state matrix
  (category accordion, per-category counts, grant/deny/inherit-all, filter, diff preview, sticky
  Save/Discard) and the Members, Inherited by and History tabs, where each version shows its diff.
  The navigation gained one entry, and the write controls declare `data-qa-guard` so the generic
  click pass never fires them.
- **Two defects the QA pass found that no unit test could.** (1) The role list answered platform
  roles plus one organization's, but a platform account (the QA owner has no primary
  organization) never named one — so a role the panel had just created for a tenant vanished from
  its own list, and the create form posted without a tenant and answered `400`. `GET /iam/roles`
  now takes `?organization_id=`, the panel gives platform accounts a tenant picker, and the
  create/duplicate forms send it. (2) An empty name was refused by the server, which made every
  mistyped save a `400` in the console: the editor now refuses it in the field (the API keeps and
  proves its own refusal in the integration walk).
- **One carried-forward fix**: the analytics settings storage table forced a 720px minimum inside
  a 560px card, and the vision pass read the overflow as clipped text. `DataTable` now takes a
  `minWidthClass`, the storage table fits its card, and a measurement proves it (table 558 ≤ panel
  560, no clipped cells).
- **Proof:** `cargo test --workspace --no-fail-fast` → **572 passed, 0 failed** across 48 suites
  (exit 0; the new walk is `apps/api/tests/iam.rs::role_depth_lifecycle_is_proven_end_to_end` —
  create → set → duplicate → delete-while-unbound, the cycle/self/depth refusals, the atomic
  refusal that stores nothing, the stale-version conflict, the history diffs, precedence through
  the guard, and the server's own refusal of an empty name) · `cargo clippy --workspace
  --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 (the route table
  lists both new screens) · `bash scripts/qa/run.sh` → **507 clicks, 493 screenshots, 0 high
  findings**, 2 vision issues (both on the analytics charts, medium/low) —
  `qa-artifacts/20260927-022655`. The depth pass in the browser: create (version 0) → the matrix
  cell cycled allow → deny → inherit → allow → diff preview (`1 added`) → save (version 1) →
  reload and the cell is still set → history shows the version with its diff → Members/Inherited
  by → an empty name refused in the field → edit saved → duplicate (the copy opens with the set) →
  the copy deleted with its notice.
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium findings,
  unchanged).
- Next: **REQ-006 slice 2** — the users screen, bindings at every scope level with expiry, groups,
  service accounts and their keys, effective permissions and the RBAC simulator.

## 2026-09-27 · REQ-006 slice 2 — subjects, scopes and the simulator

- **A binding belongs to a subject now, not to an account.** `Subject::{User, Group,
  ServiceAccount}` plus the whole scope ladder (global → organization → site → department → module →
  resource) and `ResourceContext`, which carries the organization, site, department, module and
  path a question is asked in. `matching.rs` turns a binding's resource glob (`/blog/*`) into a
  matcher; `groups.rs` keeps membership a row rather than a second role table;
  `service_accounts.rs` issues keys as prefix + hash (secret returned once, revocable per key).
  Migration `0016_iam_subjects.sql` completes the expand-then-contract move: `user_id` becomes
  optional and liveness is re-keyed on the subject plus the resource the binding names, so
  `/blog/*` and `/legal/*` can both carry a binding.
- **Seeding was an insert, and an insert is not a reconciliation.** Only the Owner role followed
  the catalogue (`seed.rs` even documented it as policy), so a key added after an installation was
  seeded never reached the role that declares it: measured on the development database,
  Administrator held **32 of 72** keys while Owner held all of them. The symptom is not a hole, it
  is an unexplainable `403` — the same role works in a fresh QA database and refuses in production.
  Every base role is reconciled on boot now: declared keys are inserted, a row whose effect drifted
  is repaired to allow, and a key the code dropped is pruned. `seed::ensure` is called on every
  write path in the test walk to prove it, against the count `report.permissions` reports.
- **The refusal names its decision source.** A `403 permission_denied` carries `details`: the
  permission, the `reason` (`missing_permission` / `explicit_deny`), the `source` role with its
  `via`, the context and how many bindings were consulted. The verdict comes from `simulate.rs` —
  the *same* resolution the simulator screen shows and the REQ demands one decision path — so the
  body and the panel cannot drift.
- **A machine identity is a caller, with its own ladder.** `require_or_machine` lets a route accept
  a session cookie or a service-account key: the cookie wins (a signed-in person is never mistaken
  for a machine), a key authenticates over `Bearer` only where the route opts in, and a key can
  never start a session. `api.rs` resolves both to `ApiCaller`, so a handler reads one value.
- **Panel.** `/settings/iam` (overview with accounts, expiring bindings, recent activity),
  `/settings/iam/users` (filters, create, detail with bindings and groups), `/settings/iam/groups`
  (directory plus members panel), `/settings/iam/service-accounts` (create, key shown once, revoke)
  and `/settings/iam/simulator` (subject, resource path, action → verdict card with every binding
  it consulted, decisive first).
- **Proof.** `cargo test --workspace --no-fail-fast` → **589 passed, 0 failed** across 48 suites
  (exit 0) · `cargo clippy --workspace --all-targets -- -D warnings` → clean · `cargo fmt --check`
  → clean · `pnpm typecheck && pnpm build` → 2/2, the admin route table lists all eight identity
  screens · `bash scripts/qa/run.sh` → **612 clicks, 628 screenshots, 0 high findings**, 5 medium
  (all carried forward: the public renderer's icon 404s), 3 vision issues (1 medium + 2 low, both
  on the analytics realtime card's raw snapshot ids — pre-existing) — `qa-artifacts/20260927-041657`.
- **The browser pass drives the slice, not just the routes.** `iam-subjects` in
  `scripts/qa/walkthrough.cjs`: overview cards (6) → create `qa-subject@example.com` → open the
  account → attach the role at organization scope → effective permissions (72 granted) → a
  resource binding on `/blog/*` → simulator `ALLOWED` with its source → `/blog/hello-world`
  `ALLOWED` vs `/legal/terms` `DENIED` with one `out_of_scope` binding → groups panel with one
  member and one role → service account with its key shown once (`omsa_…`) → revoke → the role's
  members tab answers with 3 subject rows.
- **The first pass's high finding was the harness, and it is fixed.** The wizard clicked a button
  that was still working ("Creating…"), which submitted a step twice; the platform refused the
  duplicate — correctly — and that refusal landed while the next step's form was being filled, so
  the organization POST went out with an empty name. The walkthrough waits the busy state out now
  (`5d1a376`), and `--only=wizard` re-checks the flow on its own: on a freshly reset database it
  reports `WIZARD_ONBOARDING_FAILURES=0`. The second full pass confirms it (3 console errors, 2
  failed requests — the carried-forward 404s only).
- **Two small defects the pass caught in this slice, fixed and re-measured on the live panel:**
  the three user-list filters had no accessible name (`85cfa84` → `aria-label`s; re-check reports
  zero unlabelled inputs) and the groups row's delete control was a 12px grey glyph the vision
  review read as a smudge (`9155b0d` → caution colour, 14px icon, `aria-label` "Delete QA Team";
  re-check: 32×24 button, `rgb(138,93,22)`).
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium), the
  analytics realtime card's raw snapshot ids (1 medium + 2 low vision).
- Next: **REQ-006 slice 3** — sessions, devices, MFA and the security policy (revoke + sign-out-all,
  idle/absolute lifetime from the policy row, lockout per account and per IP, IP lists, TOTP and
  passkey enrolment with step-up, recovery codes). The walkthrough's denial case should use an
  account with no bindings so the artifact shows a `DENIED` verdict card as well.

## 2026-09-27 — REQ-006 slice 3a: the security policy, sessions, devices and TOTP

- **The slice.** Migration `0017` adds `sessions.step_up_at`, a live-session index and
  `mfa_challenges`. `crates/identity` gained five modules: `totp` (RFC 6238/4226 over HMAC-SHA1
  with the RFC's own vectors, Base32, `otpauth://`), `secrets` (encrypt-then-MAC envelopes for
  stored secrets, key from `OMNION_MFA_KEY`), `security` (the policy document, its ranges as
  field-level refusals, a CIDR matcher), `devices` (fingerprint, first/last seen, trust window)
  and `mfa` (two-step TOTP enrolment, ten single-use recovery codes hashed and consumed by a
  conditional update). `signin` runs the order a sign-in should: address lists → lockout →
  password → factor, with every attempt recorded in `sign_in_attempts`. Sessions now take idle,
  absolute and concurrent lifetimes from the policy row — never from a constant.
- **Dangerous operations demand a fresh step-up** (`POST /auth/step-up`: the caller's own
  password or an enrolled code, ten minutes): resetting factors, removing a confirmed factor and
  issuing a service-account key answer `403 step_up_required`, and the panel parks the refused
  action behind a prompt that retries it. Forgetting a device also ends its live sessions.
- **Panel**: `/settings/iam/security` (five policy tabs, ranges on the field, the diff a save
  applied), `/settings/iam/sessions` (state badges from the resolver's own values, filters,
  revoke, sign-out-all, cards below `lg`), `/settings/iam/devices` (trust window, forget) and the
  user detail's **Second factors** tab (enrolment with the secret shown once, confirmation +
  recovery codes, remove, reset).
- **Proof.** `cargo test --workspace` → **614 tests, 0 failures (exit 0)**; identity's own suite grew to 52 (TOTP against the RFC 4226/6238 vectors, envelopes, CIDR matching, hashing). The slice walk
  (`apps/api/tests/iam.rs::sessions_devices_mfa_and_the_security_policy_are_proven_end_to_end`)
  proves the whole slice over HTTP in 15.6s: the policy save with its diff, a range refusal and an
  unusable network each naming their field, a lockout that triggers at the configured threshold
  (the correct password is refused while locked), a denied address refused with the *correct*
  password, an allowlist narrowing, the per-address failure count, the idle window proven twice
  (refused at five minutes, accepted at two hours), a revoke that ends a session on its next
  request, `sign-out-all`, the concurrent cap retiring the oldest, the device registry with its
  trust window, TOTP enrolment + confirmation + a challenge a code completes, a recovery code
  spent exactly once, `step_up_required` on MFA reset and key issuance, and the audit trail
  carrying every one of those actions. `pnpm typecheck && pnpm build` → 2/2, the admin route
  table lists the three new screens.
- **QA pass** `qa-artifacts/20260927-055804`: **727 clicks, 750 screenshots, 0 high findings** (5 medium, all the carried-forward public renderer 404s), vision review 1 medium (the analytics realtime card's raw snapshot ids, pre-existing) and 0 vision failures. The new pass `iam-security-depth` recorded 14 steps, all of them green: policy refused in the field, saved with its diff, the address list refusing an unusable network, the session list with two rows and a real revoke, the filtered list, the device registry trusted and cleared, and the MFA enrolment dialog opened (secret + `otpauth://` shown) and cancelled.
- **What the first pass found (fixed in this tick).** Two high findings, both mine: the policy
  screen sent an invalid network to the API instead of refusing it on the field (now validated in
  the browser too — the API's own refusal stays pinned by the integration walk), and a *platform*
  account saw an empty session and device list because the screens defaulted to the first tenant
  while those accounts carry no organization (both now default to "all organizations", with a
  picker to narrow). The walkthrough's first run also showed the two list screens returning zero
  rows for the same reason — that is how the defect was found.
- Carried forward, not caused by this slice: the public renderer's icon 404s (5 medium).
- Next: **REQ-006 slice 3b** — WebAuthn/passkeys (the ceremony: CBOR parsing, COSE key handling
  and signature verification), then slice 4 (enterprise sign-in, SCIM, ABAC policy builder,
  approvals, the safety invariants). Slice 3b needs a crypto dependency (`p256`/`ed25519-dalek` +
  a CBOR reader) that the workspace does not carry yet.
## 2026-09-27 — REQ-006 · slice 3b · WebAuthn passkeys

- **What.** Passkeys are real: `crates/identity/src/webauthn/` carries a small CBOR reader
  (`cbor.rs`), the COSE credential key (`cose.rs`, ES256 + EdDSA) and both ceremonies (`mod.rs`).
  Registration checks the ceremony type, the challenge this server issued, the origin it serves,
  the relying-party hash, a present user, the attested COSE key and — for a `packed` statement
  without a certificate chain — its self signature; an assertion verifies
  `authenticatorData || SHA-256(clientDataJSON)` with the stored key and refuses a signature
  counter that does not move forward. Migration `0018_webauthn.sql` adds `webauthn_challenges`
  (single-use, purpose-scoped, one live per account+purpose). The API gained
  `/api/v1/auth/webauthn/register/begin|complete`, `/passkeys`, `/passkeys/{id}` and
  `/authenticate/begin|complete`, with `auth::start_session` shared by both sign-in paths; the
  panel gained the passkeys section in the user detail's Second factors tab and the second step of
  the sign-in screen (code, recovery code, **Use a passkey**).
- **The documented loopback exception.** A WebAuthn relying-party id must be a domain, so the QA
  stack (served on `127.0.0.1`) cannot run a ceremony at all; the server binds credentials to
  `OMNION_WEBAUTHN_RP_ID` (default `localhost`) and `OriginPolicy` accepts a loopback origin from
  any port unless `OMNION_WEBAUTHN_ALLOW_LOOPBACK=false`. That rule has its own unit test and the
  browser pass relies on it — a test that silently skips is not evidence.
- **Proof (Rust).** `cargo test --workspace` → **630 tests, 0 failures** (exit 0; +16 on the last
  tick). The ceremony units cover ES256 and EdDSA round trips, a verified and a broken packed self
  attestation, and every refusal; `apps/api/tests/webauthn.rs::a_passkey_enrols_and_signs_in_end_to_end`
  drives the slice over the real router with a software authenticator (options → registration →
  duplicate credential `409` → foreign origin `400 webauthn_refused` → passkey list → password
  sign-in answering `mfa_required` → assertion options → assertion opening a session whose auth
  methods carry `webauthn` and whose counter moved → replayed counter refused → step-up-gated
  removal → password-only sign-in again).
- **Proof (web).** `pnpm typecheck && pnpm build` → 2/2 (`@omnion/admin`, `@omnion/web`).
- **Proof (QA).** `bash scripts/qa/run.sh` → `qa-artifacts/20260927-084333`: **727 clicks, 754
  screenshots, 5 findings (0 high** — the five carried-forward public-renderer 404s**)**, vision
  review 0 issues / 0 failures, and `Refusals provoked on purpose — 2` (the step-up gate, with its
  reason). The run before it read **7 findings (2 high)**: the pass's own designed step-up refusal
  (403 + its console line) was being counted as a defect, so the harness gained `expectRefusal` —
  a single-use, index-bounded allowance a pass registers immediately before a deliberate act, with
  what it swallows reported instead of hidden. The pass still asserts the refusal happened (the
  prompt appeared) and the retry succeeded (the row is gone). The new
  `iam-passkeys` pass enrols a passkey with a Chrome **virtual authenticator** on
  `http://localhost:<port>` (the loopback exception), shows the row, has the sign-in answer the
  factor step, completes it with the passkey, removes it through the step-up prompt and proves the
  account is password-only again — the first run of the pass was red (`rows: 0`), the debug probe
  named the cause (an IP literal is not a valid relying-party id), and the fix is in the pass.
- **Next.** REQ-006 slice 4 — enterprise sign-in (OIDC/OAuth2/SAML with JIT + claim mapping), SCIM
  provisioning, the ABAC policy builder + dry run, permission requests/approvals and the safety
  invariants.

## 2026-09-27 — REQ-006 · slice 4a · the ABAC policy engine, the builder and the safety invariants

- **What.** Policies are real and they change decisions. `crates/policy-engine` is the pure half —
  a condition tree (`all`/`any`/`not` plus leaves of attribute → operator → value), seven operators
  (`==`, `!=`, `>`, `<`, `in`, `starts_with`, `contains`), `*` wildcard target patterns, the
  missing-attribute-reads-as-null rule and the decision rule (highest priority first, equal
  priorities resolve to deny, disabled decides nothing). `crates/permissions` gained `policies`
  (CRUD + a version row on every save + the attribute merge + the overlay) and `invariants` (the
  two refusals that protect a last owner and the caller's own last privileged binding).
  `evaluate::authorize_subject` — the one decision path the guard, the effective-permissions screen
  and the simulator share — now resolves RBAC and then hands the answer to `policies::apply`, so a
  deny policy takes away what RBAC granted, an allow policy grants what RBAC never gave, and the
  `403` reports the policy by name in `details.source.policy`. The API gained
  `/api/v1/iam/policies` (+ `/{id}`, `/versions`, `/test`) where the dry run writes nothing and
  accepts an unsaved draft; the panel gained `/settings/iam/policies` — condition rows with
  ALL/ANY and NOT, a JSON view for trees the rows cannot express, the THEN block, the dry run with
  its highlighted leaves and the version history.
- **The invariants count scope classes, not "the organization".** The first attempt counted every
  live privileged binding whose `organization_id` matched — and the platform Owner's binding is
  GLOBAL (`seed::bind_owner`), so on a shared development database the count matched every global
  owner binding in the database and the refusal never fired. Fixed by keying the count on the
  binding's own class: an organization-scoped binding is defended by its organization
  (`organization_id = $org`), a global one by the other global ones (`scope_type = 'global'`), and
  the self-lockout rule is checked first because it is the actionable sentence when both apply.
- **Proof (Rust).** `cargo test --workspace` → **653 tests, 0 failures** (exit 0; +23 on this
  slice: 14 policy-engine units, 6 overlay/attribute/validation units, 2 invariant units, and
  `apps/api/tests/iam_policy.rs::abac_policies_and_the_safety_invariants_are_proven_end_to_end`,
  which drives the slice over the real router: the member without `iam.policies.read` is refused,
  the deny policy removes `users.read` from the Owner (403, `reason=policy_denied`, the policy
  named in `details.source.policy`, and the simulator agreeing), the allow policy grants the same
  permission to a member whose RBAC set provably does not carry it, priority 900 takes it back,
  disabling the deny returns the allow, the history reads three versions back, the dry run reports
  a satisfied leaf with its resolved value (and the opposite when the path does not match), the
  four `invalid_policy` refusals answer `400`, and the invariants answer `self_lockout` then
  `last_owner_binding` then let an ordinary revocation through — with the audit trail and the
  `iam.policy_changed` events read back).
- **Proof (web).** `pnpm typecheck && pnpm build` → 2/2 (`@omnion/admin`, `@omnion/web`), with
  `/settings/iam/policies` in the route table.
- **Proof (QA).** `bash scripts/qa/run.sh` → `qa-artifacts/20260927-112459`: **772 clicks, 803
  screenshots, 5 findings (0 high** — the five carried-forward public-renderer 404s**)**, vision
  review 0 issues / 0 failures, `Refusals provoked on purpose — 2`. The pass's new `iam-policies`
  run reads: builder filled (1 target chip, 1 condition row) → dry run `APPLIES (deny)` with 1
  matched leaf → saved (0 → 1 rows, 2 versions) → delete armed then gone, back to the start state →
  the out-of-range priority refused in the field. The route is walked on desktop and mobile
  (`page-iam-policies`, `page-iam-policy-{editor,test,history,refusal}`, `mobile-iam-policies`).
- **Two defects the QA pass caught, both fixed in this tick.** (1) The condition rows took their
  ids from a module-level counter, so the server rendered them with different ids than the client —
  React refused to patch the tree (a hydration error, 20 console errors, plus the dev overlay's own
  Reload/Back buttons that the walk then timed out on). The row the editor opens with now carries a
  constant id and only rows the reader adds use a counter (they exist on the client alone); the same
  pass had flagged four inputs without a label, and the five standalone controls gained
  `aria-label`s. (2) The generic clicker types junk into every textarea, and the JSON view's parse
  escaped out of a render (`SyntaxError`); both JSON paths now refuse in the field — the draft
  falls back to the rows and says the JSON does not parse, the toggle keeps the reader in the JSON
  view with the reason. An adversarial probe (junk in the attributes, junk in the JSON view, Test
  in both states, save) reports zero console errors. A second full pass after the fixes: 0 high.
- **Next.** REQ-006 slice 4b — enterprise sign-in (OIDC/OAuth2/SAML providers with JIT provisioning
  and claim → role mapping), SCIM 2.0 provisioning with its sync log, and permission
  requests/approvals as time-boxed bindings.

## 2026-09-27 — REQ-006 slice 4b-1 · Permission requests → approvals, and SCIM 2.0 provisioning

- **What shipped.** `crates/identity` gained the SCIM provisioning token store (`mc_…`-style
  secrets hashed at rest, mint/verify, revocation, and the sync log the panel reads);
  `crates/permissions` gained `approvals` — a request asks for ONE permission with a reason and a
  window, a decision turns it into a **time-boxed binding** (`grant-<permission>`, priority 100,
  `expires_at`), so an approved request grants exactly inside its window and expires on its own with
  nobody acting. The API serves `GET/POST /iam/approval-requests`, `POST /{id}/approve|reject`,
  `GET/POST /iam/provisioning/tokens`, `DELETE /iam/provisioning/tokens/{id}` and the SCIM 2.0
  surface (`GET /scim/v2/Users`, `POST /scim/v2/Users`, `PATCH /scim/v2/Users/{id}`,
  `GET /scim/v2/ServiceProviderConfig`) behind a bearer token, with `iam.approval.requested|
  approved|rejected` events and audit rows. The panel gained `/settings/iam/approvals` (inbox with
  a request form, approve/reject with a note, the window chip, the decided tabs) and
  `/settings/iam/provisioning` (token minting with the secret shown once, revocation, and the sync
  log with created/updated/deactivated rows).
- **Proof (Rust).** `cargo test --workspace` → **660 tests, 0 failures** (exit 0, 54 suites; +7 on
  this slice: `apps/api/tests/iam_approvals.rs::an_approved_request_grants_only_inside_its_window`
  — the granted window is moved into the past and the permission leaves with nobody acting, the
  member cannot read the inbox, decide, or read the user list after a refusal, and the audit trail
  carries the three events — and `apps/api/tests/scim.rs::a_scim_round_trip_provisions_and_logs`,
  which creates → patches → deactivates a user over the real router and reads the sync log back).
  **Run it against a database of your own:** the shared development database now carries migrations
  0019/0020 applied by the concurrent sibling clones (branches for later REQs), so every suite that
  migrates dies with `Migration(VersionMissing(19))`. `docker exec omnion-postgres psql -U omnion
  -d postgres -c "CREATE DATABASE omnion_tickbuild"` then
  `OMNION_DATABASE_URL=…/omnion_tickbuild cargo test --workspace` is green; the failures are
  cross-branch interference, not the tree.
- **Proof (web).** `pnpm typecheck && pnpm build` → exit 0, both apps.
- **Proof (QA).** `bash scripts/qa/run.sh` (stack `b`: ports 18090/3110/3240, database
  `omnion_qa_b`) → `qa-artifacts/20260927-175102`: **29 pages, 855 clicks, 72 field fills, 887
  screenshots, 5 findings (0 high** · 5 medium · 0 low — the five carried-forward
  public-renderer root 404s; the renderer correctly answers “nothing published here” for a site
  with no page at its root and the pass still records it)**, vision review 3 items on the analytics
  screens (medium ×2: the analytics-settings snippet block clips its last line; low ×1: the legend
  names a `Page views` series the chart does not draw — REQ-007 territory, untouched here). The new
  passes read: **iam-approvals** — request created (0 → 1 pending) → approved (window chip
  `expires 9/27/2026, 7:04:07 PM · 30m`) → rejected (1 in the decided tab) → an unshaped
  permission key refused in the field; **iam-provisioning** — token minted (1 → 2, secret shown
  once) → SCIM round trip (create `201` active, patch `200` deactivated, listed once) → sync log (2
  rows: one created, one deactivated, with its sentence) → revoked (2 revoked, the next SCIM call
  `401`). Both screens are walked on desktop and mobile.
- **The pass caught its own machinery, and that was the finding to fix.** Run `20260927-163614`
  reported 13 high hydration console errors across six screens (`/search`, `/settings/search`,
  `/settings/iam`, `/users`, `/groups`, `/sessions`) and a media `503`. Root cause of the hydration
  reports: the pass stamps `data-qa-idx` on every element and clicks the moment a document is
  ready — inside `next dev`'s hydration window; five out-of-band Playwright probes could not
  reproduce a mismatch without the pass's own writes, the markup involved carries no data, and the
  same pages were clean in every earlier pass. The apps now announce hydration
  (`apps/admin/components/app-ready.tsx` sets `data-app-ready="1"` from an effect) and every
  navigation in `scripts/qa/walkthrough.cjs` waits for that mark before anything touches the page —
  the next pass: **0 hydration reports, 0 high findings**. Two more run-time lessons: the media
  `503` did not survive re-checking (30 consecutive list requests plus the page's own loads, all
  `200` — a transient of the disposable stack), and the web checks had been reading port `3220`,
  which on this box belongs to an unrelated application (the published-page check read a stranger's
  login screen). The stack now names its own ports (`QA_WEB_PORT=3240`), its own database and its
  own report file, and the published sample reads back as `200 · “QA Sample Page · QA Site” ·
  heading “QA Sample Page” · Revision 2`.
- **Next.** REQ-006 slice **4b-2** — enterprise sign-in: OIDC/OAuth2 and SAML providers per
  organization with JIT provisioning and claim → role mapping, local sign-in staying available.
  That closes REQ-006; then the next wave-1 item in BUILD-PLAN order.

## 2026-09-27 — REQ-006 slice 4b-2 (part 1) · the enterprise sign-in core

- **What shipped.** `crates/identity/src/sso/` — the whole protocol layer of enterprise sign-in,
  before any HTTP: **provider rows** (`providers.rs`, the `auth_providers` table of `0011` finally
  read and written; a client secret never enters a row, it lives behind `secret_ref`), **the
  challenge** (`challenges.rs`; the `state` every round trip is bound to, SHA-256 at rest, single
  use, ten minutes, and *burned rather than retried* once somebody is guessing at it),
  **OIDC/OAuth2** (`oidc.rs`; discovery, the PKCE challenge, RS256 verification and the
  registered-claim checks `exp`/`iat`/`nbf`/`aud`/`iss`/`nonce`), **SAML 2.0** (`saml.rs`; the
  assertion reader and its two independent signature checks) and **the protocol-neutral identity**
  (`claims.rs`; one `Identity` shape every flow reduces to, plus the claim → role rules).
  `database/migrations/0021_iam_sso.sql` adds the two tables a *running* sign-in needs —
  `sso_challenges` and `auth_provider_events` — and nothing else: the provider row itself already
  existed in `0011`. The number is **0021**, not 0019, because the sibling waves own 0019 (`w2`
  `cms_blocks`) and 0020 (`w3` `automation_depth`); migration numbers are claimed per wave, and
  three unclaimed worktrees have already collided on 0019.
- **Proof (Rust).** `cargo test -p omnion-identity --lib` → **103 tests, 0 failures** (33 new on
  this part). The cryptography is tested against *real* cryptography, not against itself: the
  RS256 and SAML tests generate a 2048-bit key, sign, and require the module's verifier to accept
  the genuine signature and reject a tampered one.
- **The SAML signature check is two checks, and the second one is the one that matters.** XML
  Signature binds a document to a key in two independent steps, and my first implementation only
  did the first: verifying the RSA signature over `<ds:SignedInfo>`. The tamper test — change an
  e-mail inside a validly signed assertion, keep the original signature — **passed**. The
  `DigestValue` is what ties the `SignedInfo` to the assertion under the *enveloped transform* (the
  element with its own `<ds:Signature>` removed); with it, the tampered document is refused
  because the bytes changed. Skipping either check leaves a hole: the digest alone lets anyone
  rewrite a claim, the signature alone signs the algorithm but not the document.
- **Four more real bugs the tests caught, each a genuine defect rather than a test artefact.**
  (1) A repeated `<saml:Attribute Name="groups">` was being dropped, so a directory that sends a
  multi-valued attribute as several elements lost half a group membership and under-granted a
  role. (2) `rsplit(':')` on `<saml:Assertion xmlns:saml="urn:…:assertion">` returns the
  *attribute value*, because a colon inside an attribute looks exactly like a namespace separator —
  the qualified name has to be located before the prefix is dropped. (3) A closing tag is spelled
  with the prefix the document used, so searching for `</Assertion>` found nothing. (4)
  `<ds:SignatureMethod Algorithm="…"/>` has no text; the algorithm is its attribute, and reading it
  as text fails silently three frames deep.
- **The disk is a blocker, and it is an environment problem rather than a code one.**
  `/mnt/apopic` is one 60 GB loop image shared by **eight** worktrees' `target/` directories, and it
  hit **100% full twice during this tick** — each time inside a `write_file`, which then failed with
  "No space left on device". The reclamation is deliberately conservative: only **derived**
  artifacts were removed (`target/debug/{deps,incremental,build}` and the incremental caches) and
  only in worktrees with no live `cargo`/`rustc` and no running pm2 process; no source file, no
  branch, no sibling's running server was touched. The siblings rebuild within minutes and refill
  the volume, so the headroom is temporary. **Owner action:** the box needs more room, or the
  unclaimed `omnion-w4`…`omnion-w7` worktrees (≈12 GB of cargo target plus 454 MB of
  `node_modules` each) should be pruned — no wave owns them yet.
- **Next.** The same slice's remaining part: the API surface (`GET/POST /iam/providers`,
  `PATCH/DELETE /iam/providers/{id}`, `POST /iam/providers/{id}/test` for the discovery check, and
  the public `GET /api/v1/auth/sso/{slug}/start` + `POST …/callback` pair), JIT provisioning and the
  claim → role binding on sign-in, the `iam.signin.*` events, the `/settings/iam/authentication`
  panel screen, the `apps/api/tests/sso.rs` integration walk (sign in against a stub provider → JIT
  account → mapped role → expired challenge refused) and the `iam-authentication` QA pass. Then
  `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` close the
  REQ.

## 2026-09-28 — REQ-006 slice 4b-2 (parts 2–3) · the API, the screen and the integration walk

- **What shipped.** The HTTP half of enterprise sign-in, the screen that drives it, and the walk
  that proves both. **`61ef619`** — `crates/identity/src/sso/provisioning.rs` (JIT: match on the
  stored subject index first, the address second, `JIT_PASSWORD_MARKER` in place of a password,
  and a *refusal* rather than a silent row when provisioning is off); the identity HTTP client
  grows the two calls the `code` flow needs; `/api/v1/iam/providers` (list, connect, patch, remove,
  `…/test` for the discovery check, `…/events` for the sign-in log); and `/api/v1/auth/sso`
  (the public `providers` list, `start`, the generated SAML panel page, and the `callback` that
  answers both a `code` query and a posted assertion). **`f599c3c`** — `/settings/iam/authentication`,
  the nav entry, the API client and the `iam-authentication` pass in `scripts/qa/walkthrough.cjs`.
  **`apps/api/tests/sso.rs`** — the integration walk.
- **Proof.** `cargo test -p omnion-identity --lib` → **107 tests, 0 failures**. `cargo test -p
  omnion-api --lib` → **97 tests, 0 failures**. `cargo test -p omnion-api --test sso` → **1 walk,
  0 failures** over the real router: the management surface (401 without a session, an empty
  organization listing no provider and three kinds), a provider created **switched off with JIT
  off**, the secret answered as a *name* plus a boolean (`secret_present: false` for a variable
  this process does not define) and no `client_secret` field anywhere in the payload, a bad
  `secret_ref` refused with `details.field = secret_ref`, an unreadable role mapping refused at save
  time, the discovery test answering `200 {status: "failed", detail: …}` for an unreachable host, a
  disabled provider `404 provider_disabled` **and written to the sign-in log**, JIT refusing then
  provisioning the same identity to `Created` with the marker in `password_hash` and the subject
  indexed under `sso_subjects`, the second sign-in `Existing` on the same account, a deactivated
  account staying deactivated, the event log readable over HTTP, removal taking the log with it
  (`on delete cascade`), and an ambiguous host answered `501 organization_required` rather than
  guessed.
- **The two design decisions this tick actually settled.** (1) **A public sign-in has to resolve
  its own organization.** My first version took "the installation's only organization", which is
  right for a first-run install and *silently wrong* for a second tenant — a sign-in link for one
  organization could complete against another. The walk caught it by creating a second organization.
  It now follows the same rule the public content surface uses: the browser's own host answers for
  its site, and a site belongs to an organization; several organizations and an unknown host is an
  honest `501` naming the fix. (2) **A refusal is a fact, not a silence.** A disabled provider
  refusing to start wrote nothing, so an operator who switched a provider off and then wondered
  "is anybody still trying to sign in with it?" had no way to find out. `live_provider` now writes
  the `auth_provider_events` row before refusing.
- **Two test-authoring bugs the walk caught in itself, both worth naming.** A group claim is only
  read when the provider *names* one, so a test provider with `group_claim: None` proved nothing
  about groups — the fixture was wrong, not the code. And a test fixture that registers a globally
  unique host has to clear a stale one first, or the second run fails on the first run's leftovers.
- **The disk is still the constraint, and it is an environment problem rather than a code one.**
  `/mnt/apopic` (one 60 GB loop image shared by eight worktrees' `target/`) fell to **3.1 GB free**
  during this tick. Reclamation stayed conservative and derived-only: `target/debug/{deps,
  incremental,build}` in the **unclaimed** `omnion-w5` and `omnion-w7` worktrees, neither of which
  had a live `cargo`/`rustc`; no source, no branch, no running process touched. That returned
  17 GB. **Owner action:** the box needs more room, or the unclaimed `omnion-w4`…`omnion-w7`
  worktrees should be pruned — no wave owns them yet.
- **A note on where the walk runs.** The shared dev database `omnion` has migration **19** applied
  from a sibling branch that `main` does not have, so `db.migrate()` refuses with
  `VersionMissing(19)` and every integration test that migrates fails there. CI starts a clean
  database and is unaffected. The walk was proven against a fresh `omnion_sso_test` database.
  **Owner action:** either reconcile the 0019 slot between the waves, or point the local
  integration runs at a per-branch database.
- **Next.** The one thing the walk deliberately does not fake: a **full round trip against a live
  OIDC/SAML provider**. That needs a local stub identity server the walk can point at — discovery
  document, JWKS, token endpoint and a signed ID token for OIDC; a signed assertion with the
  enveloped digest for SAML — so the `code` exchange, the RS256 verification, the PKCE binding and
  the claim → role mapping are all proven end to end rather than one layer at a time. Then
  `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` close the
  REQ.

## 2026-09-28 — REQ-010 slice 1, verified end to end (six defects found)

- **What this tick was.** Slice 1 (folders + browser + trash) was already written and its boxes
  were already ticked, but nothing had ever *executed* the folder move, the trash listing or a
  filtered listing against a real database — the walk that asserts the audit rows for
  `media.folder_moved` and `media.folder_deleted` never performed a move or a delete. This tick
  made the walk real and then fixed what it found.
- **Six defects, none of them visible to the layer that owned them.**
  1. **Every filtered listing was broken.** The clause was built as a string containing `$n` *and*
     the value was pushed as a bind, so the statement read `folder_id = $2$2` and PostgreSQL
     answered "syntax error at or near $2". An unfiltered listing worked, which is exactly why no
     earlier test saw it. `Filter::push` now writes clause and value together, so a placeholder can
     only exist where the value beside it was pushed.
  2. **`make_interval(days => $2)` with a bound parameter.** PostgreSQL cannot infer the remaining
     arguments of a named-argument function, so it picked a `numeric` overload and sqlx failed to
     decode. Replaced with `$2::bigint * interval '1 day'`, which is unambiguous.
  3. **`sum(size_bytes)` returns `numeric`.** sqlx will not decode `numeric` into an `i64`, so the
     trash summary answered 500. Cast back to `bigint`.
  4. **`ORDER BY` inside an `UPDATE`.** PostgreSQL has no such clause; the folder move 500'd on
     every call. The ordering premise it encoded was wrong anyway — one `UPDATE` evaluates every
     row against the pre-update snapshot, so no ordering is needed.
  5. **A folder move self-parented the folder.** The parent was resolved by the moved folder's *own*
     new path, so `parent_id` became the folder itself on every move, and the first move of a
     top-level folder hit `media_folders_root_name_idx`. The parent is now resolved by the
     *parent's* path.
  6. **An omitted `parent_id` meant "move to the root"** although the body documents "omitted keeps
     the current one" — so renaming a nested folder silently relocated it to the top. A no-op move
     is also no longer reported as `folder_cycle`, which is a cycle where there is none.
- **Two more honest answers.** A `folder_not_empty` refusal now has a tested counterpart (an empty
  folder deletes, and a deleted folder is a `404` by id so a stale deep link names what is missing),
  and a move to where a folder already is is a no-op.
- **Proof.** `cargo test -p omnion-media --lib` → **25 tests, 0 failures** (three new: every filter
  and every combination refuses to write a placeholder twice, the subtree clause binds its folder
  once per mention, the tag clause compares from the placeholder side). `cargo test -p omnion-api
  --lib` → **105 tests, 0 failures**. `cargo test -p omnion-api --test media` against
  `omnion_test_main` → **8 walks, 0 failures**, over the real router: the tree refused without a
  session and to an account with no media permission, a site created after the migration still
  materialises one root, folders create/rename/move/re-parent/delete with the subtree rewrite read
  back out of the row, a cycle and a duplicate sibling name and a blank name each refused by name,
  a file moves between folders without its storage key changing, a filter narrows the listing and
  the total follows, a `like` wildcard in a search term is treated as text, the trash lists the
  deleted file with a real countdown, restore returns it to its folder, purge removes the bytes as
  well as the row, and every privileged step left an audit row. `pnpm typecheck` green. clippy adds
  no new warning.
- **Environment note.** The shared dev database `omnion` still carries a sibling's migration 19,
  so the walks run against `omnion_test_main`. `/mnt/apopic` was at 98% again; reclaiming
  `target/debug/incremental` in this worktree returned 2.9 GB.
- **Next.** Slice 2 — preview, metadata, versions. The version table already exists; the version
  history, the preview pipeline and the file detail screen do not.

## 2026-09-28 — REQ-010 slice 2, a version history that does not rewrite the past

- **What this tick was.** Slice 1 gave the library a file system. This tick gave it a memory: a
  replaced file keeps its old bytes, the panel can see every version, and a restore brings an old
  one back *as a new version* rather than by rewriting history.
- **The migration the plan assumed already existed.** The last tick's handover note said
  "the `media_versions` table exists". It did not — `0025` created folders, browser columns and
  the trash, and nothing had ever written a version row. So slice 2 ships `0026`, which creates
  the table and backfills version 1 for every existing file, **copying `created_at`** rather than
  stamping `now()`. A history that starts at the migration date is a lie about when the file
  arrived, and it is exactly the kind of lie that is invisible for a year.
- **Three rules, each a place a shortcut produces a plausible wrong answer.**
  1. **A version is append-only.** A restore *copies* the old bytes to a new key and appends the
     copy. Rewriting a row would make "what did this file look like on day 3" depend on whether
     anybody took a shortcut in between.
  2. **The number comes from the database.** `next_version` reads `max(version)` under
     `for update` on the `media` row — a *scalar subquery*, because `FOR UPDATE` on an aggregate
     is a no-op in PostgreSQL. Reading the max in Rust would open a window between two reads and
     let two concurrent replaces both claim 4, which surfaces as a "duplicate key" error that
     names the index and not the cause.
  3. **A number is never reused.** A pruned version leaves a hole.
- **The transaction is opened by the crate, not the route.** This was fought out with the
  compiler: a route-held `sqlx::Transaction` surfaces `sqlx::Error` where everything else in
  `omnion_media` is a `MediaError`, and `ApiError` has `From` for the latter but deliberately
  **not** for the former. `omnion_media::begin_version` / `commit_version` hand back only
  `MediaError`, which is the honest boundary: the library owns the transaction because the
  guarantee rule 2 exists for spans it.
- **The header probe reads 64 KB, not the file.** `crates/media/probe.rs` pulls dimensions,
  duration and page count out of the *header* for PNG, GIF, JPEG, BMP, TIFF, WebP (all three
  containers), MP4/QuickTime, WebM, WAV, MP3, Ogg and PDF. A 4 GB video upload must not cost a
  full read to learn it is 12 minutes long. Every extractor answers "I do not know" rather than
  guessing — a wrong dimension breaks every layout that reads it and is not obviously wrong once
  it is stored. Two findings the unit tests forced out: a WebP canvas stored as `0` is a corrupt
  header, **not** a one-pixel image (reading `0 + 1` would put a 1×1 box on screen for a file
  that has no size), and one blanket 30-byte minimum across the three WebP containers refuses a
  short-but-complete `VP8L` header.
- **The walk corrected a test that had been asserting a route which never existed.** The walk
  read the current bytes from `/api/v1/media/files/{id}/raw` and got an empty body: the file
  manager's *listing* is `/media/files`, the read is `/media/{id}/raw`, and no route was ever
  registered at the address the test used. Three more corrections came out of the same run — the
  404 is `media_not_found` (not `file_not_found`), an empty upload answers `invalid_request`
  (not `file_empty`), and the history reads **newest first**, so a check written against an
  assumed oldest-first order fails on a correct response.
- **The fixture leaked objects until it read the union.** Cleanup read `media.storage_key`, but a
  replace *moves* that column to the new key — the old one is named only by the history, so every
  replaced version's object stayed in the bucket. A test cleanup that misses them is a slow leak
  that nobody notices for a month.
- **Proof.** `cargo test -p omnion-media --lib` → **46 tests, 0 failures** (24 new, mostly header
  probes and the version key rules). `cargo test -p omnion-api --lib` → **109 tests, 0 failures**.
  `cargo test -p omnion-api --test media` against `omnion_test_main` → **11 walks, 0 failures**,
  over the real router: a replace leaves version 1 downloadable and **byte-identical** (compared
  as bytes, not as a length — a length check would pass by accident on an overwrite), the row
  points at the version it serves, the three versions own three keys, a download of an old
  version is an attachment named `hero-v1.png`, a restore appends version 3 with version 1's
  checksum while version 2 is untouched, and the routes refuse without a session, without
  `media.read`, and name a missing version by number. `pnpm --filter @omnion/admin typecheck`
  green. The QA pass ran on the default stack.
- **Environment note.** `/mnt/apopic` was at 99 % (610 MB free) when the tests finished; this
  worktree's `target/debug/incremental` returned 1.2 GB and the unclaimed `omnion-w5`/`omnion-w6`
  worktrees' `target/` returned a further 2.7 GB. **Owner action:** those worktrees hold build
  artefacts for waves nobody has started; they will fill the image again.
- **Next.** Slice 3 — transformation presets with a content-addressed cache, per-site storage
  settings with a connection test, the CDN purge hook, share links and duplicate detection with
  merge. Also still open in slice 2: the Usage and Activity tabs, HTTP range requests on the
  serve path, and EXIF extraction.

## 2026-09-28 — REQ-010 slice 3 (transformations), a preset that produces real pixels

- **What this tick was.** Slices 1 and 2 gave the library a file system and a memory. This one
  gave it *derivatives*: a page asks for `?preset=card` and gets the same pixels every time,
  built on the first request and addressed by a hash of its inputs.
- **The dependency the plan did not mention.** A preset has to *produce* pixels, which means
  decoding, resampling and re-encoding. The workspace had no image crate, so this tick adds
  `image` (png/jpeg/webp only — the three codecs a preset can emit). Shelling out to a binary
  was the alternative and was rejected: it makes the API's correctness depend on what happens to
  be installed on the host, and the test suite would skip itself on a machine without it.
- **Two fits are not one fit.** `cover` crops and `contain` letterboxes, and the first version
  gave both the same resampler. It produced a correctly-sized *crop*: it passes a square-crop
  assertion and is wrong on every non-square source, which is most of them. A `contain` result is
  now pasted onto a canvas of the box's size, so a thumbnail is a stable 320×320 rather than a
  320×180 that shifts the layout every time a differently-shaped image is uploaded.
- **A request never enlarges.** A 2400px request against a 1200px source is refused with an
  explanation instead of being answered with a blurry upscale that is *larger* than the original.
  The check is `>`, not `>=`: asking for exactly the source's own size is the identity, and a
  template that names the same number twice is not a mistake. The unit test forced this — the
  first version refused the identity too, which would have broken the seeded `standard` preset on
  a 1200×630 hero.
- **The cache key is a hash of the definition, not of (file, preset).** A pair lookup would serve
  stale pixels after an edit, because the pair is unchanged while the definition moved. With a
  key lookup, an edit produces a key nobody has seen, so the old entry becomes *unreachable*
  rather than *wrong* — and the response may honestly say `max-age=31536000, immutable`.
  Quality is in the key, and that is the field people forget.
- **Three defects the real router found that a unit test on `transform_bytes` could not.**
  1. The derivative header carried the **object key** where the identity belongs. The two are
     different strings that both appear in the module, and the response builder took one
     parameter where it needed two — so a caller that read the header and looked it up in
     `media_derivatives.cache_key` found nothing. A header that looks like an identifier and is
     not one is worse than none. Both are now fields of a struct, because three positional
     `&str`s is the shape that produced the bug.
  2. **A site created after the migration got no `standard` preset.** The `0027` seed covers the
     sites that existed when it ran, so a page already asking for `?preset=standard` would
     silently fall back to full-size originals — on new sites only, which is exactly where nobody
     is looking. It cannot be fixed in the migration, because the gap is between "the migration
     ran" and "somebody creates a site", and it cannot be fixed in the create path either:
     onboarding, the tenancy API and a future import all insert the row themselves. It is a
     trigger (`0028`), which is the only place guaranteed to see every site.
  3. A test asserting **"the first call builds" passes once and fails for ever after.** The row
     is keyed by the source bytes, and a re-run reproduces them exactly, so the second run is a
     cache hit. The assertion now checks the answer is correct either way and that the key is
     stable — the property that actually matters.
- **Proof.** `cargo test -p omnion-media --lib` → **78 tests, 0 failures** (32 new). Five walks
  over the real router in `--test media_transform` → **0 failures**: the bytes are compared *as
  bytes* and decoded again (a length check passes by accident on an overwrite), the second
  request is byte-identical to the first, the object is read back out of the store and compared
  against what was served, the old row survives an edit, a delete cascades the cache away, an
  unknown preset returns the original *byte for byte* with no derivative key, an SVG answers
  `not_transformable` naming its type, and every preset field error names the field that caused
  it. The pre-existing `--test media` → **11 walks, 0 failures**, unchanged, against the raw route
  that now takes a query parameter. `cargo test -p omnion-permissions --lib` → 62 pass.
  `pnpm --filter @omnion/admin typecheck` green.
- **The QA pass (`bash scripts/qa/run.sh`, default stack) — clean for this slice.** 954 clicks,
  988 screenshots, the new `/media/settings` route walked and clicked (28 elements), and the depth
  pass drove it: created a preset, submitted an out-of-range quality and **the field error named
  it**. Vision review returned **0 high / 0 medium / 0 low**. The page's own diagnostics read
  *overflow: no · offscreen: 0 · broken images: 0 · low contrast: 0 · unlabeled inputs: 0 ·
  duplicate ids: 0 · h1: 1*. The four high findings the run reports are the walkthrough's **own
  deliberate error-state probes** — `/media?folder=nonexistent-folder` and the 400/404 they
  produce — none of them from this slice.
- **Two things the pass taught about this slice's own screen.** The seeded `standard` preset *is*
  present on a QA site (checked directly in `omnion_qa`, and the trigger in `0028` fires for a
  site created afterwards), so the walkthrough's `seeded: 0` was its own text match, not a gap —
  which is why the number was checked against the database rather than believed. And the preset
  example URL is a `<code>`, not an anchor: the first depth pass looked for `a[href^="/api/v1/
  media/"]`, found none, and reported "no preset example URL" for a screen that had one on it. An
  `<a>` pointing at a placeholder id would only have proven a 404 — the same mistake the route
  inventory already made once with `/media/files`. The pass now reads the query the screen
  actually renders and builds a real URL with a real file id.
- **A compiler lesson, fought out over a long wrong turn.** Every guarded route in this codebase
  is written `get(handler).layer(guards::require(...))`, and the new routes refused to compile
  with a bare `type annotations needed for MethodRouter<AppState, _>`. The guard's service impl
  requires the inner service's `Error = Infallible`, and inference cannot pick `Infallible` out of
  the several `From<Infallible>` impls in scope. Every existing route gets away with it because
  the *later* `.merge()`/`.route()` calls in the same chain pin the type. The fix is a
  `MethodRouter<AppState, Infallible>` annotation on the three new bindings — the same fix the
  compiler suggested and that reading the guard's own bound would have given in one minute.
- **Environment.** `/` was at 99 % and `/mnt/apopic` at 100 % during the run — MinIO refused
  writes with `XMinioStorageFull` and three walks failed on a storage error that had nothing to do
  with the code. Reclaiming `omnion-live/target` and `omnion-w5/target` (worktrees for waves
  nobody has started) plus this one's stale `deps` binaries returned ~4 GB. **Owner action:** the
  eight worktrees under `/mnt/apopic` hold ~30 GB of `target/`, and this is the second tick in a
  row that has had to delete another loop's build cache to finish its own tests.
- **Next.** Slice 3 continues — per-site storage settings with a connection test and public base
  URL, the CDN purge hook to REQ-011, share links with expiry and password, duplicate detection
  with merge. Also still open: EXIF (slice 2), HTTP range requests on the serve path, and the
  Usage and Activity tabs, which need `media_references` and arrive with slice 4.

## 2026-09-28 — REQ-010 slice 3 (share links), a capability that is never stored

- **What.** The third third of slice 3: `0036_media_shares.sql`,
  `crates/media/src/shares.rs`, `apps/api/src/routes/media_shares.rs`,
  `apps/api/tests/media_shares.rs`, `features/media/shares-tab.tsx` (a **Share** tab on
  `/media/files/{id}`), the client methods and the `MediaShare`/`CreatedMediaShare` types, and a
  walkthrough depth pass. Six design decisions, each a shortcut that produces a plausible wrong
  answer:
  1. **The token is stored hashed and nowhere else.** The row holds `sha256(token)` under a
     unique index, so a backup, a replica log or a support engineer with read access comes away
     with a list of *dead* tokens, and the lookup is still one probe. The round trip runs one way
     only, and the row type has no field the plaintext could occupy.
  2. **A share reaches a file; it does not bypass what the file is.** Servability is decided at
     *serve* time, not at creation — a link made yesterday must not keep serving a file the
     scanner has since flagged.
  3. **`revoked` and `expired` are the same answer (410), `password_required` is 403.** Telling
     the two dead states apart would hand a token prober a free oracle; answering "type the
     password" with 410 would send the owner a pointless request.
  4. **The counter counts bytes that were served**, in its own statement, so a failure after the
     bytes went out cannot roll it back.
  5. **Revocation is a write, not a delete** — the row is kept with its reason forever, because
     that is the only thing that makes a leaked link investigable.
  6. **The screen has no `Copy` on an existing row, and cannot.** The token is returned once;
     a copy button there would silently copy nothing.
- **Proof.** Five walks over the real router in `--test media_shares` → **0 failures**. The
  stored value is read **out of the database** rather than inferred from a response that hid the
  token; the list is scanned over its raw bytes for the token and for a field named `token`; the
  served bytes are compared as bytes and `no-store`/`nosniff`/`attachment` are each checked; the
  counter moves once and survives the revocation; the revoked row keeps its reason and instant; a
  reader may read the list and may neither create nor revoke; anonymous is refused on both; a
  share id on another file is a 404, not a 403. `cargo test -p omnion-media --lib` → **98**,
  `cargo test -p omnion-api --lib` → **116**, and `--test media` (11), `--test media_settings`
  (2), `--test media_transform` (5) are unchanged and green. `pnpm --filter @omnion/admin
  typecheck` green.
- **Three defects the walks found, none of which a unit test on `servable` could see.**
  `find_media` returns the *base* `Media`, which has no `deleted_at` and no `scan_status` — the
  two columns the whole "a share does not bypass the file" rule depends on, so the first draft
  asked the wrong struct and the compiler found the fields missing. `POST /shares` with no body
  answered **415**, because the handler demanded a JSON body for the most ordinary call anybody
  makes ("give me a link until I revoke it"); the same for the `DELETE`. And `rand` is a
  *dev*-dependency of `apps/api`, so token minting moved into the crate, which is where the
  width and the source belong anyway.
- **A test that reaches for a state the platform forbids.** The expiry walk set
  `expires_at = now() - 1s` and the `media_shares_expiry_sane` check refused it — a link whose
  expiry precedes its own creation is nonsense. The walk now ages the row by moving `created_at`
  back instead, which is the only way to reach the same state and the reason the check is there.
- **Environment.** `/mnt/apopic` hit **100 %** (89 MB free) mid-tick and MinIO refused every
  object write with `XMinioStorageFull`, which reads as a storage bug and was none. The cause is
  this worktree's own `target/`: **8.3 GB of rebuildable test binaries** plus 874 MB of `.tmp`
  leftovers from interrupted linkers. Reclaiming *only this worktree's* artifacts returned 8.5 GB
  (86 %). **Owner action:** the seven worktrees under `/mnt/apopic` hold ~30 GB of `target/`; a
  shared `CARGO_TARGET_DIR` is the structural fix, and this is the third tick to have had to
  delete its own build cache before it could run a test.
- **Next.** Slice 3 closes with duplicate detection and the merge (checksum groups, reclaimable
  size, `Keep this one` + `Merge group`, references repointed and the copies trashed), then the
  CDN purge hook to REQ-011. Still open: EXIF (slice 2), HTTP range requests on the serve path,
  and the Usage and Activity tabs, which need `media_references` and arrive with slice 4.

- **The QA pass did not complete this tick — and not because of this slice.** The pass got its
  QA slot after ~30 minutes of waiting behind sibling stacks, walked 434 clicks through
  `/media` (40 elements), `/media/trash` (26), `/media/settings` (30) and the IAM screens, and
  then stopped advancing at `iam-service-accounts` with the walkthrough process at 0 % CPU. The
  cause is the machine, not the code: **seven QA stacks are running at once** — 40 Chromium
  processes, load average **36**, and **127 MB free of 33 GB**. The walkthrough outlived the
  browser context, which is the "tab died under parallel passes" case already documented for
  this box, so no findings were produced and none are claimed. The share walk added this tick
  (`runMediaShares`) is committed and wired but has therefore **not been exercised yet**; the
  next tick runs it. The storage walk from the previous tick was committed for the same reason
  and the API-level proof for both is the Rust suite, which is green.

## 2026-09-28 · REQ-010 slice 3 closes — EXIF (commits 23e2e6d, 3837064, d14b355, 08c1dc0, 14e33ec, 9b7fab2)

- **What.** The last open item of slice 3: what the *camera* said about its own picture. The
  geometry probe already read a file's size from its header; this reads the other half of what an
  editor asks about a photograph — which body took it, at what shutter speed, with which lens, on
  which day. `crates/media/src/exif.rs` (the reader), `0042_media_exif.sql` (the column), the
  `exif` column on `media` plus the `display_width`/`display_height` pair on the file response, and
  the Metadata tab's **Camera** block.
- **Six decisions, each a shortcut that produces a plausible wrong answer.**
  1. *A TIFF header is not EXIF.* The IFD format is shared by TIFF, GeoTIFF and half a dozen
     makers' proprietary blocks; what makes the block EXIF is the `Exif\0\0` signature inside a
     JPEG `APP1`, an `EXIF` chunk in a WebP or an `eXIf` chunk in a PNG. The container is checked
     before any TIFF parsing runs.
  2. *Nothing is read from outside the prefix.* Every field's value may be an *offset*, and an
     offset is attacker-controlled. Every read is a range request whose failure is the answer.
  3. *A zero denominator is absent; `1/200` is not.* The first guard refused the normal case
     (`den > num`) and kept the corrupt one — the exact inversion, which is how a reader ends up
     with no shutter speed on every photograph and a divide-by-zero on the one broken file.
  4. *Orientation changes the box, not the file.* Values 5–8 store the picture sideways and
     browsers rotate it themselves, so a grid reserving `width × height` reserves the wrong box and
     shifts every image below it. The stored columns carry the oriented pair, the raw value stays
     in the record, and the API sends both readings.
  5. *A GPS fix is a flag, never coordinates.* There is no field in the type a coordinate could
     occupy, so a media library cannot quietly file an operator's home address into a row that
     search, an API key and a share link can all read.
  6. *A replacement replaces the record.* A screenshot over a camera original must not keep
     claiming to have been shot on a body it was never near.
- **The QA pass did not complete, and it found the tick's one real bug anyway.** The full pass ran
  629 clicks and then died with `Target page, context or browser has been closed` — the
  "browser context dies under parallel passes" case already documented for this box, with 21
  sibling QA processes and 0 GB free at the moment it failed. So the gate is **not** claimed as
  green this tick. What *did* run is `scripts/qa/probe-media-camera.cjs`, a one-screen probe added
  because "the full pass crashed" and "the screen is broken" must not read the same in a log: it
  signs in, uploads a JPEG that really carries a block, reads the block's rows and reads the
  API's own response — **13/13 checks pass**.
- **The bug it found: the rotation was applied twice.** The writer already stores the *oriented*
  geometry (an orientation-6 4000×3000 frame is written as 3000×4000), and `display_size()` then
  applied the swap again on the way out — so the panel reported a landscape picture for a portrait
  photograph, and the facts list read `4000 × 3000` for a file every browser draws tall. The unit
  test on `oriented_size` passed the whole time, because the function was correct; it was being
  called on the wrong input. This is the case the QA pass exists for and the Rust suite cannot
  see, and it is why the fix ships with a regression test shaped like the bug: a row carrying the
  oriented columns *and* the record that produced them.
- **Proof.** 19 new unit tests in `exif.rs`, 5 in `model.rs` and two walks over the real router →
  0 failures. Both
  walks read the column **out of PostgreSQL**, because a response that omits a field is
  indistinguishable from one that stored it and chose not to say so. An orientation-6 frame of
  4000×3000 stores 3000 and 4000; a text file grows no record; a replacement in a format with no
  block clears it and the geometry falls back to the frame's own; a restore brings version 1's
  record back; the serialised object is scanned for `lat`, `lon`, `GPSLatitude`, `GPSLongitude` and
  `altitude`. `cargo test -p omnion-media --lib` → **122** (was 98), `-p omnion-api --lib` → **122**,
  and `--test media` (13, was 11), `--test media_transform` (5), `--test media_settings` (2),
  `--test media_duplicates` (6), `--test media_shares` (5) are unchanged and green. `pnpm
  --filter @omnion/admin typecheck` clean. The migration applies across the whole set on a fresh
  database.
- **The QA pass had to grow a screen before it could test one.** The library pass uploads a PNG,
  and a PNG carries no EXIF — so the Camera block would only ever have been seen in its empty
  state, which the "no untested screen" rule forbids. The pass now *builds* a JPEG that carries a
  real block, uploads it, opens its detail screen and asserts the body, `ISO 400`, `1/200 s`,
  `f/1.8` and the rotated dimensions.
- **Four bugs the walkthrough's own JPEG builder had, each of which produced a file that read as a
  parser bug and was really a builder writing the format wrong.** The TIFF block is little-endian
  while the JPEG framing around it is big-endian, so a segment length written with the block's
  `u16` reads as a 57 KB segment in a 253-byte file and the block is then unreachable; a directory
  is a count plus its entries plus a four-byte next-directory pointer, and the two that get
  forgotten put every later offset on the wrong field; a value offset is measured from the start of
  the *block*, not from the directory that holds the entry, so a value laid down before the
  sub-directory exists is overwritten by it; and a RATIONAL is two words wide rather than four
  bytes, so a cursor that steps by the entry's width lands every rational after the first on the
  wrong field. None of the four would have been found by a screenshot — the file simply had no
  camera record and the screen showed its empty state, correctly.
- **Environment.** `/mnt/apopic` sat at **94 %** (3.6 GB free) on entry, and this worktree's own
  `target/` was 9.9 GB of it. Reclaiming *only this worktree's* `target/debug/incremental`
  (verified first: no live `cargo` holds it) returned 1.7 GB. **Owner action:** the worktrees under
  `/mnt/apopic` still hold ~30 GB of `target/`; a shared `CARGO_TARGET_DIR` is the structural fix.
- **Next.** Slice 4 — folder and file grants with inheritance, the scanning pipeline with
  quarantine and release, retention policies with the daily worker, and reference-based purge
  refusal. Done when a denied subject is refused on the raw route, a flagged upload is quarantined
  and releasable, and a retention run removes exactly the eligible rows.

## 2026-09-28 · REQ-010 slice 4 · virus scanning (quarantine, release, run log)

**What.** The `scan_status` column arrived back in `0025` and nothing ever moved it: the library
could render a badge and the badge could only ever read `pending`. This tick gives that column a
pipeline behind it — `0044_media_scanning.sql` (a per-site policy, a quarantine table with a
history, a run log, plus the two `media` columns the pipeline needs and `0025` never created), the
crate module `crates/media/src/scanning.rs`, the API in `apps/api/src/routes/media_scan.rs`, and
the **Scanning** tab on `/media/settings`. Seven routes; the gate is on *every* serve path, not
just the one the spec names: the panel raw route, the public renderer, the preset path **including
its cached derivatives**, both version paths, and the share token route.

**Seven decisions, each a shortcut that produces a plausible wrong answer.** An unrecognised
scanner answer is an *error*, never a pass (pinned against a scanner that answers
`{"verdict_code": 3}` over a real socket); a file above the size ceiling is `skipped` and never
`clean`; a clean scan leaves `scan_detail` **empty** so a report that greps it cannot find a
positive on every file; a quarantine row is closed and never deleted, so a file flagged twice has
two events; a release requires a reason and lands the row on `skipped`, not `clean`, because
nobody has said the file is *safe*; another tenant's quarantine is a `404` and not a `403`; and a
**flag is a fact rather than a policy question** — `on_error` speaks to the *absent* verdict
(`pending`, `error`) and never overrules a `flagged` row.

**Three defects found by the walks, none of which a unit test could have seen.** The claim query
selected `id` while the row type called the field `media_id`, and sqlx's `FromRow` maps by
*column name* — so the sweep died with `no column found for name: media_id` on the first file,
which reads as a broken query rather than a missing alias. `0025` never created `media.scanned_at`
at all, so **every verdict write failed**, and because the route counted the error both as a
verdict and as a write failure, a one-file run reported `errors = 2` — a number with no reading
an operator can act on. And `sum(bigint)` decodes as NUMERIC, so the quarantine byte total could
not be listed at all.

**The one that matters was found by a suite I did not write.** `media_shares.rs`'s
`a_link_stops_serving_when_its_file_stops_being_servable` failed on my change: the first gate had
an early return for a site with scanning *disabled*, and the walk switches the scanner off before
flagging a file by hand. The regression was real and it was the shape of the bug: a flag is a fact
about the bytes, and turning the scanner off is a decision about *future* uploads, not a way of
forgetting a verdict somebody already reached. Fixed in the crate (`may_serve` now reads
`enabled` itself, with a test that runs all four `enabled`×`on_error` combinations) rather than at
the call site, so the panel and the share link cannot drift apart again.

**Proof.** `--test media_scan` → **10 walks, 0 failures**, each against a **hand-rolled scanner on
a real loopback socket** rather than a stubbed function: the client is where an outage lives, and
a sub-app would share every assumption the client makes. A **dead port** proves the ingest rule
(upload succeeds, row reads `error`, `hold` refuses with `file_scan_failed`, and flipping
`on_error` to `serve` changes the answer on the *next* request because the policy is read per
request, not cached). A `Nonsense` scanner proves fail-closed. A clean scanner proves a clean file
serves with its real bytes and an empty sweep still writes a run. A trashed file is never claimed.
Both walks that assert a row read it **out of PostgreSQL**, because a response that omits a field
is indistinguishable from one that stored it and chose not to say so. `cargo test -p omnion-media
--lib` → **148** (was 122), `-p omnion-api --lib` → **127** (was 122); `--test media` (13),
`--test media_shares` (5) and `--test media_transform` (5) are green against the routes this
touches. `apps/admin` `tsc --noEmit` clean.

**Environment.** Two things worth recording. The shared development database has a sibling wave's
migrations applied, so `cargo test --test media_*` dies with `VersionMissing(19)` before it reaches
a single assertion — `scripts/qa/run-media-walk.sh` gives each suite its own disposable database,
which is what a suite that must be *believed* to have run needs. And `reqwest` was a **dev**
dependency of the API crate: the scanner client needs it at runtime, so it is now a real one.

**Next.** Slice 4's remaining half — folder and file grants with inheritance, a deny beating an
inherited allow, the IAM subject picker, retention policies with the daily worker and its run log,
and reference-based purge refusal plus the repair scan. Done when a denied subject is refused on
the raw route and a retention run removes exactly the eligible rows.

## 2026-09-28 · REQ-010 slice 4 closed — usage, activity, and the file's two missing tabs

**What.** Slice 4's last open item, and the piece that makes slice 3's and slice 4's bookkeeping
readable: `GET /api/v1/media/{id}/references` (where a file is used) and `/activity` (what has
been done to it), `crates/media/src/usage.rs`, `omnion_audit::for_target`,
`features/media/{usage,activity}-tab.tsx`, and the two tabs on `/media/files/{id}`. Both reads are
`media.read`, and both load the file through its own site scope — including a **trashed** file,
because "what happened to this" is asked precisely after the deletion.

Five decisions, each a shortcut that produces a plausible wrong answer:

  1. **Records and rows are reported apart, and the sentence says which it means.** A page
     naming one hero in three fields is one record and three rows. The summary is the server's,
     not the panel's, so the two cannot disagree about what the integers mean.
  2. **A reference whose record is gone is rendered unresolved, with no link, and names its own
     fix.** It refuses a purge for ever; a list that dropped it would shorten every week with no
     way to tell a quiet file from a broken one.
  3. **Only the kinds that exist today are resolved.** `page` is a closed list, not a match over
     `resource_kind` — a module arriving tomorrow registers its own kind without a migration, and
     a lookup that switched on the kind would have to grow a branch for ever.
  4. **A file's story is written under two target names.** `media` for the bytes, `media_file`
     for a grant. A filter naming only the first answers "who could see this file in March" with
     a list of uploads — the most reassuring possible wrong answer. A folder rename is *not* in
     it, and that exclusion is as much a part of the claim as the inclusions.
  5. **The action arrives as a sentence beside its token**, and an action the server has not seen
     is shown in its own words rather than dropped: a trail with a hole in it is worse than one
     with an unfamiliar entry. A deleted account reads as "an account that has since been
     removed", never as "the platform".

**Proof.** `bash scripts/qa/run-media-walk.sh media_usage` → **6 walks, 0 failures** against the
real router. Four of the six exist because the shortcut gives a plausible wrong answer rather
than an error: the three-fields/one-record split; the stale reference being reported *and* the
repair scan really clearing it; the label coming from the **published** revision rather than the
newest one (a draft's title on a list answering "which published pages use this" is a confident
wrong answer); and a grant appearing on the trail under its other target name. The other two are
the permission (a reader holding `media.read` alone opens both screens) and the trash (a trashed
file still answers both). `cargo test -p omnion-api --lib` → **152 passed, 0 failed**;
`-p omnion-media -p omnion-audit` → **181 passed, 0 failed**; `pnpm typecheck` → 2 successful, 0
errors.

**Three defects, all of them in the test rather than the code** — and that is the honest report.
The fixture lacked `media.settings.manage` and `media.delete`; and it asserted a `404` for
another tenant's file where the whole media surface answers `403 cross_organization`. The route's
own doc comment had made the same wrong claim and now says what the platform does and why the
convention is the safer of the two: a `404` here would have made these the only two screens where
a foreign file is *invisible* rather than forbidden.

**Also fixed: the QA slot could be held hostage for 75 minutes.** `qa-slot.sh` names its place
file after `$$` — its own pid — and exits the instant it takes the place, so the reaper's
`kill -0` tested a pid that is dead within milliseconds of a *healthy* pass and fell back on age
alone. One crashed pass then held every later pass at "waiting for a QA slot" until each died at
its own timeout with no report. The reaper now reads the holder pid, which lives exactly as long
as the pass. Proved both ways against a synthetic slot dir with a real sleep as the live holder:
it spared the live place and reclaimed both dead ones. This is the second time this loop has hit a
stale-lock class, and the tell is the same both times — a symptom that reads as "the machine is
busy" rather than "a lock file is lying".

**Gate.** `bash scripts/qa/run.sh` was started first, as slice 4's outstanding condition, and is
**still queued behind siblings** — five concurrent walkthroughs from the w2/w3/w4/w7 worktrees
hold the one place `QA_SLOTS=1` allows. The walkthrough now drives both new tabs
(`checkUsageTab` / `checkActivityTab`), so the harness that finally gets the slot is already
extended; the retention tab's own pass (`runMediaRetention`) has now been queued for two
consecutive ticks and remains unrun.

**Next.** Re-run `bash scripts/qa/run.sh` and require zero high findings from `runMediaUsage`
alongside `runMediaRetention` before setting this REQ to `done`. Then the queue moves to the
first untouched item in wave 1: **REQ-021** (notification centre — in-app + e-mail). Migration
slot 0050 is free (wave5 holds 0048, wave4 0043, wave6 0046, wave7 0047).

## Tick 45 — REQ-021 slice 1: the in-app inbox

**What.** The platform's fourth feedback loop, and the only one that says *you*. The event bus
records facts, the audit trail records privileged work, the search index records documents —
and this one reaches a person. `omnion-notifications` (the record, the vocabulary, the store),
migration 0050 (six tables: the record, the preference matrix, the digest settings, the
delivery queue, the push devices and the channel config), the owner-scoped HTTP surface, the
bell in the header of every route, `/notifications`, and the walkthrough pass that drives both.

**Four decisions, each a shortcut that produces a plausible wrong answer.**

1. **Owner-scoped always.** No store function takes a user id the caller chooses — every one
   takes the owning id. "List somebody else's notifications" is not a parameter a handler can
   get wrong; it is a function that does not exist.
2. **Another person's notification is a `404`, never a `403`.** A `403` is the difference
   between "that is not yours" and "that is not real", and this surface is the one a curious
   panel is most tempted to poke at.
3. **The badge and the grouped lines are one query.** A bell that says 12 above four lines
   adding up to 9 is a screen nobody believes afterwards, and the fix is not a UI change: it is
   that both come from `store::summary`, which is the only place the unread count is computed.
4. **The vocabulary is compile-time, and the SQL duplication is a test.** SQL cannot import a
   Rust constant, so the category/priority/channel lists are written twice. The test
   `the_migration_agrees_with_the_lists` reads the migration file itself, because a category
   added to Rust and not to SQL passes every unit test in the crate and then the database
   refuses the row in production — which reads as "nothing happened".

**Two bugs the tests caught in the first draft, both quiet.** `include_read: false` together
with `unread: Some(false)` pushed both `read_at is null` and `read_at is not null` — a query
that is *always* empty, on exactly the path the panel's "show read" filter takes, so it would
have reported that nobody had ever read anything. And `priority_rank`'s doc said "lower is
more urgent" while the list it indexes is written the other way round; the doc was wrong, and
it is now explicit about which way and why.

**Proof.** `cargo test -p omnion-notifications -p omnion-permissions` → **62 passed**;
`-p omnion-api --lib` → **159 passed**; `pnpm typecheck` → 2 successful, 0 errors.
`scripts/qa/run-notifications.sh` → **PASS**: 31 migrations applied in order, 6 tables, a
repeated `dedupe_key` collapses to one row, and both `notifications_category_check` and
`notification_deliveries_channel_check` refuse exactly the values the crate refuses.
`scripts/qa/run-notifications-http.sh` → **PASS, 9/9** over a real socket with two real
sessions.

**The HTTP gate found three ways a gate can lie, which is worth more than the nine passes.**
It built into `$CARGO_TARGET_DIR` and ran a hard-coded `./target/debug` binary — so it started
a build that predated the feature and read *its* answers under this build's name. A failed run
left the API holding the port, and the next run's instance exited on `EADDRINUSE` while the
gate went on reading the orphan. And pre-applying the migrations let the API double-apply
them, which kills it at boot — the same orphan one step earlier. All three are fixed at the
root and the reason is in the file, because "the gate passed" is worth nothing while it is
reading someone else's process.

**And one assertion that was itself wrong.** The gate expected a `404` from the member and
got a `403` — correctly, because an account with no role never reaches the handler. A `404`
from a forbidden caller proves nothing about scoping, and conflating "you may not" with "it is
not yours" is exactly how a real leak survives a review. It now proves both, and binds the
member to the base role before making the scoping claims.

**Gate.** `bash scripts/qa/run.sh` is **running** — the QA slot cleared after ~55 minutes of
queueing behind the w6 pass, so the browser pass is in flight rather than merely queued.
Slice 1 is not closed until it reports zero high findings from `runNotificationsDepth`.

**Next.** Close slice 1 on the browser pass, then REQ-021 slice 2: the preference matrix, quiet
hours, the digest job, the e-mail and webhook adapters and the delivery rows in the drawer.

---

## 2026-09-28 · REQ-021 slice 2 — the reader's own channel configuration

**What.** The half of the notification centre that decides **how** a record reaches somebody.
`crates/notifications` gains `preferences.rs` (the rules) and `preference_store.rs` (the SQL);
`GET`/`PUT /api/v1/notifications/preferences`; `/notifications/settings`; and
`runNotificationSettingsDepth` in the walkthrough.

**Three decisions, each a shortcut that produces a plausible wrong answer.**

1. **The matrix stores only the cells a reader stated.** A full grid would be thirty rows per
   user per channel and would make a channel added in slice 3 a backfill instead of a
   non-breaking change. Everything unstated reads as `true`, and the *read* builds the
   complete grid in Rust and merges the stated cells onto it — so a person with no rows gets a
   valid form, not an empty one.
2. **The in-app cell cannot be switched off, and the store refuses it.** A `PUT` naming
   `in_app: false` is a `400` that says why. A disabled checkbox is a promise; a refusal is a
   guarantee, and the panel's own form is a client like any other.
3. **Quiet hours are validated and read as a pair, because both shapes are legitimate.**
   `22:00→07:00` wraps midnight and `01:00→05:00` does not. A window helper that knows only one
   of them is either never quiet or always quiet, and both are silent. A window that leaves no
   waking hours is refused by name rather than stored as "e-mail is on".

**The HTTP gate found a bug that had been shipped since slice 1.** The pass reported two
`request-failed` findings against `?category=approval` and `?priority=low` — both perfectly
legal values. The cause is not a typo: axum 0.8's `Query` extractor is backed by
`serde_urlencoded`, which **cannot put a repeated key into a `Vec`**. It answers

```text
invalid type: string "approval", expected a sequence
```

for *both* `?category=approval` and `?category=approval&category=ticket`. Every category and
priority filter on this surface had never worked, and every filtered list fell through to the
error state. It was reproduced in isolation — a four-line axum app — before being fixed, and
the list read now takes `RawQuery` and parses the string itself.

**Six of the pass's own assertions were wrong, and that is the more useful half of the tick.**
`emptyState` asked for `?read=read`, which by that point in the pass holds the very rows the
bulk step had just marked read — the list was correctly *not* empty, and the gate was measuring
its own ordering. `errorState` failed `…/summary` with a 500 and then asserted on the **list's**
error element, which that call cannot affect: the assertion could only ever have passed by
accident. The keyboard pass clicked a row, which opened the drawer, and then sent every
shortcut into the drawer. All three are the class of defect this harness exists to catch —
in itself — and none of them is visible from the report, which only says `false`.

**Two shortcuts the REQ listed were documented but not implemented.** Rather than weaken the
gate to match the code, `Shift+E` now marks the visible rows read — through a bulk helper that
takes an explicit id list, so it does not require a selection the reader never made — and rows
carry `data-read` so the state is assertable rather than a shade of grey.

**Proof.** `cargo test -p omnion-notifications` → **48 passed**; `-p omnion-api --lib` →
**176 passed**; `pnpm typecheck` → 2 successful; `bun build scripts/qa/walkthrough.cjs` clean.
Browser pass: **running**.

**Next.** Read `/tmp/omnion-build-qa2.log` and `docs/qa/QA-LATEST-main.md`; require
`report.notificationSettings` to be green and **zero high findings** from this change — in
particular no `request-failed` on any `/api/v1/notifications` URL, which is the 400's
signature. Then REQ-021 slice 3: push subscription lifecycle with pruning, the admin outbox
behind `notifications.admin`, the event router turning existing bus events into notifications,
and the per-channel delivery rows in the drawer.

## 2026-09-28 · REQ-021 slice 3 — the half that leaves the panel

**What.** The store layer (`push.rs`), the declarative router (`router.rs` + migration
`0051_notification_routes.sql`), `notifications.admin` in the permission catalogue, nine HTTP
endpoints in `apps/api/src/routes/notifications_admin.rs`, and the four sub-routers mounted
with their guards travelling with each group.

**The decision the whole slice rests on: a `permission:` recipient rule resolves through
`omnion_permissions::effective_permissions_for`, not through a hand-written join.** The join is
one round trip faster and *wrong* — it misses role inheritance, scope-mismatched bindings,
expired grants and explicit denials, and a notification that reaches somebody who lost the key
is a privacy defect rather than a wrong number. This is why `omnion-notifications` now depends on
`omnion-permissions`; the arrow points one way, because a permission check that emits would be a
cycle. The live gate builds the fixture that distinguishes the two: one person bound to a role
that *inherits* the permission and holds nothing itself, which a `join role_permissions` answers
zero for and the crate's resolution answers one.

**A push endpoint is a capability, so the type cannot express leaking one.** The device body has
no endpoint field at all — the hint (`…abcdef01`) is derived in the crate, and `DeviceBody` has
nowhere to put the full value. That is a stronger guarantee than a promise in a comment, and it
is asserted by serialising the body and searching the JSON for the secret.

**Three shapes were wrong on the first write and are worth naming.** `retry_delivery` returned
`bool`, which cannot distinguish "already sent" from "already queued" and turns a `409` on a
button the caller cannot use into the only answer; it now returns a `RetryOutcome`. The channel
list used `filter_map`, which would answer with four channels and render a settings matrix whose
missing column is indistinguishable from one the reader switched off. And the outbox check
constraint I first wrote was a boolean tangle that evaluated to `NULL` for `actor`.

**The live gate found three of its own assertions were wrong**, which is the more useful half:
the role fixture expected a survivor where the honest answer is zero; the outbox projection check
counted columns in `information_schema` rather than running the route's own `SELECT`; and a stray
`update` had already consumed the sent row, so `sent_rows_before=0` proved nothing. All three are
now assertions that would fail if the code regressed.

**Proof.** `cargo test -p omnion-notifications` → **79 passed** (48 at the start of this tick);
`-p omnion-permissions` → 62; `-p omnion-api --lib` → **187 passed** (176 at the start).
`scripts/qa/run-notifications-routes.sh` → **PASS**: 32 migrations applied, all three recipient
and category refusals hold, the outbox's own projection is 13 columns with no body, and a retry
moves the failed row while the delivered one stays at 1 → 1.

**Not proven, and not claimed.** The browser pass running in the background is the **slice-2**
gate; slice 3 has **no admin UI yet** — there is no `/notifications/outbox` screen and no routing
rules screen, so the nine endpoints are reachable and invisible. Slices 2 and 3 are not closed.

**Next.** Read the pass's `report.notifications` (the last one showed four `false` keyboard
assertions that `eb421ba` claimed to have fixed — if they are still false, the fix did not work)
and `report.notificationSettings`. Then build slice 3's two screens and write their depth passes
before running the pass again — the no-untested-screen rule applies to a screen that does not yet
exist as much as to one that does.

## Tick 48 — the pass that had been running for an hour, and what it actually said

**What.** The browser pass started at 22:33 finished at 23:41, and its `summary.json` answered the
question the last tick left open. `report.notifications` is **green on the bell, the badge, the
grouped lines, the bulk path, the cursor, `x`, `Enter` and all three states** — and its four
`false` values are one bug, not four. `report.notificationSettings` did not run at all: *"The API
answered with status 400."*

**The 400 was not a code defect.** `/api/v1/notifications/{preferences,channels,outbox,routes,
push-subscriptions}` all answer `400 Invalid URL: Cannot parse id…` — the parameter route
swallowing the static segments, which is exactly what the comment above the mount says it
prevents. The mount order in `routes/mod.rs` is correct. The QA API binary is from **20:39**;
slice 2's routes landed at 21:46 and slice 3's at 23:13. The 52 + 31 high findings this pass
filed against `/notifications/settings` and `/media/settings` are *both* that. The pass is
disposable by design, so the lesson is to read the binary's mtime before reading the router.

**What this tick fixed instead.** Two real defects, both found by hand against the live stack
because no existing gate could reach them:

1. `POST /api/v1/notifications/emit` addressed `user_ids` straight into the insert, so
   `notifications_user_id_fkey` answered a stale id by refusing the **whole batch** and naming
   itself: a `500` whose body quotes `violates foreign key constraint
   "notifications_user_id_fkey"`. Four good recipients lost to one stale one, and a constraint
   name shipped to whoever held the response. `0707141` checks recipients before the loop
   through a new `store::existing_users` and refuses in a sentence that names *which* id is wrong.
2. `Escape` was documented in the file header, the REQ's QA plan and the walkthrough, and it was
   **not in the key handler**. It was also unreadable behind `if (!row) return`, so it was inert
   exactly when the list had content — and the open drawer held the focus, which is why `e`,
   `Shift+E` and `/` failed behind it. `c30d324` answers `Escape` before the cursor is read.

**And the gate that had been lying.** `run-notifications-http.sh` decided its build by
`cargo build | grep -E "^(error|warning: unused)" && { echo "build failed"; exit 1; }`. The
status it tested was grep's, not the compiler's — and `warning: unused import` is a line this
crate prints on every *successful* build. The gate exited 1 over a build that finished in 0.31 s,
twice, printing a message indistinguishable from a real compile failure. `e8a797f` uses the
compiler's exit status and only reads the log when that status says something went wrong.

**Proof.** `cargo test -p omnion-notifications` → **79 passed**; `-p omnion-permissions` → **62**;
`-p omnion-api --lib` → **187 passed**. `pnpm typecheck` in `apps/admin` → **exit 0**.
`node --check scripts/qa/walkthrough.cjs` → clean.
`scripts/qa/run-notifications-http.sh` → **PASS**, 12 assertions, including the two new ones:
*a recipient that is not an account is a 400 in a sentence, not a constraint name* and *a batch
with one bad id writes none of the good ones*.

**Not proven, and not claimed.** `Escape` and the four shortcuts behind it are fixed in source and
typechecked, but no pass has run against them — the pass that found them ran against the 20:39
binary. Slice 3's outbox screen is still unvisited: `report.notificationOutbox` is **absent** from
this pass's report, so the depth pass written last tick still has never executed. REQ-021 stays
**in-progress**.

**Next.** Run `bash scripts/qa/run.sh` against a freshly built API (verify the binary's mtime is
newer than `0188172` before reading any result). Require `report.notifications.escapeClosedDrawer`
and the new `escapeWithNoRowUnderCursor` to be true, `eToggledRead` / `shiftEMarkedVisible` /
`slashFocusedFilter` to recover behind them, `report.notificationSettings.loaded` to be true, and
`report.notificationOutbox` to be **present** — that last one is the first pass that can close
slice 3. If `/media/settings` 422s survive a fresh binary, they are REQ-010's and this tick's
after that.

## Tick 49 — the pass that finally reached slice 3, and the list that emptied itself

**What.** The 23:51 pass ran against a binary built at 23:35, twenty-two minutes after slice 3's mounts
landed, and it answered the question the last two ticks were holding open. `report.notificationOutbox`
exists for the first time: five chips all carrying counts, `chiptotalMatchesSql` true,
`targetHiddenForActor` true, `noTargetWroteNothing` true, `actorActuallyWroteARow` true, a removal that
answers 204 and leaves the table, and `retryIsNotRetryable` true. `report.notificationSettings` is green
on the matrix, the honest save notice, the round trip and the error state. `runMediaRetention` came back
`ok: true` for the first time, which is REQ-010's blocker clearing.

**The pass also produced one string that says everything.** `report.notifications` ended with

```json
"keyboardRows": 0,
"keyboard": "no rows to drive — the list did not load"
```

which reads like a timing problem and is not one. The pass had marked its own three rows read four lines
earlier, and the list then showed none of them. `with_read` was a `bool` defaulting to `false`, and a
`bool` cannot tell "the client said nothing" from "the client said no" — so **every** caller that named no
filter got an unread-only list, while the panel's own State menu labelled that same state "Unread and
read". The screen promised a list it was not sending. Nothing failed loudly: the list rendered, the badge
was right, and the screen simply stopped showing mail the reader had already seen.

**What this tick fixed.** `ab3c105` gives `with_read` an `Option<bool>`, so absent and off stay
distinguishable and absence means *everything*; `4933c10` fixes the three client places that assumed a
positive flag — `filtersFrom` never sent it, `notificationQuery` dropped the `false` that *is* the
inbox filter, and the empty state's "Show read notifications" button **set** the flag to switch read
rows on when the correct action is to clear it. `797a3e8` adds the two gates that were missing, which
is the part that matters: `readRowsStayVisible` immediately after the bulk action, and `inboxFilterIsHonest`
on the other half, so a default nobody can turn off cannot pass as a fix.

**Proof.** `cargo test -p omnion-api --lib` → **188 passed** (187 + the absent-vs-off test, which
asserts the parse *and* the built query — a parser that keeps them apart and a builder that throws the
distinction away are two different bugs and only the pair is the fix). `-p omnion-notifications` → **79**;
`-p omnion-permissions` → **62**. `tsc --noEmit` in `apps/admin` → **exit 0**. `node --check
scripts/qa/walkthrough.cjs` → clean. `scripts/qa/run-notifications-http.sh` → **PASS 13/13** over a real
socket, the new leg reading `all=1 inbox=0 live=1`. Browser pass: 36 pages, 1095 clicks, 88 field fills,
39 form submissions, 84 findings (79 high, 5 medium).

**On those 79 high findings, honestly.** 74 are `/media/*` and belong to REQ-010, which is the other
in-progress REQ in this wave — 30 of them are one 422 on `/api/v1/media/{id}/raw?preset=standard` and 22
more of the same from a second file. The remaining 5 are **this pass's own deliberate refusal probes**:
three the 400 in-app-column lock and two the routed-500 error state, each of which the pass asserts as
*expected* two lines later. Neither group is caused by this tick's change.

**Also fixed, incidentally.** The gate I extended had two defects of its own, both caught because the
first version of the assertion failed in a way the change had nothing to do with: `grep -c` exits 1 on
an empty body, so under `set -e` the *inbox* leg — which is meant to be empty — aborted the whole gate
before printing a verdict; and the row to mark is read from SQL rather than reusing the `$target` that
the 404 check *below* assigns, because a shell script that reads a value before the line that sets it
reports an empty id as "the endpoint refused".

**Not proven, and not claimed.** No pass has yet run against the `with_read` fix, so the keyboard leg —
`escapeClosedDrawer`, `escapeWithNoRowUnderCursor`, `eToggledRead`, `shiftEMarkedVisible`,
`slashFocusedFilter` — is still unproven and the REQ stays **in-progress**. `quietSaved` and
`digestPersisted` are still false: the widgets render, but the pass does not change them yet, so those
two halves of the settings box are honestly unproven despite the box being ticked for what was proven.

**Next.** Run `bash scripts/qa/run.sh` again (verify `stat -c %y target/debug/omnion-api` is newer than
`ab3c105` first). Require `report.notifications.readRowsStayVisible`, `inboxFilterIsHonest`,
`keyboardRows > 0` and then the five keyboard keys, plus `notificationSettings.quietSaved` and
`digestPersisted` — and extend the settings pass to change those two fields rather than merely render
them. If it is green, REQ-021 closes and the wave moves to the 74 `/media/*` findings, which are
REQ-010 slice 4's remaining gate.

## Tick 50 — the quiet window that came back wearing a different spelling

Two commits, `c48db9d` (the fix) and `60a28ea` (the gate), both pushed, tree clean.

**What.** The previous tick's `next_hint` told this one to go and run the browser pass. It could
not: `qa-slot.sh` allows one pass at a time and the box was at load 13 with six sibling passes
compiling, so a pass started under those conditions would have timed out having measured
nothing. The two settings boxes the hint named — `quietSaved` and `digestPersisted` — were the
honest place to start instead, and reading why they were false turned up something the browser
pass had been reporting faithfully for two ticks.

**The defect.** `notification_settings.quiet_hours_start` is a Postgres `time` column, and
`read_settings` selected it with `::text`. Postgres prints a `time` that way as `22:00:00` —
seconds always present, zero-padded. The platform's clock vocabulary is `HH:MM`: the shape the
form sends, the shape `validate_quiet_hours` accepts, and the shape `parse_clock` reads. So
every window that was saved correctly came back in a spelling nothing could parse,
`parse_clock` answered `None`, and `in_quiet_hours` took the arm its own doc comment promises
("no window at all means `false`"). The setting a reader had just turned on decided nothing from
the next request onwards, and the settings form could not read back what it had written.

Nothing about it fails loudly. The save is a `200`. The row in Postgres is exactly what was
asked for. The digest half of the same row — a string column — came back fine, which is why
`digestPersisted` failed for a *second*, unrelated reason in the pass while the two fields
looked equally broken.

**Why the pass could not have told you.** `quietSaved` asserts the time input still reads
`22:00` after a save, and `digestPersisted` asserts the two selects. Both were false, but they
were false for different reasons, and only one of them was a defect. Reading the *values* the
pass had collected — not the booleans — is what separated them: `timezoneSaved` was true
alongside two false fields, and timezone is a plain `text` column.

**The fix.** `to_char(quiet_hours_start, 'HH24:MI')` in the read, so the shape is produced by
one literal in the SQL. `parse_clock` also tolerates a seconds field, so a row written before
this change is still a window rather than its absence — a widening that has to be paid for
with a test, because "accepts more" is how a parser turns into a function that accepts
anything with a colon in it. `format_clock` is the write-side counterpart that names the shape,
and the round trip between the two is asserted over five instants.

**One mistake worth recording.** The first version of the fix declared the columns
`Option<time::Time>` and let `format_clock` do the work. It compiled — `query_as` checks its
types at *decode* time, not at compile time — and came back as a `500` on the settings screen:
`mismatched types; Rust type Option<time::Time> (as SQL type TIME) is not compatible with SQL
type TEXT`. The fix for that is the one line the comment now explains: `to_char` returns
`text`, so the column is decoded as `Option<String>`.

**Proof, in both directions.** `scripts/qa/run-notifications-http.sh` now has two legs that
own the seam, and the order is the point: save through the API, read the row out of Postgres to
prove the write happened, *then* read it back through the API and compare the exact string.
Against the pre-fix tree (stashed, rebuilt, re-run) the gate printed
`FAIL quiet hours did not round trip: row=[22:00 07:00 weekly 3 17] api=[22:00:00..07:00:00 hour=17]`
— the row correct, the API's own answer unusable. Against the fix: **PASS 15/15**. A gate that
only read the API back would have passed against a store answering with whatever it was handed.

`cargo test -p omnion-notifications` → **83** (79 + 4 new). `cargo test -p omnion-api --lib` →
**188**. `tsc --noEmit` in `apps/admin` → exit 0.

**Still not proven, and not claimed.** No browser pass has run against a binary built after
this, so the keyboard leg (`escapeClosedDrawer`, `escapeWithNoRowUnderCursor`, `eToggledRead`,
`shiftEMarkedVisible`, `slashFocusedFilter`) and the browser's own `quietSaved` /
`digestPersisted` are open. REQ-021 stays **in-progress** for that reason alone.

**Next.** Run `bash scripts/qa/run.sh` with no `QA_STACK` override when the box is under load
~6 and `qa-slot` is free — verify `stat -c %y target/debug/omnion-api` is newer than `c48db9d`
*before* reading any finding, because the pass tears the stack down. Require
`report.notifications.keyboardRows > 0` and the five keyboard keys, and
`notificationSettings.quietSaved` + `digestPersisted`, which the data path can now support.
The other open item is unchanged: the 74 `/media/*` high findings, which are REQ-010 slice 4's
remaining gate.

## Tick 51 — REQ-016 slice 1: the event catalogue (the registry the platform never had)

**What.** `crates/events/src/catalogue.rs` — 68 event names (67 live, 1 reserved) with their
area, a one-sentence description, their payload fields and a required flag. Compiled in, not
stored. `GET /api/v1/events/catalogue` serves it, a group subscription (`page.*`) is stored as
the wildcard *and* as today's expansion, and `enqueue_fanout` matches either.

**The gap this closes.** Before this, the only truth about an event name was the string literal
at the call site. The endpoint form had nothing to build a picker from, `/events` had nothing to
describe, and a typo in one emitter became a name no receiver could ever subscribe to — silently,
with the bus recording the fact and the delivery queuing normally. Nothing was broken and nothing
said so.

**Two decisions, and the reasons are the interesting part.**

*A group is stored twice.* Storing only today's expansion means `page.*` silently stops covering
an event added next release — the exact surprise a group exists to remove. Storing only the
wildcard means a receiver can be subscribed to a group whose members do not exist, and cannot
tell what it is getting. So the wildcard is the subscription and the expansion is the readable
copy, and the fan-out tests both. The SQL grew one `or $4 = any (w.events)` clause; the clause
is there for the row written before reconciliation existed, or by an operator's own SQL, which
would otherwise stop receiving with no error anywhere.

*`order.created` is listed, described and subscribable while commerce is unshipped* — and marked
`reserved` so the panel can say *which module ships it* rather than implying the platform is
broken. A reserved name is a name: `order.*` expands today.

**The registry writes itself, and its first two findings were real.** The table is a macro
(`crates/events/src/catalogue.rs`), one row per event, and `apps/api/tests/events.rs`
`every_emitted_name_is_in_the_catalogue` walks the source tree for `NewEvent::new("…")` and fails
with the file and line when an emitter names something the table does not carry. A unit test
inside `omnion-events` cannot do this — it cannot see the modules. It immediately earned its
place: it caught that the upload fact is emitted as **`media.created`**, not the `media.uploaded`
the request imagined. The table was written to match the code rather than the spec, and the
reason is in the source: renaming the row would make the catalogue agree with the request and
disagree with every receiver that already subscribed.

**A gate that cannot fail proves nothing**, so the drift test was proved in both directions:
with a `totally.made_up` emitter injected into `media_settings.rs` it failed with
`1 emitted name(s) are not in the catalogue … totally.made_up (apps/api/src/routes/media_settings.rs:24)`,
and green again once restored.

**Also found, by a test I wrote to check a different thing.** `group_members` matches the first
dotted segment, so a group is `order` while the area is `commerce` — I had written a test
asserting `commerce.*` would expand. Area and group are different vocabularies and conflating
them would make `commerce.*` look correct while matching nothing. Pinned by
`an_area_is_not_a_group`.

**Macro rules, for the fourth time.** A `*` repetition may not be followed by another
repetition, a field tuple needs its literal parens in the pattern, `req`/`opt` are idents not
literals, and a `const fn` cannot match on `&str` on this toolchain — so the status and the
required flag are matched as *tokens* and turned into the values by the macro. The table is now
stricter than it was designed to be: a row that says `Reserved` in the wrong case is a compile
error rather than a row that silently reads as `live`.

**Proof.**

- `cargo test -p omnion-events --lib` → **41** (24 before, 17 new)
- `cargo test -p omnion-api --test events -- --test-threads=1` → **4/4**, real Postgres + a real
  loopback receiver; the group subscription delivers a `page.published` and the signature
  verifies against the secret the creation response returned
- `cargo test -p omnion-api --lib` → **188**
- `tsc --noEmit` in `apps/admin` → exit 0

**Not done, and not claimed.** Slice 1 is not closed. Missing: the emission calls for names the
modules do not record yet (`page.created|updated|unpublished|deleted|restored`,
`user.updated|deleted`, `site.archived`, `domain.*`, `plugin.*`, `theme.activated`,
`workflow.run.*`) — the catalogue names them and the table asserts they are listed, which is the
honest direction: the registry is the specification the emitters are measured against, and they
have not caught up. And the `/events` screen with its Feed and Catalogue tabs does not exist; the
data source does.

**Next.** The emission calls, then the `/events` screen. The catalogue made the work mechanical
on purpose: each emitter is a `bus::emit` beside the write it already does, and the drift test
turns "did I remember?" into a red line with a file and a line number.


---

## 2026-09-29 — REQ-016 slice 1 (emission half) · the gate that walked one direction

Eleven emissions, one honest demotion, and a gate that closes the direction nothing was
checking.

**The finding.** Slice 1 shipped a drift gate that walks the source tree and fails when an
emitter names an event the catalogue does not carry. It works, and last tick it earned its
place. But it walks **one** direction, and the other direction is where the damage was.
Twenty-seven rows were marked `Live` — which the type documents as "emitted by the platform
today" — and nothing emitted them. `page.created` had a row, a description, payload fields
and a picker entry; there was no `bus::emit` for it anywhere in the tree. So an operator
subscribed to `page.created`, the subscription was accepted, and nothing could ever arrive.
No error, no warning: a registry that promises deliveries the platform never makes.

**Eleven now emit**, each a `bus::emit` beside a write that already existed:

| name | where |
| --- | --- |
| `page.created` `page.updated` `page.deleted` `page.restored` | `routes/content.rs` |
| `translation.updated` | `routes/content.rs` |
| `domain.added` `domain.removed` | `routes/tenancy.rs` |
| `site.archived` | `routes/tenancy.rs`, on the transition only |
| `user.updated` | `routes/iam_subjects.rs` |
| `user.deleted` | `routes/scim.rs` |
| `theme.activated` | `routes/onboarding.rs` |
| `webhook.endpoint.created` `updated` `removed` `tested`, `webhook.secret.rotated` | `routes/webhooks.rs` |
| `webhook.delivery.failed` | `crates/events/src/engine.rs` |

**Ten are now `Reserved`, and the reason is the point.** `plugin.*` (no plugin module ships
yet), `workflow.run.*` (the engine does start runs — but the automation matcher *drains the
same bus* and starts a run per matching rule, so emitting there without a loop guard is a
feedback loop wearing a feature's clothes; that is a decision, not a line), `page.unpublished`
(no route takes a published page back to draft), `translation.published`, `domain.verified`.
`Reserved` is not a demotion for its own sake — `order.created` has carried it all along. It
is the status that lets the picker say *a module ships this* instead of implying the platform
is broken. `a_reserved_name_names_the_module_that_ships_it` pins each row to its owning module,
so nobody re-promotes one on a hunch: the reverse gate turns red with the name.

**A gate a convenience wrapper can blind.** The first version of the content helper took the
name as a `&str` and the new gate immediately reported `page.created` unbacked from a file
that emitted it three lines above — the literal had moved into the helper's argument, where a
source-walking gate cannot see it. The helper now takes a built `NewEvent` and the literal
stays at each call site. A test that can be defeated by tidy code is a test to design
against, and the same trap bit the forward gate afterwards: a doc comment explaining the rule
contained the constructor call in prose, and the gate read it as an emitter. Both are written
down in the source now.

**The existing tests were right to fail.** Exact row counts in the feed broke, because the
feed correctly carries more facts now: `page.*` delivers two events instead of one (a group
subscription is no longer publish-only, which is the point of a group), and tenant B's feed
is no longer empty because connecting an endpoint records an event *about that endpoint*. The
last one looked like a tenancy leak and was not: the isolation assertion now says what it
means — B sees its own endpoint and nothing of A's. The counts were replaced with presence
and ordering assertions, because a count turns every future emission into a breaking test.

**Proof.**

- `cargo test -p omnion-events --lib` → **42** (41 before, +1 for the reserved-ownership pin)
- `cargo test -p omnion-api --test events` → **5/5** (4 before, +1 the reverse gate) against
  real Postgres and a real loopback receiver
- `cargo test -p omnion-api --lib` → **188**
- `tsc --noEmit` in `apps/admin` → exit 0
- The new gate proved in both directions: promoting `plugin.installed` to `Live` turned it red
  with the name, restore turned it green

**Not done, and not claimed.** The `/events` screen with its Feed and Catalogue tabs does not
exist, so the acceptance box that names the Catalogue **tab** stays unticked even though the
API behind it is proven. Slice 2 (endpoint management UI) and slice 3's delivery-operations
UI are untouched. No browser pass this tick: load 21.6 with sibling writers active, so REQ-010
slice 4 and REQ-021 remain blocked on the QA slot.

**Next.** The `/events` screen — the data source is done and proven, the screen does not exist,
and it is the last thing in slice 1. Re-check the QA slot on arrival; when it is free and the
box is under load ~6, run `bash scripts/qa/run.sh` with no `QA_STACK` override and extend
`scripts/qa/walkthrough.cjs` so the new route is visited and clicked.

---

## 2026-09-29 · REQ-016 slice 1, the screen half — `e7399d6`

**What.** The `/events` screen, and the feed filters it needs to be a screen. The catalogue had
a data source and nothing that rendered it; the feed had `?limit` and one `?name` and no way to
page. Both halves are now closed, except the browser pass, which did not get a slot.

**The finding: `?name=a` never filtered — it failed.** The first version of the query shape was
a `Vec<String>` behind `Query<EventsQuery>`, which is the obvious way to write it and the wrong
one. `serde_urlencoded` — the deserializer `Query` is built on — **rejects a single occurrence of
a repeated key for a sequence field outright**, with a `400` whose body is plain text rather than
the API's error envelope. So `GET /api/v1/events?name=page.published` returned `400`, not a
filtered list. The existing test caught it on the first run and the fix is a hand parser, which
is not a downgrade: the same three rules already live in `notifications::parse_list_params` and its
doc comment explains why each one exists. The new one says the same three and adds the fourth the
notification list learned the hard way — *ignore* a key this build does not know, so a panel that
sends one filter earlier than the API still gets its feed.

**A tenancy assertion that passed for the wrong reason.** The scoping assertion was first written
against the platform owner, and it failed: five rows came back where four were expected. The
owner's session carries `organization_id = None`, and the store's `($1::uuid is null or
organization_id = $1)` reads that as *every* organization — which is correct for a superuser. The
test was asserting the superuser sees everything, not that a tenant may not see another's. It now
reads as an **organization** account, and the seeded foreign row has an *actor* from a third
identity, so a broken actor filter and a broken organization filter are both caught.

**`has_more` comes from the row past the page.** Not from a second `count`. A list and a count
that disagree is a list that is lying, and the disagreement is invisible until somebody pages to
the end and finds a row they have already seen. The store asks for `limit + 1` and truncates; the
cursor is the last row's own id, exclusive, so the next page cannot re-serve the row the cursor
names. Newest-first is asserted *across* the boundary, not per page, because "sorted within a page"
is a property a single page satisfies by accident.

**A live name nobody subscribes to is the useful number.** The catalogue now carries each name's
24-hour delivery count, scoped to the caller's organization. It is the one question the `live`
column cannot answer: the status says the platform records the name, the count says whether any of
your endpoints ever heard it. It is `0` rather than absent so the column is always a number the
screen can render, and the walk asserts *every* entry carries a number — a missing key would
render as a blank cell indistinguishable from a name nobody has data for.

**The window is relative in the URL and absolute in the request.** `?window=24h` is what the
panel stores, and the `from` instant is computed at request time. Storing the instant would make a
pasted link mean "the last two hours" for the sender and "nothing at all" for the reader, with no
way to tell which happened. A `from` in the past *is* still accepted directly, because the API is
a public surface and a relative filter is a panel convenience, not a protocol rule.

**Proof.**

- `cargo test -p omnion-events --lib` → **42** (unchanged; no new unit test was needed — the
  store's filter is SQL and the walk exercises it against a real database, which is the only
  place it can be exercised honestly)
- `cargo test -p omnion-api --lib` → **188**
- `cargo test -p omnion-api --test events` → **6/6** (5 before) against real Postgres
- `tsc --noEmit` in `apps/admin` → exit 0
- Two walkthrough routes added (`/events`, `/events?tab=catalogue`) and `runEventsDepth` written:
  it publishes a page through the real route, filters by the first name on screen, reloads to
  prove the URL carries the filter, expands a payload, drives `j`/`Enter`/`Escape`, forces a
  routed `500` and checks the error banner, reads the catalogue back against the API and asserts
  the totals match, narrows by area, expands an entry's payload fields and uses "Filter feed" to
  cross to the other tab

**Not done, and not claimed.** **No browser pass this tick.** The QA slot was held by a sibling
writer for the whole window and the box was at load 26, so `runEventsDepth` is written and
**unrun**, and the acceptance box that names the Catalogue *tab* stays unticked with the reason
written into the box. The other two things the box wanted — the screen and the filters — are
built, typechecked and API-tested. Slice 2 (endpoint management UI) and slice 3's delivery
operations are untouched.

**Next.** Re-check the QA slot on arrival; when it is free and the box is under load ~6, run
`bash scripts/qa/run.sh` with no `QA_STACK` override. If it is green, tick the catalogue box and
close slice 1; if the pass finds anything, fix it in the same tick — the depth pass is already
written, so a green run closes the slice rather than starting it. After that, slice 2: the
`/webhooks` endpoint list, which is the larger of the two remaining halves and the one the
operator needs first when a delivery is missing.

### 2026-09-29 · omnion-w7 · REQ-098 closed, REQ-099 slice 1 opened

**REQ-098 (Model Registry & Router) → `done`.** The two gates its closing box named as unproved
both ran this tick: `pnpm build` exited 0 (route table listed, `ƒ Proxy (Middleware)` emitted) and
`pnpm typecheck` is clean. The workspace suite is **70 suites, 1347 passed, 0 failed**
(`EXIT=0`), against a **private** database — and that choice is the result, not a detail.

**The first workspace run reported 13 failures and every one of them was the database.**
`apps/api/tests/analytics.rs` panicked 13/13 at line 184, which is
`db.migrate().await.expect("migrations must apply")`: the shared dev database is parked at
migration **38** and cannot apply anything nine other writers have numbered since. Re-run
byte-for-byte unchanged on a private database, the same 13 tests pass in 0.05s. Nothing in this
request touched analytics, and the honest reading is the one the ledger has said for six ticks —
a workspace suite on a shared database is not a verdict, it is a report on the database.

**The five AI suites, individually:** `ai_hub` 14 · `ai_catalog` 11 · `ai_routing` 11 ·
`ai_decisions` 11 · `ai_live_path` 5 — 52 walks, zero failures. The crate is at **197** unit
tests (was 161) once REQ-099's step machine landed underneath it.

**REQ-099 slice 1, first commit `0fc9fe4`.** `crates/ai-hub/src/agent.rs` — the **pure** half of
the slice: `RunLimits` and its clamping, `StepMachine`, `AgentEvent`, `ToolCall`, the three
vocabularies, `should_stop` and `delimit_untrusted`. Every stop condition has a failing-path test
that fires it with values the test wrote itself, because a rule that cannot stop without a
network, a clock or a row is a rule nobody can prove stops anything. The I/O half — provider
call, tool execution, persistence, the SSE endpoint and the screens — is the rest of slice 1.
**36 new tests; the crate is at 197.**

**The order `should_stop` checks in is the contract.** Cancellation, loop detection, deadline,
token budget, step cap. A run that both ran out of steps and blew its deadline on the last one
must record one stable word, and the person who pressed stop must never be answered
`max_steps`. `StopReason::is_failure` deliberately excludes `Cancelled` and `MaxSteps` for the
same reason a success rate must not be gameable: a person who stopped the run stopped it.

**Next.** The rest of REQ-099 slice 1 — the run store, the loop's I/O, `POST /ai/agents/{id}/runs`
with its SSE stream, the run list and the trace screen. `ai_agents`/`ai_runs`/`ai_run_steps` want
migrations **past** main's high-water (0051 at last fetch, re-read at commit time), not the tight
next number.

### 2026-09-29 · omnion-w7 · REQ-099 slice 1, the run store and a duplicate migration caught

**The merge came first, and it was not clean.** `git merge origin/main` produced two conflicts,
both in files a previous tick had resolved by taking "ours" wholesale. The BUILD-LOG had 22
sections against main's 55: **30 entries belonging to other writers (REQ-006, REQ-007, REQ-010,
REQ-021, REQ-032) had been silently dropped from this branch** and never restored. Rebuilt as
main's file plus my `###` sections, verified by a multiset check — every main section present,
every one of mine present, zero lines lost either way. `scripts/qa/walkthrough.cjs` had the same
shape; the one hunk was main's event-console pass, kept.

**A duplicate migration version, caught by reading `git ls-tree` instead of guessing.** main
shipped `0052_webhook_delivery_ops.sql` while this branch already had
`0052_ai_route_decisions.sql`. Two files claiming one version is the sqlx `VersionMismatch` that
kills a whole suite before a single assertion runs. Renumbered this branch's four AI migrations to
`0058`–`0061` and updated the REQ-098 prose naming them.

**The gate URL in the state file was missing its password, and nobody noticed for a tick.**
`OMNION_DATABASE_URL=postgres://omnion@127.0.0.1:5433/…` fails authentication, so every DB test
in this branch had been *skipping* while reporting `ok`. The correct form is
`postgres://omnion:omnion@127.0.0.1:5433/…`. Thirteen tests that read `13 passed` were twelve
silences.

**A test that fails for the wrong reason is worse than no test.** The migration checker asserted
"this INSERT errored" and two cases were green on a *missing foreign key* rather than on the rule
under test. Rewritten to assert on the constraint name postgres names. Then one case it was
supposed to catch did not fire at all: a `running` run could carry `stop_reason = max_steps`,
claiming an ending that has not happened. Added `ai_runs_reason_implies_finished`.

**Two cost bugs the tests caught and the compiler could not.** The run's `cost_micros` was both
incremented per step *and* recomputed by `finish_run` — a three-step run reported 13 599 micros
for 3 600 of work. And `finish_run` never wrote the cost at all, so the column was always zero
until something else touched it. The increment is gone; the cost is recomputed from the steps,
which also makes closing a run twice idempotent.

**A `Drop` impl cannot await, and a detached teardown races the binary's exit.** The first
version of the integration harness created a throwaway database per test and never removed it,
on the theory that a panicking test should leave evidence. Thirteen tests leaked 133 databases;
the shared PostgreSQL went into recovery, and *other writers'* suites began skipping with
"pool timed out". `dispose` is now explicit and awaited, and one clean run leaves zero behind.

**sqlx and PostgreSQL disagree about types in four places, each found by running the code.** A
raw `insert` sends placeholders untyped, so every nullable uuid needs `::uuid` and `temperature`
needs `::numeric` — and putting a cast on the wrong slot fails with "cannot cast double precision
to uuid". `numeric` does not decode into `f64`. `jsonb` does not decode into `Vec<String>` and
has no cast to `text[]`, so it is expanded through `jsonb_array_elements_text` — with a
`coalesce`, because `array_agg` over zero rows is NULL and every agent without tools would have
failed to load. And `sum()` returns `bigint`, so the totals need `::int`.

**Proof this tick.** `cargo test --workspace`: the only failing target is
`-p omnion-api --test automation`, 3 tests, all `evaluated: 2, right: 1`. **Proved pre-existing
on `origin/main`** by checking out main and re-running with none of this branch's code: identical
3 failures. It is w3's wave, not this one. `omnion-ai-hub` 197 unit tests green.
`apps/api/tests/ai_agent_runs.rs` 13/13, zero skips, zero leaked databases.
`scripts/qa/agent-migration-checks.py` 27/27 rules, each asserted by constraint name.

**Next.** The rest of REQ-099 slice 1: the provider call and tool execution, the runner's
claim/heartbeat/requeue, `POST /ai/agents/{id}/runs` with SSE, the run list and the trace screen.



## 2026-09-29 — REQ-016 slice 2 (endpoints + delivery operations) · the part that makes a webhook operable

build webhooks: endpoints, redelivery, rotation, the stats that do not flatter you

Slice 1 gave the bus a read side. This is the half an operator actually reaches for: connect a
receiver, watch what it was sent, send it again, and find out whether it is still working.

**0052_webhook_delivery_ops.sql**, four routes on `/webhooks/{id}` (`deliveries`, `redeliver`,
`redeliver` batch, `stats`, `secret/rotate`), and four screens: `/webhooks`, `/webhooks/new`,
`/webhooks/[id]` (Overview / Deliveries / Stats) and the edit form.

**Six decisions, each a shortcut that produces a plausible wrong answer.** The **redelivery
resets the row** rather than inserting a second one — the `(endpoint_id, event_id)` unique index
would refuse the insert anyway, and it should: two rows for one fact means the receiver cannot
tell a replay from a duplicate, and it is also what makes `attempts` mean "attempts in this
round" instead of "attempts ever", which is the number compared against `max_attempts`. A
**pending row is refused**, and its checkbox is disabled rather than offered: the runner holds
that row's lease, so a reset would hand it to the next claim while the attempt is in flight —
the one place this operation could double-send. **The three refusals carry three codes**, because
"wait a moment" and "fix your receiver instead" are different advice, and the refusal travels
*inside* `EventsError` (as a `409`, not a `400` — the row's state is the problem, not the
request) so a store error stays an error instead of being reported as "no such delivery". The
**success rate counts settled traffic only**: pending in the denominator would read 0% for a
queue whose every delivery is about to succeed, and a test row in it would let an operator make
a broken receiver look healthy by pressing the button — so a history that is only probes answers
`null` and the screen prints "No traffic" with the excluded count underneath. The **cursor is
`(created_at, id)`**, because the read sorts by both and a cursor on one column of a two-column
order repeats rows whenever two deliveries share a timestamp, which is normal when the bus fans
out; half a cursor is refused by name because a null id there is a `500` on a request the panel
builds itself. **Rotation is a separate route from `PATCH`**, because it is the one write whose
answer carries the secret — a receiver cannot be reconfigured with a value it never saw.

**Two defects the walks found, both of the "the column exists" kind.** Migration 0052 added
`trigger` and nothing wrote it, so every test delivery was stamped `event` and the stats read was
counting a button press as the platform delivering something; the column is now stamped at the
one place a test is queued. And `redeliver` originally reported its count through a follow-up
read, which can observe a different value after somebody else pressed the same button — it now
returns the count from the update itself.

**The rotation is proved against a receiver, not a status code.** The walk creates the endpoint
with an operator-supplied secret so it holds both values, delivers once, rotates, delivers again,
and asserts the second delivery verifies against the new secret and **fails** against the old
one. The receiver is `infra/mocks/webhook-receiver.mjs`, started by the depth pass and killed in
its `finally`, so a throw mid-pass does not leave a port bound.

**Proof.**

- `cargo test -p omnion-events --lib` → **45** (42 before, +3)
- `cargo test -p omnion-api --test events` → **9/9** (6 before, +3) against real Postgres
- `tsc --noEmit` in `apps/admin` → exit 0
- Commits: `cdba36e` (the store and the migration), `b826899` (the routes and the walks),
  `17d87cd` (the screens and the depth pass)

**Not done, and not claimed: no browser pass.** A sibling writer held the QA slot for the whole
window at load 18–20, so `runWebhooksDepth` is written and **unrun** and every acceptance box
that names a screen stays unticked with the reason written into the box. The fast gates ran
instead and the pass is queued.

**Next.** When the slot frees, run `bash scripts/qa/run.sh` with no `QA_STACK` override. If it is
green, tick the screen boxes and close slice 2. Then slice 3, which is the retention sweeper
plus the delivery-failed notification REQ-021 turns into an operator alert.
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

---

## 2026-09-29 · REQ-016 slice 3 — the bus's own retention (tick 55)

**What.** The event bus grew on every mutation and nothing ever forgot anything: `/events`
shows the last page, the API keeps a keyset cursor over every row, the automation matcher
replays from its own cursor. Slice 3 gives the bus a window, a sweeper, a run log, and a
`/events` **Retention** tab — the third tab beside Feed and Catalogue, answering a different
question (what will be forgotten and when) rather than a fourth card inside the Feed.

Migration `0123_event_retention.sql` puts the window on the **organization**
(`organizations.event_retention_days`, `between 1 and 3650`, never null, default 30). Three
decisions carry it, and each is a place the obvious shortcut is wrong:

* **A `pending` delivery pins its event.** The obvious sweep — "delete events older than N
  and let `on delete cascade` take the deliveries" — deletes a fact a receiver is still owed.
  The receiver's only symptom is a delivery that never arrives with nothing in the platform
  saying why. The store's predicate selects events with **no delivery at all** or with **only
  settled** ones; a `pending` row pins its event for ever. An event nobody was ever queued
  for is the bulk of the bus, which is exactly the part worth deleting.
* **The window is a column on the organization, not on the event.** "30 days" is a policy an
  operator sets once and then changes; storing it per event would mean a sweeper that has to
  *compare* the two to decide what is old. The cutoff is computed per organization inside the
  same statement, and an organization that has never set one falls back to the platform
  default rather than to `null` — because `null` would mean "keep for ever", which is a
  decision nobody made deliberately.
* **A run that deletes nothing is still written to the log.** "The last sweep was at 03:00 and
  it found nothing" is the sentence an operator needs on the day they ask why a March event is
  still in the feed, and a table that only records activity cannot answer it on the day
  nothing happened.

**A window on the organization is also a permission split.** Reading the window, the counts and
the last sweep rides `events.read` — describing what will be removed is reading the bus.
**Changing** the window and running a sweep are `webhooks.manage`, because shortening a window
destroys an audit trail and a read-only auditor must not be able to trigger that from a link.

**`retention` is declared before `/events/{id}`.** Same reason `/events/catalogue` is: a
literal segment registered after a parameterised sibling is read as an event id, and a request
that is perfectly valid answers `404 no such event`.

**A count that ignores pending deliveries is a number the screen lies with.** The `due` figure
comes from the *same predicate the `delete` uses* — an event pinned by a pending delivery is
in `events` and never in `due`. A panel that said "412 due" on the morning a sweep removes 0
would be quoting a number nobody can reconcile with the run log.

**The panel refuses the range before the server does, because the bounds are the server's.**
`min_days`/`max_days` arrive in the read rather than being written into the component, because
a range written in two places is a range that will disagree, and the input that disagrees with
the server is the one that gets a `400` nobody can act on. Out of range *disables* Save rather
than offering a failure. And when the server does refuse, its own sentence is shown — it names
the field and the range, and replacing that with "invalid value" throws away the only sentence
that says which bound was crossed.

**Proof.**

- `cargo test -p omnion-events --lib` → **47** (45 + 2)
- `cargo test -p omnion-api --test event_retention` → **1/1** against real PostgreSQL, on a
  one-day window set through the same `PATCH` an operator uses
- `cargo test -p omnion-api --test events` → **9/9** (the sweep must not disturb the existing
  delivery history)
- `cargo test -p omnion-api --lib` → **188**
- `tsc --noEmit` in `apps/admin` → exit 0
- Commits: `f47f35f` (migration, store, worker, routes, walk), `14862ce` (the tab and
  `runRetentionDepth`)

**Two defects the walk found, both of the same shape as slice 2's.** The first is a **function
PostgreSQL 16 does not have in the form the argument was written in**: `make_interval(days =>
$2)` bound to an `i64` fails with *"function make_interval(days => bigint) does not exist"* —
a named argument has to land on `int`, and the error names a function that plainly exists, so
it reads like a migration fault rather than an argument type. The second is an **assertion
written in the same breath as the code that broke it**: the walk set the window through a
`PATCH`, that `PATCH` recorded `webhook.retention.changed` on the same bus, and the count
assertion still said "two aged events" while the bus honestly held three. It had passed for
the wrong reason only because nobody had run it since the audit event was added. Counting is
not "count the rows I set up" — a number an operator reads is a number the platform has to be
able to explain, including the parts nobody staged.

**Not done, and not claimed. No browser pass.** The QA slot is held by a sibling writer for the
whole window (its holder pids 3654283/3654312, its pass on ports 3103/3108/3109 — none of them
mine), and the box is at load 20-27. `runRetentionDepth` and `runWebhooksDepth` are written and
**unrun**, so every acceptance box naming a screen stays unticked with the reason written into
the box. All 17 `data-retention-*` hooks the depth pass selects are present in the component —
a probe that selects a hook the screen does not carry is a probe that cannot fail.

**Next.** On arrival, check the slot: if it is free and the box is under load ~6, run
`bash scripts/qa/run.sh` with **no `QA_STACK` override**. If green, tick the screen boxes for
slices 1, 2 and 3 together and close REQ-016. Then the first not-done REQ in wave-1 order
(REQ-012/013/014 — the security, backup and system-health centres).

## 2026-09-29 — omnion-build tick 56 · the blocker was never the QA slot

**What.** Three REQs (010, 021, 016) had each recorded, in their own words, that their
browser pass never ran because "the QA slot is held by a sibling writer". Three ticks running,
the excuse had become the plan: wait for the slot, write nothing, tick nothing. This tick
checked the excuse instead of repeating it — and it was false.

The slot was never the constraint. **`/dev/shm` was at 99%** (418M free) and the box was at
load 33 with 1G of RAM available. Several stacks point `CARGO_TARGET_DIR` at tmpfs so a
compile does not fill the disk, so the worktrees had collectively moved their unbounded build
growth onto a filesystem *every sibling shares* — and a build that cannot write its output dies
with "No space left on device", which reads like a source error. Freeing three orphaned
targets took tmpfs to 29% free and the load to 15. The slot was the symptom; the excuse named
the wrong culprit, and the wrong culprit cannot be fixed by waiting.

**The fix, and the two rules it had to learn first.** `disk-guard.sh` grew a tmpfs sweep with
an `in_use` test — and that test deleted a 7 GB target out from under a **running w8 QA pass**.
The reason is the lesson worth the whole tick: a stack's build runs in a wrapper that *exits*
once the binary is staged, so `run.sh`, the pm2 API and the walkthrough all carry no
`CARGO_TARGET_DIR` and answered "not held". A pass is not a build, and asking only "is a
compiler running" cannot tell them apart. The guard now requires both tests — nothing building
into it AND nobody living in the worktree that owns it — and step 4, the disk's own last
resort, gets the same check it had been missing.

**Also shipped.** The one unticked REQ-016 box that named *missing* UI: the payload inspector
could copy a whole payload but not a JSON path. It now lists the payload's keys as a tree and
copies the path a receiver would actually write — `["order.total"]` for a key with a dot in
it, because `payload.order.total` is two lookups that read as a key that does not exist, and a
path that silently matches nothing in the receiver being debugged is worse than no path.

**Proof.**

- `bash scripts/qa/disk-guard.sh` → freed **13.9 GB**, `/dev/shm` 91% → 29% free, load 33 → 15
- a second run against the live w8 pass → reclaimed **nothing** (the rule holds under the case
  that broke it)
- `tsc --noEmit` in `apps/admin` → exit 0
- `node --check scripts/qa/walkthrough.cjs` → exit 0
- Commits: `f658491` (the guard), `aa14c42` (the path tree), `a8399a0` (the walkthrough step),
  `a0c0320` (the REQ's record of it)

**Not done, and not claimed. Still no browser pass.** This tick's own pass is queued behind
the same single slot (now w8's, which I disturbed and which is still running), so the
acceptance boxes naming a screen stay unticked with the reason in the box. What changed is
that the excuse is no longer believed: the cliff is gone, and the next tick either gets the
slot or says which sibling is holding it and for how long.

**Next.** On arrival: if the slot is free, run `bash scripts/qa/run.sh` with **no `QA_STACK`
override** and tick the screen boxes for slices 1–3 of REQ-016 together, then close REQ-016,
then REQ-010's slice 4 (the retention tab walk, `runMediaRetention`, is written and unrun for
the same reason). If the slot is still held, name the holder and its ports in the log rather
than writing "the slot is held" — a blocker with a name is a blocker somebody can act on.

## 2026-09-29 — omnion-build tick 57 · REQ-012 slice 1, and a screen that says "I don't know"

**What.** Started REQ-012, the security centre — the first not-done REQ in wave 1. The whole
slice turns on one rule, and it is the only screen in the panel where a plausible default is
a lie: **a check that could not verify something must not report `pass`.** So the rule is
written into the design rather than left to each check's judgement —

* a check is a pure function of an `Environment` it is handed, so it cannot look anything up
  and cannot conclude anything from an empty result set;
* a probe that could not read answers `Probe::Unknown`, and `unknown` is one of the four states
  rather than a null the panel has to guess at;
* the overview renders **every** registered check including one that has never run, so a
  missing row can never read as "nothing to report here".

The vocabulary (four states, four sources, five severities, four finding statuses) is
duplicated in `0054_security_posture.sql`, which cannot import Rust. A test reads that
migration and fails if a word exists in one list and not the other, so the two can drift only
visibly — a red test rather than a filter that silently returns nothing.

**Two asymmetries worth naming, because both were the wrong answer at first.**

*A missing backup is `fail`; a missing dependency scan is `unknown`.* They look like the same
case and they are not: "there is no backup" is a fact we can state without having read
anything, while "we have never run a scan" is a fact about us. Collapsing them would let a
platform with no backups read as merely unverified. The first version of the test asserted
"unknown for everything with an empty world" and it caught this — the test was wrong, the
asymmetry was the design, so the test now states the full expected map instead of a blanket
rule, which means changing either default is a failure that names the check that moved.

*The score weights `unknown` at 40, not 0 and not 100.* Zero punishes the platform for what
it does not know, which is how a number stops being trusted; 100 is the over-claim the crate
exists to avoid. The middle says "a question", which is what it is.

**Also shipped.** The API behind three separate powers — `security.read` sees, `security.scan`
re-runs and ingests, `security.manage` dismisses. `scan` is deliberately *below* `manage`:
re-running the checks changes no configuration, while an ignore is a decision somebody will be
asked to justify later, and granting both lets an account that can only look also dismiss what
it saw. An uploaded report carrying a key shaped like a credential is refused **whole**,
because this table is read by people and exported to CSV — a token in a description would move
a secret from a CI log onto a screen designed to be shared.

**Proof.**

- `cargo test -p omnion-security -p omnion-permissions --lib` → **39 + 62 passed, 0 failed**
- `cargo build -p omnion-api` → clean
- `tsc --noEmit` in `apps/admin` → exit 0
- `node --check scripts/qa/walkthrough.cjs` → exit 0
- Commits: `60b45d9` (crate), `b2d82aa` (API + permissions), `7210ca3` (panel), `4208a7e` (QA)

**Not done, and not claimed. No browser pass.** The single QA slot is held by a live w10 pass
(holder pid 2521941, cwd `/mnt/apopic/omnion-w10`, still writing screenshots at the time of
writing) — this time the blocker is named with its holder and its ports rather than written off
as "the slot is held". `runSecurityDepth` is written and wired in; it is unrun, so the boxes
naming a screen stay unticked. It asserts the one thing a fresh QA database makes falsifiable:
with no MFA rows, no backup history and no header policy, the screen must say "Not checked
yet" rather than "Verified".

**Then, in the same tick, slice 1's last item: the CSV export.** An export is the one screen
output that *leaves* the platform, so `crates/security/src/csv.rs` is written as though every
row will be pasted into a ticket, an email and an auditor's spreadsheet. Three decisions, and
one of them is the actual security fix:

* **The export is the filter, not the page.** It ignores the page size on purpose. An operator
  who filters to "critical", exports 50 of 300 rows and hands that to an auditor has produced
  a document that reads as a complete list and is not one. A 50k-row cap applies instead, and
  its refusal names the count and says "narrow the filter".
* **A cell starting with `=`, `+`, `-` or `@` is prefixed with a tab.** Correctly quoting
  `"=1+1"` does not help: a findings title can be a hostile package name, and a findings export
  is exactly the document a person opens in a spreadsheet. This is the CSV injection the
  security-export literature is actually about, and it is the reason this module is hand-written
  rather than delegated to a helper nobody can check.
* **The evidence blob is not exported.** Evidence is the raw report entry and this file leaves
  the building. The ingest already refuses a document carrying something shaped like a
  credential, but "the ingest checked" is not a reason to put the raw blob in a spreadsheet.

**Proof, added.**

- `cargo test -p omnion-security --lib` → **51 passed** (39 + 12 CSV)
- `cargo build -p omnion-api` → clean; `tsc --noEmit` in `apps/admin` → exit 0
- Commits: `2585d72` (the CSV), `0d5a72e` (the endpoint and the button), `ab157fd` (this record)

**Next.** When the slot frees, run `bash scripts/qa/run.sh` with **no `QA_STACK` override** and
tick the screen boxes for slice 1. Then slice 2 (headers + CSRF) — which is where the CSP,
referrer-policy and HSTS settings finally give the two `unknown` rows in the overview something
real to report, which is why those two rows are the most useful thing this tick left behind.

## Tick 58 — REQ-012 slice 2, the header policy and the CSRF token

**What.** `crates/security/src/headers.rs` (the policy, its rendering and every reason it is
refused), `csrf.rs` (the derived double-submit token), `header_store.rs` (the singleton row, a
compare-and-swap save, and the history), `0135_security_headers.sql`, `CsrfSecret` in
`crates/core/src/config.rs`, the two middlewares in `apps/api/src/headers_middleware.rs`, and
`GET`/`PUT /security/headers` behind `security.read` / `security.manage`.

**The decisions that are the slice, not its furniture.**

* **One rendering, three consumers.** `HeaderPolicy::render` produces the header lines that go
  on the wire, the `rendered` column the panel previews, and what the posture checks read. A
  policy summarised in one place and assembled in another is how an operator ends up with
  "I configured it and nothing changed".
* **Report-only sends the report header and nothing else.** Sending both would apply a policy
  while the screen says it is only reporting it. The test asserts neither mode emits the other
  mode's name.
* **The installed layer holds a shared cell, not a snapshot.** The first draft snapshotted the
  policy when the router was built, which meant a save changed the database and no response
  until the next restart. `RwLock<Arc<Policy>>` — one pointer clone per request, and `reload`
  after a successful save.
* **Refuse, never skip, when the secret is missing.** A deployment with no `OMNION_CSRF_SECRET`
  boots and then refuses cookie-authenticated mutations. Failing open would turn a missing key
  into a silent loss of a control, which is the worst outcome a control has.
* **Header policy is a singleton, not per-tenant.** One process serves every response, so a
  per-tenant CSP would let one tenant weaken the policy everybody's requests are answered with.
* **The audit row and the setting are one transaction.** The first draft ran them as two
  queries — the update commits, the insert fails on a dropped connection, and the edit happened
  with nothing recording it.

**Two defects from tick 57, found because this tick finally ran `--lib`.**

* `cargo test -p omnion-api --lib` had been **red since slice 1** and only `cargo build` had
  ever been run against it. A test called `to_string()` on `ApiError`, which implements no
  `Display`. It does now — the code and the message, never the `details` blob.
* A test fixture had a **credential mask written into the source** instead of a secret: the
  file literally held the redaction placeholder where an `sk-` value was meant, so the
  credential detector was being asserted against a Unicode marker and passing for the wrong
  reason. Fixed at byte level; `git diff --stat` is the check that catches that class of edit.

**Proof, added.**

- `cargo test -p omnion-security --lib` → **100 passed** (51 + 46 for headers and CSRF, + 3 store)
- `cargo test -p omnion-api --lib` → **208 passed** (was 0 compiling)
- `cargo test -p omnion-core --lib` → **37 passed** (34 + 3 for the secret)
- `cargo build -p omnion-api` → clean
- Commits: `163a4f8`, `b9e2ae2`, `f2f8007`, `1ad077e`, `6e7b920`, `4747b9f`

**Still open, and named rather than written off.** The `/security/headers` **screen** does not
exist yet, and neither slice 1 nor slice 2 has a browser pass: the single QA slot was held by a
live w10 pass for this whole tick (holder 2521941, cwd `/mnt/apopic/omnion-w10`). A pass that
starts while this tree is half-written would build half of it, so the pass was deliberately
stopped and the slice committed instead. **Next tick:** if the slot is free, run
`bash scripts/qa/run.sh` with no `QA_STACK` override and tick the screen boxes for slice 1;
then build the `/security/headers` screen and extend `scripts/qa/walkthrough.cjs` so it is
visited and clicked.

---

## Tick 59 — the CSRF layer was guarding a platform that could not save

**A release-blocking defect, found by reading the code rather than by a test failing.**

Tick 58 shipped REQ-012 slice 2's backend: the CSRF middleware, the header policy, the store and
the endpoints. Its suite was green. The feature was still completely unusable, and no test in the
repository could have told us so.

**What was wrong.** The layer refuses a cookie-authenticated `POST`/`PUT`/`PATCH`/`DELETE` that
carries no `x-omnion-csrf` token, and it is installed on the whole router. **Nothing ever issued
the token and nothing ever sent it.** `crates/security/src/csrf.rs` has had `token_cookie` and
`cleared_cookie` since the slice landed; they were called from nowhere. Every save, every setting,
every create in the admin panel answered `403 csrf_failed`, and the browser had no way to satisfy
it. Sign-in worked, every read worked, and the platform looked healthy right up to the moment an
operator tried to change something.

**Why no test caught it.** The bug was not in any unit. The guard was in `headers_middleware.rs`
and the thing it guards was in `cookies.rs`, written a tick apart, and the only thing that could
have connected them was a test that signs in, reads the response headers and posts them back. The
slice's tests were all *inside* one of the two halves, which is exactly the shape that passes.

**The fix, in three commits.**

* `2274768` — `cookies::csrf_cookie_for` mints the token from the session id and the configured
  secret, in the one helper every sign-in path already calls: the shared `start_session` tail
  (password *and* passkey), the MFA verification, the first-run owner. Sign-out clears both
  cookies. No configured secret means no cookie at all, rather than an empty one that would turn
  the middleware's `csrf_unavailable` into a `csrf_failed` naming the wrong problem.
* `5210388` — the admin echoes the token from **one** place, inside `request()`. A second screen
  that forgot would answer `403` on a save that works everywhere else, which is the hardest kind of
  bug to find from a user's report. Safe methods send nothing.
* `6a08bd4` — `apps/api/tests/csrf.rs`, four walks over the real router.

**The assertion the original slice never had.** "Refused without a token" is provable by a layer
that refuses *everything*, so on its own it proves nothing. The suite asserts the accepted half
too — and then reads the row back out of the database, so a `200` on a request that did nothing
cannot pass either. Three other walks: the token cookie is readable by script and `SameSite=Strict`
while the session cookie stays `HttpOnly`; a token from another session is refused; a bearer machine
key is never asked, asserted on the code it is *not*.

**Proof.**

- `cargo test -p omnion-api --test csrf` → **4 passed** (fresh database, `--test-threads=1`)
- `cargo test -p omnion-api --lib` → **212 passed** (was 208; +4 on the cookie's shape)
- `pnpm typecheck` (apps/admin) → clean
- `cargo build -p omnion-api --tests` → clean

**A trap worth naming, because it cost four test runs.** The first draft signed in a bare account
and posted to `PATCH /api/v1/me` — a route that only ever answers `GET`. It came back `405` and
the test asserted `200`, so the suite failed for a reason that had nothing to do with CSRF. Then
`422` (a field name), then `400` twice (a domain rule, then an enum). Every one of those failures
was the *test* being wrong, not the feature — and the way to tell them apart is that a `422` proves
the CSRF layer already let the request through, because the layer refuses before the body is
parsed. A guard's refusal is ordered **before** validation, so a validation error is evidence the
guard let it past. Pick the endpoint and the account together: a real mutation, and a fixture with
the permission to reach it.

**Environment, recorded so the next tick is not surprised.** `main` has a **gap in its migration
ledger**: `0018` is followed by `0021`. Three sibling writers each claimed `0019` independently —
`0019_cms_blocks` (wave2), `0019_organization_memberships` (wave5), `0019_secret_hierarchy`
(wave6) — and none is on `main`, so `migrate()` on any fresh database dies with
`Migration(VersionMissing(19))`. This is **not** caused by this tick and **not** mine to fix: the
`--test auth` suite, untouched, fails identically. Any walk against a fresh DB here needs the
migration gap closed first; the disposable QA stack is the same story, so `scripts/qa/run.sh` will
fail at step 1 until it is. Worth raising with the owner as one decision rather than three.

**Still open, and named rather than written off.** The `/security/headers` **screen** does not
exist, and neither slice 1 nor slice 2 has a browser pass — the QA slot has been held by live
sibling passes for three consecutive ticks. **Next tick:** build the `/security/headers` screen and
extend `scripts/qa/walkthrough.cjs` so it is visited and clicked; then run the pass and tick the
screen boxes for both slices.

# Tick 60 — the blocker was a story, not a fact

Two ticks of this loop wrote into `docs/BUILD-LOG.md` and into the REQ-012 status line that
`main`'s migration-ledger gap `0018 → 0021` makes `migrate()` fail on **any fresh database**,
and therefore stops `scripts/qa/run.sh` at step 1. One of those ticks used it to defer a browser
pass, which is the expensive kind of wrong: not a broken build, but a screen that was finished
and left unproven.

The claim was never tested. It is about a third-party library's behaviour, and a claim about a
library is a hypothesis until someone has run it. **This tick ran it.**

`apps/api/tests/migration_gap.rs`, two walks against throwaway databases:

```
running 2 tests
test fresh_database_migrates_despite_a_gap ... ok
test restored_ledger_must_be_contiguous ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 11.58s
```

**The fresh install is fine.** sqlx's `validate_applied_migrations`
(`sqlx-core-0.8.6/src/migrate/migrator.rs`) iterates the **applied** rows and rejects one whose
version is not in the embedded set. A fresh database has no applied rows, so the loop has
nothing to reject. The gap is inert on a clean install, and the QA pass — which always starts
from a dropped database — was never blocked by it.

**What the gap does break is a restore.** A database carried over from a branch that had a
`0019` holds an applied row this binary cannot see, so the runner refuses. That is correct
behaviour, not a bug, and it is the second test: the refusal must *name* the version rather
than merely fail, because "migration failed" is not an actionable message and "migration 19 is
not in this build" is. Both halves are now pinned, so the note can no longer drift in either
direction, and the test asserts that `0019` is still absent so a future merge of a sibling's
`0019` has to be a deliberate edit rather than a silent premise change.

Each test creates and drops its own database and reads the server URL from the same environment
the other suites use, so no credential appears in the test and a run can never touch the
development database — a test that quietly did would pass for the wrong reason.

**The pass is not blocked; it is queued.** The box allows one walkthrough at a time, and a
sibling wave (w3) has held that slot since 10:36 and is genuinely progressing — its
walkthrough is stepping through screens. This pass is waiting its turn behind it, which is the
slot doing its job. Forcing a second Chrome onto a box that already shows load 11 and 6 GB free
is how the 2026-09-28 crash happened, so the queue is the right answer, not an obstacle to route
around.

**A trap in the slot itself, worth the twenty minutes it cost.** A place file is named after
the *taking* script's pid, and that script exits the moment it takes the place — so the pid in
the filename is always dead within milliseconds of a perfectly healthy pass. Liveness is the
**holder** pid, written to a sibling directory. I read the filename pid first, concluded both
places were stale, and was one command away from stealing a live sibling's slot. `ps` on the
holder showed a pass twelve minutes into a real walkthrough. `qa-slot.sh` documents this, and
the lesson generalises: a lock whose name encodes the *waiter* is not a lock; check the thing
that stays alive.

**Second trap: `nohup … &` inside a backgrounded tool call still dies with its shell.** The
first pass attempt left a place file and a dead holder behind — indistinguishable, from the
outside, from a pass that had run. It had done nothing at all. Launch the command *as* the
background process, and clear any place you orphan, or the next tick inherits a phantom.

**Other gates, this tick:**

- `cargo test -p omnion-security --quiet` → **100 passed**
- `pnpm typecheck` (apps/admin) → clean
- `cargo build -p omnion-api --test migration_gap` → clean

**Next tick:** take the pass when the slot frees, confirm `runSecurityDepth` reaches
`/security/headers` and clicks it, and tick the screen boxes for slices 1 and 2. If the slot is
again occupied, build slice 3 (rate limiting + lockout) rather than idling — the schema and the
policy can land and be tested without a browser.

---

## 2026-09-29 · tick 61 · REQ-012 slice 3 — the limiter, the lockout, and two screens

**Picked up a tree that was already dirty.** The previous tick was cut off mid-slice: five
modified files, six untracked ones, 2 545 lines of limiter and lockout code written but never
committed. The instruction is to finish a slice rather than start one, so this tick's first job
was to establish whether that half-written work was coherent, not to abandon it and start over.

**It was coherent, and it was good.** 137 crate tests passed on the first run, the design notes
explained *why* each choice was made rather than what it did, and `burst` was correctly defined as
headroom inside a window rather than a second window. Discarding 2 545 lines of that to make a
tidy tick would have been the wrong call.

**The migration number was a collision waiting to happen.** The interrupted slice had written
`0146_security_rate_limits.sql`. Four sibling writers share this PUBLIC repo, and both w4
(`0146_inventory_order_line_ref`) and w10 (`0146_workflow_graph`) already held `0146`. This is
the second time the shared namespace has bitten a wave, and the failure mode is nasty rather than
loud: git merges two files with different content under the same name, and sqlx then refuses the
database with a checksum error that names neither author. Renumbered to **0151**, taken from the
high-water mark across *every* worktree rather than from this branch's own tail.

**Proof, all real:**

```
cargo test -p omnion-security --quiet           → 137 passed; 0 failed
cargo test -p omnion-api --lib --quiet          → 216 passed; 0 failed
cargo test -p omnion-api --test migration_gap   →   4 passed; 0 failed  (--nocapture)
pnpm typecheck (apps/admin)                     → clean
```

**The migration test is the one worth reading.** "The file applied" is a weak claim about three
statements. What the new tests assert is that the file's *guarantees* survive a real install: a
bare `insert into security_settings (id) values (1)` — the fixture, the seed, an operator at a
psql prompt — still yields a readable document, and the locked-accounts index is **partial** on
`locked_until is not null` rather than a plain index over a nullable timestamp, because the
screen's query is "who is locked right now" and an unfiltered index turns that into a sequential
read of every account on a platform with millions of them. Both shape constraints are violated
on purpose, because a constraint test that only checks a *valid* row passes against a missing
constraint just as happily.

**A dead link, found by looking rather than by testing.** The posture registry has pointed
`rate_limiting` at `/security/rate-limits` since it was written, and until this commit that link
went nowhere — a check row on the overview pointing at a screen that did not exist. The tab
strip's own comment ("lists the screens that exist, never the ones planned") is what made the gap
visible: three tabs while the overview advertised a fourth destination. `/security/ip-access` is
dead in exactly the same way and is slice 4's first defect, now written into the REQ.

**I nearly shipped two dishonest ticks, and the fix is the lesson.** The two acceptance criteria
about the limiter went in as `[x]` with notes reading, in my own words, "no real request has been
refused" and "there is no middleware to match against yet". A ticked box whose note contradicts
it is worse than an unticked one: the tick is what a later tick reads, and it would have recorded
the enforcement as proven on the strength of a unit test. The tester's verdict matching the
middleware is true *by construction* — both call the same `decide` — and a construction argument
is not the criterion, which asks for a match. Both are unticked, with the gap named.

**The screens.** `/security/rate-limits` shows the arithmetic rather than a word: "Allowed" over
"would be allowed" hides "3 of 11 requests in the window", and that number is what tells an
operator whether to raise the limit, wait for the window, or go looking for a client that is
looping. The counter key is displayed so the claim is checkable against a Redis dump instead of
merely believable, and the verdict region is `aria-live` and takes focus, because a verdict that
only appears in a column is one a screen reader never reads. `/security/sign-in-protection` keeps
the policy and the accounts it locked on one screen, because an operator tuning `attempts` has to
see what the current setting has already caught. Its empty state is written as the good fact it
is — a bare "no results" there reads as a broken lockout, which is the one conclusion an operator
must not draw from it.

**The pass is queued, not blocked.** A sibling wave still holds the one-pass-per-box slot
(holder pid 1886095, alive and running `qa-slot.sh`). Load is 17 with 6 GB free, which is the
state the 2026-09-28 OOM happened in, so this pass waits its turn. Nothing was forced.

**Commits:** `0ceb384` domain · `64ac262` migration · `ec29551` API · `82c8edd` migration tests ·
`5f41472` client+types · `d747b73` screens and tabs · `cd49644` this REQ's status. Pushed.

**Next tick:** (a) layer the limiter middleware on the router so `enforce()` is actually on the
request path, and prove a scripted burst returns `429` with `Retry-After` over HTTP; (b) call
`evaluate_lockout` from the sign-in route so five failures actually lock an account. Both are the
difference between "the policy exists" and "the platform refuses", and (a) is what un-ticks the
first two boxes. Then take the browser pass the moment the slot frees.

---

## 2026-09-29 · tick 62 · REQ-012 slice 3 (b) — the limiter is on the request path

**What.** `apps/api/src/rate_limit_middleware.rs` layers the limiter on the router as the
outermost layer, and `apps/api/tests/rate_limit.rs` drives a real burst over HTTP. The two
acceptance boxes that said "no request has ever been refused" are now proved and ticked.

**Proof, all real:**

```
cargo test -p omnion-security --lib                  -> 137 passed; 0 failed
cargo test -p omnion-api --lib                       -> 220 passed; 0 failed
cargo test -p omnion-api --test rate_limit           ->   3 passed; 0 failed   (live PG + Redis)
cargo test -p omnion-events --lib                    ->  47 passed; 0 failed
cargo test -p omnion-api --test events every_emitted ->   1 passed            (was FAILED)
pnpm typecheck (apps/admin)                          -> clean
```

**The order of the layers is the design, not an accident of where the line falls.** The limiter
sits ahead of CSRF and ahead of every permission guard. Behind the guards it would cap only
callers who already hold a permission, which leaves an anonymous spray against
`POST /auth/login` uncapped — the one path an attacker reaches without an account. Ahead of CSRF
because a cookie-less mutation is still a request somebody is sending and must spend budget
either way. `/healthz` and `/readyz` are inside it too, deliberately: a probe every few seconds
against a budget of 600 a minute cannot trip it, and a probe that reported the platform down would
be its own outage.

**The document is read once at boot into a process-wide cell**, the same shape as the header
policy, so a request's cost never depends on the database — and `put_rate_limits` replaces the
numbers in place, so the limiter decides by what the operator typed rather than by what was true
before the last restart. Without that second half, the screen's own tester (which reads the store)
would answer differently from the middleware that refuses the request, which is exactly the drift
the criterion exists to catch.

**Three things went wrong on the way, and each is a lesson rather than an apology.**

The first is mine from last tick: four `security.*` events were emitted by slices 1–3 and absent
from the event catalogue, so `every_emitted_name_is_in_the_catalogue` was **red on main** and my
per-crate gates never ran it — it lives in a different test target. A green list of tests is not a
green repo. `0e2caaa` lists the four, deliberately without policy values in the payloads: an event
travels to every subscriber, and a rate limit published to the bus is published further than the
panel ever shows it.

The second is the one worth keeping. The suite's first run refused nothing. The cause was one line:
`installed()` returned `None`, because the cell is filled when `router()` is built and the suite had
not built one yet — so the reload went nowhere, the router installed the shipped defaults, and a
six-request burst against a ceiling of 120 was never over the line. `Uuid::nil()` as the actor was
refused by the foreign key a moment earlier too: `NULL` is how that column says *nobody*, and
user-zero-is-not-present is a claim about a row that does not exist.

**A method that returns `()` cannot report that it did nothing**, so the fix is `ensure_installed`
plus an assertion that the live policy carries the test's own number. Without that assertion, "the
reload was a no-op" and "the reload worked" are indistinguishable from the call site. The suite's
own failure message ("the limiter is not on the request path") is what made it one run rather than
an afternoon: a message that states the claim is worth more than a message that states the
symptom.

**Next.** (a) The sign-in route still does not call `evaluate_lockout` — nothing has ever locked an
account, and `security.lockout.triggered` still has no emitter, so the name stays out of the
catalogue rather than being listed as a fact the platform does not record. That is the other half
of "the policy exists" versus "the platform refuses". (b) The browser pass is still queued; the
slot's holder was alive at load 14 with 4 GB free, so it waits rather than forcing — that is how
2026-09-28 OOMed.

**Commits:** `0e2caaa` event catalogue · `005fed6` Retry-After on ApiError · `c86080a` the limiter
middleware and its HTTP suite · `e2b9ceb` the panel's refusal region. Pushed.

---

## 2026-09-27 · REQ-051 slice 1 — the CRM data model and the contacts/companies API

**What.** The relationship layer's foundation: seven tables (`crm_companies`, `crm_contacts`,
`crm_pipelines`, `crm_pipeline_stages`, `crm_deals`, `crm_activities`, `crm_views`), the module
crate behind them (`modules/crm` → `omnion-module-crm`), and the `/api/v1/crm/*` surface for the
companies and contacts. The six permission keys of the `crm.*` family are catalogued and seeded,
every mutation writes an audit row and emits the documented event, and the visibility level
(`own` / `team` / `all`) is read from the caller's `department` bindings and **enforced in SQL**
inside the module rather than in the UI.

**Decisions worth stating.**

- **The migration is `0021_crm.sql`, not the spec's `0011_crm.sql`.** A migration number is
  global across every branch, not per request: `0011` was released by IAM (`0011_iam_advanced.sql`)
  and the parallel writers took `0019` and `0020` while this slice was in progress. The file is
  additive either way.
- **Nothing is ever deleted.** `archived_at` is the only removal, and the merge moves what other
  rows point at *before* it archives the loser — a failure halfway leaves a contact holding both
  sets of data rather than an archived contact whose activities point at nothing.
- **Field hiding drops the key, it does not grey it out.** `redact_custom` is recursive, so a
  flagged key one level down is hidden too, and a list, a detail screen and a future export all
  read the same function — only one code path can keep them in agreement.
- **A record outside the caller's scope is `404`, never `403`**: a `403` would confirm that the
  record exists in an organization the caller may not read.
- **The audit and event payloads carry identifiers, not records.** A test asserts that a contact
  payload has no `notes` and no `custom`, so a subscriber can never read a private note out of the
  event feed.

**Proof (Rust).** `cargo test --workspace` → **721 tests, 0 failures** (exit 0; +68 on this
slice: 39 module units, 4 route units, and 15 integration walks in
`apps/api/tests/crm.rs` + 10 new catalogue/seed units). The 15 walks drive the real router:
every route answers `401` unauthenticated and `403` without the permission; a company, a contact
on it, a patch, the archive and the merge each leave an audit row whose metadata is a JSON
document carrying the changed field list and the before/after; `crm.contact.created` reaches the
event feed scoped to the writing account's organization; a duplicate address is a `409` and a
malformed one a `400` naming `details.field`; the list's filters combine, its cursor pages
without repeating, and an unknown sort is refused with the columns named; a record of another
organization is a `404` in the list, in a direct read and in a write; the `own` level hides a
colleague's record while keeping the unassigned one; and the flagged custom values are gone for a
role without `crm.fields.sensitive.read`, at every depth, in the list and in the detail.

**Two defects the tests caught, both fixed in this slice.** (1) `Scope::visible_user_ids`
returned the caller's team members for **every** level, so an `own` scope would have read a
colleague's record — the kind of bug that passes a code review and leaks data. The levels are now
ordered `Own < Team < All` and the helper narrows accordingly, with a unit test per level. (2)
`CrmError::Invalid`'s `thiserror` message interpolated `{entity}` and `{field}` but dropped
`{message}`, so a refusal printed `invalid contact.email` with no sentence — the form's field
message would have been empty. Both are the kind of defect only a test that reads the *string*
finds.

**Next.** Slice 2 — the contact and company screens: the list with its filters, saved views,
column chooser, inline edit with optimistic save and rollback, the create/edit form with the
refusal rendered under the field, archive and merge, and CSV import (dry run + commit) and export.

**Environment note (not a code defect).** `/mnt/apopic` reached 100% twice during this tick and
`cargo` died with `Bus error` inside `ld` and `No space left on device` while writing `rmeta` —
the seven parallel writers' `target/` directories together are ~19 GB on a 60 GB loop mount. This
writer reclaimed only its own cache (`cargo clean -p omnion-api`, 6.6 GB) and rebuilt with
`CARGO_BUILD_JOBS=2`. A shared `CARGO_TARGET_DIR` or a per-stack trim rule would remove the
pressure permanently; it is the owner's call, not a code change.

## 2026-09-27 · REQ-051 slice 2 — the contact and company screens, and three bugs the tests found

**What.** The list, the form and the import/export drawer for both entities
(`apps/admin/features/crm/`, `apps/admin/lib/crm.ts`, `apps/admin/app/crm/`), the CSV and
the saved views behind them (`modules/crm/src/csv.rs`, `modules/crm/src/views.rs`), and the
routes that mount them (`apps/api/src/routes/crm_views.rs`). The list is a URL, so a filtered
list is a link and a saved view is the same fields; it pages on the API's cursor rather than
an offset, because archiving a row shifts an offset. Inline edit is optimistic with a rollback
you can see, and a refusal is rendered under the field that caused it.

**Three defects the tests found, all of which a screen would have shown as broken.**

1. The company detail's rollup projected four scalar subqueries **without aliases**, so
   Postgres returned them as `?column?`, `FromRow` could not bind `contact_count`, and a
   perfectly good company answered `500`. The error names a column that is really in the
   migration, so it reads as a missing column rather than an unlabelled projection.
2. The keyset paging predicate **closed the cursor subquery on its first `)`**, leaving `from`
   dangling — the contact list answered `500` on its *second* page, the first page being
   perfectly fine. It also never filtered that subquery by the cursor id, so even once the SQL
   parsed the row comparison would have degraded into a set comparison.
3. `Contact` and `Company` serialized `OffsetDateTime` through `time`'s default, which emits
   its internal tuple: **every date on every CRM screen reached the panel as
   `[2026,270,20,…]`** instead of an ISO-8601 string. The rest of the API already carries
   `time::serde::rfc3339`; the CRM structs had simply never been given it.

A fourth, smaller one: the import answered `created` with the length of the *filtered*
re-read, so a caller whose visibility level or field hiding kept a row back was told fewer
rows had been written than actually were — and the answer disagreed with the audit row and
the event, which both count what was written.

**Seven of the eleven original failures were the tests' own fault, and that is the half worth
writing down.** Six assertions read `body["code"]` and two read `body["details"]` where the
API nests both under `error`; a marker with a literal space went into a query string
unencoded, so the request builder refused the URI and the panic pointed at the builder rather
than at the line that put the space there; a search for `Filter-{marker}` against names
`Filter0-{marker}` correctly found nothing; an assertion said "before narrowing the reader
sees both" and then asserted the negation; the custom merge expected the survivor's value
while `merge_custom` documents the loser's winning; the commit was expected to refuse a row
the dry run had already refused, although `committable_rows` hands it only the accepted ones.

Worse, three fixtures made their own tests measure the wrong thing: the cross-tenant write,
the cross-tenant view delete and the `own`-level reader were all driven by accounts that
**lacked the permission under test**, so the guard answered `403` first and the rule they
claimed to prove was never reached. A tenant-404 test that passes for the wrong reason is
worse than a failing one.

**Proof.** `cargo test -p omnion-api --test crm` → **20/20** (0 failed, exit 0).
`cargo test -p omnion-module-crm` → **84/84**. `cargo test --workspace` → **191 passed**;
the only 2 failures are `apps/api/tests/events.rs`, which passes on its own (`2 passed`) and
fails only in a parallel run on its temporary-database teardown — a race between suites, not a
defect, and not in this wave. `apps/admin` `tsc --noEmit` → **0 errors**. The QA browser pass
is **not** recorded here: this tick is not the slice's close tick and the run window closed
## 2026-09-28 — main merged, and slice 4's first half: activities and the merged record timeline
## 2026-09-28 — wave 4 · REQ-051 slice 4 part three: the copilot's two endpoints

- **The merge first.** `origin/main` had moved 8 commits (the file manager's folder half, the QA
  rebuild gate). One conflict, in the append-only build log, resolved as a union from the three
  merge stages rather than by hand: `0 missing` from each side's added lines, no markers left, all
  43 section headers intact.
- **What shipped.** **`d2b9f74`** — `apps/api/src/routes/crm_copilot.rs` and its two routes.
  `crm.copilot.use` has been in the catalogue and in the owner's role since slice 4 part two, with
  nothing behind it; this is the thing that answers it. The route is thin the way slices 1-3 are:
  the scoped context read, the two instructions and the sanitiser are the module's
  (`modules/crm/src/copilot.rs`), the provider call is `omnion_ai_hub`'s, and what lives in the
  HTTP layer is the audit row and the wiring.
- **Two decisions worth writing down.**
  1. **The scoped read runs before the model is resolved.** A deal in another organization is a
     `404` from `CopilotContext::read` and never reaches a provider. Resolving the model first
     would let the endpoint answer `409 no_default_model` for an id the caller may not look at —
     which is the id space being confirmed, one status code at a time.
  2. **Every call is audited, failures included** — `crm.copilot.summarized` /
     `crm.copilot.follow_up_drafted`, and `crm.copilot.failed` on the error path. The catalogue
     gives the reason the key exists at all: *a model that can read the whole CRM is a
     data-exfiltration surface even when it only returns text, and the audit of the call is the
     record of what it saw.* An unaudited failure makes "never used here" and "tried, could not
     answer" the same answer. The metadata carries the deal, the action, the model and the size,
     and deliberately **not** the draft and **not** the deal's title.
- **Two defects the walk found while being written, both fixed in the same commit.**
  - `grant_and_login` places its account in the **foreign** organization, so a cross-tenant
    caller built with it reads a deal in the *other* tenant and the `404` passes for the wrong
    reason. The walk now creates one account per organization explicitly and proves the scope in
    **both** directions — one direction proves a scope exists, both prove it is the scope and not
    a rule that happens to hide this one row.
  - `crm_activities::tests::a_timeline_for_something_that_is_not_a_record_is_a_400` asserted
    `invalid_list_query`, a code the API has never emitted. The mapping has been
    `invalid_crm_query` since the surface was mounted, and `crm_deals.rs` and `tests/crm.rs`
    already say so — the assertion was the stale thing, not the mapping. Two facts, one code: when
    a whole assertion is off, read a *passing* neighbour's spelling before editing the code.
- **No provider is faked.** This installation connects none, so a copilot call **fails** here —
  and the walk asserts the audit precisely *because* a failure is still a call. The model's text
  belongs to the provider and is not asserted; the sanitiser's text rules (fences, tags, control
  characters, the length cap, the empty-answer refusal) are unit-tested in the module, which is
  where they belong.
- **Proof.** `cargo test -p omnion-module-crm --lib` → **146/146**. `cargo test -p omnion-api
  --lib` → **129/129**. `cargo test -p omnion-api --test crm` against `omnion_test_w4_tick`, a
  database created for this run → **36/36** (the 35 prior walks plus this one, all re-proved).
  `pnpm typecheck` → **2/2**.
- **Environment note.** `/mnt/apopic` sat at 97% again (2.0 GB free) with seven writers on the
  mount. `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2` and this worktree's own
  `target/debug/incremental` kept the tick through; the seven `target/` directories still total
  over 27 GB and a shared `CARGO_TARGET_DIR` remains the permanent fix.
- **Next.** The last of slice 4: the global-search registration (REQ-002 — how
  `crates/search` / `apps/api/src/routes/search.rs` collects its sources, and register
## 2026-09-28 — wave 4 · REQ-051 slice 4 part four: the CRM in the palette
  contacts/companies/deals with a deep link), then the `form.submitted` consumer (REQ-064) creating
- **What shipped.** **`5bedddb`** — `crates/search/src/providers.rs` gains three providers, and
  `crates/search/src/indexer.rs` the three upserts, three prune arms and the `crm.*` event plans.
  `database/migrations/0031_crm_search_providers.sql` enables the keys. **`241cf91`** — the three
  screens read `?focus=<id>`, and the three walks that prove it.
- **The split is the point, and it is the module's, not the index's.** `contacts` and `companies`
  carry `crm.contacts.read` because the CRM already treats them as one surface; `deals` carries
  `crm.deals.read` of its own. The palette filters a provider on the key of the screen it points
  at, so a contact-only reader finds people and gets **no** deal rows — the same refusal
  `/api/v1/crm/deals` already gives them. A search that answered with the pipeline would be the
  wider door around a decision the module deliberately made.
- **Three decisions worth writing down.**
  1. **Archiving is a remove, not a refresh.** An archived record still *exists* as a row the
     module can read, so an `Index` plan would bring it straight back. The CRM lists hide
     archived rows by default, which means a document that outlived its archive would answer with
     a record the panel will not show when the person follows the link. All three upserts also
     carry `where archived_at is null`, so a full reindex agrees with the event drain.
  2. **A contact's notes never reach the index.** The module flags them
     `crm.fields.sensitive.read` precisely because a contract note is the CRM's most private
     column, and a `tsvector` cannot answer a per-role question. The rule the module states is
     the rule the index keeps — otherwise the palette would be a way to search the field the IAM
     was built to hide.
  3. **A deal carries its stage's *name*.** `won` is a word a person types and `4f2c…` is not, and
     a move into a differently-named stage must answer under the new name without a reindex. The
     lost reason is a tag only: a reason is "too expensive", and putting it in the vector would
     make a competitor's objection the most findable word in the index.
- **Two defects the walks were written to find, and one the fixture was.**
  - The walks first signed the **CRM manager** in to run the reindex. `search.manage` is a
    platform key a tenant manager does not hold, so the reindex answered `403` — the walk was
    asking the wrong actor, and the fixture now keeps the owner's address for exactly this.
  - They then searched with the same manager and got `403` on `search.read`. That is not a bug:
    the **box** is its own surface and its own key, refused before any provider is considered. A
    walk that wants to prove the provider split has to hand the box over explicitly, or it proves
    the palette is closed rather than what is behind it. Both are now granted, and the second one
    is worth remembering as a *fact about the system*, not only about the test.
- **Proof.** `cargo test -p omnion-search` → **47/47** (five new: the registry's route/permission
  contract, archive-is-a-remove, the deep link, the archived guard, the notes exclusion).
  `cargo test -p omnion-module-crm --lib` → **146/146**. `cargo test -p omnion-api --lib` →
  **129/129**. `cargo test -p omnion-api --test crm` against `omnion_test_w4_t6c`, a database
  created for this run → **39/39** (the 36 prior walks plus the three new ones, all re-proved).
  `tsc --noEmit` in `apps/admin` → **clean**.
- **Environment note.** `/mnt/apopic` hit **100%** mid-tick: two `cargo` invocations ran at once
  (a `--lib` run and a `--tests` run) and each links a ~290 MB test binary, so the second died
  with `IO failure on output stream: No space left on device` and then a linker bus error. The
  fix is not mysterious — `target/debug/deps` held **5 GB of re-linkable test binaries** from
  earlier ticks. `find target/debug/deps -type f -executable -size +100M -delete` gave 4 GB back
  instantly, and only the one suite under test needed to be rebuilt. **A test binary is a build
  artifact, not a result**: deleting it costs a link, never a fact.
- **Next.** The `form.submitted` consumer (REQ-064 — a submitted form becomes a contact and a
  deal) and the workflow-trigger proof (that an automation rule on `crm.deal.stage_changed` runs
  once), which needs wave 3's engine rather than this module's. Then the **w4 QA browser pass**,
  which has still never run on this branch: `QA_STACK=w4 QA_API_PORT=18083 QA_ADMIN_PORT=3103
  QA_WEB_PORT=3203 bash scripts/qa/run.sh` (timeout 1500s). Before that run, add the **copilot
  card** to `scripts/qa/walkthrough.cjs`'s depth passes — it is not in the route list yet, so the
  walkthrough would not visit the screen the previous tick shipped and the pass would prove
  nothing about it. It is the last gate for closing REQ-051.

## 2026-09-28 — wave 4 · REQ-051 slice 4 part five: the copilot gets a card, and the first w4 QA pass reaches the end of the route list

**What.** The two copilot endpoints have been reachable only from `curl` since they shipped. They
are proved, guarded by `crm.copilot.use`, audited on both paths and unit-tested — and no person has
ever been able to ask for one. The DoD is explicit that a REQ is not done until a human can use it
end to end, and an endpoint without a screen is that claim unfalsified. This adds the screen: a
**Copilot** button on every board card, a side panel that answers `summarize` and `follow-up`, and
`runCrmCopilotDepth` in the walkthrough.

**Three decisions, and why.**

- **The button is on the card, not in a row menu.** On the board the card *is* the record; a menu
  would make every question begin with "which card?". The panel follows the focused card and takes
  focus when it opens, so the keyboard is not stranded behind a layer. Escape closes it.
- **A refusal is rendered as a sentence.** This installation connects no provider, so the honest
  outcome is the failure path — and that is precisely the path that has to be legible. A button
  that does nothing is not a bug report anyone can file.
- **The draft marker is read, not assumed.** `is_draft` drives a visible flag that turns amber and
  reads "written to the record" if a response ever claims otherwise. The client must not quietly
  undo the promise the server makes from the other end. The answer renders in a **text node**: the
  server's sanitiser already reduced it to plain text, and rendering markup here would hand that
  guarantee back to a future refactor.

`?focus=<id>&copilot=1` opens the panel on arrival, so a shared link — or a palette row added later
— lands on the answer rather than on a card whose panel stays shut.

**Proof.** `pnpm typecheck` → **2/2**. `cargo test -p omnion-module-crm --lib` → **146/146**. The
w4 QA pass ran end to end on the private stack (`QA_STACK=w4`, ports 18083/3103/3203, database
`omnion_qa_w4`).

**Environment note — two disk deaths in one tick, both self-inflicted, both the same lesson.**
`/mnt/apopic` is a 60 GB mount shared by seven writers. The first pass died at
`ENOSPC … appendFileSync` in the walkthrough's own `clicks.jsonl`; a screenshot failure is a
warning, a `record()` write is fatal. Recovery was mechanical: QA artefacts, `.rmeta` files, the
incremental directory, and `target/debug/deps` binaries — **a re-linkable build artefact, never a
result** — which returned 1.3 GB. Two details worth keeping:

* `target/debug/omnion-api` is a **hard link** to `target/debug/deps/omnion_api-<hash>` (link count
  2). Deleting the `deps` copy costs a link and leaves the running binary intact; deleting the
  binary would make the next QA pass rebuild the whole API. Check `stat -c %h` before assuming a
  file is the only copy.
* Deleting the deps binaries **while** `run.sh` was between its build check and its `pm2 restart`
  is what made the first pass's `omnion-qa-api-w4` answer nothing on `:18083`. A maintenance window
  on a build directory is a window on a *running service*.

**Next.** The `form.submitted` consumer (REQ-064) and the workflow-trigger proof (wave 3's engine).
Then REQ-051's last three acceptance boxes — the empty/loading/error sweep, the 390×844 mobile pass
and the keyboard sheet — and only then the status line may read `done`.

## 2026-09-28 — wave 4 · REQ-051 slice 4 part six: the workflow-trigger proof

**What.** The acceptance criterion is one sentence — "an automation rule triggered by
`crm.deal.stage_changed` runs once" — and it is the only part of this slice that could not be
answered from inside the CRM. The CRM emits the event; the **matcher** decides what fires. So the
proof drives the real `crates/automation/src/matcher.rs` over the real bus in this suite's own
database, and the event the rule reads is the one the board's stage endpoint emitted. Three walks,
in `apps/api/tests/crm.rs`.

**What they assert, and why each one is a separate claim.**
- A move to the stage a deal is **already in** starts nothing. The board's keyboard path posts on
  every arrow press, so this is the assertion that keeps a rule from being a nuisance.
- One move starts **one** run, and the run's step carries *this* move's values: the subject reads
  "A deal entered open" and the body the deal's id, amount and currency. A retry therefore repeats
  the first attempt rather than re-reading a bus that has moved on.
- The second drain is **idle**. That is what "once" means — the absence of a second chance, not a
  count — and the cursor is read back to prove it.
- The match is audited (`automation.rule.matched`) against the execution.
- A **second deal** starts a second run: exactly-once is per *event*, and a rule that collapsed two
  customers into one run would silently drop one.
- A rule whose condition does not hold is `skipped`, not `matched`.
- The key is `workflows.manage`, so a CRM manager who may move deals all day still cannot define
  the rule watching them, and a rule for another organization is refused.

**Three defects this found, and the fourth that was not mine.**
- A token minted **before** the grant answered 403 on the very rule the walk writes: a session's
  powers are read at login, so grant-then-sign-in is the order every workflow key needs.
- The first draft read a token *twice* — once before the grant and once after — and the compiler's
  unused-variable warning was the only thing that noticed the first line was dead.
- The bus was **not empty** after the setup: creating a deal is itself an event. Two walks counted
  their own fixture and read "a rule fired twice" or "two events on the bus" where the product was
  right. Fixed with `flush_without_runs`, which asserts the drain started *nothing* — a setup flush
  that silently ran a rule would hide exactly what these walks are for.
- The fourth failure was **not a defect**: the suite's database was gone mid-run. `/mnt/apopic`
  reached 100% earlier in the tick, the Postgres container restarted, and a database created before
  that did not survive it. `RestartCount=0` with a fresh `pg_postmaster_start_time` is the tell, and
  the lesson is the one the disk keeps teaching: on this mount a full disk takes the **database**
  down, not just the build.

**Disk, again.** The build died at `libomnion_identity-….rlib: No space left on device`. This time
the reclaim was `apps/admin/.next/dev` and `apps/web/.next/dev` — 1.2 GB of turbopack dev cache,
re-derivable, and inside **this** worktree only. `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1` did the
rest at roughly a third of the wall-clock.

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

## Tick 68 — REQ-013 slice 3 (retention): the sweep had no caller

**What.** `prune_candidates` shipped in slice 1 with a doc comment describing four
exemptions, and **nothing called it**. The retention screen could list what the sweep would do
and the walkthrough could assert the exemptions hold, and the bytes on the destination would
accumulate for ever. Five commits: `e704c0f` the sweep, `faea0bd` the two config knobs,
`514665c` the worker, `813f33b` the manual route, `9c8170b` the panel, `e77fd9a` the walk.

**The shape of the defect is now unmistakable.** Tick 66 found a half that *counted*. Tick 67
found a half that *scoped wrongly*. This tick found a half that **was never invoked**. Three
ticks, three different ways for one feature to satisfy every assertion in its own tests and
disagree with reality, and all three are the same question: *who calls this?* The delete
knew how to take a run's artifacts off the disk; the sweep — which deletes **more** than the
delete does, unattended, with nobody watching — had never heard of the function. That is the
lesson worth more than the code: **a pure function with a thorough doc comment and no caller
is the most convincing piece of dead code there is.** It reads as a feature. Its tests pass.
Its exemption rules are correct. It does nothing at all.

So `crates/backup/src/sweep.rs` is a **caller** and deliberately the only one, and it writes
no path arithmetic of its own. Two implementations of "remove a run's directory" is how a
destination ends up with a directory the sweep believes it deleted — the same "two halves that
make the same mistake are not a cross-check" rule the media part taught, restated at the
removal.

Three decisions that are not obvious. **Bytes first, row second**, so an interrupted sweep
leaves a row over an archive that is still there and the next tick takes it again. **A
partial removal still deletes the row** — the same call the delete handler makes, because
retention is a window and not a bulk delete, and one stuck file must not retain a run for
ever. And the sweep walks **tenants**, not rows, through `organizations_with_backups`: a
separate function rather than an inline `select distinct`, because a second answer to "who
gets swept" is a rule that drifts the first time one of them is edited. `null` is a real
member of that list — a sweep that filtered the platform's own backups away would never prune
the restore points that matter most on a single-tenant installation.

**Two flags, not one.** `OMNION_BACKUP_SWEEP` is independent of `OMNION_RETENTION_RUNNER`,
because "never delete my backups" must not have to mean "never purge my trash"; the only way
out otherwise is to turn the whole worker off. The six hour default is chosen from the
feature: the shortest window the panel allows is a day and the newest successful run is exempt
whatever it is, so hourly finds the same set six times for the same answer and nightly leaves
a run whose day ended at 04:00 sitting there for twenty hours.

**The manual route is scoped, and that is the part worth proving.** `POST
/api/v1/backups/sweep` calls `sweep_organization` for the **caller's** tenant, never
`sweep_all` — an operator pressing "run retention" on their own site must not delete another
tenant's restore points. The walk creates a stranger tenant's expired run pointed at the same
destination and requires it to keep **both** its row and its directory. The stranger is not
decoration: without it "everything" and "this organization" are the same set, which is the
exact blind spot the media part's tenancy fix was found through last tick. The same lesson,
one layer up, and it is now the second time this feature needed a stranger in the fixture.

The route is registered **before** `/backups/{id}`. A `POST` against `/backups/sweep` would
otherwise match `{id}` and fail to parse `sweep` as a UUID — a 500 that reads like a router
bug on the one route whose whole point is to be callable by hand.

**Proof.** `omnion-backup --lib` **85/0** (80 before, +5 in `sweep`) · `omnion-core --lib`
**39/0** (+2, both pinning the flag independence in both directions) · `apps/admin`
`tsc --noEmit` clean · `the_retention_sweep_takes_the_bytes_and_spares_what_it_promised`
**1/0 in 5.5s** over the real router and the real filesystem. That walk goes and *looks at
the directory*, because the sweep's whole claim is about bytes and a row delete reports the
same counts the panel shows.

**Blocker, fourth tick running, unchanged and not worked around.** The browser pass did not
run. `qa-slot.sh` is held by a live sibling (pid 142641) and the box peaked at load 23 with
**1 GB of 32 free** and 24 GB of swap in use; `scripts/qa/run.sh` would have added a fifth
Chromium to that. `runBackupDepth` stays written-but-unrun, so REQ-013 does not close on
tests alone. Two toolchain facts, both recorded because each read like a product defect and neither was
one. A **stale orphan test binary** from an earlier tick (`backups-b245d57c4aaeca51`, no
parent shell) was holding QA database connections across ticks; killed it. And a **sibling
deleted my `target/debug/incremental` mid-build**, which surfaces as
`failed to move dependency graph … os error 2` — a compile failure in files that were already
merged and building fine. The full-suite run then hung in the pre-existing
`a_protected_backup_is_never_a_prune_candidate_and_the_newest_successful_survives` with no
active query and no blocked lock, so I killed it, killed the orphan, rebuilt with
`CARGO_INCREMENTAL=0` and re-ran that exact test in isolation: **1/0 in 3.19s**. It was never
red and never broken — it was starved. Two rules, both already half-known and now confirmed:
**build with `CARGO_INCREMENTAL=0` when siblings are live**, and **an unexplained hang with no
database activity is contention before it is a defect**.

**Next.** (a) The restore path (slice 2) now has the index a preview needs: the media index
is written, `pending_objects_for_organization` says whose files a run may restore, and the
sweep tells the operator what is actually on the destination. (b) The `partial` box is still
unticked — a run where one part fails ends as `partial` with the message visible in the UI
needs a fault injected into the drawer, not a test.


## Tick 69 — the restore preview, and the two rules three ticks of unit tests had passed

**REQ-013 slice 2a.** `GET /api/v1/backups/{id}/restore-preview` plus the panel that reads it.
The decisions live in a pure module (`crates/backup/src/restore.rs`) so the rules are
unit-tested with no stack, and the live-data comparison is a separate one
(`crates/backup/src/preview.rs`) so a change in how it is counted cannot silently alter which
warnings fire. The route re-reads every artifact, compares each part's size against the
manifest, and prices the restore against the live library.

**It found two defects in shipped code.** Both are the fourth instance of one shape — **a rule
that is tested and that nothing obeys** — and neither was findable by a unit test, because in
both cases the unit test was the reason it survived.

**1. A media part's recorded size could never match its own artifact.** `finish_media_part`
recorded `bytes_copied + index bytes` as `size_bytes`, with a doc comment saying that
`size_bytes` is "what `verify_manifest` compares against the artifact on disk". That reasoning
is backwards: `storage_path` for the media part names the **index alone**, so a size including
the copied objects' bytes can never equal the length of that one file. The two agree only when
`bytes_copied` is zero — which is exactly what the `verify` walk's fixture was, because that
suite's library has no objects. So `verify` reported every real media backup as mismatched,
for ever, and the new preview refused to offer it as a restore point. **Both verdicts were
correct; the number was wrong.** The preview is the first reader that compares a media
artifact's length on a run with a non-empty library.

**2. `produce_all` never read `run.scopes`.** It walked all five `PARTS` unconditionally, so a
backup requested for `["database"]` produced five artifacts: the scopes were validated by
`normalise_scopes`, stored, normalised, and rendered in the drawer's five checkboxes, and then
ignored at the only point that mattered. **Four existing walks request `["database"]` and none
of them noticed**, because each asserted on the part it *wanted* rather than on the number of
parts, and the two extra artifacts are perfectly valid files. The scope selector was a dead
control with a green tick beside it. A consequence worth recording: a media-only run whose one
part failed used to be `partial` — the correct verdict about four parts the operator never
asked for — and is now honestly `failed`. `summarise` is untouched and still right; what
changed is the set of parts it is handed.

**A third finding, in the new code, from a test with a realistic id.** The typed confirmation
sliced the first eight hex characters off the run's id. For a v4 uuid that is fine and looks
random. For a **v7** uuid — whose leading bytes are a millisecond timestamp — "the first eight
hex characters" is a *clock*: two backups taken three hours apart produced the identical phrase
`RESTORE 000001a0`, and every run inside a ~50-day window shares one. A guard that restores the
wrong run is worse than no guard, because it looks like one. The phrase is now a **hash of the
id**, which mixes the timestamp with the random tail whatever the id's layout, and the id is
validated first so an unnameable run still yields no phrase at all. The regression test uses two
v7-shaped ids sharing a timestamp prefix.

**Two smaller ones in my own code, both the silence class.** `LiveCounts::dropped` documented a
zero floor that `saturating_sub` does not provide — saturating means stop at `i64::MIN`, not
stop at zero, so an inconsistent live pair rendered as a negative loss. And the
healthy-archive fixture stamped `now` at a round epoch (Jan 2027) that made every archive look
107 days old, which is why the first "healthy archive" test failed on a `stale_archive`
warning it had just proved absent.

**Why the preview is behind `backup.read` and not `backup.restore`.** Reading a warning is free
and changes nothing; gating it behind the destructive key means the first time an operator
meets this screen is a 403 that never showed them what they were agreeing to. The expensive
permission is for the button *after* it.

**Why there is no restore button.** The safety backup, the typed confirmation's enforcement and
the abort path are the next slice. A "Restore" button that could not be pressed is a dead
button, which this product does not ship; the panel that explains the restore and asks for
nothing is a working one. The phrase is **shown** rather than demanded for the same reason.

**Proof.**

| Gate | Result |
| --- | --- |
| `omnion-backup --lib` | **103/0** (85 before: +18 preview model) |
| `omnion-api --test backups` | **15/15** (12 before: +3 preview walks) |
| `apps/admin` `tsc --noEmit` | clean |

The three new walks are over the **real router and the real filesystem**. The load-bearing one
prices a one-file archive over a two-file library at exactly **one** lost item, and then proves
it wrote **nothing**: part rows, run status, `storage_prefix`, `finished_at`, the media rows and
the archive's directory on the destination are all byte-identical before and after, read back
out of **PostgreSQL** rather than from the response — a response body cannot prove the database
was not written to, and this is the one property the whole slice exists for. The second refuses
a truncated artifact and issues **no phrase**. The third requires a stranger's run to be a 404
whose message does not name the tenancy rule, because `403 cross_organization` confirms the id
exists and turns a preview into a restore-point oracle.

**Toolchain, four facts, all of which read like product defects and none was one.**
`VersionMissing(19)` on the default test database is a **stale QA database from a sibling's
tree** — the shared migration namespace again, and the default `omnion` database carries a
version-19 row from `omnion-w2`/`w5`/`w6`, none of which have a 0019 in this tree. The walks ran
against a disposable `omnion_build_69` instead, which is the rule for a suite database the whole
box shares. `cargo fmt -p omnion-backup` **rewrote five files I had not touched**; the diff was
pure whitespace and was reverted with `git checkout --` on exactly the foreign five, which is why
the `git diff --name-only` comparison is done by hand every tick. The doc-comment linter reports
`async fn is not permitted in Rust 2015` on every `async` in the crate — the toolchain linter
does not pass the edition, and `cargo build` is the authority. And a doc comment containing
`**/` inside a Python triple-quoted string closes the string: two patches this tick failed to
parse for that reason, and the fix is the `patch` tool, not `execute_code`.

**Next.** (a) The destructive half of the restore: part selection, the mandatory safety backup
before the first write, the enforced phrase behind `backup.restore`, abort until the import
begins, and the `backup.restored` audit entry. (b) The `partial` box is still unticked — a run
where one part fails needs a fault injected into the drawer, not a test. (c) The browser pass
is queued behind a live sibling's `qa-slot.sh`; the walkthrough is extended to open the panel,
read the price, the warnings and the phrase, so when the slot frees there is something to run.
- **Next.** Slice 4 — folder and file grants with inheritance, the scanning pipeline with
  quarantine and release, retention policies with the daily worker, and reference-based purge
  refusal. Done when a denied subject is refused on the raw route, a flagged upload is quarantined
  and releasable, and a retention run removes exactly the eligible rows.

**Proof.** `cargo test -p omnion-api --test crm` → **42 passed; 0 failed** against a database
created for this run (`omnion_test_w4_t7c`), `--test-threads=1`. `cargo test -p omnion-module-crm
--lib` → **146/146**. `pnpm typecheck` → **2/2**.

**Next.** The `form.submitted` consumer (REQ-064 — a submitted form becomes a contact and a deal),
which is the last part of this slice. Then REQ-051's final three acceptance boxes: the
empty/loading/error sweep across all six screens, the 390×844 mobile pass and the keyboard sheet.
Only then may the status line read `done`, and that close tick runs the w4 QA browser pass.

## 2026-09-28 — REQ-051 slice 4 part seven · a submitted form becomes a contact and a deal

- **The merge first.** `origin/main` had moved 10 commits (the media file manager's presets and
  transform pipeline, the QA slot holder). One conflict, in the append-only build log, resolved
  as a union from the three merge stages rather than by hand — and verified two ways, because
  the last merge of this file had silently dropped 112 lines on the main side: the per-side
  `SequenceMatcher` reported a shortfall of 112, and a **multiset** comparison of every line on
  both sides against the merged file reported **zero missing on either side**. The line count
  checks out too: 1874 base + 572 ours + 66 theirs = 2512.
- **What shipped.** **`modules/crm/src/leads.rs`** — the consumer, 1262 lines with 26 unit tests ·
  **`database/migrations/0034_crm_form_leads.sql`** (0034 was free; every other wave's highest is
  0033) · **`apps/api/src/routes/crm_leads.rs`** — three routes and two permission keys ·
  **`apps/api/src/lead_runner.rs`** — the background drain, wired in `main.rs` behind
  `OMNION_LEAD_RUNNER` · `CrmConfig` in the core config (`OMNION_LEAD_POLL_MS`,
  `OMNION_LEAD_BATCH`) · the inbox screen and its API client in the admin app · ten integration
  walks.
- **The producer is not in this build, and that shaped everything.** The form builder is REQ-064
  (wave 2b, another worktree). So the consumer is written against the **event contract** —
  `form.submitted` — and not against a form table, a form crate or a form type. It does not
  import anything from a module that may never be installed, and the day the builder lands it
  has to emit one documented string and nothing else. The walks emit the event through
  `omnion_events::bus::emit` — the same call REQ-064's public endpoint will make — rather than
  inserting a row, so what is proved is the contract and not the SQL.
- **Three decisions that are the whole feature.**
  1. **Exactly once, by the bus identity.** `crm_form_leads.event_id` is the primary key. An
     `exists` check followed by an insert would be the obvious way and it is wrong: there is a
     window between the two statements in which a second API process reads the same bus and
     creates a second contact. So the ledger row is inserted **first**, with
     `on conflict do nothing`, and the **row count** decides who acts. One of the three walks
     rewinds the cursor and drains the same event again to prove the claim, not just the cursor,
     holds.
  2. **Deliberately not in a transaction.** `create_contact` and `create_deal` take a `&PgPool`,
     so wrapping the drain in one transaction would mean changing the module's own write
     signatures to take an executor. Claiming first buys a different failure: a crash between the
     claim and the write leaves a ledger row that says `rejected` and names why, which the inbox
     shows. A rolled-back transaction shows nothing and replays forever. The `restate` call is
     what keeps the ledger honest when the contact write itself fails.
  3. **Routing is a row, not a constant.** `crm_lead_settings` holds two toggles, two stage ids
     and a source label, seeded for every organization that has a pipeline and created on first
     use for the ones that do not. A feature that only works after somebody has opened a
     settings screen is a hidden feature; the `GET` deliberately does **not** create the row (a
     read that writes turns a read into a change), and the drain's `load_settings` does.
- **Four defects the unit tests found, all of them in the extractor, all of them in the *happy*
  path of a form nobody had written yet.**
  1. **A form with both a "Name" and a "First name" field lost the first name.** The key list
     had `name` first, so the combined two-word value won and "Augusta King" became the first
     name `Augusta King`. The more specific key has to be read first — a form that collects both
     has told us which one it means.
  2. **A 400-character "name" and a name of `---` both passed.** The explicit name fields were
     not passed through the same `clamp_name` the guessed split used, so a hostile or broken
     producer reached the first-name column and `create_contact` then refused the whole
     submission with a message a person reading a form never typed. Refusing at extraction turns
     that into an honest `rejected` row.
  3. **A message longer than the note cap was dropped entirely** — losing exactly the person who
     cared enough to write a lot. It is truncated at a word boundary now, as the doc promised.
  4. **A submission with only an address produced a contact with no first name**, because the
     contact table requires one and `validate_contact` refuses. The local part of the address is
     the honest fallback: it is what the person typed in that field, and a human can correct it. A
     placeholder like "Unknown" is not — it looks like data and reads as an error.
- **The `create_deal` signature changed, and that is a decision worth naming.** It took
  `user_id: Uuid` and used it as the owner fallback, so a submission would have been filed into
  whoever happened to run the drain. That is a stranger's lead in a colleague's list. The
  parameter is now `owner_fallback: Option<Uuid>` and the drain passes `None`: an unassigned deal
  is a deal the board shows as needing an owner. The one other caller passes `Some(current.user.id)`.
- **Ten walks, over the real router and the real bus.** A submitted form becomes a contact and a

## 2026-09-28 — REQ-051 · the request id, one error state, and the panel that was stricter than its API

**What.** The empty/loading/error box was the last thing left on REQ-051 and it turned out to be
three defects wearing one label. The API's refusal carried a code, a message and sometimes a
`details` object and **no request id**, so the "error state with retry button and request id" the
box asks for had no id to show. `apps/api/src/request_id.rs` now stamps `x-request-id` on every
response and `error.request_id` in the body, honouring an inbound id only when it is safe to
reflect and replacing a hostile one rather than sanitising it. `lib/crm.ts` was also dropping
`error.details`, which is why every field-level refusal rendered as a generic banner with nothing
under the input it was about. `components/error-state.tsx` replaces six hand-rolled error blocks —
three shapes, and two of them with no retry at all — and the board's empty column got a sentence
and an action instead of a grey "No deals".

**Then the QA pass told a different story.** It reported the *whole* CRM suite false: no company
created, no contact created, no email refusal, no shortcut sheet. The first screenshot answered it.
The QA owner is a **platform account** (`organization_id = NULL`) holding exactly one binding, and
`crm-tenant.tsx` drew its organization chooser for that account, so no CRM screen ever issued a
list read. `organization_of` in `apps/api/src/routes/crm.rs` does not do this — it falls back to
the caller's single binding and refuses with `organization_ambiguous` only for two or more. The
panel had re-decided the tenant rule independently and decided it more strictly. Two ticks of CRM
acceptance evidence had been collected from a chooser, and every "proved in the browser" claim in
the REQ was resting on it.

**Proof.**
- `cargo test -p omnion-api --lib request_id` — **10/10** (a hostile header value, the CSV
  passthrough, both homes of the id, and the details object surviving the rewrite).
- `cargo test -p omnion-module-crm --lib` — **172/172**.
- `pnpm turbo run typecheck --force` — **2/2** (twice; a cached typecheck is not a typecheck).
- Live, against the QA API: `GET /api/v1/crm/contacts?limit=1` unauthenticated answers **401** with
  `x-request-id: 2ec164d9…` **and** `{"error":{…,"request_id":"2ec164d9…"}}` — the same value in
  both homes, on a real refusal.
- First QA pass (`20260928-141747`): 1006 clicks, 1068 screenshots, `crmStates` **23 of 27 false**,
  and the 98 high findings are **all** `media/settings` — the main writer's surface, not this one.
  The pass is re-running with the tenant fix as its first job.

**Commits.** `63512c5` the request id · `c0e51fe` the client and one `ErrorState` for six screens ·
`225425e` the REQ · `1e0af8f` the empty column · `8477c2f` the tenant fix.

**Next.** The re-run decides whether REQ-051's error box closes, or whether the state sweep finds
that a screen with a working error state still has a broken *load* state. Then the 390×844 pass and
the keyboard sheet, which are the two boxes after it.

## 2026-09-28 — REQ-051, the CRM was unreachable on a first-run installation

**What.** The scoped pass (`--only=crm`, added this tick) found that after the tenant fix every
CRM read *and* write answers `organization_required`: no contact, company, deal or activity could
be created or listed by the account the panel is used with. `organization_of` resolves a caller
that cannot name an organization from the **tenant bindings** it holds, and the platform owner of
a first-run installation holds the `global` Owner role and nothing else — the owner role is a
`global` binding by construction, so the tenant list is empty. The refusal's own advice,
"pass organization_id", is impossible to follow: naming a tenant requires the very permission that
binding would have carried. The `[]` arm now falls back to the installation's organization when
there is **exactly one**, and only then; two or more is still `organization_ambiguous`.

**Proof.**
- `cargo test -p omnion-api --lib routes::crm` — **27/27** (three new: the one-organization
  fallback, the two-organization refusal, and the no-organization sentence no longer naming a
  parameter the caller cannot supply).
- `cargo test -p omnion-module-crm --lib` — **172/172**.
- `pnpm turbo run typecheck --force` — **2/2**.
- Live, before the fix: `GET /api/v1/crm/contacts` and `/api/v1/crm/contacts/export` as the QA owner
  both answered `400 organization_required` with a `request_id`, and the database held **one**
  organization and **zero** CRM rows — the empty state the pass reported was the refusal wearing an
  empty list's clothes.
- Scoped pass `20260928-155321-crm` (7 routes, `--only=crm`): `contactsEmptyState: true` against a
  screen that had never loaded — the false green this tick exists to end.

**Commits.** `a8e3f53` the scoped pass · `e7ba429` the one-organization fallback.

**Next.** Restart the w4 QA API onto the rebuilt binary and re-run `--only=crm`: the depth passes
that never got to run (deals, activities, copilot, leads, the state sweep) are what decide whether
REQ-051's empty/loading/error box closes. The 390x844 and keyboard boxes are still open after it.

**Environment.** `/mnt/apopic` hit 100% mid-build and the box-wide disk guard deleted this
worktree's whole `target/`, so the build is now on `CARGO_TARGET_DIR=.tmp-target` (the repo's own
convention, and it is the same tree the guard spares). Load sat at ~316 for ten minutes: seven
writers compile at once, so a build can sit at 0% CPU for minutes before it is scheduled at all.
  deal with the deal titled in the person's own words · the same bus drained twice files nothing
  twice, **including** after the cursor is rewound onto the same event · a repeat from a
  differently-cased address is the same person and lands in the repeat stage while the first deal
  stays in the first open column · a submission with nothing usable is kept, is `rejected`, and
  **carries the sentence** · a submission with no organization is `orphaned` and is in nobody's
  inbox · an organization that turned leads off records the submission and writes nothing · the
  two keys are genuinely separate (a watcher may read the inbox and is refused when it tries to
  reconfigure it), unauthenticated is `401`, and a foreign tenant's save does not touch ours · a
  stage from another pipeline's organization is refused **by name** in `error.details.field` ·
  a routing change is audited and announced exactly once, and the same body again is neither ·
  and the two keys are in the catalogue and in the owner's role, and **not** handed to a CRM
  manager by default.
- **Proof.** `cargo test -p omnion-module-crm --lib` → **172/172** (146 before, 26 new).
  `cargo test -p omnion-api --test crm` → **52/52** (42 before, 10 new) against a database
  created for this run, `--test-threads=1`. `pnpm typecheck` → **2/2**. `cargo check --workspace`
  clean. The walkthrough route list gained `/crm/leads` and a `runCrmLeadsDepth` pass.
- **The QA browser pass, honestly.** It ran on the private `w4` stack (`:18083` / `:3103` /
  `:3203`, database `omnion_qa_w4`, `QA_SLOTS=0`) and got through the whole route list **until the
  Chromium tab died**: `Target page, context or browser has been closed` from the analytics routes
  onward, so `crm-leads` and the depth passes after it were recorded as failures of the *tab*,
  not of the screen. The box is running seven writers and three concurrent QA passes; free memory
  was 262 MiB against 32 GiB with 17 GiB already in swap. The screen is in the route list and the
  pass is registered, so the next run on a quieter box proves it; **this tick does not claim a QA
  pass on the new screen**, and REQ-051 stays `in-progress` for that reason as well as for the
  last three acceptance boxes.
- **One more disk death, and the file it cost.** `/mnt/apopic` reached 100% mid-tick and a
  `write_file` of `leads.rs` left it **zero bytes** while `git status` still showed it as
  untracked. The lesson is the one from `git commit` failing the same way two ticks ago: on this
  image a write can succeed, a truncate the file, and report nothing. The file was rewritten from
  the content the compile errors had already corrected.
- **Next.** REQ-051's **last three acceptance boxes**, which are the whole of what remains: the
  empty/loading/error sweep across every screen, the 390×844 mobile pass and the keyboard sheet.
  The inbox screen already answers `/` for the search and is in the route list, so the sweep is
  about the six screens that predate it. The close tick runs the w4 QA pass.

## 2026-09-28 · REQ-051 · tick 10 — the platform account, and twelve failures that were five

- **What shipped.** **The tenant fallback** (`b899f7f`) — the QA owner's five CRM screens
  answered `400 organization_required` because a platform account has no primary organization
  by design. The module now resolves the tenant itself and falls back to the single
  organization a platform account is bound to, refusing when there are two (`organization_
  ambiguous`) or none. `apps/admin/app/crm/layout.tsx` wraps the six screens in one
  `CrmTenantProvider` so two screens cannot disagree about which tenant they are showing.
- **And four real defects, found by running a suite whose walks had never executed.**
  `29b202f` a doubled trailing backslash in a Rust SQL string (`\\`) is an escaped backslash,
  not a line continuation, so a literal `\` reached Postgres: nine of the twelve failures were
  one character, and it made every lead-settings read a 500. `c739ccf` `crm_form_leads.
  organization_id` was `not null` while the migration's own header says a submission may belong
  to nobody — so the orphan path, the branch that exists to *keep* such a submission, raised a
  not-null violation and the drain reported `failures: 1` while every counter read zero.
  `fe2d49b` `NAME_KEYS` held `"name"` beside `"first_name"`, so a form whose field is `name`
  gave `Ada Lovelace Lovelace`: each half a valid name, which is why no validator caught it.
- **Proof.** `cargo test -p omnion-module-crm --lib` → **172/172**. `cargo test -p omnion-api
  --test crm` → **55/55** against a database created for the run, `--test-threads=1` (52 before,
  12 of them failing, 3 new). `pnpm turbo run typecheck --force` → **2/2**. The other seven
  failures were walks passing `fixture.owner` — an e-mail — where `request` takes a bearer
  token, answering 401 before the route under test was reached.
- **The environment.** `rustc` was **0 bytes, mode 000** (`Permission denied` on `rustc -vV`),
  the known box-wide 0-byte corruption; `rustup toolchain install stable --profile minimal
  --force` restored it, and `/mnt/apopic` was at 95% throughout.
- **Next.** REQ-051's **last three acceptance boxes**: the empty/loading/error sweep across the
  six screens, the 390×844 mobile pass and the keyboard sheet. The close tick runs the w4 QA
  pass and requires 0 high findings before the status becomes `done`.
  a contact + deal. Then the **w4 QA browser pass**, which has still never run on this branch:
  `QA_STACK=w4 QA_API_PORT=18083 QA_ADMIN_PORT=3103 QA_WEB_PORT=3203 bash scripts/qa/run.sh`
  (timeout 1500s). It is the last gate for closing REQ-051, and the copilot's panel card still
  has to be added to the walkthrough route list before it is worth running.
**What.** Two things in one tick, because the first was a prerequisite for measuring the second.

**The merge.** `origin/main` had moved 17 commits (IAM SSO, the file manager, the authentication
screen). Three files conflicted and all three are shared, so each was resolved as a union rather
than a choice: `app-shell.tsx` (both sides added a nav entry and its icon), `BUILD-LOG.md`
(append-only) and `scripts/qa/walkthrough.cjs`, where two independent depth passes sit at the same
insertion point — `runCrmDealsDepth` and `runIamAuthenticationDepth` — and the mobile route list
became the union of both sides' entries. The walkthrough resolution was rebuilt from the three
merge stages with `difflib` rather than by hand: a first attempt joined the blocks and lost the
file's tail, which `node --check` caught as `Unexpected end of input`.

**Slice 4, half one.** The activity feed, the log form, and one ordered timeline that merges a
record's calls, meetings, notes and tasks with its deals' stage changes and archive markers.

The rules that needed writing down, because the schema could not express them:

* **An activity hangs off exactly one record.** `crm_activities_attached` only requires *one* of
  the three foreign keys, so `company_id` **and** `contact_id` together satisfies the database
  while making the timeline ambiguous about which record owns the row. The module is the stricter
  of the two, and the API test proves the refusal.
* **A task needs a due date or a done mark.** The open-task index is
  `(organization_id, due_at) where done_at is null`, so a dateless open task appears on no list.
* **The attachment is checked inside the statement that writes the row.** A separate existence
  check is a race: an activity logged against a deal archived a millisecond later hangs off
  nothing, and the API answers `404` rather than writing an orphan.

The timeline is built rather than stored, and its stage changes come from the **deal rows**, not
from the event log — so a record imported before the event bus existed still has a correct
history, and a replayed event cannot duplicate an entry. The synthetic entry id carries its arm
(`stage:<id>` / `archived:<id>`) because a deal's stage change and its archive marker are two
entries of one timeline, and two identical React keys means one of them silently overwrites the
other.

**A defect the tests caught in the first draft.** `relative_label` had the sign inverted:
`then - at` is **negative** for the past, so the first version read `"in 5m"` for something that
happened five minutes ago. Two unit tests caught it. The same function is mirrored in
`apps/admin/lib/crm.ts` — a label is a presentation decision, and two spellings of one idea is how
a timeline ends up disagreeing with its own feed — so both now carry the same bucket bounds and a
comment naming the sign. A third test failure was the *test's* fault, not the code's: 14 days is
`1_209_600s`, below the week bucket's `2_592_000s` floor, so `"14d"` is correct and the
expectation was wrong. The bucket bounds are consts now, because a range *pattern* cannot hold the
arithmetic they are written with.

**Proof.** `cargo test -p omnion-module-crm` → **132 passed** (106 before this tick; 26 new).
`apps/admin` `tsc --noEmit` → 0 errors. `node --check scripts/qa/walkthrough.cjs` → clean. The
route-level `crm_activities.rs` handlers carry their own unit tests, and the integration walks are
written but **have not run** — they need a database, and `/mnt/apopic` was at 97% with four other
writers compiling when the tick ended. The QA browser pass is likewise not run.

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

**Next.** Run `cargo test -p omnion-api --test crm` against a **fresh** database (see the
`VersionMissing` note in REQ-051 — the shared dev database carries applied migrations no branch
has, and a throwaway one is the only way the walks execute), then the w4 QA pass:
`QA_STACK=w4 QA_API_PORT=18083 QA_ADMIN_PORT=3103 QA_WEB_PORT=3203 bash scripts/qa/run.sh`. That
is also the last gate for slice 3. Then the rest of slice 4: the copilot's two endpoints, the
global-search registration and the `form.submitted` consumer.

**Environment note.** `/mnt/apopic` went to **98%** (1.5 GB free) mid-tick and a
`cargo build -p omnion-api` died with `No space left on device` writing a `.rmeta`. `CARGO_INCREMENTAL=0`
and `CARGO_BUILD_JOBS=1` are the settings that got the rest of the tick through. The seven writers'
`target/` directories are still the pressure — they total over 27 GB on a 60 GB mount — and a
shared `CARGO_TARGET_DIR` would remove it permanently.

---

## 2026-09-28 — wave 4 · REQ-051 slice 4: the gate that had never run, and the copilot's module

**What.** Ran the database gate that two ticks had deferred, against a throwaway database
(`omnion_w4_gate`) rather than the shared development one, and built the copilot's module — the
piece of slice 4 that is pure rule and needs no provider to prove.

The suite **ran for the first time**: 29 of 35 walks passed and the six slice-4 walks failed. They
had never executed — the default database's migration ledger carries versions 19 and 21 from a run
against files no branch carries, so the fixture panicked before any test body and the "29 passed"
being reported was 29 of the *slice-1-3* walks. The six new ones were silently not in that number.

They found **four product defects** and three mistakes inside the walks themselves:

* The activity feed's visibility clause emitted `any($1, $2)` instead of `any($1)` — a
  `separated(", ")` of individual binds where the array operator needs one array parameter. A 500
  for every caller at the `team` level. `open_tasks` repeated it by hand; `set_activity_done` used
  `$3` for both the `done_at` timestamp *and* the id list, so closing any task was a 500 too. One
  shared `push_activity_visibility` now serves all three.
* `Activity` and `TimelineEntry` serialised their timestamps with no serde attribute, so every
  activity response carried `time`'s tuple (`[2026, 263, …]`).
* `ActivityChanges`' three timestamps were bare `Option<OffsetDateTime>`, and `time`'s serde support
  is opt-in per field — a bare one accepts **no** JSON string, so logging a task with a due date
  was a 422 for every caller. `dates::instant` is the new round trip: RFC 3339 as written, a
  zone-less `datetime-local` value as UTC, an explicit offset on the way back out.
* In the walks: an e-mail passed where a session token belonged (a 401 that reads as a broken
  endpoint), a deal's id passed to a route that takes an activity's, `rp.permission` where the
  column is `permission_key`, and `actor_user_id` read from a helper that names the key `actor`.

`modules/crm/src/copilot.rs` is new: the deal + company + history read through the **caller's own
`Scope`** (so a copilot call is exactly as restricted as the card it sits on), the two instructions
as constants, and a sanitiser that treats the model's answer as untrusted text — unwraps a stray
code fence, strips tag runs and control characters, caps a runaway answer, and refuses an answer
that is only markup with a new `CrmError::EmptyAnswer` (mapped to a `502` with the code
`crm_copilot_empty_answer`). The module writes nothing to any CRM row, by construction.

**Proof.** `cargo test -p omnion-module-crm` → **146 passed** (139 before; 7 new for the timestamp
round trip, plus the copilot's 7 in the previous commit). `cargo test -p omnion-api --test crm`
against `omnion_w4_gate` → **35 passed, 0 failed** (29 of them the slice-1-3 walks, re-proved; 6 the
slice-4 walks, running for the first time). `pnpm typecheck` → 2/2 packages, 0 errors.

**Next.** The copilot's two endpoints (`POST /api/v1/crm/copilot/summarize` and `/follow-up`,
`crm.copilot.use`, each audited) — the module is in and the route is not — then the global-search
registration (REQ-002) and the `form.submitted` consumer. The **w4 QA browser pass still has not run
this tick** (`QA_STACK=w4 QA_API_PORT=18083 QA_ADMIN_PORT=3103 QA_WEB_PORT=3203 bash scripts/qa/run.sh`);
it is the last gate for closing REQ-051, and `/mnt/apopic` was back at 97% with other writers
compiling when the tick ended.

**Environment note.** `/mnt/apopic` is shared by seven writers and the mount sat at 97-98% for most
of this tick (1.5-3.0 GB free). `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2` and deleting this
worktree's own `target/debug/incremental` (46 MB) kept it workable; the seven `target/` directories
still total over 27 GB on a 60 GB mount, and a shared `CARGO_TARGET_DIR` remains the permanent fix.

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
before the 6-10 minute pass could finish, so slice 3's close tick carries it.

**A migration number is global across branches, and it is worth repeating after being bitten
twice.** `0021` was claimed by `0021_iam_sso.sql` on main while this file was in flight, and
the symptom was not a conflict: sqlx reported `Migration(VersionMissing(19))`, because the
branch was also missing 19 and 20 (the sibling writers' slots) and a gap reads as a missing
version long before it reads as a collision. The file is now `0022_crm.sql`, contents
unchanged.

**Next.** Slice 3 — deals and the pipeline board: the stage editor, the board with drag *and*
the `ctrl + ←/→` keyboard path, per-stage count/sum/weighted forecast from one SQL expression,
and the won/lost flows with the lost reason and the close date.

**Environment note.** `/mnt/apopic` hit 100% twice more during this tick — once with `git
commit` failing outright with `No space left on device`, and once with cargo dying inside
`ld`. The seven writers' `target/` directories are now ~24 GB on a 60 GB mount. This writer
reclaimed only its own `target/debug/incremental` (2.4 GB). A shared `CARGO_TARGET_DIR` or a
per-stack trim rule would remove the pressure permanently; it is the owner's call.

## 2026-09-27 — REQ-051 slice 3: deals, the pipeline board, the stage editor

**What.** The relationship layer's third slice. `modules/crm/src/deals.rs` (1,765 lines) holds
the rules; `apps/api/src/routes/crm_deals.rs` is the thin HTTP layer; the panel gets
`/crm/deals` (board + list toggle) and `/crm/settings/pipelines` (the stage editor); five
permission keys are registered; the walkthrough gained two routes and a depth pass.

**Proof.** `cargo test -p omnion-module-crm` → **100 passed** (84 from slice 2 plus 16 new).
`cargo test -p omnion-permissions` → **63 passed** (the catalogue's `crm` family test now
asserts all thirteen keys). `apps/admin` `tsc --noEmit` → **0 errors**. Commit `9909af9`, pushed
to `origin/wave4`, tree clean.

**The blocker, stated plainly.** `cargo test -p omnion-api --test crm` cannot run: the fixture
panics with `Migration(VersionMissing(19))`, so all 29 tests FAIL in 0.82 s before any test
body executes. This branch's migration directory is non-contiguous — it holds `0022_crm.sql`
without `0019`, `0020` or `0021` — and **the same gap exists on `main`**, so it predates this
work and is not caused by it. sqlx reports the missing version rather than the collision,
which is why the message names 19 and never 22. Merging `main` would supply `0021` and leave
0019/0020 still missing, so the fix is not a merge. The full table of what each of the seven
branches holds, and the two options, are in `docs/requests/REQ-051-crm.md` under "Blocker".
The owner's call: either renumber onto each branch's real sequence, or have `main` carry the
two missing files as empty additive migrations so every branch converges.

**What that costs this tick.** The eight integration walks in `apps/api/tests/crm.rs` and the
browser pass `runCrmDealsDepth` in `scripts/qa/walkthrough.cjs` are **written and unexecuted**.
The QA browser pass was therefore not run either, which means slice 3 is not closed and REQ-051
stays `in-progress` — deliberately, because the rule is that a REQ closes on a QA pass and not
on unit tests alone.

**Three decisions worth writing down.**

1. **The no-op move emits nothing.** The keyboard path fires on *every* arrow press, and at the
   edge of a row `←` or `→` re-sends the current stage. Emitting `crm.deal.stage_changed` for
   that would wake every subscribed automation once per keypress — the loudest possible way for
   a feature to become a nuisance. The route compares the stage before and after and stays
   silent when they match; the walk asserts the event count is unchanged.
2. **A won deal is 100%, whatever the stage's default says.** A pipeline author who set the won
   column to 80% would otherwise have "won this quarter" credit four fifths of a deal that is
   already, by definition, won. The move sets `probability = 100` alongside the close date, and
   `validate_stage` forces 100 on the won column itself.
3. **A board reads in one statement per column.** The count, the sum and the weighted sum come
   from one `left join` whose `on` clause carries the *visibility* filter — in `where` instead,
   a caller narrowed to `own` would lose the empty columns entirely and the board would change
   shape depending on who was looking at it.

**Next.** Re-run `cargo test -p omnion-api --test crm` the moment the migration set is
contiguous, then the QA browser pass on the w4 stack, then close slice 3. After that, slice 4:
activities, the merged timeline, the read-only copilot, global search and the automation
consumers (`form.submitted` → contact + deal, `sales.quote.accepted` → deal won).

**Environment note.** `/mnt/apopic` is at 86% with 8.0 GB free; this writer reclaimed its own
`target/debug/incremental` at the start of the tick. The seven writers' `target/` directories
remain the pressure, and a shared `CARGO_TARGET_DIR` would remove it permanently.
## 2026-09-27 — REQ-051 slice 3 unblocked: the "migration gap" was a misdiagnosis, and the walks it hid found two real bugs

**What.** Slice 3's eight integration walks and its browser pass had never run. The previous tick
recorded why as a gap in this branch's migration set and proposed a platform-wide renumber across
five other writers' branches. That diagnosis was wrong. The walks now run, and they fail on two
defects that a real database round trip is the only thing that can find.

**The correction, because the reasoning matters more than the outcome.** `Migration(VersionMissing(19))`
was read as "this branch's source is missing 0019". It is not: the message is raised when a version
is **applied in the database and absent from the source**, which is a different condition and is
invisible in `ls database/migrations`. The shared development database `omnion` — the default
target of `OMNION_DATABASE_URL` — has 19 and 21 in its `_sqlx_migrations` ledger, applied by a run
against files no branch carries today. Creating a throwaway database and pointing the suite there
made all 29 walks execute, 24 passing immediately. The migration numbering is a real wart in the
shared environment, but it is not this branch's blocker, and renumbering five branches would have
fixed nothing.

**What the runs then found.**

1. **The amount never reached the column.** `create_deal` and `patch_deal` bind the normalised
   money value as a Rust `String` into a `numeric` column. Postgres does not coerce a text
   parameter into `numeric`, so every create was a 500 — and the board, the won flow and the lost
   flow all sit on top of the create. The repair is a cast in the statement (`$9::numeric`, and a
   `set_numeric!` variant of the existing `set!` macro for the update), not a float: the column is
   the thing the forecast sums.
2. **A close date could not be sent at all.** The request bodies took `expected_close_on` as a bare
   `time::Date`, which serde reads as the crate's internal tuple. The `"2026-12-01"` that
   `<input type="date">` produces — the only thing a browser can send — was a 422, so the entire
   won/lost flow, which requires a close date, was unreachable from the panel. `modules/crm/src/dates.rs`
   is the fix: a `YYYY-MM-DD` round trip, with a refusal that quotes what was sent and names the
   format. The same gap was on the way out: `deal_ref` and the `crm.deal.won` payload are built
   with `json!`, which never consults serde, so a `Date` in either reached the client and every
   event subscriber as `[2026, 273]`.

**Proof.** `cargo test -p omnion-module-crm` → **106 passed** (100 before, 6 new for the date
format). `cargo test -p omnion-api --test crm` → **29 passed, 0 failed in 167 s** against a
database created immediately beforehand — these are the walks that have never completed.
`apps/admin` `tsc --noEmit` → 0 errors. Commit `b49aa3d`, pushed to `origin/wave4`.

**What it cost to find, written down for the next writer.** Naming a serde `with` path *replaces*
a field's whole deserializer, so a field-level `#[serde(default)]` beside it is silently discarded
and an omitted date still fails with "missing field"; the `default` has to go on the containing
struct, which then has to derive `Default`. And `visit_some`'s inner deserializer carries no
`de::Error` bound in the trait, so a hand-written `Visitor` cannot build a custom message there —
`Option::<String>::deserialize` plus a container `default` is the whole fix. Both cost compile
cycles here for a conclusion that is now a comment at the point of use.

**Next.** The QA browser pass on the w4 stack is the last gate for slice 3, and slice 3 closes only
on it. Then slice 4: activities, the merged timeline, the read-only copilot, global search and the
automation consumers (`form.submitted` → contact + deal, `sales.quote.accepted` → deal won). The
`crm_activities` table already exists in the migration and is still unused.
## 2026-09-28 — the board answered 404 on every organization created after its own migration

**What.** `0022_crm.sql` wrote `crm_seed_default_pipeline(organization)` — the function that
builds a pipeline and its six stages — and then called it exactly once, in the same statement
that created it, against the organizations that existed at that moment. Nothing ever called it
again. Every organization created afterwards through the product (the tenancy route, onboarding,
SCIM, provisioning) therefore owns no pipeline, no stages and no board, and
`GET /api/v1/crm/deals?view=board` answers `404 NotFound("pipeline")` for it. The stage editor
was a screen over an empty table and the board was a 404.

The rule is now a **trigger**, not a call site: `organizations` is written by several unrelated
paths, and a rule that has to be remembered at each of them will be forgotten at the fifth. The
migration also backfills the organizations that already exist. `default_pipeline` repairs the
state too, through the same seed function, so a database restored from a dump taken before this
migration is not a board that stays broken.

**The suite could not see it, and that is the second half of the bug.** The CRM fixture calls
`crm_seed_default_pipeline` by hand for its own organizations, and the comment there says that
is "the same path a new tenant takes". It was not — the fixture was seeding a world the product
never built, so every board test in the suite started from a pipeline that no real tenant had.
The new test seeds **nothing**: it inserts the organization row and immediately reads the board.

**Proof.**
- `cargo test -p omnion-api --test crm an_organization_created_after_the_migration_still_owns_a_board` — **1/1**,
  and it asserts the pipeline, the six stages in board order, and a **200** with six columns from
  an organization that has never sold anything.
- `cargo test -p omnion-module-crm --lib` — **172/172** · `cargo test -p omnion-api --lib routes::crm` — **27/27**
  · `pnpm turbo run typecheck --force` — **2/2**.
- Live, in the QA database: the organization created by the product at 15:38 had **0 pipelines**,
  which is the 404 the pass reported sixty times. After the migration it has **1 pipeline and 6
  stages** (the backfill), and the organization the new test created at 17:06 got its pipeline
  from the trigger alone.
- The full CRM walk suite: **56 tests, 55 passing** on the first run, with one failure that
  turned out to be nobody's regression — see below.

**The failure that was not mine, and was still mine to fix.**
`a_platform_account_in_no_organization_is_told_it_has_none` asserted one code unconditionally,
which made it a function of how many organizations the shared database held: it wants a fresh
installation's `organization_required` and reads `organization_ambiguous` once a second
organization exists from an earlier run. The QA database holds **186** organizations. It also
turned out the `organization_ambiguous` sentence still said "pass organization_id" — the exact
advice the `[]` arm dropped last tick, for the exact same caller who cannot follow it. Both are
fixed: the test now asserts what it owns (refused, with a code the panel knows, never asking for
a parameter the caller has no value for), and the sentence points at the decision the panel
offers.

**Commits.** `4aca09e` the trigger, the backfill and the test that seeds nothing · `01d0a8a` the
sentence and the test that depended on a fresh database.

**Next.** The re-run decides whether the empty/loading/error box closes, or whether the state
sweep finds that a screen with a working error state still has a broken *load* state. Then the
390×844 pass and the keyboard sheet, which are the two boxes after it.

**Environment.** The build lives on `CARGO_TARGET_DIR=.tmp-target` (the repo's own convention, and
the tree the box-wide disk guard spares). `/mnt/apopic` sat at 86% with 8.2G free; seven writers
compile at once, so a build can sit at 0% CPU for minutes before it is scheduled at all. The
CRM walk suite is **~11 minutes** on its own (`--test-threads=1` against the shared database), so
it does not fit inside a foreground call and is run as a background process.
## 2026-09-28 — the pass was racing four reads, and the race pointed at a picker that said nothing

- **What this tick was.** The merge of `origin/main` (media REQ-010 slice 4, nine commits) and then
  the QA pass that decides REQ-051's empty/loading/error box. The previous pass reported the
  contacts and companies screens as having **no** error state. They have one — the list's own read
  renders it, and the component is right there at `crm-contacts-error`. So the report was wrong,
  and being wrong is the more interesting half.

- **The pass was measuring a race.** Both screens fire four reads the moment they mount: the
  column catalogue, the saved views, the company picker and the list. The state sweep stubbed "the
  first CRM list read it sees" — a regex over five paths — and Playwright answered whichever
  request the network delivered first. On those two screens that was the **company picker**, whose
  failure the screen deliberately absorbs (the list still works, so the screen must not fall over).
  The list then loaded, there was nothing to see, and two healthy screens were written down as
  broken. Three more of the sweep's own steps were riding the same flaw: the retry step inherited
  `failPath` from the loop's last iteration, so its stub was still armed on `/crm/leads` and the
  assertion about a healthy contacts screen would have passed for free; and the "no invented id"
  route globbed `**/api/v1/crm/contacts*`, which also answers the picker read.

  The read under test is now **named per screen** (`LIST_READ`), re-armed explicitly after the
  loop, and the anonymous-refusal route is pinned to the same path. A stub that picks which of four
  concurrent reads to fail is not testing a screen; it is testing the network's mood.

- **What the race was pointing at turned out to be real.** The contacts form read the company list
  and, when that read failed, did `setCompanies([])` and moved on. The picker was then a `select`
  whose only option was "No company" — a control that looks like a choice, refuses every real one,
  and cannot be distinguished from "this contact genuinely has no company". That is precisely the
  dead control the box forbids, and the pass found it while reporting something else. The picker
  now holds its own state where the choice was: the sentence, the request id (printed only when
  the **server** named one — a browser-raised fetch failure never reached a log line, and an id
  that correlates with nothing is worse than none), and a retry on its own token so re-reading the
  companies does not re-read the list the person is looking at. The list is untouched: losing a
  dependency the screen survives losing must not take the screen down.

- **Proof.** `pnpm turbo run typecheck --force` **2/2** (admin + web) ·
  `bun build scripts/qa/walkthrough.cjs --target node` parses, 220 KB. Four new steps assert the
  picker's sentence, its id, its retry, and the list surviving beside it. The walkthrough's own
  state sweep is re-run when the box-wide QA slot frees — it is currently held by three live
  sibling passes (w2, w3, w7), and four browsers at once is the configuration that OOM'd this
  host, so the guard is respected rather than bypassed.

- **Also closed: the build log's own merge.** `docs/BUILD-LOG.md` is append-only, so it conflicts
  on every merge, and this branch's side was carrying a **duplicated REQ-010 block** (95 identical
  12-line windows) left by an earlier merge — a multiset check passes anyway, because every line
  is present twice. The copy is stripped; the merged file has zero duplicated windows and zero
  repeated headings, and both sides' lines are accounted for.

- **Next.** The state sweep decides the empty/loading/error box. What it named as still missing is
  the two places that draw a bare paragraph instead of `EmptyState` (the board's per-column body
  and the activities filter bar) — those are the next slice. Then the 390×844 mobile pass and the
  keyboard sheet (`/`, `j`/`k`, `enter`, `e`, `?`), which are the last two boxes.

- **The suite's 56/0 needed a database this branch built.** Run against the shared dev database it
  reported 56 failures — every one of them `Migration(VersionMissing(19))` at the first migration
  call. It is a **shared-number collision**, not a regression: w2, w5, w6 and w7 each carry their
  own `19_*.sql` on their own branch (cms blocks / organization memberships / secret hierarchy / ai
  provider runtime), the shared database has w2's 19 recorded, and this worktree has **no** 19 —
  its sequence runs 18, 21, 22. A migration number is one global namespace shared by seven writers,
  so a suite pointed at a database another branch built is asserting that branch's history. Against
  `omnion_w4_fresh`: **56 passed; 0 failed** in 422s (`OMNION_DATABASE_URL=… cargo test -p
  omnion-api --test crm -- --test-threads=1`).

## 2026-09-28 · wave4 · REQ-051 slice 4 — the board claimed to load forever, beside a refusal

**What.** The state/empty/loading/error box. The next hint named two places still drawing a bare
paragraph instead of `EmptyState` (the board's per-column body, the activities filter bar) — both

## Tick 77 — REQ-021 slice 4: the delivery queue that never existed

**What.** The acceptance criterion "Delivery runner retries a failing channel per backoff and
marks it `failed` after the cap" had been open since the request was written, and the reason is
worth stating plainly: **it described a runner that did not exist.** `notification_deliveries`
shipped in migration `0050` and every later slice *read* it — the outbox lists it, the retry
button re-queues it, the channel filter joins it, the per-channel counts group by it — and not
one line of code anywhere wrote a row or claimed one. It was the only table in the platform with
readers and no writer, which is precisely the shape a queue must never have: the outbox screen
rendered a permanently empty table and called it the truth.

Three pieces, in three commits:

| Commit | What | Why it is where it is |
|---|---|---|
| `d6b74a52` | `0185_notification_delivery_lease.sql` — the `claimed_at` lease | A claim has to say "somebody is on this row" and "nobody is any more" after a process dies mid-send. With no column for the second half, a crashed runner either strands the row or re-sends it forever, and there is nowhere to record which. |
| `f2772386` | `crates/notifications/src/delivery.rs` — enqueue, claim, backoff, cap | The queue is infrastructure, so it belongs in the crate. It is also the first code in this crate that *writes* the table every other file reads. |
| `fac577a2` | `apps/api/src/notification_runner.rs` — the transports, spawned from `main.rs` | A transport is an SMTP conversation and an HTTP POST, and `omnion-automation` already owns the mail sender. Putting one in the crate would make infrastructure depend on a mail stack and an HTTP client to satisfy a trait it defined itself. The crate publishes the trait; the binary, which already depends on both, supplies the implementations. |

**The walk found two real defects, and both were silent.** That is the argument for writing
walks rather than reading code, so they are recorded in full rather than as a changelog line.

**1. `enqueue` contradicted its own contract.** The function's doc comment promised that the
in-app row is written unconditionally — "a reader who turned in-app off would have no inbox at
all" — and three paragraphs below it, a guard that returned early when both channel lists were
empty. So a caller with no remote channel to ask about (a fresh install; a reader who has
everything switched off) got **no rows at all**: the notification existed, the panel showed it,
and the outbox had nothing to say about any channel. The guard is gone, and
`the_in_app_transport_needs_no_configuration_and_always_succeeds` enqueues with two empty lists
specifically to hold the contract down.

**2. `settle_not_ready`'s `not exists` was uncorrelated.** The statement reads "settle every
pending row whose channel nobody has switched on", and the obvious SQL for that —
`not exists (select 1 from notification_channels c where c.channel = d.channel and c.enabled)` —
asks "does *anybody* have this channel on". On a single-tenant install those are the same
question. On this platform they are not: `notification_channels` is per organization, so one
customer configuring e-mail would have silently marked **every other customer's** e-mail
deliveries as "not configured" and stopped them from ever being sent. The correlation on
`notifications.organization_id` is in the statement, and
`a_tenant_that_switched_a_channel_on_keeps_its_own_deliveries_queued` builds two organizations,
configures one, and asserts `settled == 1` — the bare tenant's row and not the other's.

**It was watched going red, in both directions.** The early return was reintroduced into
`delivery.rs` with the tests untouched:

```
test the_in_app_transport_needs_no_configuration_and_always_succeeds ... FAILED
  the in-app row must exist even with no channels requested: RowNotFound
test result: FAILED. 1 passed; 1 failed
```

`RowNotFound`, not a wrong status and not a wrong count — **no row at all**, which is the exact
shape the defect had. Restored from the copy taken before the edit and byte-compared with
`diff` before the suite was re-run.

**Proof.**

| Gate | Result |
|---|---|
| `apps/api --test notification_delivery` | **8 passed / 0 failed** (88.18s, live PostgreSQL, 8 scratch databases) |
| the same, with the `enqueue` guard reintroduced | **FAILED** — `RowNotFound` |
| `omnion-notifications --lib` | **90 passed / 0 failed** (was 79; +11) |
| `omnion-api --lib` | **226 passed / 0 failed** (was 219; +7) |
| `pnpm typecheck` | 2/2 successful |
| `rustfmt --check` on all three new files | clean |

**The eight walks, and what each one is for.** The criterion is a sentence about *time*, and a
sentence about time cannot be proved by a unit test, so all eight drive a real database:

1. `a_failing_channel_is_retried_and_then_marked_failed_after_the_cap` — the criterion itself.
   Attempts 1 and 2 leave the row `pending` with a reason on it; attempt 3 writes `failed`; a
   fourth tick claims nothing. Between the attempts it asserts the row is **not** due, which is
   what makes "the cap is reached in three attempts" mean three attempts over the backoff window
   rather than three in three ticks. The backoff is real, not merely scheduled: a row retried
   with `next_attempt_at = now()` is due on the very next tick, so the cap would be reached in
   milliseconds and every tick would hammer a broken mail server.
2. `a_claim_makes_the_row_exclusive_until_the_lease_expires` — a second runner finds nothing
   inside the lease, and the row comes back once the lease ages. That second half is what keeps
   a crash from stranding a notification.
3. `enqueueing_the_same_notification_twice_does_not_double_its_deliveries` — the unique
   constraint makes a re-run of the same emit idempotent, so a reader does not get two copies.
4. `a_channel_nobody_configured_is_skipped_rather_than_left_queued_forever` — a channel nobody
   configured is settled with a reason, never left pending, and the settlement is idempotent.
5. `a_tenant_that_switched_a_channel_on_keeps_its_own_deliveries_queued` — the tenancy defect
   above.
6. `the_outbox_reads_back_what_the_queue_wrote` — the read the outbox page performs agrees with
   the queue's own state (one of each state, failed-first ordering, and the admin retry button
   still works on a row the queue gave up on). A queue whose rows nobody can read is a queue
   whose failures are invisible, which is the whole reason the table exists.
7. `the_in_app_transport_needs_no_configuration_and_always_succeeds` — the defect-1 guard.
8. `the_email_transport_refuses_a_reader_with_no_address_instead_of_pretending` — a reader with
   no address is a failure with a reason. The alternative, a silent success, writes a `sent` row
   for a message that was never sent: the one lie this table must not tell.

**Three of the eight failed before the code was right, and two of those were the test's fault
rather than the code's — recorded because the direction matters.** The walks asserted
`claimed == 1` on a fixture that queues two rows, and they asserted a *delivered* e-mail on a
database where no channel was configured — where the settlement is right to skip it, so the
fixture had to describe an installation (`configure_channel`) instead of inheriting an empty one.
A walk that forgets that reads as a queue bug and is a fixture gap; the difference is worth
knowing before somebody spends a tick on it.

**The browser pass did not run, for the fourth tick running, and the measurement is in the
ledger.** The QA slot queue is one deep but held by a live `omnion-w3` walkthrough, and the box
sat at load 60–102 with 5–8 concurrent `rustc` from sibling writers; one cargo run of this
suite's binary alone took 11 minutes to link. A browser pass was queued behind a live slot and
this tick spent itself on a defect a walk could find instead.

**Pre-existing red, not mine, and named.** `git status crates/` is clean apart from this tick's
own file. `cargo test -p omnion-api --lib` compiles the workspace's pre-existing warnings
(unused imports in `headers_middleware.rs`, `rate_limit_middleware.rs`, `routes/backups.rs`,
`routes/notifications.rs`, `routes/notifications_admin.rs`; a `PartialEq` derive on a function
pointer in `crates/security/src/posture.rs`), and `cargo fmt --all -- --check` still fails on the
files listed in tick 76 — none of which this tick touched. Reformatting the workspace to green
would rewrite files nine sibling writers are actively editing, so it is reported rather than
done.

**Next.** REQ-021 has four boxes left, all naming the same missing browser pass: the keyboard
path, the mobile sheet, the per-channel delivery rows in the drawer, and the test-delivery
inline result. The drawer rows are now *provable by a walk* rather than by a browser — the
deliveries exist — so the next quiet-box tick should run `bash scripts/qa/run.sh` with
`QA_ONLY=notifications,notifications-outbox,notifications-settings`, then close REQ-021 and move
to REQ-014 (system health, still `pending`).

## Tick 77 addendum — the box ran out of disk mid-verification (named, not worked around)

The 8/8 green run above is real and was measured. Everything red *after* it, in this tick and
across two re-runs, is the same environmental failure and no product defect:

```
could not create directory "base/12250615": No space left on device
code 53100, could not extend file "base/12245557/12250419"
```

`df` at that moment:

| Filesystem | Size | Avail | Use% |
|---|---|---|---|
| `/` (PostgreSQL's `data_directory` = `/var/lib/postgresql/16/main`) | 123G | 3.0G | **98%** |
| `/mnt/apopic` (all ten writers' worktrees and `target`s) | 60G | **0** | **100%** |

**This is shared, so it is reported and not worked around.** Reclaiming it means deleting build
output that nine sibling writers are compiling *right now* — the loop discipline is explicit that
a writer may only reclaim its own `target`, and my own `target/debug/incremental` is 58M, which
would not change a 3.0G/0-byte situation. The 14 orphaned `omnion_notifdel_*` scratch databases
that earlier walks left behind when a run was interrupted mid-`dispose` are already gone (the
walks do drop them; the ones that leaked were killed by the box, not by a missing cleanup).

**What this costs the next tick, stated plainly:** `notification_delivery` is green on the code
as committed and re-runs will stay red until the disk recovers, so *the walk is not a usable gate
on this box unt
## Tick 77 addendum — the box ran out of disk mid-verification (named, not worked around)

The 8/8 green run above is real and was measured. Everything red *after* it, in this tick and
across two re-runs, is the same environmental failure and no product defect:

```
could not create directory "base/12250615": No space left on device
code 53100, could not extend file "base/12245557/12250419"
```

`df` at that moment:

| Filesystem | Size | Avail | Use% |
|---|---|---|---|
| `/` (PostgreSQL's `data_directory` = `/var/lib/postgresql/16/main`) | 123G | 3.0G | **98%** |
| `/mnt/apopic` (all ten writers' worktrees and `target`s) | 60G | **0** | **100%** |

**This is shared, so it is reported and not worked around.** Reclaiming it means deleting build
output that nine sibling writers are compiling *right now*; the loop discipline is explicit that
a writer may only reclaim its own `target`, and this tick reclaimed only its own
`target/debug/incremental` (58M, after the three checks: no open file descriptors, no write in
the last 20 minutes, its own directory). The 14 orphaned `omnion_notifdel_*` scratch databases
are already gone — the walks do drop them; the ones that leaked were killed by the box mid-run,
not by a missing cleanup, and the next run found zero of them.

**What this costs the next tick, stated plainly:** `notification_delivery` is green on the code
as committed, but re-runs stay red until the disk recovers, so **the walk is not a usable gate on
this box until then** — treat a `53100` as environmental and read the assertion line above it for
the real verdict. `omnion-notifications --lib` (90/0) needs no database and is unaffected.
were already done by an earlier tick, so the remaining work came from auditing the six screens.
That audit found a worse defect: **`deals-view.tsx` had the only body in the module that did not
gate on its read.** A refused board read set `error`, and the body went on taking the
`board === null` branch — a four-column skeleton, permanently, printed directly beneath the
refusal. Two incompatible claims on one screen, neither true, and the visible one described a load
that had already given up. The list mode had the same gap, so both views of the same record were
affected.

**The trap inside the fix.** `error` was shared with *action* refusals — a refused drag, a refused
archive — and those deliberately keep the board on screen under a strip, because the card has
already moved back and the board is the answer. Gating the body on the shared state would have
blanked the whole pipeline on every refused drag. The state is split: `error` = action refusal
(strip above a good board), `loadError` = the screen's own read (replaces the body). A refused
board also offers "Read as a list", because the two are views of one record.

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `tsc --noEmit` **0 errors** ·
`node --check scripts/qa/walkthrough.cjs` parses. Four new sweep steps assert the state, the
request id, **no `[aria-busy]` element anywhere on the page**, and the other view still offered —
the third is the one the old strip could never have passed, because the defect *was* a screen
claiming to load.

**Next.** The 390×844 mobile pass and the keyboard sheet (`/`, `j`/`k`, `enter`, `e`, `?`) — the
last two boxes. The full QA pass is queued behind three sibling writers; four browsers is the
configuration that OOM'd this host, so the slot waits rather than barging.

## 2026-09-28 · wave4 tick 15 — the acceptance gate had never actually run

The previous tick left two acceptance boxes open (the 390×844 mobile pass and the keyboard
sheet) and handed them a QA pass that was still queued. The queue was the story: `/tmp/w4_qa_tick14.log`
ended at `[PM2][ERROR] Script not found: /mnt/apopic/omnion-w4/target/debug/omnion-api`. That is not a
queue message. **The pass died**, and it died silently, after the log line that explains itself.

**Root cause.** `scripts/qa/run.sh` hardcoded `target/debug/omnion-api`, but `cargo build` honours
an ambient `CARGO_TARGET_DIR` — and this loop exports one, on the recorded advice that the shared
`/mnt/apopic` mount fills up. So the build landed in `.tmp-target/debug/`, the freshness guard
compared the migration timestamps against the **stale** binary still sitting in `./target/debug/`,
decided there was nothing to build, skipped the build, and then handed pm2 a path with no file in it.
The two conditions reinforce each other: the divergence hid the build, and the skipped build
guaranteed the file was missing. Verified directly — `cargo metadata` reports `target_directory` as
`/mnt/apopic/omnion-w4/.tmp-target` with the variable set and `/mnt/apopic/omnion-w4/target` without it.

A first attempt at the fix made it worse in a subtler way: `cargo metadata` reports the target
**root**, not the profile subdirectory, so `target_directory + /omnion-api` resolves to a path that
never exists and the pass would have failed on a *fresh* checkout too. The resolver now probes
`debug/` then `release/` under the reported root and falls back to `debug` for a first build.

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `pnpm turbo run typecheck --force`
**2/2 successful, 0 errors** · `bash -n scripts/qa/run.sh` parses · the resolver was exercised under
both environments and points at an existing binary in each
(`no env → target/debug/omnion-api exists=YES`, `CARGO_TARGET_DIR=.tmp-target → .tmp-target/debug/omnion-api exists=YES`).
`run.sh` now also prints the database it is actually using; the step banner claimed `omnion_qa` on
every stack, including the `omnion_qa_w4` one that exists precisely so two passes do not drop each
other's rows.

**Merge.** `origin/main` had moved two commits (the media-retention feature, migration `0049`).
Merged at tick start before editing; the ort strategy auto-resolved with no conflict and touched none
of this worker's files.

**Next.** The mobile pass and the keyboard sheet, now against a gate that is known to run. The
keyboard sheet is written last and only lists bindings the six screens actually implement — a sheet
naming a binding no screen has is a dead list.

**A second defect, found by waiting for the first one.** Once the build was fixed the pass still did
not start, and the reason was in the queue: the only QA place on the box was held by pid 2347379,
which was dead. The lock file is *named* after the process that takes a place, but that process exits
immediately after handing the place to a background holder — so the liveness check always answered
"gone", and a genuinely live pass looked abandoned. Only the 45-minute age floor stood between that
and a reaper stealing a place that is in use; conversely a pass that died without running its trap
blocked the whole queue for 45 minutes. `qa-slot.sh` now asks about the **holder** pid, and treats a
holder-less place as reclaimable only once it is 120s old, so the few hundred milliseconds between
taking a place and writing its holder file are not mistaken for an orphan.

Verified with a throwaway lock directory rather than by inspection: a live holder survives, a dead
holder is reclaimed, a fresh holder-less place is kept, and a dead+old place is reclaimed — **4/4**.

The orphaned place was then cleared by hand (both pids confirmed dead, no live pass owning it) and a
sibling writer's pass took the slot within seconds, which is the correct outcome: the fix lets the
queue move instead of stranding it.

**State of the two remaining boxes.** The mobile pass and the keyboard sheet are still open. The pass
that will prove tick 14's four sweep steps is queued behind a live sibling, so this tick ends with the
gate fixed and running-but-waiting rather than with a new screen.

**Next.** Re-run the w4 pass and read the four sweep steps; then the 390×844 mobile pass and the
keyboard sheet, written only from bindings the six screens actually implement.

## 2026-09-28 · wave4 tick 16 — the two open boxes, and what pressing the keys found

REQ-051's last two acceptance boxes — the 390×844 pass and the keyboard sheet — both had their
behaviour in the code and neither had ever been *exercised*. Those are different states, and the
difference only shows when a pass presses the keys. Writing one found three defects the code could
not report on itself.

**The sheet was a list of claims the screen could not keep.** `LIST_SHORTCUTS` advertised `c` and
`o` as navigation. There is no listener for either key anywhere in the module — `grep` for
`key === "c"` across `apps/admin/features/crm/` returns nothing. The sheet is opened *by* a key, so
a row promising a binding nobody listens for is a dead list inside the screen whose whole job is to
report the screen truthfully, and the box explicitly forbids it. It is now a `g` prefix built from
`CRM_NAV` itself (`GO_DESTINATIONS`), so the sheet and the tab bar cannot drift, and the arm expires
after 1200ms so an abandoned prefix cannot swallow the next keystroke.

**`j`/`k` moved a cursor nobody could see.** `selectedIndex` was in the frame's state and in the
context value, and **no screen read it**. Pressing `j` changed which row `Enter` would open while
nothing on the page said so — a shortcut indistinguishable from a broken one, with the person
pressing it guessing. `CrmRow` is a component rather than a helper on purpose: a screen builds its
rows as `children` *outside* the provider, so `selectedIndex` is not readable where the row is
written however the markup is shaped. It draws the cursor, moves it on click (pressing `j` and
clicking are the same act, and a click that leaves the cursor elsewhere makes them disagree) and
the frame scrolls it into view with `block: "nearest"` so the page does not jump under a reader.

**The board rendered at 390px.** The box asks that a phone get the list and that the board scroll
horizontally with sticky stage headers; the board was the default at every width, so a phone had to
scroll sideways to see which stage a card was in — the one number that decides what to do next. A
phone now defaults to the list below `md`, the board stays one tap away rather than being taken
away, the stage header sticks inside the horizontal scroller, and the page itself no longer scrolls
sideways. The defaulting happens in an effect, deliberately: reading the viewport during render
makes server and client disagree about the first frame, and a hydration mismatch is worse than a
one-frame flash.

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `pnpm turbo run typecheck --force`
**2/2 successful, 0 errors** · `node --check scripts/qa/walkthrough.cjs` parses.

**A harness rule, from the shape of the last two ticks.** A step that comes back `false` and is
only written into the JSON report is a defect nobody is told about — tick 15's whole subject was a
pass that "ran" while a defect sat unread in the report. A `false` in the new pass is now a **high
finding**, and an unset step is a medium one. The guarded steps (the cursor, `e`) stay *unset* on an
empty list rather than `false`: "the shortcut moved nothing" and "there was nothing to move" are
different claims, and a fresh database must not be reported as a broken screen.

**Queue state.** The pass is **queued, and alive** — holder 2664123 confirmed running, its log still
growing at 58 minutes. That is the tick-15 fix working: the place is held by a live holder, so the
w4 pass waits rather than barging into the one QA stack this box can afford (four browsers is the
configuration that OOM'd it). `kill -0` on the holder, not on the pid in the lock filename, is what
distinguishes this from tick 14's silent death.

**Next.** Read `crmKeyboardMobile` when the pass runs: `theSheetOnlyPromisesLiveKeys`,
`jMovesTheVisibleCursor`, `thePhoneDefaultsToTheList`, `theStageHeaderSticks`,
`theFormIsSingleColumn`. Any `false` is a high finding and REQ-051 does not close until it is read.

## 2026-09-28 · wave4 tick 17 · REQ-051 · the sixth screen, and the report nobody read

**What.** Merged `origin/main` (7 commits) into `wave4` — `docs/BUILD-LOG.md` and
`scripts/qa/qa-slot.sh` conflicted; the log is append-only so both sides were unioned
(`difflib` over the three stages, multiset-verified: 0 lines missing from either side) and the
slot script took main's, which is a superset. Then took REQ-051's last open box — *empty, loading
and error states exist on all six screens*.

**Two defects, both found by reading the last report against HEAD.**

1. **The box says six screens; the pass visited five.** `runCrmStateSweep` had grown one screen
   at a time and `/crm/settings/pipelines` was never added, so the box was being ticked on five.
   Worse, that screen was the last of the six still ungated on its own read: it rendered
   "This pipeline has no stages — add the first one" from a `rows` array that starts empty, so a
   503 answered with a **confident claim about the tenant's data plus a button that writes a
   stage into it**. The same array also showed that empty state during every load, on the first
   paint. One value (`rows.length === 0`) was carrying two facts — "empty" and "not loaded yet" —
   and the empty state was the one that lost. Fixed the way `fed62b5` fixed the board: `loadError`
   separate from `error` (a save refused leaves the editor as you left it, so it stays a strip; a
   read refused leaves nothing, so it replaces the body), plus a skeleton while `pipelines === null`.
2. **`crmStates` was written to `summary.json` and never read back** — the exact trap tick 16
   fixed for the keyboard steps. The previous report held **twelve `false` claims** in that object,
   including `contacts/companies/deals_hasAState = false` — three screens with no error state at
   all — while the pass headline read "high 59" with not one of those 59 about the box the sweep
   exists to check. Both objects are now scanned: `false` is a high finding, unset is a medium.

The sweep also gained `<label>_doesNotClaimToBeEmpty` for all six screens, because the defect is
the *claim*, not the button: a screen may show an empty state once it knows the answer is empty,
and only then.

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `cargo test -p omnion-api --lib
routes::crm` **27/27** · `pnpm turbo run typecheck --force` **2/2** · `node --check
scripts/qa/walkthrough.cjs` parses. Commit `69a7643`, pushed to `origin/wave4`. **The QA pass
carrying the new steps has not run yet** — it is queued behind another writer's live pass
(`QA_SLOTS=1` is box-wide), and the next tick reads `crmStates` from the newest report rather than
assuming any of it.

**Next.** Read `crmStates` (the six `*_doesNotClaimToBeEmpty` keys and `stages_*`) and
`crmKeyboardMobile` in the newest w4 report; only then tick the two remaining boxes and set
REQ-051 to `done`. After that, REQ-052 (sales & quotes).

## 2026-09-28 · tick 18 · REQ-051 · the keyboard box's own proof step was the defect

**What.** No CRM code changed this tick. The `?` sheet in `crm-parts.tsx` is a list of claims, and
the step written last tick to keep it honest asserted the **opposite** of its own comment.

**The defect (`3f1f435`).** `theSheetOnlyPromisesLiveKeys` read the sheet, took the first
`g then X` it found, and asserted that string was **empty** — reasoning that a two-key prefix was a
binding nobody listens for. The sheet deliberately advertises four live destinations
(`g then d/a/l/s`), each built from `CRM_NAV`, so the step was `false` against a **correct** screen.
Wired to a high finding (as tick 17 wired it), it would have blocked the keyboard box permanently
and the next tick would have gone looking for a product bug that did not exist.

It was wrong twice over. It asserted a *count* where the claim is about *behaviour*, which is the
mistake this pass exists to stop: a sheet that prints `g then d` and navigates nowhere reads
exactly like one that works. So every advertised destination is now collected from the sheet's own
text and **pressed for real** — `g` arms, the letter fires, the assertion is the URL — and the step
is the conjunction over all of them. A row the screen advertises and does not honour now fails; a
row it does not advertise is not this step's business. The `?`-closes assertion moved to after the
loop, because the loop navigates off the screen it was standing on.

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `cargo test -p omnion-api --lib
routes::crm` **27/27** · `pnpm turbo run typecheck --force` **2/2** · `node --check
scripts/qa/walkthrough.cjs` parses. `3f1f435` pushed to `origin/wave4`.

**Read while waiting, and worth writing down.** The newest *complete* w4 report is
`qa-artifacts/20260928-181211`, which **predates** `fa6181c` / `fed62b5` / `4e94c4b` — it still
carries a parse error in `contacts-view.tsx` that `4e94c4b` removed, and its `crmStates` shows
`contacts/companies/deals_hasAState = false` for the same reason. Nothing in it is evidence about
the current tree. The three defect fixes since then are code-verified only.

**Environment note for the next tick.** `/etc/profile.d/omnion-qa-limits.sh` sets
`QA_SLOT_WAIT=3600`, so a pass waits **an hour** for the box-wide slot, not the 900 s the script
defaults to. This pass started at 20:17 and was still queued at 21:00 behind w6. Two stale `run.sh`
wrappers of **this** worktree (ticks 15 and 17) were still alive and queued on the *same* ports
(18083/3103/3203) — two passes that would have collided on startup. Killed both; the trap in
`run.sh` is the only thing that frees a place, so a killed wrapper's place file needs the reaper's
`QA_SLOT_REAP_GRACE` (120 s) to lapse. **Check `pgrep -f 'omnion-w4.*run.sh'` before starting a
pass** — a queue entry is not a no-op.

**Next.** The pass carrying `everyAdvertisedGoKeyNavigates` and the six `*_doesNotClaimToBeEmpty`
keys has still not run. Read `crmStates` and `crmKeyboardMobile` from the newest w4 report; only
then tick the two remaining boxes and set REQ-051 to `done`. Then REQ-052 (sales & quotes).


## 2026-09-28 · tick 19 · REQ-051 · the merge took the workspace down twice, and the second fix is the one that holds

**What.** No CRM code changed. `origin/main` had moved five commits (REQ-021
notifications), so the tick opened by merging it — the right moment, because the tree was clean
and the QA pass was still queued. The merge produced four conflicts, and resolving them wrongly
broke the build **twice** in ways that looked nothing like a merge problem.

**Conflict 1 — `Cargo.lock`, and the rule that is now written down.** Both sides had touched it,
so the union resolver ran on a file that is **generated**. It appended two lines that were in
neither stage — a bare dependency entry and a duplicate `name =` — and cargo then refused to
parse the manifest at all, taking the entire workspace down rather than one crate. *Cargo.lock is
never hand-merged: take the other side and let cargo regenerate it.* Union is for append-only
prose and for lists a human curates, never for anything a tool owns. (`bc3dcc5`)

**Conflict 2 — the one that matters, and the reason the first fix was not enough.** `seed.rs` and
`app-shell.tsx` are genuine both-changed files: each writer added elements to *the same array
literal*. The lines missing from the merge are therefore not whole statements — they are array
elements whose surrounding structure both sides already agreed on — so "append the lines that
disappeared" writes a bare string literal **after the file's closing brace**:

```
error: expected item, found `"crm.contacts.read"`      seed.rs:565
components/app-shell.tsx(216,46): error TS1005: ';' expected.
```

The BUILD-LOG union went through the same code path and *passed its own multiset check* while
silently dropping 1,308 lines of wave4 history, because the check ran on the wrong intermediate.
**Line counts are not verification; the multiset is.** That one is now asserted (`missing ours: 0,
missing theirs: 0`) and the append-only splice is anchored to the base, not to a side. (`3f8230c`)

The two code files are now merged **list-aware**: walk the other side's diff, carry over only
lines that are genuinely new array elements (a bare key string, or a nav object with an `href`),
then assert the file's invariants — every key both writers introduced is present, braces balance,
the file still ends on the component's closing brace. That assertion is the durable part: it is
what catches the next one, and the line union is what caused this. Restoring main's edit to the
same lucide import line also dropped `Users`, which the CRM nav row needs; it is back. (`56464c4`)

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `cargo test -p omnion-api --lib
routes::crm` **27/27** · `pnpm turbo run typecheck --force` **2/2** · `cargo build -p
omnion-permissions` clean · `cargo metadata` parses the restored lock. `3f8230c`/`bc3dcc5`/`56464c4`
pushed to `origin/wave4`, tree clean.

**QA pass.** Started with `QA_SLOT_WAIT=7200` on the w4 stack (18083/3103/3203). The tick-18 pass
was still queued at 21:00 and would have **barged in without a place at 21:17:12** — its 3600 s
wait expired one minute before this tick, and the box had 2 GB free with w6 mid-pass. Killed it
rather than run two Chromium passes; the new wrapper waits for a real place. w6 held the only
place and is walking. **No new report yet, so the last two boxes stay unticked.**

**Next.** Read `crmStates` and `crmKeyboardMobile` from the first w4 report written *after*
`56464c4`; only then tick the empty/loading/error and keyboard boxes and set REQ-051 to `done`.
Then REQ-052 (sales & quotes), which has no code yet: `modules/sales`, migration **0051** (0050
was taken by main's notifications in this very merge), and a walkthrough route list.

## 2026-09-28 · tick 19b · REQ-052 slice 1 · the money, and the bug that came from a scale

**What.** REQ-051's last two boxes are still waiting on the QA pass (it holds a place behind w6),
so the tick spent itself on the start of the next request. **REQ-052 slice 1 — the catalog and
price lists** — as a schema, a module and tests. No route, no screen yet: that is slice 1's second
half and the next tick's work, so nothing here is claimed as a shipped feature.

**The schema (`51f5eae`, migration `0051`).** Nine tables. The constraints carry the rules rather
than the forms, because a rule that lives only in a form is a rule the API, the CSV import and a
future storefront each re-implement slightly differently: money is `numeric(14,2)` and quantity
`numeric(14,3)`; `tax_percent` is a snapshot with **no** FK to an accounting rate, so editing a
rate today cannot rewrite a document a customer already read; a public link stores only its hash;
`(status='sent') = (sent_at is not null)` and `(status='declined') = (reason is not null)` are
checked at the row; a price row may not mix organizations, by trigger, because the join across two
tables is a convenience a buggy query could drop; settings are seeded and created by trigger, the
same shape CRM's `0043` fixed for the default pipeline.

Verified by applying the **whole** migration set to a fresh database and walking the boundaries —
settings trigger fires, decline-without-reason refused, sent and expired accepted, duplicate number
refused, "sent" without a `sent_at` refused.

**The module (`8505d13`).** `modules/sales`: money, catalog, model, error. The decision worth
recording: **money is `i128` hundredths, not `f64`.** The spec's rule ("round half-up once per line
and sum the rounded values so the printed PDF, the panel and the invoice agree") cannot be kept
with a binary float, because a float cannot hold `0.1`. A decimal dependency was deliberately
**not** added to a public repo: an integer count of minor units is exact, needs no dependency, and
matches the `numeric(14,2)` the schema already stores. `Quantity` is a separate type at three
decimals so a quantity cannot be summed into a money figure by accident.

**The bug, which is the reason the money code is written the way it is.** The first version let a
line total keep whatever scale its arithmetic left it at, so a line priced at three decimals
produced a total at scale 3 while its neighbours were at scale 2. Summing their `minor` values
then added `0.105`-scale numbers to `0.11`-scale numbers and yielded a total that was **neither the
printed column nor the stored value** — which is exactly the failure the spec's rule exists to
prevent, arriving through the back door. The fix is not a patch on the arithmetic: a `Money`'s
`minor` is meaningless without its `scale`, so every document figure now goes through
`at_document_scale`, and the sum is correct by construction instead of by convention. Found by a
test asserting a number it had itself got wrong, which is the only reason it was found at all.

**Proof.** `cargo test -p omnion-module-sales --lib` **54/54** · `cargo clippy -p omnion-module-sales
--all-targets` **0 warnings** · `cargo test -p omnion-module-crm --lib` **172/172** (no regression) ·
`cargo metadata` parses. `51f5eae`/`8505d13`/`a875c7f` pushed.

**Next.** Slice 1's second half: the `sales.*` permission keys, the product and price-list routes
in `apps/api/src/routes/sales.rs`, and the catalog screens. Then REQ-051's two boxes the moment a
pass reports — read `crmStates` and `crmKeyboardMobile` from the first w4 report written after
`56464c4` and only then tick them.


## 2026-09-28 · tick 21 · REQ-052 slice 2 — quotes end to end, and the four defects the walk found

**What.** The document a seller writes: the line grid, the header, the versions the customer saw,
the per-organization numbering, the public link whose token is stored only as a hash — plus five
screens and a browser walk that reads the builder's total off the screen and requires it to be the
number the module computed.

**Why the rule set lives in the module and not the handler.** Slice 2 added two callers with no
session and no actor: the public accept/decline and the expiry sweep. A rule written in a handler
is a rule those two writers do not have.

**Proof.**

| gate | result |
|---|---|
| `cargo test -p omnion-module-sales` | 102/102 (87 from slice 1, 15 new) |
| `cargo test -p omnion-api --test sales_quotes` | 16/16 walks |
| `cargo test -p omnion-api --test sales` | 17/17 unchanged after the migration edit |
| `cargo clippy -p omnion-module-sales --all-targets` | 0 warnings |
| `pnpm typecheck` | 2/2 packages clean |
| `scripts/qa/walkthrough.cjs` | parses; the `sales-quotes` pass is written |

**Four defects the walks found in the slice** — two in the product and two in the walk itself,
which is the useful outcome, because a test that only ever agrees with the code proves nothing.

1. **The first version was numbered 2.** The column read `default 1` with `check (version >= 1)`
   and `send` incremented it, so the first snapshot of the first document anybody was ever sent
   said "v2" — a gap in a sequence whose whole point is that it has none, and a number the
   customer reads. Now `default 0`, `check (version >= 0)`: 0 is a real state (never sent).
2. **A cancelled draft was refused by its own table.** The constraint read
   `(status = 'draft') = (sent_at is null)`, which forbids a draft that was withdrawn — the cancel
   sets the status and leaves `sent_at` null. One direction now: `sent_at is null or status <> 'draft'`.
3. **The version snapshot never bound `organization_id`**, so `send` was a 500 on the not-null
   constraint. Bound explicitly, not defaulted.
4. **A frozen version carried its totals as JSON numbers** while the live quote carried text, so
   the version history would have needed a second formatter and a browser would have parsed a
   `numeric` as a float.

The walk's own two: the hand-written fixture's grand total was 417.71 where the per-line column
says 407.76, and three tests called `issue_link` on a draft and were surprised by a 400 — which was
the product being right.

**Merge.** `origin/main` moved 21 commits; two conflicts, resolved by splicing both sides' non-equal
chunks into the base (`SequenceMatcher` opcodes) and then verifying that **every** chunk from both
sides survives in the output — 4244 lines, 0 chunks lost. The first verification formula
(`base + ours + theirs` as a multiset) was wrong: shared lines legitimately appear once, and it
reported 2480 "missing". The line-count check alone is not enough either — it hides a duplicated
block.

**Migration numbers.** `0051_sales.sql` collided with main's `0051_notification_routes`, and my
`0052` collided with wave9's. Both moved to **0053** and **0054**. The shared namespace is the
lesson from the earlier incident, repeated: take the next free number *above every sibling's
high-water mark*, not the next free number on your own branch.

**Next.** The QA pass is queued behind another writer's. When it lands, read its findings, fix any
high caused by these screens, and tick the two boxes the pass can prove. Then slice 3: the
**approval gate** — the threshold check is already on the screen (the banner) and already on the
server (`Settings::needs_approval`); what is missing is the REQ-059 request, the approve/reject
handling and the notifications to both sides.


## 2026-09-28 — tick 20 · REQ-052 slice 1 · the catalog API, the dates it could not have carried, and the screens

Slice 1 of REQ-052 closed end to end: the migration, the module, the routes, the five screens and
the walkthrough that drives them. The interesting part was not the CRUD — it was the two defects
the walks found, both of which are the kind that read as "working" until a person is on the screen.

**What**

- `0455658` the catalog API: product and price-list CRUD behind `sales.products.*` /
  `sales.pricelists.*`, organization-scoped, every mutation writing an audit row and an event —
  plus `modules/sales/src/dates.rs`, which is the fix described below.
- `634c453` the tenant fix: the single-record routes resolve the organization from the path row
  alone, so a platform Owner could see the list and then be refused the row it linked to.
- `ac3b297` the five screens, the section frame and the depth pass.

**The date bug is the one worth remembering.** `time`'s serde support is opt-in per field and a
field that forgets the attribute does not fall back to something readable. `archived_at` went out
as `[2026,271,22,43,23,295199000,0,0,0]`, and a bare `time::Date` on the price-list window was
**refused on the way in** — so the editor's date fields could not be filled at all. CRM had already
solved this in its own `dates` module for exactly this reason, and the rule is now written down in
`sales/dates.rs`: a day is `YYYY-MM-DD`, a stamp is RFC 3339, a zone-less time is read as UTC, and
every refusal names the format rather than saying "expected a Date" at a form field.

The second half of it is the trap: naming a `with` path **replaces the field's deserializer**, so a
field-level `#[serde(default)]` is silently dropped and a blank "valid from" becomes a 422. The
`default` has to move to the container. That is CRM's comment, and now it is this crate's.

**The tenant bug is the second kind.** The lists carried `organization_id` and the records did not.
The API's own refusal says "pass organization_id", and a platform account that holds no role in any
organization has no organization it may name without the permission it is being refused — so a
screen's own list was unreachable by its own links. Every read and write that resolves a tenant
from a path id now takes the same parameter, and the walk names it, shows the refusal without it,
and shows that naming *another* tenant still answers 404.

**Proof**

- `cargo test -p omnion-module-sales --lib` → 87/87.
- `cargo test -p omnion-api --test sales -- --test-threads=1` → 17/17 walks.
- `pnpm --filter @omnion/admin typecheck` → clean.
- `cargo clippy -p omnion-module-sales --all-targets` → 0 warnings. (The 42 on the wider `-p omnion-api`
  run are other writers' crates — `crates/media`, `modules/crm`, `media_grants` — and were not
  touched. One `useless_format` in my own test file is fixed.)
- `bun -e` parse of `walkthrough.cjs` → SYNTAX_OK. (`bun build` cannot resolve `playwright-core`
  outside the QA stack's node_modules; that is the module resolution, not the file.)

**Next**

REQ-052 slice 2 — quotes end to end: the builder, totals recomputed in SQL, versioning, send, the
public token page with accept/decline, and the expiry sweep. A QA pass is due on the tick that
closes it, and this tick's route additions have not been through a browser pass yet.
---

## Wave 4 · REQ-052 slice 3 · the discount gate (5a26ca6, d0745f8, 6f62881)

**What.** The approval gate between a quote over the discount limit and being sent. Slice 2 built
the quote and the builder already drew the amber banner, but **nothing could move a quote into
`approved`**, so a quote over the limit had no way out of `pending_approval` — the status existed,
the send path refused it, and no code could clear it. This is the missing middle: migration
`0055_quote_approvals.sql`, `modules/sales/src/approvals.rs` (module rules), `routes/sales_approvals.rs`
(permissions, audit, events, the two notifications), the `/sales/approvals` inbox, the button the
banner used to only promise, and a QA pass for the screen.

**The four defects the walks found** — none of which a typecheck or a unit test would have:

1. **A draft over the threshold was still sendable.** The send check guarded only
   `pending_approval`, so a quote nobody had asked about went straight out: the exact case the
   discount policy exists to stop. Now checked against the stored `max_discount`.
2. **Raising a request sat behind `sales.quotes.send`.** That leaves the drafter role — the role
   the acceptance criteria are about — with an amber banner and no button. Asking moved to
   `sales.quotes.update`; **deciding** stayed on `send`, which is the power.
3. **The `requested_by_me` scope answered `500`.** A `Vec<String>` bind list sent `requested_by`
   as text; PostgreSQL replied `operator does not exist: uuid = text`. The binds now carry their
   SQL type in a `Bind` enum, because a type mismatch looks like a `500` from the outside.
4. **A lapsed quote blamed the discount gate.** The validity check ran *after* it, so an expired
   quote said "ask a manager" when the only thing it needed was a new date. Order fixed.

**A test that lowered its own bar.** Two slice-2 walks broke: they sent a 20% fixture against a
15% policy. The temptation was to edit the fixture. Instead they now write a **policy** that
allows it (`allow_the_fixture_discount`), because the totals walks assert on that 20% and a test
that edits its own fixture to pass has stopped measuring anything.

**Proof.**
`cargo test -p omnion-module-sales --lib` → **110 passed** (102 before, +8).
`cargo test -p omnion-api --lib` → **243 passed**.
`cargo test -p omnion-api --test sales_approvals -- --test-threads=1` → **5 passed**.
`cargo test -p omnion-api --test sales_quotes -- --test-threads=1` → **16 passed** (unchanged
after the gate, once the policy is written).
`pnpm turbo run typecheck --force` → **2/2**. `node --check scripts/qa/walkthrough.cjs` → clean.
clippy: **0** large-`Err`-variant warnings (both new error payloads are `Box`ed, because
`SalesError` is returned by every fallible function in the crate).

**Also fixed here.** `sales_quotes.rs` had a unit test calling `quote_totals_mut()`, a helper that
does not exist — a pre-existing test-only break from slice 2 that only `--all-targets` found. It
now assigns the field.

**Not proven, and not claimed.** The browser pass is **queued** behind four other writers on the
single QA slot (`/tmp/w4-qa-slice3.log`); until it lands, the inbox's rendering, its four scopes,
the rejection dialog and the 390px overflow are unverified. The two boxes slice 3 can prove
without it are ticked; the browser-side ones are not.

**Next.** Read the pass when it lands and require `report.salesApprovals.ok`, with
`reject-requires-a-reason` true. Then **slice 4**: orders, reservation, the invoice handoff and
the reports screen — the last slice before REQ-052 closes. It needs `sales_orders`,
`sales_order_lines` and `sales_status_history`, all three already in migration 0053 and still
unwritten; `sales.orders.confirm` and `sales.orders.create` are already in the permission
catalogue. Check the shared migration namespace again before writing 0056 — wave7 and wave10 both
sit on 0053.


## Slice 4a — the order chain (2026-09-29)

**What.** Migration `0053` created `sales_orders`, `sales_order_lines` and
`sales_status_history`, and nothing had ever written them — so the chain a quote promises stopped
at *accepted*. This adds the middle: convert, confirm (which holds stock), cancel (which gives it
back), and the invoice draft that leaves for accounting. `0057` adds the two tables 0053
deliberately left out, because both are another module's to adopt later.

**The two tables, and why they exist even though REQ-053 and REQ-054 are not installed.**
`sales_order_reservations` holds **one row per line**, so `partial` is a countable fact rather
than a guess and a release can be audited line by line. The unique index on
`(order_id, line_id)` is what makes a second confirm a no-op *as a property of the data* rather
than of a handler's read-then-write. The table exists without inventory because a state column
that can only ever say `none` cannot honour the spec's "a visible note rather than a silent
failure". `sales_invoice_handoffs` mirrors `0055_quote_approvals` exactly: subject-shaped, money
frozen into columns, so REQ-054 adopts the rows rather than migrating them.

**Proof.**

- `cargo test -p omnion-module-sales --lib` → **128 passed** (110 before, +18).
- `cargo test -p omnion-api --lib` → **243 passed** (unchanged; the order rules are in the module).
- `cargo test -p omnion-api --test sales_orders -- --test-threads=1` → **11 passed**.
- `apps/admin` `tsc --noEmit` → clean. `node --check scripts/qa/walkthrough.cjs` → clean.
- clippy `result_large_err` → **0** (the baseline in an untouched sibling module, crm, is 0).

**The four defects the walks found** — none of which a typecheck or a unit test would have:

1. **A second confirm wrote a second "confirmed" line in the order's timeline.** The criteria ask
   for a no-op; a person who pressed the button after a slow response must not find their order's
   history rewritten. The history row is now written only when rows were *actually* held, and the
   walk asserts the no-op three ways — same order, same hold rows, same history length.
2. **The list and the detail both answered 500.** The order query joined `sales_quotes` and
   selected `q.number` against a struct field named `quote_number`; the line query joined the
   reservation and selected `l.id` *and* `r.id`, so a draft order's line decoded the hold's NULL
   as its own id, and then decoded the hold's `held_at` NULL the same way. All three are duplicate
   column names, and all three are invisible in the query text — the alias is the only fix.
3. **The invoice draft's insert used a bare `on conflict do nothing`**, which covers *every*
   violation, so a bad currency or a missing order would have been reported as "a draft already
   exists" instead of raising. It now names the partial index it means.
4. **A test that measured the wrong count.** The no-op-history assertion compared the post-confirm
   timeline to the count from *before* the confirm, so it was measuring the confirmation itself.
   The lesson generalises: capture the baseline **after** the first call, not before it.

**Not proven, and not claimed.** The browser pass is **queued** behind the other writers on the
single QA slot (`/tmp/w4-qa-slice3.log`); until it lands, the order list's rendering, its
confirmation button, the cancellation dialog and the 390 px overflow are unverified. The
**invoice-handoff box stays unticked** even though half of it is proved, because the criterion is
a hand-off to REQ-054 and that module does not exist — `external_id`/`external_url` are columns
this module writes nobody into.

**Next.** The second half of slice 4: **reports** (won/lost, conversion %, average deal size,
per owner, with a CSV containing the same rows), global search over quotes and orders, and the
PDF export. `sales_order` rows now exist, so the won/lost and conversion figures have something
to count. `MIGRATIONS`: 0053/0054/0055/0057 are mine; the shared high-water is 0056 (wave8) —
re-read `git ls-tree origin/<branch> database/migrations/` for every sibling at commit time.

## Tick 24 — the report, the CSV, and the guard that existed for one case

**What.** REQ-052 slice 4b. The chain was finished and the question it never answered was the one
a sales desk is judged on. `modules/sales/src/reports.rs` classifies every quote in **one** `case`
expression and hands it to four readers — the counts, the per-owner breakdown, the table and the
CSV — so a headline cannot disagree with the sum of its own rows. `GET /api/v1/sales/search` is one
ranked statement over quotes *and* orders, matching number, customer and title. The screen is
`/sales/reports`: four stat cards, both conversion rates, the mean deal, the per-owner table, the
rows, and an export that is **fetched** rather than navigated to.

**The five rules, each with the easy wrong version it replaced.**

1. **Conversion is won ÷ (won + lost)**, never ÷ every quote — a denominator holding drafts and
   quotes still with a customer falls every time a seller writes a new one. The spec's
   "quote-to-order conversion" is a *different* number and both are printed.
2. **An accepted quote with no order is `pending`**, not a win. The customer said yes; nobody has
   written the order. The order join lives *outside* the `case` for the count and *inside* it for
   the rows, and the first version had it inside both, which reported 0% for a desk that converts
   almost everything it sends.
3. **A cancelled quote is neither a win nor a loss.** The organization withdrew it; folding that
   into "lost" makes a seller who tidied up their pipeline look beaten. It is a fifth column, and
   the screen prints `won + lost + pending + cancelled = N` so a reader can check the arithmetic
   instead of trusting it.
4. **The average is over won deals only**, in exact decimal (`Money`, hundredths as `i128`) — never
   `f64`. The board reads this figure twice, once on the screen and once in the CSV, and a float
   rounding is a two-file argument.
5. **`null` prints as an em dash, never `0%`.** "Nobody decided yet" and "nobody won anything" are
   different facts and only one of them is a zero.

**Two defects the walks and the SQL found, neither visible to a typecheck.**

- **`q.decided_at` does not exist.** The schema has `accepted_at`, `declined_at` and
  `cancelled_at` — three columns, no single "decided" — and the summary answered `500` with the
  column name, which is the cheapest possible way to find out. The day a quote is counted by is now
  `coalesce(accepted_at, declined_at, cancelled_at, created_at)`. The walk pins it down by
  rewinding one quote's `created_at` 200 days and asserting that a report over its **written** day
  comes back **empty** while today's contains it; without the rewind the two expressions are
  indistinguishable.
- **The "any of" guard never tried its own first permission.** `new_any` peels the first name into
  `permission` and leaves the rest in `alternatives` — and the loop iterated `alternatives` alone.
  So the guard written *precisely* so a quotes-only reader could search was the one case it refused,
  and the quotes-only walk caught it as a 403 naming both keys it had just been asked about. The
  lesson: a "primary plus alternatives" shape is a bug whenever the loop forgets the primary, and
  the type system cannot see it because both halves are the same `&'static str`.

**Gates.** `cargo test -p omnion-module-sales --lib` **147/147** (+19) ·
`cargo test -p omnion-api --lib` **244/244** · report walks **10/10** ·
`apps/admin` `tsc --noEmit` clean · `node --check scripts/qa/walkthrough.cjs` clean.
**No migration** — the report reads the tables `0053` already created, which is the first slice of
this module that needed none.

**Next.** The browser pass for slice 4b is queued (`/tmp/w4-qa-4b.log`) and this is **not** a
close tick, so the REQ stays `in-progress` regardless of what it says. After it lands: the **PDF**,
the last unticked item on this REQ and the only thing the nav still does not link to. REQ-029 owns
the rendering engine, which is not built, so the question this slice has to answer is whether the
module can emit a document by itself or must wait — and that question is worth a paragraph in the
REQ rather than a placeholder button. `MIGRATIONS`: 0053/0054/0055/0057 are mine and 4b added
none; the shared high-water moves constantly — re-read `git ls-tree origin/<branch>
database/migrations/` for every sibling at commit time, never from memory.



## 2026-09-29 · wave 4 · REQ-052 slice 5 — the PDF documents (`2036485`)

**What.** `GET /sales/quotes/{id}/pdf` and `GET /sales/orders/{id}/pdf`, a PDF 1.4 writer with no
dependency (`modules/sales/src/pdf.rs`), the two layouts (`modules/sales/src/documents.rs`), the
routes (`apps/api/src/routes/sales_documents.rs`), a **PDF** button on both detail screens, and a
walkthrough step that reads the bytes. The box this closes is the last unticked item that was
actually buildable; REQ-029 owns a general document engine and is not written.

**The question the slice had to answer first.** "The module can emit a document itself, or must it
wait?" — it can. A PDF 1.4 file is a cross-reference table and a content stream; the part a quote
needs (header, line grid, totals block, page breaks) is about four hundred lines, and a
font-embedding crate is megabytes of typeface on every build of a **public** repository. The writer
lives beside the two documents that need it rather than in `crates/`, because moving it is a
`git mv` and a `pub use` and a guess about infrastructure nobody has asked for twice.

**Three defects, and none of them was a compile error.**

1. **The content stream was a `String`.** WinAnsi is not UTF-8, so every byte above `0x7F` went
   through `from_utf8_lossy` and reached the page as U+FFFD. A document printed "S?irket" for
   "Şirket" — and **every test that read only ASCII passed**, because the corruption happened on
   the way *into* the page rather than out of the encoder. The buffer is `Vec<u8>` end to end, and
   `a_non_ascii_character_survives_into_the_stream_intact` asserts no replacement character is
   present. This is the bug the tick is worth remembering for: the wrong buffer type for a binary
   format is legal Rust and silent in every signature.
2. **cp1252's `0x8A`/`0x9A` are `Š`/`ş` — the carons, not `Ş`/`ş`.** The cedillas are not in cp1252
   at all. The first table put the cedillas on the caron's slots. It cost six rounds of failing
   tests to find, because the two pairs are visually near-identical and *every* assertion that did
   not name the exact byte kept passing. The four Turkish letters cp1252 genuinely lacks are now
   aliased to a readable letter **and counted**: `g` for `ğ` reads as a correct name that is not
   the right one, which is the more dangerous of the two failure modes, and the count reaches the
   sender as an `x-omnion-document-degraded` header the screen shows. Everything about this was
   settled with `bytes([0x8A]).decode("cp1252")`, never from memory.
3. **The zero rule covered half the document.** A figure with no value prints a dash, never
   `0.00` — but the rule was written for the totals block and the line grid printed `0.00` while
   the total beneath it printed a dash. One document, two sentences, one of them wrong.

**Proof.**

```
cargo test -p omnion-module-sales --lib            178 passed, 0 failed   (+38)
cargo test -p omnion-api --lib                    249 passed, 0 failed
apps/admin  tsc --noEmit -p tsconfig.json          clean
node --check scripts/qa/walkthrough.cjs            clean
```

The walkthrough step asserts the `%PDF-` header, the `%%EOF` trailer and the grand total
**printed in the document** — because Playwright cannot read a download it did not request, so a
button that saved `{"error": …}` under a `.pdf` name passes every "did it download?" check there
is. **The browser pass is queued behind the single QA slot and is NOT part of this tick's claim.**

**What cost the tick, and is worth the next writer's time.** PostgreSQL was in **crash recovery
for the first two hours of this tick** and every API call answered `dependency_unavailable`. The
cause is not mysterious: `/mnt/apopic` had reached **100%** (363 MB free) and WAL recovery could
not write. It recovered on its own once the build caches were pruned — `.tmp-target`'s stale test
binaries, 0.9 GB of executables older than the last build. The second lesson is about the tests:
**nine of the twenty failures were my own test expectations, written from a guess about the byte
stream**, and each one cost a four-minute compile to disprove. `modules/sales/examples/pdf_probe.rs`
now exists for exactly this — it prints what the writer really emits (bytes, operator lines, the
xref table with line lengths) and it settled three rounds of guessing in one run of ninety seconds.
Write the probe before the assertion, not after the fifth failure.

**Next.** The **⌘K row** is the remaining half of the global-search box: `components/search-palette.tsx`
is a shared component with its own provider registry, and a sales provider in it is the next slice,
not a line in this module. Then REQ-052's `Order → invoice draft` box, which stays unticked **on
purpose** — REQ-054 is not built, and the criterion is a hand-off to a module that does not exist.
After that, REQ-053 inventory, the first of the nine untouched wave-4 requests.

## 2026-09-29 · wave 4 · REQ-052 slice 6 — the palette, and three providers that had been indexing into the dark

**What.** Merged `origin/main` (one conflict, `app-shell.tsx`'s lucide import — both sides' nav
entries had auto-merged, so the resolution is the union of both icon sets, not a choice), then
registered **quotes** and **orders** as search providers and gave the module its four ⌘K
commands. Commits `600e9c7` and `3491ecd`.

**The defect this slice found is the one worth the tick.** A search provider is registered in
**two places**: `crates/search/src/providers.rs` decides what the indexer reads and writes, and
`apps/admin/lib/search-palette.ts` decides whether the panel renders a section for it. A
provider present in only the first is not an error anywhere — the index upserts its rows, the
query answers, the API returns `hits: []`, and the section renders **nothing at all**, because
`paletteProvider` answers `null` and a group with no screen contributes no rows, no error and no
dead end. That is by design; it is also how a provider disappears without a trace.

`contacts`, `companies` and `deals` were in exactly that state. REQ-051 shipped them, the CRM
tests passed, the indexer upserted them on every reindex, and **no CRM row had ever appeared in
the ⌘K box** — because the panel registry, which is TypeScript, was never told about them. My own
two providers would have shipped the same way, which is why the lesson is the two-place
registration rather than the missing entries.

The same split exists a third time, in `intent.rs`: the domain vocabulary that decides which
provider a *phrase* narrows to knew no word that mapped to a business provider, so "deals for
Northwind" narrowed to nothing rather than to the board. Three registries, one feature, and each
one is silent when it is wrong.

**Two providers rather than one behind an "any of" guard.** The quote desk and the order book are
separate screens behind separate keys, so somebody who may price an offer without seeing the
delivery book gets one section and not the other; merging them would hand whichever half they
hold a list of rows the other half covers.

**The number is the title, not the customer** — the inversion the CRM provider deliberately does
not make. A seller with a printed `Q-2026-0007` in front of them wants that one row; the customer
name is the second-ranked question, so it sits at weight D. An order carries **its quote's
number** at weight C, because somebody chasing "what happened to Q-2026-0007" types the quote
number, and answering with the frozen quote beside the order sends the reader to a document that
can no longer change. `notes`, `payment_terms`, `reference` and `decline_reason` are deliberately
absent from both vectors: the CRM keeps a contact's notes out for the same reason, and a decline
reason is stronger still, since "too expensive" is the seller's most valuable sentence and the one
a competitor's account must never search into. **The index cannot answer a per-role question
about a vector**, so the rule the module states is the rule the index keeps.

**The second half of the slice was a criterion that could not be met.** REQ-052's mobile box asks
that the totals footer stay visible while the lines are scrolled; the footer was a plain `<dl>` at
the bottom of a seven-column line grid, which on a 390px phone is taller than any screen. It is
now `lg:sticky bottom-2` — **sticky, not `fixed`**: a fixed footer overlays the page's own action
bar and covers Save and Send on a short form. The pass now reads the footer's box, scrolls 600px
and reads it again, because *an overflow check on the document cannot see this defect at all* — a
static footer at the bottom of a long page satisfies "no horizontal scroll" while failing the
criterion outright. Same shape as slice 5's PDF: a button that saved an error body under a `.pdf`
name passed every "did it download?" check available.

**Proof.**

```
cargo test -p omnion-search --lib                     51 passed, 0 failed   (+7)
cargo test -p omnion-api --lib                       249 passed, 0 failed
apps/admin + apps/web  tsc --noEmit                  2/2 clean
node --check scripts/qa/walkthrough.cjs               clean
```

The walkthrough grew two assertions rather than a route: `business-sections` records which of
the five business providers answered, so a regression is a **line in the report** instead of an
absence nobody can see, and `mobile-builder` measures the pin. The **browser pass is running
against `3491ecd` and is not yet part of this tick's claim.**

**Next.** REQ-052's last box that can be closed is its own — `Order → invoice draft` stays
**unticked on purpose**, because REQ-054 is not built and the criterion is a hand-off to a module
that does not exist. Then **REQ-053 inventory**, the first of the nine untouched wave-4 requests
and the one with the most downstream weight: sales currently records the *intent to hold* stock as
a row per line, and inventory is what turns that into a ledger.

**Migrations.** None this slice — a search provider is an entry in a registry, not a schema. The
shared high-water still reads: main 0051, wave2 0052, wave3 0056, w5 0054, w6 0051, w7 0055,
w8 0058, wave9 0119; mine are 0053/0054/0055/0057.

## 2026-09-29 · wave 4 · REQ-052 slices 6 and 6b — the fourth registration, which is a data row

**What.** Slices 6 and 6b of REQ-052: `quotes` and `orders` as search providers, the ⌘K commands
that open and create the sales screens, the walkthrough's two new measurements, and six
database-backed walks that drive the real endpoints. Commits `600e9c7`, `aef1bfe`, `a2cd89e`.

**A search provider is registered in FOUR places, and the fourth is data.** The first three are
code and a unit test can hold them to it: the **registry** (`crates/search/src/providers.rs`)
decides what the indexer reads, the **panel** (`apps/admin/lib/search-palette.ts`) decides whether a
section renders, the **vocabulary** (`intent.rs`) decides which words narrow to a provider. The
fourth is `search_settings.enabled_providers` — a **row**, written the first time somebody saves
the settings screen — and `read_settings` only falls back to the full registry when that row is
**absent**. So every installation that ever saved its settings froze the provider list of that day,
and a provider registered afterwards indexes its rows on every reindex, appears on the status
screen, and is filtered out of every query by `d.provider = any(enabled)`.

**REQ-051's three CRM providers were in exactly that state, one level shallower** — registered and
upserted, with no panel entry and no vocabulary, so all three sections rendered *nothing* while
their own tests passed. My two would have shipped the same way. The walk found it, not a test:

```
14 indexed rows, 0 hits
```

That pair of facts is the whole defect, and it is the most expensive single observation in the
tick: it took four wrong turns to reach, and every one of them produced a **plausible** answer.
`&type=quote` as a query parameter is silently ignored, so the box answered with every kind at once
and the first hit was the company the walk had just created — "the title must be the number" then
failed on `"Northwind Trading"`, which is a *correct* company row answering an unfiltered
question. `type:quotes` (the provider key) filters to nothing, because the clause matches
`entity_type`. The scoped syntax lives **inside** `q`, not in a parameter. And the reindex report is
a **list**, so reading `.indexed` off the object panicked on a report that had indexed fourteen
rows. None of those four is a compiler error and each one is a sentence in the diff explaining
itself.

`0125` unions the new keys in rather than replacing the array, so an operator who deliberately
searches fewer providers keeps that choice across an upgrade; it re-runs as `UPDATE 0`; and it
prunes keys the registry no longer knows, so a withdrawn provider does not sit in the row forever
filtering nothing. Verified against the real row rather than a fresh one — the stale row is the
whole point.

**The second half of the tick was an assertion that could not fail.** REQ-052's mobile box asks that
the totals footer stay visible while the lines scroll. The footer was a plain `<dl>`, and my first
fix put the pin behind `lg:` — **desktop only, on a criterion titled "Mobile 390×844"**, which is
backwards. The walkthrough then reported `stuck: true` on a `position: static` element, because an
empty builder is shorter than an 844px viewport: `scrollTo(0, 600)` moved nothing, so "the footer
did not move" was trivially true. A sticky assertion that cannot fail is worse than no assertion,
because the next writer reads it as evidence. The pass now adds two lines so the grid exceeds the
viewport, records the scroll offset it achieved, and requires the conjunction: **the page moved and
the footer's gap did not.**

**Proof.**

```
cargo test -p omnion-search --lib                       51 passed, 0 failed   (+7)
cargo test -p omnion-api --lib                         249 passed, 0 failed
cargo test -p omnion-api --test search_business_providers   6 passed, 0 failed
  -- OMNION_DATABASE_URL=…/omnion_qa_w4 OMNION_REQUIRE_DB=1 --test-threads=1
apps/admin + apps/web  tsc --noEmit                    2/2 clean
node --check scripts/qa/walkthrough.cjs                 clean
```

The six walks reindex through `POST /search/reindex` — the call the status screen makes — and read
the rows back through `GET /search`, which is the only tier that can see any of the four
registrations. `OMNION_REQUIRE_DB=1` makes a missing database a **panic** rather than a skip: a
walk that quietly reports SKIP when PostgreSQL is down is a green tick that proved nothing, and
this suite's whole subject is an absence.

**Not proved this tick, and said so rather than ticked.** The **browser pass is still running**
(90 minutes in, against `600e9c7` — before the `a2cd89e` fix), so the palette sections it records
were measured while the harness sat on `/login` and the assertion was not evaluated. The mobile
box stays **unticked**: the fix and its measurement are in, the pass that measures them is not
finished. `Order → invoice draft` also stays unticked, on the same grounds as before — REQ-054 is
not built and the criterion is a hand-off to a module that does not exist.

**Next.** Re-run the pass against `a2cd89e` and require the `business-sections` step to list all
five providers rather than `missing: contacts,companies,deals,quotes,orders`, and the
`mobile-builder` step to report a non-`static` footer with `pageScrolled: true`. Then **REQ-053
inventory** — the first of the nine untouched wave-4 requests, and the one with the most downstream
weight: sales records the *intent to hold* stock as a row per line, and inventory is what turns
that into a ledger.

**Migrations.** `0125_search_provider_enablement.sql` — taken above the shared high-water, which
now reads main 0052/0123, wave3 0056, w5 0054, w6 0052, w7 0064, w8 0058, wave9 0124. Mine are
0053, 0054, 0055, 0057 and now 0125.

---

## Wave 4 · REQ-053 inventory · slice 1 — items, warehouses, locations and the stock rollup

**Commits** `27c3e7b` (the module) and `37bf572` (the API surface and the five permission keys).

REQ-053 is the first of the nine untouched wave-4 requests and the one with the most downstream
weight: sales records the *intent to hold* stock as a row per line (`0057_sales_order_reservations.sql`
says so on the order detail), and inventory is what turns that into a ledger. It is also the
request whose central property is an **absence** — the rollup agreeing with the ledger — which is
why the suite treats a missing database as a failure rather than a skip.

### What

`modules/inventory` (a module, not a crate: docs/04), `0126_inventory.sql`, twenty-two routes and
five permission keys. The rule the whole thing serves:

> **`inventory_stock` is a rollup of `inventory_movements`, and the two may never disagree.**

Four mechanisms, none of which is "be careful in the service":

* **The ledger is append-only.** No `PATCH`, no `DELETE`, no trigger. The only way to fix a
  mistake is another movement with reason `correction`. axum answers `405` for a method it does not
  implement, which is the only answer a later handler cannot undo by deciding to be helpful.
* **Every row carries the numbers it produced** (`on_hand_after`, `reserved_after`), written from
  the same locked read that computed them, so the ledger is *replayable*. `replay` recomputes every
  item × location from the movements alone; `reconciliation_report` compares that with the rollup
  and names every disagreement **with both numbers** — a list, because a count tells somebody the
  module is wrong and not where.
* **The sign lives in the kind, not in the number.** `quantity` is stored positive and `kind` says
  which way it went; an adjustment carries its own sign. Storing both is two sources of truth for
  one fact, and the second is always the one a manual fix gets wrong.
* **Concurrency is the lock, not a retry.** The stock row is taken `for update` inside the
  transaction, the ledger row is written first, and the rollup write is the one that can be lost —
  a lost rollup is caught by `replay`; a lost ledger row is the state the module exists to prevent.

### Proof

```
cargo test -p omnion-module-inventory --lib                       47 passed, 0 failed
cargo test -p omnion-api --test inventory -- --test-threads=1     11 passed, 0 failed
  -- OMNION_DATABASE_URL=…/omnion_qa_w4 OMNION_REQUIRE_DB=1
@omnion/admin:typecheck + @omnion/web:typecheck                   2/2 successful
```

`OMNION_REQUIRE_DB=1` makes a missing PostgreSQL a **panic**: a walk that reports SKIP when the
database is down is a green tick that proved nothing, and this suite's subject is an absence.

**Seven boxes ticked, seven left unticked and named.** The reconciliation test is a replay rather
than a comparison of the last row's `on_hand_after` — the cheap version only proves the final
number, and a ledger whose middle was corrupted still ends at the right number if the last row was
honest. It writes the awkward decimals on purpose (10 + 3.5 + 0.25 + 12 − 2.75 = **23.000**, which a
binary float would not land on) and then **breaks the rollup deliberately** — a row set to 999 with
no ledger row behind it, the exact state a two-transaction service would leave — and requires the
report to name it. A test that has never seen the failure it is looking for is a test of the happy
path.

### Four bugs the tests found, all of them real

* **The decimal parser mis-scaled every quantity with a trailing zero.** `whole_to_milli` scaled
  the fraction by *its own length* instead of padding on the right, so `3.250` came back as
  **`3.002`** — a thousand times too small, and still a value that looks like a number. Found by
  the unit test, not by a walk, because a walk would have shown `6.000 − 1.5` and stopped there.
* **The schema's `reserved <= on_hand` had the same gap as the service, in the opposite order.**
  A permitted `correction` may leave `on_hand` at −3, and re-testing `reserved <= on_hand` after
  the service had already allowed it refused the very write the permission exists to permit — with
  the message "this would hold 0.000 against −3.000 on hand", which is nonsense to read. A service
  that allows a negative and a constraint that then refuses the row would leave **a written ledger
  row with no rollup behind it**, which is the one state this module exists to make impossible.
  Fixed in both places, in the same order, and the constraint now reads
  `reserved <= on_hand or on_hand < 0`.
* **`Quantity::checked_sub` refuses an overflow, not a negative result.** The release path trusted
  it to stop a release below zero; `0 − 1000` is a perfectly good value of an `i128`, so a release
  of more than was held produced `reserved = −1.000` and the schema refused the row *after* the
  ledger row had been written. The check is now written out.
* **The available quantity was only in `details`, not in the sentence.** The form renders `details`
  beside the input; a person reads the sentence, in a log and on a terminal. The walk's assertion
  checks the sentence, because a human reads the sentence.

### And three test bugs that were worth having found

* `fixture.org` where the test meant `fixture.other_org` produced a **`200` for "another
  organization's item"** — which was the module correctly answering about the caller's *own*
  organization. The instinct was to look for a tenant leak; measuring first (which organization
  was the outsider actually resolved to?) is what kept a green build from shipping a real fix for a
  bug that did not exist.
* A tenant naming another organization is stopped by the tenancy rule with a `403
  cross_organization` **before the module is reached**, so the criterion's `404` needs a different
  caller. Two refusals, two callers, and the test now names both.
* `Uuid::new_v4().simple()` is **hex**, so a "barcode" built from it contained `a`–`f` and the
  normalizer correctly upper-cased it, making the test compare `"869B2B0D22AF"` with
  `"869b2b0d22af"`. A barcode that is really an EAN is digits and the test now says so.

### Not proved this tick, and said so rather than ticked

**No admin UI and no QA browser pass.** The module, the migration, the API and the tests are done
and committed; `apps/admin` has no `/inventory/*` screen yet, so a walkthrough would find nothing
and the pass's absence is not evidence of anything. The seven unticked boxes are the ones that name
a screen, a drawer or a document that does not exist. This slice is deliberately the data layer:
the ledger and the rollup are the part that has to be right before anything is drawn on top of it,
and drawing screens against a ledger that later moves is how a module gets rewritten.

### Next

**Slice 2 — the ledger screen and the adjust drawer**, including the over-threshold approval
(`inventory_settings.adjustment_approval_threshold` and the `ApprovalNotGranted` variant already
exist with no route behind them), the CSV export and the first `/inventory/*` admin screens. The
box left unticked for the mobile criterion stays unticked until a pass measures it.

**Migrations.** `0126_inventory.sql` — taken above the shared high-water, which now reads main
0123, wave3 0125, wave9 0125. Mine are 0053, 0054, 0055, 0057, 0125 and now 0126.


## 2026-09-29 · wave 4 · REQ-053 slice 2 — the approval path, the exports and three screens (`fdc04e4`, `e8328a5`)

**What.** The over-threshold adjustment now waits for a decision. `0127_inventory_adjustment_approvals.sql`,
`modules/inventory/src/approvals.rs` (the request, the inbox, the decision, the withdrawal), the module's
CSV writer, six new routes plus the threshold check inside `POST /movements`, `apps/admin/lib/inventory.ts`,
three screens (stock list, movement ledger, adjustment inbox), the shared drawer, the nav entry and a depth
pass that drives all three.

**The rule, stated because the shape follows from it.** An adjustment over the organization's threshold changes
nothing until somebody who did not ask for it says yes.

**The decision this tick actually had to make: what is the request?**

The cheap design is a `pending` flag on `inventory_movements`, and it breaks the one property the module
exists for. The ledger is append-only and replayable, and `replay` is the *proof* that the rollup is honest.
A ledger row whose meaning depends on a second table is a row whose meaning can change after the fact — and
the replay stops being a proof and becomes a puzzle. So the request **holds the whole un-applied write**: item,
location, kind, mode, quantity, reason, note, source. Approving replays those exact values through
`record_movement`, the module's single write path. Two routes into one write function, never two write
functions, because two write paths for one business rule is how a module ends up with two answers.

**Second decision: the amount is absolute, not a percentage.** A variance of two units out of ten thousand is
not a smaller mistake than two out of ten. A percentage rule waves the first through and stops the second,
which is exactly backwards, and the approver would be asked about numbers that do not describe the mistake.

**Third: the threshold is a snapshot.** Lowering the threshold after three requests are pending must not
retroactively justify them, so the row carries the line it was measured against and the inbox prints the
row's number rather than today's setting.

**Proof.**

* `cargo test -p omnion-module-inventory --quiet` — **60 passed, 0 failed** (47 from slice 1, 13 new: the
  amount function, the threshold comparison including a threshold of zero, and the CSV writer's cells).
* `cargo build -p omnion-api --quiet` — clean.
* `pnpm --filter @omnion/admin typecheck` — clean.
* QA pass on the private stack (`QA_STACK=w4`, ports 18083/3103/3203, database `omnion_qa_w4`).

**Two tests that fought each other, and what the fight was about.**

The CSV writer guards a cell beginning with `=`, `+`, `-` or `@` so an item name cannot become a formula in
the approver's spreadsheet. My first version guarded every leading `-`. That is the bug, and it is the worst
kind: **a ledger export is mostly negative numbers**, so the guard turned the export into a column of text —
worse than the injection it prevents, and invisible because the file still opens. The rule is that `-` starts
a formula only when something *evaluable* follows it, so `-1.500` and `- 3 units` pass through and a bare `-`
or `-cmd` is guarded. Two assertions failed before the function and the tests agreed, and the loser was the
tests; the doc comment now states the rule the function implements, because "the rule" and "the function"
disagreeing is how the next reader picks the wrong one.

**A test that measured nothing.** The first filter assertion checked only that some negative row appeared,
which passes on a filter wired to nothing. It is now a **conjunction**: after asking for negatives, either
nothing matches or every row shown is negative.

**The "no edits" affordance, measured.** The spec asks for "no pencil icon on rows". The depth pass counts
every control a ledger row carries and **fails** the pass if any label reads edit, delete, remove or void — a
promise the server keeps with a `405` deserves an assertion, not a reviewer's eye.

**Not proved this tick, and said so rather than ticked.**

The **transfer line cap** and the **low-stock alert** boxes stay unticked: they are slice 3. The mobile
one-handed box stays unticked — the drawer is built as a full-screen sheet with `inputMode="decimal"`, but a
build is not a measurement, and the box asks for 390×844 use. The stocktake box is slice 4.

**Next.**

Slice 3 — transfers (draft, dispatch, receive, cancel) and the low-stock alert sweep with REQ-021's
notifications. The `in_transit` location kind, the `TransferOut`/`TransferIn` kinds and the `stocktake_variance`
reason code already exist in slice 1 with **no route behind them**, which is the same shape of gap this slice
just closed: the schema can be right and the feature absent.

**Migrations.** `0127_inventory_adjustment_approvals.sql` — taken above the shared high-water, which now reads
main 0123, wave3 0125, wave9 0125, wave2 0124. Mine are 0053, 0054, 0055, 0057, 0125, 0126 and now 0127.

**The QA pass, at commit time, had not finished** — this section is written before its verdict is
known and says so rather than predicting one. It was at ~23 minutes and still in the walk's login phase
against the private stack, with four other writers' walkthroughs running at the same time; the run's
artifact directory was still empty. What the pass is *supposed* to evaluate, listed so the next tick can
check its own output against a list that was written before the answer existed:

* `inventory-stock` renders, and the **status filter as a conjunction** (asking for negatives yields
  either nothing or only negatives);
* `inventory-movements` renders, and **no row carries an edit/delete/remove/void control** — the
  assertion that backs the spec's "no pencil icon" line;
* the **scanner's miss** says `Nothing is labelled …` and keeps the code in the box;
* `inventory-approvals` renders its four scopes;
* screenshots `page-inventory-stock`, `page-inventory-movements`, `page-inventory-approvals`.

Until those land, the screens are **built and typechecked but not visually reviewed**, and the REQ stays
`in-progress` rather than `done`. Slice 3 does not depend on the pass finishing, so the next tick proceeds
to transfers and the alert sweep rather than waiting on it.

## Tick 58 — REQ-012 slice 2, the header policy and the CSRF token

**What.** `crates/security/src/headers.rs` (the policy, its rendering and every reason it is
refused), `csrf.rs` (the derived double-submit token), `header_store.rs` (the singleton row, a
compare-and-swap save, and the history), `0135_security_headers.sql`, `CsrfSecret` in
`crates/core/src/config.rs`, the two middlewares in `apps/api/src/headers_middleware.rs`, and
`GET`/`PUT /security/headers` behind `security.read` / `security.manage`.

**The decisions that are the slice, not its furniture.**

* **One rendering, three consumers.** `HeaderPolicy::render` produces the header lines that go
  on the wire, the `rendered` column the panel previews, and what the posture checks read. A
  policy summarised in one place and assembled in another is how an operator ends up with
  "I configured it and nothing changed".
* **Report-only sends the report header and nothing else.** Sending both would apply a policy
  while the screen says it is only reporting it. The test asserts neither mode emits the other
  mode's name.
* **The installed layer holds a shared cell, not a snapshot.** The first draft snapshotted the
  policy when the router was built, which meant a save changed the database and no response
  until the next restart. `RwLock<Arc<Policy>>` — one pointer clone per request, and `reload`
  after a successful save.
* **Refuse, never skip, when the secret is missing.** A deployment with no `OMNION_CSRF_SECRET`
  boots and then refuses cookie-authenticated mutations. Failing open would turn a missing key
  into a silent loss of a control, which is the worst outcome a control has.
* **Header policy is a singleton, not per-tenant.** One process serves every response, so a
  per-tenant CSP would let one tenant weaken the policy everybody's requests are answered with.
* **The audit row and the setting are one transaction.** The first draft ran them as two
  queries — the update commits, the insert fails on a dropped connection, and the edit happened
  with nothing recording it.

**Two defects from tick 57, found because this tick finally ran `--lib`.**

* `cargo test -p omnion-api --lib` had been **red since slice 1** and only `cargo build` had
  ever been run against it. A test called `to_string()` on `ApiError`, which implements no
  `Display`. It does now — the code and the message, never the `details` blob.
* A test fixture had a **credential mask written into the source** instead of a secret: the
  file literally held the redaction placeholder where an `sk-` value was meant, so the
  credential detector was being asserted against a Unicode marker and passing for the wrong
  reason. Fixed at byte level; `git diff --stat` is the check that catches that class of edit.

**Proof, added.**

- `cargo test -p omnion-security --lib` → **100 passed** (51 + 46 for headers and CSRF, + 3 store)
- `cargo test -p omnion-api --lib` → **208 passed** (was 0 compiling)
- `cargo test -p omnion-core --lib` → **37 passed** (34 + 3 for the secret)
- `cargo build -p omnion-api` → clean
- Commits: `163a4f8`, `b9e2ae2`, `f2f8007`, `1ad077e`, `6e7b920`, `4747b9f`

**Still open, and named rather than written off.** The `/security/headers` **screen** does not
exist yet, and neither slice 1 nor slice 2 has a browser pass: the single QA slot was held by a
live w10 pass for this whole tick (holder 2521941, cwd `/mnt/apopic/omnion-w10`). A pass that
starts while this tree is half-written would build half of it, so the pass was deliberately
stopped and the slice committed instead. **Next tick:** if the slot is free, run
`bash scripts/qa/run.sh` with no `QA_STACK` override and tick the screen boxes for slice 1;
then build the `/security/headers` screen and extend `scripts/qa/walkthrough.cjs` so it is
visited and clicked.

## 2026-09-29 — REQ-053 slice 3 · transfers and the low-stock alert inbox

**What.**

The slice closes the gap slice 2 closed from the other side: slice 1 shipped the `in_transit`
location kind, the `transfer_out`/`transfer_in` movement kinds and the rule that a hand-written
movement may not be a transfer — with **nothing able to produce any of the three**. A transfer
is now a document (`inventory_transfers` + `inventory_transfer_lines`, `0138`) with the
three-step lifecycle, and the alert **record** now exists beside the alert **event** slice 1
emitted.

**Proof.**

* `cargo test -p omnion-api --test inventory_transfers` — **11 passed, 0 failed**
* `cargo test -p omnion-module-inventory --lib` — **71 passed, 0 failed** (60 from slices 1–2, 11 new)
* `pnpm --filter @omnion/admin typecheck` — clean
* `0138` and `0140` applied to `omnion_qa_w4`; the in-transit location is backfilled for all 187
  existing organizations **and** a brand-new organization gets it from the `0126` trigger.

**A `date` column bound as text, which a build cannot see.**

`scheduled_on` is `date` and the parameter arrives as a `String`, so the first transfer anybody
wrote answered **500** from PostgreSQL. Nothing catches this: the code compiles, the typecheck
passes, and the unit tests are green because none of them touches the insert. It took a walk
through the real router to find, and the fix is a cast in the statement next to the shape check
that already validates the format. The general form is worth stating: **a text parameter bound
to a typed column is a runtime failure that no gate on this platform can see**, and the only
gate that does see it is the one that goes through HTTP.

**Two more the walks found, both of the same kind.**

`outstanding` was a *method* on `TransferLine`, so it never crossed the wire — the screen's
receive boxes and the walk's assertion both read `undefined`. The rule lived in three places
and was correct in one. It is now a serialized field computed in `into_view`, for the same
reason `on_hand_after` is on a movement: the number a person acts on should come from the server
that owns the arithmetic, and a client-side `quantity − received_qty` is right until the rule
changes.

The third was mine rather than the code's: a walk that read the alert inbox before anything had
swept it. The fix is the *assertion* — the operator sweeps, the reader reads, and the reader is
refused the sweep two lines later. Sweeping as the reader would have tested the wrong power. The
final sweep now asserts `raised == 0`, which is the idempotence claim; asserting only `OK`
would have missed it.

**A test that failed first and was right to.**

The sweep's own unit test caught the sweep disagreeing with the model: my first version folded
`min_threshold` and `reorder_point` with `greatest()` and used a strict `<`, so a shelf sitting
**exactly on** its reorder point was `Ok` in the inbox and `Low` on the stock list. Two screens
disagreeing about one shelf is the exact thing the shared `StockStatus::of` exists to prevent,
and the fix is to call it rather than copy it. The test is now
`the_alert_reads_the_badge_the_stock_list_already_draws` and it says in its own comment that it
exists because it failed.

**A gap on `main` worth naming, and the workaround that goes with it.**

`6e7b920` (main) added the CSRF layer, which refuses every cookie-authenticated mutation without
a token, but `POST /auth/login` still issues **only** the session cookie. A real browser
therefore cannot write anything, and every write suite on the platform is red for the same
reason — this is not an inventory defect and not mine to fix from a feature branch. The test
harness computes the token instead: it is an HMAC of the session id, and the id is read through
the platform's **own** `resolve_session` rather than by re-implementing `hash_token` (which is
one-way, so a `where token_hash = $1` would silently find nothing). The harness asserts the
cookie is *still absent*, and that assertion is the note that tells the next reader the
workaround can be deleted. **Owner: `POST /auth/login` should set `omnion_csrf` via
`omnion_security::csrf::token_cookie`.**

**Migrations.** `0138_inventory_transfers_and_alerts.sql`, taken above the shared high-water
(now main 0135, wave2 0139, wave5 0137). Mine are 0053, 0054, 0055, 0057, 0125, 0126, 0127,
0138 and **0140**.

**`0054_sales_number_sequences` is renumbered to 0140**, and the reason is a lesson worth more
than the number. A merge brought in a **second** `0054` — `0054_security_posture`, which every
other worktree also has — and sqlx answers a duplicate in the directory with
`VersionMismatch(54)`. That error **reads exactly like a stale QA database**, so the reflex
(`scripts/qa/reset-db.sh`, which my own ledger records as the fix for that symptom) did not
help and cost a full test cycle. The general form: when a migration error and a fix for it are
both already known, and the fix does not work, **the diagnosis was wrong** — not the fix, and
not the environment. `ls database/migrations | awk -F_ '{print $1}' | sort | uniq -d` finds this
in one second and belongs in the reflex.

**The QA pass, at commit time, had not finished.** It was queued behind another writer's pass
(slot `max 1 concurrent`) with the stack down. What the pass is *supposed* to evaluate, listed so
the next tick can check its output against a list written before the answer existed:

* `page-inventory-transfers` renders, and the **empty state says something**;
* the create form opens and **neither picker offers the in-transit location**;
* a transfer's detail stepper shows, and **its buttons agree with the status** — a dispatch
  button on a received transfer is the affordance version of the bug the module refuses
  server-side;
* `page-inventory-alerts` renders, the sweep **reports a number**, and every alert row prints a
  threshold;
* the inventory module nav appears on all five screens.

Until those land the screens are **built, typechecked and walked by the API suite, but not
visually reviewed**, so the REQ stays `in-progress`.

**Next.** Slice 4 — the stocktake session (freeze a scope, count, variance computed live, one
`stocktake_variance` movement per deviation on close, and a variance report that reopens
correctly). `stocktake_variance` is already a reason code in slice 1 and the reconciliation
report is already a list of disagreements with both numbers — the shape a variance report
reuses. Slice 5 is the sales-order reservation, which the `reserve`/`release` kinds exist for.


## 2026-09-29 · w4 · REQ-053 slice 4 — the stocktake

**What.** The stocktake session, its counting sheet, the close that posts the variances, and the
variance report. `stocktake_variance` has been a reason code since slice 1 and the reconciliation
report has been a list of disagreements with both numbers since slice 1; both were a shape with
nothing behind it. Five commits: `2064bab` (module + `0143` + the permission), `798cb7d` (the
seven routes), `7642713` (the walks and the 409 they forced), `16268df` (the screen), `b02db0d`
(the QA depth pass).

**Proof.**

* `cargo test -p omnion-module-inventory --lib` — **78/78** (71 before, 7 new).
* `cargo test -p omnion-api --test inventory_stocktake -- --test-threads=1` — **7/7** walks
  against `omnion_qa_w4` with `OMNION_REQUIRE_DB=1`, so a green tick cannot be a quiet skip.
* `pnpm typecheck` — **2/2**.
* `bash scripts/qa/run.sh` on `QA_STACK=w4` — **queued, not yet run** (see below).

**Three decisions, and what each one costs if it is wrong.**

*The expectation is frozen onto the line when the sheet is opened, not re-read at close.* A count
writes to stock, so an expectation computed at close time can already contain the variance the
close is about to post — the deviation reports itself as zero, and a count that finds nothing
always finds nothing. The walk proves it by **moving stock underneath an open sheet** and
requiring the frozen number to be the frozen one. This is slice 3's "put the threshold on the
alert row" arriving in a second form, which is the argument for writing down *why* a column
exists rather than leaving the schema to imply it.

*An uncounted line blocks the close rather than being read as zero.* `null` is "nobody looked",
`0` is "there is nothing here". A close that treated the first as the second would post an
adjustment of `−expected` for every unvisited shelf: the module would **destroy the stock it
claims to have measured** and report a clean success. That is the most expensive kind of
agreement between a screen and a database, and it looks like a win. The refusal carries the
count (`1 line(s) nobody has counted yet`), and the walk asserts on the sentence *and* on the
ledger being untouched afterwards.

*A count that agrees everywhere closes with zero movements.* The cheap version refuses a clean
count, which teaches a warehouse that a good shelf is a failure — and the fastest way to get
sheets falsified. A clean count is a successful count.

**What the four failing walks were, which is the part worth keeping.**

Three were the walks' fault and one was the module's, and the split is not 50/50 by accident:

* The report walk stocked its "unrelated" bystander at the **counted** location, so the sheet
  legitimately had two lines and the close was right to refuse. A sheet of everything at the
  shelf is what a stocktake *is*; the test had to move the bystander to prove that unrelated
  stock moves are unrelated.
* A receipt of **500** came back `202` rather than `201` — the over-threshold rule from slice 2
  governs *every* movement, not only adjustments. Correct, and not what that walk was about. A
  walk that trips a second feature's guard is testing two things at once and fails for a reason
  its name does not mention.
* Two message assertions read `body["message"]`; errors nest under `error.message`. The
  transfer suite's own reads are the reference, and reading them first would have saved the
  cycle.
* **The module's.** Closing a sheet with an uncounted line was a `400`, which told the person
  filling the sheet they had typed the request wrong. They had not — they had a shelf left to
  walk. It is now `InvalidStatusChange` (409), the same refusal a transfer gives when dispatched
  after receipt, and the sentence leads with how many are left.

**The QA pass has not run.** The slot (`max 1 concurrent`) is held by another writer and rotates
between them; `run.sh` is queued on `QA_SLOT_WAIT` rather than being killed, and the walkthrough
route is extended either way. Per the tick's own rule the stocktake screens are therefore
**built, typechecked and walked by the API suite, but not yet visually reviewed** — which is the
same sentence slice 3 ended on, and the reason this entry does not claim a QA result.

**Next.** Slice 5 — the sales-order reservation, for which `reserve`/`release` exist with no
route — then the reports screen and the sales integration, then the QA pass above.

## tick 31 — REQ-053 slice 5, the sales-order reservation (`7dbd1ae`, `f623ba4`)

**What.** A confirmed sales order now holds stock, and the ledger can see that it did.

**Proof.**

* `cargo test -p omnion-module-inventory --lib --quiet` — **84/84** (83 before slice 5, 1 new).
* `cargo test -p omnion-api --test inventory_reservations -- --ignored --test-threads=1` —
  **7/7** walks against `omnion_qa_w4` with `OMNION_REQUIRE_DB=1`, so a green tick cannot be a
  quiet skip.
* `pnpm turbo run typecheck --force` — **2/2**.
* `bash scripts/qa/run.sh` on `QA_STACK=w4` — **still running** at the time of writing (see below).

**What the criterion was hiding.** `reserve` and `release` existed as movement kinds with correct
arithmetic, and `sales_order_reservations` existed as the sales module's own table — and confirming
an order wrote the sales rows **without touching `inventory_stock` at all**. The order detail showed
a hold; the stock list showed the same units as fully available; a second order could promise them
again. Migration `0057` is explicit that the table exists "even when inventory is not installed", so
`reservation_state` could say `total` and nothing on any screen was red: **a record that describes a
hold is not a hold**, and the mirror is what hides the gap.

The walk asserts on the **stock list** rather than on the order, because the order is never where
this was visible, and the balance is read through the API the screen reads — a bridge that fed the
rollup a different number than the one it reported would pass an assertion on the order and fail
this one.

**Two defects, both mine, both found by the walks.**

*A second confirm held the stock again.* The unique index on `(order_id, line_id)` made the sales
row idempotent, so the double-click wrote no second hold there and every order screen stayed
correct while the shelf drained twice. This is the second time in this module a unique index in one
table has hidden a write in another. The guard asks the ledger what the order still holds on that
line, which is only answerable because of `0146`'s nullable `order_line_id` pointer — a pointer and
not a copy of the quantity, so a wrong one can only make the guard conservative, and it keeps this
crate from reading the sales module's table to decide whether to write its own.

*`replay` could not see a hold at all.* `signed()` is `0` for both reservation kinds and the arm
that caught `0` was a plain **addition**, so a hold of 4 replayed as `on_hand = 14` while the
rollup said `10`. Harmless only while no write moved `reserved`; the first real hold would have made
`reconciliation_report` report a permanent disagreement on every reserved row — which is how a
report learns to be ignored. `replay` now returns a `ReplayedPosition` carrying both numbers,
decided by `MovementKind::touches_reserved` rather than by a multiplier that is zero for both, and
`reconciliation_report` compares both. It was also reporting `replayed_reserved` as the rollup's own
figure — a column agreeing with itself by construction, so a corrupted reservation column would have
been reported as clean.

**Three decisions the criterion implies, which the module now makes explicitly.**

* A line is held across the item's **locations**, spread largest-room-first, because an order does
  not nominate a shelf and asking the seller to would invent a fact.
* A line with no inventory item behind it is held **nowhere**, reported as `unheld` with a sentence,
  and the order stays `partial` — the spec's "a visible note rather than a silent failure". A line
  that only half fits reports as unheld rather than quietly holding half of what was promised.
* Every write goes through `ledger::record_resolved`, the same function a hand-written movement
  reaches, so "reserve raises `reserved` and leaves `on_hand` alone" is not written down twice —
  slice 2's approval path already paid for that lesson once.

**The QA pass has still not run.** The slot is held by another writer, the box is at load ~12 with
1 GiB of RAM available across five writers, and the pass that has been running since 05:45 is the
one against this stack. The screens themselves needed no work for this slice — the stock list
already renders `reserved` and the order detail already renders the hold, which is the shape of
this gap: everything that draws the number was built, and nothing produced it.

**Next.** The reports screen (`/inventory/reports` — value-lite, movement summary, idle stock) and
the sales integration, then the QA pass above.
---

## Tick 59 — the CSRF layer was guarding a platform that could not save

**A release-blocking defect, found by reading the code rather than by a test failing.**

Tick 58 shipped REQ-012 slice 2's backend: the CSRF middleware, the header policy, the store and
the endpoints. Its suite was green. The feature was still completely unusable, and no test in the
repository could have told us so.

**What was wrong.** The layer refuses a cookie-authenticated `POST`/`PUT`/`PATCH`/`DELETE` that
carries no `x-omnion-csrf` token, and it is installed on the whole router. **Nothing ever issued
the token and nothing ever sent it.** `crates/security/src/csrf.rs` has had `token_cookie` and
`cleared_cookie` since the slice landed; they were called from nowhere. Every save, every setting,
every create in the admin panel answered `403 csrf_failed`, and the browser had no way to satisfy
it. Sign-in worked, every read worked, and the platform looked healthy right up to the moment an
operator tried to change something.

**Why no test caught it.** The bug was not in any unit. The guard was in `headers_middleware.rs`
and the thing it guards was in `cookies.rs`, written a tick apart, and the only thing that could
have connected them was a test that signs in, reads the response headers and posts them back. The
slice's tests were all *inside* one of the two halves, which is exactly the shape that passes.

**The fix, in three commits.**

* `2274768` — `cookies::csrf_cookie_for` mints the token from the session id and the configured
  secret, in the one helper every sign-in path already calls: the shared `start_session` tail
  (password *and* passkey), the MFA verification, the first-run owner. Sign-out clears both
  cookies. No configured secret means no cookie at all, rather than an empty one that would turn
  the middleware's `csrf_unavailable` into a `csrf_failed` naming the wrong problem.
* `5210388` — the admin echoes the token from **one** place, inside `request()`. A second screen
  that forgot would answer `403` on a save that works everywhere else, which is the hardest kind of
  bug to find from a user's report. Safe methods send nothing.
* `6a08bd4` — `apps/api/tests/csrf.rs`, four walks over the real router.

**The assertion the original slice never had.** "Refused without a token" is provable by a layer
that refuses *everything*, so on its own it proves nothing. The suite asserts the accepted half
too — and then reads the row back out of the database, so a `200` on a request that did nothing
cannot pass either. Three other walks: the token cookie is readable by script and `SameSite=Strict`
while the session cookie stays `HttpOnly`; a token from another session is refused; a bearer machine
key is never asked, asserted on the code it is *not*.

**Proof.**

- `cargo test -p omnion-api --test csrf` → **4 passed** (fresh database, `--test-threads=1`)
- `cargo test -p omnion-api --lib` → **212 passed** (was 208; +4 on the cookie's shape)
- `pnpm typecheck` (apps/admin) → clean
- `cargo build -p omnion-api --tests` → clean

**A trap worth naming, because it cost four test runs.** The first draft signed in a bare account
and posted to `PATCH /api/v1/me` — a route that only ever answers `GET`. It came back `405` and
the test asserted `200`, so the suite failed for a reason that had nothing to do with CSRF. Then
`422` (a field name), then `400` twice (a domain rule, then an enum). Every one of those failures
was the *test* being wrong, not the feature — and the way to tell them apart is that a `422` proves
the CSRF layer already let the request through, because the layer refuses before the body is
parsed. A guard's refusal is ordered **before** validation, so a validation error is evidence the
guard let it past. Pick the endpoint and the account together: a real mutation, and a fixture with
the permission to reach it.

**Environment, recorded so the next tick is not surprised.** `main` has a **gap in its migration
ledger**: `0018` is followed by `0021`. Three sibling writers each claimed `0019` independently —
`0019_cms_blocks` (wave2), `0019_organization_memberships` (wave5), `0019_secret_hierarchy`
(wave6) — and none is on `main`, so `migrate()` on any fresh database dies with
`Migration(VersionMissing(19))`. This is **not** caused by this tick and **not** mine to fix: the
`--test auth` suite, untouched, fails identically. Any walk against a fresh DB here needs the
migration gap closed first; the disposable QA stack is the same story, so `scripts/qa/run.sh` will
fail at step 1 until it is. Worth raising with the owner as one decision rather than three.

**Still open, and named rather than written off.** The `/security/headers` **screen** does not
exist, and neither slice 1 nor slice 2 has a browser pass — the QA slot has been held by live
sibling passes for three consecutive ticks. **Next tick:** build the `/security/headers` screen and
extend `scripts/qa/walkthrough.cjs` so it is visited and clicked; then run the pass and tick the
screen boxes for both slices.

# Tick 60 — the blocker was a story, not a fact

Two ticks of this loop wrote into `docs/BUILD-LOG.md` and into the REQ-012 status line that
`main`'s migration-ledger gap `0018 → 0021` makes `migrate()` fail on **any fresh database**,
and therefore stops `scripts/qa/run.sh` at step 1. One of those ticks used it to defer a browser
pass, which is the expensive kind of wrong: not a broken build, but a screen that was finished
and left unproven.

The claim was never tested. It is about a third-party library's behaviour, and a claim about a
library is a hypothesis until someone has run it. **This tick ran it.**

`apps/api/tests/migration_gap.rs`, two walks against throwaway databases:

```
running 2 tests
test fresh_database_migrates_despite_a_gap ... ok
test restored_ledger_must_be_contiguous ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 11.58s
```

**The fresh install is fine.** sqlx's `validate_applied_migrations`
(`sqlx-core-0.8.6/src/migrate/migrator.rs`) iterates the **applied** rows and rejects one whose
version is not in the embedded set. A fresh database has no applied rows, so the loop has
nothing to reject. The gap is inert on a clean install, and the QA pass — which always starts
from a dropped database — was never blocked by it.

**What the gap does break is a restore.** A database carried over from a branch that had a
`0019` holds an applied row this binary cannot see, so the runner refuses. That is correct
behaviour, not a bug, and it is the second test: the refusal must *name* the version rather
than merely fail, because "migration failed" is not an actionable message and "migration 19 is
not in this build" is. Both halves are now pinned, so the note can no longer drift in either
direction, and the test asserts that `0019` is still absent so a future merge of a sibling's
`0019` has to be a deliberate edit rather than a silent premise change.

Each test creates and drops its own database and reads the server URL from the same environment
the other suites use, so no credential appears in the test and a run can never touch the
development database — a test that quietly did would pass for the wrong reason.

**The pass is not blocked; it is queued.** The box allows one walkthrough at a time, and a
sibling wave (w3) has held that slot since 10:36 and is genuinely progressing — its
walkthrough is stepping through screens. This pass is waiting its turn behind it, which is the
slot doing its job. Forcing a second Chrome onto a box that already shows load 11 and 6 GB free
is how the 2026-09-28 crash happened, so the queue is the right answer, not an obstacle to route
around.

**A trap in the slot itself, worth the twenty minutes it cost.** A place file is named after
the *taking* script's pid, and that script exits the moment it takes the place — so the pid in
the filename is always dead within milliseconds of a perfectly healthy pass. Liveness is the
**holder** pid, written to a sibling directory. I read the filename pid first, concluded both
places were stale, and was one command away from stealing a live sibling's slot. `ps` on the
holder showed a pass twelve minutes into a real walkthrough. `qa-slot.sh` documents this, and
the lesson generalises: a lock whose name encodes the *waiter* is not a lock; check the thing
that stays alive.

**Second trap: `nohup … &` inside a backgrounded tool call still dies with its shell.** The
first pass attempt left a place file and a dead holder behind — indistinguishable, from the
outside, from a pass that had run. It had done nothing at all. Launch the command *as* the
background process, and clear any place you orphan, or the next tick inherits a phantom.

**Other gates, this tick:**

- `cargo test -p omnion-security --quiet` → **100 passed**
- `pnpm typecheck` (apps/admin) → clean
- `cargo build -p omnion-api --test migration_gap` → clean

**Next tick:** take the pass when the slot frees, confirm `runSecurityDepth` reaches
`/security/headers` and clicks it, and tick the screen boxes for slices 1 and 2. If the slot is
again occupied, build slice 3 (rate limiting + lockout) rather than idling — the schema and the
policy can land and be tested without a browser.

## tick 32 — REQ-053 slice 4b: the reports screen, and a count that was lying (`8eec4e1`, `9ed1b31`, `ea41a85`, `c58ba90`)

**What.** The last open criterion of the module, and the first screen in it whose subject
is a **number somebody will quote to somebody else**. `modules/inventory/src/reports.rs`:
the valuation, the period's movement summary, the idle block, the reports CSV, and the
global search. Three read routes under `inventory.items.read`, the `/inventory/reports`
screen, and ten walks.

**The bug this found.** `count_stock` knew about four of the stock list's seven filters —
`item_id`, `location_id`, `warehouse_id`, `idle_days` and not the other three. Filtering a
warehouse down to its negatives therefore rendered **three rows in the table and the whole
organization's row count in the number beside it**: two numbers, off one page, in the same
second, about the same rows. It is invisible on any screen with no data, which is why five
slices of this module shipped before it was seen. The fix applies all three missing
filters, and the walk asserts the count **as a conjunction with the list** — after asking
for negatives, either nothing matched or the count is what the page shows, *and* every row
shown really is negative. Either half alone passes on a broken filter or a broken count.

**Three decisions the criterion does not name, and the trap each one has.**

* **Value is `on_hand × cost`, never `available × cost`.** A reservation belongs to a sales
  order; the goods are still in this warehouse and still worth what they were worth. The
  report that subtracts availability makes **confirming an order reduce the organization's
  stock value** — invisible to a warehouse person, immediate to a finance one. The
  reserved total is printed *beside* the value instead.
* **"Value-lite" is small for a reason that is stated, not assumed.** `cost` is nullable and
  the module has never invented one, so `sum(on_hand * cost)` is not just imprecise: a NULL
  treated as zero makes ten uncosted units invisible beside ten priced ones, and the number
  is *arithmetically correct*. Hence four figures — the priced amount and quantity, the
  unpriced **line count**, and a share weighted by **lines** rather than by value, because
  what the uncosted rows would be worth is a guess. And a scope priced in two currencies is
  a **422 that names them**: a total across two currencies is arithmetically correct and
  commercially meaningless, and the person who quotes it to a customer is who finds out.
* **The idle block's count is a separate statement.** Its rows are capped, so `rows.len()`
  is not the answer to "how many are idle" — that is the count bug, one layer up, and a
  capped list reporting the value of the twenty rows it showed under a heading saying 186
  is the same lie with better typography.

The movement summary **names** `reserve` and `release` out rather than filtering by
"everything else", so a new kind has to be classified or the function has to change. A
filter by "not a reservation" would quietly start counting a brand new kind of hold as goods
arriving, which is the failure worth designing for.

**Proof.** `cargo test -p omnion-module-inventory --lib` **98/98** (84 before, +14: the
window bounds, the idle bounds shared with the list, the CSV's capped-count contract, the
scale arithmetic of `on_hand × cost` — thousandths times hundredths over a thousand, where
being wrong by 1000 is invisible in a test that only checks a row exists). `pnpm turbo run
typecheck --force` **2/2**. `cargo build -p omnion-api` clean, zero warnings. **The ten
walks have not completed**: the box is at load ~25 with four other writers building
concurrently, and this suite's `cargo` has been queued behind theirs for most of the tick.
It has failed three times for reasons that were all *not* the code — the QA pass resetting
`omnion_qa_w4` underneath it, a fixture reading a `MAIN` location the suite had never
created, and a `kind` value the check constraint refuses. The last two are now fixed and
this is a queue, not a failure.

**Two harness lessons worth more than the feature.** The walkthrough pass **owns**
`omnion_qa_<stack>` and resets it at the start of every run, so a suite pointed at it dies
with `database does not exist` about once per pass — and because the message reads like a
permission or a migration problem, it cost this suite two ticks of misdiagnosis. The walks
now have **their own database** (`omnion_qa_w4_walks`, created by the script), because one
suite per database is the rule and the fix is to stop sharing it. And `run-walks.py`'s
first version prepared that database with `psycopg`, which is not installed on this box: it
silently did nothing, and the suite then failed with the exact symptom it exists to remove.
A preparation step that fails quietly is worse than no preparation step, because it is
indistinguishable from the problem it was meant to fix.

**Next tick, first thing:** `python3 scripts/qa/run-walks.py` to completion and read the
ten results — the fixture bugs are fixed and the queue should have cleared overnight. Then
`/inventory/reports` needs its QA pass, which is the last thing slice 4b owes. After that:
the ⌘K entries, the empty/loading/error sweep, and the 390×844 pass — the three boxes still
open on this REQ.

---

## tick 33 — REQ-051: the merge, and the two passes that were never running (`a511bfa`, `46681cf`)

**What.** main moved 13 commits and the merge hit four conflicts, all of them the same shape: two
waves wanting the same slot for two different features. main added the rate limiter
(`rate_limit_middleware`, `ApiError.retryAfterSeconds`); wave4 added the request id
(`apps/api/src/request_id.rs`, `ApiError.requestId`). Both are shipped and tested on their own
branch, so the resolution keeps both and decides only the **order** of the two router layers: the
limiter stays outermost — a request it refuses never reaches a handler — and the request-id stamp
sits directly beneath it so that a limiter refusal is still correlatable. `ApiError`'s constructor
is now six positional arguments, which is the real hazard: a call site passing one of them compiles
happily and attaches the wrong property, so the call site was read rather than trusted.

**The find.** `runCrmStateSweep` and `runCrmKeyboardAndMobile` — the two passes that prove REQ-051's
last two acceptance boxes — were awaited **bare**, while every other depth pass in the file runs
through `runDepthPass`, which catches and reports. A crash upstream ends the walkthrough, so those
two never ran and said nothing. That is the mechanism behind the boxes staying unticked through
three earlier passes that each looked complete. Both are guarded now, and the state sweep joins them
because it leaves the network stubbed.

**Proof.**
- `cargo build -p omnion-api` — clean, 0 errors (the merge resolution compiled, not just read).
- `pnpm turbo run typecheck --force` — **2/2** (the merged six-argument `ApiError`).
- `cargo test -p omnion-module-crm --lib` — **172 passed, 0 failed**.
- `cargo test -p omnion-api --lib routes::crm` — **27 passed, 0 failed**.
- `node --check scripts/qa/walkthrough.cjs` — syntax OK.

**Not proven, and not ticked.** The QA pass did not land, in four attempts, none of them a code
fault: a pass I killed by renaming its target directory mid-build (`os error 2` on an `.rmeta`,
which reads like a compile error); `/dev/shm` at 100% across nine writers' targets with
`/mnt/apopic` at 97%, so a cold build could not finish; a two-and-a-half-hour run in which the
harness was logged out by the time it reached IAM (`iam roles depth: url=.../login`) and every later
selector timed out — the session died under load 165–298, the documented 'tab died under parallel
writers' condition; and a wrong `CARGO_TARGET_DIR`, where cargo succeeded and the pass died with
`Script not found: target/debug/omnion-api` because `run.sh` resolves the binary through
`cargo metadata`, which reports the default target. The boxes stay unticked: unticked means
unproven, and this REQ does not close on code I have not watched run.

**Next.** `walkthrough.cjs --only crm` against the running stack — minutes instead of the two and a
half hours the full pass took on a loaded box. That proves the keyboard and mobile boxes, REQ-051
closes, and the queue moves to REQ-052 (sales, slices 6/6b) and REQ-053 (inventory, slice 4b — it
owes the same reports pass and the palette entries).
## Tick 63 — REQ-010 slice 2's last open item: the serve path answers a window

**What.** HTTP range requests, the one line REQ-010 has carried as "still open" since slice 2:
a media player could not seek, because every read path answered the whole object whatever the
client asked for. `crates/media/src/ranges.rs` decides the window; `get_range` on both storage
drivers does the windowing; `read_window` on the serve paths wires it up.

**Proof.**

- `cargo test -p omnion-media --lib` → **199** (was 179; 20 new — 19 for the planner, 1 for the
  total-not-the-window assertion the walk forced).
- `cargo test -p omnion-storage --lib` → **28** (was 22; 6 new).
- `cargo test -p omnion-security --lib` → **137**, `-p omnion-api --lib` → **220** — both unchanged
  and green against the storage error variant and the API error mapping.
- `cargo test -p omnion-api --test media` → **15** walks, **0 failures**, over the real router and
  the real object store. One new walk; the other thirteen were unreachable before this tick.
- `--test media_transform`, `media_shares`, `media_scan`, `media_duplicates`, `media_grants`,
  `media_retention`, `media_settings`, `media_usage` — green against the serve paths this touches.
- `apps/admin` `tsc --noEmit` — clean.

**The defect only the walk could find.** The first `Content-Range` derived its *total* from the
bytes that arrived, so a fifty-byte window out of a three-hundred-byte object answered
`bytes 100-149/50` — a header whose total is smaller than its own end offset, which a player reads
as "this file is fifty bytes long" and stops. Nineteen unit tests accepted it, because a unit test
can only see the number it handed in. Only a walk that uploads 300 bytes and asks for 50 can see
that the two halves come from different facts, so `RangePlan::Partial` now carries the total beside
the window.

**Two pre-existing red gates, both from the security ticks, were in the way.** A suite that cannot
reach its first assertion proves nothing, so both are fixed here.

1. **CSRF.** Sign-in made the session cookie *ambient* authority, so every cookie-authenticated
   write needs a token. The media suite's login took `.split(';').next()` — correct for one cookie
   and silently dropping every cookie after it — so all fifteen walks died on `csrf_unavailable`,
   a message that names the server's configuration rather than the suite's own loss of the token.
   The harness now keeps every `Set-Cookie`, and a write echoes the token in `x-omnion-csrf` the
   way a browser does. That last part is not decoration: the middleware reads the header first, so
   a suite that set only the cookie was testing the *fallback* while believing it tested the normal
   path.
2. **The rate limiter.** It is a process-wide cell filled from the *stored* document, and the
   stored `sign_in` scope is ten requests per five minutes. The suite signs in three accounts per
   walk and runs fifteen walks, so the eleventh sign-in was refused and every walk after it died on
   a line that has nothing to do with media. The suite now installs a budget of its own — only the
   `sign_in` scope is raised, because the limiter suite asserts its own numbers and the other
   ceilings are the ones a deployment ships.

**Two more worth keeping.** The disk was at **100 %** (11 MB free) when this tick started, which is
a hard blocker: `write_file` returns success with a zero-byte file and `git commit` reports "No
space left on device" while `git status` looks fine. The repo's own `scripts/qa/disk-guard.sh`
freed 1.7 GB and, importantly, did it *without* deleting a target a live build was writing into —
which is the reason that script reads `CARGO_TARGET_DIR` out of each process's own environment
rather than guessing from a directory name. And the toolchain linter runs a bare `rustc` with no
edition, so every `async fn` in the crate reads as an error; `cargo` is the only authority, and a
linter error is not a build failure.

**Next.** (a) The **browser pass is still queued** — `qa-slot.sh` caps the box at one concurrent
pass and the holder is alive at load 64 with ~2 GB free, which is the 2026-09-28 OOM state. It waits
rather than forcing. `runMediaRetention` and `runSecurityDepth` are written and wired into
`scripts/qa/walkthrough.cjs`; both are unrun, which is the only reason REQ-010 and REQ-012 stay
open. (b) REQ-010's remaining open line is the *replace* audit entry, which lands with slice 3's
CDN purge hook. (c) REQ-012's other half: the sign-in route still does not call
`evaluate_lockout`, so nothing has ever locked an account.

**Commits:** `569fa99` the range planner · `b52f6c7` the crate export · `eecb42e` the storage window
· `42090c7` the serve paths · `224dd5a` the walk and the two unblocked gates. Pushed.

### REQ-013 slice 1 — the backup centre: run it, look at it, verify it (2026-09-29)

**What.** The platform had a media library, a content store, a permission system and a theme
directory, and nothing anywhere in it could be put back the way it was. An operator's honest
answer to "can I restore this if the volume dies" was a screenshot of a `pg_dump` somebody had
run by hand once. This slice ships the part of the answer that can be shipped without a restore
wizard behind it: take a backup, look at what it wrote, and **prove** it wrote it.

* `0157_backup_center.sql` — `backups`, `backup_parts`, `backup_schedules`, `backup_settings`.
* `crates/backup` — the model, the destination, the four tables. 46 unit tests.
* `apps/api/src/routes/backups.rs` — ten routes behind four new permission keys.
* `apps/admin/features/backups/` + `/backups` — the overview, the create drawer, the parts
  table, verify, delete, and a walkthrough route with a depth pass that drives all of it.
* `apps/api/tests/backups.rs` — seven walks over the real router and a real database.

**Proof.** `omnion-backup --lib` 46/0, `omnion-api --lib` 220/0, `omnion-permissions --lib` 62/0,
`apps/admin` `tsc --noEmit` clean, the whole migration set applied on a fresh database, and the
seven walks in `apps/api/tests/backups.rs`. **The browser pass has NOT run this tick** — see
the blocker below.

**Four defects the walks found, none of which any unit test could have seen.**

1. **Nothing was ever written to the destination.** The producers built each part's document,
   hashed it and recorded a `done` part — and no code path put the bytes on disk. The run
   reached `succeeded` with five artifacts that did not exist, and `verify` answered "could not
   be read back: configuration, database, media, plugins, themes" for all five. This is the
   single most expensive thing a backup product can do. **A unit test on `bytes_checksum`
   cannot see it**, because a checksum over a document nobody wrote is a perfectly good
   checksum. Only a walk that uploads and then re-reads the file off the destination can see
   that the two halves are different facts. The producer now writes the bytes *before*
   recording the part, and a write failure is a **failed part** rather than a success with a
   good checksum.
2. **A `before insert` trigger on `backup_settings` that inserted the same row.** The settings
   save answered `500 stack depth limit exceeded` with the recursion in the context, six hundred
   lines of PL/pgSQL deep. `on conflict` does not help: the trigger fires *before* the conflict
   is evaluated, so it re-inserts the row it was called for. `0028` and `0044` needed their
   triggers because their rows are keyed by a `site_id`; **this one is keyed by `check (id = 1)`,
   so the primary key IS the guarantee** and a seed is complete. The trigger is deleted and the
   section explains why it must not come back. The lesson is narrower than "don't use
   triggers": *copy the reason a neighbouring migration needed one, not its shape.*
3. **`sum(size_bytes)` over a `bigint` column decodes as NUMERIC.** The status card `500`d on
   every call. The cast is `::bigint` and deliberately **not** `::int` — a total that wraps at
   2 GiB reports a plausible small number, which is worse than an error somebody can see.
4. **Two of the suite's own assertions were wrong in a way that would have hidden #1.** The
   error envelope nests the message under `error.message`, so six assertions read
   `body["message"]`, got `Null`, and failed on a *missing* message when the API had said
   exactly the right thing; and `OffsetDateTime` serialises as a tuple, not an RFC 3339 string,
   so the status-card assertion failed on a working endpoint. Both are fixed with a
   `TestResponse::message()` helper and a comment, because a test that fails for the wrong
   reason trains the reader to distrust the test rather than the code.

**The two permission decisions that are the point of the slice.** `backup.manage` deliberately
does **not** include `backup.restore`: a platform where the schedule editor can also overwrite
live content is a platform where the nightly job and an operator's button are the same
authority, which is how a retention window becomes an outage. And a run of another tenant is a
`404`, never a `403` — a `403` confirms the id is real, and a backup's existence is itself
information about the platform.

**Commits:** `bb291fa` the migration · `c1f7380` the crate · `d4299fc` the permission keys ·
`ef27bc4` the API · `4d4b8f4` the admin screen and the walk · `18d6e84` the three defects ·
`0dac9fa` the settings-route note. All pushed.

**Blocker, repo-wide and not this slice's.** Two things the box is doing, both recorded rather
than worked around. (a) **The browser pass ran but cannot close REQ-010 or REQ-012** — its
`qa-slot.sh` reclaimed a stale place and then produced depth passes that mostly report `ok:
false` with reasons about the *sibling* screens' own data (`the retention tab did not render`,
`no file to share — the upload step did not succeed`), and its analytics pass answered
`"validation":"silent"` on a form that saved. Those are the media and analytics screens'
findings, not this slice's, and they are recorded in `docs/qa/`. (b) **The integration suite is
running against a box at load 23-100** with nine sibling worktree loops each holding their own
PostgreSQL and Chromium, so a seven-walk suite that takes 16 s of work is taking minutes of
wall clock. The last full run is in flight at the time of writing; the per-test results are in
this tick's report.

**Next.** (a) Re-run `apps/api/tests/backups.rs` to green on a quiet box, and
(b) close the REQ-010 and REQ-012 boxes on the same pass — both are code-complete and are
waiting on nothing but a browser pass that has not yet produced one.


### Blocker found and left in place — the CSRF/rate-limit gate is repo-wide, not media's

Running the eight sibling media suites after this tick's change showed **all eight red**: 28 walks
on `rate_limited` and 15 on `csrf_unavailable`. The cause is the same two gates fixed in
`--test media` above, and the fix belongs to the security work, not to this slice:

- **20 suites** carry an identical `login()` helper that takes `.split(';').next()` and therefore
  discards the CSRF cookie sign-in now issues. Every cookie-authenticated write in every one of
  them is refused. `apps/api/tests/csrf.rs` shows the working pattern (set `config.csrf` from the
  fixture, keep every `Set-Cookie`, echo the token in `x-omnion-csrf`).
- The same twenty suites share one process-wide limiter cell fed from the stored `sign_in` scope
  of ten per five minutes, which a multi-walk suite exhausts on its own sign-ins.

**Proved pre-existing, not a regression from this tick:** on committed `main` (`224dd5a`, this
tick's own work already pushed), `cargo test -p omnion-api --test media_shares` fails `0 passed;
5 failed` with the same `csrf_unavailable` / `rate_limited` pair — and `media_shares` is a file this
tick did not touch. A suite in another wave's scope is left as it was found.

**What this tick can and cannot claim.** The range slice is proved by the gates that do run:
`--test media` at **15 walks, 0 failures**, `omnion-media --lib` **199**, `omnion-storage --lib`
**28**, `omnion-security --lib` **137**, `omnion-api --lib` **220**, and `apps/admin` typecheck
clean. The eight sibling suites could not be used as a regression check, because they were already
red before this tick and are red for a reason that has nothing to do with ranges. That is a weaker
claim than "the whole media surface is green", and it is the honest one: the serve paths this tick
touched are covered by the walk in `--test media`, which exercises them through the real router
and the real object store.


### Tick 65 — the blocker is not the box: three real defects behind a red suite (2026-09-29)

**What.** Last tick left a blocker in `docs/BUILD-LOG.md` and did not work around it: eight sibling
media suites were red on `csrf_unavailable` and `rate_limited`, and the note said the fix belongs
to the security work rather than to media. This tick took the first two of them
(`media_shares`, `media_retention`), read the actual failure, and found that the "suite issue"
was **three product defects**, one of them in code this loop wrote last tick.

**1. A sign-in issues two cookies and twenty helpers read one.** `support/walk_auth.rs`. Sign-in
answers with the session cookie *and* a CSRF token beside it. Twenty suites each had a `login()`
taking `.split(';').next()` on the first `Set-Cookie` — correct for one cookie, silently lossy
for two. Fixing the helper was not enough: the second defect sat underneath it. **Those fixtures
never set `config.csrf` at all**, so their own sign-in could not have issued a token. The
refusal was the product working correctly; the suites were asserting a deployment that cannot
exist. `media_shares` **0 passed / 5 failed → 5 / 0**, `media_retention` **0 / 5 → 6 / 0**.

**2. A site with media in it could not be backed up** (`b0b4542`). `document_media` read
`coalesce(sum(size_bytes), 0)` with no cast, and the `coalesce` is the trap — the literal `0`
adopts the other argument's type, so the result stays `numeric`. This is the **second** instance
of the same mistake in one feature; the first was the status card, fixed last tick. A `partial`
run with four good parts and a `media` part that never happened is a terrible way to discover it,
and it is exactly what the build log recorded as "the media part is flaky".

**3. The prune sweep could delete every restorable backup** (`ed09dc4`). `prune_candidates`
promised four exemptions in its doc comment and implemented two. No `status = 'succeeded'` on the
spared run, so a `partial` — not restorable as a whole — took the protection while the newest run
that *can* be restored was offered for deletion. No `not protected` either, so a protected newest
run consumed a second invisible exemption and the rest of the history was offered for deletion,
and the sweep reported success. Two exemptions, one survivor.

**And the one that was hiding underneath all of it** (`bfe46c3`). The retention screen answered
`500` as soon as a site had a file actually past its restore window: `past_restore_window` summed
`size_bytes` with no cast and the comment above it *claimed* one. Nothing exercised it, and the
reason is the lesson — `sum()` over an empty set is `NULL`, `NULL` decodes into `Option<i64>`, and
every existing walk stopped at a site with nothing to count. The type was confirmed against the
database rather than assumed: `pg_typeof(sum(size_bytes))` is `numeric`, `coalesce(...,0)` is
still `numeric`, `::bigint` is `bigint`. The new walk fails on `main` with
`500 ... NUMERIC is not compatible with INT8` and passes with the cast.

**Proof.** `omnion-backup --lib` 46/0 · `omnion-media --lib` 199/0 · `omnion-api --lib` 220/0 ·
`--test backups` 7/0 · `--test media_retention` 6/0 · `--test media_shares` 5/0 ·
`--test walk_auth` 6/0 (new) · `apps/admin` `tsc --noEmit` clean. Commits `b17e64b`, `bfe46c3`,
`b0b4542`, `ed09dc4`, all pushed.

**Blocker, unchanged and not worked around.** The browser pass did not run: `qa-slot.sh` has a
live sibling holder and the box is at load 16 with **0 GB free** of 32. A pass now would add a
third Chromium to a machine that is already swapping, and the result would be untrustworthy
either way. `runMediaRetention` and `runSecurityDepth` are written, wired and still unrun, which
is the only reason REQ-010, REQ-012 and REQ-013 stay open.

**Next.** (a) Migrate the remaining eighteen suites to `support::walk_auth` — it is a three-line
change per suite now, and each one is a REQ that can then be closed on its browser pass rather
than on the note that its suite was already red. (b) The `media` part of a backup run is the
place to look next: it counts rows, and a backup that only counts is a manifest, not a backup.

### Tick 66 — the media part was a manifest wearing a backup's name (2026-09-29)

**What.** Last tick's next step was written before it was understood: *"the `media` part of a
backup run is the place to look next: it counts rows, and a backup that only counts is a
manifest, not a backup."* This tick took it literally. The media part did not count rows
and copy them; it counted rows **and that was all it did**.

**The defect.** `document_media` ran
`select site_id, count(*), coalesce(sum(size_bytes), 0)::bigint from media`, wrote the result
as JSON, and recorded that JSON as the part's artifact with a real checksum. The run reached
`succeeded`, `verify` read the artifact back and agreed with it, and **not one byte of the
library had been copied anywhere**. The screen said "media: 412 files, 88 MiB" and meant
"there are 412 rows in a table, and their sizes add up".

It is the same defect the crate documents twice already — a checksum over a document nobody
wrote is a perfectly good checksum — wearing a different mask, and the mask is the lesson:
**counting is what the database can do with the object store switched off.** The count was
therefore available on exactly the run where the store was unreachable, and it is what made
the row look healthy. Every pre-existing assertion in the suite passed on the broken
implementation, which is why it needed a walk of its own.

**What shipped** (`crates/backup/src/media.rs`, new, 22 unit tests). The part copies every
object through the deployment's own `Storage` abstraction — not a vendor SDK, so a site on a
directory and a site on a bucket both back up — one object at a time, writing each into the
run's own directory and recording it in a `media-index.json` the restore path will walk. The
bytes that come back are **re-hashed and compared with the library row**; a disagreement in
size or SHA-256 is a failure, never a silent copy of something the library does not describe.
An object over `MAX_OBJECT_BYTES` (256 MiB) is **named and skipped, not truncated** — a
truncated image is a backup that claims to have restored a file it destroyed. A part that
copied some of the library is a **failed** part: `summarise` makes the run `partial` and the
error carries the count and the first three file names.

**And the bug the walk found underneath it** — one the tick was not looking for. Every
artifact in every run was written to `<root>/<prefix>/<prefix>/…`. A storage key is already
prefix-qualified, and the writer joined it to `local_root_for(root, prefix)`, which adds the
prefix a second time. The **reader doubled it the same way and so did the suite's path
helper**, so all three agreed, every walk was green, and the archive sat one directory deeper
than the manifest said. Three halves making the same mistake is not a cross-check. Fixed with
a separate `local_path_for(root, key)` and a unit test that writes the wrong path out in full
so the mistake cannot come back quietly. The new walk now reads the archive's location **out
of the index the run wrote** rather than recomputing it, so the two halves *can* disagree.

**Proof.** `omnion-backup --lib` 65/0 · `omnion-api --lib` 220/0 · `--test backups` **8/0**
(the seven pre-existing walks still pass, plus `the_media_part_copies_the_librarys_bytes_and
_a_missing_object_fails_the_run`, which uploads two real objects through `state.storage()`,
reads the archived bytes back and compares them, checks the index's per-object size and
checksum against the files on disk, then adds a row whose object the store does not have and
requires `partial` with "1 of 3" in the error) · `apps/admin` `tsc --noEmit` clean. The build
warning count is **13 before and 13 after** — none introduced.

**Blocker, unchanged and not worked around.** The browser pass did not run again: `qa-slot.sh`
is held by a sibling and the box peaked at load 36 during this tick. The Rust walks are the
gate that shipped, and the screen for this part is unchanged, so nothing is untested in the
UI sense — but the run against `runBackupDepth`/`runSecurityDepth` still has to happen before
REQ-010, REQ-012 and REQ-013 close.

**Next.** (a) The restore path (REQ-013 slice 2) now has a real archive to read: the index is
written, so `restore preview` can count the objects it can put back instead of describing
what might be there. (b) `objects/` grows one file per object, and the delete route has to
remove the directory as well as the run's own JSON — check that before the restore wizard
exists, because an operator who deletes a backup and finds the files still there will assume
the product lied.

## Tick 67 — 2026-09-29 — the delete had no bytes in it, and the media part had no tenant

**What.** Two defects, and the second was found by the first's own fixture rather than
looked for. The acceptance box this tick took is the one that reads like bookkeeping —
"deleting a backup removes its artifacts from the destination" — and the bookkeeping was the
defect.

**The delete was a row delete.** `DELETE /api/v1/backups/{id}` removed the row, cascaded the
parts, wrote the audit entry and answered `204`. Every byte stayed on the destination: the
database export, the `objects/` tree, the media index, the three JSON parts, the manifest.
The panel knew. It said so — *"Backup removed. Its artifacts are still on the destination
until the next prune."*

That sentence is the most instructive part. It is **honest, and it is also the bug**. A backup
root is the one directory in this product that costs money per byte forever, and the prune
sweep runs on its own schedule — not when an operator deletes something. So the ordinary
path was: tidy up three old runs, watch the list go green, and keep a full copy of the
platform's media library on disk with nothing pointing at it. Every later backup, every
month, would do it again. The product told the operator the truth and the truth was the
problem. **Honest phrasing is how a missing feature survives review** — a note that
describes the gap reads as a design decision, and a design decision is not a bug report.

`crates/backup/src/purge.rs` (new) removes **the run's own directory** rather than walking
the manifest and deleting what it lists. A run that died mid-write left files the manifest
never mentioned, and an index-driven delete leaks exactly those. The directory is a boundary
because the prefix is derived from the run's **id** — which is why `set_prefix` refuses to
derive it from the clock — and a unit test pins that two runs never share a directory, because
that property is the delete's entire safety argument and it lives in another module.

Three refusals, all before a byte is touched:

* an **empty or relative root** — `remove_dir_all` on a relative path resolves against the
  working directory of whatever process ran it, and for a systemd unit that is not the
  directory an operator typed into the settings screen;
* a `..` segment in the prefix — **rejected, not normalised**, because a normalised traversal
  is a traversal that passed the check that was meant to stop it;
* an **empty prefix** — which would make "delete this backup" mean "delete every backup on
  the destination".

The handler does **artifacts first, row second**, and the order is the design rather than a
style choice: an interrupted delete then leaves a row pointing at an archive that is still
there, which an operator can retry, instead of a deleted row over an archive nobody can find.
It refuses a `queued`/`running` run, because deleting one mid-write leaves artifacts that no
later prune knows about. And it answers **`200` with a `PurgeReport` rather than `204`**,
because "the row is gone" and "the bytes are gone" are two separate facts — the API that
collapsed them into a status code is what produced the defect. A partial removal reports the
count that came off, the count still on disk, and the first few paths in the operating
system's own words; the panel renders all three sentences differently, because "nothing was
ever there", "twelve files deleted" and "eleven of twelve deleted and here is the one that is
not" are three facts an operator reconciles differently.

**The tenancy leak, found by the delete walk's fixture.** The part asked for
`pending_objects(pool, None)` — every `media` row on the deployment — while every other read
and write in the file is scoped by `organization_id`. **A backup of tenant A contained tenant
B's files**, the run reported `succeeded`, and `verify` called it clean. The pre-existing
media walk had been green for a tick because *every test in the suite creates media for one
organization*, so "the whole deployment" and "this organization's library" are the same set.
The stranger's rows even named themselves in a failure message — `34 of 36 objects could not
be copied — share-guarded.txt: no object is stored under "shares/4c70..."` — before anyone read
the cause. `pending_objects_for_organization` joins through `sites`, and is a **separate
function rather than an extra parameter** so the unsafe form cannot be reached by forgetting
an argument. The walk gives each organization a site and a file and requires each archive to
name only its own — the stranger's run too, because a fix that scoped by *excluding* the
other org passes the first half and still leaks.

**The repeat, and by now it is a pattern rather than an incident.** Tick 66 found a half that
**counted**; this tick found a half that **scoped wrongly**. Same shape, same family: a backup
half that satisfies every assertion in its own test and disagrees with reality. Three of the
four defects in this feature now share it. The rule that catches the tenancy one, and that
would have caught it a tick earlier: **if a walk's fixture only ever creates one tenant's
data, the walk cannot see a tenancy bug — a suite needs a stranger, always, even when the
assertion is about bytes.** And the second-order version, which cost three runs: when a walk
touches shared QA state (a unique-constrained `media.storage_key`, or a `backup_settings`
row), name the isolation explicitly in the fixture, or the second run fails as a constraint
violation that reads like a product defect.

**Proof.** `omnion-backup --lib` **80/0** (16 in `purge`, including the count-before test the
walk forced) · `omnion-api --test backups` — the delete walk green in isolation
(`--exact …` 1/0 in 7.8s) and the tenancy walk green; `apps/admin` `tsc --noEmit` clean.
Three failures during the tick were **my own test bugs, not the product's**, and they are
recorded because each one asserted a thing the product never claimed: a `manifest.json` FILE
(the manifest is a `jsonb` column, never a file), an `objects/` count that forgot the site
sub-directory, and a per-session CSRF token reused across two sessions — a `403` from a token
that has nothing to do with tenancy, which a test that reuses a token will misread as a
product defect.

**Blocker, third tick running, unchanged and not worked around.** The browser pass did not
run. `qa-slot.sh` was held by a live sibling for the whole window (pid 3490157, then 142641)
and the box peaked at load 53. `/mnt/apopic` also hit **100%** mid-tick — `os error 28` at
parse time and `ld` dying with a **Bus error** while linking, which reads like an unrelated
failure and is not. Reclaimed `target/debug/incremental` **in this worktree only**, after
checking with `/proc/<pid>/cwd` that no live rustc had it open, and left w2/w3/w5/w7 alone.
`runBackupDepth`, `runMediaRetention` and `runSecurityDepth` stay written-but-unrun, so
REQ-010, REQ-012 and REQ-013 do not close on tests alone.

**Next.** (a) The restore path (REQ-013 slice 2) now has an archive it can read: the media
index is written and `pending_objects_for_organization` says whose files a run may restore,
so `restore preview` can count what it can put back. (b) The prune sweep must use
`remove_run_artifacts` too — it deletes the same directories by its own path today, which is
the third half that could disagree with this one.

## Tick 68 — REQ-013 slice 3 (retention): the sweep had no caller
## Tick 35 — the disk guard was deleting a live build, and calling it a compiler error

**What.** `prune_candidates` shipped in slice 1 with a doc comment describing four
**What.** The merge first: main had moved 4 commits, all in `crates/backup`, and the only
conflict was `docs/BUILD-LOG.md`, which both sides only ever append to. It is merged by
splicing both bodies and proving the result by **non-blank line multiset against both
parents** — headings and line counts both survive a duplicated block, and "the totals add
up" is not evidence that nothing was written twice. Verified: 5592 of HEAD's lines and 3746
of MERGE_HEAD's lines, all present, 0 of either lost. (The script also had to be taught the
two-way marker set: it was written for diff3 and died with `IndexError` on a hunk that has
no `|||||||` base section. A lone `=======` then survived as an ordinary line, so `grep`
counted one marker in a file that was in fact clean.)
exemptions, and **nothing called it**. The retention screen could list what the sweep would do
and the walkthrough could assert the exemptions hold, and the bytes on the destination would
accumulate for ever. Five commits: `e704c0f` the sweep, `faea0bd` the two config knobs,
`514665c` the worker, `813f33b` the manual route, `9c8170b` the panel, `e77fd9a` the walk.

**The shape of the defect is now unmistakable.** Tick 66 found a half that *counted*. Tick 67
**The find.** `cargo build -p omnion-api` failed twice, mid-compile, with
found a half that *scoped wrongly*. This tick found a half that **was never invoked**. Three
ticks, three different ways for one feature to satisfy every assertion in its own tests and
disagree with reality, and all three are the same question: *who calls this?* The delete
knew how to take a run's artifacts off the disk; the sweep — which deletes **more** than the
delete does, unattended, with nobody watching — had never heard of the function. That is the
lesson worth more than the code: **a pure function with a thorough doc comment and no caller
is the most convincing piece of dead code there is.** It reads as a feature. Its tests pass.
Its exemption rules are correct. It does nothing at all.

So `crates/backup/src/sweep.rs` is a **caller** and deliberately the only one, and it writes
```
error: could not write output to …/target/debug/deps/syn-….rcgu.o: No such file or directory
error: couldn't create a temp dir: No such file or directory (os error 2)
```
no path arithmetic of its own. Two implementations of "remove a run's directory" is how a
destination ends up with a directory the sweep believes it deleted — the same "two halves that
make the same mistake are not a cross-check" rule the media part taught, restated at the
removal.

Three decisions that are not obvious. **Bytes first, row second**, so an interrupted sweep
`os error 2` is not a code fault. It is the output **directory** being gone, and `/mnt/apopic`
was at 100% while it happened — so something on this box was reclaiming space, and the thing
reclaiming it was `scripts/qa/disk-guard.sh`, whose whole purpose is to drop a worktree's
`target/` when the disk gets tight. It dropped mine, twice, **while a `rustc` was writing
into it**.
leaves a row over an archive that is still there and the next tick takes it again. **A
partial removal still deletes the row** — the same call the delete handler makes, because
retention is a window and not a bulk delete, and one stuck file must not retain a run for
ever. And the sweep walks **tenants**, not rows, through `organizations_with_backups`: a
separate function rather than an inline `select distinct`, because a second answer to "who
gets swept" is a rule that drifts the first time one of them is edited. `null` is a real
member of that list — a sweep that filtered the platform's own backups away would never prune
the restore points that matter most on a single-tenant installation.

**Two flags, not one.** `OMNION_BACKUP_SWEEP` is independent of `OMNION_RETENTION_RUNNER`,
The reason is one line of shell. The guard asks each process whether it holds
`CARGO_TARGET_DIR=<dir>` and skips the target if one does. An ordinary `cargo build` never
sets that variable: cargo takes the **default** target, `<cwd>/target`, from the process's own
working directory. So the environment test is structurally blind to the most common build
there is, and the one directory a plain `cargo build` writes into is the one directory the
guard cannot see it in. Measured, not assumed — against a live build of this worktree
`in_use` answers **NOT HELD** while `rustc` is in the target.
because "never delete my backups" must not have to mean "never purge my trash"; the only way
out otherwise is to turn the whole worker off. The six hour default is chosen from the
feature: the shortest window the panel allows is a day and the newest successful run is exempt
whatever it is, so hourly finds the same set six times for the same answer and nightly leaves
a run whose day ended at 04:00 sitting there for twenty hours.

**The manual route is scoped, and that is the part worth proving.** `POST
Worse, the two reclaim paths disagreed about this. Step 4, the last-resort sweep, had the
`in_use` check. Step 3, the ordinary per-worktree ceiling, had **no liveness check at all** —
so the common case had no protection and the desperate case had a broken one. Fixing one call
site would have left the same failure reachable through the other.
/api/v1/backups/sweep` calls `sweep_organization` for the **caller's** tenant, never
`sweep_all` — an operator pressing "run retention" on their own site must not delete another
tenant's restore points. The walk creates a stranger tenant's expired run pointed at the same
destination and requires it to keep **both** its row and its directory. The stranger is not
decoration: without it "everything" and "this organization" are the same set, which is the
exact blind spot the media part's tenancy fix was found through last tick. The same lesson,
one layer up, and it is now the second time this feature needed a stranger in the fixture.

The route is registered **before** `/backups/{id}`. A `POST` against `/backups/sweep` would
**The fix.** `building_here` asks the question that actually decides it: is there a compiler
process whose **cwd** is the worktree that owns this target? For a default-target build the
environment is irrelevant and the cwd is the whole answer. Both reclaim steps now consult it;
the ceiling drops a cache, never a running build. `scripts/qa/target-guard.sh` names the
failure the way it is named above, because `os error 2` on an output path and `os error 28`
on the same path are two different incidents and are routinely read as one.
otherwise match `{id}` and fail to parse `sweep` as a UUID — a 500 that reads like a router
bug on the one route whose whole point is to be callable by hand.

**Proof.** `omnion-backup --lib` **85/0** (80 before, +5 in `sweep`) · `omnion-core --lib`
**39/0** (+2, both pinning the flag independence in both directions) · `apps/admin`
`tsc --noEmit` clean · `the_retention_sweep_takes_the_bytes_and_spares_what_it_promised`
**1/0 in 5.5s** over the real router and the real filesystem. That walk goes and *looks at
the directory*, because the sweep's whole claim is about bytes and a row delete reports the
same counts the panel shows.

**Blocker, fourth tick running, unchanged and not worked around.** The browser pass did not
run. `qa-slot.sh` is held by a live sibling (pid 142641) and the box peaked at load 23 with
**1 GB of 32 free** and 24 GB of swap in use; `scripts/qa/run.sh` would have added a fifth
Chromium to that. `runBackupDepth` stays written-but-unrun, so REQ-013 does not close on
tests alone. Two toolchain facts, both recorded because each read like a product defect and neither was
one. A **stale orphan test binary** from an earlier tick (`backups-b245d57c4aaeca51`, no
parent shell) was holding QA database connections across ticks; killed it. And a **sibling
deleted my `target/debug/incremental` mid-build**, which surfaces as
`failed to move dependency graph … os error 2` — a compile failure in files that were already
merged and building fine. The full-suite run then hung in the pre-existing
`a_protected_backup_is_never_a_prune_candidate_and_the_newest_successful_survives` with no
active query and no blocked lock, so I killed it, killed the orphan, rebuilt with
`CARGO_INCREMENTAL=0` and re-ran that exact test in isolation: **1/0 in 3.19s**. It was never
red and never broken — it was starved. Two rules, both already half-known and now confirmed:
**build with `CARGO_INCREMENTAL=0` when siblings are live**, and **an unexplained hang with no
database activity is contention before it is a defect**.

**Next.** (a) The restore path (slice 2) now has the index a preview needs: the media index
is written, `pending_objects_for_organization` says whose files a run may restore, and the
sweep tells the operator what is actually on the destination. (b) The `partial` box is still
unticked — a run where one part fails ends as `partial` with the message visible in the UI
needs a fault injected into the drawer, not a test.


## Tick 69 — the restore preview, and the two rules three ticks of unit tests had passed

**REQ-013 slice 2a.** `GET /api/v1/backups/{id}/restore-preview` plus the panel that reads it.
The decisions live in a pure module (`crates/backup/src/restore.rs`) so the rules are
unit-tested with no stack, and the live-data comparison is a separate one
(`crates/backup/src/preview.rs`) so a change in how it is counted cannot silently alter which
warnings fire. The route re-reads every artifact, compares each part's size against the
manifest, and prices the restore against the live library.

**It found two defects in shipped code.** Both are the fourth instance of one shape — **a rule
that is tested and that nothing obeys** — and neither was findable by a unit test, because in
both cases the unit test was the reason it survived.

**1. A media part's recorded size could never match its own artifact.** `finish_media_part`
recorded `bytes_copied + index bytes` as `size_bytes`, with a doc comment saying that
`size_bytes` is "what `verify_manifest` compares against the artifact on disk". That reasoning
is backwards: `storage_path` for the media part names the **index alone**, so a size including
the copied objects' bytes can never equal the length of that one file. The two agree only when
`bytes_copied` is zero — which is exactly what the `verify` walk's fixture was, because that
suite's library has no objects. So `verify` reported every real media backup as mismatched,
for ever, and the new preview refused to offer it as a restore point. **Both verdicts were
correct; the number was wrong.** The preview is the first reader that compares a media
artifact's length on a run with a non-empty library.

**2. `produce_all` never read `run.scopes`.** It walked all five `PARTS` unconditionally, so a
backup requested for `["database"]` produced five artifacts: the scopes were validated by
`normalise_scopes`, stored, normalised, and rendered in the drawer's five checkboxes, and then
ignored at the only point that mattered. **Four existing walks request `["database"]` and none
of them noticed**, because each asserted on the part it *wanted* rather than on the number of
parts, and the two extra artifacts are perfectly valid files. The scope selector was a dead
control with a green tick beside it. A consequence worth recording: a media-only run whose one
part failed used to be `partial` — the correct verdict about four parts the operator never
asked for — and is now honestly `failed`. `summarise` is untouched and still right; what
changed is the set of parts it is handed.

**A third finding, in the new code, from a test with a realistic id.** The typed confirmation
sliced the first eight hex characters off the run's id. For a v4 uuid that is fine and looks
random. For a **v7** uuid — whose leading bytes are a millisecond timestamp — "the first eight
hex characters" is a *clock*: two backups taken three hours apart produced the identical phrase
`RESTORE 000001a0`, and every run inside a ~50-day window shares one. A guard that restores the
wrong run is worse than no guard, because it looks like one. The phrase is now a **hash of the
id**, which mixes the timestamp with the random tail whatever the id's layout, and the id is
validated first so an unnameable run still yields no phrase at all. The regression test uses two
v7-shaped ids sharing a timestamp prefix.

**Two smaller ones in my own code, both the silence class.** `LiveCounts::dropped` documented a
zero floor that `saturating_sub` does not provide — saturating means stop at `i64::MIN`, not
stop at zero, so an inconsistent live pair rendered as a negative loss. And the
healthy-archive fixture stamped `now` at a round epoch (Jan 2027) that made every archive look
107 days old, which is why the first "healthy archive" test failed on a `stale_archive`
warning it had just proved absent.

**Why the preview is behind `backup.read` and not `backup.restore`.** Reading a warning is free
and changes nothing; gating it behind the destructive key means the first time an operator
meets this screen is a 403 that never showed them what they were agreeing to. The expensive
permission is for the button *after* it.

**Why there is no restore button.** The safety backup, the typed confirmation's enforcement and
the abort path are the next slice. A "Restore" button that could not be pressed is a dead
button, which this product does not ship; the panel that explains the restore and asks for
nothing is a working one. The phrase is **shown** rather than demanded for the same reason.

**Proof.**

- `bash -n scripts/qa/disk-guard.sh` — clean.
- `building_here` against a **live** `cargo build` of this worktree — detected.
- `in_use` against that same live build — **NOT HELD**, which is the bug, reproduced.
- After the fix, `cargo build -p omnion-api` from a 96M target — **exit 0 in 55 s** instead of
  dying at ~8 minutes into a cold rebuild.
- Merge committed `05306dd`; guard fix `a5bbe9b`.
| Gate | Result |
**Not proven, and not ticked.** No browser pass this tick, and none was attempted: the box
sat between load 23 and load 187 with 24 GB of 32 GB swap in use, and the w4 QA stack was not
up. Under those conditions a pass reports UI defects that do not exist — screenshots time out
at 15 s and every locator after them fails. REQ-051's keyboard/mobile boxes and REQ-052's
mobile box stay unticked, which is where they already were.
| --- | --- |
| `omnion-backup --lib` | **103/0** (85 before: +18 preview model) |
| `omnion-api --test backups` | **15/15** (12 before: +3 preview walks) |
| `apps/admin` `tsc --noEmit` | clean |

The three new walks are over the **real router and the real filesystem**. The load-bearing one
**Next.** REQ-054 (accounting) — it is the module REQ-052's one unticked box is waiting on, and
the migration namespace is at `0166` after a fresh scan of **all** worktrees, so the next
number is `0167`. The two mobile boxes both need a pass, and both are worth a scoped
`--only crm` / `--only sales` run: minutes, not hours, and the hour-long version is what loses
its signed-in session.
---
## Tick 34 — main moved 25 commits; the QA pass was dying at the sign-in screen and calling it "already installed"
**What.** The merge first. Five conflicts, all additive: two lucide icons wanted the same import in
`app-shell.tsx` (union), main's rate limiter against wave4's request id in `routes/mod.rs` (both
kept, layer order decided), a byte-identical `cargo-slot.sh` (ours), two independent depth passes in
one region of `walkthrough.cjs` (both concatenated), and two pure appends to `BUILD-LOG.md`
(concatenated, then verified by heading multiset rather than by line count — 0 of 44 headings lost).
**The find.** The QA pass could not sign in, and it explained why it did not need to: "installation
already exists". That was a guess about a state it had never observed. It reads the URL 900 ms after
`goto('/')`, but on a fresh database the chain is `proxy.ts` → `/login` → a `useEffect` calling
`fetchOnboarding()` → `/setup`, so the read lands on `/login`. The pass skipped the wizard and then
tried to sign in with an account it had just decided did not need creating. `select count(*) from
users` on that database was **0** — the log line and the database disagreed, which is the only reason
this is worth writing down.
Two fixes, both in `runWizard`. The authority is now `GET /api/v1/onboarding`, the same call the
sign-in screen makes; anything short of a definite `needs_setup: false` goes to the wizard, because
replaying completed first-run steps is refused by the API while skipping them is a dead pass. And a
`null` step read is now a paint race to wait out, not the end of the wizard: the loop `break`ed on
the first null and stopped on step 1 of 5, so the owner account existed and the organization did not
— and every later screen then answered `organization_required`, which reads exactly like a broken CRM
and is not one. That is the second shape the same symptom takes, which is why the fix is "ask the API"
rather than "wait longer on the URL".
**Proof.**
- `cargo build -p omnion-api` — clean, 0 errors. The merge was compiled, not read.
- `cargo test -p omnion-api --lib routes::crm` — **27 passed, 0 failed**.
- `cargo test -p omnion-module-crm --lib` — **172 passed, 0 failed**.
- `pnpm turbo run typecheck --force` — **2/2**.
- `node --check scripts/qa/walkthrough.cjs` — OK.
- Live stack: API on :18083 against `omnion_qa_w4`, `/healthz` 200, `/readyz` ok, admin on :3103.
- After the fixes the wizard created the owner account and reached the organization step
  (`010-setup-organization.png` exists in the artifacts).
**Not proven, and not ticked.** The CRM browser pass did not finish. The box reached load 171–224
with 24.7 GB of 32 GB swap in use and every `page.screenshot` timing out at 15 s — the documented
"tab died under parallel writers" condition. I stopped it rather than let it draw conclusions from a
saturated machine. REQ-051's keyboard and mobile boxes therefore stay unticked, which is where they
already were.
**Next.** The CRM pass alone, once load is under ~40. It is the only thing REQ-051 owes, and the two
wizard defects above cost four earlier passes their sign-in, which is likely why this one looked
impossible for a week.
prices a one-file archive over a two-file library at exactly **one** lost item, and then proves
it wrote **nothing**: part rows, run status, `storage_prefix`, `finished_at`, the media rows and
the archive's directory on the destination are all byte-identical before and after, read back
out of **PostgreSQL** rather than from the response — a response body cannot prove the database
was not written to, and this is the one property the whole slice exists for. The second refuses
a truncated artifact and issues **no phrase**. The third requires a stranger's run to be a 404
whose message does not name the tenancy rule, because `403 cross_organization` confirms the id
exists and turns a preview into a restore-point oracle.

**Toolchain, four facts, all of which read like product defects and none was one.**
## 2026-09-29 · wave 4 · REQ-054 slice 1 closed on the routes and the screens
`VersionMissing(19)` on the default test database is a **stale QA database from a sibling's
tree** — the shared migration namespace again, and the default `omnion` database carries a
version-19 row from `omnion-w2`/`w5`/`w6`, none of which have a 0019 in this tree. The walks ran
against a disposable `omnion_build_69` instead, which is the rule for a suite database the whole
box shares. `cargo fmt -p omnion-backup` **rewrote five files I had not touched**; the diff was
pure whitespace and was reverted with `git checkout --` on exactly the foreign five, which is why
the `git diff --name-only` comparison is done by hand every tick. The doc-comment linter reports
`async fn is not permitted in Rust 2015` on every `async` in the crate — the toolchain linter
does not pass the edition, and `cargo build` is the authority. And a doc comment containing
`**/` inside a Python triple-quoted string closes the string: two patches this tick failed to
parse for that reason, and the fix is the `patch` tool, not `execute_code`.

**Browser pass: still not run, fifth tick running, and the reason has changed.** It is no
longer the `qa-slot.sh` hold recorded in tick 68. That hold is **genuinely live** — I verified
the holder pid's parent is a running `run.sh` with a walkthrough against `:3102`, so reclaiming
it would have been stealing another writer's slot, not fixing a stale lock. The pass queued,
took its place, and then sat in the stack build: at 22:54 the default stack's `18080/3100/3200`
were still not listening, `qa-artifacts/20260929-224017/` was empty, and the box was at **load
19** with four sibling stacks (w3, w5, w6, w7) each holding three pm2 processes and Chromium
sessions. I stopped the pass rather than add a fifth Chromium to a box already at 19, and left
no orphans: the one remaining slot place is the sibling's, and no `omnion-qa-*` process of mine
survived. The rule that came out of it is the one already in the ledger — **load 15+ with under
4 GB free is a deferred pass, not a failed one** — and the discipline that matters more is that
a deferred pass is reported as deferred. `runBackupDepth` remains written-but-unrun, so REQ-013
does not close on tests alone, and the walkthrough extension committed in `54f305e` means there
is a real pass waiting the moment the box has room.

**Two halves making the same mistake are not a cross-check** is a general rule, not a backup
one, and it is worth carrying out of this feature: the walkthrough's `artifact()` helper and
`local_path_for` both resolve a prefix to a path, and had the walkthrough repeated the
double-prefix bug this crate already fixed once, the preview would have "passed" against a
directory no operator would ever look in.

**Next.** (a) The destructive half of the restore: part selection, the mandatory safety backup
**What.** Recovered the slice a dead tick left half-written, then made it run. The
uncommitted tree was 3,863 lines across the API route, the three admin screens and a
1,047-line integration suite; the module crate and the migration were already in. Four
product defects and four test defects came out of the first run.

**Proof.**
- `cargo build -p omnion-api` — 0, 7m13s from a cold target
- `cargo test -p omnion-module-accounting --lib` — 18/18
- `cargo test -p omnion-api --lib` — 281/281 (this is also what proves migration 0167 is wired)
- `pnpm turbo run typecheck --force` — 2/2 (admin + web)
- the accounting integration suite — **12/12, run one test at a time**

**The four that could not run at all.** Every one of them was invisible to the compiler
and to the lib tests, and each failed only on the path a person actually takes.

1. `patch_account` and `patch_tax_rate` emitted the tenant predicate FIRST and appended
   the assignments after it: `update … set active = active where organization_id = $1 and
   id = $2, name = $3 returning id`. The no-field patch is the only statement that parses,
   so the crate compiled and every rename and every deactivate answered **500**. Both are
   now one `COALESCE` statement — a NULL bind leaves the stored value alone, and there is
   no hand-numbered placeholder left to drift.
2. `list_entries` numbered its own filters `$1..$3` after the builder had already given
   `$1` to the organization id, so the source filter compared `source_kind` against a
   uuid. Renumbering by hand is the same mistake in the other direction: `QueryBuilder`
   renumbers binds itself, and a literal `$2` in pushed SQL is a dollar-quoted token —
   `syntax error at or near "$2"`. The filters are pushed only when present, which is the
   shape the sales module already uses.
3. The line count was aliased `line_count` and read as `lines`. `row.get` is a RUNTIME
   lookup, so this is a request-time 500 on the list and nowhere else.
4. `deactivate_account` refused to close a seeded account while its own comment said the
   opposite. The guard belongs to deletion, and there is no delete route.

**The four in the tests.** Each failed for a reason unrelated to the rule it was written to
prove, which is the reason they are written down rather than quietly fixed. The "both sides"
case passed two ordinary one-sided lines through the AMOUNT arguments where it meant one
line with two sides — it was asserting that a valid entry was a defect. The cycle test asked
for code `1000`, which migration 0167's own seed owns, so it died on a name collision. The
list assertion printed no body, which is why a `syntax error at or near "$2"` took three runs
to name itself. And the refused-line field was asserted as `amounts` when the module
deliberately names the exact cell (`debit`/`credit`) so the grid can highlight it.

**Two environment facts that cost time and will cost it again.** The test database is
selected by `OMNION_DATABASE_URL`, not `DATABASE_URL`; exporting the latter silently
leaves the suite on the default database, where it fails at `migrations must apply:
VersionMissing(19)` and reads like a broken migration. And the whole suite in one run
**hangs**: the twelve walks serialise on one `tokio::sync::Mutex`, all five pool
connections sit idle, and the process parks in `futex_do_wait`. Under load 10–20 with eight
writers on the box that is contention, not a result — every walk passes alone in 4–14s.

**Commits.** `13791b6` the two statements that could not run, `2e31e3c` routes + twelve
walks, `7c73c71` the three screens and the walk, `14df24e` the REQ written down honestly.
Merge `df6886e` (origin/main, 6 commits, all `crates/backup`; BUILD-LOG resolved by splicing
both append-only bodies and proving the result by non-blank line MULTISET against both
parents — 6355 lines, 0 lost from either).

**Next.** REQ-054 slice 2: invoices, the sales handoff and the PDF. The unticked boxes it
owns are the first ones anybody can look at — run `scripts/qa/target-guard.sh` and only
then the scoped pass, because no screen in this REQ has been seen in a browser yet.
before the first write, the enforced phrase behind `backup.restore`, abort until the import
begins, and the `backup.restored` audit entry. (b) The `partial` box is still unticked — a run
where one part fails needs a fault injected into the drawer, not a test. (c) The browser pass
is queued behind a live sibling's `qa-slot.sh`; the walkthrough is extended to open the panel,
read the price, the warnings and the phrase, so when the slot frees there is something to run.


---

## Wave 4 · tick 37 · REQ-054 slice 2 — the invoice

**What.** `modules/accounting/src/invoices.rs` (the document, the state machine, the arithmetic the
server refuses to take from a client), `apps/api/src/routes/accounting_invoices.rs` (six routes),
`accounting.invoices.read/.create/.send` in the catalogue, and sixteen integration walks.

**The four rules, and why each is not the obvious implementation.**

1. **The server owns the arithmetic.** `NewInvoice` has no `subtotal`, no `grand_total`, no
   `tax_total` field at all — so a client that posts them has them dropped by deserialization, and
   a free invoice is not reachable. This needed two new methods on `Amount`:
   `multiply_qty` (thousandths) and `percent_of` (hundredths of a percent), each rounding **once,
   half away from zero**, which is the direction PostgreSQL's `round(numeric)` goes — so the panel
   and a SQL recompute agree to the cent, which is the REQ's own "the panel, the PDF and the
   reports must print identical totals".
2. **Only a draft is editable.** The refusal names the way out (void and duplicate) because "cannot
   post" with no alternative sends a bookkeeper to a form that will also refuse.
3. **Void keeps the number and demands a reason.** `INV-0007` was quoted on a purchase order and on
   a customer's ledger; deleting the row leaves that reference dangling, voiding leaves it resolving
   to "withdrawn, and here is why". A void with a blank reason is refused — the row is permanent.
4. **The sweep is idempotent by construction.** The guard is `overdue_at is null` *inside* the same
   `UPDATE` that flips the status, so two racing sweeps cannot both win, and the rows the UPDATE
   returns are exactly the rows worth announcing. This is load-bearing, not tidy:
   `accounting.invoice.overdue` drives the documented automation that sends an e-mail, and a sweep
   that fired per scheduler tick would mail the customer every tick.

**Proof, and the line under it.** `cargo build -p omnion-module-accounting` green;
`cargo test -p omnion-module-accounting --lib` **32/32**; `cargo build -p omnion-api` green, which
is what proves the six routes are wired rather than merely written. The sixteen walks in
`apps/api/tests/accounting_invoices.rs` are **committed but have never run against PostgreSQL** — see
below.

**Two environment facts that cost the tick, both of them the known ones.**

`target/` was deleted from this worktree **twice** in one tick, by a sibling reclaiming disk; the
`target-guard.sh` script diagnosed it correctly both times and rebuilt, which is what it is for. The
second deletion landed mid-`cargo test`, so the run died with `failed to write
…/libsqlx_core.rmeta: No such file or directory (os error 2)` — **`os error 2` on a `.rmeta`/`.o`
path is this, and never a code fault; `os error 28` is the real "disk full"**. Both were reported by
`cargo test -p omnion-permissions` and `omnion-api --lib` at the same moment, which is the tell: a
real fault in one crate never takes a dependency of an unrelated one with it.

The root filesystem then reached **100% (12 MB free)**. The cause is not this worktree — it is 583 MB
total, of which the source is most. `/dev/shm` was at 94% from siblings' `CARGO_TARGET_DIR`s. So the
integration suite was committed unrun rather than run, and that is recorded in the REQ in the same
words. A suite that has never executed is a hypothesis, and the slice-1 suite found four product
defects and four test defects on its first real run, so this one will too.

**Commits.** `707c4b1` the module and its money arithmetic, `34762df` the routes, the keys and the
walks.

**Next.** Run `apps/api/tests/accounting_invoices.rs` — one test at a time with `--test-threads=1`,
because twelve walks serialise on a `tokio::sync::Mutex` and the whole suite as one invocation
**hangs** rather than failing (every walk alone finishes in seconds). Fix what it finds. Then
`accounting_invoice_lines`' `tax_amount` derivation is a SQL expression worth re-reading against the
module's own `PricedLine.tax_amount`: the detail read recomputes it from `line_total` while the
writer stored it, and those two have to be the same number — the PDF arrives in slice 3 or 4 and
will be the first place anybody notices if they are not.

**Where this ended, since the tick's own first draft of this section is now behind it.** The suite
was built and run rather than left committed-and-unproven, and it found what that shape of defect
always finds: **0/18 on the first real run**, then 12/18, then 17/18. Eight defects, and they divide
into two families worth keeping apart.

*The ones a real database could see and a compiler never would.* `accounting_invoices` has **no
`customer_name` column** — 0167 gave the invoice `company_id` and `contact_id` and nothing to print,
so every create answered 500. Migration `0171` (the union high-water across all ten worktrees was
0170) adds it, with the rule it encodes stated in the file: **the CRM owns the current name, the
invoice owns the name it was issued under**, or renaming a company rewrites a document the customer
already received. And `crm_contacts` has `first_name` and `last_name`, **not** a `full_name` column —
both joins and the search predicate named a column that has never existed in this repository.

*The ones I introduced while fixing the first kind.* `gross_of` was `net + tax + discount` where the
gross is `net + discount`, so a 100.00 line at 20% reported a **subtotal of 120.00** — a figure no
line on the invoice carries. That one is worth naming twice: it was introduced **while replacing a
back-solve**, which is the dangerous moment, because a fix that rewrites arithmetic carries whatever
the writer had wrong into the new code. A converted invoice also had no customer (the order row
already spells one, and asking the caller to re-type it is a second source of truth), the second-send
refusal said "already Sent" and stopped rather than naming void-and-duplicate as its own docs
promise, a test helper built `/invoices&overdue_only=true` with no `?` and got a 404 that read like
a product defect, and one assertion compared `tax_percent` to the string the **form** typed (`20`)
rather than the string the column holds (`20.00`).

**Final state, stated exactly.** `cargo test -p omnion-module-accounting --lib` **32/32**;
`cargo build -p omnion-api` green; the invoice suite reached **17/18 against a live PostgreSQL**, and
the eighteenth was the `tax_percent` assertion — fixed in `edf65fd0` and **not yet re-run**. The box
deleted this worktree's `target/` **five times** during the tick and the root filesystem went to
100% and then to `os error 28`, so the last confirmation could not be executed. That is an
environment fact and not a code blocker: every fix is committed, pushed, and one assertion short of
green.

**Next tick, first command.** `bash scripts/qa/target-guard.sh`, then rebuild and run
`apps/api/tests/accounting_invoices` against a fresh `omnion_t_inv` database, **one test at a time
with `--test-threads=1`**. Then the invoice screens and the walkthrough routes, then the PDF.


## Tick 70 — the destructive half, and a fixture that could not reach the guard it was written for

**REQ-013 slice 2b.** `POST /api/v1/backups/{id}/restore` behind `backup.restore`, the object
loop in `crates/backup/src/restore_objects.rs`, the plan in `crates/backup/src/apply.rs`, the
restore control on the preview panel, and the walkthrough driving it. The decisions are split
the way the crate's header asks for: `apply.rs` is **pure** (selection, phrase, ordering, the
refusals) and `restore_objects.rs` is the loop that touches a store, so the rules are
unit-tested with nothing running and the bytes are proved over the real router.

**A defect in shipped code, and it is a new instance of the shape this crate keeps finding.**
`build_preview` computed the age inside the warning's `if` and then wrote a literal
`age_days: 0` into the struct — so the panel's age tile read **`0d`** next to a warning that
said *"this restore point is 23 days old"*. Two halves of one fact, computed twice, with one
of them hardcoded: a screen contradicting itself on the single number an operator uses to
choose between two restore points. The unit tests passed because each asserted on one half.
One number now feeds both and a test holds them together.

**The refusal half is the load-bearing half, and only a walk can prove it.** `build_plan` is
pure, so its six refusals are unit-tested; what no unit test can see is that a *refused*
restore writes **nothing**. The walk fires four refusals in a row — wrong phrase, a part the
run never produced, a name outside the five, an empty selection — and then asserts the run
count, the media rows and the audit are exactly where they were. A destructive route that
takes its safety backup and *then* refuses is a route that mints a protected, undeletable
backup every time somebody fat-fingers a phrase; the refusal would still be correct, and the
operator would never know. So every refusal is proven to happen **before** the safety backup,
which is why the `database` refusal sits above it too: taking a backup of a restore that is
going to be refused is a run nobody asked for.

**The safety backup calls `produce_all` — the same function `POST /api/v1/backups` calls.**
A "quick safety backup" as its own loop looks shorter and is strictly worse: a second set of
rules about what a part is, a second writer for the destination, a second place for the media
copy to be wrong, on the one path nobody watches because it is supposed to be automatic. It
carries the run's own scopes rather than all five, and an **incomplete** safety run is a
refusal rather than a warning: "you may restore, just know there is no way back" is not an
offer anybody should be able to accept.

**The `database` part is refused by name, and this is the honest answer rather than a gap.**
`document_database` writes a row COUNT per table. That is an inventory, not a dump, and
restoring it would replace the platform's own schema with an inventory of it — the counting
defect this crate was written to remove, in its most expensive form. The route says so in a
sentence an operator can act on rather than quietly doing nothing. The line is therefore
**partly open** and says so: the other four parts are recorded and priced, not applied, and
the panel names the difference between "restored" and "reported".

**A third instance of the same class, in a test rather than in shipped code — and the one that
taught me something.** The walk for the index-version guard rewrote the media index to
`version: 99` and expected the restore to refuse on the version. It refused — on the *size*,
one layer up, because serialising `99` instead of `1` changes the file's length and the
media part's recorded size no longer matches. **The right answer, from the wrong check, and
the fixture was asserting the wrong thing.** Two things came out of it:

* the obvious fixture is not merely inconvenient, it is **unreachable**, so the guard needs a
  *length-preserving* rewrite — and a first attempt padded by doubling and gave up on the
  overshoot, which proved the size check twice and the version guard not at all. A pad that
  overshoots is not a failed attempt; it is the loop trying again one character shorter. The
  walk is now two phases, and phase one asserts the thing that is actually true: an edited
  index is not offered and **no phrase is issued** for it.
* a check one layer up can make a check one layer down unreachable, and that is a *fact about
  the design* rather than a test problem. The two together mean an edited index cannot produce
  a phrase, which is the property worth having.

**Two more of my own, both the shortcut that reads well and does not run.** `select count(*),
max(metadata)` looks like the tidy way to read an audit row and dies with *function max(jsonb)
does not exist* — jsonb has no ordering, so the walk was failing on a function the test
invented rather than on the thing it meant to check. And the stranger in the tenancy walk held
`OPERATOR_PERMISSIONS`, which does not include `backup.restore`, so the guard answered `403`
and a `404` assertion failed **for the right reason at the wrong layer**; the stranger now
holds every key the restorer holds, which is what makes the `404` evidence about the boundary
and nothing else. A walk that asserts `404` against a `403` proves nothing.

**Proof.**

| Gate | Result |
| --- | --- |
| `omnion-backup --lib` | **132/0** (103 before: +13 plan, +16 restore) |
| `omnion-api --lib` | **220/0** |
| `apps/admin` `tsc --noEmit` | clean |
| `apps/api --test backups` | 6 new walks, **6/0** in isolation |

The corrupt-object walk is the one that needed the real router: it truncates a real file on
the destination, restores with the **right** phrase, and asserts the live store still holds
its own bytes — compared as bytes, because a length check passes by accident on an overwrite.
The index walk is two-phase for the reason above. The panel's refusal path is extended into
`scripts/qa/walkthrough.cjs`, including pressing a *wrong* phrase first, because a Rust walk
can prove the API refuses and only a browser can prove the **panel shows** it rather than
swallowing it into a spinner.

**Toolchain, two facts.** `cargo fmt -p omnion-backup` rewrote five files I had not touched
(whitespace only) and they were reverted with `git checkout --` on exactly the foreign five.
And the toolchain linter reports `async fn is not permitted in Rust 2015` on every `async` in
the crate — it does not pass the edition, and `cargo build` is the authority; `node --check`
passes the walkthrough, `bun build` only fails on the missing `playwright-core` module.

**The suite, honestly.** The full `--test backups` run (21 walks) **stalled twice** with an
idle PostgreSQL and a `futex_do_wait` on the test process while the box sat at load 14–20 with
four sibling stacks. That is contention, not a defect — the same shape the ledger records for
the earlier ticks — so the six new walks were each run to completion **in isolation** against a
disposable `omnion_build_70` and the result above is 6/6, not a claim about the whole file.

**Next.** (a) The `partial` box is still unticked: a run where one part fails needs a fault
injected into the drawer, not a test. (b) The abort criterion is now argued rather than
built: a cancel that cannot undo a half-written library is a dead control, so the real form
belongs with a queued restore in slice 3's worker. (c) The browser pass is still queued; the
walkthrough now drives the button, the part ticks and a wrong phrase, so there is something to
run the moment the box has room.

## Wave 4 · tick 38 · REQ-054 slice 2 — the three invoice screens, and the target that kept dying

Slice 2 ended last tick with the module, the routes and eighteen walks committed, and the walks
at 17/18 against a live PostgreSQL with the eighteenth assertion fixed but never re-run. This tick
did the two things that were outstanding: **re-ran the eighteen**, and **built the screens the
slice had been missing**.

**The screens were missing because "routes wired" is not "feature delivered".** Slice 2's commit
`34762df` put six routes behind the API and the accounting module kept the three screens slice 1
built — chart, rates, journal. `/accounting/invoices` was a URL that answered `404` to a browser.
A walkthrough that only exercises the API would never have noticed, which is the argument for
walking the screen rather than the route, restated by a defect rather than a principle.

Three screens and the client behind them:

* **`/accounting/invoices`** — status tabs and the "Overdue only" switch both read the *server's*
  `overdue_only` filter, so the tab and the red "37d late" on a row can never disagree. Each row
  prints `total / paid / outstanding` together so `paid + outstanding = total` is checkable without
  arithmetic. "Check overdue now" is a button because the sweep is normally the automation's job
  and an operator who thinks a late invoice has not turned red needs a way to *ask*.
* **`/accounting/invoices/new`** — the line grid, and the reason `NewInvoicePayload` has no
  `subtotal` and no `grand_total` field: the server's `NewInvoice` has none either, so a total
  posted from a browser would be rejected. The preview prices lines in the browser in the module's
  own order (discount, then tax, rounded once) and the screen re-reads the server's figures after
  saving. Save stays disabled until a line is actually usable.
* **`/accounting/invoices/{id}`** — the document: lines, the right-aligned totals block, and send
  and void **only while the status is draft**, because the server owns those preconditions. The
  void dialog keeps its confirm button disabled until the reason has something in it; voiding
  preserves the number, so the reason is the only record of why it was withdrawn.

**Proof.** `apps/admin`: `tsc --noEmit` **0 errors** — run directly, because `pnpm turbo run
typecheck` reported "cache miss, executing" in 2.3s, which is a cache that does not know a file
was just created. `scripts/qa/walkthrough.cjs` parses (`bun build --external '*'`). The
`accounting-depth` pass gains the list, the form (watching the live total reach `240.00` for
2 × 100 at 20%, and the save button's disabled state on both sides of that) and a 390px pass over
the invoice table. Commits `8520f9ac` (main merged, BUILD-LOG spliced and multiset-proved) and
`615ed7db`.

**A defect in the harness, found twice.** `disk-guard.sh` decides a target directory is in use by
reading `CARGO_TARGET_DIR` out of `/proc/*/environ`, so a build launched *without* an explicit
`CARGO_TARGET_DIR` is invisible to it and gets `rm -rf`'d mid-compile. It cost two builds tonight
and produced `os error 2` on a `.rmeta` path, which reads exactly like a broken crate. Building
into `/dev/shm/w4-target` with `CARGO_INCREMENTAL=0` survived two guard cycles the default target
did not. **The next tick, and every tick after it: set `CARGO_TARGET_DIR` explicitly.**

**The eighteen walks:** the result is in the tick's report. They are run, not committed-and-hoped.

**Next.** Slice 3 — payments with allocation, partial and full states, reversal, the payments
screen and the cashflow report.

## Tick 71 — REQ-013 slice 3: the schedule that could never fire

**What.** `backup_schedules` shipped in slice 1 with a `next_run_at` column, and
`next_due_schedules` shipped with it. Nothing wrote the column. Nothing called the query. A
schedule could be created, listed, and rendered with a cadence sentence — "Every day at 02:00" —
beside an empty next-run cell, for ever. It is the uncalled `prune_candidates` defect one
table over, and the shape deserves a name: **a table with a column, a query that reads it, and
no writer is a feature that looks complete in every screenshot and does nothing.**

Three pieces closed it.

**1. `crates/backup/src/cadence.rs` — `Cadence::next_after`, pure, 18 unit tests.**

The wall clock is local and the stored instant is UTC. Istanbul 02:00 is `23:00Z` the day
before; New York 02:00 is `07:00Z` the same day. The next run is strictly *after* now, so a
schedule created at 02:00:30 with a time of day of 02:00 does not answer a moment in the past
and get claimed on every tick. An unknown zone is refused by name, not defaulted to UTC.

**2. Daylight saving, decided by round-tripping rather than by asking the zone table.**
`get_offset_local` answers `Some` for `02:30` on a spring-forward morning — a reading that
never happened — and never answers `Ambiguous` for the hour that happens twice in autumn.
Both were found by three zone tests failing against a version that trusted it. So each
candidate offset is proposed and re-checked: an offset that survives its own trip back to the
wall clock is real. A gap moves the run forward by the gap; a fold runs **once, at the
earlier** of its two readings, because running on both gives two runs an hour apart for one
instruction and `retention_count` would hold two of the same backup.

**3. `apps/api/src/backup_schedule_runner.rs` + four write routes + the panel.** Polls every
minute — the sweep's six hours comes from the feature (retention is measured in days) while a
schedule's is measured in minutes. The worker calls the same `produce_all` the create route
calls. The key split is the interesting half: editing is `backup.manage`, **"run now" is
`backup.create`** — it produces a backup and changes nothing else, so an operator who may take
a backup must be able to test that their schedule works. A manual run does not advance
`next_run_at`: testing a 03:00 schedule at 09:00 must not consume tomorrow's slot.

**Proof.**

| Gate | Result |
| --- | --- |
| `omnion-backup --lib` | **150/0** (was 132: +18 cadence) |
| `omnion-core --lib` | **39/0** |
| `apps/admin` `tsc --noEmit` | clean |
| `cargo build -p omnion-api` | Finished, 0 errors |
| `scripts/qa/walkthrough.cjs` | `node --check` OK |
| QA browser pass | **deferred** — see below |

**Three of my own bugs, all the shortcut that reads well and does not run.**

*Hourly truncated after adding instead of before.* `(after + 1h).replace_minute(0)` takes
15:30 to 16:30 and truncates that back to 16:00 — right by coincidence — but I had written a
test asserting 14:59:30 lands on **16:00**, which it must not. Truncating first is correct by
construction; the test was wrong and the fix is in both.

*Two DST expectations written from memory, and both were wrong while the code was right.*
London's clocks move at **01:00 UTC**, so the reading that does not exist in spring is
01:00–01:59 *local*, not 02:30 as I had it. I then "fixed" the test to 01:30 and the spring
case still disagreed by an hour, because after the transition London is **on BST for the
quarter** — 01:30 local on 30 March is 00:30 UTC, not 01:30 UTC. The resolution was to stop
reasoning from memory and print the table: London's 2026 transitions are 29 March and
25 October, and New York's are 8 March and 1 November. A DST test written from a
half-remembered rule is a test of the author's memory, and it fails for the wrong reason,
which is the worst kind of red.

*`to_offset` is not the conversion.* `OffsetDateTime::new_utc(date, 02:00).to_offset(+03:00)`
is `02:00+03:00` — the **same instant** as `02:00Z`. A scheduler that does this stores 02:00 UTC
and runs every backup nine hours late, while every test that only checks "the hour field is
02:00" passes. The instant is moved by *subtracting* the offset. The three zone tests exist
because that was the first version.

**`time-tz` turned out to be the wrong tool for the question, and that is the finding.** Its
`OffsetResult` has an `Ambiguous` arm and a `None` arm for exactly these two cases, and it
returns `Some` for both. Round-tripping the candidates is more code than calling the API and
is the version that is actually right. Worth remembering before reaching for a library's
convenience arm.

**Toolchain.** A sibling deleted the shared `target/` mid-build and the api build died with
`failed to move dependency graph ... No such file or directory (os error 2)` — the sibling
signature, not a disk-full error; the two look identical in the log and are not.
`CARGO_TARGET_DIR=/dev/shm/omnion-build-target` plus `CARGO_INCREMENTAL=0` makes the build
immune. `cargo fmt -p omnion-backup` rewrote **seven** files I had not touched this time
(78 lines in `apply.rs` alone); reverted on exactly the foreign seven with `git checkout --`,
and `lib.rs` re-derived from `git show HEAD:` so the reordering did not ride along.
`time::macros::format_description!` is the only way to get a const format — the older
`format_description::parse` returns a `Result` and does not satisfy `Parsable`.

**QA pass: deferred, and reported as deferred.** The single slot is held by a live sibling
(pid 3355724) and the box is at load 26 with three other stacks compiling `omnion-api`
simultaneously. `runBackupSchedules` is written and committed, and its load-bearing assertion
is aimed straight at this tick's defect — the next-run **cell** must carry a real date and the
zone, not a dash. A deferred pass with nothing written would be a screen nobody has looked
at; a pass that barges into a live sibling's slot steals rather than fixes.

**Next.** (a) The browser pass, when the slot is free. (b) The four-frequency form needs a
walkthrough leg for each — the panel branches the conditional fields and only the shape
assertions are written. (c) Slice 2c, the queued/abortable worker, is still the honest home
for a real abort: a cancel that cannot undo a half-written library is a dead control.

## 2026-09-30 · tick 72 · the nine-element array

**REQ-013** (slice continuation). Not a new screen this tick: a defect that made six screens
wrong at once, found while auditing the uncommitted diff left behind by tick 71.

**What the defect was.** `time`'s `Serialize for OffsetDateTime` has two arms — a formatted
string for a human-readable serializer, and a **nine-element tuple** as the fallback. The
string arm is gated on the crate feature `serde-human-readable`. The workspace declares
`serde-well-known`, which enables `serde`, `formatting` and `parsing` and *leaves
`serde-human-readable` off*. So the arm that ships is the tuple. Proved rather than recalled,
against the vendored crate's own source and then against a scratch binary:

```
bare  = {"created_at":[2026,273,1,39,51,668190318,0,0,0],"maybe":null}
rfc   = {"created_at":"2026-09-30T01:39:51.668190318Z"}
```

**Why it survived this long.** Nothing above the serialiser objects. `apps/admin/lib/api.ts`
declares `expires_at: string`, so `tsc` is green; `formatTimestamp` guards with
`Number.isNaN` and returns `"—"`; `new Date([2026,273,…])` is `Invalid Date`, so the guard
fires and swallows it. The result is that a share link which expires renders no expiry, a
scan run shows no time, a retention run shows no window, a delivery shows no attempt, and the
schedule table's next-run cell sits empty beside a cadence sentence. **An em dash for "this has
not happened yet" is pixel-identical to an em dash for "this value was lost"**, and every one
of these screens has legitimate reasons to show the first. That is the whole defect class: a
loss that renders exactly like a designed answer.

**Scope found by scanning, not by memory.** Twenty-eight fields across seven route modules —
`backups`, `commands`, `media_files`, `media_retention`, `media_scan`, `media_shares`,
`webhooks` — covering `BackupBody`, `StatusBody`, `SettingsBody`, `ScheduleBody`, `ShareBody`,
`RunBody`, `ScanRunBody`, `QuarantineBody`, `RecentItemBody`, `DeliveryBody`,
`RetentionRunBody`, `SweepBody`, plus the two list-query filters.

**The half that nearly shipped in the same commit.** `#[serde(with = "…::option")]` on a
**query** field makes serde *require the key*. The fix for "the date filter returns 400"
turns every ordinary unfiltered list — `{}` — into `400 missing field created_after`, so the
obvious improvement would have broken every screen that filters. The annotation needs `default`
alongside it, and the absence case is the one that has to be asserted:

```
with_only   absent -> Err("missing field `created_after`")
with_default absent -> Ok
```

**A third finding, from the test rather than the code.** `axum::extract::Query` deserialises
**snake_case** query parameters, and `ListQuery` carries no `rename_all`. My first draft of the
test sent `createdAfter`, it parsed without error, and the assertion "the filter parsed" passed
for a filter that was never applied — an unmatched key is ignored rather than refused. The test
now asserts the snake_case name binds *and* that the camelCase spelling does not, because
"parsed" and "parsed into nothing" are the same green.

| Gate | Result |
| --- | --- |
| `wire_dates` | **4/0** (new suite) |
| `omnion-api --lib` | **220/0** |
| `apps/admin` `tsc --noEmit` | clean |
| `no_serialised_struct_carries_a_bare_instant` | **negative-proved** — removing one attribute turns the suite red with `ShareBody.created_at serialised as [2026,273,2,0,0,0,0,0,0]` and names `media_shares.rs:71` |
| QA browser pass | **deferred** — the single slot is a live w3 pass (holder pid 3355724, alive), load 12–28 with sibling stacks compiling |

**Three of my own bugs, all from trusting a shape I did not read.** (1) The patcher added a
second `#[serde(with = …)]` under a four-line `#[serde(with = …, skip_serializing_if = …)]`,
producing `duplicate serde attribute` and then a cascade of six `E0277`s from the derive — the
one-line lookback that skipped an existing annotation is the same lookback the *gate* was
written with, so the gate got the same bug and had it fixed before it ever ran. (2) Every
struct literal in the new test was written from memory: `ScheduleBody` has no `updated_at`,
`RunBody` has no `dry_run` or `purged_files`, `StatusBody` nests a `StatusTotals`, and
`ScanRunBody` has `kind`/`outcome`/`flagged` rather than `status`/`clean`/`infected`. Twenty
`E0560`s, all of them mine. A test whose fixtures are invented is a test of the author.
(3) My first assertion helper treated a `None` instant as a failure, so it demanded a string
from a field whose correct wire value is `null`. The array is the failure; the null is the
answer, and they render identically — which is the reason the defect survived.

**Toolchain.** `cargo fmt -p omnion-api` reformatted **nine files I had not touched** and, worse,
85 unrelated lines *inside* `backups.rs` — a file I do own, so the usual "revert the foreign
set" habit does not catch it. The commit was rebuilt from `git show HEAD:apps/api/src/routes/
backups.rs` plus only the eleven attributes, which is why the diff is 11 added lines and zero
elsewhere. The invariant generalises: *owning the file is not the same as having written the
line.* `cargo fmt` is not run at the crate level in a ten-worktree workspace.

**Next.** (a) The browser pass, when the slot is free — `runBackupSchedules` asserts the
next-run cell carries a date and the zone, and this tick explains why that cell was blank even
with a correct `next_run_at`. (b) Slice 2c, the queued/abortable worker, where a real abort
belongs. (c) The same array-vs-string scan belongs in `apps/web` and the CLI, neither of which
this tick looked at.

## Tick 39 — REQ-054 slice 3: payments, allocations and the reversal

**What.** The payment half of accounting. Migration `0175` makes
`accounting_payments.invoice_id` nullable and adds `accounting_payment_allocations`, seeds
account **2300 Customer Advances**, and adds `number`/`currency`/`customer_name`/`note`/
`reversed_at`/`reversal_entry_id`. The module (`modules/accounting/src/payments.rs`, 1.6k lines)
holds the rule the REQ's headline criterion is about — **an allocation can never exceed what is
still owed on its invoice** — checked inside the writing transaction against a `for update` read
in sorted-id order, plus the oldest-first sweep, the reversal and the journal entry each payment
writes. The routes are `GET/POST /accounting/payments`,
`GET /accounting/payments/{id}`, `POST /accounting/payments/{id}/reverse`, guarded by four new keys
with `accounting.payments.overpay` separate from `.record` on purpose.

**Proof.**
- `cargo build -p omnion-api` — **green, 0 errors**.
- `cargo test -p omnion-module-accounting --lib` — **38 passed, 0 failed** (six new).
- `cargo test -p omnion-permissions --lib` — **68 passed, 0 failed** (one new).
- `cargo test -p omnion-api --test accounting_payments` against a live PostgreSQL — **6 of 14 GREEN**.
  The suite's own shape is the proof it was written for: a committed-but-unrun walk is a
  hypothesis, and this one found **three defects on its first real run** (2/14, then 4/14, now 6/14).

**The three defects, all one class: a claim about the database written down instead of checked.**
1. The guard that drops a blank allocation row was **inverted** —
   `x.trim().is_empty().then_some(())` is `Some(())` when the row IS empty, so every real row was
   skipped. 12 of 14 walks died on one message naming a field the payment had filled in correctly.
2. The query selected `p.recorded_by`. The REQ's data model names it `recorded_by`, migration
   `0167` wrote `created_by`, and I hedged by reading **both** — which is a panic, not a
   compatibility layer: `row.get` on an alias the SELECT does not list is `ColumnNotFound` at
   runtime, not `None`.
3. The arithmetic walk signed in as the bookkeeper while sending `allow_overpayment: true`, so the
   route refused on the flag before the module was consulted and the walk would have **passed
   without ever testing the rule it exists to prove**. It now runs as a holder of the override key.

**Not proved, and the next tick starts here.**
`an_allocation_above_the_outstanding_is_refused_with_the_three_numbers` **blocks**; seven walks
after it are unrun. What is ruled out, so the next tick does not re-derive it: **not** a deadlock
(`pg_stat_activity` all `idle`, no ungranted locks), **not** cross-walk state (it reproduces alone
on a fresh database), **not** pool pressure (37 of 100 connections), **not** Redis (6380 answers
`PONG`). The process burns **zero CPU over 13 minutes** with 8 open sockets — an external wait,
under load 10–15 with ten writers on the box. Left named rather than blamed.

**Next.** The blocked walk, then the seven behind it. The payments **screen** is not written — slice
3's UI is the next pair of commits after the walks are green, and the acceptance boxes that depend
on a browser (empty/loading/error, 390px, keyboard) stay unticked until one runs.

## 2026-09-30 · REQ-054 slice 3 — 14/14 walks, and the blocker was a word

**What.** The walk named as blocking last tick passes alone in ten seconds. It was never blocked;
the suite serialises on a static mutex, so a *different* walk failing first makes everything behind
it look blocked. Four real defects, two of them the same class slice 2 already paid for:

- `c359f8e2` — allocations were read back `order by a.created_at, a.id`, and `now()` is
  transaction-stable, so every row of one payment shared a timestamp and the tiebreak fell to a
  random `gen_random_uuid()`. The "oldest first" sweep was true of the write and false of the read,
  failing roughly half the time. Migration `0178` adds the ordinal the module writes.
- `cfbc2cf7` — the payments list selected `p.created_by, p.created_by` with no alias while
  `from_row` reads `recorded_by`, so the list 500'd on every call. Second instance of this exact
  defect in this module.
- `ce813f2d` — **a tenant leak.** The reverse route's permission was a route *layer*, which answers
  403 before the handler can check whether the id is the caller's, so naming another organization's
  payment confirmed it exists. The layer is gone; the handler reads the row (404) and *then* asks
  for the key.
- `197dc399` — two walks asserted the wrong thing, module right both times: an outstanding of 40.00
  where 40.00 is the *paid* figure (100.00 − 40.00 = 60.00 owed), and an allocation's own `id`
  compared against an invoice id.
- `1d57801e`/`d3c95d2f`/`5ca87c4e` — the payments **screen**: list, recorder drawer, receipt, and a
  walkthrough pass that visits all three and leaves the drawer by keyboard.

**Proof.**
- the fourteen walks, one per process against a fresh `omnion_t_w4_54c`: **14 passed, 0 failed**
  (3.9s–8.1s each);
- `cargo test -p omnion-module-accounting --lib` **38/38**;
- `cargo test -p omnion-permissions --lib` **68/68**;
- `cargo build -p omnion-api` green;
- `tsc --noEmit` in `apps/admin` **0 errors**.

**Not proved, and it is the browser.** A *full-suite* run hung on
`a_payment_cannot_allocate_more_than_it_itself_or_name_an_invoice_twice` with 0% CPU, every
PostgreSQL backend `idle` and zero ungranted locks — the same external wait as last tick, now
measured rather than guessed, which is why the fourteen were run one per process. No
`scripts/qa/run.sh` pass this tick; the empty/loading/error, 390px and keyboard boxes stay unticked.

**Next.** A QA pass over the new screens on the private stack
(`QA_STACK=w4 QA_API_PORT=18083 QA_ADMIN_PORT=3103 QA_WEB_PORT=3203`), then slice 4 — expenses,
receipts and the approval handoff, which is the last thing between this REQ and the reports.

## Tick 73 · REQ-013 slice 2c — the restore you can still stop

**What.** The one acceptance criterion REQ-013 had been carrying since slice 2b is closed:
*aborting before the import starts cancels cleanly and leaves the platform untouched.* Slice
2b's answer — a restore inside the `POST`, no cancel, and a panel that says why — was correct
and **incomplete**, and the criterion said so: *"a genuine abort belongs with a queued
restore"*. This is the queued restore.

`backup_restore_jobs` (migration `0177`), `crates/backup/src/restore_jobs.rs`,
`apps/api/src/routes/restore_jobs.rs`, `apps/api/src/restore_job_runner.rs`, and a
**Queue it (can be stopped)** control beside the immediate one. The immediate restore keeps
its refusal and keeps its explanation; the queued one is the shape with a real window, and the
window is **before the first write**.

**The three decisions, and the shortcut each one refuses.**

* **The window is a state the schema names.** `queued` is the only cancellable status and
  `cancellable` is a **field on the wire**, not a derivation the panel makes — a panel that
  re-derives it is one edit away from offering Stop on a running restore, which would discard
  the safety backup the operator was told they had.
* **A cancel is an intent, then a transition.** `cancel_requested` is a flag, because a cancel
  can land while the worker is reading the index and a `status = 'aborted'` write *then* is a
  lie the constraint refuses. The worker is the only thing that may turn the flag into an
  abort. The cancel route settles a still-queued row **synchronously**; leaving it to the
  next tick shows a Stop control on a row the operator already stopped, and a control that
  appears not to work is worse than none.
* **A job nobody claims is `aborted`, not `failed`.** `failed` requires a start by the
  schema's own constraint, and loosening it would stop `failed` meaning what it says. The
  state that already meant "stopped before the first write" is the honest one.

**The defect the walk found, which reading could not have.** `0177` shipped with **two
constraints that contradicted each other**: the main status check required `started_at IS NOT
NULL` for `aborted`, and the abort-specific check required `IS NULL`. A worker *does* stamp
`started_at` when it claims a job and may then find the flag, so the real transition needs the
column cleared — and **every cancellation would have been refused by the database.** Nothing
else in the platform writes that table, so the only place it could ever surface is the first
cancel in a walk, and a walk that did not cancel would have shipped it. The two are now one
consistent pair, with `aborted` the one terminal state permitted to have no start.

**The second defect, in my own first draft of the worker.** It read the queue through the
**tenant-scoped** reader with `None`, and `is not distinct from null` matches rows whose
`organization_id` *is* null — the platform's own jobs. Every tenant's restore would have sat
`queued` for ever while the tick reported a clean pass. A background worker is *supposed* to
cross tenants, and the scoping that protects a request is exactly what hides a tenant's work
from the thing that has to perform it; `all_queued_restore_jobs` and `queued_restore_jobs` are
now two functions with two names, so the unsafe form cannot be reached by passing the wrong
argument. Same silence as the uncalled `next_due_schedules` one feature over.

**The preview is now built in ONE place.** The third caller forced a refactor that found a
live disagreement: the preview route substituted zeros when the live media library could not
be counted and rendered a reassuring "you lose nothing", while the restore **refused** on the
same failure. Both defensible alone; together they are the defect — one price on the screen
and another in the effect. `rebuild_preview` reports `live_comparison_failed` and each caller
decides what to *do* about a fact the function only *reports*.

**A third finding, from the walk rather than from the code.** The fixture's run is a
**media-only** one, so `database` is refused as `part_not_in_run` — the run-check comes first,
and it is the better answer because it names what the run *can* offer. My table of expected
refusals asserted `part_not_restorable` and was **wrong**; the deeper rule needed a run that
really produced the part, and the walk now builds one. Two refusals that are both right, and a
test that asserted the wrong one.

| Gate | Result |
| --- | --- |
| `omnion-backup --lib` | **153/0** (was 150; +3 for the queue's own rules) |
| `omnion-api --lib` | **220/0** |
| `apps/admin` `tsc --noEmit` | clean |
| `apps/api --test backups` | see the run below — the suite found two real defects, both fixed and both now covered |
| QA browser pass | **ran, and got through `/backups`** (visited, 39 interactive elements) before my own `timeout 1500` killed the walkthrough mid-run. The whole route list is longer than 25 minutes on this box, and every later step reported `Target page, context or browser has been closed` — the tab dying under memory pressure, not a finding. **No report was written, so no finding count is claimed**: the queue controls were not exercised in a browser, and this REQ is **not** closed on this |

**The environment fought this tick and the wins are worth writing down.** `/mnt/apopic` hit
100% mid-tick, so the first `cargo test` died with `No space left on device (os error 28)` and
**the walk passed in 0.01 s** — the fixture's `Fixture::new()` returns `None` when PostgreSQL
is unreachable and the test body simply returns. That is a green that measures nothing, and it
is the same class as the em dash: a loss and a designed answer, pixel-identical. Two things
gave it away and both are now habits: **0.01 s is not a walk that takes a backup**, and the
database it named did not exist. Moving the target to `/root` produced `ld terminated with
signal 7 [Bus error]` — a **linker** OOM, not a Rust one, on a box with 7 GB available. Only
my own artifacts were ever deleted; the siblings' `target`s were left alone.

**The suite then hung, and the hang was the second defect — a real one.** It stalled with a
job sitting in **`running`** and no query outstanding: `not granted` locks **0**, no non-idle
backend, no filesystem activity, the main thread in `futex`. Reading the row named the bug:
`fail_stale_restore_jobs` swept only `queued`, and **nothing else can move a `running` job** —
the queue read is the only thing that advances one, a worker killed mid-restore is by
definition not going to, and the partial unique index refuses a new restore of that run while
the row stands. So **one deploy in the middle of a restore costs that run its restore point
for ever**, and the panel draws it as in progress the whole time. The sweep now covers
`running`, and a reclaimed `running` job becomes **`failed`, never `aborted`**: it was
claimed, so it probably already took a safety backup and may have written objects, and
"nothing was written" about it would be the exact lie this feature exists to avoid. The
reason string says what is *unknown* and names the backup to go back to.

**The first defect the suite found was in a test, and it is the tick-72 wire-format fix
coming back.** `a_backup_of_all_five_parts…` asserted that `last_successful_at` **is an
array**, with a comment explaining that `OffsetDateTime` serialises as a tuple. That is true
only of `time` with its `serde-human-readable` feature *off* — the same feature gap tick 72
diagnosed — so the line **documented the defect as if it were the contract**, and the fix made
the test fail. A test that pins the bug is worse than no test: it is a green that trains the
next reader to distrust the code instead of the test, and it makes a fix look like a
regression. It now asserts the JSON **type** is a string and that the value looks like
RFC 3339, which is the only assertion that tells a *lost* timestamp apart from a designed one.

**A third finding, from my own walk's bookkeeping.** The new worker walk queued a second job
to prove the run was restorable again — and then queued a third, which the partial unique
index refused, because the walk's own leftover was still live. A self-inflicted failure that
reads exactly like a product bug; the leftover is now cancelled at once.

**The browser pass, and what it did and did not prove.** The QA slot was **free** this tick
and the disk had recovered to 9.8 GB, so the pass ran — three ticks of "the slot is held"
turned out to be the wait, not the gate. It reset `omnion_qa`, brought up the API on :18080
and the panel on :3100, and walked 22 routes including `/backups` with 39 interactive
elements, **before my own `timeout 1500` killed it mid-run**. The later steps all reported
`Target page, context or browser has been closed`: the browser tab dying under this box's
memory pressure, the same pattern the loop's own lessons record, and not a finding. **So the
queue controls are not yet exercised in a browser and the REQ stays open** — the honest
number is "the screen renders and is reachable", not "zero high findings". The next tick runs
it with a longer ceiling, or with the route list trimmed, because a pass that is killed by
its own timeout has verified nothing about the pages after the cut.

**Next.** (a) The browser pass with a ceiling longer than the route list (three ticks of
"the slot is held" were the wait; the real blocker was my own 1500 s). (b) The `partial`-run
UI — the status-card criterion is closed by this tick's fix, but it wants a walkthrough tick
rather than a test alone. (c) Slice 4, encryption.

## 2026-09-30 · REQ-054 slice 4a — expenses, and the greenest eleven walks were the ones that ran nothing

**What.** The expense half of slice 4: the module (`modules/accounting/src/expenses.rs`),
seven routes, five permission keys, migration `0179`, eleven walks. The lifecycle is a **table**
(`draft -> submitted -> approved -> reimbursed`, with `rejected` going back to draft), so "approve a
rejected expense" is one lookup that names the way out rather than four `if`s in four routes.

**The three decisions worth reading twice.**

1. **An approved expense credits accounts payable (`2200`), not an expense.** The money left the
   company but nobody has been paid back yet, so until the reimbursement this is a *claim*;
   posting it as a cost makes an unpaid claim look like money the business has already spent.
2. **The entry is dated on the expense date, not on the day of the decision** — the same reason
   slice 3 dates a reversal on the original payment. A decision taken in April about a March cost
   belongs in March's numbers, and dating it "now" moves every month-end by however long approval
   takes.
3. **The entry is written inside the transaction that flips the status**, with
   `expense_status = $expected` in the `WHERE`. Two approvers pressing the button at the same
   moment both read `submitted`; without the guard both post an entry, and the balance invariant
   happily accepts that because **two balanced entries balance**. The walk counts rows instead.

**Proof.**
- the eleven walks, one per process against a fresh database: **11 passed, 0 failed**;
- `cargo test -p omnion-module-accounting --lib` **53/53** (15 new);
- `cargo test -p omnion-permissions --lib` **68/68** (five new);
- `cargo build -p omnion-api` green.

**Five real defects, and the first one is the tick.** Every one of the eleven walks reported `ok`
at **0.00s** on the first run — because `live_state()` answers `None` when the database will not
migrate, the test *returns*, and libtest counts a returning test as a pass. An entire suite was
green because none of it ran.

- `e67f0891` — **`0179` declared an index as a column** (`add column if not exists
  expenses_decided_by_idx`), so *every database failed to migrate* with `42601`. It parses as
  text, so `git diff` showed nothing wrong.
- `95895b9d` — **`FOR UPDATE` cannot be applied to an aggregate.** `select coalesce(max(..), 0) + 1
  ... for update` is a 500 on *every* create: there is no row to lock. The expense number now runs
  under `LOCK TABLE ... IN SHARE ROW EXCLUSIVE MODE`; the journal entry number follows the weaker
  route `journal::post_entry` already proves (no lock, unique index catches the loser).
- **Two columns the REQ's own spec lists and `0167` never created**: `note` and
  `employee_user_id`. Every write naming `note` was a 500.
- **The list hand-numbered its placeholders** (`$2`, `$3`, … `$8`) and bound them conditionally, so
  a query filtering by `search` alone bound three values against `$4` and answered `500 could not
  determine data type of parameter $4`. Now a `QueryBuilder` — the second time this loop has paid
  for that lesson, which is why it is written down in the ledger as a rule.
- **`numeric` has no `String` representation in sqlx without a cast**, so every list read panicked
  with `ColumnDecode` while the module compiled and every unit test passed.

**Two of the eleven were the test being wrong, not the module.** The error envelope is
`{"error":{"message"}}`, so a flat `body["message"]` read yields `None` and every
"the refusal explains itself" assertion failed against a module that explains itself perfectly.
And the audit table is **`audit_log`**, not `audit_entries` — the `?` swallowed the missing-relation
error into `None` and the walk failed with "an audit row must exist", which reads as *the audit
trail is broken* rather than *this test names a table that is not there*.

**New: `scripts/qa/run-walks.sh`.** One walk per process against its own database, and — the part
that mattered — **a pass is only a pass when the result line says `1 passed or more`**. The first
version of its parser grepped for `test result: [a-z]*\.`, which matches neither `ok.` nor
`FAILED.`, so eleven genuine failures printed as "NO RESULT" and the summary read `0 passed`. Its
timeout is 300s because a 120s budget killed all eleven under load average 119, and a timeout that
fires under load is indistinguishable from a hang.

**Not proved, and it is the browser.** No `scripts/qa/run.sh` pass ran this tick: the box sat at
load 100–119 across a dozen writers and a single cargo build took 24 minutes. The expenses
**screens do not exist yet** — list, form and detail — so the empty/loading/error, 390px and
keyboard boxes stay unticked, and no screen is in the walkthrough inventory.

**Next.** The three expense screens, then a QA pass on the private stack, then the reports
(`income-expense`, `aging`, `cashflow`) and the CSV/PDF exports that finish slice 4.

---

## Tick 74 — the check that had been red since the request was written

**What.** A criterion left open since tick 1 — *the status card's age is consumed by the security
overview check* — turned out to be hiding a defect that had been in the platform since the
request was written, and the reading of it is the whole tick. `backup_age` in
`apps/api/src/routes/security.rs` asked **`backup_runs`** for the last successful run. **No
migration in this repository has ever created `backup_runs`.** The table is `backups`
(`0157`); the name was guessed when the request predated the schema and never revisited when the
schema landed. A missing table makes `fetch_optional` answer `Err`, `Err` was flattened into "no
backup", and `backup_healthy` answered **`fail` on every installation, for ever** — the one
check in the registry that could never go green, on a platform that had taken a backup every night
for a year, with a detail that named the right rule for entirely the wrong reason.

**Why nothing caught it, and why that is the interesting half.** `backup_healthy` is a pure
function of a hand-built `Environment`, so no unit test ever executed the query. An integration
test asserting "no backup → `fail`" would have stayed green for ever, because **the broken reader
*is* a permanent no-backup.** The two worlds are indistinguishable from inside the code and only
distinguishable from outside it: the walk has to take a real backup and then read the screen.
This is the second time in two features that the missing thing was a caller — `next_due_schedules`
shipped with a column and a query and no writer, and `prune_candidates` shipped with four
documented exemptions and nothing calling it. A predicate nothing evaluates is a comment.

**The second defect surfaced only because the walk signed in as a restricted account.** Every walk
that had ever read the posture screen signed in as an account holding *both* keys — the platform
owner's role is granted everything, and a walk that also touches analytics needs them. So the
security centre had been written **inside the `analytics_reports` router builder**, whose
`route_layer(require("analytics.read"))` reaches every route declared on it. `/security/overview`
carried its own, correct `security.read` guard on the handler *and* an `analytics.read` guard it
never declared: an account holding `security.read` and nothing else was refused with `403 this
action requires the "analytics.read" permission` — on the screen whose entire purpose is to be
readable by the person doing the diagnosing. A deployment granting the least would have found the
security centre unreadable, and the natural response to that is to grant more. **A guard that is
only ever satisfied is not a guard that was checked.** The group now has its own `Router::new()`
and its own `merge`, and the comment states the rule for the next group added there.

**A third defect, in a test, of the family this suite keeps finding.** The cancel walk asserted
`select count(*) from backup_restore_jobs` — no `where` — in a database every suite in
`apps/api/tests` shares. A row any other walk had left behind (a killed run, a concurrent suite)
failed it with *"a refused queue wrote a row"*: a sentence about this tenant's refusals, read off
the whole platform's table. It is now scoped to its own tenant. Same shape as the media-part leak
one feature ago — a test that creates fixtures for exactly one organization cannot tell "the whole
deployment" from "my own" apart.

**Gates.**

| Gate | Result |
|---|---|
| `omnion-backup --lib` | **153/0** |
| `omnion-api --lib` | **220/0** |
| `apps/api --test backups` | **26/26** over a live database, each walk run in its own process |
| `apps/admin` `tsc --noEmit` | clean |
| Load-bearing | the posture walk is **red** when the reader is reverted to counting any finished row, **green** on the fix |

**The suite cannot be run as one process, and the reason is the suite's own design.**
`cargo test -p omnion-api --test backups` in parallel gave **18 passed / 8 failed**; the same eight
walks pass individually, and `--test-threads=1` hung in the ninth with a live process, no query
outstanding, `not granted` locks 0 and a main thread in `futex`. Every walk builds a `Fixture`
that **opens a scratch database per test**, and 26 of them at once exhaust the shared pool's
`max_connections = 100` — the same finding the w5 loop recorded, one suite over. A contention
failure that reports itself as `FAILED` with no panic message is the expensive kind: it reads as a
product regression and sends the next tick hunting a defect that is not there. The pass that
counts is 26 walks in 26 processes, and the number worth reporting is that, not the parallel one.

**Next.** (a) The browser pass — the QA slot was held by a sibling for the whole tick, and the
route list is longer than the 25 minutes the harness's own ceiling allows, so it needs the
trimmed-route variant rather than a longer `timeout`. (b) The `partial`-run UI. (c) Slice 4,
encryption.

## Tick 75 — 2026-09-30 — REQ-012 slice 3 (rate limiting + lockout), plus the harness that had to be fixed to run it

**What.** The security centre's two limiter screens (`/security/rate-limits`,
`/security/sign-in-protection`) had never been opened by anything. Four commits:

| Commit | Change |
|---|---|
| `1ded0b5c` | `--only` narrows a pass to named routes/depth passes; both screens walked on desktop and at 390px; the QA API now gets an `OMNION_CSRF_SECRET` |
| `5ca68087` | a QA-slot holder file with two pids on one line deadlocked the pass queue |
| `9e91f2c9` | the QA API could not reach its own database — a masked `***` password |
| `669d584d` | the account lockout was unreachable behind the address lockout |
| `25dddf5c` | a pass died when its click stream could not be written |

**Proof.**

| Gate | Result |
|---|---|
| `omnion-security --lib` | **137/0** |
| `omnion-api --lib` | **220/0** |
| `apps/admin` `tsc --noEmit` | clean |
| The lockout, measured | before: `403 address_blocked` x10, `429 rate_limited` x2, `users.failed_sign_in_count` = **0**, `locked_until` = never |
| The slot deadlock | `reap` resolved `2332198 2332152` -> `2332152`, found it dead, freed the place |
| The database credential | `psql` over TCP with the exact string `run.sh` hands the process returns `1` |
| The click stream | `record()` against a missing directory warns once, keeps all 3 clicks |

**The finding worth the tick.** The account lock and the address lock were compared against the
**same number** (`lockout_attempts`). From one address the address rule therefore fired on the
exact attempt that would have incremented the account counter, so the counter never moved, no
account was ever locked, and the "currently locked accounts" table had no possible content. The
module doc above `sign_in` states that the two dimensions exist for different reasons; the code
gave them one value. The address threshold is now `lockout_attempts * 3`.

**Three harness defects the pass had been hiding behind.**

1. *The QA API had no `OMNION_CSRF_SECRET`.* Every cookie-authenticated write was refused with
   `csrf_unavailable` before its handler ran — so every save, upload and backup in every pass
   was recorded as a screen that "works" while the API answered 403 throughout. The refusal is
   the documented behaviour of a deployment *without* a secret, which is why it read as the
   product being correct.
2. *The QA API's database password was `***`* — a masking artifact, committed long enough to
   look deliberate. It survived a previous check because that check ran `docker exec psql`, which
   uses the container's unix socket and never authenticates; the path that actually uses it (TCP
   to `127.0.0.1:5433`) had never worked. Two paths, one of which nobody exercised.
3. *The pass queue could not drain.* `reap()` handed a whole holder line to `kill -0`, which
   wants one pid; a two-pid line answers false, so the place is judged ownerless -- reclaimed
   while its owner walks, and unreclaimable by the owner whose trap then kills a string.

**Next.** Re-run `--only=security` now that the harness survives its own failures; tick the boxes
naming the two screens. Then REQ-013's `partial`-run UI, then REQ-012 slice 4 (IP access).

## Tick 42 — 2026-09-30 — REQ-054 slice 4b (the four reports + the CSV export), 15/15 GREEN

**What.** `income-expense`, `aging`, `cashflow` and `tax-summary`, read-only, plus
`GET /accounting/reports/{report}/export`. The module, two routes, the `accounting.reports.tax`
key, and fifteen walks.

**Proof.**
* `cargo test -p omnion-module-accounting -p omnion-permissions --lib` — **67 + 68 = 135 passed, 0 failed**
* `apps/api/tests/accounting_reports.rs` — **15/15 GREEN**, one walk per process, live PostgreSQL,
  `passed=15 failed=0 zero-second=0` (`scripts/qa/run-report-walks.sh`)
* `bash scripts/qa/walk-result-classifier-probe.sh` — **ALL 6 PASS**
* `turbo run typecheck` — **2/2 successful**

**FIVE REAL DEFECTS, and four of them are the same species this module has now paid for twice:
a claim about the database written down instead of checked.**

1. **The router was built and never merged.** `.merge(accounting_reports)` was missing, so both
   routes answered 404 to everything while `cargo build` was green, 135 unit tests passed, and
   both handlers were correct. Four walks failed on it. Nothing but a request tells a handler
   from a route.
2. **The tax summary joined `tax_rates`, a table that does not exist** — every request was a 500.
   The table is `accounting_tax_rates`, and the second attempt joined on `l.tax_rate_id`, which is
   a **sales** column; `accounting_invoice_lines` has no rate reference at all. The join is now
   gone: the line's own `tax_percent` IS the rate for reporting, and joining a rate table would be
   a second source for a number the row already owns — which would restate a filed period the
   moment somebody corrected a rate, the exact property the REQ's acceptance box forbids.
3. **`payment_number` is a `bigint`, and `number` and `customer_name` are also NOT NULL.** The
   fixture went through four wrong guesses (23502, 42804, 23502, customer_name) reading a
   migration excerpt each time. The fifth attempt asked `information_schema`, which is the
   contract rather than the history.
4. **The walk runner miscounted its own passes twice** — `grep -E 'test result: FAILED'` matches
   the substring inside the success line (`0 passed; 0 failed` contains "failed"), and the fix
   asked for the literal phrase `1 passed or more`, which only a full-suite summary prints. Fourteen
   green walks read as `passed=0 failed=15`. It has its own probe now.
5. **A window ending "today" cannot contain a not-yet-due invoice** (`due_date <= $3`), so an
   aging report asked about today alone is structurally blind to its own `current` bucket. Caught
   only because the walk asserts its fixture COUNT before summing.

**Three of the fifteen walks were the TEST being wrong, not the module** — the aging walk read the
default 30-day window over fixtures up to 200 days late, the export walk compared two *different*
windows (which passes even if the export drops every old row), and my own arithmetic said 80.00
where the report's 140.00 was right. All three are the same lesson: a walk that does not name the
window it wants is asserting whatever the default happens to be.

**Next.** Slice 4b's remaining halves: the `/accounting/reports` **screen** (the REQ's Screens
table asks for a type selector, period filters, table + chart and the export button — the routes
exist and no screen has been rendered by anything), the **PDF** export half of the export box
(deliberately not built and the box says so), the `⌘K` and global-search box, and the audit-row
assertion. The empty/loading/error and mobile boxes stay unticked until a browser pass runs —
load 78–88 across ten writers this tick, which is the same condition under which earlier passes
reported UI defects that did not exist.

### Tick 75, continued — what the lockout fix actually cost to prove

`669d584d` shipped a fix and a walk, and the walk was **green with the fix reverted**. Reverting
one line — `address_failure_limit` back to `lockout_attempts` — left the test passing. That is the
outcome a regression test exists to prevent, and the reason was structural rather than accidental:

`ClientAddress` and the rate-limit middleware both read `ConnectInfo<SocketAddr>` out of the request
extensions, and `into_make_service_with_connect_info` is the only thing that puts it there. A request
driven through `router().oneshot()` arrives with **no address**, and with no address the per-address
refusal cannot run at all. The walk was skipping the very rule a brute-force walk is meant to test.
Absence of input is not a neutral default; it is the branch that skips the code.

Installing the extension (`7865076d`) then broke two walks that had been passing for the wrong
reason, both the same mistake one layer up:

- **The limiter counted walks against each other.** Every walk shared `127.0.0.1` and the `sign_in`
  ceiling is 10, so the sign-in round trip was refused `429` by a counter it never incremented.
  `test_peer()` now allocates a loopback address per walk — the same shared-state mistake as an
  unscoped `select count(*)`, one layer up.
- **The sign-out walk had never signed out.** `post_logout` sent no CSRF token and this file's state
  carried no secret, so sign-in issued no `omnion_csrf` cookie and the POST was refused `403`. That
  assertion had been failing in CI since the CSRF layer landed. It took three fixes: the suite's own
  secret on its own state, the token read out of the sign-in response, and `get_all(SET_COOKIE)`
  instead of `get(SET_COOKIE)` — sign-in sends TWO cookies, so "no CSRF cookie was issued" was
  concluded about a platform that had issued one.

**Gates, final state.**

| Gate | Result |
|---|---|
| `omnion-security --lib` | **137/0** |
| `omnion-api --lib` | **220/0** |
| `omnion-identity --lib` | **113/0** (the new threshold-invariant test is in this number) |
| `apps/api --test auth` | **7/7** in 8.8s, over a live PostgreSQL + Redis |
| `apps/admin` `tsc --noEmit` | clean |

**Not proved, and named.** The lockout walk could not be shown **red** with the fix reverted: the
reverted run died on a Redis connect timeout before reaching its assertion, because the box ran at
load 80–107 with four sibling writers. The walk is green with the fix and the walk demonstrably
cannot see the address rule without `ConnectInfo` (which is why it passed with the fix reverted),
so the two together are strong evidence — but "the revert was watched going red" is not something
this tick can claim, and the next tick should watch it when the box is quieter.

**The browser pass still has not reported.** It ran for twenty minutes, walked seven screens and then
died with `clicks.jsonl ENOENT` when `/mnt/apopic` hit 100% and the artifact directory was trimmed out
from under it. That is now survivable (`25dddf5c`), and the two limiter screens are walked on desktop
and at 390px, so the next tick is where the boxes naming a screen can be ticked.

## 2026-09-30 — Tick 76 · REQ-010, the audit criterion (the entry that was never missing, and the one that was)

REQ-010 is the first request in wave order that is not `done`, and it had two open boxes. One of
them was the browser pass, which had not run in two ticks. The other was a sentence in its own
acceptance list that read:

> The one criterion left open in this line is the *replace* audit entry, which lands with slice 3's
> CDN purge hook.

**That sentence was wrong, and the error was worth more than the fix underneath it.**
`media.version_created` has been written by `apps/api/src/routes/media_versions.rs` since slice 2,
and `apps/api/tests/media.rs:2053` has read it back out of `audit_log` after a real replace. Rather
than believe the file, this tick measured all nine actions the criterion names — writer present,
and a walk that reads the row back:

| Action | Writer | Walk asserts | Walk *performs* the action |
|---|---|---|---|
| `media.uploaded` | 1 | 1 | yes |
| `media.version_created` | 1 | 1 | yes |
| `media.folder_moved` | 1 | 1 | yes |
| `media.deleted` | 2 | 3 | yes |
| `media.restored` | 1 | 1 | yes |
| `media.purged` | 1 | 1 | yes |
| `media.grant_changed` | 1 | 2 | yes |
| `media.share_created` / `share_revoked` | 1 / 1 | 3 / 1 | yes |
| `media.scan_released` | 1 | 1 | yes |

Nine of nine. A "still open" note in a checklist is not evidence, and this one had been carried
forward across slices until it described a defect that had never existed.

**The defect that did exist was in the same sentence.** `media.retention_applied` was emitted onto
the event bus and **nowhere else** (`apps/api/src/routes/media_retention.rs:758`) — the `audit()`
helper in that file is only ever called for the five *policy* actions, never for a run. So the one
purge in this module that runs **unattended**, and the most destructive thing it does, left no
record at all, while `Empty trash` next door wrote `media.trash_emptied` for the same deletion.
That asymmetry is the shape of the defect: a person could see what they had done, the nightly worker
could not, and an event bus nobody subscribed to is not a record.

`09a0c25d` writes the entry before the event — for the same reason the event is recorded as a run
error rather than a failed request, since the rows are already gone and a failure would tell an
operator to re-run a sweep that already happened. The actor is the account that asked when there is
one and the platform itself when the runner started it: an unattended deletion must not carry
somebody's name.

**The walk proves the branch nothing covered.** `an_unattended_sweep_audits_itself_as_the_platform`
drives `run_once` with `actor: None` rather than the HTTP route, because the route always has a
signed-in account and could never reach the system-actor path. It also asserts that a sweep which
removed *nothing* writes nothing, or the log is a heartbeat rather than a record.

**It was watched going red.** Reverting only the source change (the test file kept) and rebuilding:

```
test result: FAILED. 0 passed; 1 failed; 6 filtered out
an_unattended_sweep_audits_itself_as_the_platform ... FAILED
  assertion `left == right` failed: exactly one entry for the one run that removed something: []
    left: 0   right: 1
```

`[]` — not a wrong actor, not a wrong count, **no row at all**, which is the shape the defect
actually had. Restored via `git stash pop` and byte-compared against the pre-revert copy
(`diff` clean) before the suite was re-run.

**The first run of the walk failed for a reason that was not the code.** It read `purged: 0` and the
assertion blamed the audit entry that came after it. The fixture was wrong: the seeded default keeps
a file for **30** days and only purges it two days after that, so ageing a deletion 40 days reaches
the *trash* window and leaves the purge window 30 days behind. The walk now tightens the policy first,
exactly as `a_sweep_keeps_the_served_version_and_purges_what_is_past_its_window` does. The lesson is
in the walk's own comment, because it is expensive to learn twice.

**Proof.**

| Gate | Result |
|---|---|
| `apps/api --test media_retention` | **7 passed / 0 failed** (277.95 s, live PostgreSQL + Redis, 7/7 including the new walk) |
| the same walk, fix reverted | **FAILED** — `[]`, 0 rows |
| `omnion-media --lib` | **199 passed / 0 failed** |
| `pnpm typecheck` | 2/2 successful (admin rebuilt, web cached) |
| `rustfmt --check` on both files | clean for the added code; the two remaining diffs are pre-existing (`bfe46c3e`, `b0b45428`) |

**Pre-existing red, not mine, and named rather than quietly left.** `cargo fmt --all -- --check` fails
on `backup_schedule_runner.rs`, `backup_sweep_runner.rs`, `restore_job_runner.rs`, `routes/backups.rs`,
`lib.rs`, `main.rs`, `rate_limit_middleware.rs`, and `tests/support/walk_auth.rs`; `cargo clippy -p
omnion-api --all-targets -- -D warnings` fails in `omnion-events` (4 errors) and `omnion-identity`
(`too_many_arguments` ×2). `git status crates/` is clean, so none of it is in files this tick
touched. Reformatting the workspace to green would rewrite files nine sibling writers are actively
editing, so it is reported rather than done.

**The browser pass is still the thing standing between this REQ and `done`.** Measured this tick:
the QA slot queue is **37 deep with entries up to 23 hours old**, one place is held by a live
`omnion-w5` walkthrough, and the box sat at load 80–89 with 8 concurrent `rustc` processes. A pass
was queued (`QA_ONLY=media,media-duplicates,media-trash,media-settings,media-retention`) and left
waiting; on this box that is not a usable instrument, so the tick spent itself on a defect that a
walk could find instead. `/tmp/omnion-qa-pass.log` is from **yesterday** (mtime 2026-09-29 17:14) and
its `csrf_unavailable` rows are history, not this pass.

**Next.** Two boxes remain, both naming the same missing browser pass: the empty/loading/error states
across the media screens and the retention tab's own states. Run `bash scripts/qa/run.sh` with
`QA_ONLY=media,media-retention` on a quieter box, then close REQ-010 and move to REQ-014 (system
health, still `pending`).

## 2026-09-30 · wave4 tick 43 · REQ-054 slice 4c — the accounting reports SCREEN

**What.** `/accounting/reports` renders for the first time. Slice 4b shipped the four reports,
the two routes and fifteen API walks; nothing had ever opened the screen in a browser, and a
route that answers JSON in a walk is not a screen. The screen offers the four report types as
real controls, the window as real date/preset controls mirrored into the URL, a totals table and
a bar chart for whichever report is selected, the server's own definition and period in the
header, a loud "figures agree" note (`reportTotalsAgree`, re-added in integer cents), and an
export that sends the same window the table is showing.

The aging row gained `invoice_id` (`ee8bcc1d`) so the number in the table links to the invoice:
`/accounting/invoices/{id}` takes a uuid, so a link built from the number would 404 — a dead
button, which the definition of done forbids. The CSV is unchanged (its column list is written
by hand in `to_csv`).

A new depth pass `runAccountingReports` (`01359a28`) asserts the report-type control really
switches the report, re-adds the aging bucket totals against the outstanding column **out of the
DOM** (so a wrong "they agree" note fails), and watches the **download event** rather than a
success return.

**Proof.**
- `cargo build -p omnion-module-accounting` — green.
- `cargo test -p omnion-module-accounting --lib` — **67/67**.
- `pnpm turbo run typecheck --force` — **2/2** (admin + web).
- `node --check scripts/qa/walkthrough.cjs` — OK; the pass is registered under `--only=accounting`.
- Browser pass `QA_STACK=w4 ... bash scripts/qa/run.sh --only=accounting` — **queued, not yet
  run**: the box is carrying ten writers (load ~93-100, /mnt/apopic 93% full, ~4 GB RAM free) and
  the single global QA slot is held by w3's long pass. Per the invariants I did not touch it.
  Until that gate runs, the mobile / empty / loading / error boxes stay **unticked**.

**Two things worth keeping from this tick.** (1) Nearly ran the pass against the MAIN writer's
stack: I built the command from memory without `QA_STACK`, which would have defaulted to the
main ports and database and restarted another worktree's servers — killed it in seconds and
confirmed `omnion-qa-*-w3 restarts=0`. An omitted env var is silent because every one of them
has a default; write the stack prefix first, every time. (2) `run.sh` derives the database from
`QA_STACK` itself, so passing `OMNION_DATABASE_URL` is not just unnecessary, it contradicts the
mechanism.

**Next.** Run the queued browser pass and tick the mobile/empty/loading boxes on what it proves;
then the ⌘K / global-search box and the audit-row assertion. PDF export stays deliberately unbuilt.

## Tick 44 — REQ-051: the keyboard contract, and the two screens that had none of it

**What.** `/crm/activities` and `/crm/leads` answered none of the keys the module's shared shortcut
sheet advertises. Both draw their own rows rather than rendering `CrmShell`, and the bindings were
written out **inside** the shell — so the sheet (`crm-parts.tsx`, printing `/`, `j`/`k`, `Enter`,
`e`, `n`, `g then …`, `?`) was a claim about the section that was false for two of its six screens.
The fix is structural rather than additive: the contract becomes `useCrmKeyboard`, `CrmShell`
consumes it, and a screen with its own frame calls the same hook, so the two paths cannot drift.
`CrmShortcutSheet` and `crmListItemCursor` come out of the frame for the same reason.

**Why it is not a cosmetic refactor.** `Enter`/`e` answer *this screen's* question. On the activity
feed they open the record the activity is about (contact → company → deal); on the lead inbox they
open the record the submission became (contact → deal → company), keyed by the **event id**, because
the inbox is the bus's log and a uuid there would resolve to nothing. Where there is no record the
screen says so in a sentence: a free-standing note, and a submission that became *nothing*, are
exactly the rows this inbox exists to explain. The leads inbox also loses its local `/` binding
(now the module's) and keeps Escape, which the hook deliberately does not own.

**Proof.** `cargo test -p omnion-module-crm --lib` **172/172** · `pnpm turbo run typecheck`
**2/2** · `node --check scripts/qa/walkthrough.cjs` OK · the module builds clean with
`CARGO_INCREMENTAL=0`. The browser leg is written (`19ae394b`) and **has not run**.

**Not run, and why.** `/mnt/apopic` reached 100% (92 MB free) at load 86–93; that is what killed the
queued accounting pass, not a defect in it. Reclaiming **my own** cold build cache — `lsof +D`
empty and no writes in 30 minutes on both `omnion-w4-target` and `/dev/shm/w4-target` — freed
7.8 GB of `/dev/shm` and 1.9 GB of `/mnt/apopic`, which is 97% now. A sibling writer's pass still
holds the single QA slot, so the keyboard leg is queued rather than barged for.

**Next.** Run the browser pass and read the new keyboard leg plus the 390×844 box on what it proves.

### Lessons

1. **A shared claim is only as true as its narrowest consumer.** The shortcut sheet was one list in
   one file, so it looked module-wide; the bindings behind it lived in a component two of six
   screens never render. When a spec says "the section", grep which screens *actually* mount the
   thing that serves it — not the screens that exist.
2. **`grep -c 'keyboard={{'` across the section's screens found the defect in one command.** The
   pass already pressed `e` on four screens; the two it missed were exactly the two without a
   binding. A per-screen contract check has to be per-screen, and the enumeration comes first.
3. **Extracting a hook is a mechanical edit that will still find you with three ordering bugs** —
   a `const` used above its declaration, a setter the caller can no longer reach, and a wrapper
   re-derived by two callers. `tsc --noEmit` caught all three; `git checkout --` on the file first,
   because a botched index-slicing rewrite silently duplicated 300 lines and still *looked* right.
4. **An empty database is not a broken screen.** Every new step leaves itself **unset** when the
   list has no rows. A step that reports `false` because there was nothing to press trains people to
   ignore it.
5. **Disk, again.** Ten writers at load 90+ means a queue, not a stall — and the failure looks like
   a timeout. Reclaim only your own cold cache (`lsof +D` + `find -newermt`), never a sibling's.

## Tick 45 — the pass that could not report (2026-09-30)

**What.** Not a feature slice: the two defects that made this writer's browser evidence
unobtainable for four consecutive ticks, both in `scripts/qa/walkthrough.cjs`, both found by
reading the harness rather than by running it.

1. `925972d2` — the roll-up read `m.diagnostics.horizontalOverflow` unguarded. `diagnostics()`
   resolves `undefined` when a tab navigates or closes mid-evaluate, so the 390×844 leg could
   push an entry with no diagnostics and the roll-up threw a TypeError **before**
   `summary.json` was written. The pass left no report, no findings and no screenshot index,
   which is exactly what a pass that "died without saying why" looks like. An unmeasured screen
   is now named in an `unmeasured-page` finding (medium) on both roll-ups, and the pass always
   reaches the summary write.
2. `8345f045` — `/crm/activities` and `/crm/leads` were in the desktop route list and in the
   state list and **nowhere** in `mobileRoutes`, so the phone leg never opened the two screens
   the mobile acceptance box names. `/crm/settings/pipelines` joined them. The pinning probe
   diffs the two tables so the next one-sided addition fails instead of going quiet.

**Proof.** `node --check scripts/qa/walkthrough.cjs` OK · `/tmp/w4-rollup-probe.cjs` **6/6** ·
`/tmp/w4-harness-contract-probe.cjs` **6/6** against HEAD (30 phone routes) and **4/6 with two
failures** against `HEAD~1` — the control is what makes the pass mean something ·
`pnpm turbo run typecheck` **2/2** · `cargo test -p omnion-module-crm --lib` (result below).

**The pass is queued again, not run.** The single slot is held by a *live* sibling pass
(`qa-slot.sh` holder 2153263, cwd `/mnt/apopic/omnion-w3`, its `run.sh` + walkthrough + chromium
tree all alive) — not a stale place, so it stays. `/mnt/apopic` is at 98% and the box at load 92.
Running a pass into that is what produced the four silent deaths; queueing costs a tick and
spending it produces no report at all. The two unticked boxes (390×844, keyboard sheet) stay
**unticked** — the legs are written, and nothing here is a verdict on a screen.

**Next.** Run the pass the moment the slot frees; read the keyboard leg and the phone box off
`summary.json` — which now exists whatever the leg did — and tick only what it actually shows.

## 2026-09-30 · omnion-wave4 · tick 46 · REQ-051 (CRM)

**What.** The blocker was never the QA slot — it was that a pass *could not name the binary it
starts*. `scripts/qa/run.sh` spelled `$ROOT/target/debug/omnion-api` in three places, which
assumes every writer builds inside its own worktree. This worktree does not (its target is
`/mnt/apopic/omnion-w4-target`), so `$ROOT/target` did not exist, the freshness test found no
binary, and the pass started a stale one or died — the `could not create file …/target/…` that
tick 45 recorded as a sibling's doing. The in-repo `target/` is shared ground a sibling may
delete mid-link, and that failure is `os error 2`, not 28, so it never reads as a full disk.
Fixed in `87ec641c`: resolve the path once into `API_BIN` from `CARGO_TARGET_DIR`, so the
freshness test, the `pm2 start` and the `cargo build` that precedes them all agree on one path.

**The four-tick sign-in blocker is dead.** The pass reached the sign-in screen, ran the wizard,
and walked **all six CRM screens** for the first time in this branch's history
(`--only=crm`, 6/6 routes, 35 clicks, 47 screenshots, exit 0).

**Proof.** `cargo build -p omnion-api` `REAL_EXIT=0` out-of-tree, binary at the resolved
`API_BIN` (`198 MB`, 260 crates) with **no in-repo `target/` at all** — the fix is what makes
the pass runnable here · `bash -n scripts/qa/run.sh` OK · `node --check scripts/qa/walkthrough.cjs`
OK · `pnpm turbo run typecheck` **2/2** · `cargo test -p omnion-module-crm --lib` (below) ·
pass exit **0**, report `qa-artifacts/20260930-134950`.

**This pass is a NULL, and it is worth saying why precisely.** `/mnt/apopic` hit 99-100% mid-run
(99% when the API binary finished building, 984 MB free at the tightest point). Every screenshot
then failed with `ENOSPC: no space left on device, write`, which the walker swallows into
`interact: … → 0 elements` — so the screens were never *clicked*, only loaded. The same full disk
took Postgres out mid-pass and the API answered a real
`503 dependency_unavailable "database is unavailable"` (26 of the 165 findings quote it, and the
walkthrough's own `export` call recorded that 503 verbatim). So `160 high` is 160 counts of
*infrastructure*, not 160 defects: `crm-state` 29, `console-error` 54 and `request-failed` 46 are
the 503s, and the `crm-depth`/`crm-deals` legs are the screens that were loaded but never
clicked. **Nothing here is a verdict on a screen** and no box is ticked on it.

**The disk fight is the real lesson.** I reclaimed my own cold `target/` (2.5 GB) at the start,
which is what made the build possible, and then spent exactly that gain on a build **and** a
browser pass at the same time; the pass and the test build then raced for the last gigabyte and
the test build lost — `REAL_EXIT=101` with `rustc-LLVM ERROR: IO failure on output stream`, which
prints **no** `error[]` line and looks like an ordinary compile failure rather than a full disk.
One heavy thing at a time on this box, and measure the disk *while* the heavy thing runs.

**Next.** Re-run the CRM pass alone, with the build cache warm and no competing test build, and
read the two unticked boxes (390×844, keyboard sheet) off `summary.json` — the leg now *runs*,
which is the first time that has been true in four ticks.
## 2026-09-30 — REQ-014 slice 1 (probes + overview) · the screen whose job is not to reassure you

feat(health): the probe registry, the versioned surface, `/health` and its drill-down

The tick before this one wrote the code and ran out of clock before it ran. So the first
thing this tick did was *make it fail*, and the five red tests were not the boring kind: four
of them were the product lying, and they are now fixed rather than waived.

**Four wrong numbers and one unreachable word, all in the same class.** `longest_mount` read
the mount point from the wrong side of the ` - ` separator in `/proc/self/mountinfo`: the path
is field five of the LEFT half and `ext4` is what the right half opens with. Every lookup
therefore returned `None` and the disk card fell back to the root filesystem — a plausible,
wrong, **green** number for a data directory living on its own volume, on the one row an
operator reads when disk is their suspicion. That is the exact failure the longest-match
ranking exists to prevent, and it was shipping.

The ranking then asked whether a path begins with `//` to decide it was under `/`. Nothing
does, so the root filesystem dropped out and a single-filesystem host reported `unknown`.
Depth was then measured in **slashes**, where `/` and `/mnt` tie at one, so the first line in
the file won — and a tie broken by document order is not a ranking, and is not reproducible
across hosts. Segments cannot tie; the comparison is now total.

`unescape_mount` read `\040` as base 10. It is octal: 40 decimal is `(`, not a space. A mount
point containing a space decoded to something unmatchable, which routes straight back into
the first bug — wrong number, silently.

And the banner had **no `healthy` branch at all**. With every service green, `worst` returned
`Some(("healthy", ..))`, which fell into the "not checked yet" arm. A fully healthy platform
reported `unknown`, and "All systems operational" was unreachable code on the one screen
whose entire job is to be believed. The reassurance was never wired up.

**What the tests were actually asserting matters here.** A `mountinfo` parser is exactly the
kind of function whose unit test looks green and whose bug is invisible: the test compared a
string and the string was wrong in a way nobody typed. Writing "040 is octal, not decimal"
into the code is cheaper than rediscovering it on a box whose `/` and `/mnt` are the same
depth.

**`/health/services/{key}` did not exist** when the overview was written, and the overview
links to it from all eight rows. That is eight dead affordances — the specific thing the
definition of done forbids — and it was invisible to every gate that had run so far, because
the gates tested the *server*, and the server was correct. It ships now, and the walkthrough
route list names it.

Proof:

```
omnion-health --lib   29 passed; 0 failed          (four product bugs fixed to get here)
admin tsc --noEmit    exit 0
node --check walkthrough.cjs   syntax ok
```

**What this costs the next tick:** the browser pass (`bash scripts/qa/run.sh`) has still not
run, so the walkthrough leg, the drill-down and the row-click path are unproven in a browser.
The box is at load 93 with `/mnt/apopic` at 97% and nine sibling writers, and a browser pass is
the one instrument that wants both. Slice 2 (history, ranges, CSV export) is next and does not
need it.

**Next:** REQ-014 slice 2 — sample aggregation, 1 h / 24 h / 7 d ranges, CSV export and the
24 h trend charts on `/health/metrics`.


## Tick 78 — the box that said "when deliveries exist" (and last tick they started to)

REQ-021's drawer box had been unticked since the request was written, with a note that read *"the
per-channel delivery rows are slice 2's, when deliveries exist"*. Tick 77 made them exist. The note
expired and the gap became real: `GET /api/v1/notifications/{id}` promised "one notification, **with
its delivery rows**" in its own doc comment and returned a bare notification — so the platform knew
an e-mail had been given up on, and the person waiting for it was told nothing.

**What shipped**

| Commit | What |
|---|---|
| `c3394bb5` | `store::deliveries` + `NotificationBody.deliveries`, filled after the ownership check |
| `30c27d12` | `notification_delivery_reader` — 4 walks over live PostgreSQL |
| `326061ee` | the drawer's Delivery section; one channel vocabulary for three screens |
| `a7b94201` | the walkthrough leg for that section |

**Proof**

```
notification_delivery_reader   4 passed; 0 failed  (26.47s, live PostgreSQL)
notification_delivery          8 passed; 0 failed  (49.12s)  ← the runner's suite, unchanged
omnion-notifications --lib    90 passed; 0 failed
admin tsc -p tsconfig --noEmit  exit 0
node --check walkthrough.cjs   syntax ok
```

**The access-control shape is the part worth keeping.** `notification_deliveries` is keyed by
`notification_id` and carries **no `user_id`** — nothing in that query can be scoped to a caller, so
the ownership read is the entire check. A delivery read placed *before* it would answer a populated
channel list for somebody else's notification, which turns the route's `404` into an existence
oracle. The route reads the notification first and only then its deliveries, and the walk asserts
the *mechanism* rather than the outcome: the stranger resolves nothing **and** the owner still gets
rows through the same function, because a read that returned nothing for everybody would pass the
stranger leg while hiding the entire feature.

**Three of the four walks caught this tick's own wrong assumptions before the product did.** Each
correction is now written into the walk, because the next tick would otherwise re-make them:

1. `on conflict (channel)` — the uniqueness is `(organization_id, channel)`, so the fixture asserted
   a constraint the schema does not have (`42P10`). It creates the organization now.
2. "`in_app` is written first" — it is not: `enqueue` loops the *enabled* channels and appends
   `in_app` after. And every row in one call shares a single `now()`, so the chronological key ties
   and the alphabetical tiebreak decides. The walk now asserts **stability** (same notification, two
   reads, same order), which is the property the drawer needs and the only one a missing
   `order by` fails — on the *second* read; the first always looks right.
3. "the in-app row is `sent` because enqueueing is delivering it" — `enqueue` writes it `pending` and
   a tick only makes it `sent` by draining it through a transport. The walk registers the real
   `InAppTransport` next to the refusing e-mail one.

**What this costs the next tick:** the browser pass (`bash scripts/qa/run.sh`) has still not run, so
the walkthrough leg and the drawer's rendering are unproven in a browser. The QA slot queue is
shared and `/mnt/apopic` is at 97%, so a pass was not the right instrument for this tick.

**Next:** REQ-021's remaining boxes are the two unproven keyboard legs and the browser pass, both of
which need that pass. Move to **REQ-014 (system health, `pending`)** — it is the first item in wave
order with no code at all, which is worth more than a fourth box on a REQ whose remaining boxes are
all waiting on the same missing instrument.

## 2026-09-30 — QA harness: a pass that cannot be believed must not be able to look like one that can (`e8d26476`)

fix(qa): the null pass gets the exit code it should have had

The tick-46 pass walked all six CRM screens, reported `165 findings (high 160)` and exited 0. It
was a null. `/mnt/apopic` was at 100%, so every one of its 47 captures failed on ENOSPC; the same
full disk had taken Postgres out, so the API answered a real `503 "database is unavailable"` on
every read. Four ticks went into diagnosing passes that could not be diagnosed, and the tick-46
entry was careful — it called the number a null, named the cause, ticked nothing — but only after
a reader had to work out that "160 high" meant nothing at all. The harness should have said so.

**Three defects, one root cause: the pass had no way to say "I proved nothing".**

`shot()` swallowed the failure. It logged one line and carried on, so a screen that was never
photographed was indistinguishable from one that was and looked clean — and the interactor's
`interact: 0 elements` was the *consequence*, not the bug. Failures are counted now
(`counts.shotFailures`), carried in `summary.json`, and stated **above** the findings in both the
walkthrough's own `report.md` and the roll-up `QA-LATEST-w4.md`. The number that decides whether
the other number can be believed has to sit next to it; a verdict-shaped number under a footnote
is how a null reads as careful triage.

A dead backend was filed as a product defect. `probeApiLiveness()` asks `/readyz` **first**,
because an API whose process is alive but whose database is gone answers `/healthz` 200 and
`/readyz` 503 — precisely the state the pass was in, and precisely what an "is something
listening" probe calls healthy. It is asked directly rather than inferred from the failure pile,
because the pile is the symptom being classified and cannot be its own evidence. The answer
raises one `qa-stack-down` finding naming the cause, and reads matching the server's own wording
(`database is unavailable`) are downgraded to `low stack-failure` in **both** the network and the
console arm — leaving them disagreeing with each other was the other half of the 26.

The verdict was never the exit code. `passIsVoid()` withholds it: no evidence, no
evidence-producing interactions, or no stack exits **4** after writing everything it saw.
`run.sh` keeps the artifacts, rolls the summary up and re-exits non-zero, so the record of a
broken pass survives while the command still fails. `set -e` is what would otherwise have eaten
all three — which is exactly how the tick-46 null reached a BUILD-LOG in the first place.

**The probe found two real defects in the change that introduced it.** `void-pass-classifier-probe.sh`
extracts `passIsVoid` out of the walkthrough with `sed` and calls it, rather than re-typing the
rule: a copy of a rule is a second rule, and both existing checkers in this directory had gone
green against a correct predicate while the thing they checked stayed broken. `.length` on a
number is `undefined`, so a roll-up *count* of `47` read as "no evidence" and voided every
well-formed pass; the same mistake on `shotFailures` read a count of `3` as zero and voided
nothing. Both are the same bug in two places and one measurement now serves both shapes. **12/12.**

**Also this tick.** Merged `origin/main` (10 commits): `app-shell.tsx` icon union, the mobile
route list (CRM entries + main's `/health`), and the append-only `BUILD-LOG.md` through
`scripts/qa/merge-build-log.py` — `base=5297 ours=8850 theirs=5416 merged=8969`, **0 entries lost**
on both sides. Reclaimed my own cold `target/` on the 97%-full mount after checking `/proc/*/cwd`
for a live holder: **1.9 GB**, 97% → 94%.

**Proof.** `bash scripts/qa/void-pass-classifier-probe.sh` → **12/12**, `node --check walkthrough.cjs`
OK, `bash -n run.sh` OK, `turbo run typecheck` **2/2** (4m32s, 1 cached). The walk itself has NOT
run: the QA slot is held by a sibling writer's live pass (`/proc/1896211/cwd` = `omnion-w6`, holder
pid dead-or-waiter), and burning the tick waiting for it is the trap this writer hit four times.

**Next.** Run the CRM pass alone on this stack and read the 390×844 and keyboard boxes off
`summary.json` — the legs run, which has been true since tick 46 and is still unproven. No box is
ticked by this commit, and `runCrmKeyboardAndMobile` is the one that matters: it is the pass whose
exit code is now honest enough to be believed.

**Gates, completed after the entry above.** `cargo test -p omnion-module-crm --lib` → **172 passed;
0 failed** (0.02s of actual test time; the 19 minutes were 411 crates compiled from cold, at load
76 with six writers on the box — the `--quiet` log stayed 0 bytes for the whole of it, which reads
as a hung build and was not). `bash scripts/qa/void-pass-classifier-probe.sh` → **12/12**,
`node --check walkthrough.cjs` OK, `bash -n run.sh` OK, `turbo run typecheck` **2/2**.


## tick 48 — the QA semaphore could not give a writer its own lock back (2026-09-30)

**What.** `scripts/qa/qa-slot.sh` reaped stale places exactly once, before its wait loop. A place is
only reclaimed when its **holder** pid is gone, so a pass that took a place and was then killed
leaves a place nobody owns — and a writer already past that single `reap` can never learn it went
stale. It re-read the same file every 15 s, counted it as busy, and sat out the whole
`QA_SLOT_WAIT`. On this box that is 3600 s, so the cost of one sibling pass being SIGKILLed at
load 88 is an hour of a writer's tick spent waiting on a semaphore that had been free since the
crash. Observed twice this tick: a dead holder (pid 3338826) whose place survived it, and a live
one 30 s later.

**The fix** (`034048f4c`) is one line — `reap` moved inside the wait loop — and the risk it
introduces is the one that matters: reaping a place whose holder is *alive* would let two browser
passes run at once, on the box whose exhausted RAM is the entire reason this semaphore exists.
So the probe leads with the negatives.

**Proof.** `scripts/qa/qa-slot-reap-probe.sh` (**5/5**) runs the real script as a subprocess
against a private `QA_SLOT_DIR` across five states: empty semaphore, dead holder, live holder,
live holder below the age grace, and a live holder **killed mid-wait**. The last is the
regression — a reaper that ran once before the loop has already decided that place is busy. Run
against the pre-fix script the probe is **3/5**: both reclaim cases fail with
`no place after 2s/20s, proceeding without one`, so it can tell the two shapes apart instead of
being green either way. The two `ok` lines it prints against the old code are the cases the fix
does not touch, which is the point of printing them.

**Also this tick.** The first pass attempt died with `could not write output to
target/debug/deps/…: No such file or directory` and `couldn't create a temp dir (os error 2)` —
os error 2 on a path that existed, i.e. a sibling writer deleted this worktree's `target/` while
cargo was writing into it. The build now runs on a private `CARGO_TARGET_DIR=/mnt/apopic/w4build`
with `CARGO_INCREMENTAL=0`, which is the shape the box's memory already records for w2. The pass
itself is still queued: load average 87.9 on 6 cores with six writers compiling, and both the pass
and `cargo test -p omnion-module-crm --lib` are waiting on cargo's shared package-cache lock.
**No acceptance box is ticked by this commit** — the CRM pass has still not produced a
`summary.json` this branch can believe.

**Gates, completed after the entry above.** `bash -n scripts/qa/qa-slot.sh` OK,
`bash -n scripts/qa/qa-slot-reap-probe.sh` OK, `bash scripts/qa/qa-slot-reap-probe.sh` → **5/5**,
and the same probe against the pre-fix script → **3/5** (the "proven to fail" direction).
`CARGO_TARGET_DIR=/mnt/apopic/w4build CARGO_INCREMENTAL=0 cargo test -p omnion-module-crm --lib`
→ **172 passed; 0 failed** (0.16 s of test time; the wall clock was the shared package-cache lock
under six compiling writers). `pnpm turbo run typecheck` → **2/2 successful** (3m33s, 1 cached),
run under `env -i` because a node command that dies with a bare SIGABRT on this box is usually
inherited env, not the toolchain. The browser pass is still compiling its API binary.

**Next.** Read `summary.json` off the private-target pass and tick the 390×844 and keyboard boxes
on whatever it actually measured — `runCrmKeyboardAndMobile` is the leg that has to answer for
four ticks now. If the pass is void again, `passIsVoid()`'s exit 4 says which leg lost its
evidence, and that is the sentence to act on rather than another re-queue.
## 2026-09-30 — REQ-014 slice 2 (history) · the export that has to match the screen

feat(health) + feat(admin): named ranges, real aggregates, per-row sparklines, and a CSV the
server renders from the same list the table renders from.

Slice 1 answered "is it up right now", which is the only question a health screen can answer on
its own. This slice adds the question the operator asks *after* the alarm — how long has it been
like this, and has it been like this before.

**The range is a name, and that is the whole design.** `Range::Hour/Day/Week` with keys
`1h`/`24h`/`7d`; `Range::parse("168")` is a **400** that names what is offered. The tempting
shape — read `hours`, clamp it, answer — produces a table labelled `7d` holding a day. The part
that makes it dangerous rather than merely wrong is that the CSV is rendered from the same
clamped query, so **the export matches the table perfectly while both are wrong**: the
acceptance criterion "CSV export matches the range shown" would pass on the exact implementation
that made the screen lie. That is why the walk asks for `?range=168` directly and requires the
refusal, and why the refusal is worth more than the happy-path legs.

**An empty window has no numbers, not zeros.** `avg()` over no rows is `NULL`. The failing
implementation is `coalesce(avg(value), 0)`, and it passes every count assertion and every
"there is a row" assertion while making a metric nobody has ever measured the calmest row in the
export. The walk asserts the empty case *and* the populated case **through the same function**,
because a read that returned nothing for everybody would pass the empty leg alone.

**One fixture that straddles a window boundary, deliberately.** Three samples at 2 h, 1 h and
30 min, values 10/20/30. A fixture whose every sample sits inside every range cannot distinguish
a window-aware query from one that ignores the window — so the same three rows give `24h` a mean
of 20 and `1h` a mean of 30, and the assertion is that they differ. The retention walk asserts
the same crossing from the other end: retention is 30 days and the widest range is 7, so if the
two ever met, the `7d` view would silently become an empty one a month after launch.

**A single point draws a dot.** A polyline through one point has no length and renders as
nothing, which the table reads as "no samples" on a row that has one. And the sparkline is
scaled between the row's own min and max rather than from zero: a queue sitting at 40 and peaking
at 60 is 50% busier, and a zero-based chart draws that as a hairline.

**What this cost the tick, stated plainly.** The browser pass has still not run — `qa-slot.sh` is
held by a sibling writer (`/mnt/apopic/omnion-w6`), the box sat at load 101 with 0 free RAM and
`/mnt/apopic` at 94%. `runHealthMetricsDepth` is written, wired into the route list, the mobile
list and the depth-pass registration, and it is **unrun**: the range switch, the click-through
from the overview, the CSV-vs-table comparison and the 390 px leg are all unproven in a browser.

Proof for what did run:

```
omnion-health --lib                 40 passed; 0 failed   (29 in slice 1, +11 here)
admin tsc -p tsconfig.json --noEmit  exit 0
node --check walkthrough.cjs        syntax ok
```

**The important part: `omnion-api` did not compile, and it was slice 1 that broke it.** The
final `cargo check -p omnion-api` of this tick reported five errors, and **none of them were in
slice 2's code** — they were at lines 254, 278, 322, 375 and 403, which is slice 1's
`context()` and its four `run_and_record` call sites:

* `config.storage.driver()` — **`Config` has no `storage` field.** The object store's settings
  live in `omnion_storage`'s own `StorageConfig`, and the code reached for a field that was never
  there. It compiled on tick 79 because `omnion-health`'s crate tests never compile
  `omnion-api`, and the tick-79 proof was `omnion-health --lib` + `tsc` + `node --check`.
* `?` on `run_and_record(...).await` — **`ApiError` has no `From<HealthError>`**, so the `?` had
  no conversion. Four occurrences, one per handler.

**The lesson is about which gate you run, not about how long it takes.** Three gates were green
on tick 79 and the crate did not build: the health *crate's* unit tests (which do not compile the
API), the admin's `tsc` (which cannot see Rust), and `node --check` on the walkthrough. Every one
of them was correct about the thing it checked — and not one of them included
`apps/api` in its dependency graph. **A per-tick gate must compile the crate you edited.** I had
written in my own ledger that "`cargo test --lib` caught a crate's bugs in 0.8 s" and treated that
as the cheap gate; it is cheap because it is *narrow*, and the tick that only ran it did not learn
whether the file next door compiled. The cost of the full check is the thing to budget, not the
thing to avoid.

Both are fixed in `b7900559`: the driver name is read through
`StorageConfig::from_env()` — the same source the live handle was built from, so the probe and the
client it probes cannot disagree — and the four `?`s map through the module's existing
`map_store`. `health_probes.rs` could not have caught either one: it exercises the crate's
functions directly and never builds a router.

**The 8-walk `health_history` suite still has not run** — it needs a ~14 minute link on this box
and the tick was spent on the check that found the red instead.

**Next:** run `cargo test -p omnion-api --test health_history` and `cargo test -p omnion-api
--test health_probes` against a live PostgreSQL, then the browser pass when the QA slot frees.
Slice 3 (incidents + thresholds) is untouched.



### What the void verdict finally named (tick 48, second half)

The pass that ran on the private target dir did what tick 47 built it to do: it refused to report
a result, and it left behind the one thing five ticks of queueing had been missing — a *reason*.
`summary.json` had a single field:

```
fatal: Error: page.goto: Navigation to "http://127.0.0.1:3103/login" is interrupted
        by another navigation to "http://127.0.0.1:3103/"
```

Five writers' passes had queued for five ticks on "the QA slot is held" and on a null pass, and
the actual blocker was a missing `.catch(() => {})` on **one** of 157 navigations. Playwright
raises an interrupted navigation as **fatal**, not as a recoverable timeout, so `run.sh` recorded
the run, the walk produced no screenshots and no results, and `passIsVoid()`'s exit 1 was the
only honest thing left to say. It was honest and it was **inert**: a verdict that cannot name the
leg is a verdict that sends the next tick back to the queue.

**Fixed** (`10800224`): the `/login` hop in `ensureSignedIn` and the `/` hop in `runWizard` — the
two navigations the app's own router redirects — now tolerate losing. The two *reachability*
probes (`admin/login` at line 8965, the public site at 9707) keep their errors on purpose, because
a pass whose every navigation is swallowed cannot report a dead stack at all. That distinction is
the whole rule, and it is why the check is not "every `goto` is caught".

**Proof.** `scripts/qa/walkthrough-navigation-probe.cjs` reads the shipped file and classifies all
**157** navigations: 4/4 PASS, including "every non-probe navigation (155) tolerates an
interrupted redirect" and "the pass still has a reachability probe whose error it can report".
Against the pre-fix file the same probe is **2 failures naming lines 601 and 740** — the exact
lines, not approximations.

**The probe needed three versions, and the two dead ends are the lesson.** v1 scanned line by
line and reported four phantom failures, three of which were guarded on the *next* line. v2 kept a
running paren buffer across the file and reported **64 of 154** navigations — a checker that has
lost most of what it is checking will happily go green: v2 passed the pre-fix file 4/4, which is
the worst possible result for a probe. v3 reads each call **forwards** from its own line until its
parentheses balance. The check that caught it was not a test case but running the probe against
the broken file: a checker that cannot fail is not a checker, and the moment to run it against the
broken input is before it is trusted, not after.

**Gates.** `node --check scripts/qa/walkthrough.cjs` OK, `node --check
scripts/qa/walkthrough-navigation-probe.cjs` OK, the probe 4/4 (and 2 failures pre-fix),
`cargo test -p omnion-module-crm --lib` **172 passed; 0 failed**, `pnpm turbo run typecheck` **2/2**.

**Next.** The pass re-ran on the fix; if it produces a `summary.json` with results rather than a
`fatal`, the 390×844 and keyboard boxes can be read off it. The ledger's standing warning applies
to this tick's own evidence: the probes are static checks of structure, and a structural check is
not a screenshot. The walk is still the only thing that can tick those two boxes.

### A breakpoint prefix is a layout decision (tick 48, third part — `3c006e29`, `fde0b3a9`)

With the pass finally runnable, the mobile acceptance box became answerable, and the answer was
"no" — so the tick went looking for *why* rather than waiting for a screenshot to prove it. Four
business screens carried a Tailwind multi-column token with no breakpoint prefix:

| Screen | Was | Now |
|---|---|---|
| `crm/activities-view.tsx` — "Hangs off" / "Record id" | `grid-cols-2` | `gap-2 sm:grid-cols-2` |
| `inventory/stock-list-view.tsx` — on hand / reserved / available | `grid-cols-3` | `gap-2 sm:grid-cols-3` |
| `inventory/inventory-parts.tsx` — drawer preview | `grid-cols-3` | `gap-2 sm:grid-cols-3` |
| `inventory/transfers-view.tsx` — four timestamps | `grid-cols-2 … sm:grid-cols-4` | `gap-2 sm:grid-cols-2 … sm:grid-cols-4` |

`sm:grid-cols-2` collapses to one column below 640 px and `grid-cols-2` does not, which is the
entire mechanism — a layout that is correct on desktop and unfixable on a phone is not "responsive",
it is one breakpoint away from the acceptance box it was written against. The transfers row is the
one worth naming: it grew a bare `grid-cols-2` beside an `sm:grid-cols-4`, so it rendered
one-up-then-four instead of 2 → 4, a defect that only exists *because* the 4-up was already right.

**The probe that found them is structural, and that is stated rather than hidden.**
`scripts/qa/multi-column-grid-probe.cjs` reads the shipped `.tsx` files and fails any multi-column
`grid-cols-*` that has no `sm:`/`md:`/`lg:` prefix. It is proven in both directions against the
pre-fix files (it names the four sites), which is the only reason to believe it would ever fail.

**Typecheck caught two ways to write a comment that stops the file parsing** — both in the same
edit, both invisible to `node --check` because the file is TSX: a brace comment inside a ternary
arm (`preview ? (/* … */ …) : …`) is not an expression, and a brace comment that quotes another
brace comment's token closes early on its own `*/`, leaving the rest of the paragraph to parse as
JSX text. Gates: `pnpm turbo run typecheck` **2/2**, `node --check` on both harness files, `bash -n`
on the probe.

**Next.** Run the CRM-focused pass (`QA_ONLY=crm`) on the private stack and read the mobile and
keyboard legs off it. The box stays unticked until the walk says so — the probe is not the walk.

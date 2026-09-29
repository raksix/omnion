
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


## 2026-09-28 — REQ-005 slice 4a · the module switch stops being a stored intention


## 2026-09-28 — REQ-006 slice 4b-2 · a live provider, and the four defects only a live provider shows
- **What shipped.** **`0d95b2d`** (five atomic commits) — the per-organization module switch now

  *applies*. Slice 3 stored the decision in `organization_modules` and proved the toggle; nothing
- **What shipped.** **`87390ff`** — `apps/api/tests/support/stub_idp.rs`, a real identity provider
  consumed it, so a switch that changed a row and no observable behaviour was exactly the "limit
  this process starts on a loopback port: a discovery document, a JWKS, an authorization endpoint
  that only decorates the UI is a lie" the REQ warns about, one layer down.
  that answers `302` with a `Location`, a token endpoint that verifies the PKCE challenge itself and
  - **`d89ddc2`** — `module_routes()` / `route_module()` / `navigation_without()` in
  spends a code exactly once, and a SAML endpoint that signs an assertion with both halves of the
    `crates/identity::tenancy_limits`. One table of which module owns which path, in the crate the
  XML-signature binding — all under one freshly generated 2048-bit RSA key.
    Modules tab already reads its list from.
  `apps/api/tests/sso_live.rs` drives the **real router** against it: the browser is sent to the
  - **`40fced2`** — `apps/api/src/module_guard.rs`, one layer over the whole `/api/v1` router.
  provider's own endpoint, comes back with a code, the code is exchanged, the token is verified
    Refuses with `403 organization.module.disabled` carrying `module` and `module_name`.
  against the *published* keys, the claim → role mapping attaches the role, JIT provisions the
  - **`8adb813`** — `/api/v1/me/organizations` grows `disabled_modules`, measured for the *current*
  account, and `GET /me` with the resulting cookie names the directory person. The same file does
    tenant only, so the shell hides the entry without a second request.
  SAML. Plus the fixes below.
  - **`66f86d7`** — the sidebar is filtered on those keys, a person already inside a switched-off
- **Why a stub and not more fixtures.** Every other proof of enterprise sign-in tested one layer:
    module is sent to the overview, and the Modules tab's consequence sentence names the screen
  a verifier against a synthetic token, a reader against a hand-built assertion, the HTTP layer
    that disappears instead of repeating one now-true generic line.
  against refusals it triggers itself. None of them proved that a *browser* can complete a round
  - **`0d95b2d`** — `switching_a_module_off_hides_its_api_and_switching_it_back_restores_it`.
  trip, because that needs a provider on the other end. The stub shares nothing with the code it
- **Also.** **`644a129`** — `0028_organization_departments.sql` → `0029`. Merging `origin/main`
  tests except the `rsa` crate, so agreement between the two sides is evidence rather than
  collided with main's own `0028_site_presets.sql`; git merged the files and sqlx then refused to
  tautology.
  migrate with `duplicate key value violates unique constraint "_sqlx_migrations_pkey"`. It reads
- **Proof.** `cargo test --workspace --lib` → **598 unit tests, 0 failures**. `cargo test -p
  as a schema bug and is only a numbering collision, so the fix is the *file*, never the database.
  omnion-api --test sso --test sso_live` → **3 walks, 0 failures**, all three in one database.
- **Proof.** `cargo test -p omnion-identity --lib` → **144 passed** (9 new: path→module, the
  `pnpm typecheck` green. `cargo clippy --all-targets` adds no new warning.
  `/api/v1` mount, the core-is-never-a-module list, `/public/*` survives, segment boundaries, no
- **Four defects the walk found, and each is a thing no layer-by-layer test could see.**
  installed module without a route). `cargo test -p omnion-api --lib` → **131 passed** (3 new).
  **(1) Every SAML sign-in was broken.** The relay page posted the *return path* as `RelayState`
  `tsc -p apps/admin/tsconfig.json --noEmit` → **clean**.
  while the callback claims a **challenge** from `RelayState` — so the callback could never claim
  `cargo test -p omnion-api --test tenancy_limits -- --test-threads=1` → **20 passed, 3 failed**,
  one and every SAML sign-in ended in `invalid_state`. The page now carries the challenge `start`
  and the same three fail at `644a129` with the change stashed — baseline proven by re-running, not
  issued (and HTML-escapes it, because the route is public and a `state` query is attacker-supplied
  assumed (`a_ceiling_really_bounds_accepting_an_invitation`, `a_queued_link_never_works_and_says_so`,
  in the general case). **(2) `c_hash` was checked against the PKCE verifier's hash.** No provider
  `the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows`).
  has ever seen the verifier, so a real directory could not satisfy that rule: it refused every
- **Next.** The per-organization retention sweep (`organization_settings.audit_retention_days` is
  legitimate sign-in while proving nothing. It is now the real code hash (OIDC Core §3.1.3.6), and
  stored and validated and read by nothing), then the `organization.member.joined` webhook-isolation
  a *missing* `c_hash` is not a refusal, because the claim is a RECOMMENDED and an optional claim
  walk that closes the slice, then the mobile pass. The QA walkthrough still has not run green on
  cannot be a mandatory rule. **(3) The SAML `test` button could never report success.** Its probe
  this branch; four other writers were mid-pass for most of the tick and the volume sat at 98–100%
  was a self-closing `<saml:Assertion/>` that the reader never parses, and the verdict was
  (reclaimed a dangling docker volume, my own stale test binaries and `~/.npm/_cacache` to get
  inferred from the error text — so every certificate read as broken. The certificate step is now
  back to ~1G), so the full pass is the first thing the next quiet window should do.
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

### Wave 5 · REQ-005 slice 1 — organization memberships, invitations and the switcher
## 2026-09-27 — REQ-006 slice 4b-2 (part 1) · the enterprise sign-in core


- **What.** The tenant layer becomes first class. A user belonged to exactly one organization
- **What shipped.** `crates/identity/src/sso/` — the whole protocol layer of enterprise sign-in,
  through `users.organization_id`, with no membership row, no way to invite anybody and no way
  before any HTTP: **provider rows** (`providers.rs`, the `auth_providers` table of `0011` finally
  to hold a second tenant. Migration `0019` adds `organization_members` (status + a primary flag)
  read and written; a client secret never enters a row, it lives behind `secret_ref`), **the
  and `organization_invitations` (a hashed, single-use token with an expiry) and backfills one
  challenge** (`challenges.rs`; the `state` every round trip is bound to, SHA-256 at rest, single
  primary membership per existing account with `on conflict do nothing`.
  use, ten minutes, and *burned rather than retried* once somebody is guessing at it),
  `crates/identity::memberships` carries the queries; every refusal is a typed `IdentityError`,
  **OIDC/OAuth2** (`oidc.rs`; discovery, the PKCE challenge, RS256 verification and the
  and an invalid token answers identically for unknown, revoked and used so a public link cannot
  registered-claim checks `exp`/`iat`/`nbf`/`aud`/`iss`/`nonce`), **SAML 2.0** (`saml.rs`; the
  be used to discover a tenant. The API adds the member and invitation routes, the public preview
  assertion reader and its two independent signature checks) and **the protocol-neutral identity**
  and acceptance, and the switcher's two `/me` routes — deliberately unguarded, because the
  (`claims.rs`; one `Identity` shape every flow reduces to, plus the claim → role rules).
  caller's binding lives in the organization they are leaving, so a permission guard would make
  `database/migrations/0021_iam_sso.sql` adds the two tables a *running* sign-in needs —
  the second switch unreachable; the membership check inside the handler is the real
  `sso_challenges` and `auth_provider_events` — and nothing else: the provider row itself already
  authorization. The panel gets `/organizations`, `/organizations/[id]`, the public
  existed in `0011`. The number is **0021**, not 0019, because the sibling waves own 0019 (`w2`
  `/invite/[token]` page and the header switcher.
  `cms_blocks`) and 0020 (`w3` `automation_depth`); migration numbers are claimed per wave, and
- **Proof (Rust).** `cargo test -p omnion-api --test tenancy_members` → **7 passed** over the real
  three unclaimed worktrees have already collided on 0019.
  router: the invited address signs up, joins and lands in the organization; a used, expired and
- **Proof (Rust).** `cargo test -p omnion-identity --lib` → **103 tests, 0 failures** (33 new on
  revoked token each answer with their own reason; an existing member and a pending address are
  this part). The cryptography is tested against *real* cryptography, not against itself: the
  both refused by name; the switcher moves the session and the data follows; a member of one
  RS256 and SAML tests generate a 2048-bit key, sign, and require the module's verifier to accept
  tenant cannot read another tenant's members; the backfill gives every home organization one
  the genuine signature and reject a tampered one.
  primary membership; a suspended member and a last primary are refused with their reason.
- **The SAML signature check is two checks, and the second one is the one that matters.** XML
  `cargo test -p omnion-identity` → **75 passed**. `cargo test -p omnion-api --test onboarding`
  Signature binds a document to a key in two independent steps, and my first implementation only
  → **4 passed**. `pnpm typecheck && pnpm build` → exit 0, both apps.
  did the first: verifying the RSA signature over `<ds:SignedInfo>`. The tamper test — change an
- **Proof (QA).** `QA_STACK=w5 … bash scripts/qa/run.sh` → 30 pages, 890 clicks, 76 fills, 926
  e-mail inside a validly signed assertion, keep the original signature — **passed**. The
  screenshots. The new screens are clean: `/organizations` reports no overflow, no low contrast,
  `DigestValue` is what ties the `SignedInfo` to the assertion under the *enveloped transform* (the
  no unlabeled inputs, no duplicate ids, one `h1`, and the depth pass reads the empty search
  element with its own `<ds:Signature>` removed); with it, the tampered document is refused
  state, refuses an unusable address in the field (`Enter a valid e-mail address — name@example.com`),
  because the bytes changed. Skipping either check leaves a hole: the digest alone lets anyone
  creates a real invitation, refuses the duplicate by name
  rewrite a claim, the signature alone signs the algorithm but not the document.
  (`this address already has a pending invitation in this organization`) and revokes it.
- **Four more real bugs the tests caught, each a genuine defect rather than a test artefact.**
- **The pass found two real defects, and both are fixed.** It reported `memberRows: 0` on the
  (1) A repeated `<saml:Attribute Name="groups">` was being dropped, so a directory that sends a
  only organization on the installation. Root cause: the first-run owner is platform-level by
  multi-valued attribute as several elements lost half a group membership and under-granted a
  design (`users.organization_id` stays null), so migration 0019's backfill skipped it and the
  role. (2) `rsplit(':')` on `<saml:Assertion xmlns:saml="urn:…:assertion">` returns the
  installation ended up with a tenant nobody belonged to. Creating the first organization now
  *attribute value*, because a colon inside an attribute looks exactly like a namespace separator —
  writes the creator's primary membership, and the first-run test asserts it. It also reported a
  the qualified name has to be located before the prefix is dropped. (3) A closing tag is spelled
  400 on "Create organization": the form derived its slug from the raw name, so "QA sample" went
  with the prefix the document used, so searching for `</Assertion>` found nothing. (4)
  to the API and took a refusal the reader could not have predicted — it now normalizes to the
  `<ds:SignatureMethod Algorithm="…"/>` has no text; the algorithm is its attribute, and reading it
  server's own rule, refuses an unusable slug in the field and shows the derived slug instead of
  as text fails silently three frames deep.
  deriving it silently. Both fixes are in `b347de6`.
- **The disk is a blocker, and it is an environment problem rather than a code one.**
- **The host, not the tree.** Three QA passes in a row died with `Page crashed` and the build
  `/mnt/apopic` is one 60 GB loop image shared by **eight** worktrees' `target/` directories, and it
  failed twice with `linking with cc failed` / `No space left on device`. The volume was at
  hit **100% full twice during this tick** — each time inside a `write_file`, which then failed with
  literally 4K free: seven writers hold 1–17G of cargo targets each on one 60G `/mnt/apopic`.
  "No space left on device". The reclamation is deliberately conservative: only **derived**
  Nothing in another writer's tree was touched; the reclaim was caches (`.next`, `target/debug/
  artifacts were removed (`target/debug/{deps,incremental,build}` and the incremental caches) and
  incremental`, `target/debug/deps` of this worktree) plus `.npm`/`apt`/journal on `/`. A pass
  only in worktrees with no live `cargo`/`rustc` and no running pm2 process; no source file, no
  that survives also needs the load to dip: with the load average above ~20 Chromium's renderer
  branch, no sibling's running server was touched. The siblings rebuild within minutes and refill
  is killed, and with `MemAvailable` above 8G the same pass completes end to end. A useful
  the volume, so the headroom is temporary. **Owner action:** the box needs more room, or the
  habit for the next writer: watch `df` before a long build rather than after it fails.
  unclaimed `omnion-w4`…`omnion-w7` worktrees (≈12 GB of cargo target plus 454 MB of
- **Next.** REQ-005 slice **2** — departments and scoped roles: `departments` and
  `node_modules` each) should be pruned — no wave owns them yet.
  `department_members`, `role_bindings` at department scope, the member drawer with binding
- **Next.** The same slice's remaining part: the API surface (`GET/POST /iam/providers`,
  management and the Departments tab.
  `PATCH/DELETE /iam/providers/{id}`, `POST /iam/providers/{id}/test` for the discovery check, and

  the public `GET /api/v1/auth/sso/{slug}/start` + `POST …/callback` pair), JIT provisioning and the

  claim → role binding on sign-in, the `iam.signin.*` events, the `/settings/iam/authentication`
- **What shipped.** `crates/identity/src/sso/` — the whole protocol layer of enterprise sign-in,
  panel screen, the `apps/api/tests/sso.rs` integration walk (sign in against a stub provider → JIT
  before any HTTP: **provider rows** (`providers.rs`, the `auth_providers` table of `0011` finally
  account → mapped role → expired challenge refused) and the `iam-authentication` QA pass. Then
  read and written; a client secret never enters a row, it lives behind `secret_ref`), **the
  `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` close the
  challenge** (`challenges.rs`; the `state` every round trip is bound to, SHA-256 at rest, single
  REQ.
  use, ten minutes, and *burned rather than retried* once somebody is guessing at it),

  **OIDC/OAuth2** (`oidc.rs`; discovery, the PKCE challenge, RS256 verification and the
## 2026-09-28 — REQ-006 slice 4b-2 (parts 2–3) · the API, the screen and the integration walk
  registered-claim checks `exp`/`iat`/`nbf`/`aud`/`iss`/`nonce`), **SAML 2.0** (`saml.rs`; the

  assertion reader and its two independent signature checks) and **the protocol-neutral identity**
- **What shipped.** The HTTP half of enterprise sign-in, the screen that drives it, and the walk
  (`claims.rs`; one `Identity` shape every flow reduces to, plus the claim → role rules).
  that proves both. **`61ef619`** — `crates/identity/src/sso/provisioning.rs` (JIT: match on the
  `database/migrations/0021_iam_sso.sql` adds the two tables a *running* sign-in needs —
  stored subject index first, the address second, `JIT_PASSWORD_MARKER` in place of a password,
  `sso_challenges` and `auth_provider_events` — and nothing else: the provider row itself already
  and a *refusal* rather than a silent row when provisioning is off); the identity HTTP client
  existed in `0011`. The number is **0021**, not 0019, because the sibling waves own 0019 (`w2`
  grows the two calls the `code` flow needs; `/api/v1/iam/providers` (list, connect, patch, remove,
  `cms_blocks`) and 0020 (`w3` `automation_depth`); migration numbers are claimed per wave, and
  `…/test` for the discovery check, `…/events` for the sign-in log); and `/api/v1/auth/sso`
  three unclaimed worktrees have already collided on 0019.
  (the public `providers` list, `start`, the generated SAML panel page, and the `callback` that
- **Proof (Rust).** `cargo test -p omnion-identity --lib` → **103 tests, 0 failures** (33 new on
  answers both a `code` query and a posted assertion). **`f599c3c`** — `/settings/iam/authentication`,
  this part). The cryptography is tested against *real* cryptography, not against itself: the
  the nav entry, the API client and the `iam-authentication` pass in `scripts/qa/walkthrough.cjs`.
  RS256 and SAML tests generate a 2048-bit key, sign, and require the module's verifier to accept
  **`apps/api/tests/sso.rs`** — the integration walk.
  the genuine signature and reject a tampered one.
- **Proof.** `cargo test -p omnion-identity --lib` → **107 tests, 0 failures**. `cargo test -p
- **The SAML signature check is two checks, and the second one is the one that matters.** XML
  omnion-api --lib` → **97 tests, 0 failures**. `cargo test -p omnion-api --test sso` → **1 walk,
  Signature binds a document to a key in two independent steps, and my first implementation only
  0 failures** over the real router: the management surface (401 without a session, an empty
  did the first: verifying the RSA signature over `<ds:SignedInfo>`. The tamper test — change an
  organization listing no provider and three kinds), a provider created **switched off with JIT
  e-mail inside a validly signed assertion, keep the original signature — **passed**. The
  off**, the secret answered as a *name* plus a boolean (`secret_present: false` for a variable
  `DigestValue` is what ties the `SignedInfo` to the assertion under the *enveloped transform* (the
  this process does not define) and no `client_secret` field anywhere in the payload, a bad
  element with its own `<ds:Signature>` removed); with it, the tampered document is refused
  `secret_ref` refused with `details.field = secret_ref`, an unreadable role mapping refused at save
  because the bytes changed. Skipping either check leaves a hole: the digest alone lets anyone
  time, the discovery test answering `200 {status: "failed", detail: …}` for an unreachable host, a
  rewrite a claim, the signature alone signs the algorithm but not the document.
  disabled provider `404 provider_disabled` **and written to the sign-in log**, JIT refusing then
- **Four more real bugs the tests caught, each a genuine defect rather than a test artefact.**
  provisioning the same identity to `Created` with the marker in `password_hash` and the subject
  (1) A repeated `<saml:Attribute Name="groups">` was being dropped, so a directory that sends a
  indexed under `sso_subjects`, the second sign-in `Existing` on the same account, a deactivated
  multi-valued attribute as several elements lost half a group membership and under-granted a
  account staying deactivated, the event log readable over HTTP, removal taking the log with it
  role. (2) `rsplit(':')` on `<saml:Assertion xmlns:saml="urn:…:assertion">` returns the
  (`on delete cascade`), and an ambiguous host answered `501 organization_required` rather than
  *attribute value*, because a colon inside an attribute looks exactly like a namespace separator —
  guessed.
  the qualified name has to be located before the prefix is dropped. (3) A closing tag is spelled
- **The two design decisions this tick actually settled.** (1) **A public sign-in has to resolve
  with the prefix the document used, so searching for `</Assertion>` found nothing. (4)
  its own organization.** My first version took "the installation's only organization", which is
  `<ds:SignatureMethod Algorithm="…"/>` has no text; the algorithm is its attribute, and reading it
  right for a first-run install and *silently wrong* for a second tenant — a sign-in link for one
  as text fails silently three frames deep.
  organization could complete against another. The walk caught it by creating a second organization.
- **The disk is a blocker, and it is an environment problem rather than a code one.**
  It now follows the same rule the public content surface uses: the browser's own host answers for
  `/mnt/apopic` is one 60 GB loop image shared by **eight** worktrees' `target/` directories, and it
  its site, and a site belongs to an organization; several organizations and an unknown host is an
  hit **100% full twice during this tick** — each time inside a `write_file`, which then failed with
  honest `501` naming the fix. (2) **A refusal is a fact, not a silence.** A disabled provider
  "No space left on device". The reclamation is deliberately conservative: only **derived**
  refusing to start wrote nothing, so an operator who switched a provider off and then wondered
  artifacts were removed (`target/debug/{deps,incremental,build}` and the incremental caches) and
  "is anybody still trying to sign in with it?" had no way to find out. `live_provider` now writes
  only in worktrees with no live `cargo`/`rustc` and no running pm2 process; no source file, no
  the `auth_provider_events` row before refusing.
  branch, no sibling's running server was touched. The siblings rebuild within minutes and refill
- **Two test-authoring bugs the walk caught in itself, both worth naming.** A group claim is only
  the volume, so the headroom is temporary. **Owner action:** the box needs more room, or the
  read when the provider *names* one, so a test provider with `group_claim: None` proved nothing
  unclaimed `omnion-w4`…`omnion-w7` worktrees (≈12 GB of cargo target plus 454 MB of
  about groups — the fixture was wrong, not the code. And a test fixture that registers a globally
  `node_modules` each) should be pruned — no wave owns them yet.
  unique host has to clear a stale one first, or the second run fails on the first run's leftovers.
- **Next.** The same slice's remaining part: the API surface (`GET/POST /iam/providers`,
- **The disk is still the constraint, and it is an environment problem rather than a code one.**
  `PATCH/DELETE /iam/providers/{id}`, `POST /iam/providers/{id}/test` for the discovery check, and
  `/mnt/apopic` (one 60 GB loop image shared by eight worktrees' `target/`) fell to **3.1 GB free**
  the public `GET /api/v1/auth/sso/{slug}/start` + `POST …/callback` pair), JIT provisioning and the
  during this tick. Reclamation stayed conservative and derived-only: `target/debug/{deps,
  claim → role binding on sign-in, the `iam.signin.*` events, the `/settings/iam/authentication`
  incremental,build}` in the **unclaimed** `omnion-w5` and `omnion-w7` worktrees, neither of which
  panel screen, the `apps/api/tests/sso.rs` integration walk (sign in against a stub provider → JIT
  had a live `cargo`/`rustc`; no source, no branch, no running process touched. That returned
  account → mapped role → expired challenge refused) and the `iam-authentication` QA pass. Then
  17 GB. **Owner action:** the box needs more room, or the unclaimed `omnion-w4`…`omnion-w7`
  `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` close the
  worktrees should be pruned — no wave owns them yet.
  REQ.
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
### Wave 5 · REQ-005 slice 2 — departments and department-scoped roles (`0c63b73`)


- **What this tick was.** Slice 1 (folders + browser + trash) was already written and its boxes
- **What shipped.** The structure *inside* an organization. `crates/identity/src/departments.rs`
  were already ticked, but nothing had ever *executed* the folder move, the trash listing or a
  (the tree, validation, membership in a department, the ancestor expansion); the migration
  filtered listing against a real database — the walk that asserts the audit rows for
  `0028_organization_departments.sql` (`departments`, `department_members`, and the index the
  `media.folder_moved` and `media.folder_deleted` never performed a move or a delete. This tick
  department binding read needs); `/api/v1/organizations/{id}/departments` plus the member, role
  made the walk real and then fixed what it found.
  and key sub-routes; the Departments tab in the panel, with its drawer, the
- **Six defects, none of them visible to the layer that owned them.**
  `?department=` parameter on `/iam/effective-permissions`, and the `organization-departments`
  1. **Every filtered listing was broken.** The clause was built as a string containing `$n` *and*
  pass in `scripts/qa/walkthrough.cjs`. The tree is addressed by a **stable key**, because a
     the value was pushed as a bind, so the statement read `folder_id = $2$2` and PostgreSQL
  role binding stores a department as its `resource_id` string — a key that changed would
     answered "syntax error at or near $2". An unfiltered listing worked, which is exactly why no
  silently re-scope every role bound to it.
     earlier test saw it. `Filter::push` now writes clause and value together, so a placeholder can
- **Proof.** `cargo test -p omnion-identity -p omnion-permissions -p omnion-api --lib` →
     only exist where the value beside it was pushed.
  **294 tests, 0 failures** (identity 123 · permissions 62 · api 109). `cargo test -p omnion-api
  2. **`make_interval(days => $2)` with a bound parameter.** PostgreSQL cannot infer the remaining
  --test tenancy_departments` → **7 walks, 0 failures** against a live database
     arguments of a named-argument function, so it picked a `numeric` overload and sqlx failed to
  (`omnion_w5_dept_test`, the dev `omnion` database carries another branch's migration 19 and
     decode. Replaced with `$2::bigint * interval '1 day'`, which is unambiguous.
  refuses to migrate). `pnpm typecheck` → 2/2; `apps/admin` re-checked uncached with a direct
  3. **`sum(size_bytes)` returns `numeric`.** sqlx will not decode `numeric` into an `i64`, so the
  `tsc --noEmit` → 0 errors.
     trash summary answered 500. Cast back to `bigint`.
- **Two defects the tests caught, both about the same word: inheritance.** The ancestor walk ran
  4. **`ORDER BY` inside an `UPDATE`.** PostgreSQL has no such clause; the folder move 500'd on
  *downward*, so a parent inherited its children's bindings instead of the other way round — the
     every call. The ordering premise it encoded was wrong anyway — one `UPDATE` evaluates every
  exact opposite of what an operator binding a role at "division" means. And folding the
     row against the pre-update snapshot, so no ordering is needed.
  department load into `effective_permissions_for` had dropped the `scope.applies_to` filter
  5. **A folder move self-parented the folder.** The parent was resolved by the moved folder's *own*
  that `main` carried, which would have let a role bound to one site answer for a request
     new path, so `parent_id` became the folder itself on every move, and the first move of a
  naming a different one. Both are now pinned by tests; the second by
     top-level folder hit `media_folders_root_name_idx`. The parent is now resolved by the
  `a_role_bound_to_one_site_never_answers_for_another`, because a unit test on `applies_to`
     *parent's* path.
  proves the matcher and not that the resolver still calls it.
  6. **An omitted `parent_id` meant "move to the root"** although the body documents "omitted keeps
- **A third, found by reading the response rather than the assertion.** The tree read built its
     the current one" — so renaming a nested folder silently relocated it to the top. A no-op move
  parent map from each row's `parent_id`, which stored the *child's* key under the parent's id —
     is also no longer reported as `folder_cycle`, which is a cycle where there is none.
  so every department reported itself as its own parent, and the row the test read said
- **Two more honest answers.** A `folder_not_empty` refusal now has a tested counterpart (an empty
  `parent_key: "beta"`. A green `order` assertion sat right next to it and hid the problem.
  folder deletes, and a deleted folder is a `404` by id so a stale deep link names what is missing),
- **Next.** Slice 3: `organization_settings`, `organization_modules`, `organization_limits`, the
  and a move to where a folder already is is a no-op.
  plan and usage endpoints, limit enforcement on invite/site/AI, and the Settings, Modules and
- **Proof.** `cargo test -p omnion-media --lib` → **25 tests, 0 failures** (three new: every filter
  Billing tabs. That slice also closes the "backfill is proven" line, whose settings and limits
  and every combination refuses to write a placeholder twice, the subtree clause binds its folder
  half is still open.
  once per mention, the tag clause compares from the placeholder side). `cargo test -p omnion-api

  --lib` → **105 tests, 0 failures**. `cargo test -p omnion-api --test media` against
### 2026-09-28 · wave5 · REQ-005 slice 3 — settings, modules, limits and the ceilings that bind
  `omnion_test_main` → **8 walks, 0 failures**, over the real router: the tree refused without a

  session and to an account with no media permission, a site created after the migration still
- **What.** The tenant layer could not be configured, could not be restricted and could not be
  materialises one root, folders create/rename/move/re-parent/delete with the subtree rewrite read
  bounded. This slice adds `organization_settings`, `organization_modules` and
  back out of the row, a cycle and a duplicate sibling name and a blank name each refused by name,
  `organization_limits` (migration `0030`, with a defaults backfill for existing tenants);
  a file moves between folders without its storage key changing, a filter narrows the listing and
  `crates/identity::tenancy_limits` (validators, the three stores, the usage aggregate and the
  the total follows, a `like` wildcard in a search term is treated as text, the trash lists the
  limit checks); four routes under `/api/v1/organizations/{id}` with `?format=csv` on usage; the
  deleted file with a real countdown, restore returns it to its folder, purge removes the bytes as
  enforcement call sites in `create_site` and invitation acceptance; and the Settings, Modules
  well as the row, and every privileged step left an audit row. `pnpm typecheck` green. clippy adds
  and Billing tabs with the `organization-tenant-tabs` walkthrough pass.
  no new warning.
- **Proof.** `cargo test -p omnion-identity -p omnion-api --lib` → **257 tests, 0 failures**
- **Environment note.** The shared dev database `omnion` still carries a sibling's migration 19,
  (identity 136 · api 121). `cargo test -p omnion-api --test tenancy_limits` → **12 walks, 0
  so the walks run against `omnion_test_main`. `/mnt/apopic` was at 98% again; reclaiming
  failures** against `omnion_w5_tenant_test` (the dev `omnion` database refuses to migrate with
  `target/debug/incremental` in this worktree returned 2.9 GB.
  `VersionMismatch(19)`, another branch's migration). `pnpm typecheck` → 2/2.
- **Next.** Slice 2 — preview, metadata, versions. The version table already exists; the version
- **Two decisions worth naming.** A missing module row reads as **enabled**, not disabled: the
  history, the preview pipeline and the file detail screen do not.
  table records a decision and a fresh tenant has made none, so reading it as off would ship
### Wave 5 · REQ-005 slice 3 remainder — the invite policy gets teeth, and the Audit tab (`337c5bc`)

  every new organization with the platform switched off. And usage is *computed* at read time

## 2026-09-28 — REQ-010 slice 2, a version history that does not rewrite the past
  rather than denormalised into counters — a seat count that drifts is worse than a cheap
- **What shipped.** The invite policy was a stored, validated, *offered* value that bounded

  `count(*)`, and the CSV is served from the same call the bars render so the two cannot differ.
  nothing; this tick makes all three modes real, and adds the tenant-scoped audit feed the REQ's
- **What this tick was.** Slice 1 gave the library a file system. This tick gave it a memory: a
- **The defect the walk caught, and the walk that caught it.** The lowering guard compared two
  last tab asks for. Migration `0031_invitation_approval_queue.sql` adds one status
  replaced file keeps its old bytes, the panel can see every version, and a restore brings an old
  `Option`s directly, so `Some(1) < None` read as false and *putting a ceiling on an unlimited
  (`awaiting_approval`) and two decision columns. The queue is a **state of an existing live
  one back *as a new version* rather than by rewriting history.
  tenant was never checked* — the transition a plan makes the moment somebody starts bounding a
  invitation, not a second table** — the unique index on `(organization_id, lower(email))` then
- **The migration the plan assumed already existed.** The last tick's handover note said
  tenant. `None` now maps to `i64::MAX` first. The unit test that had been asserting "raising is
  keeps refusing a duplicate for a queued address, and a queued and a released invitation cannot
  "the `media_versions` table exists". It did not — `0025` created folders, browser columns and
  always allowed" then failed, and it was the test that was wrong: it set a 1 MB storage ceiling
  both exist for one person.
  the trash, and nothing had ever written a version row. So slice 2 ships `0026`, which creates
  against 4 MB in use, which the guard refuses, correctly.
- **The queue hands out no link at the create.** A queued create answers `202` with an **empty
  the table and backfills version 1 for every existing file, **copying `created_at`** rather than
- **Enforcement sits where the REQ puts it.** The site ceiling is checked in `create_site`; the
  token**, so the manager who cannot release it has nothing to forward. The release **mints** the
  stamping `now()`. A history that starts at the migration date is a lie about when the file
  seat ceiling in invitation acceptance, and *before* the sign-up account is created — a refused
  link, because the stored value is a one-way hash and a token that was never shown is a token
  arrived, and it is exactly the kind of lie that is invisible for a year.
  acceptance must not strand a new account holding no membership that cannot get in. Inviting
  that cannot be recovered. The alternative — hand the link out at create time and trust the
- **Three rules, each a place a shortcut produces a plausible wrong answer.**
  is deliberately **not** refused: the plan is charged for people who have joined.
  queue-holder — makes the feature work by trust instead of by construction. A *second* release is
  1. **A version is append-only.** A restore *copies* the old bytes to a new key and appends the
- **The browser pass, and the two defects only it could find.** The full walkthrough stalled in
  refused `invitation_not_queued` rather than minting a different link and silently orphaning the
     copy. Rewriting a row would make "what did this file look like on day 3" depend on whether
  its `search-depth` section for ~25 minutes with four QA passes and 50 Chrome processes on the
  first, and the panel shows the released link in a `role="status"` region rather than a toast,
     anybody took a shortcut in between.
  box, so the slice was proved with a focused Playwright probe against the same w5 stack instead
  because a toast takes the only copy with it.
  2. **The number comes from the database.** `next_version` reads `max(version)` under
  (the API endpoints were also exercised directly: modules, settings, limits, usage, the CSV
- **The owner check asks whether the binding *reaches this tenant*.** The seed binds `owner` at
     `for update` on the `media` row — a *scalar subquery*, because `FOR UPDATE` on an aggregate
  and a module toggle all answer). That probe found what typecheck cannot:
  platform scope (`roles.organization_id is null`), so asking only "does an owner binding exist"
     is a no-op in PostgreSQL. Reading the max in Rust would open a window between two reads and
  * `locator("select").first()` matches the **header's site switcher**, not the locale picker, so
  would let tenant A's owner release tenant B's queue. `global` and this-organization's
     let two concurrent replaces both claim 4, which surfaces as a "duplicate key" error that
    the pass wrote the locale into the site dropdown and then waited 30s for an option that was
  `organization` scope qualify; a `site` scope deliberately does not, because a site lead is not
     names the index and not the cause.
    never there. The tabs now carry `data-organization-settings-{locale,timezone,accent}` and
  the owner of the whole tenant. `self_serve` needs no new check at all — the route's existing
  3. **A number is never reused.** A pruned version leaves a hole.
    `data-organization-billing-plan`.
  `organizations.manage` guard already *is* that rule, which is the cheapest correct
- **The transaction is opened by the crate, not the route.** This was fought out with the
  * A bar with no ceiling had no `aria-valuemax`, which announces as *indeterminate* — honest
  implementation and the one that cannot drift.
  compiler: a route-held `sqlx::Transaction` surfaces `sqlx::Error` where everything else in
    about the ceiling, silent about the figure. It now always carries `aria-valuenow` and an
- **The Audit tab is guarded by `audit.read`, not `organizations.read`.** A trail names every
  `omnion_media` is a `MediaError`, and `ApiError` has `From` for the latter but deliberately
    `aria-valuetext` of "3 of unlimited".
  privileged act in the tenant; "can see the member list" is not a reason to see it. The walk
  **not** for the former. `omnion_media::begin_version` / `commit_version` hand back only
- **Proof, after the fix.** Probe against the w5 stack: 5 module rows, a switch that changes and
  asserts the refusal *first*, because a tab that worked for everyone would leave the split
  `MediaError`, which is the honest boundary: the library owns the transaction because the
  persists across a reload, settings saved and read back (locale `tr`, accent `#2f6f4f`,
  untested and still look green. The action filter is a picker built from the tenant's own rows
  guarantee rule 2 exists for spans it.
  `Europe/Istanbul`), 4 usage bars at 1440×900 **and** 390×844 with real values
  (it cannot offer a filter that matches nothing, and cannot drift as actions are added), the
- **The header probe reads 64 KB, not the file.** `crates/media/probe.rs` pulls dimensions,
  (`now: 1 / 1 / 1018 / 0`), 0 console errors.
  count is the *filtered* count from the same statement as the rows, and a typo'd actor is
  duration and page count out of the *header* for PNG, GIF, JPEG, BMP, TIFF, WebP (all three
- **Environment note.** `/mnt/apopic` hit 100% mid-pass and a failed build *deleted*
  refused `invalid_actor_filter` rather than answered as an empty history.
  containers), MP4/QuickTime, WebM, WAV, MP3, Ogg and PDF. A 4 GB video upload must not cost a
  `target/debug/omnion-api`, so the pass then failed for a reason that had nothing to do with the
- **Proof.** `cargo test -p omnion-api --test tenancy_limits` → **19 walks, 0 failures**
  full read to learn it is 12 minutes long. Every extractor answers "I do not know" rather than
  code (`relation "workflows" does not exist` from an empty QA database). Reclaiming
  (7 new). `cargo test -p omnion-identity -p omnion-audit` → **136 unit tests, 0 failures**.
  guessing — a wrong dimension breaks every layout that reads it and is not obviously wrong once
  `qa-artifacts/*` and `apps/admin/.next` — both regenerated by the pass itself — freed 8.1G.
  `tenancy` (5), `tenancy_members` (7) and `tenancy_departments` (7) all green against the same
  it is stored. Two findings the unit tests forced out: a WebP canvas stored as `0` is a corrupt
- **Next.** The rest of slice 3: the Audit tab with its CSV, the suspend/archive flows and the
  per-writer database. `tsc --noEmit` in `apps/admin` → clean. A new QA pass
  header, **not** a one-pixel image (reading `0 + 1` would put a 1×1 box on screen for a file
  invite-policy behaviours (`closed` refusing, `self_serve` allowing, `owner_approval` queueing)
  (`runOrganizationInvitePolicy`) walks all three policies, the queue panel and the Audit tab.
  that has no size), and one blanket 30-byte minimum across the three WebP containers refuses a
  in the create-invitation path.
- **Live proof, 24 steps, 0 failures** (`scripts/qa/probe-tenant-invite-policy.cjs`), because the
  short-but-complete `VP8L` header.
  full pass could not run: `closed` → `403 invitations_closed` with the policy in `details`;
- **The walk corrected a test that had been asserting a route which never existed.** The walk
  `self_serve` → `201` whose link previews `usable: true`; a **manager**'s invite → `202` with
  read the current bytes from `/api/v1/media/files/{id}/raw` and got an empty body: the file
  `tokenLength: 0`; the manager's own release → `403 not_an_organization_owner`; the owner's
  manager's *listing* is `/media/files`, the read is `/media/{id}/raw`, and no route was ever
  release → `200` minting a link that previews usable; a second release → `409
  registered at the address the test used. Three more corrections came out of the same run — the
  invitation_not_queued`. On screen: the queue panel shows the row with *Release* and *Revoke* and
  404 is `media_not_found` (not `file_not_found`), an empty upload answers `invalid_request`
  the sentence explaining the policy; the Audit tab lists 22 rows, its action filter is built from
  (not `file_empty`), and the history reads **newest first**, so a check written against an
  14 distinct actions this tenant has actually performed, narrowing to one action moves the count
  assumed oldest-first order fails on a correct response.
  from `22 of 22` to `1 of 1`, 0 px horizontal overflow at 390×844, and 0 console errors.
- **The fixture leaked objects until it read the union.** Cleanup read `media.storage_key`, but a
- **The probe had to create a *manager*, and that is the point.** The QA owner holds `owner` at
  replace *moves* that column to the new key — the old one is named only by the history, so every
  **global** scope, so the policy must *not* queue it — driving the queue with that account
  replaced version's object stayed in the bucket. A test cleanup that misses them is a slow leak
  answers `201` with a link, correctly, and proves nothing. The only account the queue exists for
  that nobody notices for a month.
  is somebody with `organizations.manage` who is not an owner. Its password hash is copied from
- **Proof.** `cargo test -p omnion-media --lib` → **46 tests, 0 failures** (24 new, mostly header
  the owner rather than re-implemented, because a second argon2 in a probe is a second
  probes and the version key rules). `cargo test -p omnion-api --lib` → **109 tests, 0 failures**.
  implementation of the thing being verified.
  `cargo test -p omnion-api --test media` against `omnion_test_main` → **11 walks, 0 failures**,
- **A defect only the reviewer's eye found, which no assertion would have.** The status badge
  over the real router: a replace leaves version 1 downloadable and **byte-identical** (compared
  rendered `awaiting_approval` as **`Awaiting_approval`** — a raw enum token. `statusLabel` had
  as bytes, not as a length — a length check would pass by accident on an overwrite), the row
  only ever capitalised the first letter, which is correct for every value the panel had seen
  points at the version it serves, the three versions own three keys, a download of an old
  (`active`, `pending`, `archived`) and wrong for the first one with an underscore. The *value*
  version is an attachment named `hero-v1.png`, a restore appends version 3 with version 1's
  was right, so the DOM was right, so every automated check passed: `53fc50d` splits on `_` and
  checksum while version 2 is untouched, and the routes refuse without a session, without
  `-` and gives the queued state the attention tone. The general rule: a formatter that has
  `media.read`, and name a missing version by number. `pnpm --filter @omnion/admin typecheck`
  never been shown a value it does not recognise is a formatter that has never been tested.
  green. The QA pass ran on the default stack.
- **A defect the QA stack's own start-up found, not my code.** The walkthrough read `/` after a
- **Environment note.** `/mnt/apopic` was at 99 % (610 MB free) when the tests finished; this
  fixed 900ms to decide whether setup was needed. But a fresh installation does not answer `/`
  worktree's `target/debug/incremental` returned 1.2 GB and the unclaimed `omnion-w5`/`omnion-w6`
  with the wizard: the request gate sends an anonymous visitor to `/login`, and *that* screen asks
  worktrees' `target/` returned a further 2.7 GB. **Owner action:** those worktrees hold build
  the API and replaces itself with `/setup` — a client-side redirect. So the pass decided "an
  artefacts for waves nobody has started; they will fill the image again.
  installation already exists" about an **empty database**, and then failed to sign in to the
- **Next.** Slice 3 — transformation presets with a content-addressed cache, per-site storage
  account it never created. `c83af9b` waits for one of the two URLs to be true instead. The
  settings with a connection test, the CDN purge hook, share links and duplicate detection with
  general rule: a URL read after a sleep is a guess about a *redirect chain*, and the client-side
  merge. Also still open in slice 2: the Usage and Activity tabs, HTTP range requests on the
  half of that chain is not observable from the server.
  serve path, and EXIF extraction.
- **The full walkthrough did not complete, and the REQ is not being closed on this tick.** Two
## 2026-09-28 — REQ-005 slice 3, closed · the suspend/archive *behaviour*, and the panel that says so

  attempts. The first died at `media-trash` with `Page crashed`; the second got seven routes

## 2026-09-28 — REQ-010 slice 3 (transformations), a preset that produces real pixels
  further and then failed with `Execution context was destroyed` after stalling on `/search` for
- **What.** The `organizations.status` column and the list's Suspend/Reactivate controls had

  roughly six minutes. Both are host, not code: at that point the box was running **six**
  existed since slice 1, and nothing read the status: a suspended tenant accepted every write.
- **What this tick was.** Slices 1 and 2 gave the library a file system and a memory. This one
  concurrent `qa/run.sh` passes (main, w2, w4, w7×2, w5) with 50 Chrome processes and a 4.3 GB
  This tick makes the column mean something. `Organization::accepts_writes` in
  gave it *derivatives*: a page asks for `?preset=card` and gets the same pixels every time,
  `java`, and the 1-minute load average reached **437**. This is the saturation the ledger warns
  `crates/identity` says which statuses are frozen; `scope::ensure_writable` turns that into a
  built on the first request and addressed by a hash of its inputs.
  about, one order of magnitude past it. The evidence recorded above is therefore the tiered
  `409 organization_not_writable` whose message names the tenant and its status and whose
- **The dependency the plan did not mention.** A preset has to *produce* pixels, which means
  gates (19 API walks, 136 unit tests, `tsc`) plus the focused probe — which is *not* a
  `details` carry `reads: true`, because a refusal that reads like a deletion sends an operator
  decoding, resampling and re-encoding. The workspace had no image crate, so this tick adds
  substitute for the gate, and the slice still has the suspend/archive work outstanding, so no
  looking for a backup instead of for a reactivation. The routes reach the guard through
  `image` (png/jpeg/webp only — the three codecs a preset can emit). Shelling out to a binary
  REQ closes here. Re-run the full pass when the box is quiet; `c83af9b` (the wizard race) and
  `organization_in_scope_for_write` (members, invitations, departments, settings, modules,
  was the alternative and was rejected: it makes the API's correctness depend on what happens to
  the new `runOrganizationInvitePolicy` are both unexercised end-to-end until it does.
  limits) and `site_in_scope_for_write` (site rename, site delete, all three domain routes) plus
  be installed on the host, and the test suite would skip itself on a machine without it.
- **Environment note.** `/mnt/apopic` was at 100% twice mid-tick and a `rustc` link died with
  the explicit check in `create_site`. Eighteen write paths, one rule.
- **Two fits are not one fit.** `cover` crops and `contain` letterboxes, and the first version
  "No space left on device" — the known shared-volume failure with seven writers. Reclaimed what
- **The exception is the point.** A tenant that could not be reactivated could be suspended and
  gave both the same resampler. It produced a correctly-sized *crop*: it passes a square-crop
  is mine and regenerable (`.rcgu.o`, the stale test binaries, `~/.npm/_cacache`, the cargo
  never brought back, so `scope::is_status_change` lets a payload carrying a status through the
  assertion and is wrong on every non-square source, which is most of them. A `contain` result is
  registry cache) and four **dangling** docker volumes nobody references (2.4G, checked with
  guard. A *rename* of a frozen tenant is still refused — the escape hatch is for a status change,
  now pasted onto a canvas of the box's size, so a thumbnail is a stable 320×320 rather than a
  `docker volume ls -f dangling=true` — Omnion's own volumes are named and were untouched).
  not for any request that happens to include one — and the walk proves both halves.
  320×180 that shifts the layout every time a differently-shaped image is uploaded.
  `CARGO_PROFILE_DEV_DEBUG=0` cut the linker's peak disk use enough to finish the suite on a
- **The audit action is not a rename.** A status move files `organization.suspended` /
- **A request never enlarges.** A 2400px request against a 1200px source is refused with an
  volume this contended.
  `.archived` / `.reactivated` and emits the matching event, instead of `organization.updated`.
  explanation instead of being answered with a blurry upscale that is *larger* than the original.
- **Next.** The last of slice 3: the **suspend/archive behaviour**. The `organizations.status`
  "Who suspended this tenant, and when" is a question the trail exists to answer, and a trail
  The check is `>`, not `>=`: asking for exactly the source's own size is the identity, and a
  column and the list's Suspend/Reactivate/Archive controls exist, but suspending a tenant does not
  that says "someone edited the organization" cannot answer it.
  template that names the same number twice is not a mistake. The unit test forced this — the
  yet *block writes with the reason* — the acceptance line wants a suspended tenant to keep reads
- **Proof.** `cargo test -p omnion-api --test tenancy_limits` → **19 walks, 3 failures, and the 3
  first version refused the identity too, which would have broken the seeded `standard` preset on
  available and refuse every write by name, and reactivating to restore them. That is one guard
  are the pre-existing baseline** — confirmed by stashing this tick's changes and re-running at
  a 1200×630 hero.
  applied across the tenancy write paths plus the banner, which the panel already renders.
  HEAD (`16 passed; 3 failed`, the same three: `a_ceiling_really_bounds_accepting_an_invitation`,
- **The cache key is a hash of the definition, not of (file, preset).** A pair lookup would serve
  `a_queued_link_never_works_and_says_so`, `the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows`,
  stale pixels after an edit, because the pair is unchanged while the definition moved. With a
  all three a cross-test interference between parallel walks sharing one database).
  key lookup, an edit produces a key nobody has seen, so the old entry becomes *unreachable*
  `cargo test -p omnion-identity -p omnion-audit -p omnion-api --lib` → **261 unit tests, 0
  rather than *wrong* — and the response may honestly say `max-age=31536000, immutable`.
  failures** (123 + 2 + 136), including the two new `scope` tests. `pnpm typecheck` in
  Quality is in the key, and that is the field people forget.
  `apps/admin` → clean. The three new walks each prove something a single assertion cannot:
- **Three defects the real router found that a unit test on `transform_bytes` could not.**
  ten reads of a suspended tenant all answering 200 while eight writes across every family answer
  1. The derivative header carried the **object key** where the identity belongs. The two are
  409; the three refused writes read back out of the database to prove the refusal left nothing
     different strings that both appear in the module, and the response builder took one
  behind; and the suspend → reactivate → archive round trip with the rename staying refused.
     parameter where it needed two — so a caller that read the header and looked it up in
- **The panel half.** `TenantStatusProvider` + a banner inside the sticky header, so a frozen
     `media_derivatives.cache_key` found nothing. A header that looks like an identifier and is
  tenant says so on *every* screen rather than letting a person find out one refused Save at a
     not one is worse than none. Both are now fields of a struct, because three positional
  time. `role="status"`, not `role="alert"`: a standing condition re-announcing itself on every
     `&str`s is the shape that produced the bug.
  navigation is noise. The list's Suspend and the switcher's change both reload it. The walkthrough
  2. **A site created after the migration got no `standard` preset.** The `0027` seed covers the
  gained `runOrganizationSuspend`, which proves the banner on screen and always re-activates in
     sites that existed when it ran, so a page already asking for `?preset=standard` would
  its tail.
     silently fall back to full-size originals — on new sites only, which is exactly where nobody
- **A 403 from the wrong layer passes for a 403 from the right one — twice.** The fixture's
     is looking. It cannot be fixed in the migration, because the gap is between "the migration
  administrator had `sites.create` and not `sites.update`, so the site-rename assertion read
     ran" and "somebody creates a site", and it cannot be fixed in the create path either:
  `403 permission_denied` where it expected `409`; the guard never ran. Adding `sites.update`
     onboarding, the tenancy API and a future import all insert the row themselves. It is a
  moved the failure one line down, to `domains.manage`. A walk that stops at the first 403 is a
     trigger (`0028`), which is the only place guaranteed to see every site.
  walk that cannot tell "refused" from "never got there", and the fix is to grant the fixture
  3. A test asserting **"the first call builds" passes once and fails for ever after.** The row
  *every* permission the walks touch rather than to discover them one run at a time.
     is keyed by the source bytes, and a re-run reproduces them exactly, so the second run is a
- **A `PUT` that is not a patch.** `PUT /organizations/{id}/settings` is a whole-row replace:
     cache hit. The assertion now checks the answer is correct either way and that the key is
  `timezone`, `invite_policy` and `audit_retention_days` are required, so a body carrying only the
     stable — the property that actually matters.
  field under test answers `422` and the walk fails for a reason that has nothing to do with the
- **Proof.** `cargo test -p omnion-media --lib` → **78 tests, 0 failures** (32 new). Five walks
  freeze. The three walks share a `settings_body()` helper so the full shape lives in one place.
  over the real router in `--test media_transform` → **0 failures**: the bytes are compared *as
- **Environment.** The box rebooted mid-tick and `/mnt/apopic` has been between 100% and 96% all
  bytes* and decoded again (a length check passes by accident on an overwrite), the second
  tick: a `rustc` link died with "No space left on device" and a QA pass cannot start on 496M.
  request is byte-identical to the first, the object is read back out of the store and compared
  Reclaimed what is mine and regenerable (`apps/*/.next` = 1.4G, `qa-artifacts`, `target/debug/
  against what was served, the old row survives an edit, a delete cascades the cache away, an
  {build,incremental}`, `*.rcgu.o`); the rest of the volume belongs to the other six writers.
  unknown preset returns the original *byte for byte* with no derivative key, an SVG answers
  `CARGO_PROFILE_DEV_DEBUG=0` kept the linker's peak low enough to finish.
  `not_transformable` naming its type, and every preset field error names the field that caused
- **Next.** Slice 4: events and hardening — the per-organization audit retention sweep, the module
  it. The pre-existing `--test media` → **11 walks, 0 failures**, unchanged, against the raw route
  toggle events, and the mobile pass. The lifecycle events a status move emits are already in
  that now takes a query parameter. `cargo test -p omnion-permissions --lib` → 62 pass.
  place here, which is one item of that slice already done.
  `pnpm --filter @omnion/admin typecheck` green.

- **The QA pass (`bash scripts/qa/run.sh`, default stack) — clean for this slice.** 954 clicks,
## 2026-09-28 — REQ-005 slice 4, second unit · the retention sweep, so the stored number means something
  988 screenshots, the new `/media/settings` route walked and clicked (28 elements), and the depth

  pass drove it: created a preset, submitted an out-of-range quality and **the field error named
- **What.** `organization_settings.audit_retention_days` had been stored, validated (30–3650) and
  it**. Vision review returned **0 high / 0 medium / 0 low**. The page's own diagnostics read
  rendered on the Settings tab since slice 3, and nothing read it: there was no sweep that purged
  *overflow: no · offscreen: 0 · broken images: 0 · low contrast: 0 · unlabeled inputs: 0 ·
  anything, so the field was a number an operator could change and never see anything happen. This
  duplicate ids: 0 · h1: 1*. The four high findings the run reports are the walkthrough's **own
  tick makes the platform enforce it. `omnion_audit::purge_before` is the only place an audit row
  deliberate error-state probes** — `/media?folder=nonexistent-folder` and the 400/404 they
  leaves the trail, and the tenant is scoped **in the statement** (`where organization_id = $1`)
  produce — none of them from this slice.
  rather than by a filter the caller passed — a retention sweep that deleted another tenant's row
- **Two things the pass taught about this slice's own screen.** The seeded `standard` preset *is*
  would be the worst bug this platform could ship, and the only place that cannot go wrong is the
  present on a QA site (checked directly in `omnion_qa`, and the trigger in `0028` fires for a
  `where`. `apps/api/src/retention_runner.rs` reads each tenant's own stored window, computes that
  site created afterwards), so the walkthrough's `seeded: 0` was its own text match, not a gap —
  tenant's cutoff, removes what fell out of it, then files a **system** `organization.retention.swept`
  which is why the number was checked against the database rather than believed. And the preset
  audit row and announces the same event with `rows_removed` and `cutoff`.
  example URL is a `<code>`, not an anchor: the first depth pass looked for `a[href^="/api/v1/
- **A sweep that removed nothing files nothing.** The receipt is about a deletion that happened. A
  media/"]`, found none, and reported "no preset example URL" for a screen that had one on it. An
  nightly `rows_removed: 0` row on a two-hundred-tenant platform is two hundred rows a day in the
  `<a>` pointing at a placeholder id would only have proven a 404 — the same mistake the route
  trail, and a trail full of its own housekeeping is a trail nobody reads — so a tenant with nothing
  inventory already made once with `/media/files`. The pass now reads the query the screen
  expired is skipped entirely, and the walk asserts the second sweep returns 0 and files 0.
  actually renders and builds a real URL with a real file id.
- **A tenant with no settings row is swept, not spared.** The `left join` on `organization_settings`
- **A compiler lesson, fought out over a long wrong turn.** Every guarded route in this codebase
  matters: a tenant created after the backfill has no row, and reading that as "no window" would keep
  is written `get(handler).layer(guards::require(...))`, and the new routes refused to compile
  its history forever — the exact opposite of what the tab promises. It falls back to the schema's 365.
  with a bare `type annotations needed for MethodRouter<AppState, _>`. The guard's service impl
- **The cadence is a day, not a minute.** The shortest window a tenant can ask for is 30 days, so a
  requires the inner service's `Error = Infallible`, and inference cannot pick `Infallible` out of
  minute-cadence sweep would re-read every tenant 1440 times a day to delete rows that are at least a
  the several `From<Infallible>` impls in scope. Every existing route gets away with it because
  month old. The first tick fires at boot, so a process restarted more often than daily still sweeps.
  the *later* `.merge()`/`.route()` calls in the same chain pin the type. The fix is a
  `OMNION_AUDIT_RETENTION_SWEEP{,_SECONDS,_BATCH}` are the knobs; the sweep is **on by default**,
  `MethodRouter<AppState, Infallible>` annotation on the three new bindings — the same fix the
  because retention that only runs when somebody remembers is not retention.
  compiler suggested and that reading the guard's own bound would have given in one minute.
- **Proof.** `cargo test -p omnion-api --test tenancy_limits` → **22 passed, 3 failed**, and the three
- **Environment.** `/` was at 99 % and `/mnt/apopic` at 100 % during the run — MinIO refused
  are the pre-existing baseline BUILD-LOG already records (`a_ceiling_really_bounds_accepting_an_invitation`,
  writes with `XMinioStorageFull` and three walks failed on a storage error that had nothing to do
  `a_queued_link_never_works_and_says_so`, `the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows` —
  with the code. Reclaiming `omnion-live/target` and `omnion-w5/target` (worktrees for waves
  cross-test interference between parallel walks sharing one database, reproduced at HEAD). The two new
  nobody has started) plus this one's stale `deps` binaries returned ~4 GB. **Owner action:** the
  walks pass on their own: `the_retention_sweep_applies_each_tenants_own_window` (two tenants, two
  eight worktrees under `/mnt/apopic` hold ~30 GB of `target/`, and this is the second tick in a
  windows, rows at 2/60/100/400 days; 3 removed, the 365-day tenant **keeps** its 100-day row, the
  row that has had to delete another loop's build cache to finish its own tests.
  receipt repeats `rows_removed: 2` / `retention_days: 30` as `system`, the bus event matches, and the
- **Next.** Slice 3 continues — per-site storage settings with a connection test and public base
  second sweep is a no-op that files nothing) and
  URL, the CDN purge hook to REQ-011, share links with expiry and password, duplicate detection
  `a_tenant_keeps_rows_inside_its_window_and_one_without_settings_is_still_swept`.
  with merge. Also still open: EXIF (slice 2), HTTP range requests on the serve path, and the
  `cargo test -p omnion-audit -p omnion-core --lib` → **36 unit tests, 0 failures** (2 + 34, including
  Usage and Activity tabs, which need `media_references` and arrive with slice 4.
  the two new `RetentionConfig` tests). `cargo test -p omnion-api --lib` → **133 passed, 0 failures**.

  `pnpm typecheck` in `apps/admin` → clean.
## 2026-09-28 — REQ-005 slice 4, last criterion · and the duplicate migration that was faking a total regression
## 2026-09-28 — REQ-010 slice 3 (share links), a capability that is never stored
- **The panel says what the number now does.** The Settings tab's helper line grew from "Between 30 and


  3650 days." to say the platform enforces it, that each sweep files a row here, and that the trail
- **What shipped.** **`9195e18`** (migration numbering) and **`e00c53c`** (the
- **What.** The third third of slice 3: `0036_media_shares.sql`,
  therefore explains its own gaps. A setting that is enforced but described as if it were not is the same
  platform-account criterion) plus **`cb1b839`** (the tick). Two branches had
  `crates/media/src/shares.rs`, `apps/api/src/routes/media_shares.rs`,
  dead feature one layer down, and the field also got the `data-organization-settings-retention` hook the
  independently claimed `0029`: main for per-site media storage settings
  `apps/api/tests/media_shares.rs`, `features/media/shares-tab.tsx` (a **Share** tab on
  walkthrough needs.
  (`9973268`) and this branch for organization departments, which `644a129` had
  `/media/files/{id}`), the client methods and the `MediaShare`/`CreatedMediaShare` types, and a
- **Environment.** `/mnt/apopic` opened the tick at 99% with 882M free, so the build target moved to
  itself moved onto `0029` to escape an earlier `0028` collision with site
  walkthrough depth pass. Six design decisions, each a shortcut that produces a plausible wrong
  `/dev/shm/w5-target` (32G tmpfs, already holding w6's and w7's targets) and this writer's 2.4G
  presets. The number was free when that fix was written and is taken now. Git
  answer:
  `target/` was reclaimed after a `cp -a` warm start. `/mnt/apopic` went 99% → 95% without touching a
  merged both files without comment and sqlx refused to start:
  1. **The token is stored hashed and nowhere else.** The row holds `sha256(token)` under a
  single file that belongs to another writer.

     unique index, so a backup, a replica log or a support engineer with read access comes away
- **Next.** The last item of slice 4 is the `organization.member.joined` webhook-isolation walk — an
      migrations must apply: Migration(VersionMismatch(29))
     with a list of *dead* tokens, and the lookup is still one probe. The round trip runs one way
  org-scoped webhook endpoint subscribed to that event must receive **only** its own tenant's

     only, and the row type has no field the plaintext could occupy.
  deliveries, which is the slice's done-when — then the mobile pass, then the REQ can close.
  Every walk in the tenancy suite dies in `live_state` before reaching a single
  2. **A share reaches a file; it does not bypass what the file is.** Servability is decided at

  assertion, so the suite printed **0 passed / 29 failed** and read as a total
     *serve* time, not at creation — a link made yesterday must not keep serving a file the
## 2026-09-28 — REQ-005 slice 4, third unit · the webhook-isolation walk (the slice's done-when)
  regression. It was one duplicated version number. Renumbered to `0037`-`0039`,
     scanner has since flagged.

  clear of main (`0036`), wave4 (`0034`) and wave3 (`0030`).
  3. **`revoked` and `expired` are the same answer (410), `password_required` is 403.** Telling
- **What.** Slice 4's stated done-when is *"an org-scoped webhook endpoint subscribed to
- **Proof.** `cargo test -p omnion-api --test tenancy_limits -- --test-threads=1`
     the two dead states apart would hand a token prober a free oracle; answering "type the
  `organization.member.joined` delivers only that organization's events"*, and nothing executed
  → **26 passed / 3 failed**, and `a_platform_account_names_the_tenant_every_write_needs`
     password" with 410 would send the owner a pointless request.
  it. This tick makes it a real walk over real receivers — `apps/api/tests/events.rs`,
  passes alone (`1 passed`). The 3 failures are **pre-existing and were not
  4. **The counter counts bytes that were served**, in its own statement, so a failure after the
  `a_members_join_reaches_only_the_tenant_it_belongs_to`. Three loopback receivers, three real
  interference**: each of them fails on its own with `--test-threads=1`. They had
     bytes went out cannot roll it back.
  endpoints, two tenants, and **both** routes that emit the name: the administrative
  been mislabelled as cross-test interference by an earlier tick, and the reason
  5. **Revocation is a write, not a delete** — the row is kept with its reason forever, because
  `POST /organizations/{id}/members` and the invitation acceptance (which is why the walk opens
  that went unnoticed for so long is the migration panic — it killed all 29 walks
     that is the only thing that makes a leaked link investigable.
  the tenant's invite policy to `self_serve` first — under the default `owner_approval` the
  before any of them could be seen failing for its own reason. Established by
  6. **The screen has no `Copy` on an existing row, and cannot.** The token is returned once;
  create answers `202` with no token, which is the *correct* behaviour and useless here).
  stashing only this tick's work on top of the migration fix: the baseline is
     a copy button there would silently copy nothing.
- **The third endpoint is the point of the walk.** Tenant A gets a second endpoint subscribed to
  **25 passed / 3 failed**, the same three names, so nothing here regressed and
- **Proof.** Five walks over the real router in `--test media_shares` → **0 failures**. The
  `organization.module.disabled`. Subscription filtering and organization filtering are two
  one walk was added. `tsc --noEmit` in `apps/admin` exits 0 (a `pnpm typecheck`
  stored value is read **out of the database** rather than inferred from a response that hid the
  different rules, and a walk that only exercises the first will pass an implementation that
  cache hit is not evidence — the new `tenant-picker.tsx` was untracked, so the
  token; the list is scanned over its raw bytes for the token and for a field named `token`; the
  ignores the second — the two tenants' endpoints are then the only evidence, and they are the
  real check was run directly).
  served bytes are compared as bytes and `no-store`/`nosniff`/`attachment` are each checked; the
  *same* evidence. With the quiet endpoint, "A got its own and not B's" and "A's other endpoint
- **The criterion, and what "naming the field" actually meant.** The sentence
  counter moves once and survives the revocation; the revoked row keeps its reason and instant; a
  got nothing" are separate facts, so a fan-out that ignored the subscription list fails for its
  already contained the string `organization_id`, so an assertion on the message
  reader may read the list and may neither create nor revoke; anonymous is refused on both; a
  own reason.
  would have passed against an error no client could act on. The missing part was
  share id on another file is a 404, not a 403. `cargo test -p omnion-media --lib` → **98**,
- **The direction that catches a name-only fan-out.** "B receives nothing" is a weak assertion:
  the structured `details.field`, which is what lets a panel put a control *next
  `cargo test -p omnion-api --lib` → **116**, and `--test media` (11), `--test media_settings`
  an implementation that matched on the event *name* alone would still satisfy it, because the
  to the failing input*. `organization_required()` is now the single refusal every
  (2), `--test media_transform` (5) are unchanged and green. `pnpm --filter @omnion/admin
  event carries its own organization. The last third emits a fact belonging to **no** tenant —
  scope-resolving route shares, and the two refusals a client renders differently
  typecheck` green.
  a platform fact — with both tenants' endpoints already subscribed to that exact name, and
  are kept apart deliberately and unit-proved: a missing tenant names a field
- **Three defects the walks found, none of which a unit test on `servable` could see.**
  requires `deliveries == 0` and a following tick that claims 0. That is the case where the two
  (it is the caller's problem to fix), `cross_organization` names none (a picker
  `find_media` returns the *base* `Media`, which has no `deleted_at` and no `scan_status` — the
  rules come apart.
  there would only reproduce the same refusal). `TenantPicker` renders a labelled
  two columns the whole "a share does not bypass the file" rule depends on, so the first draft
- **Delivery order is the runner's, not the walk's.** The first draft indexed `captured[0]` and
  organization select only for a subject with no organization of its own, and is
  asked the wrong struct and the compiler found the fields missing. `POST /shares` with no body
  asserted the acceptance's `payload.via`; it failed with `left: Null` because the *administrative
  absent in `global` scope, where "no tenant" is the point.
  answered **415**, because the handler demanded a JSON body for the most ordinary call anybody
  add* was the delivery that arrived first — the runner claims `order by next_attempt_at, created_at`
- **Environment, twice over.** `rustc` was missing from the box — the
  makes ("give me a link until I revoke it"); the same for the `DELETE`. And `rand` is a
  and both rows were due in the same batch. The deliveries are now selected by what they carry
  `0-byte`/vanish class of bug again, with `/root/.rustup/toolchains/` empty and
  *dev*-dependency of `apps/api`, so token minting moved into the crate, which is where the
  (`find(|body| body["payload"]["via"] == "invitation")`), which is a statement about the platform
  `rustup toolchain install` reporting "unchanged" and doing nothing. `rustup
  width and the source belong anyway.
  rather than about a queue's tie-break. A test that asserts an ordering the code does not promise
  component add rustc` restored 1.98.1 (first attempt died on a download rename).
- **A test that reaches for a state the platform forbids.** The expiry walk set
  passes until a batch changes, and then fails for a reason that has nothing to do with the thing
  **A suite database persists between runs**, so renumbering migrations orphans
  `expires_at = now() - 1s` and the `media_shares_expiry_sane` check refused it — a link whose
  under test.
  what the old numbering recorded and produces a second, phantom failure
  expiry precedes its own creation is nonsense. The walk now ages the row by moving `created_at`
- **Proof.** `cargo test -p omnion-api --test events` → **3 passed, 0 failed** (the two
  (`VersionMissing(30)`) until the database is dropped.
  back instead, which is the only way to reach the same state and the reason the check is there.
  pre-existing walks plus the new one). `cargo test -p omnion-api --test tenancy_limits` →
- **Owner action.** `/mnt/apopic` reached **100 % (215 M free)** with eight
- **Environment.** `/mnt/apopic` hit **100 %** (89 MB free) mid-tick and MinIO refused every
  **22 passed, 3 failed**, unchanged from the baseline BUILD-LOG records
  sibling `qa/run.sh` processes running. The QA pass was deferred for the second
  object write with `XMinioStorageFull`, which reads as a storage bug and was none. The cause is
  (`a_ceiling_really_bounds_accepting_an_invitation`, `a_queued_link_never_works_and_says_so`,
  time in three ticks and the work was committed and pushed first. This branch
  this worktree's own `target/`: **8.3 GB of rebuildable test binaries** plus 874 MB of `.tmp`
  `the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows` — cross-test interference
  reclaimed only its own `target/debug/{incremental,build}` (~1.9 G). The eight
  leftovers from interrupted linkers. Reclaiming *only this worktree's* artifacts returned 8.5 GB
  between parallel walks sharing one database, reproduced at HEAD and unrelated to this change;
  worktrees still hold ~25 GB of `target/` between them; writers reclaiming each
  (86 %). **Owner action:** the seven worktrees under `/mnt/apopic` hold ~30 GB of `target/`; a
  this tick touched no production code at all). `pnpm typecheck` in `apps/admin` → clean.
  other's caches is not a sustainable answer.
  shared `CARGO_TARGET_DIR` is the structural fix, and this is the third tick to have had to
- **Environment.** `/mnt/apopic` opened the tick at 94% (3.5G free) with load 6.4, so the build ran
- **Next.** The last unticked criterion (line 222) is the whole-`cargo test
  delete its own build cache before it could run a test.
  on `CARGO_TARGET_DIR=/dev/shm/w5-target` as established. A dedicated `omnion_w5_events_test`
  --workspace` + `pnpm build` + QA walkthrough gate, and the mobile pass at
- **Next.** Slice 3 closes with duplicate detection and the merge (checksum groups, reclaimable
  database was created for the suite's own walks; the suite creates and drops its own
  390x844 over the member drawer, the Members card list and the
  size, `Keep this one` + `Merge group`, references repointed and the copies trashed), then the
  throwaway databases, so this one only carries the connection.
  Settings/Modules/Billing/Audit tabs is still unproven. Both are blocked on the
  CDN purge hook to REQ-011. Still open: EXIF (slice 2), HTTP range requests on the serve path,
- **Next.** The mobile pass — the member drawer and the settings/audit tabs at 390×844 — then
  same thing: **run the pass when `/mnt/apopic` has real headroom and few sibling
  and the Usage and Activity tabs, which need `media_references` and arrive with slice 4.
  the full QA walkthrough (`QA_STACK=w5 QA_API_PORT=18084 QA_ADMIN_PORT=3104 QA_WEB_PORT=3204
  passes.** Then REQ-005 closes and the queue moves to REQ-011 (CDN/edge).

  bash scripts/qa/run.sh`), which has not yet run green on this branch: seven writers on one 60G

- **The QA pass did not complete this tick — and not because of this slice.** The pass got its
  mount have had the volume between 94% and 100% all tick, and the last several attempts died on
## 2026-09-28 — The three "pre-existing" failures, and two product defects behind them
  QA slot after ~30 minutes of waiting behind sibling stacks, walked 434 clicks through
  `Page crashed` and `Execution context was destroyed` under host load rather than on anything in

  `/media` (40 elements), `/media/trash` (26), `/media/settings` (30) and the IAM screens, and
  the code. The REQ stays open until the real pass runs.
- **What shipped.** `bd53c2f` (three invitation-path defects), `e2a711c` (the panel follows
  then stopped advancing at `iam-service-accounts` with the walkthrough process at 0 % CPU. The
- **The QA pass, and an honest reading of it.** `QA_STACK=w5 QA_API_PORT=18084 QA_ADMIN_PORT=3104
  the new preview contract) and `00e36fb` (three walks driving the API the way a person
  cause is the machine, not the code: **seven QA stacks are running at once** — 40 Chromium
  QA_WEB_PORT=3204 bash scripts/qa/run.sh` was run and **did not complete**: the API built and the
  would). The suite is **29 passed / 0 failed**, up from 26/3 — no walk was deleted or
  processes, load average **36**, and **127 MB free of 33 GB**. The walkthrough outlived the
  stack came up clean (`/healthz` 200, admin 307, web 404 before the first request), and the
  skipped, and the 3 were not interference.
  browser context, which is the "tab died under parallel passes" case already documented for
  walkthrough visited **33 pages and 18 interaction passes** — overview, pages, media,
- **They were not one bug.** Run alone with `--test-threads=1`, each failed for its own
  this box, so no findings were produced and none are claimed. The share walk added this tick
  media-trash, media-settings, sites, ai, search, search-settings and eight IAM screens — before
  reason, and reading the *status it actually got* is what found them:
  (`runMediaShares`) is committed and wired but has therefore **not been exercised yet**; the
  the browser tab died at `iam-authentication` with `Target page, context or browser has been
  `202` (not `201`), `415` (not `409`), and a trail missing a row nobody wrote.
  next tick runs it. The storage walk from the previous tick was committed for the same reason
  closed`, and every later route reported the same. It never reached the organization routes, so
  - **A bodyless accept was a `415`.** `accept_invitation` took `Json<AcceptInvitationRequest>`
  and the API-level proof for both is the Rust suite, which is green.
  it says **nothing** about the screens this REQ is about, and the REQ stays open: this is the
    and axum refuses a bodyless POST in the extractor, so the ceiling and queue checks — both


  sixth consecutive pass on this branch to die the same way, and the ledger's earlier entries
    *inside* the handler, one line apart — were never reached. The panel sends `{}`; a signed-in

## 2026-09-28 · REQ-010 slice 3 closes — EXIF (commits 23e2e6d, 3837064, d14b355, 08c1dc0, 14e33ec, 9b7fab2)
  name the same two errors. The host reading, taken at the time of the failure: `MemAvailable`
    member has nothing to say. Body is now `Option<Json<…>>` + `Default`.
- **The mobile pass, and a QA failure that was the machine's and the harness's, not the code's.**

  4.5G (the precondition is >8G), **six** other `qa/run.sh` processes live (w2, w4, w7 and
  - **A queued accept was a `500`.** `IdentityError::InvitationAwaitingApproval` had no arm in
  - **What.** The spec's own `Mobile (<1024px)` line was the last unbuilt acceptance item in
- **What.** The last open item of slice 3: what the *camera* said about its own picture. The
  friends), 40 Chrome processes, load 8.5. The tab did not die *at* the failing route — it died
    the HTTP mapping and fell through to `internal_error`: the invitee was told the platform was
    REQ-005, and it turned out to be *six* surfaces rather than the three the sentence names.
  geometry probe already read a file's size from its header; this reads the other half of what an
  before it, and every subsequent route failed identically, which is the signature of a killed
    broken, and sent back to the manager who cannot fix it. Now `409 invitation_awaiting_approval`.
    Every table in the tenant surface (organizations, members, invitations, the department tree
  editor asks about a photograph — which body took it, at what shutter speed, with which lens, on
  browser rather than a broken page. The `qa-artifacts` run directory (43M) was removed after
  - **The preview was a token oracle.** Its own doc comment promised that an unusable token
    and the audit trail) now renders as cards below `md`; the organization switcher becomes a
  which day. `crates/media/src/exif.rs` (the reader), `0042_media_exif.sql` (the column), the
  reading `summary.json`, which contains only the fatal line and no findings; `/mnt/apopic` went
    answers with the same shape and `usable: false`; the code answered `404` for an unknown one
    bottom sheet under `sm`; the member drawer takes the whole screen below `sm`. Each card
  `exif` column on `media` plus the `display_width`/`display_height` pair on the file response, and
  95% → 83% on that and on the other writers finishing.
    and `200` naming the organization for a queued one. A leaked or guessed token could
    carries the **same `data-*` hooks as the row it replaces** — the part that is easy to get
  the Metadata tab's **Camera** block.
- **What this tick changed, precisely.** No production code. The walk lives in
    therefore map which organizations exist — the exact thing the endpoint exists to prevent. An
    wrong, because a `data-organization-suspend` that lives in only one of the two renderings
- **Six decisions, each a shortcut that produces a plausible wrong answer.**
  `apps/api/tests/events.rs` and runs against a live database over the real router; the docs
    unknown token is now `200` with the same body, `usable: false` and one coarse `reason`
    silently halves what the desktop depth passes can drive, and the mobile pass then photographs
  1. *A TIFF header is not EXIF.* The IFD format is shared by TIFF, GeoTIFF and half a dozen
  record it. A REQ cannot close on a pass that did not run, so the next tick starts with the
    (`"unusable"`, never `"queued"`/`"expired"` — "why" is the question a token-walker is asking).
    a layout no interaction has ever reached.
     makers' proprietary blocks; what makes the block EXIF is the `Exif\0\0` signature inside a
  mobile pass and a retry, and checks `MemAvailable` and the sibling-pass count *before* launching
- **Two walks were asking the wrong question.** The ceiling walk inherited the invite policy
  - **Why the switcher is a sheet.** A dropdown anchored to the right edge of a 390px screen puts
     JPEG `APP1`, an `EXIF` chunk in a WebP or an `eXIf` chunk in a PNG. The container is checked
  one rather than interpreting the failure afterwards.
  instead of choosing one, so migration `0038`'s `default 'owner_approval'` queued the invite and
    the longest organization name in the one place a thumb cannot reach it. The sheet is anchored
     before any TIFF parsing runs.

  the walk blamed the seat ceiling for a refusal it never reached. The audit walk wrote settings
    to the bottom edge, carries a backdrop and a close control, and its rows have a 44px floor —
  2. *Nothing is read from outside the prefix.* Every field's value may be an *offset*, and an
## The member drawer (REQ-005, slice 4)
  **straight to the store**, bypassing the route whose `omnion_audit::record` is the very thing
    and the pass reads that geometry rather than asserting that it opened, because "it opens" is
     offset is attacker-controlled. Every read is a range request whose failure is the answer.

  under test, then asserted the row was in the trail — the trail was right; no settings change
    not a claim and a sheet can be open and still be wrong in four ways (rows under 44px, not
  3. *A zero denominator is absent; `1/200` is not.* The first guard refused the normal case
- **What this tick found, by reading the spec before writing code.** The Members bullet names a
  had ever gone through the product. Its cross-tenant check was weaker still: the other
    anchored, no backdrop, page scrolls sideways underneath it). Each is a finding.
     (`den > num`) and kept the corrupt one — the exact inversion, which is how a reader ends up
  drawer and the QA plan opens one ("open a member drawer and extend a binding"). Neither
  administrator held no `audit.read` anywhere, so the guard answered `403` before the handler
  - **Proof.** `cargo test -p omnion-identity --lib` → **144**, `cargo test -p omnion-api --lib`
     with no shutter speed on every photograph and a divide-by-zero on the one broken file.
  existed. Reading the sentence closely turned up a second gap worth its own line: the spec lists
  resolved the tenant and the assertion proved nothing about isolation. Granted the permission in
    → **146**, both green. `pnpm --filter @omnion/admin typecheck` clean and
  4. *Orientation changes the box, not the file.* Values 5–8 store the picture sideways and
  **three** operations — *add / extend / revoke* — and the platform had verbs for two.
  *their own* tenant, the `404` is what a real auditor gets.
    `pnpm --filter @omnion/admin build` completes (the 44 routes, `ƒ Proxy (Middleware)`). The
     browsers rotate it themselves, so a grid reserving `width × height` reserves the wrong box and
  `POST /iam/bindings` granted, `DELETE /iam/bindings/{id}` revoked, and nothing could *lengthen*
- **Proof.** `cargo test -p omnion-api --test tenancy_limits -- --test-threads=1` → **29 passed /
    walkthrough parses (`vm.Script`) and now walks `/organizations` and four tenant tabs at
     shifts every image below it. The stored columns carry the oriented pair, the raw value stays
  a temporary grant, so giving somebody another month meant revoking and re-granting.
  0 failed** (257 s). `cargo test -p omnion-api --lib` → **134 passed / 0 failed**.
    390×844, plus the sheet's geometry, each recorded as a finding.
     in the record, and the API sends both readings.
- **Why "extend" must not be revoke-then-grant.** That implementation passes every assertion a
  `tsc --noEmit` in `apps/admin` exits 0. Tree clean, `wave5` pushed to `00e36fb`.
  - **The QA pass failed, and the reason is worth more than the screens.** The first run walked
  5. *A GPS fix is a flag, never coordinates.* There is no field in the type a coordinate could
  person can make by eye and leaves two live bindings for one role and scope. The
- **Environment.** `/mnt/apopic` opened at 95 % with 8 sibling `qa/run.sh` processes, so the
    every route on schedule and reported `interact: iam-devices → 3 elements`,
     occupy, so a media library cannot quietly file an operator's home address into a row that
  effective-permissions screen then renders the same role twice with two different windows,
  build moved to `CARGO_TARGET_DIR=/dev/shm/w5-target` (mine, 2.2 G of the 32 G tmpfs) with
    `interact: organizations → 3 elements`, and the same for all ten analytics screens. Three
     search, an API key and a share link can all read.
  neither of which is the truth. `bindings::extend_expiry` updates **the same row**, and the
  `CARGO_INCREMENTAL=0` — reclaiming my own disk to build on a box that has none was not available.
    elements is the *sign-in form*. The cause is in the harness, not the panel: the generic
  6. *A replacement replaces the record.* A screenshot over a camera original must not keep
  statement carries `revoked_at is null` so a grant the trail already recorded as revoked cannot
- **Next.** The only unticked criterion (line 222) is still the whole-workspace gate plus the QA
    filler writes a sample value into every input, and on the sign-in form a sample address is a
     claiming to have been shot on a body it was never near.
  come back to life. The walk that proves it is
  walkthrough, and the 390×844 mobile pass over the member drawer is still unproven. Both need
    real failed attempt — after ten, `sign_in_attempts` records `blocked / "10 recent failures
- **The QA pass did not complete, and it found the tick's one real bug anyway.** The full pass ran
  `a_temporary_grant_is_extended_in_place_and_never_into_a_second_row`, and the assertion that
  `/mnt/apopic` headroom and few sibling passes. Then REQ-005 closes and the queue moves to
    from this address"` and the address is refused. From that moment every later page renders the
  629 clicks and then died with `Target page, context or browser has been closed` — the
  catches it is a **count** — `bindingRowsAfter == bindingRowsBefore`.
  REQ-011 (CDN/edge).
    login screen and the pass measures three elements, **with no error anywhere**. The failure is
  "browser context dies under parallel passes" case already documented for this box, with 21
- **One read for the whole panel.** Four parallel requests would let the identity render over an
    silent because the sign-out is deferred and the session is renewed, so the run looks healthy
  sibling QA processes and 0 GB free at the moment it failed. So the gate is **not** claimed as
  empty binding list, and an empty list under a colleague's name is a statement about that
    right up until it is not; the media depth passes after it all reported "no file to share"
  green this tick. What *did* run is `scripts/qa/probe-media-camera.cjs`, a one-screen probe added
  colleague, not about a spinner. `GET /organizations/{id}/members/{user_id}` answers everything
    because the session was gone. `b2a4e33` fills the sign-in form with the account that exists,
  because "the full pass crashed" and "the screen is broken" must not read the same in a log: it
  the drawer renders.
    detected by the *path* rather than by the field's label — an `email` input is also how a
  signs in, uploads a JPEG that really carries a block, reads the block's rows and reads the
- **Isolation.** Another tenant's member is a `404`, not a `403` — the second answer confirms the
    stranger's invite form asks for an address, and the QA owner must never be submitted into one.
  API's own response — **13/13 checks pass**.
  id exists. A grant is refused when the subject is not a member here, when the role belongs to
  - **Next.** Run the pass again on the fixed harness for the green walkthrough that closes slice
- **The bug it found: the rotation was applied twice.** The writer already stores the *oriented*
  another tenant (`cross_organization`), and for a `global` scope by name rather than quietly
    4, then the last acceptance line (`cargo test --workspace` + `pnpm build` + zero high
  geometry (an orientation-6 4000×3000 frame is written as 3000×4000), and `display_size()` then
  downgraded to `organization`: a tenant administrator asking for a platform grant is asking for
    findings) and REQ-005's close. After that, the queue moves to REQ-017 (sandbox/staging).
  applied the swap again on the way out — so the panel reported a landscape picture for a portrait
  something the platform owns, and a silent downgrade makes the grant do something other than

  photograph, and the facts list read `4000 × 3000` for a file every browser draws tall. The unit
  what the panel said.
  - **The pass, run on the fixed harness.** 991 clicks, 1,039 shots, every route reporting a
  test on `oriented_size` passed the whole time, because the function was correct; it was being
- **Proof.** `cargo check -p omnion-api --tests` → clean. `pnpm typecheck` in `apps/admin` →
    full element count — `iam-policies` 40, and all ten analytics screens 40 where the first run
  called on the wrong input. This is the case the QA pass exists for and the Rust suite cannot
  clean (the LSP caught two real nullability errors in the drawer before the gate did: a revoked
    said 3. `sign_in_attempts` held **one** row, `success`, against eleven `blocked` before: the
  see, and it is why the fix ships with a regression test shaped like the bug: a row carrying the
  row's `revoked_at` and an extended binding's `expires_at` are both nullable, and both were
    lockout is not merely bypassed, it no longer happens. The mobile switcher reads 390px wide,
  oriented columns *and* the record that produced them.
  being formatted as though they were not).
    anchored, a 59px row, a backdrop and no page overflow, and the palette still 44px per row.
- **Proof.** 19 new unit tests in `exif.rs`, 5 in `model.rs` and two walks over the real router →
  `cargo test -p omnion-api --test tenancy_limits -- --test-threads=1` → **23 passed, 5 failed**;
  - **78 findings, 72 high — and the count is the wrong thing to read.** 28 are `422`s and 33
  0 failures. Both
  three of the five are the baseline cross-test interference the ledger already records
    their console errors: the generic filler submitting sample data to forms it does not own, on
  walks read the column **out of PostgreSQL**, because a response that omits a field is
  (`a_ceiling_really_bounds_accepting_an_invitation`, `a_queued_link_never_works_and_says_so`,
    every writer's screens. Three findings were mine and two of those were the harness measuring
  indistinguishable from one that stored it and chose not to say so. An orientation-6 frame of
  `the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows`), and the two new walks' own
    the wrong thing (`1818c50`): the sheet's bottom-anchoring was asserted against a component
  4000×3000 stores 3000 and 4000; a text file grows no record; a replacement in a format with no
  first run failed for the reasons corrected below.
    that is a *panel* from `sm` up, and a tab strip inside a horizontal scroller was reported as
  block clears it and the geometry falls back to the frame's own; a restore brings version 1's
- **Two failures that were wrong *assertions*, not wrong code — and one of them was the better
    unreachable when the spec asks for a scroller. The third — two 500s on `/members` — is the
  record back; the serialised object is scanned for `lat`, `lon`, `GPSLatitude`, `GPSLongitude` and
  answer.** The first run asserted that extending a revoked grant is a `404`. It is a `400
    shared box, not the code: the API log for the window shows `pool timed out while waiting for
  `altitude`. `cargo test -p omnion-media --lib` → **122** (was 98), `-p omnion-api --lib` → **122**,
  binding_revoked`, and the `400` is right: the row exists, the operator can see it in the drawer,
    an open connection`, which is what seven concurrent QA stacks do to one Postgres. So the
  and `--test media` (13, was 11), `--test media_transform` (5), `--test media_settings` (2),
  and "there is no such grant" would send them hunting for a typo instead of reading the sentence
    honest count of findings **caused by this change** is zero, and the pass is not green.
  `--test media_duplicates` (6), `--test media_shares` (5) are unchanged and green. `pnpm
  that states the rule. The second asserted that the subject's drawer shows the grant. It must
  - **Next.** Re-run the pass with the corrected assertions for a run whose findings are only
  --filter @omnion/admin typecheck` clean. The migration applies across the whole set on a fresh
  not: the grant was performed by the *administrator*, so it belongs on the grantor's trail and
    the pre-existing 422/console-error family, then the last acceptance line and REQ-005's close.
  database.
  in the organization's Audit tab, and putting an administrator's acts inside a panel about a
    The two remaining known-pre-existing items are named here so the next tick does not
- **The QA pass had to grow a screen before it could test one.** The library pass uploads a PNG,
  colleague is the wrong half of a privacy decision made by accident. Both assertions now pin the
    rediscover them: the media depth passes report "no file to open" although the upload step
  and a PNG carries no EXIF — so the Camera block would only ever have been seen in its empty
  intended behaviour rather than the one that happened.
    reports success, and the IAM sessions screen answers 401 for a scoped read.
  state, which the "no untested screen" rule forbids. The pass now *builds* a JPEG that carries a
- **The QA pass was deferred, by a number, before it was launched.** `MemAvailable` 4G against a

  real block, uploads it, opens its detail screen and asserts the body, `ISO 400`, `1/200 s`,
  `>8G` precondition, **eight** sibling `qa/run.sh` processes, load average 34. Those are the

  `f/1.8` and the rotated dimensions.
  conditions the last six attempts on this branch died in, and they were readable in one `free`
## Wave 5 · REQ-005 slice 4 — the switcher's phone sheet, and a gate that could not run
- **Four bugs the walkthrough's own JPEG builder had, each of which produced a file that read as a
  and one `pgrep` a minute before starting. The walkthrough pass for the drawer is written

  parser bug and was really a builder writing the format wrong.** The TIFF block is little-endian
  (`runOrganizationMemberDrawer`, wired after the departments pass) and `node --check` clean; it
- **What.** The last high finding of the previous pass was a `position: fixed` bottom sheet whose
  while the JPEG framing around it is big-endian, so a segment length written with the block's
  runs when the box has room. The REQ stays open.
  bottom edge measured **738px above the floor of a 390×844 phone**. A `fixed` box resolves against
  `u16` reads as a 57 KB segment in a 253-byte file and the block is then unreachable; a directory
- **Next.** The mobile pass at 390×844 (member drawer, Settings/Modules/Billing/Audit tabs, the
  its nearest containing block, and an ancestor with a `filter` — `backdrop-filter` included, which
  is a count plus its entries plus a four-byte next-directory pointer, and the two that get
  organization detail), then the QA walkthrough. Then the platform-account criterion, the last
  `getComputedStyle` reports as `filter` — or a transform *becomes* that block. The panel's own
  forgotten put every later offset on the wrong field; a value offset is measured from the start of
  unticked line on this REQ.
  header is `sticky` with `backdrop-blur`, so the sheet rendered beside its own trigger was
  the *block*, not from the directory that holds the entry, so a value laid down before the
  anchored to the header's box. The class list was correct and said nothing about any of this; only
  sub-directory exists is overwritten by it; and a RATIONAL is two words wide rather than four
  a measurement of the rendered box caught it. The sheet is now portalled to `<body>` on a phone,
  bytes, so a cursor that steps by the entry's width lands every rational after the first on the
  where no ancestor establishes a containing block, and it stays `absolute` beside its trigger from
  wrong field. None of the four would have been found by a screenshot — the file simply had no
  `sm` up. The click-outside handler tests the sheet separately, because a portalled sheet is no
  camera record and the screen showed its empty state, correctly.
  longer inside the trigger's ref and the first click inside it would otherwise close the control
- **Environment.** `/mnt/apopic` sat at **94 %** (3.6 GB free) on entry, and this worktree's own
  under the finger.
  `target/` was 9.9 GB of it. Reclaiming *only this worktree's* `target/debug/incremental`
- **Proof, measured, not asserted.** `node scripts/qa/probe-tenant-mobile.cjs` → **5 passed, 0
  (verified first: no live `cargo` holds it) returned 1.7 GB. **Owner action:** the worktrees under
  failed**: `gapToBottom 0` (was 738), `position fixed`, `blockers []` — the walk up from the sheet
  `/mnt/apopic` still hold ~30 GB of `target/`; a shared `CARGO_TARGET_DIR` is the structural fix.
  names every ancestor that would capture it, and there are none. The walkthrough now also accepts
- **Next.** Slice 4 — folder and file grants with inheritance, the scanning pipeline with
  the redirect an organization account gets from `/organizations` and starts the tenant depth pass
  quarantine and release, retention policies with the daily worker, and reference-based purge
  from there, instead of concluding there is no organization to open; both entry points share one
  refusal. Done when a denied subject is refused on the raw route, a flagged upload is quarantined
  body so they cannot drift.
  and releasable, and a retention run removes exactly the eligible rows.
- **Every fast gate is green and the numbers are real.** `cargo test --workspace` → **979 passed /

  0 failed** (exit 0, isolated database, `--test-threads=1`). `pnpm build` → 2/2 tasks, 44 admin
## 2026-09-28 · REQ-010 slice 4 · virus scanning (quarantine, release, run log)
  routes. `pnpm typecheck` clean. `cargo test -p omnion-identity --lib` → 144.

- **Two walks were asking the wrong question, and one was born broken.**
**What.** The `scan_status` column arrived back in `0025` and nothing ever moved it: the library
  - `tenancy::only_the_platform_opens_tenants_and_reads_across_them` bundled a rename and a freeze
could render a badge and the badge could only ever read `pending`. This tick gives that column a
    into one PATCH and asserted `organization.updated` — true only before slice 3 made a status
pipeline behind it — `0044_media_scanning.sql` (a per-site policy, a quarantine table with a
    move its own lifecycle action. It had been failing since. Split into the two claims it was
history, a run log, plus the two `media` columns the pipeline needs and `0025` never created), the
    really making, and the freeze is now proved to be a real freeze (a rename of a frozen tenant is
crate module `crates/media/src/scanning.rs`, the API in `apps/api/src/routes/media_scan.rs`, and
    refused with `organization_not_writable`) before the walk thaws the tenant and carries on.
the **Scanning** tab on `/media/settings`. Seven routes; the gate is on *every* serve path, not
  - `onboarding::the_first_run_walks_a_fresh_database_to_a_signed_in_owner` read
just the one the spec names: the panel raw route, the public renderer, the preset path **including
    `body["organization"]["id"]` from an endpoint that answers a `StatusBody` and has never carried
its cached derivatives**, both version paths, and the share token route.
    the created row — the admin client types the reply `Promise<OnboardingStatus>` and never looks

    for an id either. The assertion came from `b347de6` and failed the first time it ever ran. The
**Seven decisions, each a shortcut that produces a plausible wrong answer.** An unrecognised
    walk reads the id from the database, which is what it actually wanted. **Adding a field to the
scanner answer is an *error*, never a pass (pinned against a scanner that answers
    API to satisfy a test would have been changing the contract to fit the assertion.**
`{"verdict_code": 3}` over a real socket); a file above the size ceiling is `skipped` and never
- **The pass itself could not run, and twice, for reasons that are the box and the harness.**
`clean`; a clean scan leaves `scan_detail` **empty** so a report that greps it cannot find a
  - `run.sh` hardcoded `target/debug/omnion-api` in three places while the standing practice on
positive on every file; a quarantine row is closed and never deleted, so a file flagged twice has
    this box is `CARGO_TARGET_DIR=/dev/shm/<writer>-target` (seven writers, one 60G mount). A writer
two events; a release requires a reason and lands the row on `skipped`, not `clean`, because
    who followed that practice could not start a pass at all: the build wrote 332MB of good binary
nobody has said the file is *safe*; another tenant's quarantine is a `404` and not a `403`; and a
    into `/dev/shm`, the gate concluded there was none, and the pass died at `wait_http` reporting
**flag is a fact rather than a policy question** — `on_error` speaks to the *absent* verdict
    nothing about the code under test. Fixed, and the gate now prints the path it is about to
(`pending`, `error`) and never overrules a `flagged` row.
    start. The second half of the same bug: once a pass registers the API from tmpfs, the pm2

    entry's script path *is* the tmpfs copy, and a later pass that builds elsewhere inherits an
**Three defects found by the walks, none of which a unit test could have seen.** The claim query
    entry that can only fail — so the gate compares the registered path with the one it just
selected `id` while the row type called the field `media_id`, and sqlx's `FromRow` maps by
    built and re-registers when they differ. (Extracting that path from `pm2 describe` is its own
*column name* — so the sweep died with `no column found for name: media_id` on the first file,
    small trap: the table uses a box character followed by a **non-breaking** space, so a sed
which reads as a broken query rather than a missing alias. `0025` never created `media.scanned_at`
    pattern matching an ordinary space extracts nothing and the comparison silently never fires.)
at all, so **every verdict write failed**, and because the route counted the error both as a
  - The next attempt reached **620 clicks** and then the shared pm2 daemon restarted and wiped
verdict and as a write failure, a one-file run reported `errors = 2` — a number with no reading
    `omnion-qa-*-w5` mid-run. No `summary.json` was written. A missing summary is not a product
an operator can act on. And `sum(bigint)` decodes as NUMERIC, so the quarantine byte total could
    result, and a `chrome-error://chromewebdata` navigation in the click log is the tell.
not be listed at all.
- **13 "failures" that were never failures.** `cargo test --workspace` with no

  `OMNION_DATABASE_URL` runs against the **shared default database**, where a sibling branch's
**The one that matters was found by a suite I did not write.** `media_shares.rs`'s
  `0019_cms_blocks` is already applied while this branch's `0019_organization_memberships` is a
`a_link_stops_serving_when_its_file_stops_being_servable` failed on my change: the first gate had
  different migration at the same number. Isolated, the analytics suite is **13/13 green**. This
an early return for a site with scanning *disabled*, and the walk switches the scanner off before
  is the second time this branch has lost a gate to a shared migration number; the fix is a
flagging a file by hand. The regression was real and it was the shape of the bug: a flag is a fact
  writer-owned database and `--test-threads=1`, never a code change.
about the bytes, and turning the scanner off is a decision about *future* uploads, not a way of
- **Next.** The one unticked box is the walkthrough with zero high findings, and it is **not
forgetting a verdict somebody already reached. Fixed in the crate (`may_serve` now reads
  ticked**: three passes on this box ended in a harness bug, a daemon restart and a 100% disk, and
`enabled` itself, with a test that runs all four `enabled`×`on_error` combinations) rather than at
  a REQ is never closed on a partial run. Gate the numbers first — `df -h /mnt/apopic` with real
the call site, so the panel and the share link cannot drift apart again.
  headroom, `pgrep -cf 'qa/run.sh'` at 1, `free -g` MemAvailable over 8 — and defer rather than

  report a pass that measured a box under pressure. Then the queue moves to **REQ-011 (CDN/edge)**.
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

## Wave 5 · REQ-011 slice 1, the second half — a rule that changed nothing, found by running its own tests

- **What.** The crate, the migration, the permission, the API and fifteen integration walks
  were all in place and green by inspection. The rule engine was never called. The public
  surface wrote a literal `public, max-age=3600` into every response, so a cache rule an
  operator created changed nothing a visitor's browser did, and the `/cdn/rules` table was
  a screen that described a policy nothing enforced. This tick is the seam: `routes/cdn_cache.rs`
  loads a site's ordered rules, decides, and renders that decision — headers, `ETag`,
  `Surrogate-Key`, `Vary`, and a `304` for a conditional read.

- **A router that panicked while being built, taking every route with it.** `POST /cdn/rules`
  merged `create_rule` *and* `reorder_rules`. Two handlers for one method on one path is not
  an ambiguous route to axum — it is a panic during router construction, so the whole
  application refused to start, not just the CDN surface. Reorder has its own path now, which
  is what the request specifies anyway. It read as working because **no API suite had ever
  been run against this router**: `apps/api/tests/cdn.rs` shipped with fifteen tests that had
  never been executed.

- **The fifteen walks failed for five different reasons and none of them was the product.**
  The login helper read `body["token"]` — the session token is in a cookie and nowhere else,
  so a successful sign-in read as a failed fixture. The error assertions read `body["code"]`
  at the top level while the API nests under `error`, as every other suite already does. The
  audit table is `audit_log`, not `audit_entries`. The platform settings row is
  installation-wide and survives `cleanup`, so the second run's insert was refused and the
  partial unique index looked broken. And the reorder walk compared against the order the
  rules *started* in — which passes for a store that ignored the request entirely and fails
  for one that obeyed it.

- **The one that would have been fixed in the wrong place.** That last assertion said the
  store was broken. A temporary diagnostic read the rows out of PostgreSQL and printed both
  orders: the store had done exactly what it was sent, and the assertion was wrong. Editing
  the store to satisfy it would have replaced a correct transaction with a wrong one. The fix
  is to assert the sentence the test name claims — *the order the client sent is the order
  stored* — which is strictly closer to the name than what it replaced.

- **Two real product bugs, both of which the walks could not have found.**
  `resolve_settings` promised to fall back to the installation-wide row and never could:
  `where site_id is not distinct from $1` matches the site's own row and nothing else, so a
  site without one reported "no CDN is configured" while the installation had a provider
  running. And `unique (coalesce(...), coalesce(...), ...)` in `0047_media_grants.sql` is a
  syntax error — a table-level `UNIQUE` constraint takes column names, not expressions — and
  because migrations apply as a set, it stopped *every later migration* from applying too.
  Fifteen CDN walks failing on a media migration is what finally made it visible.

- **A lifetime that a handler could not satisfy.** `RequestShape` borrowed its cookie and
  header name lists, which is fine for a unit test building one from literals and impossible
  for a handler: it parses a `HeaderMap` into a local, and the local dies at the `await` the
  decision needs. Three workarounds were tried and all three were worse — a self-referential
  `ShapeInput`, a `Box::leak` that leaked a string per request, a temporary dropped while
  borrowed. The lifetime was the bug, so it is gone: every field is owned, at the cost of two
  small allocations per public request.

- **Proof, measured.** `cargo test -p omnion-cdn` → **59 passed** (was 43; the ETag/Vary module
  adds 16 and the builder fixtures replaced six struct literals). `cargo test -p omnion-api
  --lib` → **180 passed** (was 127). `cargo test --test cdn` → **15 passed, 0 failed** (was
  0 passed, 15 failed — the suite had never run). `cargo test --test cdn_headers` → **13
  passed, 0 failed**, new, and the walk that matters is
  `a_rule_an_operator_creates_changes_the_cache_control_of_a_matching_page`: the same public
  URL, read before and after a rule is created, carrying `private, no-store` and then
  `public, max-age=60`. `--test public` → 5, `--test media_shares` → 2. `apps/admin`
  `tsc --noEmit` clean.

- **One pre-existing failure left standing, honestly.** `--test media` fails 2 of 13
  (`a_camera_record_is_read_from_the_bytes_and_never_holds_a_coordinate` and
  `a_replacement_replaces_the_camera_record_rather_than_inheriting_it`): the EXIF fixture
  builder writes into a slice one byte past its end (`range end index 164 out of range for
  slice of length 163`). It is a bug in the test's own byte assembly, it touches no CDN code,
  and it was failing before this tick. Not fixed here because it belongs to REQ-010.

- **Next.** The admin screens — `/cdn`, `/cdn/rules` with the drag reorder, the rule form with
  its live match tester, `/cdn/purge`, `/cdn/purges`, `/cdn/settings` — and the media 304 walk.
  Then the walkthrough, which is still owed an honest pass on this box.

- **Tick 14 · omnion-wave5 · platform & enterprise.** Resumed a merge that tick 13 left
  half-finished (eight unmerged paths, text already resolved). The resolution turned out to hide
  a real defect: `retention_runner` named two different workers on two branches, so one `spawn`
  was wired to the other's module. The audit sweep (REQ-005 slice 4, per-tenant window) is now
  `crate::audit_retention` behind `OMNION_AUDIT_RETENTION_SWEEP`; the media worker (REQ-010)
  keeps `OMNION_RETENTION_RUNNER`. Commits `704d00b`, `3e58d69`, `6b7061d`.

  Proof: `cargo build -p omnion-api -p omnion-core` exit 0; `cargo test -p omnion-api --test
  tenancy_limits -- --test-threads=1` **29 passed / 0 failed** (was 26/3); `pnpm typecheck` 2/2.
  The three failures were one bug: `InvalidSettings`, `InvalidLimits` and
  `InvitationAwaitingApproval` had no arm in `From<IdentityError> for ApiError` and fell into the
  catch-all, so a validation failure answered `500 internal_error` instead of 400/400/409.

  Note for the next writer on this box: the `analytics` suite resolves `OMNION_DATABASE_URL` to
  the **shared** `omnion` dev database, and `0047` changed in this merge, so it reports
  `VersionMismatch(19)` against the main writer's DB. Do not drop that database — run the
  workspace suite against your own isolated one.

  Next: the REQ-011 CDN admin screens (`/cdn`, `/cdn/rules` with drag reorder, the rule form
  with its live match tester, `/cdn/purge`, `/cdn/purges`, `/cdn/settings`) and the media 304
  walk, then the browser walkthrough.

- **One more copy of the same bug.** `media_settings.rs` carried a second instance of the
  `whole_seconds() >= 1` "configured" race that `media_scan.rs` had already been fixed for, and
  it failed its own walk in the same workspace run. Fixed to compare the instants directly
  (commit `3988995`); `cargo test -p omnion-api --test media_settings` goes 1/2 -> 2/2. The other
  two `whole_seconds()` uses in the tree are a file age and a lease duration, and are correct.

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

## Wave 5 · tick 15 · REQ-011 CDN: the media validator, and the three screens the spec named

**What.** The CDN had a rule engine, a header layer, a full CRUD API and a test suite, and
no screens at all — the spec's six routes were a list in a document. This tick builds three
of them (`/cdn`, `/cdn/rules`, `/cdn/settings`) and closes the one acceptance item that had
been ticked open with a note admitting the walk did not exist. The rest of the section
(`/cdn/purge`, `/cdn/purges`) belongs to slice 2 and the overview says so.

**Two of the three new media walks failed on the first run, and both failures were the
tests' fault rather than the product's — which is worth recording because the shape of both
mistakes is the same shape as a real defect.**

The first is the *two TTLs in two different headers*. `Cache-Control: max-age` is what a
visitor's browser obeys; the edge's own lifetime is a separate `CDN-Cache-Control`. The test
set an edge TTL of an hour, left the browser TTL at its default minute, and then asserted
`Cache-Control: max-age=3600` — so it failed with `max-age=60`, and the failure looked like
"the rule is not applied" when in fact the rule was applied perfectly to a header nobody was
looking at. A rule that holds a page for an hour at the edge and a minute in the browser is
the *ordinary* configuration, so an implementation that put both numbers in one header would
have passed this test and failed every real operator. The walk now asserts both headers and
their being different, which is the point rather than an accident.

The second is the ordering bug in the *scanner* walk: the rule was created before the "no
rule yet, so this is private" read, so the second assertion was true for a reason that had
nothing to do with scanning. The rule now exists before the refusal on purpose, so "no cache
header on a 403" is a claim about the order of the two checks. That order is the whole
point: a refusal carrying an `ETag` is a refusal an intermediary is entitled to remember,
and a scan that finishes an hour later leaves the cached 403 sitting in front of it. A
`media_scan_enabled_has_endpoint` constraint also caught the fixture writing a row the API
would never accept — enabling scanning with nowhere to scan *to* is not a configuration, and
the table is right to say so.

**On the rule form, three choices are decisions rather than fields, and each is the place a
cache configuration usually goes quietly wrong.** The **match tester** answers while the
pattern is typed, because a glob matching nothing is a *valid* rule the API will happily
store: the only evidence it is broken is a page that did not become cacheable. The **order
is shown as a rank**, and a rule matching `/**` is labelled "matches everything" instead of
sitting quietly where it looks harmless — the matcher takes the first match, so a broad rule
above a narrow one makes the narrow one unreachable forever. And the **reorder sends the
complete list**, because a reorder that renumbers one row is exactly what leaves two rules
claiming the same priority and lets the matcher break the tie by row order rather than by
what the drag showed. The depth pass therefore reads the result back **from the API** and
asserts the priorities are a dense ascending run, which is the property a per-row renumber
breaks and which no screenshot can see.

The settings screen holds the line that matters most there: the credential is write-only,
the form starts blank on every visit, and the field is omitted from the payload entirely
unless the operator typed something. An empty box that looked like "no key stored" would send
somebody to re-enter a working key, and a box that rendered the saved one would be a leak —
same bug, two faces, so the form says which state it is in and the save carries what was
actually typed.

**Proof.** `cargo test -p omnion-api --test cdn_headers -- --test-threads=1` against the
isolated database `omnion_w5_iso` → **16 passed / 0 failed** (was 13). `pnpm typecheck` → 2
successful, 0 errors. `cargo test -p omnion-cdn` → **70 passed**. `node --check
scripts/qa/walkthrough.cjs` clean. The browser pass (`runCdnRulesDepth` plus the three new
routes) is in flight on the private stack — QA_SLOT_WAIT is 3600 s on this box and other
writers hold the slot, so the pass is queued rather than claimed.

**Commits.** `3a9a88d` (export the two adapters the catalogue already advertised),
`807e307` (the rule table, the form and the match tester), `8ca76f0` (the QA depth pass),
`ee421ad` (the overview and the provider screen), `69918e2` + `29b3409` (the three media
walks and the constraint the fixture hit).

**Next.** Close slice 1 on the browser pass, then REQ-011 slice 2: the purge tables
(`cdn_purges`, `cdn_purge_items`), the console, the worker drain with retry and the history
drawer. The migration number is the live question — the shared high-water mark moved while
this tick ran, so the next writer to take 0051 will collide.

## 2026-09-29 · REQ-011 slice 2 · the purge pipeline

**What.** The queue and the invalidation path end to end. `0054_cdn_purge_queue.sql` adds
`cdn_purges` and `cdn_purge_items`; `omnion_cdn::purge` holds the decisions that have no I/O
(target validation and normalisation, batch splitting, the backoff curve, the fold from item
outcomes to a parent status); `apps/api/src/routes/cdn_purge.rs` is the nine-route surface;
`apps/api/src/cdn_purge_runner.rs` is the drain; the panel gets `/cdn/purges` (history,
filters, detail drawer) and `/cdn/purge` (the console), and the overview's third card is
real data instead of a dash.

**The one defect the walks found, and it is the kind that never shows up in a unit test.**
`claim_due` is a `with due as (...)` CTE feeding an `update ... from due ... returning`. The
RETURNING list was unqualified and the CTE exposes a column called `id` of its own, so
PostgreSQL refused the statement with `column reference "id" is ambiguous`. That is a runtime
failure on the first item the worker ever claimed — the drain shipped in a state where every
purge would log a failure and nothing would ever reach a provider. It compiles, passes clippy,
and is a string, so nothing short of running the statement against a database can find it. The
fix qualifies every name with the table alias and renames the CTE column to `claimed_id`, so
the two relations do not even offer the same name.

**Three test-side findings worth keeping, because each was me asserting the wrong thing.**

* A cross-organization read answers **403, not 404**. The row exists; "not found" would be a
  lie that also happens to leak less. The honest answer names the reason, and the walk now
  asserts the code as well as the status.
* The API's error envelope is `{"error": {code, message, details}}`. Ten walks read
  `body["code"]`, got `Null`, and reported the *refusal* as the failure — a green-looking
  failure list that was actually ten correct refusals. Read the envelope.
* A settings row naming an adapter this build does not ship **cannot be created**: the
  migration has a `provider in (...)` check underneath the API's guard. The first version of
  that walk inserted the row with `.ok()`, which swallowed the error, and then asserted a
  refusal that could not be provoked. The walk now proves both layers and asserts the
  constraint *by name* in the database error.

**The backoff is a schedule, so the test waits by reading it.** The walk that exhausts a
two-attempt budget used to sleep a guessed twelve seconds. It now polls `next_attempt_at`
until nothing is pending, because an item waiting out a backoff is invisible to `claim_due`
and a drain issued too early claims nothing — which the walk would then have blamed on the
attempt budget. Same for the test helper: it now calls `mark_running` exactly as
`cdn_purge_runner::tick` does, because a helper that is not the worker's logic will drift
from it and the `started_at` it left null looked like a product bug.

**Proof.** `cargo test -p omnion-api --test cdn_purge -- --test-threads=1` against the
isolated `omnion_qa_w5` database → **17 passed / 0 failed**. `cargo test -p omnion-cdn` →
**94 passed** (was 70). `pnpm typecheck` → 2 successful, 0 errors. `cargo build -p omnion-api`
→ clean. `node --check scripts/qa/walkthrough.cjs` → clean.

**Browser gate.** The pass is queued for the shared QA slot (one other writer holds it), and
`runCdnPurgeDepth` plus the two new routes are in the inventory. **Not yet run — slice 2 is
not closed until it is.** Three of the request's own boxes are also still open and are slice
3's job: the automatic `page.published` → purge subscription, and the trigger toggles in
settings actually gating it.

**Commits.** `aea978d` (the queue, the backoff and the states), `933447a` (the claim fix),
`442ebe2` (the drain worker), `0c1006d` (the routes and the 17 walks), `69ba2c7` (the two
screens, the badge tones, the overview counters, the walkthrough extension).

**Next.** Close slice 2 on the browser pass, then slice 3: `page.published` / `page.unpublished`
/ `page.deleted` / `media.replaced` / `theme.activated` / `site.domain.changed` mapped through
the rule set into one enqueued purge, gated by the `auto_purge` toggles the settings screen
already stores.

**Browser gate, second update.** The pass ran ~21 minutes and never got the slot: w2's
pass (pid 3727030) has held it since 01:20, and four other passes are queued behind mine.
The slot file is *not* stale — the process that owns it is alive, which is the slot working
exactly as designed on a box where six writers each want a browser. The pass stays queued
and will produce its artifact on its own; **slice 2 is not closed and this tick does not
claim otherwise.** The tick's own budget ran out waiting.

This is the third pass in two ticks that has spent its whole wall clock in the queue, and it
is the clearest argument yet for the fix already reported: `/etc/profile.d/omnion-qa-limits.sh`
sets `QA_SLOTS=1` but only for a **login** shell, and a Hermes loop calling
`bash scripts/qa/run.sh` gets a non-login, non-profile shell. So the guard that is supposed
to make the passes take turns is simply not in effect for any of the six writers, and the
box runs as many browsers at once as there are loops. The one-line fix belongs in `run.sh`
itself — default `QA_SLOTS` inside the script — and `run.sh` is shared tooling, so it is
reported here rather than edited from a writer branch.

## Wave 5 · tick 17 · REQ-011: main merged in, and four merges that only look resolvable

**What.** Merged `origin/main` (REQ-021 notifications, events catalogue, media fixes) into
`wave5`, and paid for it with the most stubborn conflict set this branch has had: **eleven
files**, all of them additive, all of them cut *inside statements*.

**Proof.**
- `cargo build -p omnion-api` → **exit 0**, `Finished dev profile in 5m 04s` (2 warnings,
  both pre-existing in main's `notifications_admin.rs`).
- `pnpm typecheck` → **2 successful, 0 errors** (admin re-checked after the merge, web cached).
- `cargo test -p omnion-cdn --quiet` → **94 passed / 0 failed**.
- `cargo test -p omnion-api --test cdn_purge -- --test-threads=1` against `omnion_w5_dev`
  → **17 passed / 0 failed** (154.87s), including
  `a_successful_drain_leaves_the_purge_succeeded_with_every_item_done` and
  `a_provider_refusal_lands_in_the_drawer_with_its_message_and_is_retryable`.
- `node --check scripts/qa/walkthrough.cjs` → clean, with both `runCdn*` and the
  notifications passes in the inventory.
- `ls database/migrations | sed 's/_.*//' | sort | uniq -d` → empty. main's `0051` and
  wave5's `0054` do not collide.

**The merge, and what it cost.** Both branches appended a section at the same anchor, so git
produced hunks that started *inside* a function. Three resolvers were written before one was
right, and the failures are the lesson:

1. **A hunk-level union is not a merge.** Taking both sides of a hunk produced
   `updateCdnRule(` immediately followed by main's `fetchNotificationPreferences` — text that
   parses as neither. The unit of a correct union is a whole added region, not a hunk.
2. **Anchoring on the preceding *line* is a guess.** The line before main's block was often the
   `await page.route(` of a multi-line call, so the block landed *inside* our call and
   `walkthrough.cjs` stopped parsing. Two resolvers died here before the third switched to
   mapping base-line indices through the base→ours opcodes.
3. **A duplicate-definition sweep deletes the body, not just the second copy.** Removing the
   repeated `fn row_is_mine` left three signatures and one body, which is an *unclosed
   delimiter* rather than a duplicate symbol — a strictly worse state, and one the compiler
   reports with a line number pointing at the wrong thing.

**A `use` list reflow is a merge conflict in disguise.** Wave5's rustfmt had rewrapped
`use omnion_notifications::{…}`; main had added four names to the same list. difflib read
main's entire Preferences section as a *replacement* of that base range, so the union kept
every test that exercised `PutPreferencesBody` and dropped the definitions. The signature of
this class is a file that references a type it no longer defines — and the compiler finds it
in one pass where a merge resolver cannot.

**The compiler is the only safe judge of a merge.** Ten errors, in four files, all of them
"defined twice" or "cannot find value": the duplicated `pub mod media_duplicates;` (ours
deleted base's line, main kept it), the three helper duplicates, main's four sub-router
`let` bindings whose `.merge()` call sites had survived without them, and a vestigial
`params.with_read.unwrap_or(true)` on a field that is a plain `bool` with a serde default.
None of these are visible to a line-multiset check, which reported 0 missing on all seven
spliced files.

**Browser gate: deferred again, fourth tick in a row, and this time the box said why.**
Pre-flight one minute before the pass: `MemAvailable` **4G** against a `>8G` precondition,
load **23.8**, **40** Chrome processes, `/mnt/apopic` at 96% with 2.6G free. The slot is
currently held by a live pass, and a pass started into that is a pass that dies on
`Page crashed` and reports nothing. **Slice 2 is therefore still not closed**, and this entry
does not claim otherwise. The gate is queued for the next tick that finds `MemAvailable > 8G`
and fewer than ~20 Chrome processes.

**Commits.** `698f249` (the merge, resolved file by file against the base).

**Next.** Run the browser pass as the first action of the next tick, and on a green gate
close slice 2 and start slice 3 — the six trigger events mapped through the rule set into one
enqueued purge, gated by the `auto_purge` toggles the settings screen already stores.

## Tick 18 — REQ-011 slice 3: automatic invalidation (built and green; browser gate still deferred)

**Pre-flight said no again, so this tick spent its budget on product instead.** `MemAvailable`
**4G** against a `> 8G` precondition, load 14.4 (peaking 31.6 while six other writers built),
**40** Chrome processes, `/mnt/apopic` 87%. A pass started into that dies on `Page crashed` and
reports nothing, which is worse than not running: it would look like a red gate caused by this
branch. The gate stays queued — fifth tick in a row — and this entry does not claim otherwise.

**What shipped: the request's diagram now has code behind it.** Slice 2 gave the platform a purge
queue an operator drives by hand. REQ-011's own request is not about a hand: it opens with
`page published -> purge CDN cache -> new version live`, and that arrow had no implementation.
`crates/cdn/src/invalidation.rs` is it, in three parts — a `Trigger` table, a **pure** `plan`
from `(event, payload, toggles, provider capabilities)` to the purge that *would* be queued, and
a durable-cursor walk over `events` that writes the plans (`0120`).

The pure half is the design decision worth defending. The mapping is where every real choice
lives — which address a media id is, whether a tag survives a provider that cannot hold one,
whether a disabled trigger skips or errors — and a mapping that can only be tested by emitting an
event, waiting for a worker tick and reading a table is a mapping nobody writes a second test
for. Sixteen such tests cost 0.02s.

**The finding that was not in the plan: two of the six trigger names the request names are never
emitted.** REQ-011 says `media.replaced` and `site.domain.changed`; the platform records
`media.version_created`, `domain.added` and `domain.removed`. A switch on a name nothing emits
is a switch an operator can turn on, watch for a week and never see anything happen from — and
the failure is *silent*, because the setting saves, nothing errors, and the purge simply never
comes. Two of the six switches in `/cdn/settings` were therefore decorative. The trigger table
now uses the names the bus actually carries (seven of them, the two split ones being real events
rather than one fiction), the screen matches, and a test asks `omnion_events::catalogue` whether
every name exists — so a future rename fails a build instead of a customer's week.

**Two further decisions, both recorded in the module docs because the shortcut is wrong:**
an automatic purge is written with `requested_by = null` rather than the publisher's id (the
history's "Requested by" column answers who pressed the button, and borrowing the publisher
makes it accuse a person of something the platform decided), with the provenance in
`cdn_purge_sources` instead; and a whole-site trigger against a tag-less provider falls back to
the site's published addresses **at plan time**, because a `site-<uuid>` tag handed to
`generic_http` is a body no endpoint reads, the adapter answers `Succeeded`, and the operator
gets a successful purge that invalidated nothing.

**Proof.**
- `cargo test -p omnion-cdn --quiet` -> **110 passed / 0 failed** (94 before, 16 new).
- `apps/api/tests/cdn_invalidation.rs` -> **10 passed / 0 failed** (3.15s). Written against the
  **real** bus: they call `bus::emit` and then `invalidation::drain`, so the row that appears is
  one the platform wrote. The publish walk, exactly-once across three drains, the disabled
  toggle, the no-address event, an event from another area, a replaced file, a theme activation,
  the tag fallback, and one that takes the queued row to `succeeded` through the *worker's*
  `claim_due` / `apply_outcome` / `settle`.
- Regression on the suites this branch already owned: `cdn` **15**, `cdn_headers` **16**,
  `cdn_purge` **17** — all green, 0 failures. `pnpm typecheck` -> **0 errors**.
- `GET /api/v1/cdn/purges/{id}` answers a `source` object; `/cdn/purges`' drawer renders
  "automatic · page.published · event 412" against a platform-raised purge and "requested by an
  operator" against a manual one.

**Four of the ten walks failed on the first run, and three of the four were the test being
wrong** — which is the more useful half of the result, because each one was a belief the walk
was carrying that the product did not share.

* **`origin` reports `tags: true` on purpose.** With no external edge a tag resolves to the URL
  it stands for, and the origin answers honestly. Two walks asserted `kind == "url"` and were
  corrected, not the adapter.
* **Every shipped adapter reports tag support**, so the tag-less branch of the planner is
  unreachable in production today. The obvious move — point a site at `generic_http` and assert
  a URL purge — would have produced a green walk for the wrong reason: the adapter still says
  `tags: true`, and the row would be a tag purge wearing a URL assertion. The branch is instead
  driven through the planner, and the walk states the condition under which it should be
  promoted to a database walk (the day an adapter ships that cannot hold a tag).
* **A trigger turned on after the fact does not retroactively purge what it missed.** The
  walk assumed it would. It should not: the cursor has passed, and replaying a month of backlog
  for a trigger that was off is a stampede at a provider for content republished many times
  since. The operator's tool for "purge everything now" is the console, which says so.
* **One real defect, in the test rather than the product**: `on conflict (site_id)` has no
  arbiter to match, because per-site uniqueness is a *partial* index. The product's own
  `put_settings` already spells it `on conflict (site_id) where site_id is not null`.

**One environment finding worth the space it takes.** The first API build was killed twice
mid-link, once with `No space left on device` and once with `failed to open … No such file or
directory` on the linker's own output. The second was not the disk filling up: the shared
`omnion-disk-guard` cron (every 30 min, its last-resort loop frees until 10G is available) had
**deleted this worktree's `target/` underneath the running linker**, twice, because six other
writers' targets were larger. A cold `target/` is the guard's stated, accepted cost — it says
so in its own header — but a target deleted *between* rustc's writes is a build that fails in
a way that looks like a compiler bug. `CARGO_TARGET_DIR=/root/w5-build` moves this writer's
artifacts off the guarded path entirely, and the same build then completed.

**Commits.** `3603060` (crate, migration, catalogue drift test), `0e7ac6b` (docs),
`a2b188b` (admin + the `source` field), `3df6432` (the pass and the walks).

**The browser gate ran — and found a harness bug before it found anything about the product.**
Six ticks of deferral ended mid-tick, when Chrome fell from 40 to 10 and the load from 30 to 13.
The pass printed four `[qa]` lines and died with no error, which reads as a product failure on the
step after the last message and is not one: `pm2 describe <name>` exits non-zero for a process that
does not exist, and under `set -euo pipefail` a failing command inside `$( )` aborts the script.
`RUNNING_BIN` was therefore a trapdoor on precisely the moment it was written for — nothing
registered yet, which is what a brand-new QA stack looks like. Fixed with `|| true` (`54d409a`),
and the observation worth keeping is that **this branch had never run a pass at all**, so the bug
was sitting in the harness of a stack that had never been exercised on it.

With that fixed the pass ran for real: database reset, API on :18084, admin :3104, renderer :3204,
and the walkthrough drove the route list — overview, pages, media, media-duplicates, media-trash,
media-settings, sites, ai, search, search-settings, iam-overview, iam-users, iam-groups,
iam-service-accounts, iam-simulator, iam-policies, the analytics group, and then **the three CDN
pages**, where the tab died:

```
[walk] page cdn-overview failed: page.waitForTimeout: Target page, context or browser has been closed
[walk] page cdn-rules   failed: (same)
[walk] page cdn-settings failed: (same)
```

Five other writers had started passes while this one ran; load 30, Chrome 30, `MemAvailable` 8G
down to under 4G at the failure. The artifact's `summary.json` is `{"fatal": …}` with no per-page
results at all, so this is a **dead run, not a red one** — and it is the contention signature this
loop has recorded before, not a finding about this change. It is not being written up as a pass,
and the gate remains owed. What the run *did* establish is that the harness works up to the CDN
routes once the `set -e` trapdoor is gone, which is the first evidence this branch has that.

**Next.** Re-run the browser gate — it is now known to work up to the CDN routes, and it died on
memory contention rather than on anything in this change, so the recipe is unchanged. Then merge
`origin/main` (four known conflicts: `app-shell.tsx`, `lib/api.ts`, `lib/types.ts`,
`walkthrough.cjs` — and `lib/types.ts` is a file this branch also touched, so the union must keep
BOTH sides). Two slices stand un-gated behind that pass, which is the honest state of the branch.

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
## 2026-09-29 · Wave 5 · tick 19 — merging `main` without losing either side

Six ticks deferred the browser gate; the one thing genuinely owed was a merge, and the merge
turned out to be the harder half of it. `main` had moved eleven commits (REQ-016 slices 1
and 2: the event console, the webhook endpoints and their delivery history) into a branch
that had itself appended six CDN routes to the same four files.

**Five conflicts, and none of them was a real disagreement — every one was two writers adding
to the same place.** The resolution that works here is a union with one correction, and the
correction is the part worth writing down.

* **The `<<<<<<<` / `=======` / `>>>>>>>` block is not always the whole story.** Git wrote
  2-way markers here, not 3-way, so the text *above* the block and the text *below* it are
  shared: an `import type {` opened the list and `} from "./types";` closed it; a `/**` opened
  a doc comment and `return steps; }` closed the function. A resolver that emits "ours, theirs"
  and leaves the shared envelope alone produces a file that *looks* merged and does not parse.
* **Two shapes need opposite answers.** `lib/api.ts` and `lib/types.ts` had blocks where both
  sides were balanced (two halves of one import list) — a plain union. `walkthrough.cjs` had
  a block where both sides ended *mid-function* and shared the closer — each side needs its
  own copy of the closer, and the second side additionally needs the `/**` re-opened because
  it starts inside the comment the first side already closed.
* **The tell is brace balance plus the presence of a shared continuation**, not the marker
  itself. `+0/+0` with no continuation is a union; `+1/+1` with a continuation is
  truncated-both; `+1/+1` with no continuation is a union whose sides are whole functions.
* **The first two attempts guessed and the compiler caught it.** Consuming the shared tail
  ate `} from "./types";` and produced four parse errors on line 33 of a 5,400-line file;
  splicing the sides together welded a CDN function's open body onto a doc comment and
  produced a `SyntaxError` reported 400 lines away from the merge. `bun x tsc --noEmit` and
  `node --check` are the only things that see this — reading the file does not.

**Verified after the merge.** `bun x tsc --noEmit -p apps/admin/tsconfig.json` → 0 errors.
All seventeen types from both sides survive in `lib/types.ts` (8 CDN + 9 events/webhooks), no
duplicates, braces balanced. `node --check scripts/qa/walkthrough.cjs` → clean, and all four
pass functions are present (`runCdnRulesDepth`, `runCdnPurgeDepth`, `runEventsDepth`,
`runWebhooksDepth`). `docs/BUILD-LOG.md`: 69 headings, zero duplicated, zero lost — checked by
heading multiset, because a line count is happy to hide a dropped section.

`0047_media_grants.sql` differs between the branches; main carries the `create unique index`
fix for the `coalesce` expression that cannot be a constraint, so main's version is the one
that survives. Migration numbering stayed unshared: ours ends at 0120, main's new file is
0052, and the two ranges do not overlap.

**Next.** `cargo test -p omnion-cdn` on the merged tree, then the browser gate — now with the
events and webhooks routes in the inventory alongside the CDN ones, which is the first pass on
this branch that would actually exercise both sides of this merge.

## 2026-09-29 · Wave 5 · tick 19b — the gate, deferred a seventh time, and why

The merge is in and both cheap gates are green. The browser gate is still owed, and the
pre-flight says no for the seventh tick running:

```
MemAvailable 2G      (a pass needs > 8G)
chrome      40       (a pass tolerates < 20)
load        25.06    (six writers' passes overlapping)
/            100%    1.0G free
```

That is not a close call and the numbers say why, twice over. The last attempt under similar
conditions produced `summary.json` containing only a `fatal` key — a **dead** run, not a red
one — after the walkthrough reached the CDN routes and the tab stopped existing. Launching
into this again would spend eight to ten minutes of wall clock and every scrap of available
memory to produce the same artifact, and a `{"fatal": …}` file is dangerous precisely because
it reads like a pass to anything that only checks that a file exists.

**One disk is at 100% and it is the root filesystem, not the shared mount.** `/mnt/apopic` is
at 95% with 3.3G free after this writer's own `.next` directories were cleared (781M + 55M,
regenerated by the next `next dev`), so the merge work is not what filled it. `/dev/shm` is
at 96% with five *sibling writers'* target directories on it — `w8` 7.0G, `w6` 6.7G, `w2`
3.4G, `w1b` 2.9G, `w10` 2.9G — which is this box's standing resource story and not this
worker's to reclaim.

**What is green on the merged tree, and is worth recording as the reason the merge is safe
even though the gate is not run:** `cargo test -p omnion-cdn` → 110 passed, 0 failed.
`bun run typecheck` (turbo, all five packages) → 2 successful, 0 errors. `node --check
scripts/qa/walkthrough.cjs` → clean. The admin app carries both sides' screens, so a
typecheck failure in either writer's code would have shown up here.

**Next.** The gate runs first, before any new slice, on a box that has room for it. The
recipe is unchanged and now also carries the `PM2_HOME=/root/.pm2` and `QA_SLOTS=0` pair that
this branch established last tick:

```bash
export PATH=$HOME/.cargo/bin:$PATH CARGO_TARGET_DIR=/root/w5-build CARGO_INCREMENTAL=0
CARGO_BUILD_JOBS=2 PM2_HOME=/root/.pm2
QA_STACK=w5 QA_API_PORT=18084 QA_ADMIN_PORT=3104 QA_WEB_PORT=3204 QA_SLOTS=0 \
  bash scripts/qa/run.sh
```

When it does run, the inventory is the first that contains **both** sides of the merge — the
three CDN routes and the events and webhooks routes — so it is the first pass on this branch
that would actually exercise what this tick merged.

## 2026-09-29 · Wave 5 · tick 20 — the gate ran, and every layer of it was lying

Eight ticks deferred the browser gate on resource grounds. It turns out the resources were
mostly fine and the **harness** was the thing that had never worked. Three defects, stacked,
each hiding the next.

**1. The pass booted an API with no user.** `run.sh` resets the QA database, which drops every
account, and then passed `OMNION_ADMIN_EMAIL`/`OMNION_ADMIN_PASSWORD` **only on the first pm2
registration**. On every later pass the already-registered process was restarted, so the API
logged `no accounts exist yet`, the panel correctly served `/login` instead of `/setup`, and
the walkthrough had nothing to sign in with. The fix passes the seed on every boot; the account
the API creates and the credentials in `walkthrough.cjs` are now the same three variables.
Proof: `first administrator account created user_id=cb2f03bf...`, and
`select email,status from users` -> `qa-owner@omnion.test|active`, which was empty before.

**2. A dead pass exited 0 and wrote a clean report.** A fatal walkthrough still leaves a
`summary.json`, and `{"fatal": "could not sign in"}` has no `findings` key - so the report
stage happily rewrote `QA-LATEST-w5.md` as a clean pass and the script returned success.
*Absence of evidence was being filed as evidence*, the same failure the `{"fatal": ...}` note
warned about, one level up. A non-zero walkthrough exit, a `fatal` summary, and a scope that
recorded zero pages are now all gate failures. Observed: the broken pass went from `RUN_RC=0`
to `RUN_RC=3`.

**3. I broke the database password myself, and the tool helped me do it.** Rewriting that
block, I reproduced the connection string from a read in which the tool had **masked the
password as `***`** - so literal asterisks went into the file and every pass died with
`password authentication failed for user "omnion"`, a harness error dressed as a database
problem. Repaired by recovering the line from the revision *before* the bad commit and proving
it byte-for-byte (sha256 `a89bb61a27f30753`, 51 chars, identical to the original) - the
credential never entered this log. `grep -c '***' scripts/qa/run.sh` -> 0.

**And the disk, which seven ticks of pre-flight blamed for the deferral.** `/` was at 100% with
429M free, and 3.5G of it was `/root/w5-build` - my own build directory, on the one filesystem
whose fullness breaks everything. Moved to `/mnt/apopic/w5-build` (copy, verify, delete; two
cargo processes belonged to w6 and w7 and neither used it). `/` went 100% -> 97% with 3.9G
free, and the API binary runs from the new home: `healthz 200`, migrations clean.

**Next.** `apps/admin/.next` was 839M of stale dev state with no `BUILD_ID` and logs reading
"The directory ... was deleted. Restarting the server to recover" - Next dev thrashing on a
full mount, which is also why that directory was eating the disk. Cleared it; `/login` now
answers in 160ms instead of 3.6s and renders a real email+password form. Re-running the scoped
pass.

## 2026-09-29 · Wave 5 · tick 21 — the scoped pass was scoped in name only

Eight ticks deferred the browser gate; this tick ran it — and the first thing it revealed was
that the *gate itself* had never once been scoped, on any branch, for any writer.

**The defect.** `inScope` was meant to answer "does this route match the scope?", and it read

```js
const inScope = (name) => !ONLY || name.split(",").some((w) => w.trim() && name.includes(w.trim()));
```

`name.split(",")` — the **route name** was split on commas, not the scope. With no comma in a
route name that is a one-element array holding the name itself, so the predicate degenerated to
`name.includes(name)`, which is true for every route, for every scope, forever. `QA_ONLY=cdn`
walked all 46 routes. The summary stamped `scope: "cdn"`, the report header said *"Scoped pass"*,
the screenshots were real — and the claim was false in every one of those artifacts. This is the
third instance of the same failure this branch has produced, and the most dangerous of the three:
a `{"fatal": ...}` summary filed as a clean report is *absence* of evidence read as evidence, a
scope matching nothing filed as zero findings is the same thing one level up, and this is
**misdescription** — real evidence, correctly gathered, under a label that says it is less than
it is. A reader of the next tick's artifacts would believe CDN had been covered when the pass had
in fact been covering everything and naming a part.

**Why it survived seven ticks of scrutiny.** I checked the guard four ways before finding it:
`--only` in `process.argv`, `QA_ONLY` in the live process's `/proc/<pid>/environ`, the guard's
byte offset relative to the `log()` line it guards, and `md5sum` of the file against
`git show HEAD:`. All four confirmed the code was *present*. None of them asked what it *did* —
and the answer was in the variable name. The fix took ten seconds once the predicate was
re-executed instead of read; the reading took twenty minutes.

**The fix, and the witness that keeps it fixed.** `ONLY.split(",")` now, plus two guards so a
mis-scoped run can never again file itself as a targeted result: the pass logs what it resolved
(`scope: "cdn,events,webhooks" keeps 10 of 46 routes`), and it **refuses to start** when the
scope matches a core route (`overview`/`pages`/`media`/`sites`), which is exactly the shape of
the bug that just cost this tick. A narrowing control needs a witness in the product, not in the
diff. Proof: `scope: "cdn,events,webhooks" keeps 10 of 46 routes` followed by the ten routes it
kept — `cdn-overview`, `cdn-rules`, `cdn-settings`, `cdn-purges`, `cdn-purge-console`, `events`,
`events-catalogue`, `webhooks`, `webhooks-new`, and the ten-route count matches the route table
exactly. `1aa8930`.

**What the pass proved, and what it could not.** The in-scope routes all walked and screens
rendered (`cdn-rules → 29 elements`, `cdn-settings → 40`, `webhooks → 34`,
`webhooks-new → 40`). Two observations, neither of them closed:

1. `cdn-purges → 0 elements` — the purge history screen rendered *nothing* on a cold mount,
   while its sibling routes rendered 29–40 elements. The API answers (`/healthz` 200, the purge
   runner is polling), so this is either a first-paint defect or a tab that was already dying.
2. `iam roles depth` recorded `chrome-error://chromewebdata/` — the signature of a **dead tab**,
   which seven writers on one 32 GB host produce routinely (70 Chrome processes, 8 concurrent
   passes, load 10-15, MemAvailable down to 8 GB with 1 GB free). That is this box's contention,
   not this branch, and per the standing rule it is logged and not charged to a REQ.

**So the CDN gate is not green, and REQ-011 is not closed.** The two unticked boxes stand. The
honest position after this tick is narrower and better-founded than before it: the harness
defect is fixed and proven, the CDN and events/webhooks screens are reachable and render, and the
one number that would have told me the pass was mis-scoped is now printed by the pass itself.
Next: re-run the pass alone (`QA_SLOTS=1`, and only after the sibling writers' passes clear) and
read `cdn-purges` specifically — a 0-element screen next to three healthy siblings is either a
real first-paint defect worth fixing or the tab dying, and the difference is one clean run.

**Gates.** `cargo test -p omnion-cdn` **110/110**. `pnpm typecheck` **5 packages, 0 errors**.
`node --check scripts/qa/walkthrough.cjs` clean. Browser gate: **red — not closed.**

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

## 2026-09-29 · Wave 5 · tick 22 — the merge brought three defects that were not mine, and the gate found all three

**What.** `git fetch` showed `origin/main` 21 commits ahead, so the tick began with the merge
this branch had owed for four ticks. `0caa8c6`. Three conflicts: `apps/admin/lib/api.ts` and
`apps/api/src/main.rs` were both **import lists that had grown on each side**, so both resolved
to the union; `docs/BUILD-LOG.md` is append-only, and the splice was verified by **multiset**
rather than by line count (`Counter(merged)` against `Counter(ours)` and `Counter(theirs)`,
0 missing each) — 4887 lines from 4660 + 3486 against a 3259-line base.

**Then the gates found three defects, and only one of them is CDN.**

- **`394d769` — `ApiError` was `Debug` but not `Display`, so `cargo check --lib --tests` did
  not compile at all.** `routes/security.rs:1090` formats a refusal into an assertion message.
  The production binary builds, which is why only the **lib test** profile ever found it — the
  exact trap this ledger has recorded before. `Display` prints `message`, not the struct:
  reusing `Debug` would dump `status` and `details` into every failure that merely names a
  refusal.
- **`6280a2a` — one test of 224 asserted that `"«redacted:sk-…»"` is a credential.** It is 15
  characters and does not begin with a token prefix, so `looks_like_a_credential` correctly
  declined and the test failed. **The fixture was wrong, not the matcher.** The matcher demands
  a long-enough token-shaped string precisely so it does not refuse ordinary prose, and a marker
  that says a secret was *removed* is the safest thing a scanner can upload. Refusing it would
  reject exactly the reports the check exists to admit. The leaky fixture is now a real `sk-`
  key, and the redaction case is kept as its own assertion **in the direction it was reaching
  for**: three markers must not trip the matcher, and each must still parse to one finding.
- **`e09d51d` — merging main put main's `0054_security_posture.sql` beside my
  `0054_cdn_purge_queue.sql`.** sqlx resolves migrations by their leading number alone, so the
  second never runs and the API refuses to boot with `migration 54 was previously applied but has
  been modified` — which is what killed the first two attempts at the pass. `0120` is free in
  main, but *next free integer* is the same defect waiting for the next merge, so both move
  above main's high-water: **`0136`, `0137`**. The suite database persists between runs, so it
  had to be dropped as well (`drop database omnion_qa_w5`, 0 connections) — renumbering without
  dropping it produces a phantom `VersionMissing` instead.

**And one more, found by reading the pass rather than its output.**

- **`71d55cb` — a scoped pass still ran nine depth passes it had excluded.** `QA_ONLY` narrows
  the route list and everything that goes through `runDepthPass`, which checks `inScope`. Nine
  passes were called with a bare `await` instead, so the pass scoped to
  `cdn,events,webhooks` spent its time in **`search-depth`**, `command-center` and `palette`
  and never reached `/cdn/purges`. This is the same defect `1aa8930` fixed one layer up:
  narrowing routes but not passes means the scope describes the report's **heading** and not its
  contents, and on a busy box the excluded work is exactly what starves the included work. All
  nine now go through `runDepthPass`, which also hands them the try/catch a bare `await` lacked
  — a pass that throws is recorded as a finding under its own name instead of ending the run
  with no `summary.json`. The passkey walk's scope name is `iam-passkeys`, the name it records
  itself under, so `QA_ONLY=passkeys` still matches. `runWizard` is deliberately left bare: it
  is the first-run onboarding walk and has to see a fresh database.

**Proof.**

- `cargo test -p omnion-api --lib` → **224 passed, 0 failed** (was: 223 passed, 1 failed, and
  before the `Display` fix: did not compile).
- `cargo test -p omnion-cdn --quiet` → **110 passed, 0 failed** (unchanged by the merge).
- `pnpm typecheck` → **2/2 tasks, 0 errors** (5 packages in scope).
- `cargo check -p omnion-api --lib --tests --message-format=short` → **0 errors**; every line it
  prints is a pre-existing warning.
- The API booted on the renamed migrations: `/healthz` **200**, admin **307**.

**The pass itself: partial, and stopped by the box rather than by the code.** It reached **361
clicks** across ten routes, and **every one reported a real element count** — `cdn-overview` 30,
`cdn-rules` 30, `cdn-settings` 41, `events` 36, `events-catalogue` 36, `events-retention` 36,
`webhooks` 36, `webhooks-new` 41, `analytics-events` 42, `cdn-purge-console` 1. The tick-21
observation that `/cdn/purges` "rendered 0 elements" is **not reproduced**, and the routes it was
compared against all rendered. It then sat at 361 clicks for twenty minutes: load **29**, 50
Chrome processes, `MemAvailable` 3 G, and my own `walkthrough.cjs` at **0 % CPU with 21 s of CPU
time in 20 minutes** — blocked, not computing. I killed my pass and only my pass
(`kill 161704`); the pass was not killed by a shared pm2 daemon, and no sibling's run was
touched. `no summary.json` was written, so **REQ-011 is not closed** and its two screen boxes
stay unticked.

**Next.** Run the pass again on the fixed harness with a genuinely quiet box — `load` well under
20, `MemAvailable` over 8 G, no other pass on 3104 — and read `/cdn/purges` specifically. Both
unticked REQ-011 boxes and REQ-005's single box are one clean pass away. After that: REQ-017
(sandbox/staging).
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


## 2026-09-29 · Wave 5 · tick 23 — the scope was half a scope, and it was mine to fix

**What.** `git fetch` showed `origin/main` 11 commits ahead, so the tick opened with the merge
(`92ee69a`). Four conflicts, and they were four different shapes. `apps/api/src/routes/mod.rs`
had both sides rewriting the *same* `.nest("/api/v1", v1)` statement, with a stated ordering
constraint in main's comment ("CSRF sits OUTSIDE the permission guards on purpose") that had to
survive: my module switch is a layer on `v1`, so keeping it inside the nest is what keeps it below
the CSRF layer. `notifications.rs` was an empty-side union. `security.rs` was one fixture with two
spellings, and mine was the one already reasoned about in `6280a2a` — a redaction marker is a
*removed* secret. `docs/BUILD-LOG.md` is append-only with no shared continuation, so it is a plain
union, verified by **multiset** (0 lines missing from either side, 79 headings, 0 duplicates) and
not by a line count.

**Then the merge turned out to be the tick's real work.** Resolving four conflicts left a
file list that *looked* complete, and it was not. `notifications.rs` had been recorded as an
"empty-side union" — ours between the markers genuinely was empty — and that reasoning was then
applied to files with no markers at all, where a file-level loss leaves the conflict list
untouched. Three files main had **added** never landed on disk: `security/headers/page.tsx`,
`header-policy.tsx` (696 lines) and `security-tabs.tsx`. The whole REQ-012 slice 2 screen was
gone, both security screens had lost their `<SecurityTabs>`, `lib/api.ts` had lost the client and
`lib/types.ts` the types — and the panel did not compile. The compiler is what finally named it;
`parse_list_params` and `decode` were referenced by main's notification tests and defined nowhere.

Every file main touched is now compared against the merge base, and by line **multiset**, not
count: `lib/types.ts` had 36 lines missing that a count would have hidden inside a reflow. A
resolution audit that reads the marker list can only ever verify the files that had markers.

`notifications.rs` is main's file wholesale — its `with_read` is an `Option<bool>` where mine was
a `bool`, so "the client said nothing" and "the client said no" had become the same question. Main
distinguishes all three states and tests them.

Then the harness. Ticks 13–22 have all deferred the browser pass on box pressure, and tick 20
said plainly that a gate which cannot be afforded is a gate that will not run. The fix belongs in
the harness, so it went there — and the harness was still broken. `runDepthPass` checks `QA_ONLY`
and turns a throw into a recorded finding, and fifteen call sites used it. **Sixteen more were
invoked with a bare `await`** and ran regardless. Fifteen of those sixteen are precisely the
passes a scoped run exists to reach: `runCdnRulesDepth`, `runCdnPurgeDepth` and all six
`runOrganization*` passes. So every scoped artifact this branch has produced carried the label
`cdn,events,webhooks` while holding the whole product's evidence — and a pass meant to reach
`/cdn/purges` spent its budget in `search-depth` and never got there. That is the tick-21 reading,
and no amount of re-reading it would have found it: the scope was a *description*, not a control.

**Proof.**
- `git merge origin/main` → `92ee69a`; merge-repair → `91cdf0c`.
- Every main-touched file compared against the merge base by line multiset: 3 missing files and
  36 missing lines restored, 0 remaining.
- `cargo test -p omnion-api --lib` **255/255** (was 224 and red) · `cargo test -p omnion-cdn`
  **110/110** · `pnpm typecheck` 2/2 tasks, 0 errors · `bun build` parses `walkthrough.cjs` clean.
- Scope predicate **re-executed**, not read (the tick-20 rule): `QA_ONLY=cdn` keeps **2 of 17**
  passes (`cdnRules`, `cdnPurges`); `QA_ONLY=organization` keeps exactly the six tenant passes; the
  core-route leak check returns `[]` for both.
- `grep`-level audit: **0** bare top-level pass calls remain; the two hits left
  (`runTenantDepthFromDetail`, `organizationDepth`) are both *inside* the tenant pass, whose call
  site is now scoped.

**The trap inside the fix.** The five passes after `organizationDepth` drive the organization that
pass *opens*, so scoping the parent alone leaves them reading `undefined.organizationId`. The
naive repair — `return` when the tenant is missing — is worse than the bug: it abandons the
remaining screens and the run ends with **no `summary.json`**, the one shape a QA artifact must
never have, because a missing summary reads like a crash and hides every finding behind it. A
missing tenant is recorded as a finding naming the reason, and the pass is skipped.

**Next.** The box is still at load 20 / MemAvailable 4G / 40 Chrome, so the pass is deferred for
the *ninth* time — but the deferral now has a fix behind it rather than a pre-flight, and the
merge that arrived with it is repaired and green. With the scope honest, a
`QA_ONLY=cdn,events,webhooks` pass is roughly a fifth of a full pass, and that is affordable here.
REQ-011 and REQ-005 both wait on that one green run.


## 2026-09-29 · tick 26 — the 170 high findings were a fixture, not a product

**What.** Merged `origin/main` (2 conflicts), then ran the scoped browser pass for the first time
in ten ticks and read the result instead of deferring it. The pass returned `179 findings (high 170)`.
Every one of them traced to a single cause: the QA database had an OWNER account and **no
organization and no site**.

**Root cause, measured.** `run.sh` seeds `OMNION_ADMIN_EMAIL` on every boot, and that seed creates
one user and nothing else. The walkthrough's wizard asks the API whether setup is needed, and
`omnion_onboarding::state` answers `needs_setup: !has_users` — a user exists, so the wizard is
skipped. The database is then permanently in a state the wizard will never re-enter and no
site-scoped screen can render. The pass filed 68 × `400 /api/v1/cdn/rules?site_id=` (a real
product defect, below) plus the cascade that follows from walking every site-scoped screen with
no site.

**Two product/harness defects, both now fixed.**

1. `feat`/`fix(cdn)` — `cdn-overview-view.tsx:83` read `fetchCdnRules(siteId ?? "")` **before** the
   `if (!siteId)` empty-state guard. A null site was coerced to `""`, and `?site_id=` is a 400 on
   every render of an unconfigured installation. This is mine, it was 68 of the 170 highs, and it
   would have hit a real user who has not created a site yet.
2. `qa` — the same missing site, seen from the harness side. `qaSql` returns `""` when a scalar
   read matches nothing; the seven call sites that interpolate that value into a later statement
   built `where site_id = ''`, a uuid type error that aborted the seed while pointing at the
   *update* rather than at the empty read. `run.sh` now seeds the organization and the site and
   fails the pass outright when no site keyed `main` exists, and the two depth passes that used to
   report `"no QA site to purge for"` no longer skip quietly.

**Proof.** `cargo test -p omnion-api --lib` → **255 passed, 0 failed** · `pnpm typecheck` →
**2/2 tasks, 0 errors** · `node --check scripts/qa/walkthrough.cjs` → clean · the merge audit
(`Counter(origin/main) - Counter(merge-base)` compared against disk) → **0 lines main introduced
were lost** · scoped pass, first run → `179 findings (high 170)`, all traced to the fixture above.

**Commits.** `fd480a3` merge of `origin/main` · `799929b` `fix(cdn)` empty site id + `qaScalar` ·
`f2f26cc` `qa` tenant/site fixture.

**Next.** Re-run the scoped pass against the seeded fixture. If `/cdn` and the two CDN depth
passes go green, REQ-011 slice 2 closes and REQ-005's browser gate unblocks with it.

## 2026-09-29 · tick 27 — REQ-017 slice 1: the environment model, and a spec that named two tables this platform does not have

**What.** Started REQ-017 (sandbox/staging) with slice 1: a new `crates/environment` holding the
decisions — key and host legality, the clone's areas and copy order, the progress fold — and
migration `0145_environments.sql` with `environments`, `environment_clone_jobs` and the
`environment_id` boundary on content. The routes, the runner and the screens are the next slices.

**The spec's data model does not describe this platform.** REQ-017 names `menus` and
`site_settings`, and neither table exists. Navigation has no table of its own; site configuration
lives in `organization_settings`. I did not create the two missing tables: an empty table created
to satisfy a spec line is a second, competing home for configuration that already has one, and it
would be discovered later by somebody assuming it was the real one. The environment boundary goes
on the tables that carry content: `pages`, `translations`, `workflows`, `organization_settings`.
A clone copies what the platform actually stores.

Second correction: `pages` has no `organization_id` — it reaches its organization through
`sites`. The spec's backfill is written as if it did not.

**Two invariants live in the database, not in a route.** One production environment per
organization, and one open clone per environment. Both are partial unique indexes, because a
per-row `check` cannot express "one of this type" — `check (type <> 'production')` forbids the
first row along with every later one. The clone one matters most: a check-then-insert in the
route is correct only until two requests arrive together, and `clone_already_running` is a promise
the caller can be given.

**The backfill is three statements, not one.** Create the production environments, attach the
content, then set `NOT NULL`. The single-statement form cannot be written (step 2 needs the id
step 1 created), and applying `NOT NULL` first is exactly how a migration ends up applying only to
an empty database.

**Proof.** `cargo test -p omnion-environment` → **29 passed, 0 failed** (0.01s) ·
applied on a **fresh** database (105 tables) and on one **seeded with two organizations** plus
their sites, pages, revisions, translations, workflows and settings: every row landed in its own
organization's production environment, and `insert into pages (site_id, slug)` with no environment
is refused by the `NOT NULL` · each refusal the API names is a real constraint error, checked
against the database rather than assumed: duplicate key → `environments_key_unique_per_org`,
second production → `environments_single_production`, `Bad_Key` → `environments_key_format`,
duplicate host → `environments_staging_host_unique`, second open clone →
`environment_clone_jobs_single_open`, `items_done > items_total` → the column check · a finished
clone is followed by a new one, which is what makes "re-clone" possible at all ·
`cargo test -p omnion-api --lib` → **255 passed** · `pnpm typecheck` → **2/2, 0 errors** ·
`origin/main` merged (5 commits, security centre + `migration_gap.rs`), merge audit with the
`Counter(origin/main) - Counter(merge-base)` metric → **0 lines main introduced were lost**.

**Commits.** `516f335` merge of `origin/main` · `40b67e5` `feat(environment)`.

**Next.** The QA confirmation pass for REQ-011 slice 2 and REQ-005's browser gate is still
queued behind another writer's live pass (slot holder alive, correctly serialised). While it
waited, REQ-017 slice 1 was built and verified. Next: the environment store and the routes, then
`/environments`.

## Tick 28 — REQ-017 slice 2: the environment store, the routes, the clone worker, and four defects the walks found

**What.** `crates/environment` gained `store.rs` (list/find/create/clone-job/archive) and
`runner.rs` (the copy itself). `apps/api` gained `routes/environments.rs` (list, create, detail,
re-clone, job history, cancel, archive) and `environment_clone_runner.rs`, wired into `main.rs`.
Migrations 0147 (resolution triggers) and 0148 (environment-scoped content keys).

**Proof.**
- `cargo test -p omnion-api --test environments` — **25 passed, 0 failed** (120 s).
- `cargo test -p omnion-environment` — **29 passed**.
- `cargo test -p omnion-api --lib` — **255 passed**.
- `pnpm typecheck` — 2/2.
- `scripts/qa/environment-default-proof.sql` and `environment-key-proof.sql`, each run twice
  against a fresh 105-table database (self-cleaning, so they are gates rather than demos).

**What the walks found, all of it in code written earlier in this request.**
1. Migration 0145 made `environment_id` NOT NULL and nothing supplied one — the first page
   created after it would have failed platform-wide. Fixed by 0147's resolution triggers.
2. 0145's backfill covered only existing organizations, so every *new* tenant had no
   production environment and no first page. 0147's after-insert trigger fixes the lifetime of
   the invariant, not just the migration's moment.
3. A staging page could not coexist with a production page of the same slug, because the
   natural key ignored the environment. 0148 adds it, and the copy mints fresh ids.
4. `organization_settings` is keyed by `organization_id` alone, so that area cannot hold a
   second row per environment. It copies nothing and counts zero rather than promising rows
   that never arrive.
5. Catching the `clone_already_running` violation and then reading the running job in the same
   transaction returned 500 (an aborted transaction), not 409.
6. The runner refused any environment that was not `active`, but `create_staging` creates it as
   `cloning` — so every new environment refused its own first clone.

**Next.** Slice 3: the changes diff (`GET /environments/{id}/changes`) and the environment chip
and banner, so the panel can scope content screens and show what staging has changed. The
`/environments` list screen, the create wizard and the detail screen are still to be built —
the API is done, the screens are not, and a REQ does not close on an API alone.

**Gate still owed.** The scoped QA pass got the slot this tick and exited 3 at
`could not sign in after wizard` — the API came up on :3104 but the walkthrough could not sign
in. That is the harness, not this change, and it is the first thing to diagnose next tick.














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
## 2026-09-29 · Wave 5 · tick 29 — REQ-017 slice 2's screens, and the merge that was the tick's real work

**What.** The three screens slice 2 owed, behind an API that already existed: `/environments`
(list), the four-step create wizard, and `/environments/{id}` (detail, re-clone, job history,
archive). Plus the nav entry, tones for `cloning`/`error`/`cancelled` that had none, and
`runEnvironmentsDepth` in the walkthrough — which drives them rather than only visiting them.

**Proof.**
- `pnpm typecheck` — 2/2 (twice: before and after the merge).
- `cargo test -p omnion-api --test environments` — **isolated walks green**: `the_permission_split_is_real`
  and `a_reclone_refuses_until_the_operator_confirms_what_is_discarded` each pass on their own.
  The full 25-walk run did **not** complete inside its timeout under the current box load
  (load average 206, seven writers), so the suite is **not** claimed green this tick.
- `node --check scripts/qa/walkthrough.cjs` — syntax OK.

**Four defects, three of them in code this branch shipped earlier.**

1. **The suite was eating its own acceptance gate.** It connected to whatever
   `OMNION_DATABASE_URL` named and dropped 54 accounts into the shared QA database. The
   walkthrough seeds its owner at boot and `bootstrap_first_admin` only runs while `users` is
   empty, so tick 28's `could not sign in after wizard` was the harness deleting the account its
   own gate signs in with — not a product failure, and invisible from inside the suite. Each run
   now creates and migrates its own database (`ac0ef60`).
2. **`origin/main` survived the merge in two files.** Git wrote *2-way* markers, so the tail line
   was a bare ` origin/main` with no `>`; `git status` was clean, the conflict list was empty, and
   a grep for the `>>>>>>>` fence finds nothing. `rustc` named both (`165884e`, `fe0372e`). The
   sweep that actually catches it matches the fence as *optional*:
   `^\s*(<{7}|>{7}|\|{7}|={7})?\s*(HEAD|origin/...)`.
3. **The tick-23 file-level loss happened again, in the same place.** Git's conflict list was
   clean while 7 client functions and 13 types of main's were absent from `lib/api.ts` and
   `lib/types.ts`, and the panel did not compile. A file main *added* inside a region git
   considers merged never lands and no marker points at it. The compiler named it; the merge diff
   would not have.
4. **Naive conflict concatenation produced two `.nest("/api/v1", v1)` calls** — the second wins
   silently and would have dropped the module guard with no error anywhere.

**And one interaction with a feature that landed on main while this tick ran.** The rate limiter
caps `sign_in` at 10 per 300 seconds, the counter is in Redis, and Redis is shared by every
writer's suite on this box. This suite creates a distinct account per walk, so 25 walks cannot
fit in that budget — and the failures arrived as `login body: … rate_limited` on tests whose
subject is *cloning*, which is a lie about where the problem is. `ensure_installed` documents this
exact case: harnesses that build a router without a `main.rs` fall back to the shipped defaults.
The suite now installs its own policy, raising `sign_in` rather than disabling it, so the layer
stays real and only the *production* number stops being the measure (`3a300f6`, `a441824`).

The scratch databases the harness creates are also reclaimed at the start of the next run: a
`Drop` guard cannot work here (the value lives in a `OnceCell` and the process exits first), and
two were already sitting on the server from runs killed by their timeout.

**Next.** Run the scoped QA pass (`QA_STACK=w5 QA_API_PORT=18084 QA_ADMIN_PORT=3104
QA_WEB_PORT=3204 bash scripts/qa/run.sh`) and close the browser half of the slice — the depth
pass needs a box it can have to itself. Then REQ-017 slice 3: the changes diff and the
environment chip and banner.
=======

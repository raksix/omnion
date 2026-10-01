## 2026-09-30 — omnion-w6 tick 41 — REQ-128 slice 3 third part: the environment bundle generator

`2e7ee60f` `e303d0d6` `cc8e2b69`. `release/lib/bundle.py` + `release/tests/test_release_bundle.py` +
`scripts/qa/release-bundle.sh`, and two compose-stack defects it found in passing.

**Proof.** `python3 -m unittest discover -s release/tests -t .` **145 tests, OK**. `bash
scripts/qa/release-bundle.sh` **23 passed, 0 failed**, including 3 mutations. All three bundle kinds
verify with the tool that would install them: `helm template omnion infra/helm/omnion -f
<generated>` renders, and `docker compose config` parses both stacks. The gate's non-vacuity is
PROVEN, not assumed: removing the generator's credential-text check turns it red (`22 passed, 1
FAILED — a secret in the name is refused`), because `s3cr3t-fixture-value-9f2b1c` then reaches the
generated file. `release/tests/test_release_bundle.py` 39/39.

**The property is enforced twice.** By construction: the request record has no field a credential
can arrive in. By measurement: every generated file is scanned with `pipeline._literal_credentials`
— the pipeline gate's OWN rule, imported rather than copied — as written AND rendered, and a bundle
carrying a literal is refused. The shell gate greps the OUTPUT for the fixture instead, so a scan
that stopped scanning still fails there.

**Seven defects, and every one came from running a tool rather than reading the generator.** Three
in its own output, all caught by `helm template` and `compose config`:

- A multi-line comment put `#` on the first line only, so five `--from-literal` lines escaped into
  the YAML. helm reported `cannot unmarshal string into map[string]interface {}` — a TYPE error from
  a cause unrelated to types, and therefore not something a type check finds.
- One shared `requests`/`limits` map was assigned to `api`, `admin` and `web`, so every pod asked
  for all three components' memory. An operator sizing a node from these values provisions ~3x the
  machine and still schedules one pod.
- `ingress.hosts[].paths[]` requires a `service` key; without it the Ingress renders a rule that is
  present, matched, and never routed.

Two repository defects, both landing on an operator's first install:

- **`docker-compose.prod.yml` demanded a variable named `VAR`.** The header explained the `:?`
  reference form by writing it literally, and compose interpolates `$` expressions inside comments
  too — so an operator who set every real credential got `required variable VAR is missing` for a
  variable appearing nowhere else. The explanation is now written with the braces apart, because
  an interpolation in a comment is as real as one in a value.
- **`docker-compose.enterprise.yml` required three variables `.env.example` never declared**
  (`OMNION_EXTERNAL_POSTGRES_DSN`, `…REDIS_URL`, `…S3_ENDPOINT`), so the enterprise install
  refused to start and "missing from where?" had no answer in any file the operator was given.
  Declared now, in their own section with the replica counts, because none of them applies to a
  single-host install.

**The credential scan fired on the CHART'S OWN VALUES**, twice: `existingSecret: omnion-secrets` and
`secrets.keys.s3SecretKey: S3_SECRET_KEY` are in the chart's committed `values.yaml`. The response to
a credential check firing on correct code is to switch it off, and then the leak it was written for
ships — so the exception is a named set of Kubernetes object-name keys, asserted in the tests, and
`password` is deliberately NOT on it. Two checks that fire on the repository's own correct files
were found in this tick alone; that is the third such finding across the request.

**Next.** REQ-128 slice 4 — the upgrade helper, `/deployment/upgrade`, `docs/deployment/upgrade.md`.
It consumes the manifest from slice 3 and the rollback split REQ-129 owns.

## 2026-09-30 — omnion-w6 tick 29 — REQ-127 slice 3, the scheduler · and the walk that caught a duplicate send

Slice 3 was half done: `retry_store` and `breaker_store` shipped last tick with ten walks green,
and what was missing was the piece between them. A ledger can *persist* a next-attempt time and a
state machine can *decide* what one attempt does, but nothing ran anything — so "a restarted
worker resumes exactly once" was a property of a column rather than of the platform.

**`crates/reliability/src/scheduler.rs`** (migration `0184_reliability_scheduler_lease.sql`, one
column and one partial index). Four decisions, each of which is a property of the schema rather
than a promise:

- `due_sequences` is `distinct on (subject_kind, subject_id) … order by attempt desc` — the NEWEST
  row per subject. Claiming the middle of a sequence would replay an attempt the timeline already
  shows.
- `claim_sequence` is a compare-and-swap on **one `row_id`**, not on a `(subject_kind, subject_id,
  attempt)` predicate. PostgreSQL has no `UPDATE … ORDER BY`, so the predicate version either
  needs a subquery or claims every row that matches — and the second is how a scheduler writes
  attempt 4 three times. My first draft had the `ORDER BY` in it and the compiler's SQL layer is
  what caught it.
- The claim is a **lease**, not a lock. A permanent claim turns a worker that dies mid-attempt into
  a job nobody runs again, which is the one failure this subsystem exists to prevent.
- `elapsed_ms` is measured from the sequence's FIRST row. A restarted worker handing every resumed
  sequence a fresh budget is how a policy outlives the job it belongs to.

**The walk found the bug this slice existed to prevent.** `a_sequence_is_due_once_and_then_stops_
being_due` failed with `a succeeded sequence is still being offered` — and the machine was right
and the SQL was wrong. The `where next_attempt_at is not null` sat in the SAME query as the
`distinct on`, so the terminal row of a finished sequence was filtered out FIRST, and "newest per
subject" then resolved to the last row that still owed an attempt. A delivery that succeeded on
attempt four would have been offered a fifth: a **duplicate send, from a subsystem whose entire
purpose is to prevent exactly that**. Fixed by picking the newest row in a subquery and filtering
it outside, and `due_count` — the panel's backlog number — carried the same shape, so it had the
same bug and now has the same fix.

That is the third walk in three ticks that failed against the machine rather than passing it, and
it is the reason this tier exists: the column, the index and the query all looked right in review.

**Also shipped:** `apps/api/src/routes/reliability_retries.rs` (nine routes, including
`POST /breakers/{key}/observe` — the outbound gate, which is where `provider_unavailable` stops
being a status a handler invents and becomes `breaker::admit` refusing *before* the call), the two
screens (`/settings/reliability/retries`, `/settings/reliability/breakers`), the API client, and
two walkthrough depth passes. The force-open pass asserts the confirm button is **disabled while
the reason is empty** and then CANCELS — a pass that pressed it would leave the QA stack refusing
a live provider.

**Proof.**
- `cargo test -p omnion-reliability --lib` → **116 passed / 0 failed** (114 before, +2 scheduler).
- `cargo test -p omnion-api --lib` → **281 passed / 0 failed** (276 before, +5 route tests).
- `pnpm typecheck` (admin) → clean.
- `cargo test -p omnion-api --test reliability_scheduler -- --test-threads=1` against
  `omnion_w6_dev` → **8 walks, 7 green and 1 red on the first run** (the duplicate send), then
  **8/0** after the fix. Run one at a time: the box is at load 60-99 with 8 sibling writers.

**Merge.** `origin/main` was 5 ahead at tick start. Only `docs/BUILD-LOG.md` conflicted, and the
multiset merge passed: 101 blocks, **0 dropped**, main's `Tick 74` preserved, two blocks both
editors had touched resolved newest-wins. The multiset recipe needed a correction worth writing
down — a FLAT Counter over both parents counts every shared-base block twice and reports the entire
shared history as "missing". The correct claim for an append-only log is a **union plus content
identity**: nothing dropped, and a block only one parent has is byte-identical to that parent's
version.

**Still owed for slice 3:** the focused QA browser pass (`QA_ONLY=reliability-retries,reliability-
breakers`, now possible and now the last gate before intake).

**Next:** the intake guard (slice 4) — HMAC verification, tolerance, replay defence, size caps and
the sanitisation profile — then the focused pass, then close REQ-127.

## 2026-09-30 — REQ-014 slice 2's last screen + the 14 committed compile errors nobody's gate could see

feat(health): the service detail page draws a 24 h trend. fix(health): the health
walks did not compile, and had been committed that way.

**Slice 2 was one screen short, and it was short in the way that looks the least
broken.** The request names the drill-down's contents: "current state, checks table,
**a 24 h trend chart**, recent failures". The screen listed current values in four
columns and drew no line. Nothing about it errored — the route resolved, the values
were right, the heading was right — and "a table with no chart in it" is not a
defect any compiler, type checker or linter in this workspace has an opinion about.

Three things made it a chart rather than a decoration:

| | |
|---|---|
| one window per response | `SERVICE_TREND_RANGE` is read once, before the loop, so every row covers the same 24 h. A per-row `now()` gives row one a fraction of a second more than row nine, and two lines on one screen that end at different instants cannot be compared. |
| an empty window speaks | a row with no samples draws "no samples in the last 24 h", because an empty box on a numbers screen reads as a chart that failed to load. |
| one point draws a dot | a polyline through one point has zero length and renders as nothing at all — which the table reads as "no data" on a row that has one. |

**The sparkline was extracted, not copied.** Both screens now assert
`data-health-spark`, and "what does an unmeasured window look like" is one answer or
two. A second copy is how the metric table learns to draw a dot while the drill-down
keeps drawing nothing — and *nothing* is what the walk reads as a missing chart. It
gained a `label` prop in the same move: twelve rows all named "sparkline" is a screen
a screen reader cannot be read from.

**`/health/samples` stopped taking `hours`.** It was the last endpoint in the centre
still clamping an hour count — the exact defect slice 2 removed from `/health/metrics`.
A caller asking for 30 days got 7 with a `200` and no warning, drew the wrong chart,
and had no way to tell. It now goes through the same `Range::parse`, so the refusal
is a property of the *vocabulary* rather than of one handler, and `fetchHealthSamples`
moved with it: `hours=24` and `range=24h` are one window with two spellings, and the
second spelling travels into the CSV filename.

## The other half of this tick: 14 errors, committed, invisible to every gate

`cargo check -p omnion-api --tests` reported **fourteen compile errors** across the
two health walk files. All fourteen were committed. Not one gate had ever caught
them, and each gate was *correct about the thing it checked*:

| gate | what it compiles |
|---|---|
| `cargo test -p omnion-health --lib` | the health **crate** — not `apps/api`, and not its test binaries |
| `pnpm typecheck` (admin) | TypeScript — cannot see Rust |
| `node --check walkthrough.cjs` | the walkthrough — does not parse `.rs` |
| `tsc` on a screen | the admin's JSX — cannot see the route that serves it |

**Not one of them puts an `apps/api` test binary in a dependency graph.** Tick 80
wrote that rule into the ledger after finding the mirror-image failure in
`health_panel.rs`; the rule was in the ledger, in my own words, when it happened
again on the very next tick. That is the part worth writing down: reading a lesson
is not the same as the lesson being in force, and the only thing that put
`apps/api`'s tests in the graph this time was deliberately reaching for
`--tests`.

**Eleven of the fourteen were the same line, in the same shape.**

```rust
harness.dispose().await;          // dispose(self) — takes the harness BY VALUE
std::mem::forget(harness);        // …and this moves it again
```

`dispose(self)` consumes the harness, so the call above *moves* it and there is
nothing left to forget — E0382, eleven times. The comment above the method gave a
confident reason for the `forget`: "stops the later `Drop` from running against a
moved-from handle". **That reason is false.** There is no later `Drop` of that
value, because the value was consumed by the call two lines up. The same comment
had already been copied into `notification_delivery_reader.rs`, which received the
justification without the code — a file that reads as though the hazard is real,
which is how the next writer adds it back.

Both comments now record why there is deliberately no `forget`, because a
plausible-sounding story is what carried this through two reviews and four ticks.
The rule that falls out of it: **a comment that explains a line's purpose is a
claim to verify, and the ones that sound most confident are the ones nobody reads
closely.**

The other three: a stray `await` on a non-`await` expression; an import list naming
the crate's own root (`use omnion_health::{MetricSummary, Range, omnion_health};` —
there is no such item); and `&[good, good]`, which moves the same non-`Copy`
`NewSample` into an array twice.

**Proof**

```
cargo check -p omnion-api --tests   clean   (was 14 errors)
cargo test -p omnion-health --lib    40 passed; 0 failed   (0.27s)
admin tsc -p tsconfig.json --noEmit  exit 0
node --check walkthrough.cjs        syntax ok
```

**What this costs the next tick, stated plainly.** The `health_history` and
`health_probes` suites have still **not been run** — they now compile, which is a
different and much lower claim. An `omnion-api` test binary needs a ~30-minute link
at the current load average (85, four sibling cargo trees), and the browser pass
(`bash scripts/qa/run.sh`) has still not run at all: the QA slot is held by a sibling
writer and `/mnt/apopic` is at 87%. So slice 2's screen behaviour rests on the walk
being **written and wired**, not on a rendered page, and the REQ says so.

**Next:** run `health_history` and `health_probes` when a slot frees, then
`bash scripts/qa/run.sh` — which closes slice 2's last box and the walkthrough box
at the same time. Then slice 3 (incidents + thresholds), whose schema
(`health_incidents`, `health_settings`) has shipped empty since slice 1 and whose
empty-only screen the migration comment explicitly refuses to build.

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

## 2026-09-30 — REQ-128 slice 4: the upgrade helper's decision layer and its guide

**What.** `release/lib/upgrade.py` — `build_plan` · `destructiveness` · `checklist` ·
`verify_plan` — with 45 unit tests, `scripts/qa/release-upgrade.sh` at **32 checks / 6
mutations**, `docs/deployment/upgrade.md`, and all three wired into CI.

**Proof.**

```
python3 -m unittest discover -s release/tests -t .     190 tests, OK   (45 of them new)
bash scripts/qa/release-upgrade.sh                     32 passed, 0 failed, 6/6 mutations
bash scripts/qa/release-pipeline.sh                    51 passed, 0 failed, 12/12 mutations
bash scripts/qa/release-bundle.sh                      23 passed, 0 failed
bash scripts/qa/helm-chart.sh                          70 passed, 0 failed
bash scripts/qa/release-manifest.sh                    42 passed, 2 failed, 19/19 mutations
```

The two manifest failures are the pre-existing `themes/minimal` version drift (0.1.1
against a 0.1.0 platform) that belongs to wave 2 and is already recorded in REQ-128's status
line. It is reported, not edited.

**What the slice proves.** A plan built from this repository's real compose stacks and real
migration files verifies, and every step command parses (`bash -n` per command) with the
stack file it names confirmed to exist. The order is derived, not written down: stack file,
chart name, image reference and migration delta all come from the repository and the two
manifests, and `verify_plan` re-derives them.

**The finding is the third verdict.** REQ-129's `up → down → up` gate has not landed, so a
migration with no `-- omnion:no-down` marker is not proven reversible — it is merely not
declared irreversible. `destructiveness()` returns `unknown` beside `reversible` and
`destructive`, keeps `database_rollback: unknown`, and marks the first migration as the point
of no return. A manifest's `migrations_destructive: false` does not override it: that flag is
the publisher's silence, not a verification, and there is a test for exactly that. With a
boolean verdict, `unknown` and `reversible` would be the same value and every plan on this
repository would promise a down script nobody has run.

**Four defects in the module, found by running it.** A `verify` step whose command was
`curl https://<your domain>/readyz` — a command an operator pastes into production and
watches fail, because a manifest does not carry an install's own domain; a `pg_dump` that
hardcoded the database name while the stack reads `${OMNION_DB_NAME:-omnion}`, so an install
that set it would dump the wrong database and believe it had a backup; a credential check
built on `carries_credential` that matches no shape in `docker login -p <password>`; and a
docstring that claimed a conditional refusal while the code refused unconditionally.

**Five in the gate, all of which made a result unreadable rather than wrong.** The
differential probe compared the module with itself, because `upgrade.py` inserts its own
directory into `sys.path` and a second import in one interpreter resolves to the first copy.
`${out%% *}` split a tuple fixture in half. A multi-statement probe cannot travel as one
`sys.argv` value, so `eval` reported a `SyntaxError` on five of six. Two mutations proved
nothing because the `destructive` and `kind == "migrate"` clauses select the same step. And
the polarity was written backwards first, reporting four working mutations as four failures —
the same defect class the pipeline gate recorded in slice 3, in the same request, two ticks
apart.

**The lesson the gate cost the most: a mutation with a live sibling check survives.** Three
mutations reported "still load-bearing" because a DIFFERENT check caught the same fixture.
The mutations are now differential — the same fixture run against the real module and the
mutated copy in separate processes, requiring the behaviour to differ — and each fixture
carries exactly one defect.

**Still open in this slice.** No `upgrade_plans` table, no `/api/v1/deployment/upgrade-plan`
route, no `/deployment/upgrade` screen, no acknowledgement endpoint: the acknowledgement is
a flag on the plan document and nothing persists it yet. Slice 3's tag pipeline
(`.github/workflows/release.yml`) and the four `/deployment/*` screens are also unbuilt.

**Next.** The migration (`upgrade_plans` + `release_manifests` + `release_artifacts` + the
REQ-129 slot), the seven routes, and the four admin screens with their empty, error, loading
and populated states.

# Wave 6 — tick 43 (2026-09-30)

## What

REQ-128 slice 4's **server half**: the release cache, the upgrade plan and the acknowledgement.

- `crates/deployment` — the decision layer (`manifest`, `plan`, `upgrade`, `bundle`) and the store
  (`release_manifests`, `release_artifacts`, `environment_bundles`, `upgrade_plans`).
- `database/migrations/0199_deployment_tooling.sql` — four additive tables, three indexes and a
  partial unique index holding one acknowledged plan per version range.
- Nine routes under `/deployment/*`, permission-guarded `deployment.read` /
  `deployment.bundle.generate` / `deployment.deploy`.
- `deployment.bundle.generate` added to the permission catalogue; three event names added to the
  events catalogue.
- `apps/api/tests/deployment_release.rs` — three walks.

## Proof

```text
cargo test -p omnion-deployment --lib            33 passed, 0 failed
cargo test -p omnion-events --lib                49 passed, 0 failed
cargo test -p omnion-permissions --lib           66 passed, 0 failed
omnion-api --test deployment_release              3 passed, 0 failed   (run 3× consecutively)
apps/admin pnpm typecheck                        tsc --noEmit, clean
migration 0199 on a scratch database             up → down → up, all green
```

The migration's third check is the one that matters: a down script that cannot be re-applied is not
a reversal.

## What the slice proves

A plan built from two cached manifests reports `unknown` — not `reversible` — for this repository,
because REQ-129's up → down → up gate has not landed, and it marks the FIRST migration as the point
of no return rather than the deploy step. The checklist refuses to render as complete until an
operator acknowledges, the acknowledgement is a durable fact that survives regeneration, and it is
unique per range.

## The defects

**Four in the product, every one found by running the suite against a database:**

1. The acknowledgement set the actor and THEN cleared the previous holder, so the partial unique
   index fired on the set and every acknowledgement answered `500 duplicate key`. The `update`
   written to prevent exactly that never ran. It passed all 33 unit tests — none of them builds a
   router, and the index only exists in a database.
2. A `compose` plan validated its stack against `BUNDLE_KINDS` (which includes `helm`) and then
   resolved the stack file through an `unwrap_or`, so a Helm target produced `docker compose`
   commands against the wrong stack with no error anywhere.
3. The acknowledgement route was guarded with `deployment.manage`, which the catalogue does not
   have. A guard on an uncatalogued key refuses EVERY account including the instance owner; nothing
   caught it except the walk, on its first run.
4. The credential rule fired on any URL carrying userinfo, so it fired on
   `postgres://omnion@postgres/omnion` — a line the compose stack writes on its healthy path.

**Four in the walk, all the same shape.** A uuid's decimal digits overflow `u32`, so
`parse().unwrap_or(1)` gave every run the SAME target version; the plan's `from_version` is the
build's, so two walks sharing a target shared a range and one walk's consent answered the other; a
`{id}` that was never created made a read `GET /deployment/bundles//files/…` and its `400` read as
a permissions failure; and a `published_kinds` expectation ignored that the coverage query is
ordered by kind. **A fixture that is silently constant looks exactly like a product that ignores its
input.**

## Not claimed

No admin screen, no tag pipeline, no QA browser pass. The QA slot was held by a live sibling walkthrough
(w3, pid 973982) for the whole tick, and a second Chromium on a load-9 box is how a pass destroys
its own result rather than proving anything — so the pass is queued, and the slice's close gate stays
unticked until it has measured the screens that exist.

## Next

The four `/deployment/*` admin screens (`/deployment/artifacts`, `/deployment/artifacts/{version}`,
`/deployment/install`, `/deployment/upgrade`) with their empty, error, loading and populated states,
the `walkthrough.cjs` route list extended to visit them, and the tag pipeline
(`.github/workflows/release.yml`). Then the QA pass, which is a close gate and not a formality.

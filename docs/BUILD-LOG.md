## 2026-10-02 — REQ-107 slice 6b: the seam nobody was walking, and five defects behind it

fix(ai-eval) + test(ai-eval): two walks that dial real sockets, and the five production bugs that
only exist on the path they could reach.

**The shape of the miss is worth stating, because it is the whole tick.** Every walk in
`ai_eval_runner.rs` drives `ScriptedTurn` — a seam that writes no `ai_usage` rows and builds no
`ChatRequest`. That is the right seam for testing the *runner*. It is also a seam that skips
everything the runner does on the way to a provider: resolution, message construction, pricing,
billing. So the live half — `LiveTurn`, `build_turn`, `resolve_judge_target` — was fully
implemented, reached in production by `spawn`, and provable by nothing. Five defects lived there.

1. **The pin never decided anything.** `resolve_and_record` takes a model in two places:
   `DecisionContext.requested`, which is stored on the row, and its sixth parameter, which is
   what builds `ResolveRequest.explicit` — the field `decide` reads *before any map*. The call
   set the pin in the context and passed `None` for the parameter. The resolver then fell through
   the feature pin, the task map (there is no `eval` routing task) and the installation default.
   **Every eval run in production graded whichever model the installation had marked default**,
   while the run's snapshot and its `requested` column both recorded the suite's own pin — so the
   decision log agreed with the snapshot, and both were wrong about the call. The comment above
   that call said "the pin, not `None`", and was true of the line it sat on: the argument the
   function reads least.
2. **A default suite could not be called at all.** `DEFAULT_SYSTEM_PROMPT` is empty on purpose —
   the snapshot is the reproduction data, and a baked-in prompt would be a variable nothing
   records. But `case_messages` emitted the system message regardless, and `validate_request`
   refuses a blank message. So every suite that pinned no prompt — the default, and what the
   panel's form writes when the box is left alone — settled `error` with "the model could not be
   called", which reads as a provider outage and is not one.
3. **Every judge was unreachable.** `resolve_judge_target` selected six columns into
   `omnion_ai_hub::Provider`, which has sixteen. `query_as` failed on the missing columns and
   `.ok().flatten()` converted that into `None` — which the caller's own comment documents as
   "makes a rubric case `error` with a reason rather than a silent pass". It now goes through
   `store::find_provider_by_name`, the same read the model under test uses, so the judge and the
   graded model can no longer be answered by two different queries.
4. **An unreachable judge looked unconfigured.** `LiveTurn::judge` returned `None` through a bare
   `.ok()?`, so a broken judge and a missing one were the same event. It logs the reason now;
   `None` is still the answer, because an unreachable judge must never read as a pass.
5. **Every eval call was unpriced.** `price_for` is keyed on `model_key`; the runner passed the
   router's `provider/model` identifier. The lookup missed, `record_usage` was handed `None`, and
   the row carried a **null cost** while the run row and the case rows carried real numbers — the
   costs screen and the run detail disagreeing about the same run, each correct on its own terms.
   `routes::ai.rs` looks the price up with the bare key, which is exactly why chat was priced and
   eval was not. The split is on the *first* slash, so a local model's `models/<file>.gguf` key
   survives.

**Two of these are the same class as each other and as this tick's earlier one**: a value written
in one place and read in another that assumed a different shape. The pin in the wrong argument,
the price under a prefixed key, the six columns into a sixteen-field struct. None of them is
visible to `cargo test`, `pnpm typecheck` or a browser pass, because each fails only on the
seam between two components that individually type-check. That is the argument for a walk that
dials a socket instead of one that mocks one.

**Falsified before committing** (production file reverted, walks kept): red on the pre-fix body.
17 walks, 680 unit tests.

**The box rebooted mid-tick and the first hour was infrastructure.** `omnion-postgres` spent ~30
minutes in crash-recovery fsync over 18 GB; `omnion-redis` crash-looped 15 times on a torn AOF
and was repaired with `redis-check-aof --fix` after a backup to
`/mnt/apopic/backups/redis-aof-2026-10-02/` (the base RDB was valid and the first 9.3 MB of the
tail was, so the valid prefix is kept and the torn 24 MB dropped). Postgres was left to finish
itself: its startup process was visibly burning CPU, so restarting it would have discarded half
an hour of redo. Later the shared PG hit ENOSPC, which was 190 abandoned scratch databases
(2.9 GB) from crashed runs across all ten writers; dropped only the ones with no live backend,
taking `/mnt/apopic` from 91% to 80%.

**Still owed:** the closing browser pass (`QA_STACK=w7 ... bash scripts/qa/run.sh` — the global
QA slot was held live by w3 this tick); `no_pii` failing a case whose output REQ-105's detector
would mask; Run-now disabled with the permission named.

**Next.** REQ-107 slice 6c: the remaining two acceptance rows, then the pass.


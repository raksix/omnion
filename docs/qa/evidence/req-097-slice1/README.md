# REQ-097 slice 1 — QA evidence

Browser walkthrough of the AI provider runtime, run against the private `w7` stack
(`QA_STACK=w7`, ports 18086/3106/3206, database `omnion_qa_w7`).

## What the pass observed

| Step | Observation |
|---|---|
| `empty` | Empty state "No provider is connected yet" with a working **Connect provider** action. |
| `protocols` | The select is driven by `GET /ai/protocols`. |
| `refusal` | `not-a-url` is refused in the field: `a base URL must start with http:// or https://` lands under **Base URL**, not only in the banner. |
| `connected` | The row appears with its **Local** kind badge. |
| `test` | The modal lists all five steps with their own status and latency: `resolve 342 ms · tls not applicable ("the endpoint is plain http") · auth 0 ms · models 0 ms · stream 22 ms`, and the summary "The endpoint answers and serves 2 models, none registered yet. · openai_compatible · 367 ms in total". |
| `dead` | A provider on `http://127.0.0.1:1/v1` stops at **resolve** — "the test stopped at resolve: the host did not answer" — and every later step stays unrun. |
| `verdict` | After a reload the rows carry `down:Down` and `ok:Ok`, and the dead provider shows its stored error. |

**Zero console errors** across the whole pass (1 066 clicks, 22 screens). The single
`click-error` in the log is on `analytics-forms`, which this slice does not touch and which
carried forward from the previous pass.

## Vision review

Two defects the review found in the first pass, both fixed before this evidence was collected:

1. **The protocol select held one option.** The form fetched `/ai/protocols` inside the same
   `Promise.all` as the provider and model lists, so a failure in either discarded a good
   answer and left the select on a one-option fallback. All three calls now settle
   independently, and the fallback itself carries all three adapters.
2. Verified after the fix against the running stack: three options, the note under the select
   follows the chosen protocol, the key field is a `type="password"` input that renders no
   value, zero console errors.

The test modal was re-read after the fix: the five rows are legible, the `tls` row reads as
"not applicable" with a neutral icon rather than a failure, nothing is clipped, and no key or
secret appears anywhere on the screen.

## Files

`ai-providers-empty.png` · `ai-providers-refusal.png` · `ai-providers-connected.png` ·
`ai-providers-test.png` · `ai-providers-dead.png` · `ai-providers-verdict.png` ·
`page-ai.png` · `clicks.jsonl` (the full click log of the pass)

"use client";

/**
 * `/ai/guard/tester` — a two-pane dry run (docs/requests/REQ-105, slice 1).
 *
 * The tester exists to answer one question before an operator changes a policy: **what would
 * happen to this text?** Three properties make it trustworthy rather than decorative:
 *
 * 1. **No provider call happens, and the screen says so in the place it matters** — beside the
 *    Run button, not in a help page. The endpoint has no code path that could dial out; a tester
 *    that looked safe but wasn't would be worse than none.
 *
 * 2. **The masked text is shown, and it is the server's.** The tempting version re-implements the
 *    mask in the browser to render it instantly, which means the panel can disagree with the
 *    guard — showing a plaintext address the guard would have replaced, or a placeholder the
 *    guard would not have produced. The response's `masked_text` is printed verbatim.
 *
 * 3. **A match shows a hash, never the value.** `value_hash` is a short salted digest, and the
 *    panel's job is to make the operator trust that rather than to help them recover the value
 *    they just pasted. They already have it; the panel is not a way around the guard.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Eraser, Play, ShieldAlert, ShieldCheck } from "lucide-react";

import {
  ApiError,
  type GuardFixture,
  type GuardTestResult,
  createGuardFixture,
  fetchGuardFixtures,
  runGuardTest,
} from "@/lib/guard-api";

/** What the tester holds while it is loaded. */
export function GuardTester() {
  const [payload, setPayload] = useState("");
  const [provider, setProvider] = useState("");
  const [feature, setFeature] = useState("");
  const [result, setResult] = useState<GuardTestResult | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fixtures, setFixtures] = useState<GuardFixture[]>([]);
  const [savingFixture, setSavingFixture] = useState(false);

  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    fetchGuardFixtures()
      .then(setFixtures)
      .catch(() => {
        // The seeded samples are a convenience; the tester works without them, and an empty
        // sample list must not read as "this tester has nothing to try".
      });
  }, []);

  const run = useCallback(async () => {
    if (!payload.trim()) return;
    setRunning(true);
    setError(null);
    try {
      setResult(
        await runGuardTest({
          payload,
          provider: provider || undefined,
          feature: feature || undefined,
        }),
      );
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
      setResult(null);
    } finally {
      setRunning(false);
    }
  }, [payload, provider, feature]);

  const clear = useCallback(() => {
    setPayload("");
    setResult(null);
    setError(null);
    textareaRef.current?.focus();
  }, []);

  const saveFixture = useCallback(async () => {
    if (!payload.trim()) return;
    setSavingFixture(true);
    setError(null);
    try {
      const created = await createGuardFixture({
        name: `Sample ${new Date().toISOString().slice(0, 16).replace("T", " ")}`,
        payload,
      });
      setFixtures((current) => [...current, created]);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setSavingFixture(false);
    }
  }, [payload]);

  /**
   * `⌘Enter` runs and `Esc` clears.
   *
   * `Esc` on the tester clears rather than merely closing something, which is a deliberate
   * choice: this screen has no drawer and no modal, so an `Esc` that did nothing visible would
   * look like a broken key. Both are ignored while focus is in a context where they would be
   * destructive by accident — there is none here, and adding one speculatively would be the
   * kind of guard that hides the feature it claims to protect.
   */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        event.preventDefault();
        void run();
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        clear();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [run, clear]);

  const verdictTone = useMemo(() => {
    if (!result) return "text-muted";
    if (result.would_block) return "text-danger";
    if (result.verdict === "masked") return "text-warning";
    return "text-ink";
  }, [result]);

  return (
    <div className="flex flex-col gap-4" data-guard-tester>
      <div className="grid gap-4 lg:grid-cols-2">
        {/* Left: the input. On mobile this stacks above the result, which is the order the spec
            asks for — an operator on a phone reads the verdict below the text they pasted. */}
        <section aria-label="Payload" className="flex flex-col gap-3">
          <div>
            <label htmlFor="tester-payload" className="block text-[12px] font-medium">
              Payload to inspect
            </label>
            <textarea
              id="tester-payload"
              ref={textareaRef}
              data-guard-tester-payload
              value={payload}
              onChange={(event) => setPayload(event.target.value)}
              rows={10}
              placeholder="Paste the text a model would receive…"
              className="mt-1 w-full resize-y rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-[12.5px]"
            />
          </div>

          <div className="grid gap-3 sm:grid-cols-2">
            <div>
              <label htmlFor="tester-provider" className="block text-[12px] font-medium">
                Provider context (optional)
              </label>
              <input
                id="tester-provider"
                value={provider}
                onChange={(event) => setProvider(event.target.value)}
                placeholder="commandcode"
                className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
              />
            </div>
            <div>
              <label htmlFor="tester-feature" className="block text-[12px] font-medium">
                Feature context (optional)
              </label>
              <input
                id="tester-feature"
                value={feature}
                onChange={(event) => setFeature(event.target.value)}
                placeholder="crm"
                className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
              />
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => void run()}
              data-guard-tester-run
              disabled={running || !payload.trim()}
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg disabled:opacity-40"
            >
              <Play aria-hidden className="size-3.5" />
              {running ? "Running…" : "Run"}
            </button>
            <button
              type="button"
              onClick={clear}
              data-guard-tester-clear
              disabled={!payload && !result}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-40"
            >
              <Eraser aria-hidden className="size-3.5" />
              Clear
            </button>
            <button
              type="button"
              onClick={() => void saveFixture()}
              disabled={savingFixture || !payload.trim()}
              className="rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-40"
            >
              {savingFixture ? "Saving…" : "Save as sample"}
            </button>
          </div>

          <p className="text-[11.5px] text-muted">
            ⌘Enter runs · Esc clears. No provider is contacted: the detector runs in this process
            and the payload never leaves it.
          </p>

          {fixtures.length > 0 ? (
            <div>
              <h2 className="text-[12px] font-medium">Samples</h2>
              <ul className="mt-1.5 flex flex-col gap-1">
                {fixtures.map((fixture) => (
                  <li key={fixture.id}>
                    <button
                      type="button"
                      onClick={() => setPayload(fixture.payload)}
                      className="w-full rounded-md border border-line px-2.5 py-1.5 text-left text-[12px] hover:bg-muted/30"
                    >
                      {fixture.name}
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
        </section>

        {/* Right: the verdict. */}
        <section aria-label="Result" className="flex flex-col gap-3">
          {error ? (
            <p role="alert" className="rounded-md bg-danger/10 px-3 py-2 text-[12.5px] text-danger">
              {error}
            </p>
          ) : null}

          {!result && !error ? (
            <p className="rounded-lg border border-line px-4 py-10 text-center text-[12.5px] text-muted">
              Nothing run yet. Paste a payload and press Run — the result is what the provider
              would receive.
            </p>
          ) : null}

          {result ? (
            <>
              <div
                role="status"
                data-guard-tester-verdict
                className="flex items-start gap-3 rounded-lg border border-line p-4"
              >
                {result.would_block ? (
                  <ShieldAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-danger" />
                ) : (
                  <ShieldCheck aria-hidden className="mt-0.5 size-4 shrink-0 text-ink" />
                )}
                <div>
                  <p className={`text-[14px] font-medium ${verdictTone}`}>
                    {result.would_block ? "Would be blocked" : `Verdict: ${result.verdict}`}
                  </p>
                  <p className="mt-0.5 text-[12.5px] text-muted">
                    Action <span className="font-medium text-ink">{result.action}</span> ·{" "}
                    {result.rules_evaluated} rule{result.rules_evaluated === 1 ? "" : "s"} evaluated
                    {result.blocked_label
                      ? ` · ${result.blocked_label} (${result.blocked_rule ?? "unknown rule"})`
                      : ""}
                  </p>
                </div>
              </div>

              <div>
                <h2 className="text-[12px] font-medium">Matches</h2>
                {result.matches.length === 0 ? (
                  <p className="mt-1 text-[12.5px] text-muted">
                    Nothing matched. The payload below is what the provider would receive,
                    unchanged.
                  </p>
                ) : (
                  <ul className="mt-1.5 flex flex-col gap-1.5">
                    {result.matches.map((match) => (
                      <li
                        key={`${match.rule_key}-${match.start}`}
                        className="rounded-md border border-line px-2.5 py-1.5 text-[12px]"
                      >
                        <span className="font-medium">{match.label}</span>
                        <span className="text-muted"> · {match.rule_key} · bytes {match.start}–{match.end}</span>
                        <p className="mt-0.5 font-mono text-[11px] text-muted">
                          {match.value_hash}
                        </p>
                      </li>
                    ))}
                  </ul>
                )}
              </div>

              <div>
                <h2 className="text-[12px] font-medium">What the provider would receive</h2>
                <pre
                  data-guard-tester-masked
                  className="mt-1.5 overflow-x-auto whitespace-pre-wrap break-words rounded-md border border-line bg-muted/20 p-2.5 font-mono text-[12px]"
                >
                  {result.masked_text}
                </pre>
                <p className="mt-1 text-[11.5px] text-muted">
                  Printed exactly as the server returned it — the browser does not re-render the
                  mask, because a client-side mask is one that can disagree with the guard.
                </p>
              </div>
            </>
          ) : null}
        </section>
      </div>
    </div>
  );
}
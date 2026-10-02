"use client";

/**
 * `/ai/tools/[key]` — one tool (docs/requests/REQ-100, slice 1).
 *
 * Four sections, in the order the spec names them, and each one answers a question an operator
 * arrives with:
 *
 *  - **Arguments** — the schema, read-only, with a copy button and the example payload beside it.
 *    The schema is rendered *verbatim* from the API and never re-derived from the model: a client
 *    that reconstructed the schema from the description could show a shape the validator does not
 *    enforce, and the operator would write a call that is refused for a reason the screen said
 *    was fine.
 *  - **Which agents may call it** — from the API's own `used_by_agents`, which reads the agents'
 *    allow-lists rather than the grant table. An agent can carry a tool no identity grants it,
 *    and that is exactly the row an operator needs before switching a tool off.
 *  - **Recent calls** — status, duration, error code and *sizes*. Arguments are never returned by
 *    the API; a call log that rendered raw arguments would accumulate tenant content on a screen
 *    outside every retention path the rest of the platform honours.
 *  - **Limits** — the four operator-owned numbers, editable, each with its range printed next to
 *    the field so the validation message and the form agree.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";

import { ArrowLeft, Check, Copy, Loader2, TriangleAlert } from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import {
  type AiToolDetail,
  type AiToolUsageChart,
  fetchAiTool,
  fetchAiToolUsageChart,
  updateAiTool,
} from "@/lib/api";

/** The limit ranges, quoted from the migration's checks so the form cannot drift from them. */
const LIMITS = {
  timeout: { min: 1000, max: 300000 },
  cap: { min: 1, max: 200 },
} as const;

/** The call status badge's tones. */
const STATUS_TONE: Record<string, string> = {
  ok: "text-emerald-700 dark:text-emerald-300 bg-emerald-500/10",
  denied: "text-rose-700 dark:text-rose-300 bg-rose-500/10",
  failed: "text-rose-700 dark:text-rose-300 bg-rose-500/10",
  timeout: "text-amber-700 dark:text-amber-300 bg-amber-500/10",
  limited: "text-amber-700 dark:text-amber-300 bg-amber-500/10",
};

/** `Copy` needs a clipboard write that can be refused, so the button has a failure answer too. */
function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState<"idle" | "done" | "failed">("idle");
  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied("done");
    } catch {
      // A clipboard write fails on an insecure origin and on a denied permission. Saying "copied"
      // when it did not copy is worse than saying nothing worked, so the button says which.
      setCopied("failed");
    }
    setTimeout(() => setCopied("idle"), 1600);
  }, [text]);

  return (
    <button
      type="button"
      onClick={() => void copy()}
      className="inline-flex items-center gap-1 rounded-md border px-2 py-1 text-xs"
      aria-label={label}
    >
      {copied === "done" ? (
        <>
          <Check className="size-3" /> Copied
        </>
      ) : copied === "failed" ? (
        "Copy failed"
      ) : (
        <>
          <Copy className="size-3" /> Copy
        </>
      )}
    </button>
  );
}

export function AiToolDetailView({
  toolKey,
  organizationId,
}: {
  toolKey: string;
  organizationId?: string | null;
}) {
  const [tool, setTool] = useState<AiToolDetail | null>(null);
  const [usage, setUsage] = useState<AiToolUsageChart | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [timeout, setTimeoutValue] = useState("");
  const [cap, setCap] = useState("");

  const load = useCallback(async () => {
    setError(null);
    try {
      const [detail, chart] = await Promise.all([
        fetchAiTool(toolKey, organizationId),
        fetchAiToolUsageChart(toolKey, { organizationId, days: 30 }),
      ]);
      setTool(detail);
      setUsage(chart);
      setTimeoutValue(String(detail.timeout_ms));
      setCap(String(detail.max_calls_per_run));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [toolKey, organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = useCallback(
    async (changes: {
      timeout_ms?: number;
      max_calls_per_run?: number;
      requires_approval?: boolean;
    }) => {
      setBusy(true);
      setError(null);
      setSaved(false);
      try {
        const updated = await updateAiTool(toolKey, changes, organizationId);
        setTool((current) => (current ? { ...current, ...updated } : current));
        setSaved(true);
        setTimeout(() => setSaved(false), 2200);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setBusy(false);
      }
    },
    [toolKey, organizationId],
  );

  if (error !== null && tool === null) {
    return (
      <div data-ai-tool-detail className="flex flex-col gap-3">
        <Link href="/ai/tools" className="inline-flex items-center gap-1 text-sm text-muted-foreground">
          <ArrowLeft className="size-4" /> Back to the registry
        </Link>
        <div className="flex items-center justify-between gap-3 rounded-md border border-rose-500/40 bg-rose-500/10 p-3 text-sm">
          <span className="text-rose-700 dark:text-rose-300">{error}</span>
          <button type="button" onClick={() => void load()} className="rounded-md border px-2 py-1 text-sm">
            Retry
          </button>
        </div>
      </div>
    );
  }

  if (tool === null) {
    return <LoadingTable columns={4} rows={6} />;
  }

  const schema = JSON.stringify(tool.input_schema, null, 2);
  const example = tool.example ? JSON.stringify(tool.example, null, 2) : null;

  return (
    <div data-ai-tool-detail className="flex flex-col gap-5">
      <header className="flex flex-col gap-2">
        <Link href="/ai/tools" className="inline-flex items-center gap-1 text-sm text-muted-foreground">
          <ArrowLeft className="size-4" /> Back to the registry
        </Link>
        <div className="flex flex-wrap items-center gap-2">
          <h1 className="font-mono text-lg font-semibold">{tool.key}</h1>
          <span className="rounded bg-muted px-1.5 py-0.5 text-xs">{tool.class}</span>
          {tool.requires_approval ? (
            <span className="rounded bg-violet-500/10 px-1.5 py-0.5 text-xs text-violet-700 dark:text-violet-300">
              gated
            </span>
          ) : null}
          {!tool.compiled && (
            <span className="rounded bg-muted px-1.5 py-0.5 text-xs text-muted-foreground">
              retired
            </span>
          )}
        </div>
        <p className="text-sm text-muted-foreground">{tool.description}</p>
        <p className="text-sm">
          Requires{" "}
          <span className="font-mono text-xs">{tool.permission}</span>
        </p>
      </header>

      {tool.retired_note && (
        <div className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
          <p>{tool.retired_note}</p>
        </div>
      )}

      {tool.ungated_high_risk && (
        <div className="flex items-start gap-2 rounded-md border border-rose-500/40 bg-rose-500/10 p-3 text-sm">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-rose-600" />
          <p>
            This tool is high risk, enabled and ungated. Every call it makes happens without a
            second pair of eyes.
          </p>
        </div>
      )}

      {/* ---- Arguments -------------------------------------------------------------------------- */}
      <section className="rounded-md border p-4">
        <div className="mb-2 flex items-center justify-between">
          <h2 className="text-sm font-semibold">Arguments</h2>
          <CopyButton text={schema} label="Copy the argument schema" />
        </div>
        <p className="mb-2 text-xs text-muted-foreground">
          Validated before anything runs. Unknown fields are refused, not ignored.
        </p>
        <pre className="overflow-x-auto rounded bg-muted/40 p-3 text-xs">
          <code>{schema}</code>
        </pre>
        {example && (
          <div className="mt-3">
            <div className="mb-1 flex items-center justify-between">
              <p className="text-xs font-medium">Example payload</p>
              <CopyButton text={example} label="Copy the example payload" />
            </div>
            <pre className="overflow-x-auto rounded bg-muted/40 p-3 text-xs">
              <code>{example}</code>
            </pre>
          </div>
        )}
      </section>

      {/* ---- Which agents may call it ------------------------------------------------------------ */}
      <section className="rounded-md border p-4">
        <h2 className="mb-2 text-sm font-semibold">Which agents may call it</h2>
        {tool.used_by_agents.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No agent names this tool in its allow-list, so disabling it changes no agent.
          </p>
        ) : (
          <ul className="flex flex-col gap-1 text-sm">
            {tool.used_by_agents.map((agent) => (
              <li key={agent.id}>
                <Link href={`/ai/agents/${agent.id}`} className="underline-offset-2 hover:underline">
                  {agent.name}
                </Link>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* ---- Recent calls ------------------------------------------------------------------------ */}
      <section className="rounded-md border p-4">
        <h2 className="mb-2 text-sm font-semibold">Recent calls</h2>
        <p className="mb-2 text-xs text-muted-foreground">
          Arguments are never returned — only their size, so tenant content stays out of this
          screen.
        </p>
        {tool.recent_calls.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No call has been made. The usage column on the registry shows an em dash for a tool
            that has never run, not 0 %.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
                <tr>
                  <th className="px-2 py-1.5">When</th>
                  <th className="px-2 py-1.5">Agent</th>
                  <th className="px-2 py-1.5">Status</th>
                  <th className="px-2 py-1.5 text-right">Duration</th>
                  <th className="px-2 py-1.5 text-right">Args</th>
                  <th className="px-2 py-1.5">Error</th>
                </tr>
              </thead>
              <tbody>
                {tool.recent_calls.map((call) => (
                  <tr key={call.id}>
                    <td className="px-2 py-1.5">{new Date(call.created_at).toLocaleString()}</td>
                    <td className="px-2 py-1.5">
                      {call.agent_name ?? <span className="text-muted-foreground">—</span>}
                    </td>
                    <td className="px-2 py-1.5">
                      <span className={`rounded px-1.5 py-0.5 text-xs ${STATUS_TONE[call.status]}`}>
                        {call.status}
                      </span>
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {call.duration_ms === null ? "—" : `${call.duration_ms} ms`}
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {call.args_bytes === null ? "—" : `${call.args_bytes} B`}
                    </td>
                    <td className="px-2 py-1.5 font-mono text-xs">
                      {call.error_code ?? <span className="text-muted-foreground">—</span>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {/* ---- Limits ------------------------------------------------------------------------------ */}
      <section className="rounded-md border p-4">
        <h2 className="mb-2 text-sm font-semibold">Limits</h2>
        <div className="flex flex-wrap items-end gap-4">
          <label className="flex flex-col gap-1 text-sm">
            <span>Timeout (ms)</span>
            <input
              type="number"
              value={timeout}
              min={LIMITS.timeout.min}
              max={LIMITS.timeout.max}
              onChange={(event) => setTimeoutValue(event.target.value)}
              className="w-32 rounded-md border px-2 py-1"
            />
            <span className="text-xs text-muted-foreground">
              {LIMITS.timeout.min}–{LIMITS.timeout.max}
            </span>
          </label>
          <label className="flex flex-col gap-1 text-sm">
            <span>Max calls per run</span>
            <input
              type="number"
              value={cap}
              min={LIMITS.cap.min}
              max={LIMITS.cap.max}
              onChange={(event) => setCap(event.target.value)}
              className="w-32 rounded-md border px-2 py-1"
            />
            <span className="text-xs text-muted-foreground">
              {LIMITS.cap.min}–{LIMITS.cap.max}
            </span>
          </label>
          <label className="flex items-center gap-2 pb-6 text-sm">
            <input
              type="checkbox"
              checked={tool.requires_approval}
              onChange={(event) => void save({ requires_approval: event.target.checked })}
            />
            Requires approval
          </label>
          <button
            type="button"
            disabled={busy}
            onClick={() =>
              void save({
                timeout_ms: Number(timeout),
                max_calls_per_run: Number(cap),
              })
            }
            className="rounded-md border px-3 py-1.5 text-sm disabled:opacity-50"
          >
            {busy ? <Loader2 className="size-4 animate-spin" /> : "Save limits"}
          </button>
          {saved && <span className="pb-2 text-sm text-emerald-600">Saved</span>}
        </div>
        {error !== null && (
          <p className="mt-2 text-sm text-rose-700 dark:text-rose-300">{error}</p>
        )}
      </section>

      {/* ---- Usage ------------------------------------------------------------------------------ */}
      {usage !== null && (
        <section className="rounded-md border p-4">
          <h2 className="mb-2 text-sm font-semibold">Usage, last 30 days</h2>
          <div className="flex flex-wrap gap-6 text-sm">
            <p>
              <span className="text-xs text-muted-foreground">Calls</span>{" "}
              <span className="font-medium tabular-nums">{usage.calls}</span>
            </p>
            <p>
              <span className="text-xs text-muted-foreground">Errors</span>{" "}
              <span className="font-medium tabular-nums">{usage.errors}</span>
            </p>
            <p>
              <span className="text-xs text-muted-foreground">Error rate</span>{" "}
              <span className="font-medium tabular-nums">
                {usage.error_rate === null ? "—" : `${usage.error_rate.toFixed(1)}%`}
              </span>
            </p>
            <p>
              <span className="text-xs text-muted-foreground">Average</span>{" "}
              <span className="font-medium tabular-nums">
                {usage.avg_duration_ms === null
                  ? "—"
                  : `${Math.round(usage.avg_duration_ms)} ms`}
              </span>
            </p>
          </div>
          {/* The bars are real numbers read from the API's per-day series — a 30-day window with
              the quiet days included, so a week of silence reads as silence rather than as a
              straight line. */}
          <div className="mt-3 flex h-16 items-end gap-px" aria-hidden="true">
            {usage.series.map((point) => {
              const peak = Math.max(1, ...usage.series.map((p) => p.calls));
              return (
                <div
                  key={point.day}
                  className="flex-1 rounded-t bg-primary/60"
                  style={{ height: `${Math.round((point.calls / peak) * 100)}%` }}
                  title={`${point.day}: ${point.calls} calls, ${point.errors} errors`}
                />
              );
            })}
          </div>
        </section>
      )}
    </div>
  );
}

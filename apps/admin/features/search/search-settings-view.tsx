"use client";

/**
 * `/settings/search` — the index's own screen (REQ-002, slice 3).
 *
 * Two halves, both read from the API rather than guessed:
 *
 * * **The providers** — one row per provider with its document count, when it was last written,
 *   its state (`ready`, `indexing`, `stale`, `failed`, `empty`) and the last pass's numbers, plus
 *   a Reindex button per row and one for the whole index. The progress line is fed by the status
 *   endpoint while a pass runs and by the pass's own report when it finishes.
 * * **The ranking** — the four weights (`0..=10`, title ≥ body) and the set of providers that
 *   answer a query. Validation is the server's: whatever it refuses is shown with its own message,
 *   and "Restore defaults" uses the defaults the API answers with.
 */
import { useCallback, useEffect, useState } from "react";

import { RefreshCw, RotateCcw, Save, ShieldAlert, TriangleAlert } from "lucide-react";

import {
  ApiError,
  fetchSearchSettings,
  fetchSearchStatus,
  reindexSearch,
  saveSearchSettings,
  type SearchProviderStatus,
  type SearchSettings,
  type SearchWeights,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The weight fields, in the order the form shows them. */
const WEIGHT_FIELDS: { key: keyof SearchWeights; label: string; hint: string }[] = [
  { key: "title", label: "Title", hint: "A name that matches — the strongest signal" },
  { key: "tags", label: "Tags", hint: "Status and type of a document" },
  { key: "subtitle", label: "Subtitle", hint: "The line under a title (site, address)" },
  { key: "body", label: "Body", hint: "Full text — a mention, not a name" },
];

/** The badge a state reads as. */
const STATE_STYLES: Record<string, { label: string; className: string }> = {
  ready: { label: "Ready", className: "border-positive/40 bg-positive-soft text-positive" },
  indexing: { label: "Indexing", className: "border-accent/40 bg-accent-soft text-accent-strong" },
  stale: { label: "Stale", className: "border-caution/40 bg-caution-soft text-caution" },
  failed: { label: "Failed", className: "border-caution/40 bg-caution-soft text-caution" },
  empty: { label: "Empty", className: "border-line bg-canvas text-muted" },
};

/** One provider's row, as a card — the phone's layout (a six-column table does not fit one). */
function ProviderCard({
  line,
  busy,
  onReindex,
}: {
  line: SearchProviderStatus;
  busy: boolean;
  onReindex: (provider: string) => void;
}) {
  const state = STATE_STYLES[line.state] ?? STATE_STYLES.empty;
  const run = line.last_run;
  return (
    <li data-provider-row={line.provider} className="flex flex-col gap-2 p-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <p className="text-[13px] font-medium text-ink">{line.title}</p>
          <p className="font-mono text-[11.5px] text-muted">{line.provider}</p>
        </div>
        <span
          data-provider-state={line.state}
          className={`rounded-md border px-1.5 py-0.5 text-[11.5px] font-medium ${state.className}`}
        >
          {state.label}
        </span>
      </div>
      <dl className="grid grid-cols-2 gap-x-3 gap-y-1 text-[12px]">
        <dt className="text-muted">Documents</dt>
        <dd className="text-right font-mono text-ink">{line.documents}</dd>
        <dt className="text-muted">Last indexed</dt>
        <dd className="text-right whitespace-nowrap text-ink">
          {line.last_indexed_at ? formatTimestamp(line.last_indexed_at) : "never"}
        </dd>
      </dl>
      <p className="text-[12px] text-muted">
        {run?.finished_at
          ? `${run.indexed ?? 0} written · ${run.pruned ?? 0} pruned · ${run.duration_ms ?? 0} ms`
          : run
            ? "running…"
            : "no pass yet"}
      </p>
      {run?.error ? (
        <p className="flex items-start gap-1 text-[12px] text-caution">
          <TriangleAlert className="mt-0.5 size-3 shrink-0" aria-hidden />
          {run.error}
        </p>
      ) : null}
      <button
        type="button"
        onClick={() => onReindex(line.provider)}
        disabled={busy}
        data-reindex={line.provider}
        className="self-start rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft disabled:opacity-50"
      >
        Reindex
      </button>
    </li>
  );
}

/** One provider's row. */
function ProviderRow({
  line,
  busy,
  onReindex,
}: {
  line: SearchProviderStatus;
  busy: boolean;
  onReindex: (provider: string) => void;
}) {
  const state = STATE_STYLES[line.state] ?? STATE_STYLES.empty;
  const run = line.last_run;
  return (
    <tr data-provider-row={line.provider} className="border-t border-line">
      <td className="px-3 py-2.5">
        <span className="text-[13px] font-medium text-ink">{line.title}</span>
        <span className="ml-2 font-mono text-[11.5px] text-muted">{line.provider}</span>
      </td>
      <td className="px-3 py-2.5 text-right font-mono text-[12.5px] text-ink">
        {line.documents}
      </td>
      <td className="px-3 py-2.5 text-[12px] whitespace-nowrap text-muted">
        {line.last_indexed_at ? formatTimestamp(line.last_indexed_at) : "never"}
      </td>
      <td className="px-3 py-2.5">
        <span
          data-provider-state={line.state}
          className={`rounded-md border px-1.5 py-0.5 text-[11.5px] font-medium ${state.className}`}
        >
          {state.label}
        </span>
      </td>
      <td className="px-3 py-2.5 text-[12px] text-muted">
        {run?.finished_at
          ? `${run.indexed ?? 0} written · ${run.pruned ?? 0} pruned · ${run.duration_ms ?? 0} ms`
          : run
            ? "running…"
            : "no pass yet"}
        {run?.error ? (
          <span className="mt-0.5 flex items-start gap-1 text-caution">
            <TriangleAlert className="mt-0.5 size-3 shrink-0" aria-hidden />
            {run.error}
          </span>
        ) : null}
      </td>
      <td className="px-3 py-2.5 text-right">
        <button
          type="button"
          onClick={() => onReindex(line.provider)}
          disabled={busy}
          data-reindex={line.provider}
          className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft disabled:opacity-50"
        >
          Reindex
        </button>
      </td>
    </tr>
  );
}

/** The search settings screen. */
export function SearchSettingsView() {
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [lines, setLines] = useState<SearchProviderStatus[]>([]);
  const [documents, setDocuments] = useState(0);
  const [settings, setSettings] = useState<SearchSettings | null>(null);
  const [weights, setWeights] = useState<SearchWeights | null>(null);
  const [enabled, setEnabled] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [progress, setProgress] = useState<string | null>(null);
  const [problems, setProblems] = useState<string[]>([]);
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);

  const load = useCallback(async () => {
    setStatus("loading");
    setLoadError(null);
    try {
      const [health, saved] = await Promise.all([fetchSearchStatus(), fetchSearchSettings()]);
      setLines(health.providers);
      setDocuments(health.documents);
      setSettings(saved);
      setWeights(saved.weights);
      setEnabled(new Set(saved.enabled_providers));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The search settings could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const reindex = useCallback(
    async (provider?: string) => {
      setBusy(provider ?? "all");
      setProgress(provider ? `Reindexing ${provider}…` : "Reindexing every provider…");
      try {
        const answer = await reindexSearch(provider);
        const written = answer.providers.reduce((sum, report) => sum + report.indexed, 0);
        const pruned = answer.providers.reduce((sum, report) => sum + report.pruned, 0);
        const duration = answer.providers.reduce((sum, report) => sum + report.duration_ms, 0);
        setProgress(
          `Indexed ${written} document${written === 1 ? "" : "s"}${
            pruned > 0 ? `, pruned ${pruned}` : ""
          } in ${duration} ms.`,
        );
        const health = await fetchSearchStatus();
        setLines(health.providers);
        setDocuments(health.documents);
      } catch (cause) {
        setProgress(
          cause instanceof ApiError
            ? `The reindex failed: ${cause.message} (${cause.code})`
            : "The reindex failed.",
        );
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const submit = async () => {
    if (!weights) return;
    const found: string[] = [];
    for (const field of WEIGHT_FIELDS) {
      const value = weights[field.key];
      if (!Number.isInteger(value) || value < 0 || value > 10) {
        found.push(`${field.label} must be a whole number between 0 and 10.`);
      }
    }
    if (weights.title < weights.body) {
      found.push("Title must be at least Body — a name outranks a mention.");
    }
    if (enabled.size === 0) {
      found.push("At least one provider must stay enabled, or search would answer nothing.");
    }
    setProblems(found);
    if (found.length > 0) {
      return;
    }

    setBusy("save");
    setProgress("Saving the ranking…");
    try {
      const saved = await saveSearchSettings({
        weights,
        enabled_providers: [...enabled].sort(
          (a, b) =>
            (settings?.available_providers.indexOf(a) ?? 0) -
            (settings?.available_providers.indexOf(b) ?? 0),
        ),
      });
      setSettings(saved);
      setWeights(saved.weights);
      setEnabled(new Set(saved.enabled_providers));
      setProgress("The ranking was saved.");
    } catch (cause) {
      setProgress(
        cause instanceof ApiError
          ? `The settings were refused: ${cause.message} (${cause.code})`
          : "The settings could not be saved.",
      );
    } finally {
      setBusy(null);
    }
  };

  const restoreDefaults = () => {
    if (!settings) return;
    setWeights(settings.defaults);
    setProblems([]);
    setProgress("The defaults are filled in — Save writes them.");
  };

  const dirty =
    settings !== null &&
    weights !== null &&
    (JSON.stringify(weights) !== JSON.stringify(settings.weights) ||
      [...enabled].sort().join(",") !== [...settings.enabled_providers].sort().join(","));

  if (status === "loading") {
    return (
      <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
        <span className="sr-only">Reading the index…</span>
        {[0, 1, 2].map((row) => (
          <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
        ))}
      </div>
    );
  }

  if (status === "error" || !settings || !weights) {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <ShieldAlert className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The search settings are unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {loadError?.message} <span className="font-mono text-[11.5px]">({loadError?.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <section className="flex flex-col gap-3" aria-label="Providers">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-[13.5px] font-semibold">Providers</h2>
            <p className="text-[12.5px] text-muted">
              {documents} document{documents === 1 ? "" : "s"} in the index. A pass leaves its own
              record: what it wrote, how long it took, and why it failed when it did.
            </p>
          </div>
          <button
            type="button"
            onClick={() => void reindex()}
            disabled={busy !== null}
            data-reindex-all
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Reindex all
          </button>
        </div>

        {/* The table needs a wide column; a phone gets the same rows as cards. */}
        <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface md:block">
          <table className="w-full text-left">
            <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
              <tr>
                <th className="px-3 py-2 font-medium">Provider</th>
                <th className="px-3 py-2 text-right font-medium">Documents</th>
                <th className="px-3 py-2 font-medium">Last indexed</th>
                <th className="px-3 py-2 font-medium">State</th>
                <th className="px-3 py-2 font-medium">Last pass</th>
                <th className="px-3 py-2" />
              </tr>
            </thead>
            <tbody>
              {lines.map((line) => (
                <ProviderRow
                  key={line.provider}
                  line={line}
                  busy={busy !== null}
                  onReindex={(provider) => void reindex(provider)}
                />
              ))}
            </tbody>
          </table>
        </div>

        <ul className="flex flex-col divide-y divide-line rounded-xl border border-line bg-surface md:hidden">
          {lines.map((line) => (
            <ProviderCard
              key={line.provider}
              line={line}
              busy={busy !== null}
              onReindex={(provider) => void reindex(provider)}
            />
          ))}
        </ul>

        {progress ? (
          <p
            role="status"
            data-search-progress
            className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px] text-ink"
          >
            {progress}
          </p>
        ) : null}
      </section>

      <section className="flex flex-col gap-3" aria-label="Ranking">
        <div>
          <h2 className="text-[13.5px] font-semibold">Ranking weights</h2>
          <p className="text-[12.5px] text-muted">
            How much each field of a document counts when the index orders the hits. Whole numbers
            from 0 to 10; the title may not weigh less than the body.
          </p>
        </div>

        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          {WEIGHT_FIELDS.map((field) => (
            <label key={field.key} className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">{field.label}</span>
              <input
                type="number"
                min={0}
                max={10}
                step={1}
                value={weights[field.key]}
                data-weight={field.key}
                onChange={(event) =>
                  setWeights({
                    ...weights,
                    [field.key]: Number(event.target.value),
                  })
                }
                className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              <span className="text-[11.5px] text-muted">{field.hint}</span>
            </label>
          ))}
        </div>

        <fieldset className="flex flex-col gap-2">
          <legend className="text-[12.5px] font-medium text-ink">Providers that answer</legend>
          <div className="flex flex-wrap gap-x-5 gap-y-2">
            {settings.available_providers.map((provider) => (
              <label key={provider} className="flex items-center gap-2 text-[12.5px]">
                <input
                  type="checkbox"
                  checked={enabled.has(provider)}
                  data-enabled-provider={provider}
                  onChange={(event) => {
                    const next = new Set(enabled);
                    if (event.target.checked) {
                      next.add(provider);
                    } else {
                      next.delete(provider);
                    }
                    setEnabled(next);
                  }}
                  className="size-3.5 accent-accent"
                />
                {provider}
              </label>
            ))}
          </div>
        </fieldset>

        {problems.length > 0 ? (
          <ul className="flex flex-col gap-1 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
            {problems.map((problem) => (
              <li key={problem}>{problem}</li>
            ))}
          </ul>
        ) : null}

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy !== null || !dirty}
            data-save-settings
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
          >
            <Save className="size-3.5" aria-hidden />
            Save
          </button>
          <button
            type="button"
            onClick={restoreDefaults}
            data-restore-defaults
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
          >
            <RotateCcw className="size-3.5" aria-hidden />
            Restore defaults
          </button>
          {!dirty ? (
            <span className="text-[12px] text-muted">
              Nothing to save
              {settings.updated_at ? ` · last change ${formatTimestamp(settings.updated_at)}` : ""}
            </span>
          ) : null}
        </div>
      </section>
    </div>
  );
}

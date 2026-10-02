"use client";

/**
 * `/deployment/seeds` — the datasets an operator can load into an installation (REQ-129, slice 3).
 *
 * ## The refusal replaces the button, it does not disable it
 *
 * `load_refused` comes from the API, which is the only thing that knows the installation kind. A
 * production installation renders the refusal and NO button at all — a Load button that 409s when
 * pressed is a dead button, and the request forbids those. Rendering the refusal instead is also
 * the honest thing: the operator learns that the environment is production without pressing
 * anything.
 *
 * ## The typed confirmation is the design, so the screen shows it
 *
 * Loading writes rows into a table that already has data, so the API demands the dataset's own
 * name in the request body. The dialog says why and asks for exactly that, with a live check of
 * whether what has been typed matches — the mistake the API is designed to catch (a typo answered
 * with a sentence about production) is then impossible to make in the first place.
 *
 * ## `files_present: false` is a real state, not a broken one
 *
 * A dataset declared by a migration whose manifest files are not in this build answers
 * `files_present: false` with its checksum still carrying the `declared-` prefix. That card renders
 * as unavailable with the reason, and offers nothing to press.
 *
 * Keyboard: `r` refreshes, `Esc` cancels the dialog.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import { AlertTriangle, Database, FileWarning, Lock, RefreshCw, Sprout } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, listSeeds, loadSeed, type SeedDataset, type SeedList } from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

export function SeedsView() {
  const [list, setList] = useState<SeedList | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // The dataset whose dialog is open. `null` is the closed state — not `false`, so there is exactly
  // one "no dialog" value rather than two that have to be kept in step.
  const [asking, setAsking] = useState<SeedDataset | null>(null);
  const [typed, setTyped] = useState("");
  const [confirmError, setConfirmError] = useState<string | null>(null);
  const typedRef = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setList(await listSeeds());
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The seed datasets could not be loaded.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.tagName === "INPUT") return;
      if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "Escape" && asking) {
        event.preventDefault();
        setAsking(null);
        setTyped("");
        setConfirmError(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, asking]);

  // Focus the confirmation box the moment the dialog opens: the whole point is that the operator
  // types the name, and a dialog that opens with focus on the backdrop is one extra click.
  useEffect(() => {
    if (asking) typedRef.current?.focus();
  }, [asking]);

  const close = useCallback(() => {
    setAsking(null);
    setTyped("");
    setConfirmError(null);
  }, []);

  const confirm = useCallback(async () => {
    if (!asking) return;
    if (typed.trim() !== asking.name) {
      setConfirmError(
        `Type "${asking.name}" exactly. This confirmation is what stops a mistyped dataset from loading.`,
      );
      return;
    }
    setConfirmError(null);
    setBusy(asking.name);
    try {
      const result = await loadSeed(asking.name, asking.name);
      setNotice(
        `"${result.dataset}" loaded — ${result.rows_loaded.toLocaleString()} rows written.`,
      );
      close();
      await load();
    } catch (caught) {
      setConfirmError(
        caught instanceof ApiError ? caught.message : "The load could not be completed.",
      );
    } finally {
      setBusy(null);
    }
  }, [asking, typed, close, load]);

  if (loading) return <LoadingTable columns={3} rows={2} />;

  const refusal = list?.load_refused ?? null;
  const matched = asking !== null && typed.trim() === asking.name;

  return (
    <div className="flex flex-col gap-5" data-view="deployment-seeds">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
        >
          <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
          <span>
            {error}{" "}
            <button type="button" onClick={() => void load()} className="inline-flex underline">
              Try again
            </button>
          </span>
        </div>
      ) : null}

      {/* The environment, named. And the refusal in place of the buttons rather than under them. */}
      <div
        className={`flex items-start gap-2.5 rounded-md border px-3 py-2.5 text-[12.5px] ${
          refusal
            ? "border-amber-300 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200"
            : "border-line bg-surface text-muted"
        }`}
      >
        {refusal ? (
          <Lock size={15} aria-hidden className="mt-0.5 shrink-0" />
        ) : (
          <Sprout size={15} aria-hidden className="mt-0.5 shrink-0" />
        )}
        <span>
          <span className="font-medium text-foreground">
            Installation kind: {list?.installation_kind}
          </span>
          {refusal ? <> — {refusal}</> : null}
          {list ? (
            <>
              {" "}
              <button
                type="button"
                onClick={() => void load()}
                className="ml-1 inline-flex items-center gap-1 underline"
              >
                <RefreshCw size={12} aria-hidden />
                Refresh <kbd className="text-[10.5px]">r</kbd>
              </button>
            </>
          ) : null}
        </span>
      </div>

      {notice ? (
        <p role="status" className="rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      {!list || list.datasets.length === 0 ? (
        <EmptyState
          title="No dataset is declared"
          hint="A migration declares the datasets this installation knows about. None is declared here, so there is nothing to load."
        />
      ) : (
        <ul className="grid gap-3 lg:grid-cols-2">
          {list.datasets.map((dataset) => {
            const unavailable = !dataset.files_present;
            const blocked = refusal !== null || unavailable;
            return (
              <li
                key={dataset.name}
                className="flex flex-col gap-2.5 rounded-md border border-line bg-surface p-4"
              >
                <div className="flex items-start justify-between gap-2">
                  <div>
                    <p className="font-mono text-[14px] font-medium">{dataset.name}</p>
                    <p className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-muted">
                      <span className="inline-flex items-center gap-1">
                        <Database size={12} aria-hidden />~{dataset.row_estimate.toLocaleString()}{" "}
                        rows
                      </span>
                      <span>schema {dataset.compatible_from}+</span>
                      {dataset.compatible_to ? (
                        <span>until {dataset.compatible_to}</span>
                      ) : null}
                    </p>
                  </div>
                </div>

                <p className="text-[12.5px] text-muted">{dataset.description}</p>

                {unavailable ? (
                  <p className="flex items-start gap-2 rounded border border-amber-300 bg-amber-50 px-2 py-1.5 text-[11.5px] text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200">
                    <FileWarning size={13} aria-hidden className="mt-0.5 shrink-0" />
                    <span>
                      Declared by a migration, but this build carries no manifest file for it — its
                      checksum is still the <code>declared-</code> placeholder. It cannot be loaded
                      until the dataset ships.
                    </span>
                  </p>
                ) : null}

                <div className="mt-auto flex items-center justify-between gap-2 pt-1">
                  <span className="truncate font-mono text-[10.5px] text-muted">
                    {dataset.manifest_checksum}
                  </span>
                  {blocked ? null : (
                    <button
                      type="button"
                      onClick={() => {
                        setAsking(dataset);
                        setTyped("");
                        setConfirmError(null);
                      }}
                      className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-line px-3 py-2 text-[12.5px]"
                    >
                      <Sprout size={13} aria-hidden />
                      Load this dataset
                    </button>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}

      {list && list.loads.length > 0 ? (
        <section aria-labelledby="loads-heading" className="flex flex-col gap-2">
          <h2 id="loads-heading" className="text-[13px] font-medium">
            Recent loads
          </h2>
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th className="px-3 py-2 font-normal">Dataset</th>
                  <th className="px-3 py-2 font-normal">Rows</th>
                  <th className="px-3 py-2 font-normal">Kind</th>
                  <th className="px-3 py-2 font-normal">When</th>
                </tr>
              </thead>
              <tbody>
                {list.loads.map((entry) => (
                  <tr key={entry.id} className="border-b border-line">
                    <td className="px-3 py-2.5 font-mono">{entry.dataset}</td>
                    <td className="px-3 py-2.5">{entry.rows_loaded.toLocaleString()}</td>
                    <td className="px-3 py-2.5">{entry.installation_kind}</td>
                    <td className="px-3 py-2.5 text-muted">{formatTimestamp(entry.loaded_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      ) : null}

      {/* The confirmation. Rendered only when open, so its inputs are never in the tab order of a
          screen that has nothing to confirm. */}
      {asking ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
          role="dialog"
          aria-modal="true"
          aria-labelledby="confirm-heading"
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-surface p-5 shadow-xl">
            <h3 id="confirm-heading" className="text-[15px] font-medium">
              Load <span className="font-mono">{asking.name}</span>?
            </h3>
            <p className="mt-1.5 text-[12.5px] text-muted">
              This writes roughly {asking.row_estimate.toLocaleString()} rows into a table that
              already has data. Every statement in the dataset is written to be safe to run twice,
              but it is still a write to production-shaped tables — so type the dataset&apos;s name
              to confirm.
            </p>

            <label className="mt-4 block">
              <span className="text-[12px] font-medium">
                Type <span className="font-mono">{asking.name}</span> to confirm
              </span>
              <input
                ref={typedRef}
                value={typed}
                onChange={(event) => {
                  setTyped(event.target.value);
                  setConfirmError(null);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void confirm();
                  }
                }}
                aria-invalid={confirmError !== null}
                className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 font-mono text-[13px] outline-none focus:border-accent"
              />
            </label>

            {confirmError ? (
              <p role="alert" className="mt-2 text-[12px] text-red-700 dark:text-red-300">
                {confirmError}
              </p>
            ) : null}

            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={close}
                className="rounded-md border border-line px-3 py-2 text-[13px]"
              >
                Cancel <kbd className="text-[10.5px] text-muted">Esc</kbd>
              </button>
              <button
                type="button"
                disabled={!matched || busy === asking.name}
                onClick={() => void confirm()}
                className="inline-flex items-center gap-1.5 rounded-md bg-red-600 px-3 py-2 text-[13px] text-white disabled:opacity-60"
              >
                <Sprout size={14} aria-hidden />
                {busy === asking.name ? "Loading…" : `Load ${asking.row_estimate.toLocaleString()} rows`}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

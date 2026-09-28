"use client";

/**
 * Scanning: the virus-scanning policy, the run log and the quarantine list (REQ-010, slice 4).
 *
 * Four things on this tab are not decoration, and each exists because the shortcut is wrong:
 *
 * - **the policy states its consequence in a sentence.** A checkbox reading "Hold files whose
 *   scan could not complete" tells an operator what the *setting* does; `behaviour` tells them
 *   what will happen to their *uploads*, and those are different sentences and only one of
 *   them is the one somebody needs at 02:00;
 * - **the secret field is a reference, and the screen says whether it resolves.** The row
 *   stores the *name* of an environment variable. If this process cannot see it, scanning is
 *   configured and will never run — which looks exactly like a scanner that finds nothing, so
 *   the mismatch is stated rather than left to be discovered;
 * - **the probe posts real bytes.** A `Test` that answered "port open" would be green on a
 *   scanner that refuses actual uploads, which is the configuration that leaves a library
 *   where every file reads `error`;
 * - **a release asks for a reason and says what the release does not do.** A released file
 *   goes back to serving, but nothing has said it is *safe* — it is not re-scanned, and the
 *   row records that a human decided to let it go rather than that the scanner cleared it.
 */
import { useCallback, useEffect, useState } from "react";

import {
  CheckCircle2,
  Loader2,
  Play,
  Save,
  ShieldAlert,
  ShieldCheck,
  TriangleAlert,
  Unlock,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchMediaQuarantine,
  fetchMediaScanRuns,
  fetchMediaScanSettings,
  releaseMediaQuarantine,
  runMediaScan,
  saveMediaScanSettings,
  testMediaScanner,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import type {
  MediaQuarantineList,
  MediaScanProbe,
  MediaScanRunList,
  MediaScanSettings,
  MediaScanSettingsInput,
} from "@/lib/types";

/** What an unreachable scanner means, spelled out as the consequence rather than a word. */
const ON_ERROR: [string, string][] = [
  ["hold", "Hold — a file whose scan could not complete is NOT served until it clears"],
  ["serve", "Serve — a file whose scan could not complete is served anyway, marked `error`"],
];

/** The bounds the API enforces, restated here so the form can refuse before it posts. */
const TIMEOUT_RANGE = { min: 1, max: 120 } as const;
const SIZE_RANGE = { min: 1, max: 1024 } as const;

/** One setting as the editor holds it while it is being changed. */
type Draft = {
  enabled: boolean;
  endpoint: string;
  secret_env: string;
  timeout_seconds: string;
  on_error: string;
  max_scan_mb: string;
};

/** The form's draft as a fresh copy of a stored policy. */
function toDraft(settings: MediaScanSettings): Draft {
  return {
    enabled: settings.enabled,
    endpoint: settings.endpoint,
    secret_env: settings.secret_env,
    timeout_seconds: String(settings.timeout_seconds),
    on_error: settings.on_error,
    max_scan_mb: String(settings.max_scan_mb),
  };
}

/** The draft as the body a save or a probe sends. */
function toInput(draft: Draft): MediaScanSettingsInput {
  return {
    enabled: draft.enabled,
    endpoint: draft.endpoint.trim(),
    secret_env: draft.secret_env.trim(),
    timeout_seconds: Number(draft.timeout_seconds),
    on_error: draft.on_error,
    max_scan_mb: Number(draft.max_scan_mb),
  };
}

/** A human byte count, because "held 1 file" and "held 900 MB" are different urgencies. */
function bytes(value: number): string {
  if (value < 1024) return `${value} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let size = value / 1024;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size >= 10 ? Math.round(size) : size.toFixed(1)} ${units[unit]}`;
}

/** A short local timestamp, so the run log is readable without a date library. */
function when(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** The per-site scanning policy, the run log and the quarantine list. */
export function MediaScanningView() {
  const { selectedSite } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [settings, setSettings] = useState<MediaScanSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [runs, setRuns] = useState<MediaScanRunList | null>(null);
  const [quarantine, setQuarantine] = useState<MediaQuarantineList | null>(null);
  const [probe, setProbe] = useState<MediaScanProbe | null>(null);
  const [sweep, setSweep] = useState<string | null>(null);

  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [running, setRunning] = useState(false);
  const [releasing, setReleasing] = useState<string | null>(null);
  const [releaseFor, setReleaseFor] = useState<string | null>(null);
  const [releaseReason, setReleaseReason] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);

  const load = useCallback(async () => {
    if (!siteId) return;
    setLoading(true);
    setError(null);
    try {
      const [policy, log, held] = await Promise.all([
        fetchMediaScanSettings(siteId),
        fetchMediaScanRuns(siteId),
        fetchMediaQuarantine(siteId),
      ]);
      setSettings(policy);
      setDraft(toDraft(policy));
      setRuns(log);
      setQuarantine(held);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load]);

  /** Split an API failure into a field error and a general one, using the server's own answer. */
  const applyError = useCallback((cause: unknown) => {
    if (cause instanceof ApiError) {
      const field = cause.details?.field;
      if (typeof field === "string") {
        setFieldError({ field, message: cause.message });
        return;
      }
      setError(cause.message);
      return;
    }
    setError(String(cause));
  }, []);

  const save = useCallback(async () => {
    if (!siteId || !draft) return;
    setSaving(true);
    setError(null);
    setFieldError(null);
    try {
      const saved = await saveMediaScanSettings(siteId, toInput(draft));
      setSettings(saved);
      setDraft(toDraft(saved));
    } catch (cause) {
      applyError(cause);
    } finally {
      setSaving(false);
    }
  }, [siteId, draft, applyError]);

  const test = useCallback(async () => {
    if (!siteId || !draft) return;
    setTesting(true);
    setProbe(null);
    setFieldError(null);
    try {
      // The *candidate*, not the saved row: the person clicking has unsaved edits in front
      // of them, and a result about the saved row is a result about something else.
      setProbe(await testMediaScanner(siteId, toInput(draft)));
    } catch (cause) {
      applyError(cause);
    } finally {
      setTesting(false);
    }
  }, [siteId, draft, applyError]);

  const sweepNow = useCallback(async () => {
    if (!siteId) return;
    setRunning(true);
    setError(null);
    setSweep(null);
    try {
      const result = await runMediaScan(siteId);
      setSweep(result.summary);
      setQuarantine(result.quarantine);
      setRuns(await fetchMediaScanRuns(siteId));
      setSettings(await fetchMediaScanSettings(siteId));
    } catch (cause) {
      applyError(cause);
    } finally {
      setRunning(false);
    }
  }, [siteId, applyError]);

  const release = useCallback(async () => {
    if (!releaseFor) return;
    const reason = releaseReason.trim();
    if (!reason) {
      // Checked here as well as on the server: a person who clicks Release with an empty box
      // should be told why rather than watching a request fail.
      setFieldError({
        field: "reason",
        message: "Say why this file is being released — the quarantine record keeps it.",
      });
      return;
    }
    setReleasing(releaseFor);
    setError(null);
    setFieldError(null);
    try {
      await releaseMediaQuarantine(releaseFor, reason);
      setReleaseFor(null);
      setReleaseReason("");
      if (siteId) {
        setQuarantine(await fetchMediaQuarantine(siteId));
        setRuns(await fetchMediaScanRuns(siteId));
      }
    } catch (cause) {
      applyError(cause);
    } finally {
      setReleasing(null);
    }
  }, [releaseFor, releaseReason, siteId, applyError]);

  if (!siteId) {
    return (
      <EmptyState
        title="No site selected"
        hint="Choose a site to configure how its uploads are scanned."
      />
    );
  }
  if (loading || !settings || !draft) return <LoadingTable columns={3} rows={4} />;

  /** One field's error, shown under the input that caused it. */
  const fieldProblem = (name: string) =>
    fieldError?.field === name ? (
      <p className="mt-1 text-[12px] text-danger" role="alert">
        {fieldError.message}
      </p>
    ) : null;

  const inputClass = (name: string) =>
    [
      "w-full rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink",
      fieldError?.field === name ? "border-danger" : "border-line",
    ].join(" ");

  return (
    <div className="space-y-6">
      {error ? (
        <p
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
        >
          <TriangleAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}

      {/* ------------------------------------------------------------------ the policy */}
      <section className="rounded-xl border border-line p-4">
        <header className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-[13.5px] font-medium text-ink">Scanning policy</h2>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => void test()}
              disabled={testing}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:bg-canvas hover:text-ink disabled:opacity-50"
            >
              {testing ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <ShieldCheck className="size-3.5" aria-hidden />
              )}
              Test scanner
            </button>
            <button
              type="button"
              onClick={() => void save()}
              disabled={saving}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] text-white transition disabled:opacity-50"
            >
              {saving ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <Save className="size-3.5" aria-hidden />
              )}
              Save
            </button>
          </div>
        </header>

        {/* The consequence, in a sentence, before the controls that produce it. */}
        <p className="mb-4 rounded-lg bg-canvas px-3 py-2 text-[12.5px] leading-relaxed text-muted">
          {settings.behaviour}
        </p>

        <div className="space-y-4">
          <label className="flex items-start gap-3">
            <input
              type="checkbox"
              checked={draft.enabled}
              onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })}
              className="mt-0.5 size-4 accent-[var(--accent)]"
            />
            <span className="text-[13px] text-ink">
              Scan uploads
              <span className="mt-0.5 block text-[12px] text-muted">
                Uploads are stored immediately and scanned afterwards. An upload is never lost
                because a scanner is down.
              </span>
            </span>
          </label>

          <div className="grid gap-4 sm:grid-cols-2">
            <div>
              <label htmlFor="scan-endpoint" className="mb-1 block text-[12.5px] text-muted">
                Scanner endpoint
              </label>
              <input
                id="scan-endpoint"
                type="text"
                value={draft.endpoint}
                onChange={(event) => setDraft({ ...draft, endpoint: event.target.value })}
                placeholder="https://scanner.internal:3310"
                aria-invalid={fieldError?.field === "endpoint"}
                className={inputClass("endpoint")}
              />
              {fieldProblem("endpoint")}
              <p className="mt-1 text-[11.5px] text-muted">
                A bare origin — no path, query or fragment.
              </p>
            </div>

            <div>
              <label htmlFor="scan-secret" className="mb-1 block text-[12.5px] text-muted">
                Shared secret
              </label>
              <input
                id="scan-secret"
                type="text"
                value={draft.secret_env}
                onChange={(event) => setDraft({ ...draft, secret_env: event.target.value })}
                placeholder="MEDIA_SCAN_SECRET"
                aria-invalid={fieldError?.field === "secret_env"}
                className={inputClass("secret_env")}
              />
              {fieldProblem("secret_env")}
              <p className="mt-1 text-[11.5px] text-muted">
                The *name* of the environment variable that holds the secret — not the secret.
                {settings.secret_env && !settings.secret_available ? (
                  <span className="mt-1 block font-medium text-warning">
                    This process cannot see {settings.secret_env}, so scans will fail until the
                    variable is present in its environment.
                  </span>
                ) : null}
              </p>
            </div>

            <div>
              <label htmlFor="scan-timeout" className="mb-1 block text-[12.5px] text-muted">
                Timeout (seconds)
              </label>
              <input
                id="scan-timeout"
                type="number"
                min={TIMEOUT_RANGE.min}
                max={TIMEOUT_RANGE.max}
                value={draft.timeout_seconds}
                onChange={(event) => setDraft({ ...draft, timeout_seconds: event.target.value })}
                aria-invalid={fieldError?.field === "timeout_seconds"}
                className={inputClass("timeout_seconds")}
              />
              {fieldProblem("timeout_seconds")}
            </div>

            <div>
              <label htmlFor="scan-ceiling" className="mb-1 block text-[12.5px] text-muted">
                Size ceiling (MB)
              </label>
              <input
                id="scan-ceiling"
                type="number"
                min={SIZE_RANGE.min}
                max={SIZE_RANGE.max}
                value={draft.max_scan_mb}
                onChange={(event) => setDraft({ ...draft, max_scan_mb: event.target.value })}
                aria-invalid={fieldError?.field === "max_scan_mb"}
                className={inputClass("max_scan_mb")}
              />
              {fieldProblem("max_scan_mb")}
              <p className="mt-1 text-[11.5px] text-muted">
                Files above this are marked <code>skipped</code> — nobody looked at them.
              </p>
            </div>
          </div>

          <div>
            <span className="mb-1 block text-[12.5px] text-muted">When a scan cannot complete</span>
            <div className="space-y-1.5">
              {ON_ERROR.map(([value, label]) => (
                <label key={value} className="flex items-start gap-2 text-[12.5px] text-ink">
                  <input
                    type="radio"
                    name="scan-on-error"
                    value={value}
                    checked={draft.on_error === value}
                    onChange={() => setDraft({ ...draft, on_error: value })}
                    className="mt-0.5 size-3.5 accent-[var(--accent)]"
                  />
                  {label}
                </label>
              ))}
            </div>
            {fieldProblem("on_error")}
          </div>
        </div>

        {probe ? (
          <p
            role="status"
            className={[
              "mt-4 flex items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px]",
              probe.ok
                ? "border-success/40 bg-success/5 text-success"
                : "border-warning/40 bg-warning/5 text-warning",
            ].join(" ")}
          >
            {probe.ok ? (
              <CheckCircle2 className="mt-0.5 size-4 shrink-0" aria-hidden />
            ) : (
              <TriangleAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
            )}
            <span>
              {probe.ok
                ? `The scanner accepted a real payload and reported \`${probe.status}\`${probe.engine ? ` from ${probe.engine}` : ""}.`
                : `The scanner answered \`${probe.status}\`${probe.detail ? `: ${probe.detail}` : ""}.`}
              <span className="mt-0.5 block opacity-80">{probe.note}</span>
            </span>
          </p>
        ) : null}
      </section>

      {/* ------------------------------------------------------------------ the sweep */}
      <section className="rounded-xl border border-line p-4">
        <header className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <div>
            <h2 className="text-[13.5px] font-medium text-ink">Scanning pass</h2>
            <p className="mt-0.5 text-[12px] text-muted">
              {settings.pending_count} file{settings.pending_count === 1 ? "" : "s"} waiting for
              their first scan.
            </p>
          </div>
          <button
            type="button"
            onClick={() => void sweepNow()}
            disabled={running || !settings.enabled}
            title={settings.enabled ? undefined : "Turn scanning on first"}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:bg-canvas hover:text-ink disabled:cursor-not-allowed disabled:opacity-50"
          >
            {running ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Play className="size-3.5" aria-hidden />
            )}
            Run now
          </button>
        </header>
        {sweep ? (
          <p role="status" className="rounded-lg bg-canvas px-3 py-2 text-[12.5px] text-muted">
            {sweep}
          </p>
        ) : null}

        {runs && runs.runs.length > 0 ? (
          <table className="mt-3 w-full text-left text-[12.5px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="py-2 pr-3 font-medium">When</th>
                <th scope="col" className="py-2 pr-3 font-medium">Outcome</th>
                <th scope="col" className="py-2 font-medium">Summary</th>
              </tr>
            </thead>
            <tbody>
              {runs.runs.map((run) => (
                <tr key={run.id} className="border-b border-line/50 last:border-0">
                  <td className="py-2 pr-3 whitespace-nowrap text-muted">{when(run.started_at)}</td>
                  <td className="py-2 pr-3">
                    <span
                      className={[
                        "rounded-md px-1.5 py-0.5 text-[11px] font-medium",
                        run.outcome === "clean"
                          ? "bg-success/10 text-success"
                          : run.outcome === "flagged"
                            ? "bg-danger/10 text-danger"
                            : "bg-warning/10 text-warning",
                      ].join(" ")}
                    >
                      {run.outcome}
                    </span>
                  </td>
                  <td className="py-2 text-ink">{run.summary}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : (
          <p className="mt-3 rounded-lg bg-canvas px-3 py-2 text-[12.5px] text-muted">
            No pass has run yet. A run that finds nothing still writes a row here — that is how
            &ldquo;the last pass was clean&rdquo; is answerable on the day nothing was found.
          </p>
        )}
      </section>

      {/* ------------------------------------------------------------------ the quarantine */}
      <section className="rounded-xl border border-line p-4">
        <header className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-[13.5px] font-medium text-ink">Quarantine</h2>
          {quarantine && quarantine.file_count > 0 ? (
            <p className="text-[12px] text-muted">
              {quarantine.file_count} held · {bytes(quarantine.total_bytes)}
            </p>
          ) : null}
        </header>

        {quarantine && quarantine.entries.length > 0 ? (
          <ul className="space-y-2">
            {quarantine.entries.map((entry) => (
              <li key={entry.id} className="rounded-lg border border-line p-3">
                <div className="flex flex-wrap items-start justify-between gap-2">
                  <div className="min-w-0 flex-1">
                    <p className="flex items-center gap-1.5 text-[12.5px] font-medium text-ink">
                      <ShieldAlert className="size-3.5 shrink-0 text-danger" aria-hidden />
                      Held {when(entry.quarantined_at)}
                    </p>
                    {/* The scanner's own words, not a translation of them. */}
                    <p className="mt-1 text-[12px] leading-relaxed break-words text-muted">
                      {entry.detail}
                    </p>
                  </div>
                  {releaseFor === entry.id ? (
                    <div className="w-full space-y-2 sm:w-72">
                      <label
                        htmlFor={`reason-${entry.id}`}
                        className="block text-[12px] text-muted"
                      >
                        Why is it being released?
                      </label>
                      <input
                        id={`reason-${entry.id}`}
                        type="text"
                        autoFocus
                        value={releaseReason}
                        onChange={(event) => setReleaseReason(event.target.value)}
                        placeholder="Checked by hand: an internal test payload"
                        aria-invalid={fieldError?.field === "reason"}
                        className={inputClass("reason")}
                      />
                      {fieldProblem("reason")}
                      <p className="text-[11.5px] text-muted">
                        A release puts the file back into circulation. It is <em>not</em> a clean
                        scan, and the record says a human decided, not that the scanner cleared it.
                      </p>
                      <div className="flex items-center gap-2">
                        <button
                          type="button"
                          onClick={() => void release()}
                          disabled={releasing === entry.id}
                          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] text-white disabled:opacity-50"
                        >
                          {releasing === entry.id ? (
                            <Loader2 className="size-3.5 animate-spin" aria-hidden />
                          ) : (
                            <Unlock className="size-3.5" aria-hidden />
                          )}
                          Release
                        </button>
                        <button
                          type="button"
                          onClick={() => {
                            setReleaseFor(null);
                            setReleaseReason("");
                            setFieldError(null);
                          }}
                          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:bg-canvas hover:text-ink"
                        >
                          Cancel
                        </button>
                      </div>
                    </div>
                  ) : (
                    <button
                      type="button"
                      onClick={() => {
                        setReleaseFor(entry.id);
                        setReleaseReason("");
                        setFieldError(null);
                      }}
                      className="inline-flex shrink-0 items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:bg-canvas hover:text-ink"
                    >
                      <Unlock className="size-3.5" aria-hidden />
                      Release
                    </button>
                  )}
                </div>
              </li>
            ))}
          </ul>
        ) : (
          <EmptyState
            title="Nothing is held"
            hint="Files the scanner flags appear here, unserved until somebody releases them."
          />
        )}
      </section>
    </div>
  );
}

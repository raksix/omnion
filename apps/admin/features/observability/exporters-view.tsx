"use client";

/**
 * `/observability/exporters` — the sinks, their health, and their drop counters
 * (docs/requests/REQ-126, slice 3).
 *
 * The screen's job is to make one trade-off legible, because the request states it plainly:
 * **"Buffered telemetry loses data by design when a backend is down; the drop counter and the
 * health chip make that honest instead of silent, and the export screen states the trade-off."**
 * So every row shows the buffer as a bar against its cap, the drop counter, and the last flush —
 * and a degraded exporter says what is being lost, not just that it is sad.
 *
 * The states this screen refuses to blur, each of which otherwise looks fine:
 *
 * - **`unknown` is not `ok`.** A row saved but not yet loaded by this process reports `unknown`,
 *   which is a real state with a real meaning: this process has not proven the backend works. A
 *   green chip there would be the screen lying on the operator's behalf.
 * - **`degraded` is one refused batch; `down` is two.** Deliberate, from slice 3: a single
 *   refused batch is the normal noise of a restarting backend, and flipping to red for it is how
 *   operators learn to ignore the chip.
 * - **`Test` sends something.** It posts a fixed, obviously-synthetic document and reports the
 *   backend's own words. A `Test` that only validated the form would be a dead button that tells
 *   the operator their endpoint is fine right up until telemetry silently does not arrive. A
 *   failure renders as a degraded REPORT, not an error page.
 * - **The form never renders a secret back.** Auth is a secret id from the store, the API never
 *   returns a value, and the field here is a picker of ids. There is no "reveal" and no value
 *   input, because a field that could hold a credential is a field that eventually will.
 *
 * Keyboard: `/` focuses the filter, `n` opens the new form, `t` tests the selected row, `r`
 * refreshes, `Esc` closes the form. Under `sm:` the table becomes cards and the form is one
 * column.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  Loader2,
  Plus,
  RefreshCw,
  Search,
  Send,
  Trash2,
  X,
  XCircle,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createExporter,
  deleteExporter,
  fetchExporters,
  testExporter,
  updateExporter,
  type ExporterInput,
  type ExporterRow,
  type ExporterTestResult,
  type ExportersResponse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The health chip, and the sentence under it. The two are one decision, not two. */
const HEALTH: Record<
  string,
  { tone: string; icon: typeof Activity; label: string; meaning: string }
> = {
  ok: {
    tone: "bg-positive-soft text-positive",
    icon: CheckCircle2,
    label: "ok",
    meaning: "A batch was accepted and nothing is waiting.",
  },
  degraded: {
    tone: "bg-caution-soft text-caution",
    icon: AlertTriangle,
    label: "degraded",
    meaning: "One batch was refused. The buffer is still draining; watch the next flush.",
  },
  down: {
    tone: "bg-danger-soft text-danger",
    icon: XCircle,
    label: "down",
    meaning: "Two or more batches refused. Telemetry is being dropped oldest-first and counted.",
  },
  unknown: {
    tone: "bg-quiet-soft text-muted",
    icon: Activity,
    label: "unknown",
    meaning: "This process has not proven the backend works — saved, loaded, but never flushed.",
  },
};

function healthOf(name: string) {
  return (
    HEALTH[name] ?? {
      tone: "bg-quiet-soft text-muted",
      icon: Activity,
      label: name,
      meaning: "No health rule matches this value.",
    }
  );
}

/** The buffer as a bar. Full is the alarming state, and it is drawn as such. */
function BufferBar({ buffered, capacity }: { buffered: number; capacity: number }) {
  const percent = capacity > 0 ? Math.min(100, (buffered / capacity) * 100) : 0;
  const full = percent >= 90;
  return (
    <span className="inline-flex items-center gap-1.5" title={`${buffered} of ${capacity} waiting`}>
      <span className="relative h-2 w-16 overflow-hidden rounded-full bg-quiet-soft">
        <span
          className={`absolute inset-y-0 left-0 rounded-full ${full ? "bg-danger" : "bg-accent"}`}
          style={{ width: `${percent}%` }}
        />
      </span>
      <span className="font-mono text-[11px] tabular-nums text-muted">
        {buffered}/{capacity}
      </span>
    </span>
  );
}

interface FormState {
  name: string;
  kind: string;
  endpoint: string;
  protocol: string;
  auth_secret_id: string;
  batch_ms: string;
  timeout_ms: string;
  enabled: boolean;
}

const BLANK: FormState = {
  name: "",
  kind: "otlp",
  endpoint: "",
  protocol: "",
  auth_secret_id: "",
  batch_ms: "5000",
  timeout_ms: "10000",
  enabled: true,
};

function toForm(row: ExporterRow): FormState {
  return {
    name: row.name,
    kind: row.kind,
    endpoint: row.endpoint,
    protocol: row.protocol ?? "",
    auth_secret_id: "",
    batch_ms: String(row.batch_ms),
    timeout_ms: String(row.timeout_ms),
    enabled: row.enabled,
  };
}

function ExporterForm({
  initial,
  kinds,
  saving,
  error,
  onSave,
  onClose,
}: {
  initial: FormState;
  kinds: string[];
  saving: boolean;
  error: string | null;
  onSave: (form: FormState) => void;
  onClose: () => void;
}) {
  const [form, setForm] = useState<FormState>(initial);
  const nameRef = useRef<HTMLInputElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    setForm((current) => ({ ...current, [key]: value }));

  // The OTLP endpoint is a collector base, and the transport appends `/v1/logs`; a syslog or
  // webhook endpoint is posted to verbatim. Saying that in the form is cheaper than a 404 later.
  const endpointHint =
    form.kind === "otlp"
      ? "The collector base, e.g. http://otel-collector:4318. The /v1/logs path is appended."
      : "The full URL batches are POSTed to.";

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="exporter-form-title"
      data-exporter-form
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <div className="w-full max-w-lg rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="exporter-form-title"
          ref={heading}
          tabIndex={-1}
          className="flex items-center gap-2 text-[15px] font-medium outline-none"
        >
          <Send className="size-4 text-accent" aria-hidden="true" />
          {initial.name ? "Edit this exporter" : "Add an exporter"}
        </h3>
        <p className="mt-1.5 text-[12.5px] text-muted">
          Auth is a secret reference from the store, never a value typed here. Configuring an
          exporter sends this instance&rsquo;s log lines and span attributes to that endpoint;
          prompts, completions, user data and secret values never leave.
        </p>

        <form
          className="mt-4 space-y-3"
          onSubmit={(event) => {
            event.preventDefault();
            onSave(form);
          }}
        >
          <div>
            <label className="text-[12px] text-muted" htmlFor="exporter-name">
              Name — the drop counter&rsquo;s label, so keep it short and stable
            </label>
            <input
              id="exporter-name"
              ref={nameRef}
              value={form.name}
              onChange={(event) => set("name", event.target.value)}
              data-exporter-name
              required
              maxLength={64}
              placeholder="otel-collector"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
            />
          </div>

          <div className="grid gap-3 sm:grid-cols-2">
            <div>
              <label className="text-[12px] text-muted" htmlFor="exporter-kind">
                Kind
              </label>
              <select
                id="exporter-kind"
                value={form.kind}
                onChange={(event) => set("kind", event.target.value)}
                data-exporter-kind
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
              >
                {kinds.map((kind) => (
                  <option key={kind} value={kind}>
                    {kind}
                  </option>
                ))}
              </select>
            </div>
            <div>
              <label className="text-[12px] text-muted" htmlFor="exporter-protocol">
                Protocol (optional)
              </label>
              <input
                id="exporter-protocol"
                value={form.protocol}
                onChange={(event) => set("protocol", event.target.value)}
                data-exporter-protocol
                placeholder="http/protobuf"
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
              />
            </div>
          </div>

          <div>
            <label className="text-[12px] text-muted" htmlFor="exporter-endpoint">
              Endpoint
            </label>
            <input
              id="exporter-endpoint"
              value={form.endpoint}
              onChange={(event) => set("endpoint", event.target.value)}
              data-exporter-endpoint
              required
              placeholder="http://otel-collector:4318"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
            />
            <p className="mt-1 text-[11.5px] text-muted">{endpointHint}</p>
          </div>

          <div>
            <label className="text-[12px] text-muted" htmlFor="exporter-secret">
              Auth secret id (optional) — a reference, not a value
            </label>
            <input
              id="exporter-secret"
              value={form.auth_secret_id}
              onChange={(event) => set("auth_secret_id", event.target.value)}
              data-exporter-secret
              placeholder="a uuid from the secret store, left blank for none"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
            />
            <p className="mt-1 text-[11.5px] text-muted">
              Leave blank on an edit to keep the current reference. The form can never show what
              the secret holds, because the API never returns it.
            </p>
          </div>

          <div className="grid gap-3 sm:grid-cols-2">
            <div>
              <label className="text-[12px] text-muted" htmlFor="exporter-batch">
                Batch interval (ms) — 100 to 3,600,000
              </label>
              <input
                id="exporter-batch"
                value={form.batch_ms}
                onChange={(event) => set("batch_ms", event.target.value)}
                data-exporter-batch
                inputMode="numeric"
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
              />
            </div>
            <div>
              <label className="text-[12px] text-muted" htmlFor="exporter-timeout">
                Flush timeout (ms) — 100 to 600,000
              </label>
              <input
                id="exporter-timeout"
                value={form.timeout_ms}
                onChange={(event) => set("timeout_ms", event.target.value)}
                data-exporter-timeout
                inputMode="numeric"
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
              />
            </div>
          </div>

          <label className="flex items-center gap-2 text-[13px]">
            <input
              type="checkbox"
              checked={form.enabled}
              onChange={(event) => set("enabled", event.target.checked)}
              data-exporter-enabled
              className="size-4 accent-[var(--accent)]"
            />
            Enabled — turning this off drains and counts the backlog rather than holding it
          </label>

          {error ? (
            <p role="alert" data-exporter-form-error className="text-[12px] text-danger">
              {error}
            </p>
          ) : null}

          <div className="flex justify-end gap-2 pt-1">
            <button
              type="button"
              onClick={onClose}
              className="rounded-lg border border-line px-3 py-1.5 text-[13px] hover:bg-quiet-soft"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={saving}
              data-exporter-save
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[13px] text-white disabled:opacity-60"
            >
              {saving ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
              ) : (
                <CheckCircle2 className="size-3.5" aria-hidden="true" />
              )}
              Save
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

export function ExportersView() {
  const [data, setData] = useState<ExportersResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [notice, setNotice] = useState<string | null>(null);

  const [editing, setEditing] = useState<ExporterRow | null>(null);
  const [creating, setCreating] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const [testing, setTesting] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<ExporterTestResult | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);

  const filterRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchExporters());
    } catch (caught) {
      setError(
        caught instanceof ApiError
          ? caught.message
          : "The exporter list could not be read.",
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
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (typing || creating || editing) return;
      if (event.key === "/") {
        event.preventDefault();
        filterRef.current?.focus();
      } else if (event.key === "n") {
        event.preventDefault();
        setFormError(null);
        setCreating(true);
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, creating, editing]);

  const rows = useMemo(() => {
    const all = data?.exporters ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (row) =>
        row.name.toLowerCase().includes(needle) ||
        row.kind.toLowerCase().includes(needle) ||
        row.endpoint.toLowerCase().includes(needle),
    );
  }, [data, filter]);

  const save = async (form: FormState) => {
    setSaving(true);
    setFormError(null);
    const input: ExporterInput = {
      name: form.name.trim(),
      kind: form.kind,
      endpoint: form.endpoint.trim(),
      protocol: form.protocol.trim() || null,
      // An empty box on an edit means "keep the current reference", so it is not sent at all —
      // sending `null` would silently de-authenticate the exporter on a typo.
      ...(form.auth_secret_id.trim() ? { auth_secret_id: form.auth_secret_id.trim() } : {}),
      batch_ms: Number(form.batch_ms),
      timeout_ms: Number(form.timeout_ms),
      enabled: form.enabled,
    };
    try {
      if (editing) {
        await updateExporter(editing.id, input);
        setNotice(`${input.name} updated`);
      } else {
        await createExporter(input);
        setNotice(`${input.name} added`);
      }
      setEditing(null);
      setCreating(false);
      await load();
      window.setTimeout(() => setNotice(null), 3000);
    } catch (caught) {
      setFormError(
        caught instanceof ApiError ? caught.message : "The exporter could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  };

  const test = async (row: ExporterRow) => {
    setTesting(row.id);
    setTestResult(null);
    try {
      setTestResult(await testExporter(row.id));
    } catch (caught) {
      setTestResult({
        name: row.name,
        kind: row.kind,
        ok: false,
        detail:
          caught instanceof ApiError ? caught.message : "The probe could not be sent.",
        health: "unknown",
      });
    } finally {
      setTesting(null);
      await load();
    }
  };

  const remove = async (row: ExporterRow) => {
    setRemoving(row.id);
    try {
      await deleteExporter(row.id);
      setNotice(`${row.name} removed`);
      await load();
      window.setTimeout(() => setNotice(null), 3000);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The exporter could not be removed.");
    } finally {
      setRemoving(null);
    }
  };

  const openForm = creating || editing !== null;

  return (
    <div className="space-y-4" data-view="observability-exporters">
      <p
        data-exporter-egress-notice
        className="rounded-lg border border-line bg-quiet-soft/40 px-3 py-2.5 text-[12.5px]"
      >
        <span className="font-medium">What leaves this instance.</span>{" "}
        {data?.egress_notice ??
          "Configuring an exporter sends log lines and span attributes to the endpoint you give it."}
      </p>

      <section className="rounded-xl border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="flex items-center gap-2 text-[13.5px] font-medium">
            <Send className="size-4 text-accent" aria-hidden="true" />
            Exporters
          </h2>
          {data ? (
            <span className="text-[12px] text-muted">
              {rows.length} of {data.exporters.length}
            </span>
          ) : null}
          {notice ? (
            <span data-exporter-notice className="text-[12px] text-positive">
              {notice}
            </span>
          ) : null}
          <div className="ml-auto flex items-center gap-2">
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden="true"
              />
              <input
                ref={filterRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                data-exporter-filter
                placeholder="Filter by name, kind or endpoint"
                aria-label="Filter exporters"
                className="h-8 w-56 rounded-lg border border-line bg-surface pl-7 pr-2 text-[12px] outline-none focus:border-accent"
              />
            </div>
            <button
              type="button"
              onClick={() => void load()}
              data-exporter-refresh
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <RefreshCw className="h-3 w-3" aria-hidden="true" />
              Refresh
            </button>
            <button
              type="button"
              onClick={() => {
                setFormError(null);
                setCreating(true);
              }}
              data-exporter-add
              className="flex items-center gap-1 rounded-md bg-accent px-2.5 py-1 text-[12px] text-white hover:opacity-90"
            >
              <Plus className="h-3 w-3" aria-hidden="true" />
              Add
            </button>
          </div>
        </header>

        {error ? (
          <p role="alert" data-exporter-error className="px-4 py-3 text-[12.5px] text-danger">
            {error}
          </p>
        ) : loading && !data ? (
          <div className="px-4 py-3">
            <LoadingTable rows={3} columns={6} />
          </div>
        ) : rows.length === 0 ? (
          <div className="px-4 py-3">
            <EmptyState
              title={filter ? "No exporter matches" : "No exporters configured"}
              hint={
                filter
                  ? "Clear the filter to see every configured sink."
                  : "Telemetry stays in this instance until you configure a sink. Add one to send log lines and redacted span attributes to a collector, a Prometheus remote-write target, syslog or a webhook."
              }
            />
          </div>
        ) : (
          <>
            <table className="hidden w-full text-left text-[12.5px] md:table" data-exporter-table>
              <thead className="text-[11.5px] uppercase tracking-wide text-muted">
                <tr>
                  <th scope="col" className="px-4 py-2 font-medium">Name</th>
                  <th scope="col" className="px-2 py-2 font-medium">Kind</th>
                  <th scope="col" className="px-2 py-2 font-medium">Health</th>
                  <th scope="col" className="px-2 py-2 font-medium">Buffer</th>
                  <th scope="col" className="px-2 py-2 font-medium">Dropped</th>
                  <th scope="col" className="px-2 py-2 font-medium">Last flush</th>
                  <th scope="col" className="px-4 py-2 font-medium">Actions</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => {
                  const health = healthOf(row.health);
                  const Icon = health.icon;
                  return (
                    <tr key={row.id} className="border-t border-line" data-exporter-row={row.name}>
                      <td className="px-4 py-2">
                        <p className="font-mono text-[12px]">{row.name}</p>
                        <p className="max-w-[22ch] truncate font-mono text-[11px] text-muted">
                          {row.endpoint}
                        </p>
                        {!row.auth_configured ? (
                          <p className="text-[11px] text-caution">no auth secret referenced</p>
                        ) : null}
                        {!row.enabled ? (
                          <p className="text-[11px] text-muted">disabled — not sending</p>
                        ) : null}
                      </td>
                      <td className="px-2 py-2 font-mono text-[11.5px]">{row.kind}</td>
                      <td className="px-2 py-2">
                        <span
                          className={`inline-flex items-center gap-1 rounded-full px-1.5 py-0.5 text-[11px] ${health.tone}`}
                          title={health.meaning}
                          data-exporter-health={row.health}
                        >
                          <Icon className="size-3" aria-hidden="true" />
                          {health.label}
                        </span>
                        {row.last_error ? (
                          <p
                            data-exporter-last-error
                            className="mt-0.5 max-w-[28ch] truncate font-mono text-[11px] text-danger"
                            title={row.last_error}
                          >
                            {row.last_error}
                          </p>
                        ) : null}
                      </td>
                      <td className="px-2 py-2">
                        <BufferBar buffered={row.buffered} capacity={row.capacity} />
                      </td>
                      <td className="px-2 py-2 font-mono text-[11.5px] tabular-nums">
                        {row.dropped_total.toLocaleString("en")}
                      </td>
                      <td className="whitespace-nowrap px-2 py-2 text-muted">
                        {row.last_flush_at ? formatTimestamp(row.last_flush_at) : "never"}
                      </td>
                      <td className="px-4 py-2">
                        <div className="flex gap-1">
                          <button
                            type="button"
                            onClick={() => void test(row)}
                            disabled={testing === row.id}
                            data-exporter-test={row.name}
                            className="flex items-center gap-1 rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft disabled:opacity-60"
                          >
                            {testing === row.id ? (
                              <Loader2 className="size-3 animate-spin" aria-hidden="true" />
                            ) : (
                              <Send className="size-3" aria-hidden="true" />
                            )}
                            Test
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              setFormError(null);
                              setEditing(row);
                            }}
                            data-exporter-edit={row.name}
                            className="rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft"
                          >
                            Edit
                          </button>
                          <button
                            type="button"
                            onClick={() => void remove(row)}
                            disabled={removing === row.id}
                            data-exporter-remove={row.name}
                            className="flex items-center gap-1 rounded-md border border-line px-1.5 py-1 text-[11.5px] text-danger hover:bg-danger-soft disabled:opacity-60"
                          >
                            <Trash2 className="size-3" aria-hidden="true" />
                            Remove
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>

            <ul className="space-y-2 p-3 md:hidden" data-exporter-cards>
              {rows.map((row) => {
                const health = healthOf(row.health);
                const Icon = health.icon;
                return (
                  <li key={row.id} className="rounded-lg border border-line p-3" data-exporter-row={row.name}>
                    <div className="flex items-center gap-2">
                      <p className="font-mono text-[12px]">{row.name}</p>
                      <span
                        className={`ml-auto inline-flex items-center gap-1 rounded-full px-1.5 py-0.5 text-[11px] ${health.tone}`}
                        title={health.meaning}
                      >
                        <Icon className="size-3" aria-hidden="true" />
                        {health.label}
                      </span>
                    </div>
                    <p className="mt-0.5 font-mono text-[11px] break-all text-muted">
                      {row.kind} · {row.endpoint}
                    </p>
                    <div className="mt-2 flex items-center gap-3">
                      <BufferBar buffered={row.buffered} capacity={row.capacity} />
                      <span className="text-[11.5px] text-muted">
                        {row.dropped_total.toLocaleString("en")} dropped
                      </span>
                    </div>
                    {row.last_error ? (
                      <p className="mt-1 font-mono text-[11px] break-all text-danger">
                        {row.last_error}
                      </p>
                    ) : null}
                    <div className="mt-2 flex gap-1">
                      <button
                        type="button"
                        onClick={() => void test(row)}
                        disabled={testing === row.id}
                        data-exporter-test={row.name}
                        className="flex items-center gap-1 rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft disabled:opacity-60"
                      >
                        <Send className="size-3" aria-hidden="true" />
                        Test
                      </button>
                      <button
                        type="button"
                        onClick={() => {
                          setFormError(null);
                          setEditing(row);
                        }}
                        data-exporter-edit={row.name}
                        className="rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        onClick={() => void remove(row)}
                        disabled={removing === row.id}
                        data-exporter-remove={row.name}
                        className="rounded-md border border-line px-1.5 py-1 text-[11.5px] text-danger hover:bg-danger-soft disabled:opacity-60"
                      >
                        <Trash2 className="size-3" aria-hidden="true" />
                        Remove
                      </button>
                    </div>
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </section>

      {testResult ? (
        <section
          data-exporter-test-result
          className={`rounded-xl border p-4 ${
            testResult.ok ? "border-positive-soft bg-positive-soft" : "border-caution-soft bg-caution-soft"
          }`}
        >
          <div className="flex items-start gap-2">
            {testResult.ok ? (
              <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-positive" aria-hidden="true" />
            ) : (
              <AlertTriangle className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden="true" />
            )}
            <div className="min-w-0 flex-1">
              <h3 className="text-[13.5px] font-medium">
                {testResult.ok ? "The backend accepted the probe" : "The backend refused the probe"}
                <span className="ml-2 font-normal text-muted">
                  {testResult.name} · {testResult.kind}
                </span>
              </h3>
              <p className="mt-1 break-words font-mono text-[11.5px]">{testResult.detail}</p>
              <p className="mt-1.5 text-[11.5px] text-muted">
                The probe sent a fixed synthetic document with no telemetry in it, and reported the
                backend&rsquo;s own words. The chip is now{" "}
                <span className="font-mono">{testResult.health}</span> — one refused batch is
                degraded, two is down.
              </p>
            </div>
            <button
              type="button"
              onClick={() => setTestResult(null)}
              data-exporter-test-dismiss
              className="rounded-md border border-line bg-surface px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft"
            >
              <X className="size-3" aria-hidden="true" />
              <span className="sr-only">Dismiss the probe result</span>
            </button>
          </div>
        </section>
      ) : null}

      <p className="text-[11.5px] text-muted">
        Press <kbd className="rounded border border-line px-1">/</kbd> to filter,{" "}
        <kbd className="rounded border border-line px-1">n</kbd> to add,{" "}
        <kbd className="rounded border border-line px-1">r</kbd> to refresh. A dropped sample is
        lost on purpose: when a backend is behind, the oldest telemetry is the least useful, and
        the counter above is the honest record of what that cost.
      </p>

      {openForm ? (
        <ExporterForm
          initial={editing ? toForm(editing) : BLANK}
          kinds={data?.kinds ?? ["otlp", "prometheus_remote_write", "syslog", "webhook"]}
          saving={saving}
          error={formError}
          onSave={(form) => void save(form)}
          onClose={() => {
            setCreating(false);
            setEditing(null);
            setFormError(null);
          }}
        />
      ) : null}
    </div>
  );
}

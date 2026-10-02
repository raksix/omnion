"use client";

/**
 * `/ai/local` — the endpoints this installation talks to (REQ-106, slice 1).
 *
 * The screen answers one question: **is anything still leaving this machine?** It therefore lists
 * every provider with its verified locality rather than only the local ones. A list containing
 * just the local endpoints could not answer the question the screen exists for, and the air-gap
 * switch reads exactly that answer.
 *
 * Four decisions shape it, and each one exists because the naive version is actively misleading:
 *
 * 1. **"Local" is a verified claim, not a toggle.** Registering a public host is refused with the
 *    host named, so the form says *why* next to the field. The badge reads `host_kind` — the rule
 *    that fired — because "the platform proves it by name" and "an operator vouched for it" are
 *    different things for the same word "local".
 *
 * 2. **A never-probed endpoint is not a healthy one.** `last_seen_at === null` renders as
 *    "not checked yet", in a neutral tone. A green badge on a row nobody has ever measured is
 *    the one thing this screen must never draw.
 *
 * 3. **A failed load is not an empty list.** "No endpoints" sends an operator to stop looking, so
 *    a failure keeps its own wording and its own Retry button.
 *
 * 4. **Registering does not check reachability.** The screen says so under the form instead of
 *    implying a saved row is a working server: the endpoint gets its answer from a scan, and a
 *    deliberate action with a visible outcome — never as a side effect of typing a URL.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter } from "next/navigation";

import { Cpu, Loader2, Plus, RefreshCw, Server, ShieldCheck, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  createLocalEndpoint,
  fetchLocalEndpoints,
  scanLocalEndpoint,
  type LocalEndpoint,
  type LocalEndpointList,
} from "@/lib/local-api";
import { formatTimestamp } from "@/lib/format";

/**
 * How a `host_kind` renders.
 *
 * Each entry pairs a *word* with its tone, because the request asks for badges that are
 * distinguishable without colour alone: `loopback`, `Private range`, `Allow-listed`,
 * `Unverified` and `Remote` are five different words, and the colour is only the second signal.
 */
function localityBadge(endpoint: LocalEndpoint): { label: string; tone: string } {
  if (endpoint.locality !== "local") {
    return { label: "Remote", tone: "bg-caution-soft text-caution" };
  }
  switch (endpoint.host_kind) {
    case "loopback":
      return { label: "Loopback", tone: "bg-positive-soft text-positive" };
    case "private":
      return { label: "Private range", tone: "bg-positive-soft text-positive" };
    case "allowlisted":
      return { label: "Allow-listed", tone: "bg-positive-soft text-positive" };
    default:
      // A `local` row with no rule behind it. Shown as its own state rather than folded into
      // "local", because the air-gap check and this screen must not read it as verified.
      return { label: "Unverified", tone: "bg-quiet-soft text-muted" };
  }
}

/** `openai_compatible` → `OpenAI compatible`. The wire value stays visible in the row. */
function protocolLabel(value: string): string {
  return value.replace(/_/g, " ").replace(/^./, (char) => char.toUpperCase());
}

export function AiLocalView() {
  const router = useRouter();
  const [data, setData] = useState<LocalEndpointList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [formOpen, setFormOpen] = useState(false);
  const [name, setName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [scanState, setScanState] = useState<Record<string, string>>({});

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    fetchLocalEndpoints()
      .then(setData)
      .catch((cause: unknown) => {
        setData(null);
        setError(
          cause instanceof ApiError ? cause.message : "The endpoints could not be loaded.",
        );
      })
      .finally(() => setBusy(false));
  }, []);

  useEffect(load, [load]);

  const endpoints = useMemo(() => data?.endpoints ?? [], [data]);
  const hasAny = (data?.local_count ?? 0) + (data?.remote_count ?? 0) > 0;

  /**
   * A scan reports what the server said, next to the row it was asked about.
   *
   * It is stored as a per-endpoint message rather than a boolean so a 502 ("your Ollama is
   * down") and a 422 ("it answers something this platform cannot read") can be told apart — the
   * two send an operator to completely different places.
   */
  const scan = useCallback(async (id: string) => {
    setScanState((state) => ({ ...state, [id]: "" }));
    try {
      const answer = await scanLocalEndpoint(id);
      setScanState((state) => ({
        ...state,
        [id]: `Recorded ${answer.written} of ${answer.served} served models.`,
      }));
      load();
    } catch (cause: unknown) {
      setScanState((state) => ({
        ...state,
        [id]: cause instanceof ApiError ? cause.message : "The scan failed.",
      }));
    }
  }, [load]);

  const submit = useCallback(async () => {
    setSaving(true);
    setFormError(null);
    try {
      await createLocalEndpoint({ name: name.trim(), base_url: baseUrl.trim(), api_key: apiKey || null });
      setName("");
      setBaseUrl("");
      setApiKey("");
      setFormOpen(false);
      load();
      router.refresh();
    } catch (cause: unknown) {
      setFormError(
        cause instanceof ApiError ? cause.message : "The endpoint could not be registered.",
      );
    } finally {
      setSaving(false);
    }
  }, [name, baseUrl, apiKey, load, router]);

  if (error) {
    return (
      <div data-local-error className="flex flex-col gap-3">
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={load}
          className="self-start rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!data) return <LoadingTable columns={5} rows={3} />;

  return (
    <div data-local-endpoints className="flex flex-col gap-5">
      <div className="grid gap-3 sm:grid-cols-3">
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Verified local</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.local_count}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Remote (traffic can leave this host)</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.remote_count}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Models served locally</p>
          <p className="mt-1 text-2xl font-semibold text-ink">
            {endpoints.reduce((total, endpoint) => total + endpoint.available_count, 0)}
          </p>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12px] text-muted">
          A local endpoint is verified against the host, never taken on trust — a public address
          is refused with the host named.
        </p>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={load}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            {busy ? <Loader2 aria-hidden size={14} className="animate-spin" /> : <RefreshCw aria-hidden size={14} />}
            Refresh
          </button>
          <button
            type="button"
            onClick={() => setFormOpen((open) => !open)}
            aria-expanded={formOpen}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong"
          >
            {formOpen ? <X aria-hidden size={14} /> : <Plus aria-hidden size={14} />}
            Register endpoint
          </button>
        </div>
      </div>

      {formOpen ? (
        <form
          data-local-endpoint-form
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
        >
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Name
              <input
                value={name}
                onChange={(event) => setName(event.target.value)}
                required
                placeholder="Workstation Ollama"
                aria-label="Endpoint name"
                className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Base URL
              <input
                value={baseUrl}
                onChange={(event) => setBaseUrl(event.target.value)}
                required
                placeholder="http://127.0.0.1:11434/v1"
                aria-label="Base URL"
                className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
              />
            </label>
          </div>
          <label className="flex flex-col gap-1 text-[12px] text-muted">
            API key <span className="text-muted">(optional — llama.cpp usually needs none)</span>
            <input
              type="password"
              value={apiKey}
              onChange={(event) => setApiKey(event.target.value)}
              aria-label="API key"
              autoComplete="off"
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
            />
          </label>
          <p className="text-[12px] text-muted">
            Registering records the endpoint and checks its host; it does not contact the server.
            Use <strong>Scan</strong> on the row to ask what it serves.
          </p>
          {formError ? (
            <p data-local-endpoint-form-error className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger">
              {formError}
            </p>
          ) : null}
          <div className="flex items-center gap-2">
            <button
              type="submit"
              disabled={saving || !name.trim() || !baseUrl.trim()}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-50"
            >
              {saving ? <Loader2 aria-hidden size={14} className="animate-spin" /> : <Server aria-hidden size={14} />}
              Register
            </button>
            <button
              type="button"
              onClick={() => {
                setFormOpen(false);
                setFormError(null);
              }}
              className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}

      {data.is_empty || !hasAny ? (
        <EmptyState
          title="No endpoints registered"
          hint="Register an Ollama, vLLM or llama.cpp server to run inference on this machine. The host is checked against loopback, private ranges and the internal allow-list."
          action={
            <button
              type="button"
              onClick={() => setFormOpen(true)}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
            >
              <Plus aria-hidden size={14} />
              Register endpoint
            </button>
          }
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {endpoints.map((endpoint) => {
            const badge = localityBadge(endpoint);
            return (
              <li
                key={endpoint.id}
                data-local-endpoint-row
                className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <Server aria-hidden size={15} className="text-muted" />
                  <span className="text-[14px] font-medium text-ink">{endpoint.name}</span>
                  <span
                    data-local-badge
                    className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ${badge.tone}`}
                  >
                    {endpoint.locality === "local" ? <ShieldCheck aria-hidden size={11} /> : null}
                    {badge.label}
                  </span>
                  {!endpoint.enabled ? (
                    <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] font-medium text-muted">
                      Disabled
                    </span>
                  ) : null}
                </div>
                <p className="truncate text-[12px] text-muted">
                  {endpoint.base_url} · {protocolLabel(endpoint.protocol)}
                </p>
                <p className="text-[12px] text-muted">
                  {endpoint.model_count} model{endpoint.model_count === 1 ? "" : "s"} registered ·{" "}
                  {endpoint.available_count} available
                </p>
                <p className="text-[11px] text-muted">
                  {endpoint.last_seen_at
                    ? `Last reached ${formatTimestamp(endpoint.last_seen_at)}`
                    : "Not checked yet — scan the endpoint to see what it serves"}
                </p>
                {scanState[endpoint.id] ? (
                  <p
                    data-local-scan-result
                    className="rounded-lg bg-quiet-soft px-3 py-2 text-[12px] text-muted"
                  >
                    {scanState[endpoint.id]}
                  </p>
                ) : null}
                <div className="flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    onClick={() => void scan(endpoint.id)}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                  >
                    <Cpu aria-hidden size={14} />
                    Scan
                  </button>
                  <a
                    href={`/ai/local/models?endpoint=${encodeURIComponent(endpoint.id)}`}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                  >
                    Models
                  </a>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

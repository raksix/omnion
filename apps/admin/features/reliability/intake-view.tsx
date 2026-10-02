"use client";

/**
 * `/settings/reliability/intake` — the inbound guard (REQ-127, slice 4).
 *
 * This is the screen an integrator opens after their provider says `401` and the platform says
 * the signature is invalid. It has one job above all others: **never let an operator conclude
 * that the wrong thing.** Five readings it refuses to blur:
 *
 * - **`has_secret` is a reference, not a value.** There is no button anywhere on this screen
 *   that reveals a signing key, and there cannot be one, because the API has no route that
 *   would answer it. A tester that showed the key would be the exfiltration path for every
 *   future bug in this area.
 * - **The `Verify sample` result is the platform's verdict, not the browser's.** The request
 *   goes to the server and runs the same function the request path runs. A local HMAC check
 *   would need the secret in the browser and would disagree with the platform the first time
 *   somebody changed a scheme.
 * - **A refusal names a reason, and the reason is the wire code.** `401 signature_invalid` and
 *   `400 timestamp_stale` are different problems with different fixes; showing both as "invalid
 *   signature" is how an operator spends a day checking a key that was always right.
 * - **An endpoint with no secret is a configuration gap, not a failed verification.** It says
 *   so, because blaming the operator's signature for a missing key is the most expensive
 *   misreading this screen can produce.
 * - **Refusal counts come from the same call as the log.** A chip that says 12 while the table
 *   beneath it shows 3 sends the operator to the wrong filter.
 *
 * Keyboard: `/` filters, `n` declares, `v` opens the tester for the selected endpoint, `Esc`
 * closes a dialog. Under `sm:` the table becomes cards and the dialogs stay reachable.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  Fingerprint,
  Inbox,
  Loader2,
  Pencil,
  Plus,
  Save,
  Search,
  ShieldQuestion,
  Trash2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createReliabilityIntakeEndpoint,
  deleteReliabilityIntakeEndpoint,
  fetchReliabilityIntake,
  fetchReliabilityIntakeRejections,
  updateReliabilityIntakeEndpoint,
  verifyReliabilityIntakeSample,
  type ReliabilityIntake,
  type ReliabilityIntakeEndpoint,
  type ReliabilityIntakeRejection,
  type ReliabilityIntakeSample,
} from "@/lib/api";

/** The reason chip: icon + text, so the reason is legible without colour. */
function reasonChip(reason: string): { label: string; tone: string } {
  switch (reason) {
    case "payload_too_large":
      return { label: "Payload too large", tone: "border-amber-300 text-amber-800" };
    case "replay":
      return { label: "Replay", tone: "border-violet-300 text-violet-800" };
    case "timestamp_stale":
      return { label: "Stale timestamp", tone: "border-sky-300 text-sky-800" };
    case "content_type_refused":
      return { label: "Content type refused", tone: "border-sky-300 text-sky-800" };
    case "malformed":
      return { label: "Misconfigured", tone: "border-rose-300 text-rose-800" };
    case "signature_missing":
    case "signature_invalid":
    default:
      return { label: "Signature refused", tone: "border-rose-300 text-rose-800" };
  }
}

const BLANK = {
  path: "",
  name: "",
  hmac_scheme: "sha256_hex",
  signature_header: "x-omnion-signature",
  timestamp_header: "x-omnion-timestamp",
  tolerance_seconds: 300,
  secret_id: "",
  max_payload_bytes: 1048576,
  sanitize_profile: "strict",
  enabled: true,
};

type Draft = typeof BLANK;

export function ReliabilityIntakeView() {
  const [data, setData] = useState<ReliabilityIntake | null>(null);
  const [rejections, setRejections] = useState<ReliabilityIntakeRejection[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [reasonFilter, setReasonFilter] = useState("");
  const [editing, setEditing] = useState<ReliabilityIntakeEndpoint | null>(null);
  const [draft, setDraft] = useState<Draft>({ ...BLANK });
  const [formError, setFormError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<ReliabilityIntakeEndpoint | null>(null);
  const [reason, setReason] = useState("");
  const [tester, setTester] = useState<ReliabilityIntakeEndpoint | null>(null);
  const [sample, setSample] = useState({ payload: "", signature: "" });
  const [verdict, setVerdict] = useState<ReliabilityIntakeSample | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchReliabilityIntake());
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setLoading(false);
    }
  }, []);

  const loadRejections = useCallback(async (reason: string) => {
    try {
      const result = await fetchReliabilityIntakeRejections(reason ? { reason } : {});
      setRejections(result.rejections);
    } catch {
      // The log failing is not the screen failing: the declarations above are still the
      // actionable half, and a blank log next to a red banner sends the operator looking for
      // a database problem that is really a filter.
      setRejections([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    void loadRejections(reasonFilter);
  }, [loadRejections, reasonFilter]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (event.key === "Escape") {
        setEditing(null);
        setRemoving(null);
        setTester(null);
        setVerdict(null);
        return;
      }
      if (typing) return;
      if (event.key === "n") {
        event.preventDefault();
        setEditing({} as ReliabilityIntakeEndpoint);
        setDraft({ ...BLANK });
        setFormError(null);
      }
      if (event.key === "v" && data && data.endpoints.length > 0) {
        event.preventDefault();
        setTester(data.endpoints[0]);
        setSample({ payload: "", signature: "" });
        setVerdict(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [data]);

  const visible = useMemo(() => {
    if (!data) return [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return data.endpoints;
    return data.endpoints.filter((e) =>
      [e.path, e.name, e.hmac_scheme, e.signature_header]
        .join(" ")
        .toLowerCase()
        .includes(needle),
    );
  }, [data, filter]);

  const openEditor = (endpoint: ReliabilityIntakeEndpoint | null) => {
    if (!endpoint || !endpoint.id) {
      setEditing({} as ReliabilityIntakeEndpoint);
      setDraft({ ...BLANK });
      setFormError(null);
      return;
    }
    setEditing(endpoint);
    setFormError(null);
    setDraft({
      path: endpoint.path,
      name: endpoint.name,
      hmac_scheme: endpoint.hmac_scheme,
      signature_header: endpoint.signature_header,
      timestamp_header: endpoint.timestamp_header ?? "",
      tolerance_seconds: endpoint.tolerance_seconds,
      secret_id: endpoint.secret_id ?? "",
      max_payload_bytes: endpoint.max_payload_bytes,
      sanitize_profile: endpoint.sanitize_profile,
      enabled: endpoint.enabled,
    });
  };

  const save = async () => {
    setBusy(true);
    setFormError(null);
    // The empty string is `null` and not `""`: a header name of "" is not a header, and the
    // route's Option<String> cannot tell an absent one from an empty one — so the form does.
    const input = {
      path: draft.path.trim(),
      name: draft.name.trim(),
      hmac_scheme: draft.hmac_scheme,
      signature_header: draft.signature_header.trim(),
      timestamp_header: draft.timestamp_header.trim() || null,
      tolerance_seconds: Number(draft.tolerance_seconds),
      secret_id: draft.secret_id.trim() || null,
      max_payload_bytes: Number(draft.max_payload_bytes),
      sanitize_profile: draft.sanitize_profile,
      enabled: draft.enabled,
    };
    try {
      if (editing && editing.id) {
        await updateReliabilityIntakeEndpoint(editing.id, input);
        setNotice(`Saved ${input.path}`);
      } else {
        await createReliabilityIntakeEndpoint(input);
        setNotice(`Declared ${input.path}`);
      }
      setEditing(null);
      await load();
    } catch (caught) {
      setFormError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!removing) return;
    if (reason.trim().length === 0) {
      setFormError("A reason is required — this turns a guarded door into an open one.");
      return;
    }
    setBusy(true);
    setFormError(null);
    try {
      await deleteReliabilityIntakeEndpoint(removing.id, reason.trim());
      setNotice(`Removed ${removing.path}`);
      setRemoving(null);
      setReason("");
      await load();
      await loadRejections(reasonFilter);
    } catch (caught) {
      setFormError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  const runSample = async () => {
    if (!tester) return;
    setBusy(true);
    setVerdict(null);
    try {
      setVerdict(await verifyReliabilityIntakeSample(tester.id, sample));
    } catch (caught) {
      setVerdict({
        valid: false,
        reason: null,
        detail: caught instanceof ApiError ? caught.message : String(caught),
        changes: null,
        body_bytes: sample.payload.length,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-5" data-view="reliability-intake">
      {notice ? (
        <div
          role="status"
          className="flex items-center gap-2 rounded-md border border-emerald-300 bg-emerald-50 px-3 py-2 text-[13px] text-emerald-900"
        >
          <CheckCircle2 className="h-4 w-4" aria-hidden="true" />
          {notice}
          <button
            type="button"
            className="ml-auto"
            aria-label="Dismiss"
            onClick={() => setNotice(null)}
          >
            <X className="h-3.5 w-3.5" aria-hidden="true" />
          </button>
        </div>
      ) : null}

      {error ? (
        <div
          role="alert"
          className="flex items-center gap-2 rounded-md border border-rose-300 bg-rose-50 px-3 py-2 text-[13px] text-rose-900"
        >
          <AlertTriangle className="h-4 w-4" aria-hidden="true" />
          {error}
          <button type="button" className="ml-auto" onClick={() => void load()}>
            Retry
          </button>
        </div>
      ) : null}

      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <div className="min-w-0">
            <h2 className="text-[14px] font-medium">Inbound endpoints</h2>
            <p className="text-[12px] text-muted">
              Each declared path is authenticated before anything reads it. The signing key stays
              in the secret store and is never shown here.
            </p>
          </div>
          <div className="ml-auto flex items-center gap-2">
            <label className="sr-only" htmlFor="intake-filter">
              Filter endpoints
            </label>
            <div className="flex items-center gap-1.5 rounded-md border border-line px-2 py-1">
              <Search className="h-3.5 w-3.5 text-muted" aria-hidden="true" />
              <input
                id="intake-filter"
                ref={searchRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder="Filter by path or name  /"
                className="w-56 bg-transparent text-[13px] outline-none"
              />
            </div>
            <button
              type="button"
              onClick={() => openEditor(null)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1 text-[12.5px]"
            >
              <Plus className="h-3.5 w-3.5" aria-hidden="true" />
              Declare
            </button>
          </div>
        </header>

        {data && data.reason_counts.length > 0 ? (
          <div className="flex flex-wrap gap-2 border-b border-line px-4 py-2.5">
            {data.reason_counts.map(([reason, count]) => {
              const chip = reasonChip(reason);
              const active = reasonFilter === reason;
              return (
                <button
                  key={reason}
                  type="button"
                  onClick={() => setReasonFilter(active ? "" : reason)}
                  aria-pressed={active}
                  className={`inline-flex items-center gap-1.5 rounded border px-2 py-0.5 text-[12px] ${chip.tone} ${active ? "ring-2 ring-current" : ""}`}
                >
                  {chip.label}
                  <span className="tabular-nums">{count}</span>
                </button>
              );
            })}
          </div>
        ) : null}

        {loading ? (
          <LoadingTable columns={5} rows={3} />
        ) : !data || data.endpoints.length === 0 ? (
          <EmptyState
            title="No inbound endpoint is declared"
            hint="Declare the path a provider posts to, point it at a secret, and the guard authenticates every request on it before your code sees it."
            action={
              <button
                type="button"
                onClick={() => openEditor(null)}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden="true" />
                Declare an endpoint
              </button>
            }
          />
        ) : visible.length === 0 ? (
          <EmptyState
            title="Nothing matches that filter"
            hint={`No declared path or name contains "${filter}".`}
          />
        ) : (
          <>
            <div className="hidden sm:block">
              <table className="w-full text-left text-[13px]">
                <thead className="text-[12px] text-muted">
                  <tr>
                    <th className="px-4 py-2 font-medium">Path</th>
                    <th className="px-4 py-2 font-medium">Scheme &amp; header</th>
                    <th className="px-4 py-2 font-medium">Limits</th>
                    <th className="px-4 py-2 font-medium">Refusals</th>
                    <th className="px-4 py-2 text-right font-medium">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {visible.map((endpoint) => (
                    <tr key={endpoint.id} className="border-t border-line" data-intake={endpoint.path}>
                      <td className="px-4 py-3">
                        <span className="font-medium">{endpoint.name}</span>
                        <span className="block text-[12px] text-muted">{endpoint.path}</span>
                        {!endpoint.enabled ? (
                          <span className="mt-1 inline-flex items-center gap-1 rounded border border-amber-300 px-1.5 py-0.5 text-[11.5px] text-amber-800">
                            <AlertTriangle className="h-3 w-3" aria-hidden="true" />
                            Declared but not enabled
                          </span>
                        ) : null}
                        {!endpoint.has_secret ? (
                          <span className="mt-1 block text-[11.5px] text-rose-700">
                            No secret reference — nothing on this path can be authenticated.
                          </span>
                        ) : null}
                      </td>
                      <td className="px-4 py-3">
                        <span className="font-mono text-[12.5px]">{endpoint.hmac_scheme}</span>
                        <span className="block text-[12px] text-muted">
                          {endpoint.signature_header}
                          {endpoint.timestamp_header
                            ? ` · ${endpoint.timestamp_header}`
                            : " · no timestamp header"}
                        </span>
                      </td>
                      <td className="px-4 py-3 tabular-nums text-muted">
                        ±{endpoint.tolerance_seconds}s window
                        <span className="block">
                          {(endpoint.max_payload_bytes / 1024).toFixed(0)} KB cap ·{" "}
                          {endpoint.sanitize_profile}
                        </span>
                      </td>
                      <td className="px-4 py-3 tabular-nums">
                        {endpoint.rejection_count}
                        {endpoint.last_rejection_at ? (
                          <span className="block text-[12px] text-muted">
                            last {new Date(endpoint.last_rejection_at).toLocaleString()}
                          </span>
                        ) : null}
                      </td>
                      <td className="px-4 py-3 text-right">
                        <span className="inline-flex gap-1.5">
                          <button
                            type="button"
                            onClick={() => {
                              setTester(endpoint);
                              setSample({ payload: "", signature: "" });
                              setVerdict(null);
                            }}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                          >
                            <ShieldQuestion className="h-3.5 w-3.5" aria-hidden="true" />
                            Verify sample
                          </button>
                          <button
                            type="button"
                            onClick={() => openEditor(endpoint)}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                          >
                            <Pencil className="h-3.5 w-3.5" aria-hidden="true" />
                            Edit
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              setRemoving(endpoint);
                              setReason("");
                              setFormError(null);
                            }}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                          >
                            <Trash2 className="h-3.5 w-3.5" aria-hidden="true" />
                            Remove
                          </button>
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            {/* The same rows as cards. The table is `hidden sm:block` because a five-column
                table at 390px is not a table, it is a horizontal scroll — and a guard screen
                read on a phone during an incident is exactly the one that must not scroll. */}
            <ul className="divide-y divide-line sm:hidden">
              {visible.map((endpoint) => (
                <li key={endpoint.id} className="flex flex-col gap-2 px-4 py-3" data-intake-card={endpoint.path}>
                  <div className="flex items-baseline gap-2">
                    <span className="font-medium">{endpoint.name}</span>
                    {!endpoint.enabled ? (
                      <span className="rounded border border-amber-300 px-1.5 py-0.5 text-[11.5px] text-amber-800">
                        not enabled
                      </span>
                    ) : null}
                  </div>
                  <span className="break-all font-mono text-[12px] text-muted">{endpoint.path}</span>
                  <span className="text-[12px] text-muted">
                    {endpoint.hmac_scheme} · {endpoint.signature_header} · ±
                    {endpoint.tolerance_seconds}s · {(endpoint.max_payload_bytes / 1024).toFixed(0)} KB
                    · {endpoint.sanitize_profile}
                  </span>
                  <span className="text-[12px] text-muted">{endpoint.rejection_count} refusals</span>
                  <div className="flex flex-wrap gap-1.5">
                    <button
                      type="button"
                      onClick={() => {
                        setTester(endpoint);
                        setSample({ payload: "", signature: "" });
                        setVerdict(null);
                      }}
                      className="rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                    >
                      Verify sample
                    </button>
                    <button
                      type="button"
                      onClick={() => openEditor(endpoint)}
                      className="rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      onClick={() => {
                        setRemoving(endpoint);
                        setReason("");
                        setFormError(null);
                      }}
                      className="rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                    >
                      Remove
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <div>
            <h2 className="text-[14px] font-medium">Rejection log</h2>
            <p className="text-[12px] text-muted">
              Time · endpoint · reason · source · request id. The size is a count, never the
              payload — this log does not keep a second copy of what it refused.
            </p>
          </div>
          {reasonFilter ? (
            <button
              type="button"
              onClick={() => setReasonFilter("")}
              className="ml-auto inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1 text-[12.5px]"
            >
              <X className="h-3.5 w-3.5" aria-hidden="true" />
              Clear “{reasonFilter}”
            </button>
          ) : null}
        </header>
        {rejections.length === 0 ? (
          <EmptyState
            title={reasonFilter ? `No refusals for ${reasonFilter}` : "Nothing has been refused"}
            hint={
              reasonFilter
                ? "The filter above is active; clear it to see every refusal."
                : "This log fills when a provider sends something the guard will not accept. An empty log on a busy instance is worth a second look."
            }
          />
        ) : (
          <ul className="divide-y divide-line">
            {rejections.map((row) => {
              const chip = reasonChip(row.reason);
              return (
                <li
                  key={row.id}
                  className="flex flex-wrap items-baseline gap-2 px-4 py-2.5 text-[13px]"
                  data-rejection={row.reason}
                >
                  <span
                    className={`inline-flex items-center gap-1.5 rounded border px-2 py-0.5 text-[12px] ${chip.tone}`}
                  >
                    {chip.label}
                  </span>
                  <span className="break-all font-mono text-[12px] text-muted">
                    {row.path ?? "(endpoint removed)"}
                  </span>
                  <span className="text-muted">{row.source_ip ?? "no source address"}</span>
                  <span className="text-[12px] text-muted">{row.body_bytes} bytes</span>
                  {row.request_id ? (
                    <span className="font-mono text-[11.5px] text-muted">{row.request_id}</span>
                  ) : null}
                  <span className="ml-auto text-[12px] text-muted">
                    {new Date(row.created_at).toLocaleString()}
                  </span>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      {editing ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label={editing.id ? "Edit intake endpoint" : "Declare intake endpoint"}
          className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
        >
          <div className="mt-10 w-full max-w-2xl rounded-lg border border-line bg-background p-5">
            <div className="mb-4 flex items-center gap-2">
              <Fingerprint className="h-4 w-4" aria-hidden="true" />
              <h2 className="text-[15px] font-medium">
                {editing.id ? "Edit intake endpoint" : "Declare an intake endpoint"}
              </h2>
              <button
                type="button"
                className="ml-auto"
                aria-label="Close"
                onClick={() => setEditing(null)}
              >
                <X className="h-4 w-4" aria-hidden="true" />
              </button>
            </div>

            <div className="grid gap-3 sm:grid-cols-2">
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Path</span>
                <input
                  value={draft.path}
                  onChange={(e) => setDraft({ ...draft, path: e.target.value })}
                  placeholder="/api/v1/public/intake/&lt;uuid&gt;"
                  className="rounded-md border border-line px-2 py-1.5 font-mono text-[13px]"
                />
                <span className="text-[11.5px] text-muted">
                  Exact. A declaration never guards a path it does not name.
                </span>
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Name</span>
                <input
                  value={draft.name}
                  onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                  placeholder="Stripe invoices"
                  className="rounded-md border border-line px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">HMAC scheme</span>
                <select
                  value={draft.hmac_scheme}
                  onChange={(e) => setDraft({ ...draft, hmac_scheme: e.target.value })}
                  className="rounded-md border border-line px-2 py-1.5 text-[13px]"
                >
                  {(data?.schemes ?? [draft.hmac_scheme]).map((scheme) => (
                    <option key={scheme} value={scheme}>
                      {scheme}
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Sanitisation profile</span>
                <select
                  value={draft.sanitize_profile}
                  onChange={(e) => setDraft({ ...draft, sanitize_profile: e.target.value })}
                  className="rounded-md border border-line px-2 py-1.5 text-[13px]"
                >
                  {(data?.profiles ?? [draft.sanitize_profile]).map((profile) => (
                    <option key={profile} value={profile}>
                      {profile}
                    </option>
                  ))}
                </select>
                <span className="text-[11.5px] text-muted">
                  Both profiles leave a legitimate JSON payload byte-identical; `strict` also
                  strips `\x00` escapes, which are almost never real data.
                </span>
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Signature header</span>
                <input
                  value={draft.signature_header}
                  onChange={(e) => setDraft({ ...draft, signature_header: e.target.value })}
                  className="rounded-md border border-line px-2 py-1.5 font-mono text-[13px]"
                />
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Timestamp header</span>
                <input
                  value={draft.timestamp_header}
                  onChange={(e) => setDraft({ ...draft, timestamp_header: e.target.value })}
                  placeholder="leave empty if the scheme has none"
                  className="rounded-md border border-line px-2 py-1.5 font-mono text-[13px]"
                />
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Tolerance (seconds)</span>
                <input
                  type="number"
                  min={30}
                  max={3600}
                  value={draft.tolerance_seconds}
                  onChange={(e) => setDraft({ ...draft, tolerance_seconds: Number(e.target.value) })}
                  className="rounded-md border border-line px-2 py-1.5 text-[13px]"
                />
                <span className="text-[11.5px] text-muted">
                  30–3600. A stale request is refused, which is what makes the replay window
                  meaningful.
                </span>
              </label>
              <label className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">Max payload (bytes)</span>
                <input
                  type="number"
                  min={1024}
                  max={10485760}
                  step={1024}
                  value={draft.max_payload_bytes}
                  onChange={(e) => setDraft({ ...draft, max_payload_bytes: Number(e.target.value) })}
                  className="rounded-md border border-line px-2 py-1.5 text-[13px]"
                />
                <span className="text-[11.5px] text-muted">
                  1 KB–10 MB. Refused with `413 payload_too_large` before the signature is even
                  hashed.
                </span>
              </label>
              <label className="flex flex-col gap-1 text-[12.5px] sm:col-span-2">
                <span className="font-medium">Secret id</span>
                <input
                  value={draft.secret_id}
                  onChange={(e) => setDraft({ ...draft, secret_id: e.target.value })}
                  placeholder="uuid of a secret in the secret store"
                  className="rounded-md border border-line px-2 py-1.5 font-mono text-[13px]"
                />
                <span className="text-[11.5px] text-muted">
                  A reference. The value never travels to this screen, and an endpoint with none
                  cannot authenticate anything.
                </span>
              </label>
              <label className="flex items-center gap-2 text-[13px] sm:col-span-2">
                <input
                  type="checkbox"
                  checked={draft.enabled}
                  onChange={(e) => setDraft({ ...draft, enabled: e.target.checked })}
                />
                Enabled — a disabled declaration is served as a misconfiguration, not as a guard.
              </label>
            </div>

            {formError ? (
              <p role="alert" className="mt-3 text-[12.5px] text-rose-700">
                {formError}
              </p>
            ) : null}

            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setEditing(null)}
                className="rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void save()}
                disabled={busy}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                {busy ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <Save className="h-3.5 w-3.5" aria-hidden="true" />
                )}
                {editing.id ? "Save" : "Declare"}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {tester ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Verify a signature sample"
          className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
        >
          <div className="mt-10 w-full max-w-2xl rounded-lg border border-line bg-background p-5">
            <div className="mb-1 flex items-center gap-2">
              <ShieldQuestion className="h-4 w-4" aria-hidden="true" />
              <h2 className="text-[15px] font-medium">Verify a signature sample</h2>
              <button type="button" className="ml-auto" aria-label="Close" onClick={() => setTester(null)}>
                <X className="h-4 w-4" aria-hidden="true" />
              </button>
            </div>
            <p className="mb-4 text-[12.5px] text-muted">
              {tester.path} · {tester.hmac_scheme}. This runs the platform's own guard — the same
              function the request path runs — and answers with a reason, never the key.
            </p>

            <label className="flex flex-col gap-1 text-[12.5px]">
              <span className="font-medium">Payload the provider sent</span>
              <textarea
                value={sample.payload}
                onChange={(e) => setSample({ ...sample, payload: e.target.value })}
                rows={6}
                className="rounded-md border border-line px-2 py-1.5 font-mono text-[12.5px]"
              />
            </label>
            <label className="mt-3 flex flex-col gap-1 text-[12.5px]">
              <span className="font-medium">Signature header value</span>
              <input
                value={sample.signature}
                onChange={(e) => setSample({ ...sample, signature: e.target.value })}
                placeholder="v1,&lt;id&gt;:&lt;hex tag&gt;"
                className="rounded-md border border-line px-2 py-1.5 font-mono text-[12.5px]"
              />
            </label>

            {verdict ? (
              <div
                role="status"
                data-verdict={verdict.valid ? "valid" : "invalid"}
                className={`mt-4 flex items-start gap-2 rounded-md border px-3 py-2 text-[13px] ${
                  verdict.valid
                    ? "border-emerald-300 bg-emerald-50 text-emerald-900"
                    : "border-rose-300 bg-rose-50 text-rose-900"
                }`}
              >
                {verdict.valid ? (
                  <CheckCircle2 className="mt-0.5 h-4 w-4" aria-hidden="true" />
                ) : (
                  <AlertTriangle className="mt-0.5 h-4 w-4" aria-hidden="true" />
                )}
                <span>
                  <strong>{verdict.valid ? "Signature valid" : `Refused: ${verdict.reason}`}</strong>
                  <span className="block">{verdict.detail}</span>
                  {verdict.changes && verdict.changes.length > 0 ? (
                    <span className="block text-[12px]">
                      Sanitisation would {verdict.changes.join(", ")}.
                    </span>
                  ) : verdict.changes ? (
                    <span className="block text-[12px]">
                      Sanitisation leaves this payload byte-identical.
                    </span>
                  ) : null}
                </span>
              </div>
            ) : null}

            <div className="mt-4 flex justify-end">
              <button
                type="button"
                onClick={() => void runSample()}
                disabled={busy || sample.payload.length === 0 || sample.signature.length === 0}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                {busy ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <Fingerprint className="h-3.5 w-3.5" aria-hidden="true" />
                )}
                Verify
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {removing ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Remove intake endpoint"
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-background p-5">
            <div className="flex items-center gap-2">
              <Inbox className="h-4 w-4" aria-hidden="true" />
              <h2 className="text-[15px] font-medium">Remove {removing.path}</h2>
            </div>
            <p className="mt-2 text-[13px] text-muted">
              This removes the guard and its rejection history. It is written to the audit log with
              the reason below.
            </p>
            <label className="mt-3 flex flex-col gap-1 text-[12.5px]">
              <span className="font-medium">Reason</span>
              <input
                value={reason}
                onChange={(e) => setReason(e.target.value)}
                placeholder="provider migrated to a new endpoint"
                className="rounded-md border border-line px-2 py-1.5 text-[13px]"
              />
            </label>
            {formError ? (
              <p role="alert" className="mt-2 text-[12.5px] text-rose-700">
                {formError}
              </p>
            ) : null}
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setRemoving(null)}
                className="rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void remove()}
                disabled={busy}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" /> : null}
                Remove
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

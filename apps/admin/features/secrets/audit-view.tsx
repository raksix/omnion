"use client";

/**
 * `/secrets/audit` — who touched which secret, when, from where, and which flags the detectors
 * raised (docs/requests/REQ-125, slice 4).
 *
 * The screen is built around three things an operator cannot answer any other way, and each one
 * shapes a part of the layout:
 *
 * 1. **The request id is the join key.** A refusal arrives with a request id in its banner. If the
 *    operator could not type that id into a filter and land on the row that explains it, the id
 *    would be decoration — so the id column is a button that filters by it, and a copied id from
 *    a stack trace pastes straight into the request-id box.
 * 2. **Denials are first-class rows, not an error page.** `secret.access.denied` sits in the same
 *    table as a successful reveal, because "who tried to reach a secret they could not" is a more
 *    interesting question than "who read it", and it is the one an operator is most likely to
 *    want a screen for.
 * 3. **A flag is advisory and the screen says so.** The acknowledge action is the *only* write
 *    here, and the header states that no reveal is ever blocked by a flag — with the reason,
 *    because a security screen that overstates what it enforces is worse than one that admits its
 *    own limits. The thresholds come from the API, so the explanation is the operator's own
 *    configuration rather than a sentence invented in the panel.
 *
 * Two things this screen deliberately cannot do, and the shapes are what make that visible:
 * there is no field on a row that could hold a credential value, and the export is a download of
 * newline-delimited metadata rather than a filtered view of the same table.
 *
 * Keyboard: `/` focuses the search box, `f` focuses the request-id box (the field you paste a
 * refusal's id into), `a` toggles the anomalies panel, `e` exports, `Esc` closes it.
 * Under `sm:` the table becomes cards and the filter row wraps to one column.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Check,
  ChevronDown,
  Download,
  Filter,
  RefreshCw,
  Search,
  ShieldCheck,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  acknowledgeSecretAnomaly,
  downloadSecretAuditExport,
  fetchSecretAnomalies,
  fetchSecretAudit,
  type SecretAnomaly,
  type SecretAuditEntry,
  type SecretAuditFilter,
  type SecretAuditResponse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The action chip's tone, per action family. A refusal is never the same colour as a read. */
const ACTION_TONE: Record<string, string> = {
  "secret.revealed": "bg-caution-soft text-caution",
  "secret.access.denied": "bg-danger-soft text-danger",
  "secret.root_key.rotated": "bg-accent-soft text-accent",
  "secret.slot_changed": "bg-quiet-soft text-muted",
  "secret.deploy_key_use": "bg-accent-soft text-accent",
  "secret.lease": "bg-quiet-soft text-muted",
};

/** The action name in the panel's own words, so the row is readable without the vocabulary. */
const ACTION_LABEL: Record<string, string> = {
  "secret.revealed": "revealed",
  "secret.access.denied": "denied",
  "secret.root_key.rotated": "root key rotated",
  "secret.slot_changed": "slot changed",
  "secret.deploy_key_use": "deployment key used",
  "secret.lease": "lease",
};

/** The four patterns, in the words the detector used. */
const PATTERN_LABEL: Record<string, string> = {
  off_hours_reveal: "Reveal outside business hours",
  reveal_burst: "Burst of reveals",
  new_network: "First access from a new address",
  unfamiliar_principal: "Account that never held this secret",
};

const PATTERN_HINT: Record<string, string> = {
  off_hours_reveal: "The reveal happened outside the configured business hours.",
  reveal_burst: "More reveals of one secret inside an hour than the threshold allows.",
  new_network: "The address has not been used for this secret before.",
  unfamiliar_principal: "This account had never revealed this secret before.",
};

/** `/secrets/audit`. */
export function AuditView() {
  const [state, setState] = useState<SecretAuditResponse | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);

  const [needle, setNeedle] = useState("");
  const [actions, setActions] = useState<string[]>([]);
  const [requestId, setRequestId] = useState("");
  const [address, setRequestIdAddress] = useState("");
  const [showAnomalies, setShowAnomalies] = useState(true);
  const [anomalies, setAnomalies] = useState<SecretAnomaly[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [exporting, setExporting] = useState(false);

  const search = useRef<HTMLInputElement | null>(null);
  const requestBox = useRef<HTMLInputElement | null>(null);

  // The server filter is what the trail is narrowed by; `needle` is a local scan on top of it so
  // typing feels immediate. Keeping the two separate is what stops every keystroke becoming a
  // request against a table that is already capped at 200 rows.
  const filter = useMemo<SecretAuditFilter>(
    () => ({
      actions: actions.length ? actions : undefined,
      requestId: requestId.trim() || undefined,
      address: address.trim() || undefined,
      limit: 200,
    }),
    [actions, requestId, address],
  );

  const load = useCallback(async (next: SecretAuditFilter) => {
    try {
      const [trail, flags] = await Promise.all([
        fetchSecretAudit(next),
        fetchSecretAnomalies().catch(() => ({ anomalies: [] })),
      ]);
      setState(trail);
      setAnomalies(flags.anomalies);
      setStatus("ready");
      setLoadError(null);
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "network", message: "The secrets audit trail could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    void load(filter);
  }, [filter, load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT" ||
        target?.isContentEditable;
      if (event.key === "Escape") {
        setShowAnomalies(false);
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === "/") {
        event.preventDefault();
        search.current?.focus();
      } else if (event.key === "f") {
        event.preventDefault();
        requestBox.current?.focus();
      } else if (event.key === "a") {
        setShowAnomalies((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const rows = useMemo(() => {
    const all = state?.entries ?? [];
    const query = needle.trim().toLowerCase();
    if (!query) return all;
    return all.filter((entry) =>
      [actionLabel(entry.action), entry.ip_address ?? "", entry.pipeline ?? "", entry.request_id ?? ""]
        .join(" ")
        .toLowerCase()
        .includes(query),
    );
  }, [state, needle]);

  const openFlags = anomalies.filter((flag) => flag.acknowledged_at === null);

  const acknowledge = async (flag: SecretAnomaly) => {
    setBusy(flag.id);
    setNotice(null);
    try {
      const result = await acknowledgeSecretAnomaly(flag.id);
      // Patched in place rather than re-read, because the API's `already_acknowledged` answer is
      // a normal outcome and a re-read would turn a second click into a full round trip.
      setAnomalies((current) =>
        current.map((row) =>
          row.id === flag.id
            ? {
                ...row,
                acknowledged_at: row.acknowledged_at ?? new Date().toISOString(),
                acknowledged_by: row.acknowledged_by,
              }
            : row,
        ),
      );
      setState((current) =>
        current
          ? { ...current, open_anomalies: Math.max(0, current.open_anomalies - 1) }
          : current,
      );
      setNotice(
        result.state === "acknowledged"
          ? `Flag ${flag.id} acknowledged. It stays in the trail — nothing is deleted.`
          : `Flag ${flag.id} was already acknowledged.`,
      );
    } catch (cause) {
      setNotice(
        cause instanceof ApiError
          ? `The flag could not be acknowledged: ${cause.message}`
          : "The flag could not be acknowledged.",
      );
    } finally {
      setBusy(null);
    }
  };

  const exportFeed = async () => {
    setExporting(true);
    setNotice(null);
    try {
      const body = await downloadSecretAuditExport(filter);
      // A real file, with the row count in the name so two exports are told apart. The blob is
      // revoked immediately: an object URL that is never released is a leak on a long-lived
      // panel session, and this screen can be left open for days.
      const blob = new Blob([body], { type: "application/x-ndjson" });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = `omnion-secrets-audit-${rows.length}.ndjson`;
      link.click();
      URL.revokeObjectURL(url);
      const lines = body.split("\n").filter((line) => line.trim().length > 0).length;
      setNotice(
        lines === 0
          ? "The export was empty: no row matches the current filters."
          : `Exported ${lines} ${lines === 1 ? "row" : "rows"} as newline-delimited JSON. The feed carries metadata only — no credential value can appear in it.`,
      );
    } catch (cause) {
      setNotice(
        cause instanceof ApiError
          ? `The export failed: ${cause.message}`
          : "The export failed.",
      );
    } finally {
      setExporting(false);
    }
  };

  if (status === "loading") return <LoadingTable columns={6} />;

  if (status === "error" && loadError) {
    return (
      <div className="flex flex-col gap-3">
        <p
          role="alert"
          data-audit-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {loadError.message}
        </p>
        <p className="text-[11.5px] text-muted">
          Code <code className="font-mono">{loadError.code}</code>
        </p>
        <button
          type="button"
          onClick={() => void load(filter)}
          className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const detectors = state?.detectors;
  const filters = state?.filters ?? [];

  return (
    <div className="flex flex-col gap-4">
      {/* The header strip carries the numbers, the filters and — the part that is easy to leave
          out — the sentence saying what a flag does and does not do. */}
      <section
        data-audit-summary
        className="flex flex-col gap-3 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <div className="flex flex-wrap items-center gap-x-6 gap-y-2">
          <Stat label="Rows" value={rows.length} />
          <Stat label="Denials" value={rows.filter((row) => row.action === "secret.access.denied").length} />
          <Stat label="Reveals" value={rows.filter((row) => row.action === "secret.revealed").length} />
          <Stat
            label="Open flags"
            value={state?.open_anomalies ?? 0}
            tone={state?.open_anomalies ? "caution" : "quiet"}
          />
          <Stat label="Local hour" value={state?.local_hour ?? 0} />
          <div className="flex flex-1 flex-wrap items-center justify-end gap-2">
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden
              />
              <input
                ref={search}
                value={needle}
                onChange={(event) => setNeedle(event.target.value)}
                placeholder="Search rows — press /"
                aria-label="Search the audit rows"
                data-audit-search
                className="h-8 w-52 rounded-lg border border-line bg-surface pl-8 pr-2 text-[12.5px] outline-none focus:border-accent"
              />
            </div>
            <button
              type="button"
              onClick={exportFeed}
              disabled={exporting}
              data-audit-export
              className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel disabled:opacity-60"
            >
              <Download className="size-3.5" aria-hidden />
              {exporting ? "Exporting…" : "Export feed"}
            </button>
            <button
              type="button"
              onClick={() => void load(filter)}
              className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
            >
              <RefreshCw className="size-3.5" aria-hidden />
              Refresh
            </button>
          </div>
        </div>

        <div className="flex flex-wrap items-center gap-2 border-t border-line pt-3">
          <span className="flex items-center gap-1.5 text-[11.5px] text-muted">
            <Filter className="size-3.5" aria-hidden />
            Action
          </span>
          {filters.length === 0 ? (
            <span className="text-[11.5px] text-muted">No action has been written yet.</span>
          ) : (
            filters.map((action) => (
              <button
                key={action}
                type="button"
                onClick={() =>
                  setActions((current) =>
                    current.includes(action)
                      ? current.filter((value) => value !== action)
                      : [...current, action],
                  )
                }
                aria-pressed={actions.includes(action)}
                data-audit-action={action}
                className={`rounded-full border px-2.5 py-0.5 text-[11.5px] transition ${
                  actions.includes(action)
                    ? "border-accent bg-accent-soft text-accent"
                    : "border-line text-muted hover:text-ink"
                }`}
              >
                {actionLabel(action)}
              </button>
            ))
          )}
        </div>

        <div className="flex flex-wrap items-end gap-2 border-t border-line pt-3">
          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span>Request id</span>
            <input
              ref={requestBox}
              value={requestId}
              onChange={(event) => setRequestId(event.target.value)}
              placeholder="paste a refusal's id — press f"
              data-audit-request-id
              className="h-8 w-60 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] outline-none focus:border-accent"
            />
          </label>
          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span>Address</span>
            <input
              value={address}
              onChange={(event) => setRequestIdAddress(event.target.value)}
              placeholder="exact peer address"
              data-audit-address
              className="h-8 w-48 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] outline-none focus:border-accent"
            />
          </label>
          {actions.length || requestId.trim() || address.trim() ? (
            <button
              type="button"
              onClick={() => {
                setActions([]);
                setRequestId("");
                setRequestIdAddress("");
              }}
              data-audit-clear
              className="h-8 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
            >
              Clear filters
            </button>
          ) : null}
          <p className="ml-auto max-w-md text-[11.5px] leading-snug text-muted">
            {detectors?.explanation}
          </p>
        </div>
      </section>

      {notice ? (
        <p
          role="status"
          data-audit-notice
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      {/* The flags. Placed above the trail deliberately: a flag is the reason to come to this
          screen, and the trail is what you read after. */}
      {showAnomalies ? (
        <section data-audit-anomalies className="flex flex-col gap-2">
          <div className="flex items-center gap-2">
            <TriangleAlert className="size-4 text-caution" aria-hidden />
            <h2 className="text-[13px] font-medium">
              Advisory flags
              {openFlags.length ? (
                <span className="ml-2 text-[11.5px] font-normal text-muted">
                  {openFlags.length} unacknowledged of {anomalies.length}
                </span>
              ) : null}
            </h2>
            <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
              blocking rules: {detectors?.hard_rule_enforced ? "on" : "off"}
            </span>
            <button
              type="button"
              onClick={() => setShowAnomalies(false)}
              aria-label="Hide the advisory flags"
              data-audit-anomalies-hide
              className="ml-auto flex items-center gap-1 text-[11.5px] text-muted transition hover:text-ink"
            >
              Hide
              <ChevronDown className="size-3.5" aria-hidden />
            </button>
          </div>

          {anomalies.length === 0 ? (
            <EmptyState
              title="No flag has been raised"
              hint="Four detectors watch every reveal: one outside business hours, one for a burst on a single secret, one for an address never used for that secret before, and one for an account that had never held it. A flag is recorded and can be acknowledged; nothing is ever blocked by one."
            />
          ) : (
            <ul className="flex flex-col gap-2">
              {anomalies.map((flag) => (
                <li
                  key={flag.id}
                  data-audit-anomaly={flag.id}
                  className={`flex flex-col gap-2 rounded-xl border px-4 py-3 sm:flex-row sm:items-center sm:justify-between ${
                    flag.acknowledged_at
                      ? "border-line bg-surface opacity-70"
                      : "border-caution/40 bg-caution-soft"
                  }`}
                >
                  <div className="flex flex-col gap-1">
                    <p className="text-[12.5px] font-medium">
                      {PATTERN_LABEL[flag.pattern] ?? flag.pattern}
                      <span className="ml-2 rounded-full bg-surface px-2 py-0.5 text-[11px] text-muted">
                        {flag.severity}
                      </span>
                    </p>
                    <p className="text-[11.5px] text-muted">
                      {PATTERN_HINT[flag.pattern] ?? ""}{" "}
                      {flag.secret_name ? `On “${flag.secret_name}”.` : ""}{" "}
                      {flag.address ? `From ${flag.address}.` : ""}{" "}
                      {formatTimestamp(flag.created_at)}
                    </p>
                  </div>
                  {flag.acknowledged_at ? (
                    <p className="text-[11.5px] text-muted">
                      Acknowledged {formatTimestamp(flag.acknowledged_at)}
                    </p>
                  ) : (
                    <button
                      type="button"
                      onClick={() => void acknowledge(flag)}
                      disabled={busy === flag.id}
                      data-audit-acknowledge={flag.id}
                      className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel disabled:opacity-60"
                    >
                      <Check className="size-3.5" aria-hidden />
                      {busy === flag.id ? "Acknowledging…" : "Acknowledge"}
                    </button>
                  )}
                </li>
              ))}
            </ul>
          )}
        </section>
      ) : (
        <button
          type="button"
          onClick={() => setShowAnomalies(true)}
          data-audit-anomalies-show
          className="flex w-fit items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-panel"
        >
          <TriangleAlert className="size-3.5" aria-hidden />
          Show advisory flags
          {openFlags.length ? (
            <span className="rounded-full bg-caution-soft px-1.5 text-[11px] text-caution">
              {openFlags.length}
            </span>
          ) : null}
        </button>
      )}

      {rows.length === 0 ? (
        <EmptyState
          title={
            actions.length || requestId.trim() || address.trim()
              ? "No row matches those filters"
              : "The trail is empty"
          }
          hint={
            actions.length || requestId.trim() || address.trim()
              ? "The filters narrow the trail the same way they narrowed the export. Clear them to see everything this installation has recorded."
              : "Every read, write, rotation, reveal, denial, lease, redemption, slot change and deployment key use lands here with the actor, the address and the request id. Nothing appears until a secrets operation actually runs."
          }
        />
      ) : (
        <>
          {/* Desktop: a table. */}
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface sm:block">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th className="px-4 py-2 font-medium">When</th>
                  <th className="px-4 py-2 font-medium">Action</th>
                  <th className="px-4 py-2 font-medium">Actor</th>
                  <th className="px-4 py-2 font-medium">Address</th>
                  <th className="px-4 py-2 font-medium">Request id</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((entry) => (
                  <AuditRow
                    key={entry.id}
                    entry={entry}
                    onFilterRequest={(id) => {
                      setRequestId(id);
                      requestBox.current?.focus();
                    }}
                  />
                ))}
              </tbody>
            </table>
          </div>
          {/* Mobile: cards, because a six-column table on a phone is a horizontal scroll. */}
          <ul className="flex flex-col gap-2 sm:hidden">
            {rows.map((entry) => (
              <li
                key={entry.id}
                data-audit-card={entry.id}
                className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface px-3 py-2"
              >
                <div className="flex items-center gap-2">
                  <span
                    className={`rounded-full px-2 py-0.5 text-[11px] ${
                      ACTION_TONE[entry.action] ?? "bg-quiet-soft text-muted"
                    }`}
                  >
                    {actionLabel(entry.action)}
                  </span>
                  <span className="text-[11.5px] text-muted">{formatTimestamp(entry.created_at)}</span>
                </div>
                <p className="font-mono text-[11.5px] text-muted">
                  {entry.request_id ?? "—"}
                </p>
                <p className="text-[11.5px] text-muted">{entry.ip_address ?? "—"}</p>
              </li>
            ))}
          </ul>
        </>
      )}

      <p className="flex items-center gap-1.5 text-[11.5px] text-muted">
        <ShieldCheck className="size-3.5" aria-hidden />
        This screen can read metadata and acknowledge a flag. It has no action that returns a
        credential value, and the export it produces is built from an explicit list of fields
        rather than by redacting a row, so a column added to the trail later cannot leak into it.
      </p>
    </div>
  );
}

/** One trail row, shared by the table and (in a reduced form) nothing else — the cards inline it. */
function AuditRow({
  entry,
  onFilterRequest,
}: {
  entry: SecretAuditEntry;
  onFilterRequest: (requestId: string) => void;
}) {
  const actor = entry.actor_user_id
    ? `${entry.actor_type} · ${shortId(entry.actor_user_id)}`
    : entry.actor_type;
  return (
    <tr data-audit-row={entry.id} className="border-b border-line last:border-0">
      <td className="whitespace-nowrap px-4 py-2 text-[12.5px] text-muted">
        {formatTimestamp(entry.created_at)}
      </td>
      <td className="px-4 py-2">
        <span
          className={`rounded-full px-2 py-0.5 text-[11.5px] ${
            ACTION_TONE[entry.action] ?? "bg-quiet-soft text-muted"
          }`}
        >
          {actionLabel(entry.action)}
        </span>
        {entry.pipeline ? (
          <span className="ml-2 text-[11.5px] text-muted">{entry.pipeline}</span>
        ) : null}
      </td>
      <td className="px-4 py-2 text-[12.5px]">{actor}</td>
      <td className="px-4 py-2 font-mono text-[11.5px] text-muted">
        {entry.ip_address ?? "—"}
      </td>
      <td className="px-4 py-2">
        {entry.request_id ? (
          <button
            type="button"
            onClick={() => onFilterRequest(entry.request_id as string)}
            title="Filter the trail by this request id"
            data-audit-row-request={entry.id}
            className="font-mono text-[11.5px] text-accent underline underline-offset-2"
          >
            {shortId(entry.request_id)}
          </button>
        ) : (
          <span className="font-mono text-[11.5px] text-muted">—</span>
        )}
      </td>
    </tr>
  );
}

/** A counter in the header strip. */
function Stat({
  label,
  value,
  tone = "quiet",
}: {
  label: string;
  value: number;
  tone?: "quiet" | "caution";
}) {
  return (
    <div className="flex flex-col">
      <span className="text-[11px] uppercase tracking-wide text-muted">{label}</span>
      <span
        className={`text-[17px] font-semibold tabular-nums ${
          tone === "caution" && value > 0 ? "text-caution" : ""
        }`}
      >
        {value}
      </span>
    </div>
  );
}

/** The action name in words, falling back to the raw name rather than to nothing. */
function actionLabel(action: string): string {
  return ACTION_LABEL[action] ?? action.replace(/^secret\./, "").replace(/_/g, " ");
}

/** `9f3c1a2b` — enough to recognise a uuid by eye without eating the column. */
function shortId(id: string): string {
  return id.length > 8 ? id.slice(0, 8) : id;
}

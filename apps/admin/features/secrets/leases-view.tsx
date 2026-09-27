"use client";

/**
 * `/secrets/leases` — the outstanding credential leases (docs/requests/REQ-125, slice 3).
 *
 * The screen's reason to exist is a rule an operator has to be able to see at a glance: **a lease
 * is a live copy of a credential, and a deploy makes every one of them false.** The moment an
 * operator replaces a payment key, the leases already handed to a pipeline are still valid until
 * something revokes them — and the thing that revokes them is a `deployment.started` event,
 * which belongs to the deployment centre and knows nothing about this request. So the screen has
 * two jobs, and the second is the more important one:
 *
 * 1. show what is live, for which environment, until when, and how much of its redemption budget
 *    is left, so "how many copies of this credential are out there" has an answer;
 * 2. show **why** a lease is dead. A revoked lease says `revoked by an operator` or
 *    `revoked automatically: a deployment started in production` — a lease that silently stopped
 *    working is the failure mode that makes people disable the feature.
 *
 * Two rules the screen keeps, each of them a cost already paid somewhere:
 *
 * - **No value, and no token.** A lease row has a name and a version, never a credential. There is
 *   no "reveal" here even for an operator who could reveal the secret, because a lease is the
 *   whole point of a path that does not put values in a browser.
 * - **Revoking asks for a reason**, because the reason is what the next operator reads. The API
 *   stores it and the audit carries it; a bare "revoked" with no reason is not worth keeping.
 *
 * Keyboard: `/` focuses the search box, `r` revokes the selected row, `Esc` closes the dialogs.
 * Under `sm:` the table becomes cards, and the dialogs are one column.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Ban,
  Clock,
  KeyRound,
  RefreshCw,
  Search,
  ShieldAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchSecretLeases,
  revokeLease,
  type LeasesResponse,
  type SecretLease,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The lifecycle chip, in the panel's own words. A lease is not a secret, so the states are its own. */
const STATE_TONE: Record<string, string> = {
  live: "bg-positive-soft text-positive",
  spent: "bg-quiet-soft text-muted",
  expired: "bg-quiet-soft text-muted",
  revoked: "bg-caution-soft text-caution",
};

const STATE_LABEL: Record<string, string> = {
  live: "live",
  spent: "budget spent",
  expired: "expired",
  revoked: "revoked",
};

/** `14 min` / `2 h 05 m` / `expired` — the countdown, in a form a person can scan. */
function countdown(seconds: number): string {
  if (seconds <= 0) return "expired";
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  if (hours < 24) return rest ? `${hours} h ${String(rest).padStart(2, "0")} m` : `${hours} h`;
  return `${Math.floor(hours / 24)} d ${hours % 24} h`;
}

/** `/secrets/leases`. */
export function LeasesView() {
  const [state, setState] = useState<LeasesResponse | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [needle, setNeedle] = useState("");
  const [filter, setFilter] = useState<"all" | "live">("all");
  const [revoking, setRevoking] = useState<SecretLease | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const search = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    try {
      const next = await fetchSecretLeases();
      setState(next);
      setStatus("ready");
      setLoadError(null);
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "network", message: "The leases could not be read." },
      );
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
        target?.tagName === "SELECT" ||
        target?.isContentEditable;
      if (event.key === "Escape") {
        if (revoking) {
          setRevoking(null);
          return;
        }
      }
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        search.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [revoking]);

  const rows = useMemo(() => {
    const all = state?.leases ?? [];
    const query = needle.trim().toLowerCase();
    return all.filter((lease) => {
      if (filter === "live" && lease.state !== "live") return false;
      if (!query) return true;
      return [lease.name, lease.consumer, lease.environment]
        .filter(Boolean)
        .some((value) => value.toLowerCase().includes(query));
    });
  }, [state, needle, filter]);

  if (status === "loading") return <LoadingTable columns={6} />;

  if (status === "error" && loadError) {
    return (
      <div className="flex flex-col gap-3">
        <p
          role="alert"
          data-leases-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {loadError.message}
        </p>
        <p className="text-[11.5px] text-muted">
          Code <code className="font-mono">{loadError.code}</code>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const live = state?.live ?? 0;
  const autoRevoked = (state?.leases ?? []).filter(
    (lease) => lease.state === "revoked" && (lease.revoke_reason ?? "").startsWith("revoked automatically"),
  ).length;

  return (
    <div className="flex flex-col gap-4">
      <section
        data-leases-summary
        className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <Stat label="Leases" value={state?.total ?? 0} />
        <Stat label="Live" value={live} />
        <Stat label="Spent" value={state?.spent ?? 0} />
        <Stat label="Revoked" value={state?.revoked ?? 0} />
        <Stat label="Revoked by a deploy" value={autoRevoked} />
        <div className="flex flex-1 items-center justify-end gap-2">
          <div className="relative">
            <Search
              className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
              aria-hidden
            />
            <input
              ref={search}
              value={needle}
              onChange={(event) => setNeedle(event.target.value)}
              placeholder="Search leases — press /"
              aria-label="Search leases"
              data-leases-search
              className="h-8 w-56 rounded-lg border border-line bg-surface pl-8 pr-2 text-[12.5px] outline-none focus:border-accent"
            />
          </div>
          <button
            type="button"
            onClick={() => setFilter(filter === "live" ? "all" : "live")}
            aria-pressed={filter === "live"}
            data-leases-filter
            className={`flex h-8 items-center gap-1.5 rounded-lg border px-3 text-[12.5px] transition ${
              filter === "live"
                ? "border-accent bg-accent-soft text-accent"
                : "border-line bg-surface hover:bg-panel"
            }`}
          >
            <Clock className="size-3.5" aria-hidden />
            Live only
          </button>
          <button
            type="button"
            onClick={() => void load()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
        </div>
      </section>

      {notice ? (
        <p
          role="status"
          data-leases-notice
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      {rows.length === 0 ? (
        <EmptyState
          title={
            needle
              ? "No lease matches that search"
              : filter === "live"
                ? "Nothing is leased right now"
                : "No lease has been issued yet"
          }
          hint={
            needle
              ? "The search covers the credential, the consumer and the environment."
              : "A lease is a short-lived, use-capped copy of a credential handed to a workload. Issue one from a credential's detail screen; it expires on its own, and a deployment revokes every lease in that environment without anyone asking."
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line bg-surface">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th className="px-4 py-2 font-medium">Credential</th>
                <th className="px-4 py-2 font-medium">Consumer</th>
                <th className="px-4 py-2 font-medium">Environment</th>
                <th className="px-4 py-2 font-medium">State</th>
                <th className="px-4 py-2 font-medium">Budget</th>
                <th className="px-4 py-2 font-medium">Expires</th>
                <th className="px-4 py-2" />
              </tr>
            </thead>
            <tbody>
              {rows.map((lease) => (
                <LeaseRow
                  key={lease.id}
                  lease={lease}
                  onRevoke={() => {
                    setNotice(null);
                    setRevoking(lease);
                  }}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {revoking ? (
        <RevokeDialog
          lease={revoking}
          onClose={() => setRevoking(null)}
          onRevoked={async (message) => {
            setRevoking(null);
            setNotice(message);
            await load();
          }}
        />
      ) : null}
    </div>
  );
}

/** A counter in the header strip. */
function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="flex flex-col">
      <span className="text-[11px] text-muted">{label}</span>
      <span className="text-[15px] font-medium tabular-nums">{value}</span>
    </div>
  );
}

/** One lease. The mobile layout is the same markup in a stacked card. */
function LeaseRow({ lease, onRevoke }: { lease: SecretLease; onRevoke: () => void }) {
  const tone = STATE_TONE[lease.state] ?? "bg-quiet-soft text-muted";
  return (
    <tr data-lease-row data-lease-state={lease.state} className="border-b border-line last:border-0">
      <td className="px-4 py-2.5">
        <span className="font-medium">{lease.name}</span>
        <span className="ml-2 text-[11.5px] text-muted">v{lease.version}</span>
      </td>
      <td className="px-4 py-2.5 font-mono text-[12px] text-muted">{lease.consumer}</td>
      <td className="px-4 py-2.5">{lease.environment}</td>
      <td className="px-4 py-2.5">
        <span className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${tone}`}>
          {STATE_LABEL[lease.state] ?? lease.state}
        </span>
        {lease.revoke_reason ? (
          <span data-lease-reason className="mt-1 block max-w-xs text-[11.5px] text-muted">
            {lease.revoke_reason}
          </span>
        ) : null}
      </td>
      <td className="px-4 py-2.5 tabular-nums text-muted">
        {lease.uses} / {lease.max_uses}
      </td>
      <td className="px-4 py-2.5 tabular-nums text-muted">
        {lease.state === "live" ? countdown(lease.expires_in_seconds) : "—"}
      </td>
      <td className="px-4 py-2.5 text-right">
        {lease.state === "live" ? (
          <button
            type="button"
            onClick={onRevoke}
            data-lease-revoke={lease.id}
            className="inline-flex h-7 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-panel"
          >
            <Ban className="size-3.5" aria-hidden />
            Revoke
          </button>
        ) : (
          <span className="text-[11.5px] text-muted">
            {lease.last_redeemed_at
              ? `last used ${formatTimestamp(lease.last_redeemed_at)}`
              : "never redeemed"}
          </span>
        )}
      </td>
    </tr>
  );
}

/** The revoke dialog. The reason is required, because the reason is what the next reader needs. */
function RevokeDialog({
  lease,
  onClose,
  onRevoked,
}: {
  lease: SecretLease;
  onClose: () => void;
  onRevoked: (message: string) => Promise<void>;
}) {
  const [reason, setReason] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const heading = useRef<HTMLHeadingElement | null>(null);

  useEffect(() => {
    heading.current?.focus();
  }, []);

  const submit = async () => {
    const trimmed = reason.trim();
    if (!trimmed) {
      setError("Say why this lease is being taken back — the reason is kept in the audit.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await revokeLease(lease.id, trimmed);
      await onRevoked(
        `The lease on ${lease.name} for ${lease.consumer} is revoked. A workload still holding it gets a denial and an audit row, not a credential.`,
      );
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The lease could not be revoked.");
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[10vh]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="lease-revoke-title"
      data-lease-dialog
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-md rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="lease-revoke-title"
          ref={heading}
          tabIndex={-1}
          className="flex items-center gap-2 text-[15px] font-medium outline-none"
        >
          <ShieldAlert className="size-4 text-caution" aria-hidden />
          Revoke this lease
        </h3>
        <p className="mt-1.5 text-[12.5px] text-muted">
          <span className="font-medium">{lease.name}</span> v{lease.version} is leased to{" "}
          <span className="font-mono">{lease.consumer}</span> in{" "}
          <span className="font-medium">{lease.environment}</span>, with{" "}
          {lease.uses} of {lease.max_uses} redemptions spent. Revoking stops the next redemption;
          the handle the workload already holds becomes a denial with an audit row.
        </p>
        <label className="mt-4 block text-[12px] text-muted" htmlFor="lease-revoke-reason">
          Reason (kept in the audit and on the lease row)
        </label>
        <input
          id="lease-revoke-reason"
          value={reason}
          onChange={(event) => setReason(event.target.value)}
          data-lease-reason-input
          placeholder="pipeline retired, credential rotated, wrong consumer…"
          className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
        />
        {error ? (
          <p role="alert" data-lease-dialog-error className="mt-2 text-[12px] text-caution">
            {error}
          </p>
        ) : null}
        <div className="mt-4 flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="h-8 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            Keep it
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy}
            data-lease-revoke-confirm
            className="flex h-8 items-center gap-1.5 rounded-lg border border-danger/50 bg-danger-soft px-3 text-[12.5px] text-caution transition disabled:opacity-60"
          >
            <KeyRound className="size-3.5" aria-hidden />
            {busy ? "Revoking…" : "Revoke the lease"}
          </button>
        </div>
      </div>
    </div>
  );
}

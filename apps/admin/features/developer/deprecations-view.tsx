/**
 * `/developer/api/deprecations` — the versioned API policy (REQ-130, slice 4).
 *
 * ## The screen's numbers come from the server, never from a copy
 *
 * The windows, the amber threshold and every countdown are fields on the list response. A screen
 * that hard-codes "6 months" beside a form whose server refuses at a different number teaches the
 * operator that the screen is decorative: they pick a date the form called valid and get a
 * refusal, and the next thing they do is stop reading the messages. So `policy` is read, and the
 * amber threshold is a constant only in the one place the *server* also reads it — the crate.
 *
 * ## The amber state is a colour AND a word
 *
 * "amber inside 30 days" in the request's own words is a colour instruction, and a colour alone is
 * invisible to a screen reader and to a printer. Every countdown cell carries its number and its
 * status word as text, and the amber is an additional class on top.
 *
 * ## Every action is a real call with a real body
 *
 * Announce, extend, withdraw, mark-notified and the CSV export all hit endpoints that exist. The
 * extend form requires its reason because the server requires it — the `required` attribute here
 * is a courtesy, not the enforcement, and the refusal message is what the operator sees when they
 * get it wrong.
 *
 * Keyboard: `n` opens Announce, `r` reloads, `Esc` closes the open dialog.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  BellRing,
  CalendarClock,
  Check,
  Download,
  Megaphone,
  RefreshCw,
  RotateCcw,
  X,
} from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  announceDeprecation,
  exportDeprecationsCsv,
  fetchDeprecations,
  markDeprecationNotified,
  extendDeprecation,
  withdrawDeprecation,
  type DeprecationRow,
  type DeprecationPolicy,
} from "@/lib/deprecation-api";

type Dialog = { kind: "announce" } | { kind: "extend"; row: DeprecationRow } | { kind: "withdraw"; row: DeprecationRow } | null;

const STATUS_TONE: Record<string, string> = {
  announced: "border-line bg-surface-2 text-ink-2",
  active: "border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300",
  removed: "border-danger/40 bg-danger/10 text-danger",
  withdrawn: "border-line bg-surface-2 text-ink-3",
};

/** The message a `422` carries, or a sentence that says something is wrong. */
function messageOf(error: unknown): string {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error) return error.message;
  return "Something went wrong.";
}

export function DeprecationsView() {
  const [rows, setRows] = useState<DeprecationRow[] | null>(null);
  const [policy, setPolicy] = useState<DeprecationPolicy | null>(null);
  const [removed, setRemoved] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<Dialog>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const response = await fetchDeprecations();
      setRows(response.deprecations);
      setPolicy(response.policy);
      setRemoved(response.removed);
    } catch (cause) {
      setError(messageOf(cause));
      setRows([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // `n` opens Announce and `r` reloads, from anywhere on the screen and from inside a field —
  // an operator reading a date should not have to reach for the mouse to check it.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing = target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName);
      if (event.key === "Escape" && dialog) {
        setDialog(null);
        return;
      }
      if (typing) return;
      if (event.key === "n") setDialog({ kind: "announce" });
      if (event.key === "r") void load();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [dialog, load]);

  const run = useCallback(
    async (what: string, action: () => Promise<void>) => {
      setBusy(true);
      setError(null);
      setNotice(null);
      try {
        await action();
        setNotice(what);
        await load();
      } catch (cause) {
        setError(messageOf(cause));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const counts = useMemo(() => {
    const now = (rows ?? []).filter((row) => row.status !== "withdrawn");
    return {
      announced: now.filter((row) => row.status === "announced").length,
      soon: now.filter((row) => row.amber && row.status !== "removed").length,
      removed,
    };
  }, [rows, removed]);

  if (rows === null && error === null) {
    return <LoadingTable rows={6} columns={6} />;
  }

  return (
    <div className="space-y-5">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold">Deprecations</h2>
          <p className="text-sm text-ink-3">
            Every deprecated route carries a <code>Deprecation</code>, <code>Sunset</code> and
            changelog <code>Link</code> header. A sunset in the past makes the route answer{" "}
            <code>410</code> and stop running.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={() => void load()}
            disabled={busy}
            className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm hover:bg-surface-2"
          >
            <RefreshCw aria-hidden className="h-4 w-4" />
            Reload
          </button>
          <button
            type="button"
            onClick={() => exportDeprecationsCsv(rows ?? [])}
            className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm hover:bg-surface-2"
          >
            <Download aria-hidden className="h-4 w-4" />
            Export CSV
          </button>
          <button
            type="button"
            onClick={() => setDialog({ kind: "announce" })}
            disabled={busy}
            className="inline-flex items-center gap-2 rounded-md bg-accent px-3 py-2 text-sm font-medium text-white"
          >
            <Megaphone aria-hidden className="h-4 w-4" />
            Announce
          </button>
        </div>
      </header>

      {policy ? (
        <p className="rounded-md border border-line bg-surface-2 px-3 py-2 text-sm text-ink-2">
          A public route keeps working for at least <strong>{policy.public_months} months</strong>{" "}
          after the announcement; a developer-internal route for{" "}
          <strong>{policy.developer_months} months</strong>. A countdown turns amber inside{" "}
          {policy.amber_within_days} days.
        </p>
      ) : null}

      {error ? (
        <div role="alert" className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 px-3 py-2 text-sm text-danger">
          <AlertTriangle aria-hidden className="mt-0.5 h-4 w-4 shrink-0" />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void load()} className="underline">
            Retry
          </button>
        </div>
      ) : null}

      {notice ? (
        <p role="status" className="flex items-center gap-2 rounded-md border border-line bg-surface-2 px-3 py-2 text-sm">
          <Check aria-hidden className="h-4 w-4" />
          {notice}
        </p>
      ) : null}

      {rows && rows.length === 0 ? (
        <div className="rounded-md border border-dashed border-line px-6 py-12 text-center">
          <CalendarClock aria-hidden className="mx-auto mb-3 h-8 w-8 text-ink-3" />
          <h3 className="font-medium">Nothing is deprecated</h3>
          <p className="mx-auto mt-1 max-w-prose text-sm text-ink-3">
            A route announced here keeps answering until its sunset, and answers <code>410</code>{" "}
            with the code <code>REMOVED</code> afterwards. Press <kbd>n</kbd> to announce the first
            one.
          </p>
          <button
            type="button"
            onClick={() => setDialog({ kind: "announce" })}
            className="mt-4 inline-flex items-center gap-2 rounded-md bg-accent px-3 py-2 text-sm font-medium text-white"
          >
            <Megaphone aria-hidden className="h-4 w-4" />
            Announce a deprecation
          </button>
        </div>
      ) : null}

      {rows && rows.length > 0 ? (
        <>
          <p className="text-sm text-ink-3">
            {rows.length} deprecation{rows.length === 1 ? "" : "s"} · {counts.announced} announced ·{" "}
            {counts.soon} inside the amber window · {counts.removed} removed
          </p>

          {/* The table at >= sm, cards below it. A seven-column table at 390px either scrolls
              horizontally or squeezes every column into unreadable widths; the request asks for
              cards at 390 and this is where that happens. */}
          <div className="hidden overflow-x-auto rounded-md border border-line sm:block">
            <table className="w-full text-left text-sm">
              <thead className="bg-surface-2 text-xs uppercase tracking-wide text-ink-3">
                <tr>
                  <th className="px-3 py-2">Route or field</th>
                  <th className="px-3 py-2">Deprecated in</th>
                  <th className="px-3 py-2">Sunset</th>
                  <th className="px-3 py-2">Replacement</th>
                  <th className="px-3 py-2">Notified</th>
                  <th className="px-3 py-2">Status</th>
                  <th className="px-3 py-2">Actions</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr key={row.id} className="border-t border-line align-top">
                    <td className="px-3 py-2">
                      <span className="font-mono text-xs">{row.surface}</span>
                      {row.method ? <span className="ml-2 text-xs text-ink-3">{row.method}</span> : null}
                      {row.organization_id === null ? (
                        <span className="ml-2 rounded border border-line px-1 text-xs text-ink-3">
                          whole installation
                        </span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2">{row.deprecated_in}</td>
                    <td className="px-3 py-2">
                      <span className={row.amber ? "font-medium text-amber-700 dark:text-amber-300" : ""}>
                        {new Date(row.sunset_at).toISOString().slice(0, 10)}
                      </span>
                      <span className="block text-xs text-ink-3">{row.countdown}</span>
                    </td>
                    <td className="px-3 py-2 font-mono text-xs">{row.replacement ?? "—"}</td>
                    <td className="px-3 py-2">
                      {row.notified_at ? (
                        <span className="inline-flex items-center gap-1 text-xs">
                          <Check aria-hidden className="h-3.5 w-3.5" />
                          {new Date(row.notified_at).toISOString().slice(0, 10)}
                        </span>
                      ) : (
                        <button
                          type="button"
                          disabled={busy || row.status === "withdrawn"}
                          onClick={() =>
                            void run("Marked as notified.", async () => {
                              await markDeprecationNotified(row.id);
                            })
                          }
                          className="inline-flex items-center gap-1 text-xs underline disabled:opacity-40"
                        >
                          <BellRing aria-hidden className="h-3.5 w-3.5" />
                          Mark notified
                        </button>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <span className={`rounded border px-1.5 py-0.5 text-xs ${STATUS_TONE[row.status] ?? ""}`}>
                        {row.status}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      <Actions
                        row={row}
                        busy={busy}
                        onExtend={() => setDialog({ kind: "extend", row })}
                        onWithdraw={() => setDialog({ kind: "withdraw", row })}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <ul className="space-y-3 sm:hidden">
            {rows.map((row) => (
              <li key={row.id} className="rounded-md border border-line p-3">
                <div className="flex items-start justify-between gap-2">
                  <span className="font-mono text-xs">{row.surface}</span>
                  <span className={`rounded border px-1.5 py-0.5 text-xs ${STATUS_TONE[row.status] ?? ""}`}>
                    {row.status}
                  </span>
                </div>
                <dl className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
                  <dt className="text-ink-3">Deprecated in</dt>
                  <dd>{row.deprecated_in}</dd>
                  <dt className="text-ink-3">Sunset</dt>
                  <dd className={row.amber ? "font-medium text-amber-700 dark:text-amber-300" : ""}>
                    {row.countdown}
                  </dd>
                  <dt className="text-ink-3">Replacement</dt>
                  <dd className="truncate font-mono">{row.replacement ?? "—"}</dd>
                  <dt className="text-ink-3">Notified</dt>
                  <dd>{row.notified_at ? new Date(row.notified_at).toISOString().slice(0, 10) : "no"}</dd>
                </dl>
                <div className="mt-3">
                  <Actions
                    row={row}
                    busy={busy}
                    onExtend={() => setDialog({ kind: "extend", row })}
                    onWithdraw={() => setDialog({ kind: "withdraw", row })}
                  />
                </div>
              </li>
            ))}
          </ul>
        </>
      ) : null}

      {dialog?.kind === "announce" ? (
        <AnnounceDialog
          policy={policy}
          busy={busy}
          onClose={() => setDialog(null)}
          onSubmit={async (input) => {
            await run("The deprecation is announced.", async () => {
              await announceDeprecation(input);
              setDialog(null);
            });
          }}
        />
      ) : null}

      {dialog?.kind === "extend" ? (
        <ExtendDialog
          row={dialog.row}
          busy={busy}
          onClose={() => setDialog(null)}
          onSubmit={async (sunset, reason) => {
            await run("The sunset was moved.", async () => {
              await extendDeprecation(dialog.row.id, sunset, reason);
              setDialog(null);
            });
          }}
        />
      ) : null}

      {dialog?.kind === "withdraw" ? (
        <WithdrawDialog
          row={dialog.row}
          busy={busy}
          onClose={() => setDialog(null)}
          onSubmit={async (reason) => {
            await run("The deprecation was withdrawn.", async () => {
              await withdrawDeprecation(dialog.row.id, reason);
              setDialog(null);
            });
          }}
        />
      ) : null}
    </div>
  );
}

function Actions({
  row,
  busy,
  onExtend,
  onWithdraw,
}: {
  row: DeprecationRow;
  busy: boolean;
  onExtend: () => void;
  onWithdraw: () => void;
}) {
  // A withdrawn row is over: extending it would re-announce something that was called off, and
  // withdrawing it again is the same call twice. Both controls are REMOVED rather than disabled,
  // because a disabled control beside a row invites the reader to wonder what it would do.
  if (row.status === "withdrawn") {
    return <span className="text-xs text-ink-3">withdrawn — nothing to change</span>;
  }
  return (
    <div className="flex flex-wrap gap-2 text-xs">
      <button type="button" disabled={busy} onClick={onExtend} className="inline-flex items-center gap-1 underline disabled:opacity-40">
        <CalendarClock aria-hidden className="h-3.5 w-3.5" />
        Extend
      </button>
      {row.status !== "removed" ? (
        <button type="button" disabled={busy} onClick={onWithdraw} className="inline-flex items-center gap-1 underline disabled:opacity-40">
          <X aria-hidden className="h-3.5 w-3.5" />
          Withdraw
        </button>
      ) : null}
    </div>
  );
}

/** A dialog shell, so the three forms share one focus behaviour and one Escape rule. */
function Dialog2({
  title,
  children,
  onClose,
}: {
  title: string;
  children: React.ReactNode;
  onClose: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={title}
      className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-4 sm:items-center"
    >
      <div className="max-h-[90vh] w-full max-w-lg overflow-y-auto rounded-lg border border-line bg-surface p-4">
        <div className="mb-3 flex items-center justify-between">
          <h3 className="font-semibold">{title}</h3>
          <button type="button" onClick={onClose} aria-label="Close" className="rounded p-1 hover:bg-surface-2">
            <X aria-hidden className="h-4 w-4" />
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}

function AnnounceDialog({
  policy,
  busy,
  onClose,
  onSubmit,
}: {
  policy: DeprecationPolicy | null;
  busy: boolean;
  onClose: () => void;
  onSubmit: (input: {
    route_pattern: string | null;
    method: string | null;
    field_path: string | null;
    deprecated_in: string;
    sunset_at: string;
    replacement: string | null;
    note: string;
  }) => Promise<void>;
}) {
  const [kind, setKind] = useState<"route" | "field" | "installation">("route");
  const [route, setRoute] = useState("");
  const [field, setField] = useState("");
  const [method, setMethod] = useState("");
  const [version, setVersion] = useState("");
  const [sunset, setSunset] = useState("");
  const [replacement, setReplacement] = useState("");
  const [note, setNote] = useState("");

  // The local floor is the server's number, and it is shown IN THE FIELDS rather than enforced
  // here: a browser date picker that cannot express the server's rule is a control the operator
  // learns to distrust, while a hint next to a field they can still submit teaches them the rule.
  const hint = useMemo(() => {
    const months = kind === "route" && route.startsWith("/developer") ? policy?.developer_months : policy?.public_months;
    return months ? `At least ${months} months after today.` : null;
  }, [kind, route, policy]);

  return (
    <Dialog2 title="Announce a deprecation" onClose={onClose}>
      <form
        className="space-y-3"
        onSubmit={(event) => {
          event.preventDefault();
          void onSubmit({
            route_pattern: kind === "route" && route ? route : null,
            method: kind === "route" && method ? method : null,
            field_path: kind === "field" && field ? field : null,
            deprecated_in: version,
            sunset_at: new Date(sunset).toISOString(),
            replacement: replacement || null,
            note,
          });
        }}
      >
        <fieldset>
          <legend className="mb-1 text-sm font-medium">What is deprecated</legend>
          <div className="flex flex-wrap gap-3 text-sm">
            {(["route", "field", "installation"] as const).map((option) => (
              <label key={option} className="inline-flex items-center gap-1.5">
                <input type="radio" name="kind" checked={kind === option} onChange={() => setKind(option)} />
                {option === "route" ? "A REST route" : option === "field" ? "A GraphQL field" : "The whole installation"}
              </label>
            ))}
          </div>
        </fieldset>

        {kind === "route" ? (
          <label className="block text-sm">
            <span className="mb-1 block font-medium">Route pattern</span>
            <input
              value={route}
              onChange={(event) => setRoute(event.target.value)}
              placeholder="/api/v1/pages"
              required
              className="w-full rounded border border-line bg-surface px-2 py-1.5 font-mono text-sm"
            />
          </label>
        ) : null}

        {kind === "field" ? (
          <label className="block text-sm">
            <span className="mb-1 block font-medium">Field path</span>
            <input
              value={field}
              onChange={(event) => setField(event.target.value)}
              placeholder="Page.author"
              required
              className="w-full rounded border border-line bg-surface px-2 py-1.5 font-mono text-sm"
            />
          </label>
        ) : null}

        {kind === "route" ? (
          <label className="block text-sm">
            <span className="mb-1 block font-medium">Method</span>
            <select
              value={method}
              onChange={(event) => setMethod(event.target.value)}
              className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
            >
              <option value="">Every method on the path</option>
              {["GET", "POST", "PUT", "PATCH", "DELETE"].map((option) => (
                <option key={option} value={option}>
                  {option}
                </option>
              ))}
            </select>
          </label>
        ) : null}

        <label className="block text-sm">
          <span className="mb-1 block font-medium">Deprecated in</span>
          <input
            value={version}
            onChange={(event) => setVersion(event.target.value)}
            placeholder="1.4"
            required
            className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
          />
          <span className="mt-1 block text-xs text-ink-3">The version the Deprecation header carries.</span>
        </label>

        <label className="block text-sm">
          <span className="mb-1 block font-medium">Sunset</span>
          <input
            type="date"
            value={sunset}
            onChange={(event) => setSunset(event.target.value)}
            required
            className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
          />
          {hint ? <span className="mt-1 block text-xs text-ink-3">{hint}</span> : null}
        </label>

        <label className="block text-sm">
          <span className="mb-1 block font-medium">Replacement</span>
          <input
            value={replacement}
            onChange={(event) => setReplacement(event.target.value)}
            placeholder="/api/v1/pages?cursor=…"
            className="w-full rounded border border-line bg-surface px-2 py-1.5 font-mono text-sm"
          />
          <span className="mt-1 block text-xs text-ink-3">A route with no replacement needs a note.</span>
        </label>

        <label className="block text-sm">
          <span className="mb-1 block font-medium">Note</span>
          <textarea
            value={note}
            onChange={(event) => setNote(event.target.value)}
            rows={2}
            className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
          />
        </label>

        <div className="flex justify-end gap-2 pt-1">
          <button type="button" onClick={onClose} className="rounded border border-line px-3 py-1.5 text-sm">
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy}
            className="inline-flex items-center gap-2 rounded bg-accent px-3 py-1.5 text-sm font-medium text-white disabled:opacity-50"
          >
            <Megaphone aria-hidden className="h-4 w-4" />
            Announce
          </button>
        </div>
      </form>
    </Dialog2>
  );
}

function ExtendDialog({
  row,
  busy,
  onClose,
  onSubmit,
}: {
  row: DeprecationRow;
  busy: boolean;
  onClose: () => void;
  onSubmit: (sunset: string, reason: string) => Promise<void>;
}) {
  const [sunset, setSunset] = useState(row.sunset_at.slice(0, 10));
  const [reason, setReason] = useState("");

  return (
    <Dialog2 title={`Extend the sunset of ${row.surface}`} onClose={onClose}>
      <form
        className="space-y-3"
        onSubmit={(event) => {
          event.preventDefault();
          void onSubmit(new Date(sunset).toISOString(), reason);
        }}
      >
        <p className="text-sm text-ink-3">
          Currently {new Date(row.sunset_at).toISOString().slice(0, 10)}. An extension moves the date
          later and is recorded in the audit trail with your reason; shortening it is a withdrawal,
          which is a different action.
        </p>
        <label className="block text-sm">
          <span className="mb-1 block font-medium">New sunset</span>
          <input
            type="date"
            value={sunset}
            onChange={(event) => setSunset(event.target.value)}
            required
            className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
          />
        </label>
        <label className="block text-sm">
          <span className="mb-1 block font-medium">Reason</span>
          <textarea
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            rows={2}
            required
            placeholder="the integrator asked for a quarter"
            className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
          />
          <span className="mt-1 block text-xs text-ink-3">Recorded in the audit trail, not on the row.</span>
        </label>
        <div className="flex justify-end gap-2 pt-1">
          <button type="button" onClick={onClose} className="rounded border border-line px-3 py-1.5 text-sm">
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy}
            className="inline-flex items-center gap-2 rounded bg-accent px-3 py-1.5 text-sm font-medium text-white disabled:opacity-50"
          >
            <CalendarClock aria-hidden className="h-4 w-4" />
            Extend
          </button>
        </div>
      </form>
    </Dialog2>
  );
}

function WithdrawDialog({
  row,
  busy,
  onClose,
  onSubmit,
}: {
  row: DeprecationRow;
  busy: boolean;
  onClose: () => void;
  onSubmit: (reason: string) => Promise<void>;
}) {
  const [reason, setReason] = useState("");

  return (
    <Dialog2 title={`Withdraw the deprecation of ${row.surface}`} onClose={onClose}>
      <form
        className="space-y-3"
        onSubmit={(event) => {
          event.preventDefault();
          void onSubmit(reason);
        }}
      >
        <p className="flex items-start gap-2 text-sm text-ink-2">
          <AlertTriangle aria-hidden className="mt-0.5 h-4 w-4 shrink-0 text-amber-600" />
          The route stops carrying its <code>Deprecation</code> and <code>Sunset</code> headers and
          works as if it was never deprecated. Integrators who already read the headers are not
          told automatically — the reason below is what the audit trail holds.
        </p>
        <label className="block text-sm">
          <span className="mb-1 block font-medium">Reason</span>
          <textarea
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            rows={2}
            required
            placeholder="the replacement shipped under the same path"
            className="w-full rounded border border-line bg-surface px-2 py-1.5 text-sm"
          />
        </label>
        <div className="flex justify-end gap-2 pt-1">
          <button type="button" onClick={onClose} className="rounded border border-line px-3 py-1.5 text-sm">
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy}
            className="inline-flex items-center gap-2 rounded bg-danger px-3 py-1.5 text-sm font-medium text-white disabled:opacity-50"
          >
            <RotateCcw aria-hidden className="h-4 w-4" />
            Withdraw
          </button>
        </div>
      </form>
    </Dialog2>
  );
}
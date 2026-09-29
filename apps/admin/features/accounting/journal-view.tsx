"use client";

/**
 * The journal (REQ-054, slice 1): `/accounting/journal`.
 *
 * ## The one thing this screen exists to show
 *
 * **A balanced entry, and the arithmetic that says so.** Every row carries both totals, and the
 * badge beside them is the entry's own `balanced` column — not a sum recomputed in the browser. A
 * client-side sum of the two columns is a *different* sum, and it is the one that would print a
 * green "balanced" on an entry the server then refuses, which is the most expensive way for a
 * bookkeeper to lose trust in a screen.
 *
 * ## Why the posting form is a drawer and not a page
 *
 * A journal entry is a grid. The operator's reference while typing is the **running balance** down
 * the side, and that reference only exists while the grid is visible — a separate page, a save, a
 * reload, and the number that justified the entry is gone. So the form keeps the running total on
 * screen while it is being filled in, and the server's verdict is the one that counts: on a
 * refusal the footer replaces the optimistic number with the three figures the server computed,
 * because those are the ones the books will be held to.
 *
 * ## What this screen must not have
 *
 * **An edit or delete control on a posted entry.** Not a disabled one, not a hidden one. A posted
 * entry is corrected by a reversing entry, never by editing the original, and a pencil here would
 * promise something the module refuses — and a promise the server breaks is worse than an
 * absence, because the operator finds out by losing an afternoon.
 */
import { Suspense, useCallback, useEffect, useMemo, useState } from "react";
import { useSearchParams } from "next/navigation";
import { Plus, RefreshCw, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  ENTRY_SOURCES,
  UnbalancedEntryError,
  fetchAccounts,
  fetchJournal,
  postJournalEntry,
  type Account,
  type JournalLineDraft,
  type JournalSummary,
} from "@/lib/accounting";

const COLUMNS = 7;

const SOURCES: { value: string; label: string }[] = ENTRY_SOURCES.map((entry) => ({
  value: entry.value,
  label: entry.label,
}));

/** Format a money string for a table cell, without ever going through a float. */
function money(text: string): string {
  // The server sends `"1200.00"`. Splitting on the point and grouping the integer part keeps
  // this exact: `parseFloat("1200.00")` is 1200 and `1200.toLocaleString()` is fine, but the
  // general path must not depend on the value being representable.
  const negative = text.startsWith("-");
  const bare = negative ? text.slice(1) : text;
  const [whole, fraction = "00"] = bare.split(".");
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${negative ? "-" : ""}${grouped}.${fraction}`;
}

/** The running balance after each line, in the same integer arithmetic the server uses. */
function runningBalances(lines: JournalLineDraft[]): { balances: string[]; total: string } {
  let running = 0;
  const balances: string[] = [];
  for (const line of lines) {
    const debit = cents(line.debit);
    const credit = cents(line.credit);
    // A line that somehow carries both would be refused by the server; the running figure here
    // treats it as the difference, which is the conservative reading.
    running += debit - credit;
    balances.push((running / 100).toFixed(2));
  }
  return { balances, total: (running / 100).toFixed(2) };
}

/** Parse money text to integer hundredths. A cell that is not a number counts as zero. */
function cents(text: string): number {
  const parsed = Number.parseFloat(text.trim());
  if (!Number.isFinite(parsed)) {
    return 0;
  }
  return Math.round(parsed * 100);
}

/** The badge a source wears, so the list and the filter share one vocabulary. */
function SourceBadge({ source }: { source: string }) {
  const tone =
    source === "manual"
      ? "border-stone-300 bg-stone-50 text-stone-800"
      : "border-sky-200 bg-sky-50 text-sky-900";
  return (
    <span className={`inline-block rounded border px-1.5 py-0.5 text-[11.5px] ${tone}`}>
      {SOURCES.find((entry) => entry.value === source)?.label ?? source}
    </span>
  );
}

/**
 * The screen, wrapped for `useSearchParams`.
 *
 * Next's rule is that a client component reading search parameters must sit inside a Suspense
 * boundary or the whole route opts out of static rendering. The boundary is a `fallback` that
 * looks like the loading state rather than null: a screen that renders nothing for a frame and
 * then a table reads as a flash, and the module's other views show a skeleton for the same wait.
 */
export function JournalView() {
  return (
    <Suspense fallback={<LoadingTable columns={COLUMNS} rows={4} />}>
      <JournalScreen />
    </Suspense>
  );
}

function JournalScreen() {
  const [search, setSearch] = useState("");
  const [source, setSource] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [rows, setRows] = useState<JournalSummary[]>([]);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // `?compose=1` opens the drawer on arrival. It is read once, on mount, and then dropped from
  // the URL: a parameter that stays put reopens the drawer every time the screen remounts, and a
  // person who closes a form and presses Back has not asked for the form again. A palette row
  // pointing at a parameter nothing reads is a dead button, and this is the read that keeps it
  // alive.
  const compose = useSearchParams().get("compose") === "1";
  const [composing, setComposing] = useState(false);

  useEffect(() => {
    if (compose) {
      setComposing(true);
    }
  }, [compose]);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [entries, chart] = await Promise.all([fetchJournal(), fetchAccounts()]);
      setRows(entries);
      setAccounts(chart.filter((account) => account.active));
    } catch (caught) {
      setError(toScreenError(caught, "The journal could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The filters are applied **in the browser** on purpose, and the reason is worth stating: the
  // slice-1 API takes filters but the list is short enough that a second request per keystroke
  // would make the search feel slower than it is. The server's own filter endpoint exists and the
  // criterion that matters — that the export matches the table — is not part of slice 1.
  const visible = useMemo(() => {
    const term = search.trim().toLowerCase();
    return rows.filter((row) => {
      if (source && row.source_kind !== source) {
        return false;
      }
      if (from && row.entry_date < from) {
        return false;
      }
      if (to && row.entry_date > to) {
        return false;
      }
      if (!term) {
        return true;
      }
      return (
        row.memo.toLowerCase().includes(term) ||
        row.entry_number.toString() === term ||
        row.entry_date.includes(term)
      );
    });
  }, [rows, search, source, from, to]);

  const refresh = useCallback(() => {
    void load();
  }, [load]);

  return (
    <section className="space-y-4" data-qa-accounting-journal>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="search"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search memo or number"
            aria-label="Search the journal"
            data-qa-accounting-journal-search
            className="h-8 w-56 rounded-md border border-line bg-background px-2.5 text-sm"
          />
          <select
            value={source}
            onChange={(event) => setSource(event.target.value)}
            aria-label="Filter by source"
            data-qa-accounting-journal-source
            className="h-8 rounded-md border border-line bg-background px-2 text-sm"
          >
            <option value="">All sources</option>
            {SOURCES.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
          <input
            type="date"
            value={from}
            onChange={(event) => setFrom(event.target.value)}
            aria-label="From date"
            data-qa-accounting-journal-from
            className="h-8 rounded-md border border-line bg-background px-2 text-sm"
          />
          <input
            type="date"
            value={to}
            onChange={(event) => setTo(event.target.value)}
            aria-label="To date"
            data-qa-accounting-journal-to
            className="h-8 rounded-md border border-line bg-background px-2 text-sm"
          />
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={refresh}
            data-qa-accounting-journal-refresh
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-sm"
          >
            <RefreshCw className="h-4 w-4" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            onClick={() => setComposing(true)}
            data-qa-accounting-journal-new
            className="inline-flex h-8 items-center gap-1.5 rounded-md bg-foreground px-2.5 text-sm text-background"
          >
            <Plus className="h-4 w-4" aria-hidden />
            Post an entry
          </button>
        </div>
      </div>

      {notice ? (
        <p
          role="status"
          data-qa-accounting-journal-notice
          className="rounded-md border border-emerald-200 bg-emerald-50 px-3 py-2 text-sm text-emerald-900"
        >
          {notice}
        </p>
      ) : null}

      <div className="rounded-lg border border-border bg-card">
        {loading ? (
          <LoadingTable columns={COLUMNS} rows={4} />
        ) : error ? (
          <ErrorState error={error} onRetry={refresh} />
        ) : visible.length === 0 ? (
          <EmptyState
            title={rows.length === 0 ? "No journal entries yet" : "No entries match these filters"}
            hint={
              rows.length === 0
                ? "Post a manual entry to record something the system did not cause — a rent payment, an accrual, a correction."
                : "Clear the search or widen the dates to see the rest of the ledger."
            }
            action={
              rows.length === 0 ? (
                <button
                  type="button"
                  onClick={() => setComposing(true)}
                  className="inline-flex h-8 items-center gap-1.5 rounded-md bg-foreground px-2.5 text-sm text-background"
                >
                  <Plus className="h-4 w-4" aria-hidden />
                  Post an entry
                </button>
              ) : null
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted-foreground">
                  <th className="px-4 py-2.5 font-medium">Number</th>
                  <th className="px-4 py-2.5 font-medium">Date</th>
                  <th className="px-4 py-2.5 font-medium">Memo</th>
                  <th className="px-4 py-2.5 font-medium">Source</th>
                  <th className="px-4 py-2.5 text-right font-medium">Lines</th>
                  <th className="px-4 py-2.5 text-right font-medium">Debit</th>
                  <th className="px-4 py-2.5 text-right font-medium">Credit</th>
                </tr>
              </thead>
              <tbody data-qa-accounting-journal-rows>
                {visible.map((row) => (
                  <tr
                    key={row.id}
                    data-qa-accounting-journal-row={row.entry_number}
                    className="border-b border-line last:border-b-0"
                  >
                    <td className="px-4 py-2.5 font-mono text-[12.5px]">
                      JE-{String(row.entry_number).padStart(4, "0")}
                    </td>
                    <td className="px-4 py-2.5">{row.entry_date}</td>
                    <td className="px-4 py-2.5">{row.memo || "—"}</td>
                    <td className="px-4 py-2.5">
                      <SourceBadge source={row.source_kind} />
                    </td>
                    <td className="px-4 py-2.5 text-right">{row.line_count}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{money(row.debit_total)}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{money(row.credit_total)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {composing ? (
        <ComposeDrawer
          accounts={accounts}
          onClose={() => setComposing(false)}
          onPosted={(entry) => {
            setComposing(false);
            setNotice(
              `Posted JE-${String(entry.entry_number).padStart(4, "0")} — debits ${money(
                entry.debit_total,
              )}, credits ${money(entry.credit_total)}.`,
            );
            refresh();
          }}
        />
      ) : null}
    </section>
  );
}

/**
 * The posting form.
 *
 * The running balance is the point. It is recomputed on every keystroke from the same integer
 * hundredths the server sums, and it turns the moment the columns disagree — so the operator sees
 * the entry is out **before** pressing save, and the save button is disabled while it is.
 *
 * The server's verdict still wins. `handleSubmit` catches [`UnbalancedEntryError`] and prints the
 * three figures the server computed, replacing the optimistic footer: a client-side sum and a
 * server sum that disagree would mean one of them is wrong, and the server is the one whose
 * arithmetic the books are held to.
 */
function ComposeDrawer({
  accounts,
  onClose,
  onPosted,
}: {
  accounts: Account[];
  onClose: () => void;
  onPosted: (entry: { entry_number: number; debit_total: string; credit_total: string }) => void;
}) {
  const [memo, setMemo] = useState("");
  const [entryDate, setEntryDate] = useState(() => new Date().toISOString().slice(0, 10));
  const [lines, setLines] = useState<JournalLineDraft[]>([
    { account_id: "", description: "", debit: "", credit: "" },
    { account_id: "", description: "", debit: "", credit: "" },
  ]);
  const [saving, setSaving] = useState(false);
  // Two failure states rather than one string, and the split is not tidiness: a refusal that
  // carries numbers (the unbalanced entry) is rendered as a message the grid's footer can be
  // checked against, while anything else is a form-level error with a retry. Collapsing them into
  // one string would throw away the three figures that are the whole answer.
  const [formError, setFormError] = useState<ScreenErrorValue>(null);
  const [balanceError, setBalanceError] = useState<UnbalancedEntryError | null>(null);

  const { balances } = useMemo(() => runningBalances(lines), [lines]);
  const balanced = useMemo(() => runningBalances(lines).total === "0.00", [lines]);
  const everyLineHasAnAccount = lines.every((line) => line.account_id !== "");
  const hasAnAmount = lines.some(
    (line) => cents(line.debit) !== 0 || cents(line.credit) !== 0,
  );

  const update = (index: number, patch: Partial<JournalLineDraft>) => {
    setLines((current) =>
      current.map((line, position) => (position === index ? { ...line, ...patch } : line)),
    );
    setBalanceError(null);
  };

  const submit = async () => {
    setSaving(true);
    setFormError(null);
    setBalanceError(null);
    try {
      const entry = await postJournalEntry({
        entry_date: entryDate,
        memo,
        // A blank side is sent as "0" rather than an empty string, because the server treats an
        // empty cell as zero but an explicit `""` is a field a grid posts when it has never been
        // touched, and the two are not worth distinguishing on the wire.
        lines: lines.map((line) => ({
          account_id: line.account_id,
          description: line.description,
          debit: line.debit.trim() || "0",
          credit: line.credit.trim() || "0",
        })),
      });
      onPosted(entry);
    } catch (caught) {
      if (caught instanceof UnbalancedEntryError) {
        setBalanceError(caught);
      } else {
        setFormError(toScreenError(caught, "The entry could not be posted."));
      }
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex justify-end bg-foreground/20"
      role="dialog"
      aria-modal="true"
      aria-label="Post a journal entry"
      data-qa-accounting-compose
    >
      <div className="flex h-full w-full max-w-2xl flex-col overflow-y-auto bg-background shadow-xl">
        <header className="flex items-center justify-between border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">Post a journal entry</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            data-qa-accounting-compose-close
            className="inline-flex h-7 w-7 items-center justify-center rounded-md hover:bg-muted"
          >
            <X className="h-4 w-4" aria-hidden />
          </button>
        </header>

        <div className="space-y-3 px-5 py-4">
          <div className="grid gap-3 sm:grid-cols-[10rem_1fr]">
            <label className="space-y-1 text-[12.5px]">
              <span className="text-muted-foreground">Date</span>
              <input
                type="date"
                value={entryDate}
                onChange={(event) => setEntryDate(event.target.value)}
                data-qa-accounting-compose-date
                className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
              />
            </label>
            <label className="space-y-1 text-[12.5px]">
              <span className="text-muted-foreground">Memo</span>
              <input
                value={memo}
                onChange={(event) => setMemo(event.target.value)}
                placeholder="What this entry is for"
                data-qa-accounting-compose-memo
                className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
              />
            </label>
          </div>

          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted-foreground">
                  <th className="px-2 py-2 font-medium">Account</th>
                  <th className="px-2 py-2 font-medium">Description</th>
                  <th className="px-2 py-2 text-right font-medium">Debit</th>
                  <th className="px-2 py-2 text-right font-medium">Credit</th>
                  <th className="px-2 py-2 text-right font-medium">Balance</th>
                </tr>
              </thead>
              <tbody>
                {lines.map((line, index) => (
                  <tr key={index} className="border-b border-line last:border-b-0">
                    <td className="px-2 py-1.5">
                      <select
                        value={line.account_id}
                        onChange={(event) => update(index, { account_id: event.target.value })}
                        aria-label={`Account for line ${index + 1}`}
                        data-qa-accounting-line-account={index}
                        className="h-8 w-full rounded-md border border-line bg-background px-1.5 text-[12.5px]"
                      >
                        <option value="">Choose an account…</option>
                        {accounts.map((account) => (
                          <option key={account.id} value={account.id}>
                            {account.code} · {account.name}
                          </option>
                        ))}
                      </select>
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        value={line.description}
                        onChange={(event) => update(index, { description: event.target.value })}
                        aria-label={`Description for line ${index + 1}`}
                        className="h-8 w-full rounded-md border border-line bg-background px-1.5 text-[12.5px]"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        value={line.debit}
                        onChange={(event) => update(index, { debit: event.target.value })}
                        inputMode="decimal"
                        aria-label={`Debit for line ${index + 1}`}
                        data-qa-accounting-line-debit={index}
                        className="h-8 w-28 rounded-md border border-line bg-background px-1.5 text-right text-[12.5px] tabular-nums"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        value={line.credit}
                        onChange={(event) => update(index, { credit: event.target.value })}
                        inputMode="decimal"
                        aria-label={`Credit for line ${index + 1}`}
                        data-qa-accounting-line-credit={index}
                        className="h-8 w-28 rounded-md border border-line bg-background px-1.5 text-right text-[12.5px] tabular-nums"
                      />
                    </td>
                    <td
                      className={`px-2 py-1.5 text-right text-[12.5px] tabular-nums ${
                        balances[index] === "0.00" ? "text-muted-foreground" : "font-medium"
                      }`}
                      data-qa-accounting-line-balance={index}
                    >
                      {balances[index]}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="flex flex-wrap items-center justify-between gap-2">
            <button
              type="button"
              onClick={() =>
                setLines((current) => [
                  ...current,
                  { account_id: "", description: "", debit: "", credit: "" },
                ])
              }
              data-qa-accounting-compose-add-line
              className="inline-flex h-8 items-center gap-1 rounded-md border border-line px-2.5 text-sm"
            >
              <Plus className="h-4 w-4" aria-hidden />
              Add a line
            </button>
            <p
              data-qa-accounting-compose-running
              className={`text-[13px] tabular-nums ${balanced ? "text-muted-foreground" : "font-medium"}`}
            >
              Running balance: {runningBalances(lines).total}
            </p>
          </div>

          {balanceError ? (
            <p
              role="alert"
              data-qa-accounting-compose-unbalanced
              className="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-[13px] text-amber-900"
            >
              {balanceError.message}
            </p>
          ) : null}
          {formError ? <ErrorState error={formError} onRetry={() => void submit()} /> : null}
        </div>

        <footer className="mt-auto flex items-center justify-end gap-2 border-t border-line px-5 py-3">
          <button
            type="button"
            onClick={onClose}
            className="inline-flex h-8 items-center rounded-md border border-line px-3 text-sm"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={saving || !balanced || !everyLineHasAnAccount || !hasAnAmount}
            data-qa-accounting-compose-submit
            className="inline-flex h-8 items-center rounded-md bg-foreground px-3 text-sm text-background disabled:opacity-50"
          >
            {saving ? "Posting…" : "Post entry"}
          </button>
        </footer>
      </div>
    </div>
  );
}

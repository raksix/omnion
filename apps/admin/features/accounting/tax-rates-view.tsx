"use client";

/**
 * The tax rates (REQ-054, slice 1): `/accounting/tax-rates`.
 *
 * ## The one thing this screen must make obvious
 *
 * **Editing a rate does not change an invoice somebody already sent.** That is not a promise this
 * screen makes, it is a property of the schema: an invoice line stores its own `tax_percent`
 * snapshot precisely so the rate can be corrected without rewriting history. The hint under the
 * percent field says it in the operator's own terms, because a bookkeeper who suspects the
 * opposite will not enter the correction without being told why it is safe.
 *
 * ## Why the default is a single choice and not two checkboxes
 *
 * There is exactly one default sales rate and one default purchase rate per organization, and the
 * schema enforces it with a partial unique index. The screen shows it as **one row's badge** rather
 * than a column of checkboxes, because two rows both claiming "default" is exactly what the index
 * exists to prevent, and a column of checkboxes invites the person clicking the second one to
 * believe it worked. Taking the default is a POST that demotes the previous holder in the same
 * transaction, and the badge moves.
 */
import { useCallback, useEffect, useState } from "react";
import { Check, Plus, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  TAX_RATE_KINDS,
  createTaxRate,
  fetchTaxRates,
  updateTaxRate,
  type TaxRate,
} from "@/lib/accounting";

const COLUMNS = 5;

const KIND_LABELS: Record<string, string> = Object.fromEntries(
  TAX_RATE_KINDS.map((kind) => [kind.value, kind.label]),
);

export function TaxRatesView() {
  const [rows, setRows] = useState<TaxRate[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<TaxRate | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setRows(await fetchTaxRates());
    } catch (caught) {
      setError(toScreenError(caught, "The tax rates could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const makeDefault = useCallback(
    async (rate: TaxRate) => {
      setNotice(null);
      try {
        const after = await updateTaxRate(rate.id, { is_default: true });
        setRows((current) =>
          // The demotion is the server's answer, not a local guess: asking for the default
          // clears the previous holder, and a screen that only patched the clicked row would
          // show two badges.
          current.map((row) =>
            row.kind === after.kind
              ? { ...row, is_default: row.id === after.id }
              : row,
          ),
        );
        setNotice(`${after.name} is now the default ${KIND_LABELS[after.kind].toLowerCase()} rate.`);
      } catch (caught) {
        setError(toScreenError(caught, "The default could not be changed."));
      }
    },
    [],
  );

  return (
    <section className="space-y-4" data-qa-accounting-tax-rates>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[13px] text-muted-foreground">
          {rows.length} {rows.length === 1 ? "rate" : "rates"} · exactly one default per side
        </p>
        <button
          type="button"
          onClick={() => setAdding(true)}
          data-qa-accounting-tax-rates-new
          className="inline-flex h-8 items-center gap-1.5 rounded-md bg-foreground px-2.5 text-sm text-background"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Add a rate
        </button>
      </div>

      {notice ? (
        <p
          role="status"
          data-qa-accounting-tax-rates-notice
          className="rounded-md border border-emerald-200 bg-emerald-50 px-3 py-2 text-sm text-emerald-900"
        >
          {notice}
        </p>
      ) : null}

      <div className="rounded-lg border border-border bg-card">
        {loading ? (
          <LoadingTable columns={COLUMNS} rows={4} />
        ) : error ? (
          <ErrorState error={error} onRetry={() => void load()} />
        ) : rows.length === 0 ? (
          <EmptyState
            title="No tax rates"
            hint="Every tenant is seeded with one default sales rate. Add a rate for the purchase side when the two differ."
            action={
              <button
                type="button"
                onClick={() => setAdding(true)}
                className="inline-flex h-8 items-center gap-1.5 rounded-md bg-foreground px-2.5 text-sm text-background"
              >
                <Plus className="h-4 w-4" aria-hidden />
                Add a rate
              </button>
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted-foreground">
                  <th className="px-4 py-2.5 font-medium">Name</th>
                  <th className="px-4 py-2.5 text-right font-medium">Rate</th>
                  <th className="px-4 py-2.5 font-medium">Applies to</th>
                  <th className="px-4 py-2.5 font-medium">State</th>
                  <th className="px-4 py-2.5 text-right font-medium">Actions</th>
                </tr>
              </thead>
              <tbody data-qa-accounting-tax-rates-rows>
                {rows.map((rate) => (
                  <tr
                    key={rate.id}
                    data-qa-accounting-tax-rate={rate.name}
                    className="border-b border-line last:border-b-0"
                  >
                    <td className="px-4 py-2.5">{rate.name}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{rate.percent}%</td>
                    <td className="px-4 py-2.5">{KIND_LABELS[rate.kind] ?? rate.kind}</td>
                    <td className="px-4 py-2.5">
                      {rate.is_default ? (
                        <span
                          data-qa-accounting-tax-rate-default={rate.name}
                          className="inline-block rounded border border-emerald-300 bg-emerald-50 px-1.5 py-0.5 text-[11.5px] text-emerald-900"
                        >
                          Default
                        </span>
                      ) : rate.active ? (
                        <span className="text-[12px] text-muted-foreground">Active</span>
                      ) : (
                        <span className="text-[12px] text-amber-800">Closed</span>
                      )}
                    </td>
                    <td className="px-4 py-2.5 text-right">
                      <div className="inline-flex items-center gap-1">
                        <button
                          type="button"
                          onClick={() => setEditing(rate)}
                          data-qa-accounting-tax-rate-edit={rate.name}
                          className="h-7 rounded-md border border-line px-2 text-[12px]"
                        >
                          Edit
                        </button>
                        {rate.is_default ? null : (
                          <button
                            type="button"
                            onClick={() => void makeDefault(rate)}
                            data-qa-accounting-tax-rate-make-default={rate.name}
                            className="inline-flex h-7 items-center gap-1 rounded-md border border-line px-2 text-[12px]"
                          >
                            <Check className="h-3.5 w-3.5" aria-hidden />
                            Make default
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {adding ? (
        <RateDrawer
          onClose={() => setAdding(false)}
          onSaved={(rate) => {
            setAdding(false);
            setNotice(`${rate.name} at ${rate.percent}% was added.`);
            void load();
          }}
        />
      ) : null}

      {editing ? (
        <RateDrawer
          rate={editing}
          onClose={() => setEditing(null)}
          onSaved={(rate) => {
            setEditing(null);
            setRows((current) => current.map((row) => (row.id === rate.id ? rate : row)));
            setNotice(`${rate.name} is now ${rate.percent}%. Issued invoices keep the rate they were sent with.`);
          }}
        />
      ) : null}
    </section>
  );
}

/** The add and edit form. One component for both, because a rate is four fields and two of them
 *  are the same question ("what is it called, how much is it") in either direction. */
function RateDrawer({
  rate,
  onClose,
  onSaved,
}: {
  rate?: TaxRate;
  onClose: () => void;
  onSaved: (rate: TaxRate) => void;
}) {
  const [name, setName] = useState(rate?.name ?? "");
  const [percent, setPercent] = useState(rate?.percent ?? "");
  const [kind, setKind] = useState(rate?.kind ?? TAX_RATE_KINDS[0].value);
  const [isDefault, setIsDefault] = useState(rate?.is_default ?? false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const submit = async () => {
    setSaving(true);
    setError(null);
    try {
      if (rate) {
        onSaved(
          await updateTaxRate(rate.id, {
            name: name.trim(),
            percent: percent.trim(),
            is_default: isDefault,
          }),
        );
      } else {
        onSaved(
          await createTaxRate({
            name: name.trim(),
            percent: percent.trim(),
            kind,
            is_default: isDefault,
          }),
        );
      }
    } catch (caught) {
      setError(toScreenError(caught, "The rate could not be saved."));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex justify-end bg-foreground/20"
      role="dialog"
      aria-modal="true"
      aria-label={rate ? "Edit a tax rate" : "Add a tax rate"}
      data-qa-accounting-tax-rate-drawer
    >
      <div className="flex h-full w-full max-w-lg flex-col overflow-y-auto bg-background shadow-xl">
        <header className="flex items-center justify-between border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">
            {rate ? "Edit a tax rate" : "Add a tax rate"}
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            data-qa-accounting-tax-rate-close
            className="inline-flex h-7 w-7 items-center justify-center rounded-md hover:bg-muted"
          >
            <X className="h-4 w-4" aria-hidden />
          </button>
        </header>

        <div className="space-y-3 px-5 py-4">
          <label className="block space-y-1 text-[12.5px]">
            <span className="text-muted-foreground">Name</span>
            <input
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="Standard VAT"
              data-qa-accounting-tax-rate-name
              className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
            />
          </label>
          <label className="block space-y-1 text-[12.5px]">
            <span className="text-muted-foreground">Rate, as a number of percent</span>
            <input
              value={percent}
              onChange={(event) => setPercent(event.target.value)}
              inputMode="decimal"
              placeholder="20"
              data-qa-accounting-tax-rate-percent
              className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
            />
            <span className="text-[11.5px] text-muted-foreground">
              Enter 20 for twenty percent, not 0.2. Changing this does not alter an invoice that
              has already been sent — each line keeps the rate it was issued with.
            </span>
          </label>
          {rate ? null : (
            <label className="block space-y-1 text-[12.5px]">
              <span className="text-muted-foreground">Applies to</span>
              <select
                value={kind}
                onChange={(event) => setKind(event.target.value)}
                data-qa-accounting-tax-rate-kind
                className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
              >
                {TAX_RATE_KINDS.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
              <span className="text-[11.5px] text-muted-foreground">
                An organization that sells at one rate and buys at another keeps two. The side is
                fixed once the rate exists.
              </span>
            </label>
          )}
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={isDefault}
              onChange={(event) => setIsDefault(event.target.checked)}
              data-qa-accounting-tax-rate-default
              className="h-4 w-4"
            />
            <span>Use this as the default for its side</span>
          </label>
          {error ? <ErrorState error={error} onRetry={() => void submit()} /> : null}
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
            disabled={saving || !name.trim() || !percent.trim()}
            data-qa-accounting-tax-rate-submit
            className="inline-flex h-8 items-center rounded-md bg-foreground px-3 text-sm text-background disabled:opacity-50"
          >
            {saving ? "Saving…" : rate ? "Save rate" : "Add rate"}
          </button>
        </footer>
      </div>
    </div>
  );
}

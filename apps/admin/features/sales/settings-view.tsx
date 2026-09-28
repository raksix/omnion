"use client";

/**
 * The sales settings (REQ-052, slice 1): `/sales/settings`.
 *
 * Five values, and every one of them changes what a **new** quote looks like without changing any
 * existing one — which is the property the screen has to make obvious, because "I changed the
 * currency and last month's quotes changed too" is the fear that stops anybody editing settings at
 * all. A quote's currency and its numbers are snapshotted on the line when it is written; these are
 * the defaults the *next* one starts from.
 *
 * The discount threshold is the one an operator changes with some care, so the screen says what it
 * will do: a quote whose largest line discount is **over** it needs a manager. Exactly at the
 * threshold is inside policy, and a person who cannot tell which side of that line they are on
 * will set it a point too low and then wonder why a quote needs approval.
 */
import { useEffect, useState } from "react";

import { Loader2, Save } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  fetchSalesSettings,
  fieldOf,
  saveSalesSettings,
  type SalesSettings,
} from "@/lib/sales";

import { useSales } from "./sales-parts";

/** The form's own copy, so an unsaved edit is visible as an unsaved edit. */
type Draft = {
  currency: string;
  discount_approval_threshold: string;
  quote_validity_days: string;
  quote_number_prefix: string;
  order_number_prefix: string;
};

function draftOf(settings: SalesSettings): Draft {
  return {
    currency: settings.currency,
    discount_approval_threshold: String(settings.discount_approval_threshold),
    quote_validity_days: String(settings.quote_validity_days),
    quote_number_prefix: settings.quote_number_prefix,
    order_number_prefix: settings.order_number_prefix,
  };
}

/** `/sales/settings`. */
export function SalesSettingsView() {
  const { organizationId } = useSales();

  const [settings, setSettings] = useState<SalesSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [formField, setFormField] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    setError(null);
    fetchSalesSettings(organizationId)
      .then((loaded) => {
        setSettings(loaded);
        setDraft(draftOf(loaded));
      })
      .catch((problem) => setError(toScreenError(problem, "The sales settings could not be loaded.")));
  }, [organizationId, reloadToken]);

  const save = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!draft) return;
    setSaving(true);
    setFormError(null);
    setFormField(null);
    setNotice(null);
    try {
      const saved = await saveSalesSettings(
        {
          currency: draft.currency.trim().toUpperCase(),
          discount_approval_threshold: Number(draft.discount_approval_threshold),
          quote_validity_days: Number(draft.quote_validity_days),
          quote_number_prefix: draft.quote_number_prefix.trim(),
          order_number_prefix: draft.order_number_prefix.trim(),
        },
        organizationId,
      );
      setSettings(saved);
      setDraft(draftOf(saved));
      setNotice("Saved. Quotes already written keep the currency and the totals they were issued with.");
    } catch (problem) {
      setFormError(problem instanceof Error ? problem.message : "The settings could not be saved.");
      setFormField(fieldOf(problem));
    } finally {
      setSaving(false);
    }
  };

  if (error) {
    return <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />;
  }
  if (!settings || !draft) {
    return <LoadingTable columns={2} rows={5} />;
  }

  const dirty = JSON.stringify(draftOf(settings)) !== JSON.stringify(draft);
  const fieldError = (name: string) => (formField === name ? formError : null);

  return (
    <div className="max-w-2xl">
      <p className="mb-4 text-[12.5px] text-muted">
        These are the defaults a <strong>new</strong> quote starts from. A quote that has been
        written keeps the currency, the totals and the validity its lines were saved with, so
        changing a value here never rewrites a document that already exists.
      </p>

      {notice ? (
        <p data-qa-sales-notice className="mb-3 rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      <form onSubmit={save} data-qa-sales-settings-form className="rounded-lg border border-line bg-panel p-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Default currency" name="currency" error={fieldError("currency")}>
            <input
              value={draft.currency}
              onChange={(event) => setDraft({ ...draft, currency: event.target.value })}
              data-qa-sales-field="currency"
              maxLength={3}
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] uppercase outline-none focus:border-ink-soft"
            />
          </Field>

          <Field
            label="Quote validity (days)"
            name="quote_validity_days"
            error={fieldError("quote_validity_days")}
            hint="How long a new quote stays open before it is marked expired."
          >
            <input
              type="number"
              min="1"
              step="1"
              value={draft.quote_validity_days}
              onChange={(event) => setDraft({ ...draft, quote_validity_days: event.target.value })}
              data-qa-sales-field="quote_validity_days"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </Field>

          <Field
            label="Discount needing approval (%)"
            name="discount_approval_threshold"
            error={fieldError("discount_approval_threshold")}
            hint="A quote with a line discounted more than this needs a manager. Exactly this much is still inside policy."
          >
            <input
              type="number"
              min="0"
              max="100"
              step="0.5"
              value={draft.discount_approval_threshold}
              onChange={(event) => setDraft({ ...draft, discount_approval_threshold: event.target.value })}
              data-qa-sales-field="discount_approval_threshold"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </Field>

          <div className="sm:col-span-2">
            <p className="mb-2 text-[12.5px] text-muted">Number prefixes</p>
            <div className="grid gap-3 sm:grid-cols-2">
              <Field label="Quotes" name="quote_number_prefix" error={fieldError("quote_number_prefix")}>
                <input
                  value={draft.quote_number_prefix}
                  onChange={(event) => setDraft({ ...draft, quote_number_prefix: event.target.value })}
                  data-qa-sales-field="quote_number_prefix"
                  className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
                />
                <span className="mt-1 block text-[11.5px] text-muted">
                  The next quote will be {draft.quote_number_prefix || "Q"}-2026-0001.
                </span>
              </Field>
              <Field label="Orders" name="order_number_prefix" error={fieldError("order_number_prefix")}>
                <input
                  value={draft.order_number_prefix}
                  onChange={(event) => setDraft({ ...draft, order_number_prefix: event.target.value })}
                  data-qa-sales-field="order_number_prefix"
                  className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
                />
                <span className="mt-1 block text-[11.5px] text-muted">
                  The next order will be {draft.order_number_prefix || "SO"}-2026-0001.
                </span>
              </Field>
            </div>
          </div>
        </div>

        {formError && !formField ? (
          <p data-qa-sales-form-error className="mt-3 rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative">
            {formError}
          </p>
        ) : null}

        <div className="mt-4 flex items-center gap-2">
          <button
            type="submit"
            disabled={saving || !dirty}
            data-qa-sales-save-settings
            className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
          >
            {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Save className="h-3.5 w-3.5" aria-hidden />}
            Save the settings
          </button>
          {dirty ? <span className="text-[12px] text-muted">Unsaved changes</span> : null}
        </div>
      </form>
    </div>
  );
}

/** A labelled input with its own hint and its own refusal. */
function Field({
  label,
  name,
  error,
  hint,
  children,
}: {
  label: string;
  name: string;
  error: string | null;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <label className="block text-[12.5px]">
      <span className="mb-1 block text-muted">{label}</span>
      {children}
      {hint && !error ? <span className="mt-1 block text-[11.5px] text-muted">{hint}</span> : null}
      {error ? (
        <span data-qa-sales-field-error={name} className="mt-1 block text-[11.5px] text-negative">
          {error}
        </span>
      ) : null}
    </label>
  );
}

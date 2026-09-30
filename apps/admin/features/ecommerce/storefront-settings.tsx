/**
 * `/commerce/storefront` — the per-site shop configuration (REQ-118, slice 1a).
 *
 * Twelve of REQ-118's acceptance lines end in a number a customer sees, and every one of them
 * is a field on this form. That makes the screen unusual: it is not a list of records to
 * browse but a **set of numbers the public site is currently serving**, and the only honest
 * thing to put on it is what the server actually answered.
 *
 * ## Three decisions the screen makes visible
 *
 * 1. **A site nobody configured is shown as unconfigured, not as a set of defaults.** The API
 *    answers `configured: false` alongside platform defaults, and the screen renders that as a
 *    banner rather than filling the form with 24/24/20 and letting the operator believe they
 *    chose them. Without the flag the two are the same pixels — a shop that never opened this
 *    page and a shop whose operator typed 24 look identical, and "who set this" is a question
 *    every configuration screen eventually has to answer.
 * 2. **The vocabulary comes from the server, not from this file.** `/vocabulary` returns the
 *    closed lists *and* the numeric bands, so a `<select>` cannot offer a value the database's
 *    `check` constraint refuses and a number input's `min`/`max` are the same numbers the
 *    server validates against. The lists are written three times in this feature (crate,
 *    migration, screen) and this file is deliberately not one of the places where they can
 *    drift — it renders what it is handed.
 * 3. **A field error lands under its own input.** The API answers
 *    `{"field": "page_size", "message": "…"}`, and the screen renders that message beneath the
 *    control named by `field`. This is the acceptance line: *"raising the per-order maximum
 *    changes the stepper cap"* — a cap that cannot be raised with a legible error is a cap
 *    nobody raises.
 *
 * ## What the screen deliberately does not do
 *
 * It does not compute what the public site will look like. The screen shows the *stored* row
 * and the server's derived answers (`quantity_cap`, `shows_tax_inclusive`) side by side with
 * what the operator typed; it never renders a preview from its own arithmetic. A preview that
 * disagreed with the storefront by one rounding rule would be a support ticket with a
 * screenshot on it.
 */

"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { ApiError, request } from "@/lib/api";

/** One site's configuration, exactly as the API shapes it. */
interface StorefrontSettings {
  site_id: string;
  guest_checkout: boolean;
  tax_display: string;
  listing_variant: string;
  page_size: number;
  pagination: string;
  per_order_item_max: number;
  wishlist_enabled: boolean;
  low_stock_badge_threshold: number;
  abandonment_hours: number;
  confirmation_template: string;
  currency: string;
  /** Whether a row backs these values, or they are the platform defaults. */
  configured: boolean;
  quantity_cap: number;
  shows_tax_inclusive: boolean;
}

/** The closed lists and the numeric bands, as the server states them. */
interface StorefrontVocabulary {
  tax_display: string[];
  pagination: string[];
  listing_variant: string[];
  page_size_min: number;
  page_size_max: number;
  per_order_item_max_min: number;
  per_order_item_max_max: number;
  low_stock_badge_threshold_min: number;
  low_stock_badge_threshold_max: number;
  abandonment_hours_min: number;
  abandonment_hours_max: number;
}

/** The editable shape — the row without the server-derived answers. */
type Draft = Omit<
  StorefrontSettings,
  "configured" | "quantity_cap" | "shows_tax_inclusive"
>;

/** A validation refusal that names the control it belongs to. */
interface FieldError {
  field: string;
  message: string;
}

/**
 * Every field the save body carries — the contract the `PUT` has to satisfy.
 *
 * It was `DRAFT_FIELDS`, private, read by nothing but a re-export that also had no
 * reader, under a comment claiming the naming "keeps the two honest". It kept nothing
 * honest: `keyof Draft` makes the list *type-check* against the shape, which catches a
 * key that does not exist and nothing else, so a field added to `StorefrontSettings` and
 * forgotten here compiled green, rendered no control, and was never edited. The half a
 * developer actually adds is the one the type system cannot see.
 *
 * So the completeness half is asserted at the type level, below, and the assertion
 * reads THIS constant — which is what gives the export the caller it never had.
 */
export const STOREFRONT_DRAFT_FIELDS = [
  "site_id",
  "guest_checkout",
  "tax_display",
  "listing_variant",
  "page_size",
  "pagination",
  "per_order_item_max",
  "wishlist_enabled",
  "low_stock_badge_threshold",
  "abandonment_hours",
  "confirmation_template",
  "currency",
] as const satisfies readonly (keyof Draft)[];

/** A key of `Draft` this list does not name: the type is a union of them, so any is an error. */
type UncoveredDraftField = Exclude<keyof Draft, (typeof STOREFRONT_DRAFT_FIELDS)[number]>;

/** A key this list names that is not a field of `Draft` — the opposite drift. */
type UnknownDraftField = Exclude<
  (typeof STOREFRONT_DRAFT_FIELDS)[number],
  keyof Draft
>;

/** Compiles only when both are `never`; otherwise the error names every offending key. */
type AssertNone<T extends never> = T;
export type DraftFieldsAreComplete = AssertNone<UncoveredDraftField>;
export type DraftFieldsAreKnown = AssertNone<UnknownDraftField>;


/** `inclusive` → `Prices include tax`. The label a customer would read. */
function taxLabel(value: string): string {
  return value === "inclusive" ? "Prices include tax" : "Prices exclude tax";
}

/** `pagination` → `Pagination links`. Same reason. */
function paginationLabel(value: string): string {
  switch (value) {
    case "load_more":
      return "Load more";
    case "infinite":
      return "Infinite scroll";
    default:
      return "Pagination links";
  }
}

/** `grid` → `Grid`. */
function listingLabel(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}

/**
 * Read the offending field out of an API error.
 *
 * The endpoint answers `.with_details({field})` for a validation refusal, so this is the
 * server naming the control rather than the screen guessing which one a message belongs to.
 * Anything else (a network failure, a 500) is a page-level error, and rendering it under an
 * arbitrary input would be a lie about where it came from.
 */
function fieldOf(caught: unknown): FieldError | null {
  if (!(caught instanceof ApiError)) return null;
  const details = caught.details as { field?: string } | undefined;
  if (!details || typeof details.field !== "string") return null;
  return { field: details.field, message: caught.message };
}

export function StorefrontSettingsScreen() {
  const [sites, setSites] = useState<StorefrontSettings[] | null>(null);
  const [vocabulary, setVocabulary] = useState<StorefrontVocabulary | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<FieldError | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [dirty, setDirty] = useState(false);
  const headingRef = useRef<HTMLHeadingElement>(null);

  const load = useCallback(async () => {
    try {
      const [rows, words] = await Promise.all([
        request<StorefrontSettings[]>("/api/v1/commerce/storefront"),
        request<StorefrontVocabulary>("/api/v1/commerce/storefront/vocabulary"),
      ]);
      setSites(rows);
      setVocabulary(words);
      setError(null);
      return rows;
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
      setSites([]);
      return [];
    }
  }, []);

  useEffect(() => {
    void load().then((rows) => {
      if (rows.length > 0) {
        setSelected(rows[0].site_id);
      }
    });
  }, [load]);

  // The form follows the selection rather than keeping two sources of truth: picking another
  // shop re-seeds the draft from the row the server just answered, so a half-typed edit is
  // never carried onto somebody else's storefront by accident.
  const current = useMemo(
    () => sites?.find((row) => row.site_id === selected) ?? null,
    [sites, selected],
  );

  useEffect(() => {
    if (!current) return;
    const next: Draft = {
      site_id: current.site_id,
      guest_checkout: current.guest_checkout,
      tax_display: current.tax_display,
      listing_variant: current.listing_variant,
      page_size: current.page_size,
      pagination: current.pagination,
      per_order_item_max: current.per_order_item_max,
      wishlist_enabled: current.wishlist_enabled,
      low_stock_badge_threshold: current.low_stock_badge_threshold,
      abandonment_hours: current.abandonment_hours,
      confirmation_template: current.confirmation_template,
      currency: current.currency,
    };
    setDraft(next);
    setFieldError(null);
    setSaved(null);
    setDirty(false);
  }, [current?.site_id, current?.configured, sites]);

  const choose = (siteId: string) => {
    if (dirty && !window.confirm("Discard the unsaved changes on this shop?")) return;
    setSelected(siteId);
  };

  const set = <K extends keyof Draft>(key: K, value: Draft[K]) => {
    setDraft((previous) => (previous ? { ...previous, [key]: value } : previous));
    setDirty(true);
    setSaved(null);
    // Editing the field an error names clears that error: the message described the value that
    // was there, and leaving it under a control the operator has just fixed reads as a refusal
    // that survived the correction.
    setFieldError((previous) => (previous?.field === key ? null : previous));
  };

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    try {
      await request<StorefrontSettings>(`/api/v1/commerce/storefront/${draft.site_id}`, {
        method: "PUT",
        body: JSON.stringify(draft),
      });
      await load();
      setFieldError(null);
      setDirty(false);
      setSaved("Saved. The public site serves these values from the next request.");
    } catch (caught) {
      const named = fieldOf(caught);
      if (named) setFieldError(named);
      else setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  // `1` and `2` switch shop, `s` saves, `Escape` returns to the top of the form. The digits are
  // why the shortcut list is rendered next to them: a keyboard shortcut nobody can discover is
  // a shortcut the acceptance criteria cannot be met with.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT");
      if (typing) return;
      if (event.key === "Escape") {
        headingRef.current?.focus();
        return;
      }
      if (event.key === "s" && draft && dirty) {
        event.preventDefault();
        void save();
      }
      const index = Number.parseInt(event.key, 10) - 1;
      if (Number.isInteger(index) && sites && sites[index]) choose(sites[index].site_id);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [sites, draft, dirty]);

  const loading = sites === null;
  const empty = sites !== null && sites.length === 0;

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2
            ref={headingRef}
            tabIndex={-1}
            className="text-[15px] font-medium text-ink outline-none"
          >
            Storefront
          </h2>
          <p className="mt-0.5 max-w-prose text-[12.5px] text-muted">
            How the public shop behaves for each of your sites. These values are read on every
            request that shows a number to a customer, so a change is live the moment it
            saves.
          </p>
        </div>
        {draft ? (
          <div className="flex items-center gap-2">
            <span className="text-[11.5px] text-muted" data-testid="storefront-dirty">
              {dirty ? "unsaved" : "no changes"}
            </span>
            <button
              type="button"
              disabled={busy || !dirty}
              onClick={() => void save()}
              data-testid="storefront-save"
              className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-paper disabled:opacity-50"
            >
              Save
            </button>
          </div>
        ) : null}
      </header>

      {error ? (
        <p
          role="alert"
          data-testid="storefront-error"
          className="rounded-md border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
        >
          {error}
        </p>
      ) : null}

      {loading ? (
        <div className="space-y-2" aria-busy="true" data-testid="storefront-skeleton">
          <div className="h-9 w-full animate-pulse rounded-md border border-line bg-line/30" />
          <div className="h-64 w-full animate-pulse rounded-md border border-line bg-line/30" />
        </div>
      ) : empty ? (
        <p
          data-testid="storefront-empty"
          className="rounded-md border border-line px-3 py-6 text-center text-[12.5px] text-muted"
        >
          No sites yet. A storefront belongs to a site — create one under Sites and it appears
          here with the platform defaults.
        </p>
      ) : (
        <>
          <nav aria-label="Sites" data-testid="storefront-sites">
            <ul className="flex flex-wrap gap-2">
              {(sites ?? []).map((row, index) => (
                <li key={row.site_id}>
                  <button
                    type="button"
                    onClick={() => choose(row.site_id)}
                    aria-pressed={row.site_id === selected}
                    data-testid={`storefront-site-${row.site_id.slice(0, 8)}`}
                    className={`rounded-md border px-2.5 py-1 text-[12px] ${
                      row.site_id === selected
                        ? "border-ink bg-ink text-paper"
                        : "border-line text-ink"
                    }`}
                  >
                    {row.site_id.slice(0, 8)}
                    <span className="ml-1.5 opacity-70">{index + 1}</span>
                    {row.configured ? null : (
                      <span className="ml-1.5 opacity-70">·defaults</span>
                    )}
                  </button>
                </li>
              ))}
            </ul>
          </nav>

          {current && !current.configured ? (
            <p
              data-testid="storefront-unconfigured"
              className="rounded-md border border-line bg-line/20 px-3 py-2 text-[12.5px] text-muted"
            >
              This site has no saved storefront settings. The values below are the platform
              defaults the public site is serving right now.
            </p>
          ) : null}

          {saved ? (
            <p
              role="status"
              data-testid="storefront-saved"
              className="rounded-md border border-ok/40 bg-ok/5 px-3 py-2 text-[12.5px] text-ok"
            >
              {saved}
            </p>
          ) : null}

          {current && draft ? (
            <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_16rem]">
              <form
                className="space-y-6"
                onSubmit={(event) => {
                  event.preventDefault();
                  void save();
                }}
              >
                <Section title="Checkout" hint="What a visitor is asked for before paying.">
                  <Toggle
                    id="guest_checkout"
                    label="Guest checkout"
                    hint="Off forces the sign-in step; the cart is kept through it."
                    checked={draft.guest_checkout}
                    onChange={(value) => set("guest_checkout", value)}
                  />
                  <Choice
                    id="tax_display"
                    label="Tax display"
                    hint="Changes the wording and the presentation only. The stored amount is identical either way."
                    value={draft.tax_display}
                    options={(vocabulary?.tax_display ?? []).map((value) => ({
                      value,
                      label: taxLabel(value),
                    }))}
                    error={fieldError?.field === "tax_display" ? fieldError.message : null}
                    onChange={(value) => set("tax_display", value)}
                  />
                  <Text
                    id="confirmation_template"
                    label="Confirmation template"
                    hint="Mail template reference used for the order confirmation. Blank falls back to the platform default."
                    value={draft.confirmation_template}
                    error={fieldError?.field === "confirmation_template" ? fieldError.message : null}
                    onChange={(value) => set("confirmation_template", value)}
                  />
                </Section>

                <Section title="Catalogue" hint="How a listing page is laid out and cut up.">
                  <Choice
                    id="listing_variant"
                    label="Listing variant"
                    value={draft.listing_variant}
                    options={(vocabulary?.listing_variant ?? []).map((value) => ({
                      value,
                      label: listingLabel(value),
                    }))}
                    error={fieldError?.field === "listing_variant" ? fieldError.message : null}
                    onChange={(value) => set("listing_variant", value)}
                  />
                  <NumberField
                    id="page_size"
                    label="Products per page"
                    value={draft.page_size}
                    min={vocabulary?.page_size_min}
                    max={vocabulary?.page_size_max}
                    error={fieldError?.field === "page_size" ? fieldError.message : null}
                    onChange={(value) => set("page_size", value)}
                  />
                  <Choice
                    id="pagination"
                    label="Pagination"
                    hint="An infinite list is also reachable as a real URL."
                    value={draft.pagination}
                    options={(vocabulary?.pagination ?? []).map((value) => ({
                      value,
                      label: paginationLabel(value),
                    }))}
                    error={fieldError?.field === "pagination" ? fieldError.message : null}
                    onChange={(value) => set("pagination", value)}
                  />
                </Section>

                <Section title="Cart &amp; stock" hint="The numbers a stepper and a badge obey.">
                  <NumberField
                    id="per_order_item_max"
                    label="Maximum quantity per order line"
                    hint={`The stepper offers at most ${current.quantity_cap}.`}
                    value={draft.per_order_item_max}
                    min={vocabulary?.per_order_item_max_min}
                    max={vocabulary?.per_order_item_max_max}
                    error={fieldError?.field === "per_order_item_max" ? fieldError.message : null}
                    onChange={(value) => set("per_order_item_max", value)}
                  />
                  <NumberField
                    id="low_stock_badge_threshold"
                    label="Low-stock badge threshold"
                    hint="A card badges at or below this many left."
                    value={draft.low_stock_badge_threshold}
                    min={vocabulary?.low_stock_badge_threshold_min}
                    max={vocabulary?.low_stock_badge_threshold_max}
                    error={
                      fieldError?.field === "low_stock_badge_threshold"
                        ? fieldError.message
                        : null
                    }
                    onChange={(value) => set("low_stock_badge_threshold", value)}
                  />
                  <Toggle
                    id="wishlist_enabled"
                    label="Wishlist"
                    hint="Off hides the wishlist everywhere for this site."
                    checked={draft.wishlist_enabled}
                    onChange={(value) => set("wishlist_enabled", value)}
                  />
                </Section>

                <Section title="Abandoned carts" hint="When a cart counts as abandoned.">
                  <NumberField
                    id="abandonment_hours"
                    label="Window (hours)"
                    hint="At least 1: a zero window abandons a basket the moment it is created."
                    value={draft.abandonment_hours}
                    min={vocabulary?.abandonment_hours_min}
                    max={vocabulary?.abandonment_hours_max}
                    error={fieldError?.field === "abandonment_hours" ? fieldError.message : null}
                    onChange={(value) => set("abandonment_hours", value)}
                  />
                </Section>

                <Section title="Currency" hint="Used in every stored amount on this site.">
                  <Text
                    id="currency"
                    label="ISO 4217 code"
                    value={draft.currency}
                    maxLength={3}
                    error={fieldError?.field === "currency" ? fieldError.message : null}
                    onChange={(value) => set("currency", value)}
                  />
                </Section>

                <button type="submit" disabled={busy || !dirty} className="sr-only">
                  Save
                </button>
              </form>

              <aside className="space-y-3" data-testid="storefront-summary">
                <div className="rounded-md border border-line px-3 py-2.5">
                  <h3 className="text-[12px] font-medium text-ink">What the site serves now</h3>
                  <dl className="mt-2 space-y-1 text-[11.5px] text-muted">
                    <Summary label="Price wording" value={taxLabel(current.tax_display)} />
                    <Summary label="Stepper cap" value={String(current.quantity_cap)} />
                    <Summary
                      label="Low stock at"
                      value={`${current.low_stock_badge_threshold} or fewer`}
                    />
                    <Summary
                      label="Abandoned after"
                      value={`${current.abandonment_hours} h`}
                    />
                    <Summary label="Currency" value={current.currency} />
                    <Summary
                      label="Checkout"
                      value={current.guest_checkout ? "guest allowed" : "sign-in required"}
                    />
                    <Summary
                      label="Wishlist"
                      value={current.wishlist_enabled ? "offered" : "hidden"}
                    />
                  </dl>
                  <p className="mt-2 text-[11px] text-muted">
                    Read back from the server, not recomputed here.
                  </p>
                </div>
                <div className="rounded-md border border-line px-3 py-2.5 text-[11.5px] text-muted">
                  <p className="text-[12px] font-medium text-ink">Keyboard</p>
                  <ul className="mt-1 space-y-0.5">
                    <li>
                      <Key>1</Key>–<Key>{sites?.length ?? 0}</Key> switch shop
                    </li>
                    <li>
                      <Key>s</Key> save
                    </li>
                    <li>
                      <Key>Esc</Key> back to the top
                    </li>
                  </ul>
                </div>
              </aside>
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint: string;
  children: React.ReactNode;
}) {
  return (
    <fieldset className="space-y-3 rounded-md border border-line px-3 py-3">
      <legend className="px-1 text-[12.5px] font-medium text-ink">{title}</legend>
      <p className="-mt-1 text-[11.5px] text-muted">{hint}</p>
      {children}
    </fieldset>
  );
}

function Summary({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt>{label}</dt>
      <dd className="text-right text-ink" data-testid={`storefront-summary-${label.toLowerCase().replace(/[^a-z]+/g, "-")}`}>
        {value}
      </dd>
    </div>
  );
}

function Key({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="rounded border border-line px-1 py-0.5 font-mono text-[10.5px] text-ink">
      {children}
    </kbd>
  );
}

function FieldFrame({
  id,
  label,
  hint,
  error,
  children,
}: {
  id: string;
  label: string;
  hint?: string;
  error: string | null;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-[12.5px] text-ink">
        {label}
      </label>
      {hint ? <p className="text-[11.5px] text-muted">{hint}</p> : null}
      {children}
      {error ? (
        <p
          role="alert"
          data-testid={`${id}-error`}
          className="text-[11.5px] text-danger"
        >
          {error}
        </p>
      ) : null}
    </div>
  );
}

const CONTROL =
  "w-full rounded-md border border-line bg-paper px-2 py-1.5 text-[12.5px] text-ink";

function Toggle({
  id,
  label,
  hint,
  checked,
  onChange,
}: {
  id: string;
  label: string;
  hint: string;
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  return (
    <FieldFrame id={id} label={label} hint={hint} error={null}>
      <label className="inline-flex items-center gap-2 text-[12.5px] text-ink">
        <input
          id={id}
          type="checkbox"
          checked={checked}
          data-testid={id}
          onChange={(event) => onChange(event.target.checked)}
          className="h-3.5 w-3.5"
        />
        {checked ? "on" : "off"}
      </label>
    </FieldFrame>
  );
}

function Choice({
  id,
  label,
  hint,
  value,
  options,
  error,
  onChange,
}: {
  id: string;
  label: string;
  hint?: string;
  value: string;
  options: { value: string; label: string }[];
  error: string | null;
  onChange: (value: string) => void;
}) {
  return (
    <FieldFrame id={id} label={label} hint={hint} error={error}>
      <select
        id={id}
        value={value}
        data-testid={id}
        onChange={(event) => onChange(event.target.value)}
        className={CONTROL}
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    </FieldFrame>
  );
}

function NumberField({
  id,
  label,
  hint,
  value,
  min,
  max,
  error,
  onChange,
}: {
  id: string;
  label: string;
  hint?: string;
  value: number;
  min?: number;
  max?: number;
  error: string | null;
  onChange: (value: number) => void;
}) {
  return (
    <FieldFrame id={id} label={label} hint={hint} error={error}>
      <input
        id={id}
        type="number"
        value={value}
        min={min}
        max={max}
        data-testid={id}
        onChange={(event) => onChange(Number.parseInt(event.target.value, 10) || 0)}
        className={`${CONTROL} w-28`}
      />
    </FieldFrame>
  );
}

function Text({
  id,
  label,
  hint,
  value,
  error,
  maxLength,
  onChange,
}: {
  id: string;
  label: string;
  hint?: string;
  value: string;
  error: string | null;
  maxLength?: number;
  onChange: (value: string) => void;
}) {
  return (
    <FieldFrame id={id} label={label} hint={hint} error={error}>
      <input
        id={id}
        type="text"
        value={value}
        maxLength={maxLength}
        data-testid={id}
        onChange={(event) => onChange(event.target.value)}
        className={CONTROL}
      />
    </FieldFrame>
  );
}

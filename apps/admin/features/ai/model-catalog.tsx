"use client";

/**
 * The model catalog (docs/requests/REQ-098, slice 1).
 *
 * A list of models is a list of *facts an operator acts on*, not a list of names: what a model
 * can do, how much it costs, how old that price is and whether anything will ever ask for it.
 * The screen is built around those four answers, and the one rule it is careful about is that it
 * never shows a number the platform does not actually have:
 *
 * - A model with no price says **no price**, not `0`. Free is a claim.
 * - A model with only an input price says **half known** and leaves the output cell empty, rather
 *   than showing a zero that would make a cost estimate built from this row too small.
 * - A price nobody has revisited in ninety days says how old it is, because a hand-typed figure
 *   presented without a date reads as current no matter how old it is.
 *
 * The narrowing (search, capability chips, provider, status, sort) is **server-side**: the API
 * owns what a query means, so the table and the endpoint can never disagree about which rows a
 * filter selects. A failed request is an error banner with a retry, never an empty table — an
 * empty table says "you have nothing", and that is a different, wrong thing to tell somebody
 * whose registry has forty models.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { ArrowDownUp, Copy, Loader2, Power, Search, Star, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiCapability,
  type AiModel,
  type AiModelPrice,
  type AiModelQuery,
  type AiProvider,
  fetchAiModels,
  updateAiModel,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/**
 * Format one price half for the table.
 *
 * Micros are the currency's smallest unit, so the number is shown as a plain integer with a
 * currency mark rather than as a decimal: at per-1K rates the decimal places an operator cares
 * about are in the fourth digit, and `0.000015` in a table cell is unreadable where `15 µ` is
 * not.
 */
function priceCell(micros: number | null): string {
  return micros === null ? "—" : `${micros.toLocaleString("en-US")} µ`;
}

/** What a price cell's hover text says, so the unit is never a guess. */
function priceTitle(price: AiModelPrice, half: "input" | "output"): string {
  const stored = half === "input" ? price.input_micros_per_mtok : price.output_micros_per_mtok;
  if (stored === null) {
    return half === "input"
      ? "No input price recorded. Cost estimates for this model are unknown, not zero."
      : "No output price recorded. Cost estimates for this model are unknown, not zero.";
  }
  return `${stored.toLocaleString("en-US")} micros per million ${half} tokens, shown per 1K above.`;
}

/** How a price's age reads in the table's source badge. */
function priceAgeLabel(price: AiModelPrice): string {
  if (price.age_days === null) {
    return "no price";
  }
  if (price.age_days === 0) {
    return "entered today";
  }
  if (price.age_days === 1) {
    return "entered yesterday";
  }
  if (price.age_days < 30) {
    return `entered ${price.age_days} days ago`;
  }
  const months = Math.round(price.age_days / 30);
  return months === 1 ? "entered a month ago" : `entered ${months} months ago`;
}

/** The one price of a model whose two halves the operator edits together. */
function PriceEditor({ model, onSaved }: { model: AiModel; onSaved: (m: AiModel) => void }) {
  const [open, setOpen] = useState(false);
  const [input, setInput] = useState(() =>
    model.price.input_micros_per_mtok === null ? "" : String(model.price.input_micros_per_mtok),
  );
  const [output, setOutput] = useState(() =>
    model.price.output_micros_per_mtok === null ? "" : String(model.price.output_micros_per_mtok),
  );
  const [source, setSource] = useState(model.price.source);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      // An emptied field sends `null` (forget the half) rather than `""` or `0`, because a blank
      // price means *unknown* and a zero would mean free. Those are different facts and only one
      // of them is what the operator just typed.
      const parse = (value: string): number | null => {
        const trimmed = value.trim();
        return trimmed === "" ? null : Number(trimmed);
      };
      const updated = await updateAiModel(model.id, {
        inputCostMicrosPerMtok: parse(input),
        outputCostMicrosPerMtok: parse(output),
        // Only sent when it changed: an unchanged source must not be restamped, or a price
        // re-entry would quietly claim the platform had re-verified it.
        ...(source !== model.price.source ? { priceSource: source } : {}),
      });
      onSaved(updated);
      setOpen(false);
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The price could not be saved. A price must be null or between 1 and 1000000000.",
      );
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return (
      <button
        type="button"
        onClick={() => setOpen(true)}
        data-model-price-edit={model.model_id}
        className="rounded-md border border-line px-1.5 py-0.5 text-[11px] transition hover:bg-canvas"
      >
        Edit price
      </button>
    );
  }

  return (
    <div
      className="flex flex-col gap-2 rounded-lg border border-line bg-canvas p-2.5"
      data-model-price-editor={model.model_id}
    >
      <div className="grid grid-cols-2 gap-2">
        <label className="flex flex-col gap-1">
          <span className="text-[11px] font-medium">Input µ / 1M tokens</span>
          <input
            value={input}
            onChange={(event) => setInput(event.target.value)}
            type="number"
            inputMode="numeric"
            min={0}
            placeholder="unknown"
            data-model-price-input={model.model_id}
            className="rounded-lg border border-line bg-surface px-2 py-1 text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11px] font-medium">Output µ / 1M tokens</span>
          <input
            value={output}
            onChange={(event) => setOutput(event.target.value)}
            type="number"
            inputMode="numeric"
            min={0}
            placeholder="unknown"
            data-model-price-output={model.model_id}
            className="rounded-lg border border-line bg-surface px-2 py-1 text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
      </div>
      <label className="flex flex-col gap-1">
        <span className="text-[11px] font-medium">Where this came from</span>
        <select
          value={source}
          onChange={(event) => setSource(event.target.value)}
          data-model-price-source={model.model_id}
          className="rounded-lg border border-line bg-surface px-2 py-1 text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
        >
          <option value="manual">Entered by an operator — an estimate</option>
          <option value="discovery">Reported by the provider's listing</option>
          <option value="probe">Measured by a live probe</option>
        </select>
      </label>
      <p className="text-[11px] text-muted">
        A price change applies to the next request. What earlier calls were billed is not
        recalculated — a bill that changes after the fact is worse than an approximate one.
      </p>
      {error ? (
        <p data-model-price-error={model.model_id} className="text-[11px] text-danger">
          {error}
        </p>
      ) : null}
      <div className="flex items-center gap-2">
        <button
          type="button"
          disabled={busy}
          onClick={() => void save()}
          data-model-price-save={model.model_id}
          className="flex items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1 text-[11.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
        >
          {busy ? <Loader2 className="size-3 animate-spin" aria-hidden /> : null}
          Save price
        </button>
        <button
          type="button"
          onClick={() => setOpen(false)}
          className="rounded-lg border border-line px-2.5 py-1 text-[11.5px] transition hover:bg-canvas"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

/** The catalog table: search, capability chips, filters, sortable columns, bulk enable/disable. */
export function ModelCatalog({
  providers,
  models,
  onReload,
}: {
  providers: AiProvider[];
  /** The unfiltered registry, so the table can tell "no models" from "no match". */
  models: AiModel[];
  onReload: () => void;
}) {
  const [q, setQ] = useState("");
  const [capabilities, setCapabilities] = useState<AiCapability[]>([]);
  const [providerId, setProviderId] = useState("");
  const [status, setStatus] = useState<"" | "enabled" | "disabled">("");
  const [sort, setSort] = useState<NonNullable<AiModelQuery["sort"]>>("model");
  const [rows, setRows] = useState<AiModel[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [token, setToken] = useState(0);

  const query: AiModelQuery = useMemo(
    () => ({
      q: q.trim() || undefined,
      capabilities: capabilities.length ? capabilities : undefined,
      providerId: providerId || undefined,
      status: status || undefined,
      sort,
    }),
    [q, capabilities, providerId, status, sort],
  );

  // The listing is a *request*, not a derivation: every keystroke and every chip change re-reads
  // the endpoint, so what the table shows is what the API says the query means.
  useEffect(() => {
    let cancelled = false;
    setError(null);

    // Debounced so typing does not fire a request per character. The first load skips the
    // delay, because a skeleton that shimmers for 250ms before the table appears is worse than
    // one that appears 250ms later.
    const delay = token === 0 ? 0 : 220;
    const timer = setTimeout(() => {
      fetchAiModels(query)
        .then((found) => {
          if (!cancelled) {
            setRows(found);
          }
        })
        .catch((cause: unknown) => {
          if (cancelled) {
            return;
          }
          // A failure is an *error*, never an empty table: "you have no models" and "the
          // catalog could not be read" send an operator to completely different places.
          setError(
            cause instanceof ApiError
              ? cause.message
              : "The catalog could not be read.",
          );
          setRows(null);
        });
    }, delay);

    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [query, token]);

  const reload = useCallback(() => setToken((current) => current + 1), []);

  const toggleCapability = (capability: AiCapability) =>
    setCapabilities((current) =>
      current.includes(capability)
        ? current.filter((entry) => entry !== capability)
        : // Conjunction, matching the API: every selected chip must be claimed. Adding a chip
          // therefore *narrows* the table, and a row that stops satisfying an earlier chip
          // disappears rather than staying on a weaker match.
          [...current, capability],
    );

  const applyBulk = async (enabled: boolean) => {
    const targets = rows ?? [];
    if (targets.length === 0) {
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      for (const model of targets) {
        await updateAiModel(model.id, { enabled });
      }
      setNotice(
        `${targets.length} model${targets.length === 1 ? "" : "s"} ${enabled ? "enabled" : "disabled"}.`,
      );
      onReload();
      reload();
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError ? cause.message : "The models could not be changed.",
      );
    } finally {
      setBusy(false);
    }
  };

  // The capability vocabulary is read off the first model rather than hard-coded: the crate ships
  // the list with every model, so a flag added there appears here with no second edit.
  const capabilityCatalog: AiCapability[] = useMemo(() => {
    const first = models[0];
    if (!first) {
      return [];
    }
    return first.capability_catalog
      .filter((entry) => entry.editable)
      .map((entry) => entry.capability);
  }, [models]);

  const filtering = q.trim() !== "" || capabilities.length > 0 || providerId !== "" || status !== "";
  const registryIsEmpty = models.length === 0;

  return (
    <div className="flex flex-col" data-models-catalog>
      {/* The filter bar. Every control is a real narrowing with a way back to "all". */}
      <div className="flex flex-col gap-2 border-b border-line px-4 py-2.5">
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex min-w-[200px] flex-1 items-center gap-1.5 rounded-lg border border-line bg-canvas px-2 py-1.5">
            <Search className="size-3.5 shrink-0 text-muted" aria-hidden />
            <input
              value={q}
              onChange={(event) => setQ(event.target.value)}
              type="search"
              placeholder="Search the key, the display name or the provider"
              aria-label="Search the model catalog"
              data-catalog-search
              className="min-w-0 flex-1 bg-transparent text-[12.5px] outline-none placeholder:text-muted"
            />
            {q ? (
              <button
                type="button"
                onClick={() => setQ("")}
                aria-label="Clear the search"
                data-catalog-search-clear
                className="shrink-0 text-muted transition hover:text-ink"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            ) : null}
          </label>

          <select
            value={providerId}
            onChange={(event) => setProviderId(event.target.value)}
            aria-label="Filter by provider"
            data-catalog-provider
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="">Every provider</option>
            {providers.map((provider) => (
              <option key={provider.id} value={provider.id}>
                {provider.name}
              </option>
            ))}
          </select>

          <select
            value={status}
            onChange={(event) => setStatus(event.target.value as typeof status)}
            aria-label="Filter by status"
            data-catalog-status
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="">Any status</option>
            <option value="enabled">Enabled</option>
            <option value="disabled">Disabled</option>
          </select>

          <select
            value={sort}
            onChange={(event) => setSort(event.target.value as typeof sort)}
            aria-label="Sort the catalog"
            data-catalog-sort
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="model">Sort by model</option>
            <option value="provider">Sort by provider</option>
            <option value="context">Sort by context window</option>
            <option value="price">Sort by price</option>
            <option value="updated">Sort by last change</option>
          </select>
        </div>

        {/* Capability chips. Multi-select and conjunctive, which is the only reading that helps:
            a disjunction would show a vision model for "tools or vision" and hide the fact that
            it cannot do the other half. */}
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-[11px] font-medium text-muted">Must have</span>
          {capabilityCatalog.map((capability) => {
            const on = capabilities.includes(capability);
            return (
              <button
                key={capability}
                type="button"
                onClick={() => toggleCapability(capability)}
                aria-pressed={on}
                data-catalog-chip={capability}
                className={`rounded-full px-2 py-0.5 text-[11px] font-medium transition ${
                  on
                    ? "bg-accent-soft text-accent-strong ring-1 ring-accent/40"
                    : "bg-quiet-soft text-muted hover:text-ink"
                }`}
              >
                {capability.replace(/_/g, " ")}
              </button>
            );
          })}
          {capabilities.length > 0 ? (
            <button
              type="button"
              onClick={() => setCapabilities([])}
              data-catalog-chips-clear
              className="rounded-full border border-line px-2 py-0.5 text-[11px] text-muted transition hover:text-ink"
            >
              Clear {capabilities.length}
            </button>
          ) : null}
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            disabled={busy || !rows?.length}
            onClick={() => void applyBulk(true)}
            data-catalog-enable-all
            className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            <Power className="size-3" aria-hidden />
            Enable shown
          </button>
          <button
            type="button"
            disabled={busy || !rows?.length}
            onClick={() => void applyBulk(false)}
            data-catalog-disable-all
            className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            <Power className="size-3" aria-hidden />
            Disable shown
          </button>
          {rows ? (
            <span className="text-[11px] text-muted" data-catalog-count>
              {rows.length} of {models.length} shown
            </span>
          ) : null}
        </div>

        {notice ? (
          <p data-catalog-notice className="text-[11.5px] text-positive">
            {notice}
          </p>
        ) : null}
      </div>

      {error ? (
        <div
          className="flex flex-col items-center gap-2 px-6 py-8 text-center"
          data-catalog-error
        >
          <p className="text-[13.5px] font-medium">The catalog could not be loaded</p>
          <p className="max-w-sm text-[12.5px] text-muted">{error}</p>
          <button
            type="button"
            onClick={reload}
            data-catalog-retry
            className="mt-1 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : rows === null ? (
        <LoadingTable columns={6} rows={3} />
      ) : rows.length === 0 ? (
        registryIsEmpty ? (
          <EmptyState
            title="No model is registered"
            hint="Add models to a provider — or pull them from the provider itself with Discover."
          />
        ) : (
          <EmptyState
            title="No model matches this filter"
            hint={`The registry holds ${models.length} model${models.length === 1 ? "" : "s"}; this filter selects none of them.`}
            action={
              <button
                type="button"
                onClick={() => {
                  setQ("");
                  setCapabilities([]);
                  setProviderId("");
                  setStatus("");
                }}
                data-catalog-clear-filters
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Clear the filters
              </button>
            }
          />
        )
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[840px] border-collapse text-left">
            <thead>
              <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <th className="px-4 py-2 font-medium">Model</th>
                <th className="px-3 py-2 font-medium">Provider</th>
                <th className="px-3 py-2 text-right font-medium">
                  <button
                    type="button"
                    onClick={() => setSort(sort === "context" ? "model" : "context")}
                    data-catalog-sort-context
                    className="inline-flex items-center gap-1 transition hover:text-ink"
                  >
                    Context
                    <ArrowDownUp className="size-3" aria-hidden />
                  </button>
                </th>
                <th className="px-3 py-2 font-medium">Capabilities</th>
                <th className="px-3 py-2 text-right font-medium">
                  <button
                    type="button"
                    onClick={() => setSort(sort === "price" ? "model" : "price")}
                    data-catalog-sort-price
                    className="inline-flex items-center gap-1 transition hover:text-ink"
                  >
                    In / Out per 1K
                    <ArrowDownUp className="size-3" aria-hidden />
                  </button>
                </th>
                <th className="px-3 py-2 font-medium">Price</th>
                <th className="px-3 py-2 font-medium">Status</th>
                <th className="px-4 py-2 font-medium">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--color-line)]">
              {rows.map((model) => {
                const copyable = `${model.model_id}`;
                return (
                  <tr
                    key={model.id}
                    data-catalog-row={model.model_id}
                    className="align-top text-[12.5px]"
                  >
                    <td className="px-4 py-2.5">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <span className="font-mono text-[12px]">{model.model_key}</span>
                        {model.is_default ? (
                          <span
                            data-catalog-default
                            title="The model a request that names none is sent to"
                            className="inline-flex items-center gap-0.5 rounded-full bg-accent-soft px-1.5 py-0.5 text-[10.5px] font-medium text-accent-strong"
                          >
                            <Star className="size-2.5" aria-hidden />
                            Default
                          </span>
                        ) : null}
                      </div>
                      {model.display_name !== model.model_key ? (
                        <p className="text-[11.5px] text-muted">{model.display_name}</p>
                      ) : null}
                    </td>
                    <td className="px-3 py-2.5 text-[12px]">{model.provider_name}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[11.5px] tabular-nums">
                      {model.context_window === null
                        ? "—"
                        : model.context_window.toLocaleString("en-US")}
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="flex max-w-[220px] flex-wrap gap-1">
                        {model.capabilities
                          // `chat` is true for every registered model, so printing it ten times
                          // across a table is noise rather than information.
                          .filter((capability) => capability !== "chat")
                          .map((capability) => (
                            <span
                              key={capability}
                              className="inline-flex items-center rounded-full bg-quiet-soft px-1.5 py-0.5 text-[10.5px] font-medium text-muted"
                            >
                              {capability.replace(/_/g, " ")}
                            </span>
                          ))}
                        {model.capabilities.length <= 1 ? (
                          <span className="text-[11px] text-muted">—</span>
                        ) : null}
                      </div>
                    </td>
                    <td
                      className="px-3 py-2.5 text-right font-mono text-[11.5px] tabular-nums"
                      data-catalog-cost={model.model_id}
                      title="Micros of the currency per 1K tokens"
                    >
                      <span title={priceTitle(model.price, "input")}>
                        {priceCell(model.price.input_micros_per_1k)}
                      </span>
                      {" / "}
                      <span title={priceTitle(model.price, "output")}>
                        {priceCell(model.price.output_micros_per_1k)}
                      </span>
                    </td>
                    <td className="px-3 py-2.5" data-catalog-price={model.model_id}>
                      <span
                        className={`inline-flex items-center rounded-full px-1.5 py-0.5 text-[10.5px] font-medium ${
                          model.price.stale
                            ? "bg-caution-soft text-caution-strong"
                            : "bg-quiet-soft text-muted"
                        }`}
                        title={model.price.source_note}
                        data-catalog-price-source={model.price.source}
                      >
                        {priceAgeLabel(model.price)}
                        {!model.price.complete && model.price.age_days !== null ? " · half" : ""}
                      </span>
                      <div className="mt-1">
                        <PriceEditor
                          model={model}
                          onSaved={() => {
                            onReload();
                            reload();
                          }}
                        />
                      </div>
                    </td>
                    <td className="px-3 py-2.5">
                      <span
                        className={`text-[11.5px] ${model.enabled ? "text-positive" : "text-muted"}`}
                        data-catalog-enabled={String(model.enabled)}
                      >
                        {model.enabled ? "Enabled" : "Disabled"}
                      </span>
                    </td>
                    <td className="px-4 py-2.5">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <button
                          type="button"
                          onClick={() => {
                            void navigator.clipboard
                              ?.writeText(copyable)
                              .then(() => setNotice(`${copyable} copied.`))
                              .catch(() => setNotice("The clipboard refused the copy."));
                          }}
                          data-catalog-copy={model.model_id}
                          title={`Copy ${copyable}`}
                          className="flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11px] transition hover:bg-canvas"
                        >
                          <Copy className="size-3" aria-hidden />
                          Copy
                        </button>
                        <button
                          type="button"
                          disabled={busy}
                          onClick={() => {
                            setBusy(true);
                            setError(null);
                            setNotice(null);
                            void updateAiModel(model.id, { isDefault: true })
                              .then(() => {
                                setNotice(`${model.model_key} is now the default.`);
                                onReload();
                                reload();
                              })
                              .catch((cause: unknown) => {
                                setError(
                                  cause instanceof ApiError
                                    ? cause.message
                                    : "The default could not be moved.",
                                );
                              })
                              .finally(() => setBusy(false));
                          }}
                          data-catalog-make-default={model.model_id}
                          className="flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11px] transition hover:bg-canvas disabled:opacity-50"
                        >
                          <Star className="size-3" aria-hidden />
                          {model.is_default ? "Default" : "Set default"}
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {filtering && rows && rows.length > 0 ? (
        <p className="px-4 py-2 text-[11px] text-muted">
          Filtered. The {rows.length} row{rows.length === 1 ? "" : "s"} above are what this
          selection selects; the rest of the registry is untouched.
        </p>
      ) : null}
      {models.length > 0 ? (
        <p className="px-4 pb-3 text-[11px] text-muted">
          The registry was last read {formatTimestamp(models[0]?.updated_at ?? null)}.
        </p>
      ) : null}
    </div>
  );
}

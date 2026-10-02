"use client";

/**
 * `/developer/events` — the event catalogue, framed for someone building an integration.
 *
 * The same registry is already on screen at `/events` (REQ-016), and this deliberately does not
 * become a second copy of it. The two answer different questions and the difference is the whole
 * point of the second page:
 *
 *   - `/events` answers *"what happened?"* — a feed with a time window, for an operator.
 *   - this page answers *"what CAN happen, and what will I receive when it does?"* — a contract,
 *     for a developer writing the subscriber. The window, the cursor and the retention panel are
 *     absent because none of them is part of that question.
 *
 * The parts that are specific to this framing, and that a copy of `/events` could not have:
 *
 *   - **The subscribable set is separated from the rest.** `live` is what a subscriber may
 *     choose; `reserved` is a name the platform has claimed and does not yet emit. Offering both
 *     in one list invites a developer to build against a name that will start working one day
 *     without a version bump — so reserved names are shown, labelled and *unselectable*, and the
 *     count is stated rather than left to be discovered.
 *   - **The webhook deep link prefills the subscription form.** A developer who has just read
 *     `customer.created` should be able to subscribe to it in one click, with the event already
 *     named. The link carries the name as a query parameter the webhook form reads; that is the
 *     one piece of shared state between the two screens and it is a URL, not a store.
 *   - **The schema is offered as JSON, not rendered.** A developer pastes it into their own
 *     project. It comes from `payload_schema`, which the server generates from the *same registry
 *     row* as the field list, so the two cannot describe different payloads — the reason this
 *     page does not hand-build a schema in TypeScript.
 *   - **The delivery count is labelled as this tenant's.** It is a per-organization number
 *     (REQ-016's read scopes it), so showing it without saying so invites "why is this zero?"
 *     from a developer whose subscriber is simply not registered yet.
 *
 * Every hook here is `data-dev-event-*`; the walkthrough pass drives them by name.
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { AlertTriangle, Check, Copy, Radio, Webhook } from "lucide-react";

import { fetchEventCatalogue } from "@/lib/api";
import type { CatalogueEntry, EventCatalogue } from "@/lib/types";

type Scope = "subscribable" | "all" | "reserved";

const SCOPES: { id: Scope; label: string; hint: string }[] = [
  {
    id: "subscribable",
    label: "Subscribable",
    hint: "Names a subscriber may choose today",
  },
  { id: "all", label: "All names", hint: "Live and reserved together" },
  {
    id: "reserved",
    label: "Reserved",
    hint: "Claimed by the platform, not emitted yet",
  },
];

const STATUS_TEXT: Record<CatalogueEntry["status"], string> = {
  live: "Live",
  reserved: "Reserved",
};

function isLive(entry: CatalogueEntry): boolean {
  return entry.status === "live";
}

export function DeveloperEventsView() {
  const [catalogue, setCatalogue] = useState<EventCatalogue | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState("");
  const [area, setArea] = useState<string>("");
  const [scope, setScope] = useState<Scope>("subscribable");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const [sampleFor, setSampleFor] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    fetchEventCatalogue()
      .then((data) => {
        if (!live) return;
        setCatalogue(data);
        setError(null);
      })
      .catch((err: unknown) => {
        if (!live) return;
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (live) setLoading(false);
      });
    return () => {
      live = false;
    };
  }, []);

  const copy = useCallback(async (value: string, key: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(key);
      window.setTimeout(() => setCopied((c) => (c === key ? null : c)), 1500);
    } catch {
      // A clipboard the browser refuses is not an error worth a banner: the text is on screen and
      // selectable, and a developer who is reading it can copy it by hand.
    }
  }, []);

  const entries = useMemo(() => {
    if (!catalogue) return [];
    const needle = query.trim().toLowerCase();
    return catalogue.events.filter((entry) => {
      if (scope === "subscribable" && !isLive(entry)) return false;
      if (scope === "reserved" && isLive(entry)) return false;
      if (area && entry.area !== area) return false;
      if (!needle) return true;
      // The name is the thing a developer pastes into code, so it is matched on that and on the
      // description. The area is deliberately not searched: "billing" matching every billing
      // event is noise, and the area dropdown already narrows it exactly.
      return (
        entry.name.toLowerCase().includes(needle) ||
        entry.description.toLowerCase().includes(needle)
      );
    });
  }, [catalogue, query, area, scope]);

  const grouped = useMemo(() => {
    const byGroup = new Map<string, CatalogueEntry[]>();
    for (const entry of entries) {
      const list = byGroup.get(entry.group);
      if (list) list.push(entry);
      else byGroup.set(entry.group, [entry]);
    }
    return [...byGroup.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [entries]);

  const filtered = query.trim() !== "" || area !== "" || scope !== "all";

  if (loading) {
    return (
      <div data-dev-event-loading className="space-y-3">
        <p className="text-[13px] text-muted">Reading the event registry…</p>
        {[0, 1, 2].map((n) => (
          <div
            key={n}
            aria-hidden
            className="h-12 animate-pulse rounded border border-line bg-surface"
          />
        ))}
      </div>
    );
  }

  if (error) {
    return (
      <div
        data-dev-event-error
        role="alert"
        className="rounded border border-line bg-surface p-4"
      >
        <p className="flex items-center gap-2 text-[13px] font-medium text-caution">
          <AlertTriangle className="size-4" aria-hidden />
          The event catalogue could not be read
        </p>
        <p className="mt-1 text-[13px] text-muted">{error}</p>
        <p className="mt-2 text-[12px] text-muted">
          The registry is compiled into the platform, so this is the request rather than the data.
          The endpoint is <code className="font-mono">GET /api/v1/events/catalogue</code> and needs{" "}
          <code className="font-mono">events.read</code>.
        </p>
      </div>
    );
  }

  if (!catalogue) return null;

  return (
    <div className="space-y-4">
      <p className="text-[13px] text-muted">
        {catalogue.live_count} of {catalogue.events.length} names can be subscribed to today. A
        subscriber may hold at most {catalogue.max_subscriptions} event names.
      </p>

      {/* The summary is a claim about the registry, not about the filter, so it is stated from
          the server's own totals: a screen that counted the rows it happened to receive would
          report a number that moves with a filter the developer cannot see. */}
      <div data-dev-event-summary className="flex flex-wrap gap-2">
        <span className="rounded border border-line bg-surface px-2 py-1 text-[12px] text-muted">
          {catalogue.live_count} live
        </span>
        <span className="rounded border border-line bg-surface px-2 py-1 text-[12px] text-muted">
          {catalogue.reserved_count} reserved
        </span>
        <span className="rounded border border-line bg-surface px-2 py-1 text-[12px] text-muted">
          {catalogue.areas.length} areas
        </span>
      </div>

      <div data-dev-event-filters className="flex flex-wrap items-end gap-3">
        <label className="flex min-w-[16rem] flex-1 flex-col gap-1">
          <span className="text-[12px] text-muted">Search names and descriptions</span>
          <input
            data-dev-event-filter={query}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="customer.created"
            className="rounded border border-line bg-surface px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[12px] text-muted">Area</span>
          <select
            data-dev-event-filter-area={area}
            value={area}
            onChange={(e) => setArea(e.target.value)}
            className="rounded border border-line bg-surface px-2 py-1.5 text-[13px]"
          >
            <option value="">All areas</option>
            {catalogue.areas.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </label>
        <fieldset className="flex flex-col gap-1">
          <legend className="text-[12px] text-muted">Names</legend>
          <div className="flex gap-1">
            {SCOPES.map((option) => (
              <button
                key={option.id}
                type="button"
                data-dev-event-scope={option.id}
                aria-pressed={scope === option.id}
                title={option.hint}
                onClick={() => setScope(option.id)}
                className={
                  scope === option.id
                    ? "rounded border border-accent bg-accent/10 px-2 py-1.5 text-[12px] text-ink"
                    : "rounded border border-line bg-surface px-2 py-1.5 text-[12px] text-muted"
                }
              >
                {option.label}
              </button>
            ))}
          </div>
        </fieldset>
        {filtered ? (
          <button
            type="button"
            data-dev-event-reset
            onClick={() => {
              setQuery("");
              setArea("");
              setScope("all");
            }}
            className="rounded border border-line bg-surface px-2 py-1.5 text-[12px] text-muted"
          >
            Clear
          </button>
        ) : null}
      </div>

      {grouped.length === 0 ? (
        <p data-dev-event-empty className="rounded border border-line bg-surface p-6 text-center text-[13px] text-muted">
          {catalogue.events.length === 0
            ? "The registry is empty."
            : "No event name matches these filters."}
        </p>
      ) : (
        <div data-dev-event-list className="space-y-4">
          {grouped.map(([group, rows]) => (
            <section key={group} data-dev-event-group={group}>
              <h3 className="mb-1.5 text-[12px] font-medium uppercase tracking-wide text-muted">
                {group}
              </h3>
              <ul className="space-y-1.5">
                {rows.map((entry) => {
                  const live = isLive(entry);
                  const open = expanded === entry.name;
                  return (
                    <li
                      key={entry.name}
                      data-dev-event-row={entry.name}
                      data-dev-event-status={entry.status}
                      className="rounded border border-line bg-surface"
                    >
                      <div className="flex flex-wrap items-center gap-2 px-3 py-2">
                        <code
                          data-dev-event-name={entry.name}
                          className="font-mono text-[13px] text-ink"
                        >
                          {entry.name}
                        </code>
                        <button
                          type="button"
                          data-dev-event-copy={entry.name}
                          title={`Copy ${entry.name}`}
                          aria-label={`Copy the event name ${entry.name}`}
                          onClick={() => void copy(entry.name, `name:${entry.name}`)}
                          className="rounded p-0.5 text-muted transition hover:text-ink"
                        >
                          {copied === `name:${entry.name}` ? (
                            <Check className="size-3" aria-hidden />
                          ) : (
                            <Copy className="size-3" aria-hidden />
                          )}
                        </button>
                        <span
                          data-dev-event-badge={entry.status}
                          className={
                            live
                              ? "rounded border border-line px-1.5 py-0.5 text-[11px] text-muted"
                              : "rounded border border-caution/40 px-1.5 py-0.5 text-[11px] text-caution"
                          }
                        >
                          {live ? <Radio className="size-3" aria-hidden /> : null}
                          {STATUS_TEXT[entry.status]}
                        </span>
                        <span className="text-[12px] text-muted">{entry.area}</span>
                        {entry.deliveries_24h > 0 ? (
                          <span className="text-[12px] text-muted">
                            {entry.deliveries_24h} delivered (24h, this organization)
                          </span>
                        ) : null}
                        <div className="ml-auto flex items-center gap-1">
                          <button
                            type="button"
                            data-dev-event-expand={entry.name}
                            aria-expanded={open}
                            onClick={() => setExpanded(open ? null : entry.name)}
                            className="rounded border border-line px-2 py-1 text-[12px] text-muted"
                          >
                            {open ? "Hide payload" : `Payload (${entry.payload_fields.length})`}
                          </button>
                          {/* The deep link is the one thing this page does that /events cannot:
                              it carries the name forward, so the webhook form opens with the
                              event already chosen. A reserved name has no such link, because
                              there is nothing to subscribe to. */}
                          {live ? (
                            <Link
                              href={`/webhooks/new?event=${encodeURIComponent(entry.name)}`}
                              data-dev-event-subscribe={entry.name}
                              className="flex items-center gap-1 rounded border border-accent px-2 py-1 text-[12px] text-ink"
                            >
                              <Webhook className="size-3" aria-hidden />
                              Subscribe
                            </Link>
                          ) : (
                            <span
                              data-dev-event-not-subscribable={entry.name}
                              title="This name is claimed by the platform but not emitted yet."
                              className="rounded border border-line px-2 py-1 text-[12px] text-muted"
                            >
                              Not subscribable
                            </span>
                          )}
                        </div>
                      </div>

                      <p className="px-3 pb-2 text-[12px] text-muted">{entry.description}</p>

                      {open ? (
                        <div className="border-t border-line px-3 py-2">
                          <table
                            data-dev-event-fields={entry.name}
                            className="w-full text-left text-[12px]"
                          >
                            <thead>
                              <tr className="text-muted">
                                <th scope="col" className="py-1 pr-3 font-medium">
                                  Field
                                </th>
                                <th scope="col" className="py-1 pr-3 font-medium">
                                  Type
                                </th>
                                <th scope="col" className="py-1 font-medium">
                                  Required
                                </th>
                              </tr>
                            </thead>
                            <tbody>
                              {entry.payload_fields.length === 0 ? (
                                <tr>
                                  <td colSpan={3} className="py-1.5 text-muted">
                                    This name carries no payload fields.
                                  </td>
                                </tr>
                              ) : (
                                entry.payload_fields.map((field) => (
                                  <tr
                                    key={field.name}
                                    data-dev-event-field={`${entry.name}.${field.name}`}
                                    className="border-t border-line"
                                  >
                                    <td className="py-1 pr-3 font-mono text-ink">
                                      {field.name}
                                    </td>
                                    <td className="py-1 pr-3 font-mono text-muted">
                                      {field.kind}
                                    </td>
                                    <td className="py-1 text-muted">
                                      {field.required ? "required" : "optional"}
                                    </td>
                                  </tr>
                                ))
                              )}
                            </tbody>
                          </table>

                          {/* The schema is copied, never rendered into a fake editor: it is
                              generated by the server from the same registry row as the table
                              above, and a TypeScript rebuild of it would be a second source of
                              truth for the same payload. */}
                          <div className="mt-2 flex items-center gap-2">
                            <button
                              type="button"
                              data-dev-event-sample={entry.name}
                              aria-expanded={sampleFor === entry.name}
                              onClick={() =>
                                setSampleFor(sampleFor === entry.name ? null : entry.name)
                              }
                              className="rounded border border-line px-2 py-1 text-[12px] text-muted"
                            >
                              {sampleFor === entry.name ? "Hide JSON schema" : "JSON schema"}
                            </button>
                            <button
                              type="button"
                              data-dev-event-copy-schema={entry.name}
                              onClick={() =>
                                void copy(
                                  JSON.stringify(entry.payload_schema, null, 2),
                                  `schema:${entry.name}`,
                                )
                              }
                              className="rounded border border-line px-2 py-1 text-[12px] text-muted"
                            >
                              {copied === `schema:${entry.name}` ? "Copied" : "Copy schema"}
                            </button>
                          </div>
                          {sampleFor === entry.name ? (
                            <>
                              {/* The sample is a payload the server guarantees satisfies its own
                                  schema, and the QA plan asks the pass to check that. Showing
                                  both together is the check a developer can read: if the sample
                                  ever stopped matching the field list above, the two would
                                  disagree on screen before any test noticed. */}
                              <p className="mt-2 text-[11px] text-muted">
                                A sample payload that satisfies the schema above:
                              </p>
                              <pre
                                data-dev-event-sample-payload={entry.name}
                                className="mt-1 max-h-64 overflow-auto rounded border border-line bg-canvas p-2 font-mono text-[11px]"
                              >
                                {JSON.stringify(entry.sample, null, 2)}
                              </pre>
                              <pre
                                data-dev-event-schema={entry.name}
                                className="mt-2 max-h-72 overflow-auto rounded border border-line bg-canvas p-2 font-mono text-[11px]"
                              >
                                {JSON.stringify(entry.payload_schema, null, 2)}
                              </pre>
                            </>
                          ) : null}
                        </div>
                      ) : null}
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
        </div>
      )}
    </div>
  );
}

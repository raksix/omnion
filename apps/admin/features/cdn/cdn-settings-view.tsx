"use client";

/**
 * `/cdn/settings` — the provider, the trigger toggles and the queue bounds (REQ-011, slice 4
 * surface, shipped with slice 1's screen set).
 *
 * Four things here are decisions rather than fields, and each is the place a CDN
 * configuration usually goes quietly wrong:
 *
 * - **The credential is write-only and the form says so.** The API never returns the stored
 *   value, so the panel can only report *whether one is present*. A text box that rendered
 *   the saved key — or an empty one that looked like "no key stored" — is the same bug with
 *   two faces: the first leaks, the second sends an operator to re-enter a working key. The
 *   box therefore starts blank every time, says what is stored, and is left out of the save
 *   entirely unless the operator typed something.
 * - **The adapter picker shows what each one actually does.** "Generic HTTP" and "Hosted
 *   CDN" are names; the description line is what the operator needs at the moment they pick
 *   one, because the difference is whether their provider invalidates by URL or by tag.
 * - **The bounds are restated, not trusted.** Batch size and attempts have server-side
 *   constraints; a form that only finds out on save is a form that has already been used
 *   wrongly once.
 * - **The trigger toggles name their consequence.** "Purge on publish" is a switch; "every
 *   publish queues a purge of that page and its assets" is the same switch with the cost
 *   attached, which is what the person enabling it is deciding about.
 */
import { useCallback, useEffect, useState } from "react";

import { KeyRound, Loader2, Save } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ApiError, fetchCdnAdapters, fetchCdnSettings, saveCdnSettings } from "@/lib/api";
import { useSites } from "@/lib/sites";
import type { CdnAdapterInfo, CdnSettings } from "@/lib/types";

/**
 * The events a purge can be triggered by.
 *
 * Written out with their consequences rather than as a list of names: the person turning a
 * switch on is deciding how much invalidation traffic they are signing up for, and an
 * event name alone does not tell them.
 *
 * The names are the ones the platform **records**, not the ones REQ-011's own text used.
 * Two of the request's names — `media.replaced` and `site.domain.changed` — are not
 * emitted by anything: a file's bytes are replaced by `media.version_created`, and a domain
 * changing is `domain.added` or `domain.removed`. A switch on a name nothing emits is a
 * switch an operator can turn on, watch for a week and never see anything happen from, and
 * the failure is silent — nothing errors, the setting saves, the purge simply never comes.
 * The seven below are the seven names `crates/cdn`'s trigger table consumes, and the crate's
 * test asserts they are all in the event catalogue.
 */
const TRIGGERS: { event: string; consequence: string }[] = [
  {
    event: "page.published",
    consequence: "every publish queues a purge of that page and the assets it references",
  },
  {
    event: "page.unpublished",
    consequence: "a page taken down stops being served from the edge straight away",
  },
  {
    event: "page.deleted",
    consequence: "a deleted page is invalidated rather than left cached until its TTL ends",
  },
  {
    event: "media.version_created",
    consequence: "a replaced file invalidates its own address, not the whole library",
  },
  {
    event: "theme.activated",
    consequence: "a new theme invalidates every page at once — a large purge, once",
  },
  {
    event: "domain.added",
    consequence: "a new domain invalidates every page cached under the old one",
  },
  {
    event: "domain.removed",
    consequence:
      "a removed domain invalidates the addresses nobody can reach any more",
  },
];

/** The bounds the API enforces, restated so the form can refuse before it posts. */
const BATCH_RANGE = { min: 1, max: 1000 } as const;
const ATTEMPTS_RANGE = { min: 1, max: 10 } as const;

/** The form's own state, kept apart from the saved row. */
type Draft = {
  provider: string;
  endpoint_url: string;
  zone_ref: string;
  credential: string;
  batch_size: string;
  max_attempts: string;
  auto_purge: Record<string, boolean>;
};

function toDraft(settings: CdnSettings, adapters: CdnAdapterInfo[]): Draft {
  const toggles: Record<string, boolean> = {};
  for (const trigger of TRIGGERS) {
    toggles[trigger.event] = settings.auto_purge?.[trigger.event] === true;
  }
  return {
    provider: adapters.some((adapter) => adapter.key === settings.provider)
      ? settings.provider
      : "origin",
    endpoint_url: settings.endpoint_url ?? "",
    zone_ref: settings.zone_ref ?? "",
    // Deliberately blank: the API does not return the stored value, so there is nothing to
    // prefill and something to prefill *with* would be a key in the DOM.
    credential: "",
    batch_size: String(settings.batch_size),
    max_attempts: String(settings.max_attempts),
    auto_purge: toggles,
  };
}

/** The per-site CDN settings of the selected site. */
export function CdnSettingsView() {
  const { selectedSite } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [settings, setSettings] = useState<CdnSettings | null>(null);
  const [adapters, setAdapters] = useState<CdnAdapterInfo[]>([]);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    Promise.all([fetchCdnAdapters(), fetchCdnSettings(siteId)])
      .then(([catalogue, row]) => {
        if (cancelled) {
          return;
        }
        setAdapters(catalogue);
        setSettings(row);
        setDraft(toDraft(row, catalogue));
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(
            cause instanceof ApiError ? cause.message : "The CDN settings could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  const save = async () => {
    if (!draft) {
      return;
    }
    const adapter = adapters.find((entry) => entry.key === draft.provider);
    if (adapter?.needs_endpoint && draft.endpoint_url.trim().length === 0) {
      setFieldError({
        field: "endpoint_url",
        message: `${adapter.label} needs an endpoint URL before it can be used.`,
      });
      return;
    }
    if (adapter?.needs_zone && draft.zone_ref.trim().length === 0) {
      setFieldError({
        field: "zone_ref",
        message: `${adapter.label} needs a zone reference before it can be used.`,
      });
      return;
    }
    if (adapter?.needs_credential && !settings?.has_credential && draft.credential.length === 0) {
      setFieldError({
        field: "credential",
        message: `${adapter.label} needs a credential. It is stored encrypted and never shown again.`,
      });
      return;
    }
    const batch = Number(draft.batch_size);
    if (!Number.isInteger(batch) || batch < BATCH_RANGE.min || batch > BATCH_RANGE.max) {
      setFieldError({
        field: "batch_size",
        message: `The batch size must be a whole number between ${BATCH_RANGE.min} and ${BATCH_RANGE.max}.`,
      });
      return;
    }
    const attempts = Number(draft.max_attempts);
    if (
      !Number.isInteger(attempts) ||
      attempts < ATTEMPTS_RANGE.min ||
      attempts > ATTEMPTS_RANGE.max
    ) {
      setFieldError({
        field: "max_attempts",
        message: `Attempts must be a whole number between ${ATTEMPTS_RANGE.min} and ${ATTEMPTS_RANGE.max}.`,
      });
      return;
    }

    setBusy(true);
    setFieldError(null);
    setNotice(null);
    try {
      // The credential is only in the payload when the operator typed one. Sending the
      // empty string would clear a working key, because "the field is empty" and "clear the
      // stored key" are otherwise the same request.
      const credential = draft.credential.length > 0 ? draft.credential : undefined;
      const saved = await saveCdnSettings({
        site_id: siteId,
        provider: draft.provider,
        endpoint_url: draft.endpoint_url.trim() || null,
        zone_ref: draft.zone_ref.trim() || null,
        ...(credential === undefined ? {} : { credential }),
        auto_purge: draft.auto_purge,
        batch_size: batch,
        max_attempts: attempts,
      });
      setSettings(saved);
      setDraft(toDraft(saved, adapters));
      setNotice(
        credential === undefined
          ? "Settings saved."
          : "Settings saved, and the credential was replaced. It will not be shown again.",
      );
    } catch (cause: unknown) {
      if (cause instanceof ApiError && typeof cause.details?.field === "string") {
        setFieldError({ field: cause.details.field, message: cause.message });
      } else {
        setError(cause instanceof ApiError ? cause.message : "The settings could not be saved.");
      }
    } finally {
      setBusy(false);
    }
  };

  if (!draft || settings === null) {
    return error ? (
      <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <p className="text-[12.5px] text-accent-strong">{error}</p>
        <button
          type="button"
          onClick={reload}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    ) : (
      <div className="flex items-center gap-2 px-1 py-8 text-[12.5px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Loading the CDN settings…
      </div>
    );
  }

  const adapter = adapters.find((entry) => entry.key === draft.provider);
  const inherited = settings.site_id === null && siteId !== null;

  return (
    <div className="flex flex-col gap-4">
      {error ? (
        <p
          role="alert"
          className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          className="rounded-xl border border-positive/25 bg-positive-soft px-4 py-3 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}
      {inherited ? (
        <p className="rounded-xl border border-line bg-canvas/60 px-4 py-3 text-[12.5px] text-muted">
          This site has no row of its own and is using the installation default. Saving here
          writes the site&apos;s own row, which then takes precedence.
        </p>
      ) : null}

      <section className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-4 py-4">
        <h2 className="text-[13.5px] font-medium">Provider</h2>

        <fieldset className="flex flex-col gap-2">
          <legend className="text-[12.5px] font-medium">Adapter</legend>
          {adapters.map((entry) => (
            <label
              key={entry.key}
              className="flex cursor-pointer items-start gap-2.5 rounded-lg border border-line px-3 py-2.5"
            >
              <input
                type="radio"
                name="cdn-provider"
                value={entry.key}
                data-cdn-adapter={entry.key}
                checked={draft.provider === entry.key}
                onChange={() => setDraft({ ...draft, provider: entry.key })}
                className="mt-0.5 size-3.5 accent-[var(--color-accent)]"
              />
              <span className="flex flex-col gap-0.5">
                <span className="text-[13px] font-medium">{entry.label}</span>
                <span className="text-[12px] text-muted">{entry.description}</span>
              </span>
            </label>
          ))}
        </fieldset>

        {adapter?.needs_endpoint ? (
          <label className="flex flex-col gap-1.5 text-[12.5px]">
            <span className="font-medium">Endpoint URL</span>
            <input
              value={draft.endpoint_url}
              data-cdn-endpoint
              onChange={(event) => setDraft({ ...draft, endpoint_url: event.target.value })}
              placeholder="https://purge.example.com/v1/purge"
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            {fieldError?.field === "endpoint_url" ? (
              <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
            ) : null}
          </label>
        ) : null}

        {adapter?.needs_zone ? (
          <label className="flex flex-col gap-1.5 text-[12.5px]">
            <span className="font-medium">Zone reference</span>
            <input
              value={draft.zone_ref}
              data-cdn-zone
              onChange={(event) => setDraft({ ...draft, zone_ref: event.target.value })}
              placeholder="example.com"
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            {fieldError?.field === "zone_ref" ? (
              <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
            ) : null}
          </label>
        ) : null}

        {adapter?.needs_credential ? (
          <label className="flex flex-col gap-1.5 text-[12.5px]">
            <span className="flex items-center gap-1.5 font-medium">
              <KeyRound className="size-3.5" aria-hidden />
              Credential
            </span>
            <input
              type="password"
              value={draft.credential}
              data-cdn-credential
              autoComplete="off"
              onChange={(event) => setDraft({ ...draft, credential: event.target.value })}
              placeholder={settings.has_credential ? "Leave empty to keep the stored one" : "Paste the provider's API key"}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            <span className="text-[11.5px] text-muted">
              {settings.has_credential
                ? "A credential is stored for this site. It is write-only: the API never returns it and this form starts empty on every visit."
                : "No credential is stored yet. It is written encrypted and never shown again."}
            </span>
            {fieldError?.field === "credential" ? (
              <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
            ) : null}
          </label>
        ) : null}
      </section>

      <section className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-4 py-4">
        <h2 className="text-[13.5px] font-medium">Automatic purges</h2>
        <p className="text-[12px] text-muted">
          Each of these is a real purge queued when the event happens. Turning one on is a
          decision about invalidation traffic, so each line says what it costs.
        </p>
        <ul className="flex flex-col divide-y divide-line">
          {TRIGGERS.map((trigger) => (
            <li key={trigger.event} className="flex items-start justify-between gap-4 py-2.5">
              <span className="flex min-w-0 flex-col gap-0.5">
                <span className="font-mono text-[12px]">{trigger.event}</span>
                <span className="text-[11.5px] text-muted">{trigger.consequence}</span>
              </span>
              <input
                type="checkbox"
                data-cdn-trigger={trigger.event}
                checked={draft.auto_purge[trigger.event] === true}
                onChange={(event) =>
                  setDraft({
                    ...draft,
                    auto_purge: { ...draft.auto_purge, [trigger.event]: event.target.checked },
                  })
                }
                aria-label={`Purge on ${trigger.event}`}
                className="mt-0.5 size-4 shrink-0 accent-[var(--color-accent)]"
              />
            </li>
          ))}
        </ul>
      </section>

      <section className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-4 py-4">
        <h2 className="text-[13.5px] font-medium">Queue bounds</h2>
        <div className="grid gap-4 md:grid-cols-2">
          <label className="flex flex-col gap-1.5 text-[12.5px]">
            <span className="font-medium">Targets per provider call</span>
            <input
              value={draft.batch_size}
              data-cdn-batch-size
              onChange={(event) => setDraft({ ...draft, batch_size: event.target.value })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            <span className="text-[11.5px] text-muted">
              {`A larger purge is split into calls of this size. Between ${BATCH_RANGE.min} and ${BATCH_RANGE.max}.`}
            </span>
            {fieldError?.field === "batch_size" ? (
              <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
            ) : null}
          </label>
          <label className="flex flex-col gap-1.5 text-[12.5px]">
            <span className="font-medium">Attempts before an item is left failed</span>
            <input
              value={draft.max_attempts}
              data-cdn-max-attempts
              onChange={(event) => setDraft({ ...draft, max_attempts: event.target.value })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            <span className="text-[11.5px] text-muted">
              {`Between ${ATTEMPTS_RANGE.min} and ${ATTEMPTS_RANGE.max}. A provider that is down is retried rather than dropped.`}
            </span>
            {fieldError?.field === "max_attempts" ? (
              <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
            ) : null}
          </label>
        </div>
      </section>

      {adapters.length === 0 ? (
        <EmptyState
          title="No adapters were returned"
          hint="The catalogue endpoint answered with nothing, so there is no provider to choose from."
        />
      ) : null}

      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => void save()}
          disabled={busy || adapters.length === 0}
          data-cdn-settings-save
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
        >
          <Save className="size-3.5" aria-hidden />
          Save settings
        </button>
        <button
          type="button"
          onClick={() => setDraft(toDraft(settings, adapters))}
          disabled={busy}
          className="rounded-lg border border-line px-3 py-2 text-[12.5px] transition hover:bg-canvas disabled:opacity-60"
        >
          Discard changes
        </button>
      </div>
    </div>
  );
}

"use client";

/**
 * The endpoint form: connect a receiver, or change one that is already connected.
 *
 * The form has one job that is different from every other form in the panel, and everything
 * unusual about it follows from that job: **a secret has exactly one lifetime in the UI**.
 *
 * - On creation, the platform may generate the signing secret, and the response is the only
 *   time it ever travels. So the create flow does not navigate away: it shows the secret, asks
 *   for confirmation that it was stored, and does not offer `Done` until that box is ticked.
 *   Navigating first would mean the secret is gone before it was read.
 * - On edit, the secret is never shown at all, because the API never returns it. The form
 *   offers `Rotate secret` instead, which is a different operation with a different answer —
 *   which is why it is a separate route and not a field.
 *
 * The other two decisions are about not blocking the operator:
 *
 * - **A non-HTTPS URL warns and does not block.** A receiver on a private network is a
 *   legitimate setup, and a panel that refuses it is a panel people work around. The warning
 *   says what is exposed, because the signature is the only thing protecting the payload.
 * - **Validation happens on the way out *and* on the way back.** The fields check themselves
 *   so the operator is told before a round trip, and the API's own codes are rendered next to
 *   the field they refused, because the API is the authority on what it accepts.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Check, Copy, KeyRound, TriangleAlert, Wand2 } from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import {
  ApiError,
  createWebhookEndpoint,
  fetchEventCatalogue,
  fetchWebhookEndpoint,
  updateWebhookEndpoint,
} from "@/lib/api";
import type { EventCatalogue } from "@/lib/types";

/** Which field an API refusal was about, so the message lands next to the right box. */
type FieldErrors = Partial<Record<"name" | "url" | "events" | "secret", string>>;

/** The API's error codes, mapped to the field each one is really about. */
function fieldForCode(code: string): keyof FieldErrors | null {
  switch (code) {
    case "webhook_name_taken":
      return "name";
    case "invalid_webhook_endpoint":
      return "url";
    case "invalid_event":
      return "events";
    default:
      return null;
  }
}

export function EndpointForm({ endpointId }: { endpointId?: string }) {
  const router = useRouter();
  const editing = Boolean(endpointId);

  /**
   * The event the developer arrived with, from `/developer/events?event=<name>`.
   *
   * This is the deep link that makes the catalogue more than a read-only table: a developer who
   * has just read what `customer.created` carries should be able to subscribe to it without
   * retyping the name and without hunting for its checkbox in a grouped list.
   *
   * Two rules keep it honest, and both matter more than the convenience:
   *
   * - It is applied **once**, and only when creating. On edit the stored subscription list wins,
   *   because the endpoint already has subscriptions and a deep link must not silently add one
   *   to a receiver that is live and already delivering to someone else.
   * - It is applied **after the catalogue arrives**, not before. The list this form can subscribe
   *   to is the `live` names only; a reserved name carried in the URL is simply not found, and
   *   the form opens with nothing ticked rather than with a name the API would reject on save.
   *   That is the correct outcome — a reserved name is not subscribable, and the catalogue says
   *   so in as many words before the link is ever rendered.
   */
  const preselect = useSearchParams().get("event");

  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [secretMode, setSecretMode] = useState<"generate" | "own">("generate");
  const [ownSecret, setOwnSecret] = useState("");
  const [enabled, setEnabled] = useState(true);

  const [catalogue, setCatalogue] = useState<EventCatalogue | null>(null);
  const [catalogueError, setCatalogueError] = useState<string | null>(null);
  const [loading, setLoading] = useState(editing);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>({});

  // The secret the API just generated, and the acknowledgement that it was stored.
  const [issuedSecret, setIssuedSecret] = useState<string | null>(null);
  /** The id of the endpoint just created — where `Done` goes. */
  const [createdId, setCreatedId] = useState<string | null>(null);
  const [stored, setStored] = useState(false);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        setCatalogue(await fetchEventCatalogue());
        setCatalogueError(null);
      } catch (caught) {
        setCatalogueError((caught as ApiError).message);
      }
    })();
  }, []);

  useEffect(() => {
    if (!endpointId) return;
    void (async () => {
      setLoading(true);
      try {
        const endpoint = await fetchWebhookEndpoint(endpointId);
        setName(endpoint.name);
        setUrl(endpoint.url);
        setSelected(endpoint.events);
        setEnabled(endpoint.enabled);
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setLoading(false);
      }
    })();
  }, [endpointId]);

  /** The live names, grouped, and the ceiling the API enforces. */
  const groups = useMemo(() => {
    const byGroup = new Map<string, string[]>();
    for (const entry of catalogue?.events ?? []) {
      if (entry.status !== "live") continue;
      const bucket = byGroup.get(entry.group) ?? [];
      bucket.push(entry.name);
      byGroup.set(entry.group, bucket);
    }
    return [...byGroup.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [catalogue]);

  const ceiling = catalogue?.max_subscriptions ?? 32;

  /**
   * Apply the deep link's event, once the catalogue that can confirm it has arrived.
   *
   * `preselect` is in the dependency list and the effect is idempotent (`selected.length === 0`
   * is the guard), so a catalogue refetch cannot re-apply over an operator's own choices: the
   * guard is on the *state* rather than on a ref, and a tick the operator made themselves has
   * made `selected` non-empty. The alternative — a `useRef` to mean "once" — would re-tick the
   * box whenever the operator deliberately unticked it and the catalogue refreshed.
   */
  useEffect(() => {
    if (editing || !preselect || !catalogue || selected.length > 0) return;
    if (!groups.some(([, names]) => names.includes(preselect))) return;
    setSelected([preselect]);
  }, [preselect, catalogue, groups, editing, selected.length]);

  /**
   * A stored subscription list is *expanded* — it carries every name a group wildcard covers.
   * Showing all of them ticked would be honest but unusable, and re-submitting the expansion
   * is harmless (reconcile is idempotent), so the form shows the groups and the explicitly
   * named events, and the expansion is left in the payload.
   */
  const groupSelected = useMemo(() => {
    const chosen = new Set(selected);
    return new Set(
      groups
        .filter(([, names]) => names.every((name) => chosen.has(name)))
        .map(([group]) => `${group}.*`),
    );
  }, [groups, selected]);

  const toggleGroup = (group: string) => {
    const names = groups.find(([key]) => key === group)?.[1] ?? [];
    const wildcard = `${group}.*`;
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(wildcard)) {
        next.delete(wildcard);
        for (const name of names) next.delete(name);
        return [...next];
      }
      for (const name of names) next.add(name);
      return [...next].sort();
    });
  };

  const toggleName = (name: string) => {
    setSelected((current) =>
      current.includes(name)
        ? current.filter((entry) => entry !== name)
        : [...current, name].sort(),
    );
  };

  /** The client-side pass. The API is the authority; this only saves a round trip. */
  const check = useCallback((): FieldErrors => {
    const problems: FieldErrors = {};
    if (!name.trim()) problems.name = "Name the endpoint so the team can tell receivers apart.";
    if (!url.trim()) problems.url = "The URL deliveries are POSTed to.";
    else if (!/^https?:\/\//i.test(url.trim())) problems.url = "The URL must start with http:// or https://";
    else if (/\s/.test(url.trim())) problems.url = "The URL cannot contain a space.";
    if (selected.length === 0) problems.events = "Subscribe the endpoint to at least one event.";
    if (selected.length > ceiling) problems.events = `At most ${ceiling} events; ${selected.length} selected.`;
    if (secretMode === "own") {
      if (ownSecret.length < 16) problems.secret = "At least 16 characters.";
      else if (ownSecret.length > 128) problems.secret = "At most 128 characters.";
      else if (/\s/.test(ownSecret)) problems.secret = "No whitespace: the secret is sent as a header.";
    }
    return problems;
  }, [ceiling, name, ownSecret, secretMode, selected, url]);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const problems = check();
    setFieldErrors(problems);
    setError(null);
    if (Object.keys(problems).length > 0) return;

    setSaving(true);
    try {
      if (editing) {
        // The secret is not part of an edit: it is never shown, so there is nothing to send
        // back and nothing that could be sent back by accident.
        await updateWebhookEndpoint(endpointId as string, {
          name: name.trim(),
          url: url.trim(),
          events: selected,
          enabled,
        });
        router.push(`/webhooks/${endpointId}`);
        return;
      }

      const created = await createWebhookEndpoint({
        name: name.trim(),
        url: url.trim(),
        events: selected,
        ...(secretMode === "own" ? { secret: ownSecret } : {}),
      });
      setName(created.name);
      setUrl(created.url);
      setSelected(created.events);
      setIssuedSecret(created.secret ?? null);
      setCreatedId(created.id);
      // An operator who supplied their own secret has nothing to store, so the gate is
      // satisfied by construction: the value is already theirs.
      setStored(created.secret === undefined);
    } catch (caught) {
      const api = caught as ApiError;
      const field = fieldForCode(api.code);
      setFieldErrors(field ? { [field]: api.message } : {});
      setError(api.message);
    } finally {
      setSaving(false);
    }
  };

  const copySecret = async () => {
    if (!issuedSecret) return;
    try {
      await navigator.clipboard.writeText(issuedSecret);
      setCopied(true);
    } catch {
      // Saying "copied" when nothing was copied is worse than saying nothing; the value is
      // still on screen and selectable.
      setError("The browser refused clipboard access — select the secret and copy it by hand.");
    }
  };

  if (issuedSecret) {
    return (
      <div className="flex flex-col gap-4" data-webhook-secret-once>
        <div
          role="alert"
          className="flex items-start gap-2 rounded-xl border border-caution/40 bg-caution-soft px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
          <span>
            This is the only time the signing secret is shown. Store it now — if it is lost, the
            only way forward is to rotate it, and every delivery signed with the old one stops
            verifying.
          </span>
        </div>

        <div className="rounded-xl border border-line bg-surface p-3">
          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span className="font-medium">Signing secret</span>
            <div className="flex items-center gap-2">
              <code
                data-webhook-secret-value
                className="flex-1 overflow-x-auto rounded-lg border border-line bg-canvas px-2.5 py-2 font-mono text-[12.5px] text-ink"
              >
                {issuedSecret}
              </code>
              <button
                type="button"
                onClick={() => void copySecret()}
                data-webhook-secret-copy
                className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-2 text-[12.5px] text-muted hover:text-ink"
              >
                <Copy className="size-3.5" aria-hidden />
                {copied ? "Copied" : "Copy"}
              </button>
            </div>
          </label>
        </div>

        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            checked={stored}
            data-webhook-secret-stored
            onChange={(event) => setStored(event.target.checked)}
            className="mt-0.5"
          />
          <span>I stored this secret.</span>
        </label>

        <div className="flex items-center gap-2">
          <button
            type="button"
            data-webhook-secret-done
            disabled={!stored}
            onClick={() => router.push(createdId ? `/webhooks/${createdId}` : "/webhooks")}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
          >
            <Check className="size-3.5" aria-hidden />
            Done
          </button>
          <span className="text-[11.5px] text-muted">
            {stored ? "The endpoint is ready to receive." : "Tick the box to finish."}
          </span>
        </div>
      </div>
    );
  }

  if (loading) {
    return (
      <p aria-busy="true" className="text-[13px] text-muted">
        Loading the endpoint…
      </p>
    );
  }

  const insecure = /^http:\/\//i.test(url.trim());

  return (
    <form onSubmit={submit} className="flex max-w-2xl flex-col gap-4">
      {catalogueError ? (
        <p data-webhook-form-catalogue-error className="text-[12.5px] text-caution">
          The event catalogue did not load ({catalogueError}). The platform will refuse any
          subscription list until it does, so this form cannot be submitted.
        </p>
      ) : null}

      {error ? (
        <p
          data-webhook-form-error
          role="alert"
          className="rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2 text-[12.5px]"
        >
          {error}
        </p>
      ) : null}

      <label className="flex flex-col gap-1 text-[12.5px]">
        <span className="font-medium text-ink">Name</span>
        <input
          value={name}
          onChange={(event) => setName(event.target.value)}
          data-webhook-field-name
          aria-invalid={Boolean(fieldErrors.name)}
          aria-describedby={fieldErrors.name ? "webhook-name-error" : undefined}
          placeholder="Order service"
          className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
        />
        {fieldErrors.name ? (
          <span id="webhook-name-error" data-webhook-error-name className="text-[12px] text-red-600">
            {fieldErrors.name}
          </span>
        ) : null}
      </label>

      <label className="flex flex-col gap-1 text-[12.5px]">
        <span className="font-medium text-ink">Receiver URL</span>
        <input
          value={url}
          onChange={(event) => setUrl(event.target.value)}
          data-webhook-field-url
          aria-invalid={Boolean(fieldErrors.url)}
          aria-describedby={fieldErrors.url ? "webhook-url-error" : undefined}
          placeholder="https://example.test/hooks/omnion"
          className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
        />
        {insecure ? (
          <span
            data-webhook-insecure-warning
            className="inline-flex items-center gap-1.5 text-[12px] text-caution"
          >
            <TriangleAlert className="size-3.5" aria-hidden />
            Plain HTTP: the signature is the only thing protecting the payload, and anyone on the
            path can read it. It will still be accepted.
          </span>
        ) : null}
        {fieldErrors.url ? (
          <span id="webhook-url-error" data-webhook-error-url className="text-[12px] text-red-600">
            {fieldErrors.url}
          </span>
        ) : null}
      </label>

      <fieldset className="flex flex-col gap-2 rounded-xl border border-line p-3">
        <legend className="px-1 text-[12.5px] font-medium text-ink">
          Events ({selected.length}/{ceiling})
        </legend>
        {catalogueError ? null : (
          <div
            data-webhook-event-picker
            className="flex max-h-72 flex-col gap-2 overflow-y-auto"
          >
            {groups.map(([group, names]) => {
              const wildcard = `${group}.*`;
              const all = groupSelected.has(wildcard);
              return (
                <div key={group} className="flex flex-col gap-1">
                  <label className="flex items-center gap-2 text-[12px] font-medium text-ink">
                    <input
                      type="checkbox"
                      checked={all}
                      data-webhook-group={group}
                      onChange={() => toggleGroup(group)}
                    />
                    {group}.*
                    <span className="text-muted">({names.length})</span>
                  </label>
                  <div className="ml-5 flex flex-wrap gap-2">
                    {names.map((name) => (
                      <label
                        key={name}
                        className="flex items-center gap-1.5 rounded-full bg-quiet-soft px-2 py-0.5 text-[11.5px] text-muted"
                      >
                        <input
                          type="checkbox"
                          checked={selected.includes(name)}
                          data-webhook-event={name}
                          onChange={() => toggleName(name)}
                        />
                        {name}
                      </label>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        )}
        {fieldErrors.events ? (
          <span data-webhook-error-events className="text-[12px] text-red-600">
            {fieldErrors.events}
          </span>
        ) : null}
      </fieldset>

      {!editing ? (
        <fieldset className="flex flex-col gap-2 rounded-xl border border-line p-3">
          <legend className="px-1 text-[12.5px] font-medium text-ink">Signing secret</legend>
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="radio"
              name="secret-mode"
              checked={secretMode === "generate"}
              data-webhook-secret-generate
              onChange={() => setSecretMode("generate")}
            />
            <Wand2 className="size-3.5" aria-hidden />
            Generate for me
            <span className="text-muted">— shown once, at creation</span>
          </label>
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="radio"
              name="secret-mode"
              checked={secretMode === "own"}
              data-webhook-secret-own
              onChange={() => setSecretMode("own")}
            />
            <KeyRound className="size-3.5" aria-hidden />
            Provide my own
          </label>
          {secretMode === "own" ? (
            <input
              value={ownSecret}
              onChange={(event) => setOwnSecret(event.target.value)}
              data-webhook-secret-input
              aria-invalid={Boolean(fieldErrors.secret)}
              placeholder="16–128 characters, no whitespace"
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
            />
          ) : null}
          {fieldErrors.secret ? (
            <span data-webhook-error-secret className="text-[12px] text-red-600">
              {fieldErrors.secret}
            </span>
          ) : null}
        </fieldset>
      ) : (
        <p className="text-[12px] text-muted">
          The signing secret is not shown here and cannot be changed by an edit. Use{" "}
          <strong>Rotate secret</strong> on the endpoint to replace it.
        </p>
      )}

      <label className="flex items-center gap-2 text-[12.5px]">
        <input
          type="checkbox"
          checked={enabled}
          data-webhook-field-enabled
          onChange={(event) => setEnabled(event.target.checked)}
        />
        Deliver to this endpoint
      </label>

      <div className="flex items-center gap-2">
        <button
          type="submit"
          disabled={saving || Boolean(catalogueError)}
          data-webhook-submit
          className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
        >
          {saving ? "Saving…" : editing ? "Save changes" : "Connect endpoint"}
        </button>
        <Link href="/webhooks" className="text-[12.5px] text-accent-strong hover:underline">
          Cancel
        </Link>
      </div>
    </form>
  );
}

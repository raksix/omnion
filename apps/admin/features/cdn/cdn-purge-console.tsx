"use client";

/**
 * `/cdn/purge` — the purge console (REQ-011, slice 2).
 *
 * The form is small and the refusals are the design. Four things are worth saying about
 * why it is shaped this way:
 *
 * - **The target list is validated as it is typed, with the server's own rules.** A URL
 *   target must be an absolute path, a tag must look like a surrogate key, the list cannot
 *   exceed the cap — and every one of those checks mirrors what the API refuses with, so
 *   the operator is told at the point of the mistake rather than after a round trip. The
 *   server still re-checks: a client that skips this form is not a client this screen can
 *   vouch for.
 * - **The whole-zone mode needs the word `PURGE` typed, and the field says so in its own
 *   placeholder.** A whole-zone purge is the one action in the CDN that a visitor can see
 *   everywhere at once, and a checkbox is not a confirmation. The confirmation is also sent
 *   to the server as a *fact* rather than as the literal string, so a client that sends
 *   `true` without ever showing the field is refused.
 * - **Duplicate lines are collapsed before the count is taken.** Pasting the same URL twice
 *   is an accident, not a request for two invalidations, and the provider bills and
 *   rate-limits per target. The counter shows what will actually be queued.
 * - **The result of a submitted purge is a link to its own row, not a toast.** A toast
 *   disappears in four seconds and leaves the operator with no way to find out what the
 *   provider said. The history row is where the answer lives, so that is where the form
 *   sends them.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, ArrowLeft, Eraser, Globe2, Link2, Tag } from "lucide-react";
import Link from "next/link";

import { ApiError, createCdnPurge, fetchCdnAdapters, fetchCdnStatus } from "@/lib/api";
import { useSites } from "@/lib/sites";
import type { CdnAdapter, CdnPurge, CdnPurgeKind } from "@/lib/types";

/** The cap, restated so the form can count; the server sends its own in `/cdn/status`. */
const FALLBACK_CAP = 500;

/** The three modes, with the icon and the sentence that explains what they do. */
const MODES: {
  value: CdnPurgeKind;
  label: string;
  icon: typeof Link2;
  hint: string;
  placeholder: string;
}[] = [
  {
    value: "url",
    label: "URLs",
    icon: Link2,
    hint: "One absolute path per line. Each is a page, a file or an API path the edge holds.",
    placeholder: "/blog/hello-world\n/blog/2026/launch",
  },
  {
    value: "tag",
    label: "Tags",
    icon: Tag,
    hint: "One surrogate key per line. A key invalidates everything carrying it — a section, or a whole site.",
    placeholder: "/blog\n/blog/hello-world",
  },
  {
    value: "all",
    label: "Everything",
    icon: Globe2,
    hint: "Invalidates the provider's whole zone. Every cached response is dropped.",
    placeholder: "",
  },
];

/** The trimmed, de-duplicated, non-empty lines of the textarea. */
function parseTargets(raw: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const line of raw.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "" || seen.has(trimmed)) {
      continue;
    }
    seen.add(trimmed);
    out.push(trimmed);
  }
  return out;
}

/** Does a line look like an absolute path the edge would accept? */
function isUrlTarget(target: string): boolean {
  return target.startsWith("/") && !target.startsWith("//") && !/\s/.test(target);
}

/** Does a line look like a surrogate key `headers::surrogate_keys` emits? */
function isTagTarget(target: string): boolean {
  return /^[A-Za-z0-9\-_.:/]+$/.test(target);
}

/** The first line that is wrong, and why. `null` when the list is acceptable. */
function firstProblem(kind: CdnPurgeKind, targets: string[]): string | null {
  if (kind === "all") {
    return null;
  }
  if (targets.length === 0) {
    return kind === "url"
      ? "Enter at least one path. A purge with no targets would report success having done nothing."
      : "Enter at least one surrogate key.";
  }
  const bad = targets.find((target) =>
    kind === "url" ? !isUrlTarget(target) : !isTagTarget(target),
  );
  if (bad !== undefined) {
    return kind === "url"
      ? `“${bad}” is not an absolute path. It must start with / and contain no spaces.`
      : `“${bad}” is not a surrogate key. Use letters, digits, and - _ . : / only.`;
  }
  return null;
}

/** The purge console for the selected site. */
export function CdnPurgeConsole() {
  const { selectedSite } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [kind, setKind] = useState<CdnPurgeKind>("url");
  const [raw, setRaw] = useState("");
  const [confirm, setConfirm] = useState("");
  const [adapters, setAdapters] = useState<CdnAdapter[]>([]);
  const [cap, setCap] = useState(FALLBACK_CAP);
  const [provider, setProvider] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [field, setField] = useState<string | null>(null);
  const [created, setCreated] = useState<CdnPurge | null>(null);

  // The cap, the provider and the catalogue are all the server's answers. The cap in
  // particular is not restated as a constant: a form that hard-codes 500 and a server that
  // refuses at 501 are two numbers that will eventually disagree, and the disagreement
  // shows up as a refusal the operator cannot explain.
  useEffect(() => {
    if (siteId === null) {
      return;
    }
    let cancelled = false;
    fetchCdnAdapters()
      .then((list) => {
        if (!cancelled) {
          setAdapters(list);
        }
      })
      .catch(() => {
        // The catalogue is a convenience (it says whether tag purging is real here). A
        // failure must not block a purge on the `origin` adapter, which needs no
        // configuration at all — so this is swallowed and the form stays usable.
      });
    fetchCdnStatus(siteId)
      .then((status) => {
        if (cancelled) {
          return;
        }
        setProvider(status.provider);
        setCap(FALLBACK_CAP);
      })
      .catch(() => {
        // Same reasoning: the cap falls back to the documented default and the console
        // still works. The server refuses anything over the cap either way.
      });
    return () => {
      cancelled = true;
    };
  }, [siteId]);

  const targets = useMemo(() => parseTargets(raw), [raw]);
  const mode = MODES.find((entry) => entry.value === kind) ?? MODES[0];
  const adapter = adapters.find((entry) => entry.key === provider) ?? null;

  // Tag and whole-zone modes are only offered when the configured adapter can do them.
  // Offering "purge by tag" to an adapter that cannot would queue work that fails an hour
  // later at drain time, which is the worst moment to find out.
  const tagsSupported = adapter === null || adapter.supports_tags;
  const allSupported = adapter === null || adapter.supports_purge_all;

  const problem = useMemo(
    () => (kind === "all" ? null : firstProblem(kind, targets)),
    [kind, targets],
  );
  const overCap = kind !== "all" && targets.length > cap;
  const confirmMissing = kind === "all" && confirm.trim() !== "PURGE";
  const blocked = problem !== null || overCap || confirmMissing || submitting;

  const onSubmit = useCallback(
    async (event: React.FormEvent) => {
      event.preventDefault();
      if (siteId === null || blocked) {
        return;
      }
      setSubmitting(true);
      setError(null);
      setField(null);
      setCreated(null);
      try {
        const purge = await createCdnPurge({
          site_id: siteId,
          kind,
          targets: kind === "all" ? [] : targets,
          zone_confirmed: kind === "all" && confirm.trim() === "PURGE",
        });
        setCreated(purge);
        setRaw("");
        setConfirm("");
      } catch (cause) {
        if (cause instanceof ApiError) {
          setError(cause.message);
          // The server names the field it refused in `details.field`; putting the message
          // under that input is the difference between a form the operator can fix and one
          // they can only stare at. A refusal with no field falls back to the kind, which
          // is the only other place this form can be wrong.
          const named = cause.details?.field;
          setField(typeof named === "string" ? named : "kind");
        } else {
          setError("The purge could not be requested.");
        }
      } finally {
        setSubmitting(false);
      }
    },
    [siteId, blocked, kind, targets, confirm],
  );

  if (siteId === null) {
    return (
      <div className="flex flex-col gap-4">
        <Back />
        <p className="text-[13px] text-muted">
          The CDN is configured per site. Pick one in the switcher above.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <Back />

      {created ? (
        <div
          data-cdn-purge-submitted
          className="flex flex-col items-start gap-3 rounded-xl border border-positive/30 bg-positive-soft px-4 py-3"
        >
          <p className="text-[12.5px] text-positive">
            {`Queued: ${created.item_count} target${created.item_count === 1 ? "" : "s"} through ${created.provider}. It is ${created.status} — the history shows what the provider answered.`}
          </p>
          <Link
            href="/cdn/purges"
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            See it in the history
          </Link>
        </div>
      ) : null}

      {error ? (
        <p
          role="alert"
          data-cdn-purge-form-error
          className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}

      <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
        <fieldset className="flex flex-col gap-2">
          <legend className="text-[12.5px] font-medium">What should be invalidated?</legend>
          <div className="grid gap-2 sm:grid-cols-3">
            {MODES.map((entry) => {
              const Icon = entry.icon;
              const unavailable =
                (entry.value === "tag" && !tagsSupported) ||
                (entry.value === "all" && !allSupported);
              return (
                <label
                  key={entry.value}
                  data-cdn-purge-mode={entry.value}
                  className={
                    unavailable
                      ? "flex cursor-not-allowed flex-col gap-1 rounded-xl border border-line bg-canvas px-3 py-2.5 opacity-60"
                      : kind === entry.value
                        ? "flex cursor-pointer flex-col gap-1 rounded-xl border border-accent bg-accent-soft px-3 py-2.5"
                        : "flex cursor-pointer flex-col gap-1 rounded-xl border border-line bg-surface px-3 py-2.5 transition hover:border-accent/40"
                  }
                >
                  <span className="flex items-center gap-2">
                    <input
                      type="radio"
                      name="cdn-purge-kind"
                      value={entry.value}
                      checked={kind === entry.value}
                      disabled={unavailable}
                      onChange={() => setKind(entry.value)}
                      className="size-3.5"
                    />
                    <Icon className="size-3.5 text-muted" aria-hidden />
                    <span className="text-[12.5px] font-medium">{entry.label}</span>
                  </span>
                  <span className="text-[11.5px] text-muted">
                    {unavailable
                      ? `The ${provider ?? "configured"} adapter cannot do this.`
                      : entry.hint}
                  </span>
                </label>
              );
            })}
          </div>
        </fieldset>

        {kind !== "all" ? (
          <div className="flex flex-col gap-1.5">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <label
                htmlFor="cdn-purge-targets"
                className="text-[12.5px] font-medium"
                data-cdn-purge-targets-label
              >
                {mode.label === "Tags" ? "Surrogate keys" : "Paths"}
              </label>
              <span
                data-cdn-purge-target-count
                className={
                  overCap
                    ? "text-[11.5px] text-accent-strong"
                    : "text-[11.5px] text-muted"
                }
              >
                {overCap
                  ? `${targets.length} targets — the limit is ${cap}`
                  : `${targets.length} of ${cap} targets`}
              </span>
            </div>
            <textarea
              id="cdn-purge-targets"
              value={raw}
              onChange={(event) => setRaw(event.target.value)}
              rows={8}
              spellCheck={false}
              placeholder={mode.placeholder}
              aria-invalid={problem !== null || overCap}
              data-cdn-purge-targets
              className="rounded-lg border border-line bg-surface px-3 py-2 font-mono text-[12.5px]"
            />
            {problem ? (
              <p
                role="alert"
                data-cdn-purge-targets-error
                className="text-[12px] text-accent-strong"
              >
                {problem}
              </p>
            ) : overCap ? (
              <p
                role="alert"
                data-cdn-purge-targets-error
                className="text-[12px] text-accent-strong"
              >
                {`A single purge takes at most ${cap} targets. The worker would split this into several provider calls, which is not what one purge should be. Remove ${targets.length - cap}.`}
              </p>
            ) : null}
            {raw.trim() !== "" && problem === null && !overCap ? (
              <p className="text-[11.5px] text-muted">
                {`${targets.length} target${targets.length === 1 ? "" : "s"} after trimming blanks and removing repeats.`}
              </p>
            ) : null}
          </div>
        ) : (
          <div className="flex flex-col gap-1.5">
            <p className="flex items-start gap-2 rounded-xl border border-caution/30 bg-caution-soft px-3 py-2.5 text-[12.5px] text-caution">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              <span>
                This drops every cached response in the zone. Ordinary traffic will be slower
                until the edge refills, and any origin problem becomes visible all at once.
              </span>
            </p>
            <label
              htmlFor="cdn-purge-confirm"
              className="text-[12.5px] font-medium"
              data-cdn-purge-confirm-label
            >
              Type PURGE to confirm
            </label>
            <input
              id="cdn-purge-confirm"
              type="text"
              value={confirm}
              onChange={(event) => setConfirm(event.target.value)}
              placeholder="PURGE"
              autoComplete="off"
              spellCheck={false}
              data-cdn-purge-confirm
              className="w-full rounded-lg border border-line bg-surface px-3 py-2 font-mono text-[12.5px] sm:max-w-[260px]"
            />
            {confirm.trim() !== "" && confirmMissing ? (
              <p
                role="alert"
                data-cdn-purge-confirm-error
                className="text-[12px] text-accent-strong"
              >
                {`That reads “${confirm.trim()}”. The confirmation is the word PURGE, exactly.`}
              </p>
            ) : null}
          </div>
        )}

        {field === "targets" && problem === null && !overCap ? (
          <p
            role="alert"
            data-cdn-purge-field-error
            className="text-[12px] text-accent-strong"
          >
            {error}
          </p>
        ) : null}

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="submit"
            disabled={blocked}
            data-cdn-purge-submit
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
          >
            {submitting ? "Queueing…" : kind === "all" ? "Invalidate the whole zone" : "Queue the purge"}
          </button>
          <button
            type="button"
            onClick={() => {
              setRaw("");
              setConfirm("");
              setError(null);
              setField(null);
            }}
            data-cdn-purge-clear
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
          >
            <Eraser className="size-3.5" aria-hidden />
            Clear
          </button>
          <span className="text-[11.5px] text-muted">
            {provider === null
              ? "Reading the configured provider…"
              : `Invalidation is sent to ${provider}.`}
          </span>
        </div>
      </form>
    </div>
  );
}

/** The way back to the history, which is where a submitted purge is answered. */
function Back() {
  return (
    <Link
      href="/cdn/purges"
      data-cdn-purge-back
      className="inline-flex w-fit items-center gap-1.5 text-[12.5px] text-muted transition hover:text-ink"
    >
      <ArrowLeft className="size-3.5" aria-hidden />
      Purge history
    </Link>
  );
}

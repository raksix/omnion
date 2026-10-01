"use client";

/**
 * `/security/secrets` — the read-only secret inventory (REQ-012, slice 4).
 *
 * ## What this screen refuses to do
 *
 * **1. It has no edit button, and that is not a missing feature.** Management of secrets belongs
 * to the secrets manager request (REQ-125). This screen reports. A screen that could edit a
 * *reference* would invite an operator to believe it could rotate a *secret*, and rotating one
 * means replacing a value in an environment and redeploying. The API has no write verb either —
 * `the_inventory_cannot_be_written_through` drives all four and asserts the router refuses.
 *
 * **2. It shows no values, and the type makes that structural.** There is no `value` field on the
 * row type, so no renderer can print one; `no_secret_value_reaches_the_inventory_response` walks
 * the real response body against a database holding real material and fails if any of those
 * literals appear. What replaced the value is a **count** — "3 configured" — and the row says so.
 *
 * **3. It has no green state.** `unverifiable` is the best a configured secret can read, because
 * the platform can see that a reference exists and can read nothing about the value behind it. A
 * state meaning "this secret is fine" would have to be a lie, so the vocabulary does not contain
 * one — `there_is_no_state_that_reads_as_healthy` asserts it and `healthy`/`ok` are absent from
 * the type, not merely unused.
 *
 * ## The rotation date is a reading, not a guarantee
 *
 * Rotation happens outside the platform. So `rotated_at` is the reference row's own timestamp,
 * and `evidence` names whether it was **changed** or merely **created** — a date an operator
 * reads as "rotated on" when it only means "registered on" is worse than no date at all. An
 * environment variable has no evidence at all, and the screen says so rather than showing a blank
 * cell that reads as "recently rotated".
 *
 * ## The limitation note is permanent
 *
 * The environment list is maintained by hand, because a process cannot enumerate its own
 * environment. So this screen cannot list a secret it was not told about, and the API says so in
 * a `limitation` field the header renders every time. An inventory that looks exhaustive when it
 * is partial is the defect this note exists to prevent.
 *
 * Loading skeleton, error state with retry, the empty state, `/` to focus, and a mobile card
 * layout are real states rather than placeholders.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, EyeOff, Info, Loader2, RefreshCw, ShieldAlert } from "lucide-react";

import { fetchSecretInventory, type ApiError } from "@/lib/api";
import { SecurityTabs } from "@/features/security/security-tabs";
import type { SecretInventory, SecretReference } from "@/lib/types";

/** The badge a state gets. Never colour alone, so it reads without hue. */
function stateLabel(state: SecretReference["state"]): string {
  switch (state) {
    case "unverifiable":
      return "not verifiable";
    case "missing":
      return "missing";
    case "expired":
      return "expired";
    default:
      return state;
  }
}

/**
 * The tone class for a state.
 *
 * `unverifiable` is deliberately **neutral** rather than a warning. Almost every row on this
 * screen is unverifiable, because that is the honest state for almost every secret — colouring it
 * amber would paint the whole screen amber and train the operator to ignore the tone. The two
 * that are genuinely actionable — missing and expired — are the coloured ones.
 */
function stateTone(state: SecretReference["state"]): string {
  switch (state) {
    case "missing":
      return "border-rose-200 bg-rose-50 text-rose-800";
    case "expired":
      return "border-amber-200 bg-amber-50 text-amber-900";
    default:
      return "border-line bg-quiet-soft text-muted";
  }
}

/** What the rotation evidence means in one line, or `null` when it says nothing. */
function evidenceNote(row: SecretReference): string | null {
  switch (row.evidence) {
    case "reference_changed":
      return "the reference was last edited on this date";
    case "reference_created":
      return "the reference has not been edited since it was created — this is not a rotation date";
    default:
      return null;
  }
}

export function SecuritySecretsScreen() {
  const [inventory, setInventory] = useState<SecretInventory | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const [filter, setFilter] = useState("");

  const load = useCallback(async () => {
    setError(null);
    try {
      setInventory(await fetchSecretInventory());
    } catch (failure) {
      setError((failure as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // `/` focuses the filter, the way the other security screens do — and never while the operator
  // is already typing, where it would swallow the character.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const rows = inventory?.secrets ?? [];
  // The filter is client-side and only ever **narrows** the list — a security
  // inventory that hides a row behind a search box is one an operator can
  // mistake for "not configured".
  const visible = filter
    ? rows.filter((row) =>
        `${row.name} ${row.source} ${row.scope}`.toLowerCase().includes(filter.toLowerCase()),
      )
    : rows;

  return (
    <div className="space-y-5" data-security-secrets>
      <SecurityTabs current="secrets" />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold text-ink">Secret inventory</h1>
          <p className="mt-1 max-w-2xl text-[13px] text-muted">
            What this platform holds, by reference. Names, scopes and counts — never a value.
            Rotating a secret happens in the environment and a deploy, not here.
          </p>
        </div>
        <button
          type="button"
          onClick={() => {
            setLoading(true);
            void load();
          }}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft disabled:opacity-60"
          disabled={loading}
          data-security-secrets-refresh
        >
          {loading ? (
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
          ) : (
            <RefreshCw className="size-3.5" aria-hidden />
          )}
          Refresh
        </button>
      </header>

      {/* The limitation, always. A field the API fills and this screen shows on
          every load, so it cannot be quietly dropped when a row is added. */}
      {inventory && (
        <p
          className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px] text-muted"
          data-security-secrets-limitation
        >
          <Info className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>{inventory.limitation}</span>
        </p>
      )}

      {error ? (
        <div
          className="flex flex-wrap items-center gap-3 rounded-lg border border-rose-200 bg-rose-50 px-3 py-2.5 text-[13px] text-rose-900"
          role="alert"
          data-security-secrets-error
        >
          <AlertTriangle className="size-4 shrink-0" aria-hidden />
          <span className="min-w-0 flex-1">{error}</span>
          <button
            type="button"
            onClick={() => {
              setLoading(true);
              void load();
            }}
            className="rounded-lg border border-rose-300 bg-white px-2.5 py-1 text-[12.5px]"
          >
            Retry
          </button>
        </div>
      ) : loading ? (
        <div
          className="flex items-center gap-2 rounded-lg border border-line px-3 py-6 text-[13px] text-muted"
          data-security-secrets-loading
        >
          <Loader2 className="size-4 animate-spin" aria-hidden />
          Reading the inventory…
        </div>
      ) : rows.length === 0 ? (
        <div
          className="rounded-lg border border-dashed border-line px-4 py-8 text-center"
          data-security-secrets-empty
        >
          <EyeOff className="mx-auto size-5 text-muted" aria-hidden />
          <p className="mt-2 text-[13px] font-medium text-ink">No references are configured</p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            This is the empty state for a deployment with no environment secrets set and no
            integration configured — not a filter that matched nothing.
          </p>
        </div>
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-3">
            <label className="sr-only" htmlFor="security-secrets-search">
              Filter the inventory
            </label>
            <input
              id="security-secrets-search"
              ref={searchRef}
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter by name, source or scope"
              className="w-full max-w-sm rounded-lg border border-line bg-surface px-3 py-1.5 text-[13px] text-ink outline-none focus:border-ink-soft sm:w-72"
              data-security-secrets-search
            />
            <span className="text-[12.5px] text-muted" data-security-secrets-counts>
              {visible.length === rows.length
                ? `${rows.length} reference${rows.length === 1 ? "" : "s"}`
                : `${visible.length} of ${rows.length} references`}
              {(inventory?.missing ?? 0) > 0 && ` · ${inventory?.missing} missing`}
            </span>
          </div>

          {/* The legend is served by the API, so a state the vocabulary grows
              cannot appear in the table with no explanation beside it. */}
          <ul className="flex flex-wrap gap-3 text-[12px] text-muted" data-security-secrets-legend>
            {(inventory?.states ?? []).map((state) => (
              <li key={state} className="flex items-center gap-1.5">
                <span className="inline-block size-2 rounded-full border border-line" aria-hidden />
                {stateLabel(state as SecretReference["state"])}
              </li>
            ))}
          </ul>

          {/* Desktop: a table. The note column is wide because it is the only
              thing on this screen that explains a row, and truncating it would
              leave a badge with no reason. */}
          <div className="hidden overflow-x-auto rounded-lg border border-line md:block">
            <table className="w-full min-w-[820px] text-left text-[13px]">
              <thead className="border-b border-line text-[12px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-3 py-2 font-medium">Reference</th>
                  <th className="px-3 py-2 font-medium">Source</th>
                  <th className="px-3 py-2 font-medium">Scope</th>
                  <th className="px-3 py-2 font-medium">State</th>
                  <th className="px-3 py-2 font-medium">Count</th>
                  <th className="px-3 py-2 font-medium">Last observed</th>
                  <th className="px-3 py-2 font-medium">Note</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((row) => (
                  <tr
                    key={row.key}
                    className="border-b border-line last:border-0 align-top"
                    data-security-secret-row={row.key}
                  >
                    <td className="px-3 py-2 font-medium text-ink">{row.name}</td>
                    <td className="px-3 py-2 text-muted">{row.source}</td>
                    <td className="px-3 py-2 text-muted">{row.scope}</td>
                    <td className="px-3 py-2">
                      <span
                        className={`inline-block rounded-md border px-1.5 py-0.5 text-[11.5px] ${stateTone(row.state)}`}
                      >
                        {stateLabel(row.state)}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {row.material_count > 0 ? row.material_count : "—"}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {row.rotated_at
                        ? `${new Date(row.rotated_at).toLocaleDateString()} (${row.age_days ?? 0}d)`
                        : "—"}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {row.note}
                      {evidenceNote(row) && (
                        <span className="mt-0.5 block text-[11.5px] text-muted/80">
                          {evidenceNote(row)}
                        </span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Mobile: cards, because a seven-column table on a phone is a
              horizontal scroll that hides the note column entirely. */}
          <ul className="space-y-3 md:hidden">
            {visible.map((row) => (
              <li
                key={row.key}
                className="rounded-lg border border-line p-3"
                data-security-secret-card={row.key}
              >
                <div className="flex flex-wrap items-start justify-between gap-2">
                  <span className="break-all font-medium text-ink">{row.name}</span>
                  <span
                    className={`shrink-0 rounded-md border px-1.5 py-0.5 text-[11.5px] ${stateTone(row.state)}`}
                  >
                    {stateLabel(row.state)}
                  </span>
                </div>
                <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-[12.5px]">
                  <dt className="text-muted">Source</dt>
                  <dd className="text-ink">{row.source}</dd>
                  <dt className="text-muted">Scope</dt>
                  <dd className="break-all text-ink">{row.scope}</dd>
                  {row.material_count > 0 && (
                    <>
                      <dt className="text-muted">Count</dt>
                      <dd className="text-ink">{row.material_count}</dd>
                    </>
                  )}
                  {row.rotated_at && (
                    <>
                      <dt className="text-muted">Observed</dt>
                      <dd className="text-ink">
                        {new Date(row.rotated_at).toLocaleDateString()} ({row.age_days ?? 0}d)
                      </dd>
                    </>
                  )}
                </dl>
                <p className="mt-2 text-[12.5px] text-muted">{row.note}</p>
                {evidenceNote(row) && (
                  <p className="mt-0.5 text-[11.5px] text-muted/80">{evidenceNote(row)}</p>
                )}
              </li>
            ))}
          </ul>

          {visible.length === 0 && (
            <p
              className="rounded-lg border border-dashed border-line px-4 py-6 text-center text-[13px] text-muted"
              data-security-secrets-no-match
            >
              Nothing matches that filter. The rows are still on the page above the search box —
              this screen never hides a secret behind a search.
            </p>
          )}
        </>
      )}

      {/* Why there is no "rotate" or "edit" button, said where the operator will
          look for one. A silent absence reads as an unfinished screen. */}
      <p
        className="flex items-start gap-2 text-[12px] text-muted"
        data-security-secrets-readonly
      >
        <ShieldAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
        <span>
          Read-only by design. Rotation means replacing the value in the environment and
          redeploying — management of that belongs to the secrets manager, and this screen never
          receives a value to rotate.
        </span>
      </p>
    </div>
  );
}

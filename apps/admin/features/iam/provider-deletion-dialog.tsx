"use client";

/**
 * The provider deletion dialog: read the impact, name the count, offer the repair.
 *
 * Why this is a dialog and not the inline "Remove / Remove for good" pair it replaces: the
 * refusal only exists when the provider provisioned accounts, and the two-button confirm has no
 * way to know that before the click. It discovers the block from a `409` error body, which
 * reaches the operator as "something went wrong" plus a raw payload, and it offers nothing —
 * "cannot delete" with no next step is a dead end. The backend already answers
 * `GET /deletion-impact` with the same count on a route that can return `200` with zero
 * accounts, so the question is asked *before* the button and the answer is rendered.
 *
 * Three things are decided here rather than in the parent, because each one is a claim about the
 * impact and a claim has to be checkable:
 *
 * 1. The breakdown is shown **next to** the total and adds up to it. "7 accounts" is a number
 *    to wave through; "7 accounts: 5 LDAP, 2 SCIM" tells the operator a directory sweep created
 *    these people. When the sum does not reconcile — a source string a newer migration wrote —
 *    the dialog says so instead of printing a total that looks broken.
 * 2. A **SCIM-provisioned account counts with no provider id at all.** `0127` deliberately
 *    leaves a pushed row unattributed because a connector names no provider, so anything keyed on
 *    the provider id would answer "0 accounts" for a directory that just created eight people.
 * 3. The reassign is offered **only** when the delete is actually blocked. A provider with no
 *    provisioned accounts has nothing to reassign, and a button that appears anyway is a button
 *    that eventually sends an empty batch and gets told `no_subjects`.
 *
 * The list of accounts is a bounded sample, and the dialog says how many are not shown rather
 * than letting an operator believe they are reading the whole set.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, ArrowRightLeft, Loader2, ShieldAlert, Trash2, X } from "lucide-react";
import {
  deleteIamProvider,
  fetchIamProviderDeletionImpact,
  reassignIamProviderAccounts,
  type IamProviderDeletionImpact,
} from "@/lib/api";

/** The outcome the parent needs to hear about once the dialog is done. */
export type ProviderDeletionOutcome =
  | { kind: "deleted" }
  | { kind: "reassigned"; moved: number; skipped: number; deleted: boolean };

type Props = {
  /** The provider being removed. */
  provider: { id: string; slug: string; name: string };
  /** The accounts the dialog may reassign, i.e. the sample the impact route returned. */
  onClose: () => void;
  /** Called with what happened, so the caller can refresh the list and announce the result. */
  onDone: (outcome: ProviderDeletionOutcome) => void;
};

/** The refusal in the operator's terms, not the API's. */
function headline(impact: IamProviderDeletionImpact): string {
  if (impact.affected_accounts === 0) {
    return "No account loses its sign-in";
  }
  const n = impact.affected_accounts;
  return n === 1
    ? "1 account would lose its sign-in"
    : `${n} accounts would lose their sign-in`;
}

/**
 * What a source is called on screen.
 *
 * `0127`'s vocabulary is closed, but a newer migration may write a string this build has never
 * heard of, and showing a raw enum to an administrator reading a dialog is how a product starts
 * looking broken. Unknown values are passed through with their underscores turned back into
 * spaces rather than hidden — the operator needs to be able to report the exact string.
 */
const SOURCE_LABELS: Record<string, string> = {
  local: "Local",
  sso: "Single sign-on",
  scim: "SCIM",
  ldap: "LDAP",
  oidc: "OpenID Connect",
  saml: "SAML",
};

function sourceLabel(source: string): string {
  return SOURCE_LABELS[source] ?? source.replace(/_/g, " ");
}

export function ProviderDeletionDialog({ provider, onClose, onDone }: Props) {
  const [impact, setImpact] = useState<IamProviderDeletionImpact | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<null | "reassign" | "delete">(null);

  /* Escape closes, and the dialog takes focus when it opens so a keyboard operator is not left
   * tabbing through the provider table behind the overlay. */
  const panel = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    panel.current?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setImpact(await fetchIamProviderDeletionImpact(provider.id));
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : "the impact could not be read");
    } finally {
      setLoading(false);
    }
  }, [provider.id]);

  useEffect(() => {
    void load();
  }, [load]);

  /* The sum is the point of the breakdown, so it is *computed* here and compared to the total
   * rather than assumed. `unknown_sources` is the API admitting it cannot name one of them. */
  const breakdownTotal = impact
    ? impact.by_source.reduce((sum, row) => sum + row.count, 0)
    : 0;
  const reconciles = !impact || breakdownTotal === impact.affected_accounts;

  async function reassign() {
    if (!impact) return;
    const ids = impact.accounts.map((account) => account.user_id);
    setBusy("reassign");
    setError(null);
    try {
      const result = await reassignIamProviderAccounts(provider.id, ids);
      setBusy(null);
      /* Re-read before saying anything is now deletable. The impact the dialog is holding was
       * true a moment ago; between then and now somebody else may have moved an account, and
       * announcing "now you can delete this" from a stale read is a lie the button then
       * contradicts. */
      await load();
      onDone({ kind: "reassigned", moved: result.reassigned, skipped: result.skipped, deleted: false });
    } catch (caught) {
      setBusy(null);
      setError(caught instanceof Error ? caught.message : "the accounts did not fall back");
    }
  }

  async function remove() {
    setBusy("delete");
    setError(null);
    try {
      await deleteIamProvider(provider.id);
      onDone({ kind: "deleted" });
    } catch (caught) {
      setBusy(null);
      /* A `409 provider_in_use` here means the world moved under the dialog — somebody
       * provisioned an account between the impact read and the click. The impact is re-read
       * rather than the raw error being printed, so the operator gets the new count and the
       * repair instead of a payload. */
      await load();
      setError(
        caught instanceof Error
          ? caught.message
          : "the provider could not be removed",
      );
    }
  }

  return (
    <div
      data-provider-deletion-dialog={provider.slug}
      role="dialog"
      aria-modal="true"
      aria-label={`Remove ${provider.name}`}
      className="fixed inset-0 z-50 flex items-end justify-center bg-ink/30 p-0 sm:items-center sm:p-4"
    >
      <button type="button" aria-label="Close" onClick={onClose} className="flex-1 cursor-default self-stretch" />

      <div
        ref={panel}
        tabIndex={-1}
        data-provider-deletion-panel
        className="max-h-[90vh] w-full max-w-lg overflow-y-auto rounded-t-2xl border border-line bg-surface p-4 outline-none sm:rounded-2xl"
      >
        <header className="flex items-start justify-between gap-3">
          <div className="flex min-w-0 flex-col gap-1">
            <h2 className="text-[15px] font-semibold text-ink">Remove {provider.name}</h2>
            <p className="text-[12px] text-muted">
              <span data-provider-deletion-slug>{provider.slug}</span> is a directory connector.
              Removing it is reversible only by connecting another one and re-provisioning.
            </p>
          </div>
          <button
            type="button"
            data-provider-deletion-close
            onClick={onClose}
            className="rounded-lg border border-line p-1.5 text-ink transition hover:bg-panel"
            aria-label="Close"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </header>

        {loading ? (
          <div
            data-provider-deletion-loading
            className="mt-4 flex items-center gap-2 rounded-xl border border-line px-3 py-6 text-[12px] text-muted"
          >
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Reading what this would take away…
          </div>
        ) : error && !impact ? (
          /* The count could not be read, so the dialog cannot answer the question it opened to
           * answer. Refusing to offer the delete here is the whole point: a button that works
           * without a count is the button this dialog exists to prevent. */
          <div
            data-provider-deletion-error
            className="mt-4 flex flex-col gap-3 rounded-xl border border-danger/40 bg-danger/5 p-3"
          >
            <p className="flex items-start gap-2 text-[12px] text-caution">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              {error}
            </p>
            <div className="flex gap-2">
              <button
                type="button"
                onClick={() => void load()}
                className="h-8 rounded-lg border border-line px-3 text-[12px] text-ink"
              >
                Try again
              </button>
              <button
                type="button"
                onClick={onClose}
                className="h-8 rounded-lg border border-line px-3 text-[12px] text-ink"
              >
                Close
              </button>
            </div>
          </div>
        ) : impact ? (
          <div className="mt-4 flex flex-col gap-3">
            <div
              data-provider-deletion-count
              data-affected={impact.affected_accounts}
              data-blocked={impact.blocked ? "true" : "false"}
              className="flex flex-col gap-2 rounded-xl border border-line p-3"
            >
              <p className="flex items-start gap-2 text-[13px] font-medium text-ink">
                {impact.blocked ? (
                  <ShieldAlert className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
                ) : null}
                {headline(impact)}
              </p>

              {impact.blocked ? (
                <p className="text-[12px] text-muted">
                  Each of these keeps working and becomes a local account, keeping its sessions
                  and its role grants. The directory that vouched for it is what goes away.
                </p>
              ) : (
                <p className="text-[12px] text-muted">
                  Nothing is provisioned from this connector, so removing it signs nobody out.
                </p>
              )}

              {impact.by_source.length > 0 ? (
                <div className="flex flex-col gap-1.5" data-provider-deletion-breakdown>
                  <span className="text-[11px] font-medium uppercase tracking-wide text-muted">
                    Where they come from
                  </span>
                  <ul className="flex flex-wrap gap-1.5">
                    {impact.by_source.map((row) => (
                      <li
                        key={row.source}
                        data-deletion-source={row.source}
                        className="flex items-center gap-1.5 rounded-full border border-line px-2 py-0.5 text-[11.5px] text-ink"
                      >
                        {sourceLabel(row.source)}
                        <span className="font-medium tabular-nums">{row.count}</span>
                      </li>
                    ))}
                  </ul>
                  {!reconciles ? (
                    /* A total that does not add up is not a rendering bug to hide: a newer
                     * migration may write a source this build cannot name, and the operator has
                     * to be able to see that and report the string. */
                    <p
                      data-provider-deletion-unreconciled
                      className="text-[11.5px] text-caution"
                    >
                      {impact.unknown_sources
                        ? "Some accounts come from a source this version cannot name, so these numbers do not add up to the total."
                        : "These numbers do not add up to the total."}
                    </p>
                  ) : null}
                </div>
              ) : null}
            </div>

            {impact.accounts.length > 0 ? (
              <div className="flex flex-col gap-1.5" data-provider-deletion-accounts>
                <span className="text-[11px] font-medium uppercase tracking-wide text-muted">
                  Accounts
                </span>
                <ul className="flex max-h-40 flex-col gap-1 overflow-y-auto rounded-xl border border-line p-2">
                  {impact.accounts.map((account) => (
                    <li
                      key={account.user_id}
                      data-deletion-account={account.user_id}
                      className="flex items-center justify-between gap-2 text-[12px]"
                    >
                      <span className="truncate text-ink">{account.email}</span>
                      <span className="shrink-0 text-[11px] text-muted">
                        {sourceLabel(account.source)}
                      </span>
                    </li>
                  ))}
                </ul>
                {impact.affected_accounts > impact.accounts.length ? (
                  <p className="text-[11.5px] text-muted">
                    Showing {impact.accounts.length} of {impact.affected_accounts}.
                  </p>
                ) : null}
              </div>
            ) : null}

            {error ? (
              <p data-provider-deletion-inline-error className="text-[12px] text-caution">
                {error}
              </p>
            ) : null}

            <footer className="flex flex-col gap-2 pt-1 sm:flex-row sm:justify-end">
              <button
                type="button"
                onClick={onClose}
                className="h-9 rounded-lg border border-line px-3 text-[12px] text-ink"
              >
                Keep
              </button>
              {impact.blocked ? (
                /* Offered only when the delete is genuinely refused. A provider with nothing
                 * provisioned has nothing to fall back, and a button that appears anyway ends
                 * up sending an empty batch and being told `no_subjects`. */
                <button
                  type="button"
                  data-provider-reassign
                  disabled={busy !== null}
                  onClick={() => void reassign()}
                  className="flex h-9 items-center justify-center gap-1.5 rounded-lg border border-line px-3 text-[12px] text-ink transition hover:bg-panel disabled:opacity-50"
                >
                  {busy === "reassign" ? (
                    <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  ) : (
                    <ArrowRightLeft className="size-3.5" aria-hidden />
                  )}
                  Fall back to local
                </button>
              ) : null}
              <button
                type="button"
                data-provider-delete-final
                data-provider-delete-confirm={provider.slug}
                disabled={busy !== null}
                onClick={() => void remove()}
                className="flex h-9 items-center justify-center gap-1.5 rounded-lg border border-danger/50 px-3 text-[12px] text-caution transition hover:bg-danger/5 disabled:opacity-50"
              >
                {busy === "delete" ? (
                  <Loader2 className="size-3.5 animate-spin" aria-hidden />
                ) : (
                  <Trash2 className="size-3.5" aria-hidden />
                )}
                Remove for good
              </button>
            </footer>
          </div>
        ) : null}
      </div>
    </div>
  );
}

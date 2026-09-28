"use client";

/**
 * The share-link tab of the file detail screen (docs/requests/REQ-010, slice 3).
 *
 * A share link is a way to hand one file to somebody who cannot sign in, and this screen is
 * built around the one property of that which the operator cannot get wrong by accident:
 *
 * **the link is shown once.** The API returns the token exactly once, at creation, because the
 * row stores only its hash — so there is no "copy the link" button on an existing row and there
 * cannot be one. What an existing row offers is a revoke, because revoking a link you cannot
 * re-derive is the only action that still matters. A screen that showed a `Copy` next to a
 * live link would be showing a button that silently copies nothing, and the operator would
 * learn that by handing an empty link to a client.
 *
 * Everything else follows from who the link is for: an expiry is a decision the operator makes
 * (the default is "until revoked", because a platform-chosen expiry silently expires somebody's
 * link), a password is optional and is never shown back, and the download count is the number
 * the operator will quote to the person they shared with.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, Check, Copy, KeyRound, Link2, Loader2, Plus, ShieldOff, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { createMediaShare, fetchMediaShares, revokeAllMediaShares, revokeMediaShare } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { CreatedMediaShare, MediaShare } from "@/lib/types";

/** The longest lifetime the API accepts, in days. Mirrored so the form refuses it before the round trip. */
const MAX_EXPIRY_DAYS = 3650;

/** How a share's state is drawn: a live link is positive, a dead one is quiet, never alarming. */
function stateTone(share: MediaShare): string {
  return share.state === "live" ? "bg-positive-soft text-positive" : "bg-quiet-soft text-muted";
}

/** What the state column says, in words rather than in the API's own vocabulary. */
function stateLabel(share: MediaShare): string {
  switch (share.state) {
    case "revoked":
      return "Revoked";
    case "expired":
      return "Expired";
    default:
      return "Live";
  }
}

/**
 * The share tab: the list of a file's links, the form that makes a new one, and the one-time
 * panel that shows it.
 */
export function SharesTab({ mediaId }: { mediaId: string }) {
  const [shares, setShares] = useState<MediaShare[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [created, setCreated] = useState<CreatedMediaShare | null>(null);
  const [copied, setCopied] = useState(false);

  // The form's own state. Kept here rather than in a form library because there are three fields
  // and one of them (the expiry) has a "no expiry" mode rather than a value.
  const [expiresInDays, setExpiresInDays] = useState("");
  const [password, setPassword] = useState("");
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);

  const load = useCallback(async () => {
    try {
      setShares(await fetchMediaShares(mediaId));
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "the share links could not be read");
    }
  }, [mediaId]);

  useEffect(() => {
    void load();
  }, [load]);

  const live = useMemo(() => shares?.filter((share) => share.state === "live") ?? [], [shares]);

  /** Create a link, after refusing the values the API would refuse anyway. */
  const onCreate = useCallback(
    async (event: React.FormEvent) => {
      event.preventDefault();
      setFieldError(null);
      setNotice(null);

      let days: number | undefined;
      if (expiresInDays.trim() !== "") {
        days = Number(expiresInDays);
        if (!Number.isInteger(days) || days < 1) {
          setFieldError({
            field: "expires_in_days",
            message: "Enter a whole number of days, at least 1 — or leave it empty for a link that lasts until you revoke it.",
          });
          return;
        }
        if (days > MAX_EXPIRY_DAYS) {
          setFieldError({
            field: "expires_in_days",
            message: `A link may last at most ${MAX_EXPIRY_DAYS} days. Leave it empty for a link that lasts until you revoke it.`,
          });
          return;
        }
      }

      setBusy(true);
      try {
        const result = await createMediaShare(mediaId, {
          ...(days !== undefined ? { expiresInDays: days } : {}),
          ...(password.trim() !== "" ? { password } : {}),
        });
        setCreated(result);
        setExpiresInDays("");
        setPassword("");
        setNotice(result.notice);
        // Re-read rather than appending the returned row: the list's `state` is resolved by the
        // API against the clock, and a locally built row would have to guess it.
        await load();
      } catch (err) {
        const apiError = err as { code?: string; message?: string };
        // A refusal that names a field goes under that field; anything else goes at the top,
        // because there is no input to point at.
        if (apiError.code === "expires_in_days" || apiError.code === "password_rejected") {
          setFieldError({ field: "expires_in_days", message: apiError.message ?? "refused" });
        } else {
          setError(apiError.message ?? "the share link could not be created");
        }
      } finally {
        setBusy(false);
      }
    },
    [expiresInDays, password, mediaId, load],
  );

  const onRevoke = useCallback(
    async (share: MediaShare) => {
      setBusy(true);
      setError(null);
      try {
        await revokeMediaShare(mediaId, share.id);
        await load();
      } catch (err) {
        setError(err instanceof Error ? err.message : "the share link could not be revoked");
      } finally {
        setBusy(false);
      }
    },
    [mediaId, load],
  );

  const onRevokeAll = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await revokeAllMediaShares(mediaId);
      setNotice(
        result.revoked === 0
          ? "There were no live links left to revoke."
          : `${result.revoked} link${result.revoked === 1 ? "" : "s"} revoked. Anyone holding one of them can no longer open this file.`,
      );
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "the share links could not be revoked");
    } finally {
      setBusy(false);
    }
  }, [mediaId, load]);

  const onCopy = useCallback(async () => {
    if (!created) {
      return;
    }
    try {
      await navigator.clipboard.writeText(created.url);
      setCopied(true);
      // The tick is feedback, not state: it goes away on its own so a second copy does not
      // leave a "Copied" that is no longer true.
      setTimeout(() => setCopied(false), 2500);
    } catch {
      setNotice("Copying is blocked in this browser — select the link above and copy it by hand.");
    }
  }, [created]);

  return (
    <section className="flex flex-col gap-4" aria-label="Share links">
      {/* The one-time panel. It is first because it is the only moment the link exists. */}
      {created ? (
        <div
          className="flex flex-col gap-3 border border-accent/40 bg-accent-soft/40 p-4"
          data-testid="media-share-created"
        >
          <div className="flex items-start gap-2">
            <Link2 aria-hidden className="mt-0.5 size-4 shrink-0 text-accent-strong" />
            <div className="flex flex-col gap-1">
              <h3 className="text-[13px] font-semibold">Copy this link now</h3>
              <p className="text-[12px] text-muted">
                It is shown once. The platform stores only a hash of it, so nobody — including an
                administrator — can read it back later. If you lose it, revoke this link and make
                another.
              </p>
            </div>
          </div>
          <div className="flex flex-col gap-2 sm:flex-row">
            <input
              aria-label="The new share link"
              className="min-w-0 flex-1 border border-line bg-surface px-3 py-2 font-mono text-[12px]"
              readOnly
              value={created.url}
              onFocus={(event) => event.currentTarget.select()}
            />
            <button
              type="button"
              onClick={onCopy}
              className="inline-flex shrink-0 items-center justify-center gap-2 border border-line px-3 py-2 text-[12px] hover:bg-quiet-soft"
            >
              {copied ? <Check aria-hidden className="size-3.5" /> : <Copy aria-hidden className="size-3.5" />}
              {copied ? "Copied" : "Copy"}
            </button>
            <button
              type="button"
              onClick={() => setCreated(null)}
              className="inline-flex shrink-0 items-center justify-center gap-2 border border-line px-3 py-2 text-[12px] hover:bg-quiet-soft"
            >
              <X aria-hidden className="size-3.5" />
              Done
            </button>
          </div>
        </div>
      ) : null}

      {error ? (
        <p role="alert" className="border border-line bg-quiet-soft px-3 py-2 text-[12px]">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p role="status" className="border border-line bg-quiet-soft px-3 py-2 text-[12px]">
          {notice}
        </p>
      ) : null}

      {/* The list. A revoked link stays on it, because "this link was handed out and no longer
          works" is a question the screen has to be able to answer. */}
      {shares === null ? (
        <div className="flex items-center gap-2 px-1 py-4 text-[12px] text-muted">
          <Loader2 aria-hidden className="size-3.5 animate-spin" />
          Loading share links…
        </div>
      ) : shares.length === 0 ? (
        <EmptyState
          title="No share links yet"
          hint="A share link lets somebody download this file without an account. You can set an expiry and a password, and you can revoke it at any time."
        />
      ) : (
        <div className="overflow-x-auto border border-line">
          <table className="w-full text-left text-[12px]">
            <caption className="sr-only">Share links over this file</caption>
            <thead className="border-b border-line bg-quiet-soft/60">
              <tr>
                <th scope="col" className="px-3 py-2 font-medium">State</th>
                <th scope="col" className="px-3 py-2 font-medium">Expires</th>
                <th scope="col" className="px-3 py-2 font-medium">Protection</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Downloads</th>
                <th scope="col" className="px-3 py-2 font-medium">Created</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {shares.map((share) => (
                <tr key={share.id} className="border-b border-line last:border-0">
                  <td className="px-3 py-2">
                    <span
                      className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[11px] font-medium ${stateTone(share)}`}
                      data-testid={`media-share-state-${share.id}`}
                    >
                      {share.state === "live" ? null : <AlertTriangle aria-hidden className="size-3" />}
                      {stateLabel(share)}
                    </span>
                    {share.revoked_reason ? (
                      <span className="mt-1 block text-[11px] text-muted">{share.revoked_reason}</span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2">
                    {share.expires_at ? (
                      <>
                        {formatTimestamp(share.expires_at)}
                        {share.state === "expired" ? (
                          <span className="block text-[11px] text-muted">expired</span>
                        ) : null}
                      </>
                    ) : (
                      <span className="text-muted">When you revoke it</span>
                    )}
                  </td>
                  <td className="px-3 py-2">
                    {share.has_password ? (
                      <span className="inline-flex items-center gap-1.5">
                        <KeyRound aria-hidden className="size-3.5 text-muted" />
                        Password
                      </span>
                    ) : (
                      <span className="text-muted">None</span>
                    )}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums">{share.download_count}</td>
                  <td className="px-3 py-2 text-muted">{formatTimestamp(share.created_at)}</td>
                  <td className="px-3 py-2 text-right">
                    {share.state === "live" ? (
                      <button
                        type="button"
                        onClick={() => onRevoke(share)}
                        disabled={busy}
                        className="inline-flex items-center gap-1.5 border border-line px-2.5 py-1.5 hover:bg-quiet-soft disabled:opacity-50"
                        data-testid={`media-share-revoke-${share.id}`}
                      >
                        <ShieldOff aria-hidden className="size-3.5" />
                        Revoke
                      </button>
                    ) : (
                      <span className="text-[11px] text-muted">
                        {share.revoked_at ? `closed ${formatTimestamp(share.revoked_at)}` : "closed"}
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {live.length > 0 ? (
        <div className="flex justify-end">
          <button
            type="button"
            onClick={onRevokeAll}
            disabled={busy}
            className="inline-flex items-center gap-2 border border-line px-3 py-2 text-[12px] hover:bg-quiet-soft disabled:opacity-50"
          >
            <ShieldOff aria-hidden className="size-3.5" />
            Revoke all {live.length} live link{live.length === 1 ? "" : "s"}
          </button>
        </div>
      ) : null}

      {/* The form. Every field is labelled, and the expiry field says what leaving it empty
          means — a link with a hidden lifetime is a link the operator cannot reason about. */}
      <form onSubmit={onCreate} className="flex flex-col gap-3 border border-line p-4">
        <h3 className="text-[13px] font-semibold">Create a share link</h3>
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1">
            <label htmlFor="share-expires" className="text-[12px] font-medium">
              Expires in (days)
            </label>
            <input
              id="share-expires"
              type="number"
              min={1}
              max={MAX_EXPIRY_DAYS}
              inputMode="numeric"
              value={expiresInDays}
              onChange={(event) => setExpiresInDays(event.target.value)}
              placeholder="Leave empty for no expiry"
              aria-describedby="share-expires-hint"
              className="border border-line bg-surface px-3 py-2 text-[12px]"
            />
            <p id="share-expires-hint" className="text-[11px] text-muted">
              Empty means the link works until you revoke it.
            </p>
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="share-password" className="text-[12px] font-medium">
              Password (optional)
            </label>
            <input
              id="share-password"
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              placeholder="None"
              aria-describedby="share-password-hint"
              className="border border-line bg-surface px-3 py-2 text-[12px]"
            />
            <p id="share-password-hint" className="text-[11px] text-muted">
              At least 10 characters. It is stored hashed and never shown again.
            </p>
          </div>
        </div>
        {fieldError ? (
          <p role="alert" className="text-[12px] text-accent-strong" data-testid="share-field-error">
            {fieldError.message}
          </p>
        ) : null}
        <div>
          <button
            type="submit"
            disabled={busy}
            className="inline-flex items-center gap-2 border border-line px-3 py-2 text-[12px] hover:bg-quiet-soft disabled:opacity-50"
            data-testid="share-create"
          >
            {busy ? (
              <Loader2 aria-hidden className="size-3.5 animate-spin" />
            ) : (
              <Plus aria-hidden className="size-3.5" />
            )}
            Create link
          </button>
        </div>
      </form>
    </section>
  );
}

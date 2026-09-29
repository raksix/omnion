"use client";

/**
 * `/security/sign-in-protection` — the brute-force policy and the accounts it has locked
 * (REQ-012, slice 3).
 *
 * Two halves that belong on one screen because one causes the other: the policy decides when an
 * account is locked, and the list is who that policy has locked *so far*. Split across two
 * screens, the operator tuning `attempts` cannot see the accounts the current setting has
 * already caught, which is the number that tells them whether the setting is right.
 *
 * What makes this screen easy to get dangerously wrong, and how each part is built to resist it:
 *
 * 1. **The unlock button is one click, and it says what it does.** The REQ names the risk
 *    directly: lockout can be weaponised against a known account, so a person locked out by
 *    somebody else needs a path that does not require the platform to have failed. That cuts
 *    both ways — a confirmation dialog behind which the operator has to hunt for the button is
 *    a path that does not exist at 3am. The audit entry carries the actor, so the record of
 *    *who* released a lock is on the server, not in the politeness of the dialog.
 * 2. **The empty state is a fact, not a shrug.** "No accounts are locked" is a *successful
 *    state* of a brute-force policy and is stated as one, with the policy that produced it
 *    shown above it. An empty table that renders as a broken table is the screen's worst
 *    possible failure: it reads as "the lockout is not working" when it means the opposite.
 * 3. **The bounds come from the server.** `bounds` is part of the document so the number inputs
 *    carry their own `min`/`max` and the refusal text quotes the same range the validator uses.
 *    Hard-coding `1..50` in the form as well would be a second source of truth, and the two
 *    would disagree the first time a range changed.
 * 4. **"Progressive delay" is explained, not just labelled.** It is the switch most likely to
 *    be toggled by somebody who does not know what it does, and it is the one that decides
 *    whether an attacker's *time* or the attacker's *attempts* is the thing being priced. The
 *    row carries the arithmetic it produces from the current numbers.
 * 5. **The counter is shown per locked account.** `failed_sign_in_count` is what proves the
 *    threshold that fired was the one configured here, and it is the first thing somebody
 *    questions when a legitimate user is locked out.
 *
 * Keyboard: `Ctrl/Cmd+S` saves, the toggles are real checkboxes, and the unlock buttons are
 * reachable in table order. Mobile: the table becomes cards, each carrying the address, the
 * remaining time and its own button — a table row that wraps a button into an unreachable
 * second line is the classic small-screen failure and is why this list is a card list at
 * `sm` and below.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { KeyRound, Loader2, RotateCcw, Save, Unlock } from "lucide-react";

import {
  fetchLockedAccounts,
  fetchSignInProtection,
  saveSignInProtection,
  unlockAccount,
  type ApiError,
} from "@/lib/api";
import { SecurityTabs } from "@/features/security/security-tabs";
import type {
  LockedAccount,
  LockedAccountsPage,
  LockoutPolicy,
  SignInProtectionDocument,
} from "@/lib/types";

/** The editable policy, as text, so an emptied field is empty rather than zero. */
type Draft = {
  window_seconds: string;
  attempts: string;
  lockout_minutes: string;
  base_delay_seconds: string;
  progressive_delay: boolean;
  reset_on_success: boolean;
};

function draftFrom(policy: LockoutPolicy): Draft {
  return {
    window_seconds: String(policy.window_seconds),
    attempts: String(policy.attempts),
    lockout_minutes: String(policy.lockout_minutes),
    base_delay_seconds: String(policy.base_delay_seconds),
    progressive_delay: policy.progressive_delay,
    reset_on_success: policy.reset_on_success,
  };
}

function policyOf(policy: LockoutPolicy): unknown {
  return { ...policy };
}

/** A duration a person can check against a clock, and never a bare number of minutes. */
function humanDuration(minutes: number): string {
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 1440) return `${Math.round(minutes / 60)}h`;
  const days = minutes / 1440;
  return `${Number.isInteger(days) ? days : days.toFixed(1)}d`;
}

function humanSeconds(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  return `${Math.round(seconds / 3600)}h`;
}

/** The delay schedule the current numbers produce, for the progressive-delay row. */
function delayLadder(base: number, attempts: number, max: number): string {
  const ladder: number[] = [];
  for (let step = 0; step < attempts; step += 1) {
    const delay = Math.min(max, base * 2 ** step);
    ladder.push(delay);
    if (delay >= max) break;
  }
  return ladder.map((delay) => humanSeconds(delay)).join(" → ");
}

export function SignInProtectionScreen() {
  const [document_, setLoaded] = useState<SignInProtectionDocument | null>(null);
  const [locked, setLocked] = useState<LockedAccountsPage | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<ApiError | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);
  const [unlocking, setUnlocking] = useState<string | null>(null);
  const [unlockError, setUnlockError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      // Both halves, and deliberately in parallel: a screen that showed the policy against a
      // list from a different moment would let somebody raise `attempts` while looking at
      // accounts the *old* value locked.
      const [policy, accounts] = await Promise.all([fetchSignInProtection(), fetchLockedAccounts()]);
      setLoaded(policy);
      setDraft(draftFrom(policy.policy));
      setLocked(accounts);
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : "the policy could not be read");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const dirty = useMemo(() => {
    if (!document_ || !draft) return false;
    return JSON.stringify(draftFrom(document_.policy)) !== JSON.stringify(draft);
  }, [document_, draft]);

  /**
   * The refusal to send, before the request.
   *
   * "Not a whole number" only — deliberately, and this is the one place the client is allowed
   * to disagree. An out-of-range number has a *field-level* message the server can produce with
   * the exact range and the reason ("must be between 1 and 50"), and a client copy of that rule
   * would be a second range table to drift. A field that is not a number at all has no
   * server-side meaning to report, so the client refuses it rather than sending `NaN`.
   */
  const localRefusal = useMemo(() => {
    if (!draft) return null;
    for (const key of ["window_seconds", "attempts", "lockout_minutes", "base_delay_seconds"] as const) {
      const value = draft[key];
      if (value.trim() === "" || !Number.isInteger(Number(value))) {
        return `the ${key.replace(/_/g, " ")} must be a whole number`;
      }
    }
    return null;
  }, [draft]);

  const setField = (key: keyof Omit<Draft, "progressive_delay" | "reset_on_success">, value: string) => {
    setDraft((current) => (current ? { ...current, [key]: value } : current));
    setSavedAt(null);
  };

  const setSwitch = (key: "progressive_delay" | "reset_on_success", value: boolean) => {
    setDraft((current) => (current ? { ...current, [key]: value } : current));
    setSavedAt(null);
  };

  const save = useCallback(async () => {
    if (!document_ || !draft || localRefusal) return;
    setSaving(true);
    setSaveError(null);
    setSavedAt(null);
    try {
      const saved = await saveSignInProtection({
        policy: {
          window_seconds: Number(draft.window_seconds),
          attempts: Number(draft.attempts),
          lockout_minutes: Number(draft.lockout_minutes),
          progressive_delay: draft.progressive_delay,
          base_delay_seconds: Number(draft.base_delay_seconds),
          reset_on_success: draft.reset_on_success,
        },
        expected_policy: policyOf(document_.policy),
      });
      setLoaded(saved);
      setDraft(draftFrom(saved.policy));
      setSavedAt(new Date().toISOString());
    } catch (error) {
      setSaveError(error as ApiError);
    } finally {
      setSaving(false);
    }
  }, [document_, draft, localRefusal]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (dirty && !localRefusal && !saving) void save();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [dirty, localRefusal, saving, save]);

  const unlock = useCallback(async (account: LockedAccount) => {
    setUnlocking(account.user_id);
    setUnlockError(null);
    try {
      // The server re-reads the list, so the row disappears because the platform says it has,
      // not because this client decided it should.
      setLocked(await unlockAccount(account.user_id));
    } catch (error) {
      setUnlockError(
        error instanceof Error ? error.message : `${account.email} could not be unlocked`,
      );
    } finally {
      setUnlocking(null);
    }
  }, []);

  if (loading) {
    return (
      <div className="space-y-3" data-sign-in-protection="loading" aria-busy="true">
        <div className="h-5 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-32 animate-pulse rounded-lg bg-quiet-soft" />
        <p className="flex items-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
          Reading the policy
        </p>
      </div>
    );
  }

  if (loadError || !document_ || !draft) {
    return (
      <div className="space-y-3" data-sign-in-protection="error">
        <p role="alert" className="text-[13px] text-danger">
          {loadError}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
        >
          <RotateCcw className="size-3.5" aria-hidden="true" />
          Try again
        </button>
      </div>
    );
  }

  const { bounds } = document_;
  const accounts = locked?.accounts ?? [];
  const ladder = delayLadder(
    Number(draft.base_delay_seconds) || 0,
    Number(draft.attempts) || 0,
    bounds.max_delay_seconds,
  );

  return (
    <div className="space-y-6" data-sign-in-protection="ready">
      <SecurityTabs current="sign-in-protection" />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium text-ink">Sign-in protection</h2>
          <p className="mt-0.5 text-[12.5px] text-muted">
            {document_.is_saved
              ? "A policy saved on this deployment"
              : "The platform baseline — nobody has saved a policy here yet"}
            {" · "}
            <span data-locked-count>
              {locked?.total ?? 0} account{(locked?.total ?? 0) === 1 ? "" : "s"} locked right now
            </span>
          </p>
        </div>
        <div className="flex items-center gap-2">
          {dirty ? (
            <span
              data-sign-in-protection-dirty
              className="rounded border border-warn-soft bg-warn-soft px-2 py-1 text-[12px] text-warn"
            >
              Unsaved changes
            </span>
          ) : null}
          <button
            type="button"
            onClick={() => void save()}
            disabled={!dirty || saving || Boolean(localRefusal)}
            data-sign-in-protection-save
            className="inline-flex items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-[12.5px] font-medium text-accent-ink disabled:opacity-40"
          >
            {saving ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
            ) : (
              <Save className="size-3.5" aria-hidden="true" />
            )}
            Save policy
          </button>
        </div>
      </header>

      {localRefusal ? (
        <p
          role="alert"
          data-sign-in-protection-local-error
          className="rounded-md border border-danger-soft bg-danger-soft px-3 py-2 text-[12.5px] text-danger"
        >
          {localRefusal}
        </p>
      ) : null}

      {saveError ? (
        <p
          role="alert"
          data-sign-in-protection-save-error
          className="rounded-md border border-danger-soft bg-danger-soft px-3 py-2 text-[12.5px] text-danger"
        >
          {saveError.message}
        </p>
      ) : null}

      {savedAt ? (
        <p data-sign-in-protection-saved className="text-[12.5px] text-ok">
          Saved. The next failed sign-in is counted under this policy.
        </p>
      ) : null}

      {/* The four numbers. Each carries its own min/max from the server's `bounds`, so the
          browser refuses a keystroke that could never be valid and the server still owns the
          range that decides the message. */}
      <section aria-labelledby="lockout-numbers" className="rounded-lg border border-line p-4">
        <h3 id="lockout-numbers" className="text-[13.5px] font-medium text-ink">
          When an account is locked
        </h3>
        <div className="mt-3 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <label className="text-[11.5px] text-muted">
            Failures counted over (seconds)
            <input
              type="number"
              inputMode="numeric"
              min={bounds.window_seconds[0]}
              max={bounds.window_seconds[1]}
              value={draft.window_seconds}
              data-lockout-field="window_seconds"
              onChange={(event) => setField("window_seconds", event.target.value)}
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            />
            <span className="text-[11px]">
              {bounds.window_seconds[0]}–{bounds.window_seconds[1]}
            </span>
          </label>
          <label className="text-[11.5px] text-muted">
            Attempts that lock it
            <input
              type="number"
              inputMode="numeric"
              min={bounds.attempts[0]}
              max={bounds.attempts[1]}
              value={draft.attempts}
              data-lockout-field="attempts"
              onChange={(event) => setField("attempts", event.target.value)}
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1.5 text-[12.5px] text-ink"
            />
            <span className="text-[11px]">
              {bounds.attempts[0]}–{bounds.attempts[1]}
            </span>
          </label>
          <label className="text-[11.5px] text-muted">
            Lock lasts (minutes)
            <input
              type="number"
              inputMode="numeric"
              min={bounds.lockout_minutes[0]}
              max={bounds.lockout_minutes[1]}
              value={draft.lockout_minutes}
              data-lockout-field="lockout_minutes"
              onChange={(event) => setField("lockout_minutes", event.target.value)}
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            />
            <span className="text-[11px]">
              {bounds.lockout_minutes[0]}–{bounds.lockout_minutes[1]} (
              {humanDuration(Number(draft.lockout_minutes) || 0)})
            </span>
          </label>
          <label className="text-[11.5px] text-muted">
            First delay (seconds)
            <input
              type="number"
              inputMode="numeric"
              min={bounds.base_delay_seconds[0]}
              max={bounds.base_delay_seconds[1]}
              value={draft.base_delay_seconds}
              data-lockout-field="base_delay_seconds"
              onChange={(event) => setField("base_delay_seconds", event.target.value)}
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            />
            <span className="text-[11px]">
              {bounds.base_delay_seconds[0]}–{bounds.base_delay_seconds[1]}
            </span>
          </label>
        </div>

        <div className="mt-3 space-y-2 border-t border-line/60 pt-3">
          <label className="flex items-start gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={draft.progressive_delay}
              data-lockout-field="progressive_delay"
              onChange={(event) => setSwitch("progressive_delay", event.target.checked)}
              className="mt-0.5 accent-[var(--accent)]"
            />
            <span>
              <span className="text-ink">Delay grows with each failure</span>
              <span className="mt-0.5 block text-[11.5px] text-muted">
                {draft.progressive_delay
                  ? `Each further failure waits twice as long, up to ${humanSeconds(bounds.max_delay_seconds)}: ${ladder || "—"}`
                  : "Every failed attempt is refused immediately, and only the threshold counts"}
              </span>
            </span>
          </label>
          <label className="flex items-start gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={draft.reset_on_success}
              data-lockout-field="reset_on_success"
              onChange={(event) => setSwitch("reset_on_success", event.target.checked)}
              className="mt-0.5 accent-[var(--accent)]"
            />
            <span>
              <span className="text-ink">A successful sign-in clears the counter</span>
              <span className="mt-0.5 block text-[11.5px] text-muted">
                Without this, a user who mistypes twice, signs in, and then mistypes twice more is
                locked by a session that went fine in between.
              </span>
            </span>
          </label>
        </div>
      </section>

      {/* The accounts this policy has locked. */}
      <section aria-labelledby="locked-accounts" className="space-y-2">
        <h3 id="locked-accounts" className="flex items-center gap-1.5 text-[13.5px] font-medium text-ink">
          <KeyRound className="size-3.5 text-muted" aria-hidden="true" />
          Locked accounts
          {locked && locked.total > accounts.length ? (
            <span className="text-[11.5px] font-normal text-muted">
              showing {accounts.length} of {locked.total}
            </span>
          ) : null}
        </h3>

        {unlockError ? (
          <p role="alert" data-unlock-error className="text-[12.5px] text-danger">
            {unlockError}
          </p>
        ) : null}

        {accounts.length === 0 ? (
          /* The empty state is a fact about the platform, and it is the *good* fact. Written
             that way on purpose: a bare "no results" here reads as a broken lockout, which is
             the one conclusion an operator must not draw from it. */
          <p
            data-locked-empty
            className="rounded-lg border border-line bg-quiet-soft/40 px-4 py-6 text-center text-[12.5px] text-muted"
          >
            No accounts are locked. Every account on this deployment has signed in successfully or
            has not failed enough times to lock.
          </p>
        ) : (
          <>
            {/* Desktop: a table. */}
            <div className="hidden overflow-x-auto sm:block">
              <table className="w-full border-collapse text-[12.5px]">
                <caption className="sr-only">
                  Accounts currently locked out, with when each lock ends and an unlock action
                </caption>
                <thead>
                  <tr className="border-b border-line text-left text-muted">
                    <th scope="col" className="py-2 pr-3 font-medium">Account</th>
                    <th scope="col" className="py-2 pr-3 font-medium">Failures</th>
                    <th scope="col" className="py-2 pr-3 font-medium">Unlocks in</th>
                    <th scope="col" className="py-2 pr-3 font-medium">Until</th>
                    <th scope="col" className="py-2 pr-3 font-medium">
                      <span className="sr-only">Action</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {accounts.map((account) => (
                    <tr
                      key={account.user_id}
                      data-locked-row={account.user_id}
                      className="border-b border-line/60 last:border-0"
                    >
                      <th scope="row" className="py-2 pr-3 text-left font-normal text-ink">
                        {account.email}
                      </th>
                      <td className="py-2 pr-3 tabular-nums text-muted">
                        {account.failed_sign_in_count}
                      </td>
                      <td
                        data-locked-remaining={account.user_id}
                        className="py-2 pr-3 tabular-nums text-muted"
                      >
                        {humanSeconds(account.seconds_remaining)}
                      </td>
                      <td className="py-2 pr-3 tabular-nums text-muted">{account.locked_until}</td>
                      <td className="py-2 pr-3 text-right">
                        <button
                          type="button"
                          onClick={() => void unlock(account)}
                          disabled={unlocking === account.user_id}
                          data-unlock={account.user_id}
                          className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-40"
                        >
                          {unlocking === account.user_id ? (
                            <Loader2 className="size-3 animate-spin" aria-hidden="true" />
                          ) : (
                            <Unlock className="size-3" aria-hidden="true" />
                          )}
                          Unlock
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            {/* Mobile: cards, each with its own button inside it. A table row that pushes the
                button onto a wrapped second line is the failure this list avoids. */}
            <ul className="space-y-2 sm:hidden" data-locked-cards>
              {accounts.map((account) => (
                <li
                  key={account.user_id}
                  data-locked-card={account.user_id}
                  className="rounded-lg border border-line p-3"
                >
                  <p className="truncate text-[13px] font-medium text-ink">{account.email}</p>
                  <p className="mt-0.5 text-[11.5px] text-muted">
                    {account.failed_sign_in_count} failed attempts · unlocks in{" "}
                    {humanSeconds(account.seconds_remaining)}
                  </p>
                  <button
                    type="button"
                    onClick={() => void unlock(account)}
                    disabled={unlocking === account.user_id}
                    data-unlock={account.user_id}
                    className="mt-2 inline-flex w-full items-center justify-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-40"
                  >
                    {unlocking === account.user_id ? (
                      <Loader2 className="size-3 animate-spin" aria-hidden="true" />
                    ) : (
                      <Unlock className="size-3" aria-hidden="true" />
                    )}
                    Unlock {account.email}
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>
    </div>
  );
}

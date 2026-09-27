"use client";

/**
 * The step-up prompt (REQ-006, slice 3).
 *
 * Dangerous operations — resetting an account's factors, issuing a machine key, removing a
 * confirmed factor — refuse a session that has not proved identity again recently. Rather than
 * telling the reader "403", the panel asks for their own password and retries the action when
 * the API accepts it.
 */
import { useState } from "react";

import { KeyRound, ShieldCheck } from "lucide-react";

import { ApiError, stepUpSession } from "@/lib/api";

/** Props of [`StepUpPrompt`]. */
export type StepUpPromptProps = {
  /** Whether the dialog is open. */
  open: boolean;
  /** The action that was refused, for the sentence the reader sees. */
  action: string;
  /** Close without proving anything. */
  onClose: () => void;
  /** The step-up succeeded: retry the action. */
  onDone: () => void;
};

/** A small dialog that proves the caller is still the person at the keyboard. */
export function StepUpPrompt({ open, action, onClose, onDone }: StepUpPromptProps) {
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!open) return null;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await stepUpSession({
        ...(password ? { password } : {}),
        ...(!password && code ? { code } : {}),
      });
      setPassword("");
      setCode("");
      onDone();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "Your identity could not be confirmed.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
      data-step-up
      role="dialog"
      aria-modal="true"
      aria-label="Confirm it is you"
    >
      <div className="w-full max-w-sm rounded-xl border border-line bg-surface p-4 shadow-lg">
        <h2 className="flex items-center gap-1.5 text-[13px] font-medium text-ink">
          <ShieldCheck className="size-4 text-accent" aria-hidden />
          Confirm it is you
        </h2>
        <p className="mt-1 text-[12px] text-muted">
          &ldquo;{action}&rdquo; changes credentials or access, so it needs a fresh proof. Enter
          your own password — or a code from an enrolled factor — and the action continues where
          it stopped.
        </p>
        <label className="mt-3 flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Your password</span>
          <input
            type="password"
            value={password}
            data-step-up-password
            autoFocus
            onChange={(event) => setPassword(event.target.value)}
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="mt-2 flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">
            Or a code from an enrolled factor
          </span>
          <input
            value={code}
            data-step-up-code
            inputMode="numeric"
            autoComplete="one-time-code"
            placeholder="123456"
            onChange={(event) => setCode(event.target.value)}
            className="h-9 rounded-lg border border-line bg-surface px-2 font-mono text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        {error ? (
          <p
            role="alert"
            data-step-up-error
            className="mt-2 rounded-lg border border-danger/40 bg-danger-soft px-2.5 py-1.5 text-[12px] text-caution"
          >
            {error}
          </p>
        ) : null}
        <div className="mt-3 flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:bg-panel"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy || (!password && !code)}
            data-step-up-submit
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
          >
            <KeyRound className="size-3.5" aria-hidden />
            {busy ? "Confirming…" : "Confirm"}
          </button>
        </div>
      </div>
    </div>
  );
}

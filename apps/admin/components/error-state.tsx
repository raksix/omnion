"use client";

/**
 * The error state every list screen shows when its read failed.
 *
 * ## Why one component
 *
 * Before this existed each screen hand-rolled its own error block, and the six of them had
 * drifted into three shapes and two of the six had lost the retry entirely — an error strip with
 * no way to ask again is a dead end, and "no dead buttons" is a rule of the platform, not a
 * preference. A screen that cannot read its list is still a screen with one useful action: try
 * again. This is that action, plus the one number an operator needs.
 *
 * ## The request id
 *
 * REQ-051's acceptance box asks for "error state with retry button and request id". The id is the
 * handle the reader quotes: it is in the API's refusal body, it is what correlates this failure
 * with the log line on the server, and it is the difference between "it broke at 14:02" and
 * "request `9f2c…` broke at 14:02". It is printed **only when the API named one** — a network
 * failure never reaches the server, so inventing an id there would be a number that correlates
 * with nothing, which is worse than showing none.
 *
 * The id is selectable text rather than a copy-to-clipboard button on purpose: a one-click copy
 * needs a permission, a focus, a transient confirmation and a way to be discovered, and every one
 * of those is a way for the button to be the thing that does not work.
 */
import { useState } from "react";
import { RotateCcw, TriangleAlert } from "lucide-react";

import { ApiError } from "@/lib/api";

/** A failure the panel can describe, whatever shape it arrived in. */
type ScreenError = {
  /** The sentence to read. */
  message: string;
  /** The id the API named, when it named one. */
  requestId?: string | null;
  /** The machine code behind the sentence, useful next to the id. */
  code?: string | null;
};

/**
 * What a screen's `error` state holds.
 *
 * An `ApiError` rather than a bare string, because flattening an `ApiError` to `.message` at the
 * catch site throws away the request id — and the catch site is the only place that still has it.
 * A screen that stored `string` could not show an id no matter what the component did.
 */
export type ScreenErrorValue = string | Error | ApiError | null;

/**
 * Turn a caught value into something the error state can render.
 *
 * An `ApiError` is kept whole so the id and the code survive; anything else becomes its message.
 * The fallback is only for a throw that carried no message of its own, which in practice means a
 * `TypeError` from a dead connection or a hand-rolled `throw new Error("")`.
 */
export function toScreenError(caught: unknown, fallback: string): ScreenErrorValue {
  if (caught instanceof ApiError) {
    return caught;
  }
  if (caught instanceof Error && caught.message.length > 0) {
    return caught;
  }
  return fallback;
}

type ErrorStateProps = {
  /** What failed, as a string, an `Error` or an `ApiError`. */
  error: string | Error | ApiError | null;
  /** Ask the screen to read again. */
  onRetry: () => void;
  /** The action beside the retry, when the reader has somewhere to go instead. */
  action?: React.ReactNode;
  /** A stable hook for the walkthrough. */
  qa?: string;
  /** `true` while the retry is in flight, so the button cannot be pressed twice. */
  busy?: boolean;
};

/** Read the three things an error can tell us, in one place. */
export function describeError(error: ErrorStateProps["error"]): ScreenError {
  if (!error) {
    return { message: "Something went wrong." };
  }
  if (typeof error === "string") {
    return { message: error };
  }
  if (error instanceof ApiError) {
    return { message: error.message, requestId: error.requestId, code: error.code };
  }
  // A plain `Error` carries no id and no code: it was raised in the browser, so the server never
  // saw the request and there is nothing on the other side to correlate with.
  return { message: error.message };
}

/**
 * The error strip. Rendered in place of a list, not above it: a table header with no rows and a
 * banner above it reads as "the table loaded and found nothing", which is a different problem
 * with a different fix.
 */
export function ErrorState({ error, onRetry, action, qa = "error-state", busy }: ErrorStateProps) {
  const [retrying, setRetrying] = useState(false);
  const { message, requestId, code } = describeError(error);
  const inFlight = busy || retrying;

  const retry = () => {
    setRetrying(true);
    try {
      onRetry();
    } finally {
      // The screen swaps this component out when its read succeeds; if the read fails again the
      // same component is re-rendered and the button is enabled again. The brief unlock only
      // matters when the retry is a no-op, and a spinner that never stops is worse than one that
      // does.
      setRetrying(false);
    }
  };

  return (
    <div
      role="alert"
      data-qa={qa}
      data-qa-request-id={requestId ?? undefined}
      className="flex flex-col items-center gap-3 px-6 py-10 text-center"
    >
      <TriangleAlert className="size-5 text-danger" aria-hidden />
      <div className="space-y-1">
        <p className="text-[13px] font-medium text-ink">{message}</p>
        {code ? <p className="text-[11.5px] text-muted">{code}</p> : null}
        {requestId ? (
          <p className="text-[11.5px] text-muted">
            request{" "}
            <code className="select-all rounded bg-quiet-soft px-1 py-0.5 font-mono text-[11px] text-ink">
              {requestId}
            </code>
          </p>
        ) : null}
      </div>
      <div className="flex items-center gap-2">
        <button
          type="button"
          data-qa={`${qa}-retry`}
          onClick={retry}
          disabled={inFlight}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
        >
          <RotateCcw className="size-3.5" aria-hidden />
          {inFlight ? "Trying again…" : "Try again"}
        </button>
        {action}
      </div>
    </div>
  );
}

/**
 * The compact form, for a screen that keeps its own body and only replaces the list.
 *
 * A strip rather than a centred block, because the reader is mid-screen and the message has to be
 * found without scrolling: an error nobody can see is the same as no error state at all.
 */
export function ErrorStrip({ error, onRetry, qa = "error-strip" }: ErrorStateProps) {
  const { message, requestId, code } = describeError(error);
  return (
    <p
      role="alert"
      data-qa={qa}
      data-qa-request-id={requestId ?? undefined}
      className="flex flex-wrap items-center gap-2 border-b border-line bg-danger-soft px-4 py-2.5 text-[12px] text-danger"
    >
      <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
      <span>{message}</span>
      {code ? <span className="text-[11px] opacity-80">{code}</span> : null}
      {requestId ? (
        <span className="text-[11px] opacity-80">
          request <code className="select-all font-mono">{requestId}</code>
        </span>
      ) : null}
      <button
        type="button"
        data-qa={`${qa}-retry`}
        onClick={onRetry}
        className="underline underline-offset-2"
      >
        Retry
      </button>
    </p>
  );
}

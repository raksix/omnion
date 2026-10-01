"use client";

/**
 * `/ai/guard/about` — what this guard does **not** catch (docs/requests/REQ-105).
 *
 * The other four guard screens describe what is in force. This one describes the gap, and it is
 * the only screen whose job is to say something the operator would rather not hear.
 *
 * Three rules shape it:
 *
 * 1. **The misses lead.** Each row prints `misses` first and `catches` second. The failure this
 *    screen exists to prevent is an operator concluding from a clean events screen that nothing
 *    sensitive has escaped, so the honest half cannot be the footnote under a reassuring half.
 *
 * 2. **The counts are measured, and a zero is stated as a zero.** `labels_disabled` counts labels
 *    whose rule is switched off — `person_name` ships that way because no name list ships with it,
 *    so the honest reading is "this build catches nothing by name", not a silent omission. A screen
 *    that hid a disabled label behind a row would turn "does not work" into "is not listed".
 *
 * 3. **It is `read`, not `manage`.** The API answers it to anyone with `ai.guard.read`, so an
 *    operator who may audit the guard but not reconfigure it can still find out what it misses —
 *    which is the audience most likely to be asked about it in a review.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";

import { Info, ShieldQuestion, TriangleAlert } from "lucide-react";

import { ApiError, type GuardAbout, fetchGuardAbout } from "@/lib/guard-api";

/** Why a refused read reads as a sentence rather than a code. */
function reason(error: unknown): string {
  if (error instanceof ApiError) {
    return error.code === "forbidden"
      ? `This account may not read the guard's configuration (${error.message}).`
      : error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

/** The About screen. */
export function GuardAboutView() {
  const [about, setAbout] = useState<GuardAbout | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setAbout(await fetchGuardAbout());
    } catch (cause: unknown) {
      setError(reason(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (loading) {
    return (
      <p className="text-[13px] text-muted" role="status">
        Reading what this guard does not catch…
      </p>
    );
  }

  if (error || !about) {
    return (
      <div className="rounded-lg border border-line bg-panel p-4" role="alert">
        <p className="text-[13px] text-danger">{error ?? "The guard's risk statement is unreadable."}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-muted/40"
        >
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-5" data-guard-about>
      {/* The headline: a filter that only reports hits is indistinguishable from a complete one. */}
      <section
        className="rounded-lg border border-line bg-panel p-4"
        aria-labelledby="guard-about-headline"
      >
        <h2
          id="guard-about-headline"
          className="flex items-center gap-2 text-[14px] font-medium"
        >
          <ShieldQuestion className="h-4 w-4" aria-hidden="true" />
          This guard is a pattern filter, not a completeness guarantee
        </h2>
        <p className="mt-2 text-[13px] leading-relaxed text-muted">
          Every rule below is a pattern or a checksum. It finds the shapes it was written for and
          passes everything else through untouched, including sensitive text written in a shape it
          was not written for. A clean event log means no rule fired — it does not mean no
          sensitive value left the platform.
        </p>

        <dl className="mt-4 grid gap-3 sm:grid-cols-3">
          <div className="rounded-md border border-line px-3 py-2" data-guard-about-disabled>
            <dt className="text-[12px] text-muted">Labels switched off</dt>
            <dd className="text-[18px] font-medium">
              {about.labels_disabled}
              <span className="ml-1 text-[12px] font-normal text-muted">
                of {about.labels.length}
              </span>
            </dd>
          </div>
          <div className="rounded-md border border-line px-3 py-2">
            <dt className="text-[12px] text-muted">Enabled rules</dt>
            <dd className="text-[18px] font-medium">
              {about.enabled_rules}
              <span className="ml-1 text-[12px] font-normal text-muted">
                of {about.rule_budget} allowed
              </span>
            </dd>
          </div>
          <div className="rounded-md border border-line px-3 py-2">
            <dt className="text-[12px] text-muted">State</dt>
            <dd className="text-[18px] font-medium">
              {about.all_permissive ? "Allow all" : "Enforcing"}
            </dd>
          </div>
        </dl>

        {about.all_permissive ? (
          <p
            className="mt-3 flex items-start gap-2 rounded-md border border-line bg-muted/40 px-3 py-2 text-[13px]"
            data-guard-about-all-permissive
          >
            <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
            <span>
              Every label is set to <strong>allow</strong>, so this guard is installed and
              inspecting nothing. The list below describes what it <em>would</em> catch.
            </span>
          </p>
        ) : null}
      </section>

      {/* The labels themselves: misses first, always. */}
      <section className="rounded-lg border border-line bg-panel" aria-labelledby="guard-about-labels">
        <h2
          id="guard-about-labels"
          className="flex items-center gap-2 border-b border-line px-4 py-3 text-[14px] font-medium"
        >
          <Info className="h-4 w-4" aria-hidden="true" />
          Every label, and what it misses
        </h2>

        {about.labels.length === 0 ? (
          <p className="px-4 py-6 text-[13px] text-muted">
            This build ships no detection labels, so it catches nothing.
          </p>
        ) : (
          <ul className="divide-y divide-line">
            {about.labels.map((label) => (
              <li key={label.key} className="px-4 py-3" data-guard-about-label={label.key}>
                <p className="text-[13px] font-medium">{label.key}</p>

                <p className="mt-1 text-[13px] leading-relaxed" data-guard-about-misses>
                  <span className="font-medium text-danger">Misses: </span>
                  <span className="text-muted">{label.misses}</span>
                </p>
                <p className="mt-1 text-[13px] leading-relaxed" data-guard-about-catches>
                  <span className="font-medium text-muted">Catches: </span>
                  <span className="text-muted">{label.catches}</span>
                </p>
                {label.validator ? (
                  <p className="mt-1 text-[12px] text-muted">
                    Validator: <code className="font-mono">{label.validator}</code>
                  </p>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      <p className="text-[12.5px] text-muted">
        The rules behind these labels are on the{" "}
        <Link href="/ai/guard/rules" className="underline underline-offset-2">
          rules table
        </Link>
        , and what has actually fired is on the{" "}
        <Link href="/ai/guard/events" className="underline underline-offset-2">
          event log
        </Link>
        .
      </p>
    </div>
  );
}

"use client";

/**
 * Getting-started checklist (REQ-050).
 *
 * The first run leaves a short list of things an installation still needs — publish a page,
 * connect a domain — and every item is derived from the platform's own rows, so it ticks
 * itself off as the work happens. The card disappears once everything is done.
 */
import { useEffect, useState } from "react";

import { ArrowRight, Check, Circle } from "lucide-react";
import Link from "next/link";

import { fetchOnboarding } from "@/lib/api";
import type { OnboardingStatus } from "@/lib/types";

/** Checklist card of the overview screen. */
export function GettingStartedCard() {
  const [status, setStatus] = useState<OnboardingStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchOnboarding()
      .then((next) => {
        if (!cancelled) {
          setStatus(next);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  if (!status) {
    return null;
  }
  const open = status.checklist.filter((item) => !item.done);
  if (status.completed && open.length === 0) {
    return null;
  }

  return (
    <section
      aria-label="Getting started"
      className="rounded-xl border border-line bg-surface p-5"
      data-getting-started
    >
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-[13.5px] font-medium">Getting started</h2>
          <p className="mt-1 text-[12.5px] text-muted">
            {status.completed
              ? `${open.length} of ${status.checklist.length} left — they finish themselves as you work.`
              : "Finish the first-run setup to open the rest of the panel."}
          </p>
        </div>
        {status.completed ? null : (
          <Link
            href="/setup"
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            Finish setup
            <ArrowRight className="size-3.5" aria-hidden />
          </Link>
        )}
      </header>

      <ul className="mt-4 flex flex-col gap-2">
        {status.checklist.map((item) => (
          <li key={item.key} className="flex items-center gap-2.5" data-checklist-item={item.key}>
            <span
              aria-hidden
              className={`flex size-5 shrink-0 items-center justify-center rounded-full border ${
                item.done
                  ? "border-positive/30 bg-positive-soft text-positive"
                  : "border-line text-muted"
              }`}
            >
              {item.done ? <Check className="size-3" /> : <Circle className="size-3" />}
            </span>
            <span className="flex min-w-0 flex-1 flex-col leading-tight">
              <span className={`text-[13px] ${item.done ? "text-muted" : "font-medium"}`}>
                {item.label}
              </span>
              <span className="truncate text-[11.5px] text-muted">{item.description}</span>
            </span>
            {item.done ? (
              <span className="text-[11.5px] text-positive">Done</span>
            ) : (
              <Link
                href={item.href}
                className="shrink-0 text-[12px] text-accent-strong transition hover:underline"
              >
                Open
              </Link>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

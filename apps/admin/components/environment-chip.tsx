"use client";

/**
 * The environment chip in the panel header, and the staging banner under it (REQ-017, slice 5).
 *
 * Both surfaces are rendered by the shell rather than by the environments screens, because the
 * acceptance line is about the *panel*: "the environment chip appears in the panel header while
 * staging is active". A chip that only exists on `/environments` answers a question nobody was
 * asking — the question is "which copy am I editing?", and that is asked on the Pages screen.
 *
 * **The banner is undismissable, and that is a real constraint rather than a missing button.**
 * The obvious build is a notice with an ✕, and it is wrong: a dismissable staging banner is
 * dismissed once, on a screen where the answer is not what you are working on, and then it is
 * gone for the rest of the session — which is precisely the mistake it exists to prevent. So
 * there is no dismiss control, no "hide" preference, and no `localStorage` flag; the only way to
 * make it go away is to leave staging, which is the action it is asking for. The `role="status"`
 * announcement matches `TenantStatusBanner`: this is a standing condition, not an event, and
 * re-announcing it on every navigation is noise a screen-reader user pays for on every screen.
 *
 * **What the banner says is the whole value of it.** "You are in staging" is a fact the person
 * supplying already knows. What they need is the two things that are *not* knowable from where
 * they are standing: which copy this is (by name and host, since the host is what a colleague
 * will be sent), and that nothing here reaches production until a promotion is approved. So the
 * copy names the environment, shows the host when there is one, and links to the changes tab —
 * which is the screen where the difference is actually readable.
 */
import Link from "next/link";
import { useEffect, useRef, useState } from "react";

import { AlertTriangle, Check, ChevronDown, FlaskConical } from "lucide-react";

import { StatusBadge } from "@/components/status-badge";
import { useActiveEnvironment } from "@/lib/active-environment";

/**
 * The chip: the current environment, and the list to change it.
 *
 * Rendered for every environment, not only staging. A control that appears only in its unusual
 * state is a control nobody learns, and the production chip is what makes the staging one read as
 * a *change* rather than as a badge that appeared for no reason.
 *
 * Keyboard: the button is a real `button` with `aria-expanded`; the list closes on `Escape` and
 * on a click outside, and `↑`/`↓` move between options. On a phone the list is the same absolute
 * panel — the header row is the one place with no vertical room for anything else.
 */
export function EnvironmentChip() {
  const { environments, environment, isStaging, loaded, select } = useActiveEnvironment();
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  const wrapRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // The chip must not claim an environment before the list has answered: "Production" drawn as a
  // default and then replaced by a staging one is a flash of a *false* statement, and the flash is
  // what a person screenshots. It renders as a skeleton-width pill until the read completes.
  if (!loaded) {
    return (
      <span
        data-env-chip="loading"
        className="h-7 w-32 animate-pulse rounded-full bg-quiet-soft"
        aria-hidden
      />
    );
  }

  const options = environments.length > 0 ? environments : environment ? [environment] : [];
  const onKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const delta = event.key === "ArrowDown" ? 1 : -1;
      setActiveIndex((index) => (index + delta + options.length) % options.length);
    } else if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      const chosen = options[activeIndex];
      if (chosen) {
        select(chosen.id);
        setOpen(false);
      }
    }
  };

  return (
    <div ref={wrapRef} className="relative">
      <button
        type="button"
        data-env-chip={environment?.type ?? "none"}
        data-env-chip-key={environment?.key ?? ""}
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-label={`Environment: ${environment?.name ?? "unknown"}`}
        onClick={() => setOpen((value) => !value)}
        onKeyDown={onKeyDown}
        className={
          isStaging
            ? "flex h-7 items-center gap-1.5 rounded-full bg-caution-soft pl-2.5 pr-2 text-[12.5px] font-medium text-caution transition hover:bg-caution/15"
            : "flex h-7 items-center gap-1.5 rounded-full bg-quiet-soft px-2.5 text-[12.5px] text-muted transition hover:text-ink"
        }
      >
        {isStaging ? (
          <FlaskConical className="size-3.5 shrink-0" aria-hidden />
        ) : (
          <Check className="size-3.5 shrink-0 text-positive" aria-hidden />
        )}
        <span className="max-w-[9rem] truncate">{environment?.name ?? "No environment"}</span>
        <ChevronDown className="size-3 shrink-0 opacity-60" aria-hidden />
      </button>

      {open ? (
        <ul
          role="listbox"
          data-env-chip-list
          className="absolute right-0 z-50 mt-1.5 w-64 overflow-hidden rounded-xl border border-line bg-surface shadow-xl"
        >
          {options.map((option, index) => {
            const selected = option.id === environment?.id;
            return (
              <li key={option.id} role="option" aria-selected={selected}>
                <button
                  type="button"
                  data-env-chip-option={option.key}
                  onMouseEnter={() => setActiveIndex(index)}
                  onClick={() => {
                    select(option.id);
                    setOpen(false);
                  }}
                  className={
                    index === activeIndex
                      ? "flex w-full items-center gap-2 bg-quiet-soft px-3 py-2 text-left"
                      : "flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-quiet-soft"
                  }
                >
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-[13px] font-medium">{option.name}</span>
                    <span className="block truncate text-[11.5px] text-muted">
                      {option.staging_host ?? option.type}
                    </span>
                  </span>
                  {selected ? <Check className="size-3.5 shrink-0 text-positive" aria-hidden /> : null}
                </button>
              </li>
            );
          })}
        </ul>
      ) : null}
    </div>
  );
}

/**
 * The strip that says the panel is inside a staging copy. Renders nothing outside staging.
 *
 * It carries no dismiss control on purpose — see the module comment. The one action it offers is
 * leaving, and the one link it offers is to the changes that have not been promoted.
 */
export function StagingEnvironmentBanner() {
  const { environment, isStaging } = useActiveEnvironment();

  if (!isStaging || environment === null) {
    return null;
  }

  return (
    <div
      role="status"
      data-qa-staging-banner={environment.key}
      className="flex flex-wrap items-center gap-x-3 gap-y-1.5 border-b border-caution/30 bg-caution-soft px-4 py-2.5 sm:px-6"
    >
      <AlertTriangle className="size-4 shrink-0 text-caution" aria-hidden />
      <p className="min-w-0 flex-1 text-[12.5px] leading-relaxed text-caution">
        <span className="font-medium">Staging — {environment.name}.</span> Everything you change
        here stays in this copy
        {environment.staging_host ? (
          <>
            {" "}
            (<span className="font-mono">{environment.staging_host}</span>)
          </>
        ) : null}{" "}
        until a promotion is approved. Production is not affected.
      </p>
      <div className="flex shrink-0 items-center gap-2">
        <StatusBadge status={environment.status} />
        <Link
          href={`/environments/${environment.id}?tab=changes`}
          data-qa-staging-banner-link
          className="rounded-lg border border-caution/40 px-2.5 py-1 text-[12px] font-medium text-caution transition hover:bg-caution/10"
        >
          See unpromoted changes
        </Link>
        <Link
          href={`/environments/${environment.id}`}
          data-qa-staging-banner-exit
          className="rounded-lg bg-caution px-2.5 py-1 text-[12px] font-medium text-canvas transition hover:bg-caution/90"
        >
          Back to environment
        </Link>
      </div>
    </div>
  );
}

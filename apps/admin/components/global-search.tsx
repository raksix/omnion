"use client";

/**
 * The panel's one search box (REQ-002, slice 2).
 *
 * The box sits in the shell's header — 320px beside the site switcher on large screens, a
 * full-width row under the header below that — and the palette opens from it:
 *
 * * a click or `Enter` opens the palette (with whatever the box holds),
 * * `⌘K` / `Ctrl+K` toggles it from anywhere,
 * * `/` focuses the box without opening anything,
 * * typing two characters into the box hands over to the palette.
 *
 * The component also keeps the browser's own trail of visited screens, which is what the
 * palette's "Recently viewed" list is built from.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import { Search } from "lucide-react";
import { usePathname } from "next/navigation";

import { SearchPalette } from "@/components/search-palette";
import { MIN_QUERY } from "@/lib/search-palette";
import { pushRecentView } from "@/lib/search-memory";

type GlobalSearchProps = {
  /** Title of the screen the box sits on — the label its visit is remembered under. */
  title: string;
  /** Placement classes the shell hands in (the box sizes itself to its slot). */
  className?: string;
};

/** The header search box and the palette it opens. */
export function GlobalSearch({ title, className }: GlobalSearchProps) {
  const pathname = usePathname();
  const [open, setOpen] = useState(false);
  const [seed, setSeed] = useState("");
  const [value, setValue] = useState("");
  const boxRef = useRef<HTMLInputElement | null>(null);

  // The trail is the browser's, not the API's: every screen the account lands on is remembered.
  useEffect(() => {
    if (pathname) {
      pushRecentView({ path: pathname, label: title });
    }
  }, [pathname, title]);

  const openPalette = useCallback((query: string) => {
    setSeed(query);
    setValue("");
    setOpen(true);
  }, []);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing = Boolean(
        target &&
          (target.tagName === "INPUT" ||
            target.tagName === "TEXTAREA" ||
            target.tagName === "SELECT" ||
            target.isContentEditable),
      );

      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        if (open) {
          setOpen(false);
        } else {
          setSeed("");
          setValue("");
          setOpen(true);
        }
        return;
      }

      if (event.key === "/" && !typing) {
        event.preventDefault();
        boxRef.current?.focus();
        boxRef.current?.select();
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open]);

  return (
    <div className={className}>
      <div className="relative">
        <Search
          className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted"
          aria-hidden
        />
        <input
          ref={boxRef}
          data-search-box
          type="text"
          value={value}
          placeholder="Search Omnion…"
          aria-label="Search Omnion"
          autoComplete="off"
          spellCheck={false}
          onChange={(event) => {
            const next = event.target.value;
            if (next.trim().length >= MIN_QUERY) {
              openPalette(next);
              return;
            }
            setValue(next);
          }}
          onClick={() => openPalette(value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              openPalette(value);
            } else if (event.key === "Escape") {
              setValue("");
            }
          }}
          className="h-9 w-full rounded-lg border border-line bg-surface pr-14 pl-8 text-[13px] text-ink outline-none transition placeholder:text-muted focus:border-accent focus:ring-2 focus:ring-accent/15"
        />
        <kbd className="pointer-events-none absolute top-1/2 right-2 hidden -translate-y-1/2 rounded border border-line bg-quiet-soft px-1.5 py-0.5 text-[10.5px] text-muted lg:block">
          ⌘K
        </kbd>
      </div>
      {open ? <SearchPalette initialQuery={seed} onClose={() => setOpen(false)} /> : null}
    </div>
  );
}

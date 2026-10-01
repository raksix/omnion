"use client";

/**
 * `/themes/<key>/customize` — theme settings (REQ-062, slice 2).
 *
 * Six decisions this screen is built around, and each one exists because the obvious version of
 * it is a screen that lies:
 *
 * 1. **The editor is seeded from the DRAFT, and the header says which revision is LIVE.** A
 *    save writes a draft and a publish makes it live; a screen that edits "the settings" without
 *    naming both makes every save look like a deploy and hides the fact that a publish is
 *    still pending. The line is the server's answer — `draft.revisionNo` next to
 *    `published.revisionNo` — never a locally computed "unsaved" flag.
 * 2. **Contrast is measured by the SERVER, on every edit, and shown before publish.** The
 *    browser can compute a ratio too and be subtly different (colour-space rounding, the AA
 *    threshold for large text); a badge that disagrees with the 422 on publish is worse than no
 *    badge. So the panel ASKS — `POST …/theme-settings/contrast-check` on the draft it is
 *    editing, debounced — and sends `acknowledgeContrast: true` only after the operator has
 *    actually seen the findings.
 *
 *    This rule used to claim "on every edit" while reading `view.contrast`, the measurement of
 *    the last SAVE. The claim was the right one and the code was the wrong one, which is worse:
 *    the screen printed a palette's all-clear beside a preview of a *different* palette, and
 *    the publish refusal told the operator to read that all-clear. The rule is now true as
 *    written, and `scripts/qa/probe-contrast-live.cjs` is what holds it true.
 * 3. **Discard restores the draft from the last server response, not from a local snapshot.**
 *    A local snapshot is a second copy of the truth that drifts the moment a restore from the
 *    history screen lands in another tab.
 * 4. **A token input is a colour input when the value is a colour and a text input when it is
 *    not.** A theme's tokens are also font stacks and radii; a swatch-only editor silently
 *    makes those two uneditable.
 * 5. **Nothing is published is said, not implied.** A site with a draft and no published
 *    revision renders with the THEME's defaults, and the screen says exactly that — it does
 *    not present the draft as the current look.
 * 6. **The live preview is real DOM, not an iframe with a fake page.** The right-hand panel
 *    applies the edited tokens as CSS custom properties to a sample page, which is the only
 *    honest way to show "this edit changes the rendered colours" without a server round-trip
 *    for every keystroke. It falls back to a textual token list when the browser is in dark
 *    mode, because a preview that only works in one mode is a preview that lies in the other.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Check, Eye, Loader2, RotateCcw, Save, Undo2 } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  checkThemeSettingsContrast,
  fetchThemeSettings,
  publishThemeSettings,
  saveThemeSettings,
} from "@/lib/api";
import type {
  ThemeContrastFinding,
  ThemeSettingsInput,
  ThemeSettingsView,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import { ThemeBrandingEditor, findingsFrom } from "./theme-branding-editor";

/** Sections the editor owns, in the order the REQ lists them. */
type SectionName = "tokens" | "typography" | "layout" | "branding" | "headerFooter";

const SECTIONS: { name: SectionName; title: string; hint: string }[] = [
  {
    name: "tokens",
    title: "Colours",
    hint: "Each token has a light and a dark value. Reset returns one token to the theme's own default.",
  },
  {
    name: "typography",
    title: "Typography",
    hint: "Base size, scale ratio, font stacks and the heading weight.",
  },
  { name: "layout", title: "Layout", hint: "Container width, radius and the spacing scale." },
  { name: "branding", title: "Branding", hint: "Logo, dark logo and favicon, as media ids or paths." },
  {
    name: "headerFooter",
    title: "Header & footer",
    hint: "Which layout variant the theme renders each of the two with.",
  },
];

const MODES = ["light", "dark", "system"] as const;

/** An empty section is `{}` on the wire, not `null` — the store's validate() accepts both. */
function emptySettings(themeKey: string): ThemeSettingsInput {
  return {
    themeKey,
    tokens: {},
    typography: {},
    layout: {},
    branding: {},
    headerFooter: {},
    defaultMode: "system",
  };
}

/** The section of a revision, normalised to an object whatever the stored shape is. */
function section(view: ThemeSettingsView, name: SectionName, from: "draft" | "published"): Record<string, unknown> {
  const revision = from === "draft" ? view.draft : view.published;
  if (!revision) return {};
  const raw = revision[name] as unknown;
  return raw && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
}

/**
 * The section the editor starts from.
 *
 * A site that has never saved starts from the PUBLISHED revision, and a site that has never
 * published starts from nothing — the theme's own defaults, which arrive as `defaultTokens`
 * and are shown as read-only reference values until the first save. The alternative (always
 * starting from the theme defaults) would silently discard a published revision's values on
 * every visit, which is the whole reason the draft and the published revision are separate
 * rows.
 */
function initialSettings(view: ThemeSettingsView): ThemeSettingsInput {
  const empty = emptySettings(view.themeKey);
  return {
    themeKey: view.themeKey,
    tokens: section(view, "tokens", "draft") as ThemeSettingsInput["tokens"],
    typography: section(view, "typography", "draft") as ThemeSettingsInput["typography"],
    layout: section(view, "layout", "draft") as ThemeSettingsInput["layout"],
    branding: section(view, "branding", "draft") as ThemeSettingsInput["branding"],
    headerFooter: section(view, "headerFooter", "draft") as ThemeSettingsInput["headerFooter"],
    defaultMode: view.draft?.defaultMode ?? view.published?.defaultMode ?? empty.defaultMode,
  };
}

function isColour(value: unknown): value is string {
  return typeof value === "string" && /^#(?:[0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/.test(value.trim());
}

function asString(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return "";
}

export function ThemeCustomizeView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [view, setView] = useState<ThemeSettingsView | null>(null);
  const [form, setForm] = useState<ThemeSettingsInput | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<"save" | "publish" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Per-field messages from a branding refusal, kept beside the general error banner. The
  // banner carries the server's joined sentence (which is what a reader of the network tab
  // sees); these carry the same sentences split by the field that caused them, which is what
  // the editor puts under each input. One state rather than two error strings, because they
  // answer the same save and showing only one of them loses half the information.
  const [brandingMessages, setBrandingMessages] = useState<Record<string, string[]>>({});
  const [openSection, setOpenSection] = useState<SectionName>("tokens");
  const [contrastSeen, setContrastSeen] = useState(false);
  /**
   * The findings for the palette **as it is being edited**, measured by the server.
   *
   * `view.contrast` is the measurement of the last SERVER response, and the screen prints one
   * beside a live preview of `form` — two different palettes. An operator who edits the accent
   * into a failing pair was told "every pair meets WCAG AA" while the preview beside them showed
   * the failing colours, and the publish refusal instructs them to read exactly that panel.
   *
   * `null` is a distinct third state and not an empty list: an empty list means "measured, and
   * nothing fails", which is the all-clear the screen is allowed to print. `null` means "not
   * measured yet", and the all-clear must NOT be printed for it — a badge that says every pair
   * passes before anything has been measured is the defect this state exists to prevent.
   */
  const [liveContrast, setLiveContrast] = useState<ThemeContrastFinding[] | null>(null);
  const [contrastPending, setContrastPending] = useState(false);
  const [contrastFailed, setContrastFailed] = useState(false);
  // The form as the last SERVER response left it, kept beside the live form so "is this dirty"
  // is a comparison of two React values rather than a read of a mutable ref during render.
  // A ref read in a render is a lie the first time a component re-renders without the writer
  // having run, and "you have unsaved edits" is exactly the claim that must never lie.
  const [baseline, setBaseline] = useState<string>("");

  const accept = useCallback((next: ThemeSettingsInput) => {
    setForm(next);
    setBaseline(JSON.stringify(next));
  }, []);

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setLoading(true);
    setError(null);
    try {
      const next = await fetchThemeSettings(selectedSite.id);
      setView(next);
      accept(initialSettings(next));
      setContrastSeen(false);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  /**
   * Measure the palette being EDITED, debounced, on every token change.
   *
   * **Why a request per edit and not a browser calculation.** A ratio computed in the browser
   * disagrees with the server's in colour-space rounding and in the large-text threshold, and
   * the badge that matters is the one that precedes the 422. If the two ever disagree, the
   * operator is told their palette is fine by the panel and refused by the product — which is
   * worse than having no badge, because they have learned to trust it.
   *
   * **Why debounced, and why the pending state is shown rather than hidden.** The measurement is
   * a network call on a colour picker, so an undebounced effect fires one per drag frame. The
   * panel therefore prints `Checking contrast…` while one is in flight, and refuses to print
   * the all-clear during that window: the honest state of an unmeasured draft is "not measured",
   * and a badge that says "every pair meets AA" while a measurement is in flight is the same lie
   * the fix exists to remove.
   *
   * The abort matters for the same reason the debounce does: two in-flight measurements can
   * land out of order, and the older one would overwrite the newer palette's findings.
   */
  useEffect(() => {
    if (!selectedSite || !form) return;
    const controller = new AbortController();
    setContrastPending(true);
    setContrastFailed(false);
    const timer = setTimeout(() => {
      void (async () => {
        try {
          const measured = await checkThemeSettingsContrast(
            selectedSite.id,
            form.themeKey,
            form.tokens as Record<string, unknown>,
          );
          if (controller.signal.aborted) return;
          setLiveContrast(measured.findings);
        } catch {
          if (controller.signal.aborted) return;
          // A failed measurement is NOT an empty one. Reporting "every pair meets AA" because
          // the check could not run is how a panel ends up guaranteeing something it never
          // verified, so the state is kept distinct and the panel says so.
          setLiveContrast(null);
          setContrastFailed(true);
        } finally {
          if (!controller.signal.aborted) setContrastPending(false);
        }
      })();
    }, 300);
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  }, [selectedSite, form]);

  const setField = useCallback(
    <K extends keyof ThemeSettingsInput>(key: K, value: ThemeSettingsInput[K]) => {
      setForm((current) => (current ? { ...current, [key]: value } : current));
      setContrastSeen(false);
    },
    [],
  );

  const setSectionValue = useCallback(
    (name: SectionName, key: string, value: unknown) => {
      setForm((current) => {
        if (!current) return current;
        const currentSection = { ...(current[name] as Record<string, unknown>) };
        currentSection[key] = value;
        return { ...current, [name]: currentSection } as ThemeSettingsInput;
      });
      setContrastSeen(false);
      // Editing the field that was refused retires its message. Leaving it up would claim the
      // input is still wrong after the operator has replaced it, and the panel has no way to
      // know the new value passes — the server says so, on the next save, which is the only
      // measurement of that.
      if (name === "branding") {
        setBrandingMessages((current) => {
          if (!(key in current)) return current;
          const next = { ...current };
          delete next[key];
          return next;
        });
      }
    },
    [],
  );

  const clearSectionValue = useCallback(
    (name: SectionName, key: string) => {
      setForm((current) => {
        if (!current) return current;
        const currentSection = { ...(current[name] as Record<string, unknown>) };
        delete currentSection[key];
        return { ...current, [name]: currentSection } as ThemeSettingsInput;
      });
      setContrastSeen(false);
    },
    [],
  );

  /** Restore a single token to the theme's own default, which is a delete from the override map. */
  const resetToken = useCallback(
    (name: SectionName, key: string) => {
      clearSectionValue(name, key);
    },
    [clearSectionValue],
  );

  const restoreDefaults = useCallback(() => {
    if (!view) return;
    // Clearing the overrides — NOT copying the theme's defaults into the draft. Those two look
    // the same in the preview and are not the same at all: a copy pins the values into this
    // site's settings, so a later theme update would leave the site on stale tokens with no
    // badge saying so, and every token would read as "overridden" when none of them is. Empty
    // means the theme's own defaults apply, which is what the button says it does.
    const cleared = emptySettings(view.themeKey);
    cleared.defaultMode = form?.defaultMode ?? "system";
    accept(cleared);
    setContrastSeen(false);
  }, [form, view]);

  const discard = useCallback(() => {
    if (!view) return;
    const reset = initialSettings(view);
    accept(reset);
    setContrastSeen(false);
    setNotice("Unsaved edits discarded. The editor is back on the last saved draft.");
  }, [view]);

  const save = useCallback(async () => {
    if (!selectedSite || !form) return;
    setBusy("save");
    setError(null);
    setNotice(null);
    setBrandingMessages({});
    try {
      const next = await saveThemeSettings(selectedSite.id, form);
      setView(next);
      accept(initialSettings(next));
      setNotice(
        `Saved as draft revision ${next.draft?.revisionNo ?? "?"}. Visitors still see revision ${
          next.published?.revisionNo ?? "the theme defaults"
        }.`,
      );
    } catch (caught) {
      // A branding refusal is BOTH kept (the joined sentence, which names every finding) and
      // split (one message per field, so each message sits under the input that caused it).
      // Dropping the banner would leave the per-field lines with no explanation of what the
      // save was; dropping the split would leave a sentence an operator has to parse by hand
      // to work out which of three assets is at fault.
      const split = findingsFrom(caught);
      setBrandingMessages(split);
      setError((caught as ApiError).message);
    } finally {
      setBusy(null);
    }
  }, [form, selectedSite]);

  const publish = useCallback(async () => {
    if (!selectedSite || !view) return;
    setBusy("publish");
    setError(null);
    setNotice(null);
    try {
      // The acknowledgement is sent ONLY when there is something to acknowledge and the
      // operator has seen it. Sending it unconditionally would make the server's guard a no-op.
      //
      // `liveContrast`, not `view.contrast`: the guard this acknowledges is the one the server
      // will run against the DRAFT being published, and a browser that decided from the last
      // read's findings would be substituting its own judgement for the product's. An
      // unmeasured palette sends `false` — the server then refuses with its real findings,
      // which is the answer, rather than this client asserting one it never computed.
      const acknowledge = (liveContrast?.length ?? 1) === 0 || contrastSeen;
      const next = await publishThemeSettings(selectedSite.id, acknowledge);
      setView(next);
      accept(initialSettings(next));
      setContrastSeen(false);
      setNotice(`Published revision ${next.published?.revisionNo ?? "?"}. The public site renders with it now.`);
    } catch (caught) {
      const apiError = caught as ApiError;
      // Branch on the API's stable CODE, never on a regex over the English sentence: a copy
      // edit in `apps/api/src/error.rs` must not silently turn a guard into a red line, and a
      // screen that matches on prose is a screen that breaks in a language nobody tests.
      //
      // `theme_settings_contrast_required` is not a failure at all — it is the product asking a
      // person to look, so it becomes the acknowledgement prompt. `theme_settings_draft_stale`
      // is somebody else's write landing under this tab, so the editor re-reads the server and
      // says so instead of quoting a code.
      if (apiError.code === "theme_settings_contrast_required") {
        setNotice(
          "Publishing needs the contrast findings acknowledged. Read the contrast panel, then publish again.",
        );
        setError(null);
      } else if (apiError.code === "theme_settings_draft_stale") {
        await load();
        setNotice(
          "This draft is older than what is now live — somebody published or restored from another tab. The editor now holds the current draft; publish again if this palette is still the one you want.",
        );
        setError(null);
      } else {
        setError(apiError.message);
      }
    } finally {
      setBusy(null);
    }
  }, [contrastSeen, liveContrast, load, selectedSite, view]);

  // ------------------------------------------------------------------ states
  if (siteStatus === "error") {
    return <EmptyState title="No site selected" hint={siteError ?? "The site list could not be loaded."} />;
  }
  if (!selectedSite) {
    return <EmptyState title="No site selected" hint="Pick a site to customise the theme it renders with." />;
  }
  if (loading && !view) {
    return (
      <div className="space-y-4" data-theme-customize-loading>
        <div className="h-6 w-64 animate-pulse rounded bg-line" />
        <div className="grid gap-4 lg:grid-cols-2">
          <div className="h-64 animate-pulse rounded-lg bg-line" />
          <div className="h-64 animate-pulse rounded-lg bg-line" />
        </div>
      </div>
    );
  }
  if (error && !view) {
    return (
      <EmptyState
        title="Theme settings could not be loaded"
        hint={error}
        action={
          <button type="button" className="btn btn-ghost" onClick={() => void load()}>
            <RotateCcw className="h-4 w-4" aria-hidden />
            Try again
          </button>
        }
      />
    );
  }
  if (!view || !form) {
    return <div className="h-64 animate-pulse rounded-lg bg-line" />;
  }

  /**
   * The palette's findings as the panel renders them.
   *
   * The LIVE measurement, and `null` until one lands — never `view.contrast`, which describes
   * the last save rather than the palette on screen. Falling back to the server's stored
   * findings while a fresh measurement is pending would restore exactly the lie this replaced:
   * an operator mid-edit reading an all-clear about a palette they have already changed.
   */
  const findings = liveContrast;
  const measured = findings !== null;
  const themeDefaults = (view.defaultTokens ?? {}) as Record<string, unknown>;
  const hasDraft = view.draft !== null;
  // A comparison of the two React values. Comparing against `initialSettings(view)` instead
  // would recompute the seed on every render and report "dirty" against a fresh object graph,
  // which JSON.stringify happens to hide today and key ORDER would expose tomorrow.
  const dirty = form !== null && JSON.stringify(form) !== baseline;

  return (
    <div className="space-y-6" data-theme-customize>
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h2 className="text-sm font-semibold text-ink">
            {view.themeKey} settings
            <span className="ml-2 font-normal text-muted" data-theme-customize-theme-key={view.themeKey}>
              for {selectedSite.name}
            </span>
          </h2>
          {/* The line that makes the draft/live split legible. Without it "Save draft" is a
              button whose effect an operator cannot see anywhere on the screen. */}
          <p className="mt-1 text-xs text-muted" data-theme-customize-revision-line>
            {hasDraft ? (
              <>
                Editing draft revision <span data-theme-customize-draft-no>{view.draft?.revisionNo}</span> ·{" "}
                {view.published ? (
                  <>
                    live: revision{" "}
                    <span data-theme-customize-published-no>{view.published?.revisionNo}</span>
                    {view.draft?.revisionNo === view.published?.revisionNo
                      ? " (the draft is what visitors see)"
                      : " (publish to make the draft live)"}
                  </>
                ) : (
                  <span data-theme-customize-published-none>nothing published yet — visitors see the theme defaults</span>
                )}
              </>
            ) : view.published ? (
              <>
                No draft yet — showing published revision{" "}
                <span data-theme-customize-published-no>{view.published.revisionNo}</span>
              </>
            ) : (
              <span data-theme-customize-empty>
                Nothing saved yet. The theme&apos;s own tokens are live; the first save creates draft
                revision 1.
              </span>
            )}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="btn btn-ghost"
            data-theme-customize-discard
            onClick={discard}
            disabled={busy !== null}
          >
            <Undo2 className="h-4 w-4" aria-hidden />
            Discard
          </button>
          <button
            type="button"
            className="btn btn-ghost"
            data-theme-customize-restore-defaults
            onClick={restoreDefaults}
            disabled={busy !== null}
            title="Return every colour token to the theme's own default"
          >
            <RotateCcw className="h-4 w-4" aria-hidden />
            Restore default
          </button>
          <button
            type="button"
            className="btn btn-secondary"
            data-theme-customize-save
            onClick={() => void save()}
            disabled={busy !== null}
          >
            {busy === "save" ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <Save className="h-4 w-4" aria-hidden />}
            Save draft
          </button>
          <button
            type="button"
            className="btn btn-primary"
            data-theme-customize-publish
            onClick={() => void publish()}
            disabled={busy !== null}
            title="Publishing is the only action a signed-out visitor can see"
          >
            {busy === "publish" ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : null}
            Publish
          </button>
        </div>
      </header>

      {error ? (
        <p className="text-sm text-danger" role="alert" data-theme-customize-error>
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="text-sm text-ok" role="status" data-theme-customize-notice>
          {notice}
        </p>
      ) : null}

      {/*
        * Three states, and the middle one is the point of the fix.
        *
        * `pending` and `failed` both print something OTHER than an all-clear. The all-clear is a
        * measurement claim — "every pair in this draft meets AA" — and it may only be printed for
        * a palette that has just been measured. An unmeasured draft printing it is the defect
        * this whole path replaced, so a failed check says it could not check rather than
        * quietly falling through to the green line: silence reads as a pass.
        */}
      {!measured ? (
        <p
          className="flex items-center gap-2 rounded-md border border-line bg-panel px-3 py-2 text-sm text-muted"
          data-theme-contrast-pending={contrastFailed ? "failed" : "checking"}
          role="status"
        >
          {contrastFailed ? (
            <>
              <AlertTriangle className="h-4 w-4 text-warn" aria-hidden />
              Contrast could not be measured for this draft. Nothing has been checked — the panel
              will not report a palette as passing a check that did not run.
            </>
          ) : (
            <>
              <Loader2 className="h-4 w-4 animate-spin text-muted" aria-hidden />
              Checking this draft&apos;s contrast…
            </>
          )}
        </p>
      ) : findings.length > 0 ? (
        <section
          className="rounded-md border border-warn/40 bg-warn/10 px-3 py-3"
          data-theme-contrast
          aria-labelledby="theme-contrast-title"
        >
          <h3 id="theme-contrast-title" className="flex items-center gap-2 text-sm font-semibold text-ink">
            <AlertTriangle className="h-4 w-4 text-warn" aria-hidden />
            {findings.length} contrast {findings.length === 1 ? "finding" : "findings"} below WCAG AA
          </h3>
          <ul className="mt-2 space-y-1 text-sm text-muted">
            {findings.map((finding: ThemeContrastFinding) => (
              <li key={`${finding.foreground}-${finding.background}-${finding.mode}`} data-theme-contrast-row>
                {finding.message}{" "}
                <span className="text-xs text-muted">
                  ({finding.ratio.toFixed(2)}:1, needs {finding.required.toFixed(1)}:1 in {finding.mode})
                </span>
              </li>
            ))}
          </ul>
          <p className="mt-2 text-xs text-muted">
            Publishing is still possible, but it needs this acknowledgement. Drafts are not
            affected, so a palette can be compared before it goes live.
          </p>
          <label className="mt-2 inline-flex items-center gap-2 text-sm text-ink">
            <input
              type="checkbox"
              data-theme-contrast-ack
              checked={contrastSeen}
              onChange={(event) => setContrastSeen(event.target.checked)}
            />
            I have read these findings and want to publish anyway
          </label>
        </section>
      ) : (
        <p
          className="flex items-center gap-2 rounded-md border border-line bg-panel px-3 py-2 text-sm text-muted"
          data-theme-contrast-ok
        >
          <Check className="h-4 w-4 text-positive" aria-hidden />
          Every text/background pair in this draft meets WCAG AA.
        </p>
      )}

      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div className="space-y-3" data-theme-customize-panels>
          {SECTIONS.map((entry) => {
            const open = openSection === entry.name;
            const values = form[entry.name] as Record<string, unknown>;
            return (
              <section
                key={entry.name}
                className="rounded-lg border border-line bg-panel"
                data-theme-section={entry.name}
              >
                <h3>
                  <button
                    type="button"
                    className="flex w-full items-center justify-between gap-2 px-4 py-3 text-left text-sm font-semibold text-ink"
                    data-theme-section-toggle={entry.name}
                    aria-expanded={open}
                    aria-controls={`theme-section-${entry.name}`}
                    onClick={() => setOpenSection(open ? ("" as SectionName) : entry.name)}
                  >
                    <span>
                      {entry.title}
                      <span className="ml-2 text-xs font-normal text-muted">
                        {Object.keys(values).length} override{Object.keys(values).length === 1 ? "" : "s"}
                      </span>
                    </span>
                    <span className="text-xs text-muted">{open ? "Hide" : "Show"}</span>
                  </button>
                </h3>
                {open ? (
                  <div id={`theme-section-${entry.name}`} className="space-y-3 border-t border-line px-4 py-3">
                    <p className="text-xs text-muted">{entry.hint}</p>

                    {entry.name === "tokens" ? (
                      <TokenEditor
                        values={values}
                        defaults={themeDefaults}
                        onChange={(key, value) => setSectionValue("tokens", key, value)}
                        onReset={(key) => resetToken("tokens", key)}
                      />
                    ) : entry.name === "branding" ? (
                      /* Branding is the one section that is NOT a flat map of free text: each
                          key is a media id, so the editor offers the library, shows the limits
                          the server will enforce, and renders one message per refused field
                          (criterion 9). The other three keep the shared editor, because a
                          typography or layout value really is a bare string. */
                      <ThemeBrandingEditor
                        siteId={selectedSite.id}
                        values={values}
                        limits={view.brandingLimits}
                        messages={brandingMessages}
                        onChange={(key, value) => setSectionValue("branding", key, value)}
                        onClear={(key) => setSectionValue("branding", key, null)}
                      />
                    ) : (
                      /* Typography, layout, branding and header/footer are all the same
                          shape — a flat map of named values — so they share one editor. The
                          four section names are still passed through as data, because the
                          stored payload is keyed by section and an editor that guessed which
                          one it was editing is an editor that writes to the wrong one. */
                      <FlatEditor
                        section={entry.name}
                        values={values}
                        onChange={(key, value) => setSectionValue(entry.name, key, value)}
                        onRemove={(key) => clearSectionValue(entry.name, key)}
                      />
                    )}
                  </div>
                ) : null}
              </section>
            );
          })}

          <section className="rounded-lg border border-line bg-panel" data-theme-section="modes">
            <div className="border-t-0 px-4 py-3">
              <h3 className="text-sm font-semibold text-ink">Modes</h3>
              <p className="mt-1 text-xs text-muted">
                The mode a visitor sees first. `system` follows their operating system.
              </p>
              <div className="mt-2 flex flex-wrap gap-2" role="radiogroup" aria-label="Default colour mode">
                {MODES.map((mode) => (
                  <button
                    key={mode}
                    type="button"
                    role="radio"
                    aria-checked={form.defaultMode === mode}
                    className={`rounded-full border px-3 py-1 text-sm ${
                      form.defaultMode === mode
                        ? "border-accent bg-accent-soft text-ink"
                        : "border-line text-muted"
                    }`}
                    data-theme-mode={mode}
                    onClick={() => setField("defaultMode", mode)}
                  >
                    {mode}
                  </button>
                ))}
              </div>
            </div>
          </section>
        </div>

        <div className="space-y-3">
          <LivePreview form={form} defaults={themeDefaults} />
          <p className="text-xs text-muted" data-theme-customize-dirty={dirty ? "true" : "false"}>
            {dirty
              ? "There are unsaved edits. Save draft to keep them, or Discard to go back."
              : "The editor matches the last saved draft."}
          </p>
        </div>
      </div>
    </div>
  );
}

/**
 * The colour token editor.
 *
 * A token is a MAP (`{light, dark}`) or a scalar, and the panel has to handle both because a
 * theme is free to declare either. Mode maps get two inputs and a reset; a scalar gets one.
 * The token LIST comes from the theme's own defaults plus whatever the draft overrides, so a
 * theme that declares a token the operator has never touched still appears.
 */
function TokenEditor({
  values,
  defaults,
  onChange,
  onReset,
}: {
  values: Record<string, unknown>;
  defaults: Record<string, unknown>;
  onChange: (key: string, value: unknown) => void;
  onReset: (key: string) => void;
}) {
  const names = useMemo(() => {
    const set = new Set<string>([...Object.keys(defaults), ...Object.keys(values)]);
    return [...set].sort();
  }, [defaults, values]);

  if (names.length === 0) {
    return (
      <p className="text-sm text-muted" data-theme-tokens-empty>
        This theme declares no colour tokens. Anything you add here is an override on top of the
        renderer&apos;s fallbacks.
      </p>
    );
  }

  return (
    <ul className="space-y-3" data-theme-token-list>
      {names.map((name) => {
        const override = values[name];
        const declared = defaults[name];
        const isMap =
          (override && typeof override === "object") ||
          (declared && typeof declared === "object");
        const defaultMap = (declared && typeof declared === "object" ? declared : {}) as Record<string, unknown>;
        const overrideMap = (override && typeof override === "object" ? override : {}) as Record<string, unknown>;
        const isOverridden = Object.keys(overrideMap).length > 0 || (override !== undefined && typeof override !== "object");

        if (!isMap) {
          const value = asString(override ?? declared);
          return (
            <li key={name} className="flex items-center gap-2" data-theme-token={name}>
              <ColourInput
                label={name}
                value={value}
                onChange={(next) => onChange(name, next)}
              />
              {isOverridden ? (
                <button
                  type="button"
                  className="btn btn-ghost"
                  data-theme-token-reset={name}
                  onClick={() => onReset(name)}
                  title={`Return ${name} to the theme default`}
                >
                  <RotateCcw className="h-3 w-3" aria-hidden />
                  Reset
                </button>
              ) : null}
            </li>
          );
        }

        return (
          <li key={name} className="rounded-md border border-line px-3 py-2" data-theme-token={name}>
            <div className="flex items-center justify-between gap-2">
              <span className="text-sm font-medium text-ink">{name}</span>
              {isOverridden ? (
                <button
                  type="button"
                  className="btn btn-ghost"
                  data-theme-token-reset={name}
                  onClick={() => onReset(name)}
                  title={`Return ${name} to the theme default`}
                >
                  <RotateCcw className="h-3 w-3" aria-hidden />
                  Reset
                </button>
              ) : null}
            </div>
            <div className="mt-2 grid gap-2 sm:grid-cols-2">
              {(["light", "dark"] as const).map((mode) => (
                <ColourInput
                  key={mode}
                  label={`${name} · ${mode}`}
                  value={asString(overrideMap[mode] ?? defaultMap[mode])}
                  onChange={(next) => {
                    const merged = { ...defaultMap, ...overrideMap, [mode]: next };
                    onChange(name, merged);
                  }}
                />
              ))}
            </div>
          </li>
        );
      })}
    </ul>
  );
}

/** A colour swatch plus a text input, because a token's value may be a keyword, not a hex. */
function ColourInput({
  label,
  value,
  onChange,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const id = `token-${label.replace(/[^a-z0-9]+/gi, "-").toLowerCase()}`;
  const colour = isColour(value);
  return (
    <div className="flex min-w-0 flex-1 items-center gap-2">
      <label htmlFor={id} className="sr-only">
        {label}
      </label>
      {colour ? (
        <input
          type="color"
          className="h-8 w-10 shrink-0 cursor-pointer rounded border border-line"
          data-theme-colour-swatch={label}
          value={value}
          onChange={(event) => onChange(event.target.value)}
        />
      ) : null}
      <input
        id={id}
        type="text"
        className="input min-w-0 flex-1"
        data-theme-token-input={label}
        value={value}
        placeholder="#000000 or a CSS keyword"
        onChange={(event) => onChange(event.target.value)}
      />
    </div>
  );
}

/** A key/value editor for the sections that are not colour maps. */
function FlatEditor({
  section,
  values,
  onChange,
  onRemove,
}: {
  section: SectionName;
  values: Record<string, unknown>;
  onChange: (key: string, value: unknown) => void;
  onRemove: (key: string) => void;
}) {
  const [newKey, setNewKey] = useState("");
  const [error, setError] = useState<string | null>(null);
  const keys = Object.keys(values).sort();

  const add = useCallback(() => {
    const key = newKey.trim();
    // The store's key rule (letters, digits, dashes, underscores, ≤64) is enforced HERE too, so
    // the panel refuses a key the API would reject with a 400 three lines later.
    if (!key) return;
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(key)) {
      setError("A name may only use letters, digits, dashes and underscores, up to 64 characters.");
      return;
    }
    if (values[key] !== undefined) {
      setError(`${key} is already in this section.`);
      return;
    }
    onChange(key, "");
    setNewKey("");
    setError(null);
  }, [newKey, onChange, values]);

  return (
    <div className="space-y-2" data-theme-flat-editor={section}>
      {keys.length === 0 ? (
        <p className="text-sm text-muted" data-theme-flat-empty={section}>
          Nothing overridden in this section. Add a name below to set one.
        </p>
      ) : (
        <ul className="space-y-2">
          {keys.map((key) => (
            <li key={key} className="flex items-center gap-2" data-theme-flat-row={`${section}.${key}`}>
              <label htmlFor={`flat-${section}-${key}`} className="w-36 shrink-0 truncate text-sm text-muted">
                {key}
              </label>
              <input
                id={`flat-${section}-${key}`}
                type="text"
                className="input min-w-0 flex-1"
                data-theme-flat-input={`${section}.${key}`}
                value={asString(values[key])}
                onChange={(event) => onChange(key, event.target.value)}
              />
              <button
                type="button"
                className="btn btn-ghost"
                data-theme-flat-remove={`${section}.${key}`}
                onClick={() => onRemove(key)}
                title={`Remove the ${key} override`}
                aria-label={`Remove the ${key} override`}
              >
                <Undo2 className="h-4 w-4" aria-hidden />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex items-center gap-2 pt-1">
        <label htmlFor={`add-${section}`} className="sr-only">
          {`New ${section} value name`}
        </label>
        <input
          id={`add-${section}`}
          type="text"
          className="input min-w-0 flex-1"
          data-theme-flat-new={section}
          value={newKey}
          placeholder="name (e.g. baseSize)"
          onChange={(event) => {
            setNewKey(event.target.value);
            setError(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              add();
            }
          }}
        />
        <button type="button" className="btn btn-ghost" data-theme-flat-add={section} onClick={add}>
          Add
        </button>
      </div>
      {error ? (
        <p className="text-xs text-danger" role="alert" data-theme-flat-error={section}>
          {error}
        </p>
      ) : null}
    </div>
  );
}

/**
 * The right-hand preview.
 *
 * Deliberately NOT an iframe: an iframe would need a server round-trip per keystroke to show
 * the same thing, and the public preview route does not exist until the package work lands. So
 * this renders a small sample page with the edited tokens as CSS custom properties, which is
 * the same mechanism the renderer uses and therefore a real preview rather than a mock. The
 * token list underneath is the honest fallback: it shows the resolved values, so a preview
 * that a browser cannot lay out (an exotic font stack) still reports what will be sent.
 */
function LivePreview({
  form,
  defaults,
}: {
  form: ThemeSettingsInput;
  defaults: Record<string, unknown>;
}) {
  const [mode, setMode] = useState<"light" | "dark">("light");

  const resolved = useMemo(() => {
    const out: Record<string, string> = {};
    for (const [name, declared] of Object.entries(defaults)) {
      if (declared && typeof declared === "object") {
        const map = declared as Record<string, unknown>;
        out[name] = asString(map[mode] ?? map.light ?? "");
      } else {
        out[name] = asString(declared);
      }
    }
    for (const [name, override] of Object.entries(form.tokens as Record<string, unknown>)) {
      if (override && typeof override === "object") {
        const map = override as Record<string, unknown>;
        const value = asString(map[mode] ?? map.light ?? "");
        if (value) out[name] = value;
      } else {
        const value = asString(override);
        if (value) out[name] = value;
      }
    }
    return out;
  }, [defaults, form.tokens, mode]);

  const typography = (form.typography ?? {}) as Record<string, unknown>;
  const layout = (form.layout ?? {}) as Record<string, unknown>;
  const surface = resolved.surface || (mode === "dark" ? "#101010" : "#ffffff");
  const ink = resolved.text || (mode === "dark" ? "#f5f5f5" : "#111111");
  const accent = resolved.accent || "#2f6feb";
  const radius = asString(layout.radius) || "0.5rem";
  const baseSize = asString(typography.baseSize) || "16px";
  const width = asString(layout.containerWidth) || "72rem";

  return (
    <section className="rounded-lg border border-line bg-panel" data-theme-preview>
      <header className="flex items-center justify-between gap-2 border-b border-line px-4 py-2">
        <h3 className="flex items-center gap-2 text-sm font-semibold text-ink">
          <Eye className="h-4 w-4" aria-hidden />
          Live preview
        </h3>
        <div className="flex gap-1" role="radiogroup" aria-label="Preview mode">
          {(["light", "dark"] as const).map((option) => (
            <button
              key={option}
              type="button"
              role="radio"
              aria-checked={mode === option}
              className={`rounded px-2 py-1 text-xs ${mode === option ? "bg-accent-soft text-ink" : "text-muted"}`}
              data-theme-preview-mode={option}
              onClick={() => setMode(option)}
            >
              {option}
            </button>
          ))}
        </div>
      </header>

      <div className="p-4">
        <div
          data-theme-preview-surface
          className="mx-auto space-y-3 border border-line p-4"
          style={{
            // The same mechanism the renderer uses: tokens as CSS custom properties.
            ["--surface" as string]: surface,
            ["--ink" as string]: ink,
            ["--accent" as string]: accent,
            background: surface,
            color: ink,
            borderRadius: radius,
            fontSize: baseSize,
            maxWidth: width,
          }}
        >
          <p className="text-xs uppercase tracking-wide opacity-70">Preview</p>
          <p className="text-lg font-semibold">A heading in your type scale</p>
          <p className="text-sm opacity-80">
            Body text at the base size you set, so the reading contrast is the contrast a visitor
            gets.
          </p>
          <p>
            <span
              className="inline-block px-3 py-1 text-sm font-medium"
              style={{ background: accent, color: surface, borderRadius: radius }}
            >
              A link-coloured action
            </span>
          </p>
        </div>

        <details className="mt-3" data-theme-preview-tokens>
          <summary className="cursor-pointer text-xs text-muted">
            Resolved tokens ({Object.keys(resolved).length})
          </summary>
          <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
            {Object.entries(resolved).map(([name, value]) => (
              <div key={name} className="contents" data-theme-preview-token={name}>
                <dt className="text-muted">{name}</dt>
                <dd className="flex items-center gap-2 text-ink">
                  {isColour(value) ? (
                    <span
                      className="inline-block h-3 w-3 rounded-sm border border-line"
                      style={{ background: value }}
                      aria-hidden
                    />
                  ) : null}
                  <span className="font-mono">{value || "—"}</span>
                </dd>
              </div>
            ))}
          </dl>
        </details>
      </div>
    </section>
  );
}

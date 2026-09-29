"use client";

/**
 * `/themes` — the theme gallery (REQ-062, slice 1).
 *
 * Five things this screen refuses to do, each because the gallery is where an operator goes to
 * decide what a live site looks like, and every one of them is a way to answer that question
 * with a card that lies:
 *
 * 1. **The `Active` badge is the server's, not the client's.** The gallery payload carries
 *    `activeKey`, and a card is marked active when its key matches. Deriving it from the list
 *    order is how two cards end up badged after an activation that the database refused.
 * 2. **`Restore previous` exists only when the server says there is something to restore.**
 *    `rollbackTarget` is `null` for "never switched" *and* for "going back would change
 *    nothing", and a button that re-activates the current theme is a button that reports work
 *    it did not do.
 * 3. **A key the gallery cannot show is stated, not hidden.** A site restored from a dump, or
 *    pointed at a theme an operator removed, renders *something* — so the screen says
 *    "this site renders with a theme that is not installed" and names the key, rather than
 *    showing a gallery in which nothing is active and no explanation.
 * 4. **Activation is a confirmation, and the confirmation names what is being replaced.** A
 *    theme switch changes every page a signed-out visitor sees, and the previous theme is
 *    exactly what makes the action reversible.
 * 5. **A bundled theme offers no delete.** `canDelete` comes from the server; slice 3 owns the
 *    installer, and until it does there is no delete to offer, so the action is not rendered
 *    rather than rendered disabled.
 */
import Link from "next/link";
import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Check, History, Loader2, RefreshCw, RotateCcw, SlidersHorizontal, Trash2 } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  activateTheme,
  fetchThemeGallery,
  rollbackTheme,
} from "@/lib/api";
import type { GalleryEntry, ThemeCard, ThemeGallery } from "@/lib/api";
import { useSites } from "@/lib/sites";

/** One line under the mode chips, and the thing the QA pass reads to check the card rendered. */
function describe(theme: ThemeCard): string {
  const parts: string[] = [];
  parts.push(`${theme.slotCount} slot${theme.slotCount === 1 ? "" : "s"}`);
  parts.push(`${theme.tokenCount} token${theme.tokenCount === 1 ? "" : "s"}`);
  return parts.join(" · ");
}

export function ThemesView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [gallery, setGallery] = useState<ThemeGallery | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [pending, setPending] = useState<
    { kind: "activate"; key: string; name: string } | { kind: "rollback"; key: string } | null
  >(null);

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setLoading(true);
    setError(null);
    try {
      setGallery(await fetchThemeGallery(selectedSite.id));
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  const confirm = useCallback(async () => {
    if (!selectedSite || !pending) return;
    const target = pending;
    setPending(null);
    setBusyKey(target.key);
    setError(null);
    setNotice(null);
    try {
      const result =
        target.kind === "activate"
          ? await activateTheme(selectedSite.id, target.key)
          : await rollbackTheme(selectedSite.id);
      // The write answers with the gallery AFTER the write, so the badge moves without a
      // second request and without this screen guessing what the server did.
      setGallery(result.gallery);
      const name =
        result.gallery.themes.find((entry) => entry.theme.key === result.themeKey)?.theme.name ??
        result.themeKey;
      setNotice(
        target.kind === "activate"
          ? `${name} is now the theme of ${result.gallery.siteKey || "this site"}. The previous theme is one click away.`
          : `Restored ${name}.`,
      );
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusyKey(null);
    }
  }, [pending, selectedSite]);

  if (siteStatus === "error") {
    return <EmptyState title="No site selected" hint={siteError ?? "The site list could not be loaded."} />;
  }
  if (!selectedSite) {
    return <EmptyState title="No site selected" hint="Pick a site to choose the theme it renders with." />;
  }
  if (loading && !gallery) {
    return <LoadingTable columns={3} rows={3} />;
  }
  if (error && !gallery) {
    return <EmptyState title="The gallery could not be loaded" hint={error} action={<ReloadButton onClick={load} />} />;
  }
  if (!gallery) {
    return <LoadingTable columns={3} rows={3} />;
  }

  const active = gallery.themes.find((entry) => entry.isActive) ?? null;

  return (
    <div className="space-y-6" data-themes-gallery>
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <p className="text-sm text-muted" data-themes-active-key={gallery.activeKey}>
            <span className="font-medium text-ink">{active?.theme.name ?? gallery.activeKey}</span>
            {active ? ` · version ${active.theme.version}` : null}
            {gallery.rollbackTarget ? (
              <> · one click from <span data-themes-rollback-target>{gallery.rollbackTarget}</span></>
            ) : null}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <ReloadButton onClick={load} busy={loading} />
          <button
            type="button"
            className="btn btn-ghost"
            data-themes-rollback
            disabled={!gallery.rollbackTarget || busyKey !== null}
            onClick={() =>
              gallery.rollbackTarget
                ? setPending({ kind: "rollback", key: gallery.rollbackTarget })
                : undefined
            }
            title={
              gallery.rollbackTarget
                ? `Restore ${gallery.rollbackTarget}`
                : "This site has not switched theme, so there is nothing to go back to"
            }
          >
            <RotateCcw className="h-4 w-4" aria-hidden />
            Restore previous
          </button>
        </div>
      </header>

      {/* A key nothing renders is a fact, and it is stated rather than rendered as an empty
          gallery: a visitor is still seeing this site, so the operator has to know which
          theme that is even though no card can offer it. */}
      {!gallery.activeKnown ? (
        <p
          className="flex items-start gap-2 rounded-md border border-warn/40 bg-warn/10 px-3 py-2 text-sm"
          data-themes-unknown-active
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-warn" aria-hidden />
          <span>
            This site renders with <code>{gallery.activeKey}</code>, which is not an installed
            theme. Visitors are seeing the renderer&apos;s fallback. Activate a theme below to
            put it back on a real one.
          </span>
        </p>
      ) : null}

      {error ? (
        <p className="text-sm text-danger" role="alert" data-themes-error>
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="text-sm text-ok" role="status" data-themes-notice>
          {notice}
        </p>
      ) : null}

      {gallery.themes.length === 0 ? (
        <EmptyState
          title="No themes are installed"
          hint="The bundled themes ship with the platform. If this list is empty, the installation has not mirrored them yet — reload once the API has booted."
          action={<ReloadButton onClick={load} />}
        />
      ) : (
        <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3" data-themes-list>
          {gallery.themes.map((entry: GalleryEntry) => (
            <ThemeCardView
              key={entry.theme.key}
              entry={entry}
              busy={busyKey === entry.theme.key}
              disabled={busyKey !== null}
              onActivate={() =>
                setPending({ kind: "activate", key: entry.theme.key, name: entry.theme.name })
              }
            />
          ))}
        </ul>
      )}

      {pending ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
          role="dialog"
          aria-modal="true"
          aria-labelledby="themes-confirm-title"
          data-themes-confirm
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-lg">
            <h2 id="themes-confirm-title" className="text-base font-semibold text-ink">
              {pending.kind === "activate"
                ? `Use ${pending.key} for ${gallery.siteKey || "this site"}?`
                : `Restore ${pending.key}?`}
            </h2>
            <p className="mt-2 text-sm text-muted">
              {pending.kind === "activate" ? (
                <>
                  Every page a visitor sees changes to {pending.key}. The current theme (
                  {gallery.activeKey}) is kept and can be restored with one click.
                </>
              ) : (
                <>
                  This site goes back to {pending.key}. The theme it is on now ({gallery.activeKey})
                  is kept, so the restore is reversible too.
                </>
              )}
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button type="button" className="btn btn-ghost" onClick={() => setPending(null)}>
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-primary"
                data-themes-confirm-accept
                onClick={() => void confirm()}
              >
                {pending.kind === "activate" ? "Activate" : "Restore"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

function ThemeCardView({
  entry,
  busy,
  disabled,
  onActivate,
}: {
  entry: GalleryEntry;
  busy: boolean;
  disabled: boolean;
  onActivate: () => void;
}) {
  const { theme, isActive } = entry;
  return (
    <li
      className="flex flex-col gap-3 rounded-lg border border-line bg-panel p-4"
      data-theme-card={theme.key}
      data-active={isActive ? "true" : "false"}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <h3 className="truncate text-sm font-semibold text-ink" data-theme-name={theme.key}>
            {theme.name}
          </h3>
          <p className="text-xs text-muted">
            {theme.key} · v{theme.version}
            {theme.author ? ` · ${theme.author}` : ""}
          </p>
        </div>
        {isActive ? (
          <span
            className="inline-flex items-center gap-1 rounded-full bg-positive-soft px-2 py-0.5 text-[11px] font-medium text-positive"
            data-theme-badge={theme.key}
          >
            <Check className="h-3 w-3" aria-hidden />
            Active
          </span>
        ) : null}
      </div>

      {theme.description ? (
        <p className="line-clamp-2 text-sm text-muted">{theme.description}</p>
      ) : null}

      <p className="text-xs text-muted" data-theme-shape={theme.key}>
        {describe(theme)}
        {theme.modes.length ? ` · ${theme.modes.join(" / ")}` : ""}
      </p>

      <div className="mt-auto flex items-center gap-2">
        {isActive ? (
          <span className="text-xs text-muted" data-theme-active-note={theme.key}>
            What visitors see now.
          </span>
        ) : (
          <button
            type="button"
            className="btn btn-primary"
            data-theme-activate={theme.key}
            disabled={disabled || busy}
            onClick={onActivate}
          >
            {busy ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : null}
            Activate
          </button>
        )}
        {/* A bundled theme can never be deleted, and the server says so per card. Rendering
            the action disabled would be a dead button; not rendering it is honest until
            slice 3 ships the installer that can refuse a removal. */}
        {theme.canDelete ? (
          <button
            type="button"
            className="btn btn-ghost"
            data-theme-delete={theme.key}
            disabled={disabled || isActive}
            title={
              isActive
                ? "A site cannot render a theme that is not installed"
                : "Remove this uploaded theme"
            }
          >
            <Trash2 className="h-4 w-4" aria-hidden />
          </button>
        ) : null}
        {/* Customise and History are per-SITE resources, not per-theme ones: the settings
            routes are `/sites/{site_id}/theme-settings`, and a card links to them with the
            theme key in the path because that is the URL the operator remembers. The editor
            re-reads the site's active theme and shows it, so a link from an inactive card
            lands on the site's own settings rather than pretending to edit another theme. */}
        <Link
          href={`/themes/${encodeURIComponent(theme.key)}/customize`}
          className="btn btn-ghost"
          data-theme-customize-link={theme.key}
          title="Colours, type, layout and branding"
        >
          <SlidersHorizontal className="h-4 w-4" aria-hidden />
          Customize
        </Link>
        <Link
          href={`/themes/${encodeURIComponent(theme.key)}/history`}
          className="btn btn-ghost"
          data-theme-history-link={theme.key}
          title="Every saved revision, with diffs and a restore"
        >
          <History className="h-4 w-4" aria-hidden />
          History
        </Link>
      </div>
    </li>
  );
}

function ReloadButton({ onClick, busy }: { onClick: () => void; busy?: boolean }) {
  return (
    <button
      type="button"
      className="btn btn-ghost"
      onClick={onClick}
      disabled={busy}
      aria-label="Reload the gallery"
    >
      <RefreshCw className={`h-4 w-4${busy ? " animate-spin" : ""}`} aria-hidden />
    </button>
  );
}

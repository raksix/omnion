"use client";

/**
 * `/menus` — the site's navigation, one row per menu (REQ-064, slice 1).
 *
 * Three things this screen refuses to do, each because a navigation is the one surface every
 * visitor sees and the one nobody re-checks after a change:
 *
 * 1. **A location is claimed, not decorated.** The row names the theme slots the menu renders
 *    into, and a location can only hold one menu. The claim happens in the editor's save, where a
 *    conflict is answered with the holder's name — a list that merely displayed `header` on two
 *    rows would be reporting a state the server refuses to store.
 * 2. **The item count is the stored count, nested included.** A menu with two top-level rows and
 *    nine children is not a two-item menu, and a list that counts rows is a list that understates
 *    the work.
 * 3. **Deleting a menu that holds a location is a confirmation, not a click.** The theme's header
 *    disappears with it, and the confirmation names the slots rather than saying "are you sure".
 */
import { useCallback, useEffect, useState } from "react";
import { Loader2, Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { ApiError, createMenu, deleteMenu, fetchMenus } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { Menu } from "@/lib/types";

/** A key derived from the name, so the common case is one field instead of two. */
function suggestKey(name: string): string {
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
}

export function MenusView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [menus, setMenus] = useState<Menu[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Menu | null>(null);

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setError(null);
    try {
      setMenus(await fetchMenus(selectedSite.id));
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  const remove = useCallback(
    async (menu: Menu) => {
      setBusyId(menu.id);
      setNotice(null);
      setError(null);
      try {
        await deleteMenu(menu.id);
        // The confirmation names what disappeared: a menu is not just a row, it is the header or
        // the footer of a live site, and the sentence is the only record of that.
        setNotice(
          menu.locations.length === 0
            ? `Deleted ${menu.name}.`
            : `Deleted ${menu.name} — ${menu.locations.join(", ")} now renders nothing.`,
        );
        setPendingDelete(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
        setPendingDelete(null);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  if (siteStatus === "error" && siteError) {
    return <ErrorStrip message={siteError} onRetry={() => void load()} />;
  }
  // Three states, not two: the site list is still coming, the site is known but the menus are
  // not, and the account genuinely has no site. The third one is an empty state rather than a
  // spinner that never resolves — a list that spins forever is a lie about work in progress.
  if (siteStatus === "loading" || siteStatus === "idle") return <MenusSkeleton />;
  if (siteStatus === "ready" && !selectedSite) return <NoSite />;
  if (menus === null) return <MenusSkeleton />;

  return (
    <div className="space-y-6" data-menus-state="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          {menus && menus.length > 0
            ? `${menus.length} menu${menus.length === 1 ? "" : "s"} in this site. A theme slot holds one menu.`
            : "Menus are per site and a theme slot holds one menu."}
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            data-menus-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-menus-create
            onClick={() => {
              setCreating((value) => !value);
              setError(null);
            }}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden />
            New menu
          </button>
        </div>
      </div>

      {notice ? (
        <p data-menus-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-menus-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {creating && selectedSite ? (
        <CreateMenuForm
          siteId={selectedSite.id}
          onCancel={() => setCreating(false)}
          onError={setError}
          onCreated={async (name) => {
            setCreating(false);
            setNotice(`Created ${name}. Open it to build the tree.`);
            await load();
          }}
        />
      ) : null}

      {menus && menus.length === 0 ? (
        <div className="rounded-lg border border-line" data-menus-empty>
          <EmptyState
            title="No menus in this site yet"
            hint="A site with no menu renders every page but shows no navigation, so a reader can only reach the front page."
            action={
              <button
                type="button"
                onClick={() => setCreating(true)}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Create the first menu
              </button>
            }
          />
        </div>
      ) : (
        <ul className="space-y-2" data-menus-list>
          {(menus ?? []).map((menu) => (
            <li
              key={menu.id}
              data-menu-row={menu.key}
              className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-line px-4 py-3"
            >
              <div className="min-w-0">
                <p className="text-[13.5px] font-medium">{menu.name}</p>
                <p className="text-[12px] text-muted">
                  <code className="font-mono">{menu.key}</code> · {menu.item_count} item
                  {menu.item_count === 1 ? "" : "s"} · changed {formatTimestamp(menu.updated_at)}
                </p>
                {/* The claimed slots are the row's second life: a menu nobody assigned renders
                    nothing, and the list is where that is noticed. */}
                {menu.locations.length === 0 ? (
                  <span data-menu-unassigned className="mt-1 inline-block text-[12px] text-amber-700 dark:text-amber-300">
                    No location assigned — it renders nowhere.
                  </span>
                ) : (
                  <span className="mt-1 flex flex-wrap gap-1">
                    {menu.locations.map((location) => (
                      <span
                        key={location}
                        data-menu-location={location}
                        className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                      >
                        {location}
                      </span>
                    ))}
                  </span>
                )}
              </div>
              <div className="flex gap-2">
                <Link
                  href={`/menus/${menu.id}/edit`}
                  data-menu-edit={menu.id}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  <Pencil className="h-3.5 w-3.5" aria-hidden />
                  Edit
                </Link>
                <button
                  type="button"
                  data-menu-delete={menu.id}
                  onClick={() => setPendingDelete(menu)}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  <Trash2 className="h-3.5 w-3.5" aria-hidden />
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {pendingDelete ? (
        <ConfirmDelete
          menu={pendingDelete}
          busy={busyId === pendingDelete.id}
          onCancel={() => setPendingDelete(null)}
          onConfirm={() => void remove(pendingDelete)}
        />
      ) : null}
    </div>
  );
}

function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="space-y-3" data-menus-state="error">
      <p className="text-[13px] text-red-700 dark:text-red-300">{message}</p>
      <button
        type="button"
        onClick={onRetry}
        className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
      >
        <RefreshCw className="h-4 w-4" aria-hidden />
        Retry
      </button>
    </div>
  );
}

/** The account has no site, so there is nothing for a menu to navigate. Named, not spun on. */
function NoSite() {
  return (
    <div className="rounded-lg border border-line" data-menus-empty="no-site">
      <EmptyState
        title="This account has no site yet"
        hint="Menus belong to a site, so there is nothing to list until a site exists."
      />
    </div>
  );
}

function MenusSkeleton() {
  return (
    <div className="space-y-3" data-menus-state="loading" aria-busy="true">
      <div className="h-4 w-48 animate-pulse rounded bg-quiet-soft" />
      {Array.from({ length: 3 }, (_, index) => (
        <div key={index} className="h-16 animate-pulse rounded-lg bg-quiet-soft" />
      ))}
    </div>
  );
}

/** Name and key. The key is editable because two menus cannot share one. */
function CreateMenuForm({
  siteId,
  onCancel,
  onCreated,
  onError,
}: {
  siteId: string;
  onCancel: () => void;
  onCreated: (name: string) => Promise<void>;
  onError: (message: string | null) => void;
}) {
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [keyTouched, setKeyTouched] = useState(false);
  const [busy, setBusy] = useState(false);

  const submit = useCallback(async () => {
    setBusy(true);
    onError(null);
    try {
      await createMenu({ site_id: siteId, key: key.trim(), name: name.trim() });
      await onCreated(name.trim());
    } catch (caught) {
      onError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [siteId, key, name, onCreated, onError]);

  return (
    <form
      data-menu-form
      className="space-y-3 rounded-lg border border-line p-4"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Name</span>
          <input
            data-menu-form-name
            required
            value={name}
            onChange={(event) => {
              setName(event.target.value);
              // The key follows the name until the editor takes it over. Two menus cannot share
              // one key, and a duplicate is a 409 on a field the editor never looked at.
              if (!keyTouched) setKey(suggestKey(event.target.value));
            }}
            placeholder="Main navigation"
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Key</span>
          <input
            data-menu-form-key
            required
            value={key}
            onChange={(event) => {
              setKeyTouched(true);
              setKey(event.target.value);
            }}
            placeholder="main"
            className="rounded-md border border-line bg-transparent px-2 py-1.5 font-mono text-[13px]"
          />
          <span className="text-muted">Unique inside the site. Themes read it.</span>
        </label>
      </div>
      <div className="flex gap-2">
        <button
          type="submit"
          data-menu-form-save
          disabled={busy}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
          Create menu
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

/**
 * The confirmation. It names the slots, because "delete this menu?" and "the footer of your live
 * site stops rendering — are you sure?" are different questions and only one of them is informed.
 */
function ConfirmDelete({
  menu,
  busy,
  onCancel,
  onConfirm,
}: {
  menu: Menu;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      data-menu-confirm
      role="alertdialog"
      aria-label={`Delete ${menu.name}`}
      className="rounded-lg border border-caution/40 bg-caution-soft/30 p-4"
    >
      <p className="text-[13px] font-medium">Delete {menu.name}?</p>
      <p className="mt-1 text-[12.5px] text-muted">
        {menu.locations.length === 0
          ? `Its ${menu.item_count} item${menu.item_count === 1 ? "" : "s"} go with it.`
          : `Its ${menu.item_count} item${menu.item_count === 1 ? "" : "s"} go with it, and ${menu.locations.join(
              ", ",
            )} renders nothing until another menu claims ${menu.locations.length === 1 ? "it" : "those slots"}.`}
      </p>
      <div className="mt-3 flex gap-2">
        <button
          type="button"
          data-menu-confirm-delete
          disabled={busy}
          onClick={onConfirm}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
          Delete the menu
        </button>
        <button
          type="button"
          data-menu-confirm-cancel
          onClick={onCancel}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Keep it
        </button>
      </div>
    </div>
  );
}

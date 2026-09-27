"use client";

/**
 * `/settings/iam/roles/{id}` — one role in full (REQ-006, slice 1).
 *
 * Four tabs, each read from the API rather than guessed:
 *
 * * **Permissions** — the matrix: a category accordion over the whole catalogue, one tri-state
 *   cell per key (`allow` / `deny` / `inherit`), header counts, grant-all per category, a
 *   search box, and a sticky footer with Save, Discard and a diff preview. The save is atomic
 *   and carries the version the screen read, so a stale screen is refused instead of
 *   overwriting somebody else's change.
 * * **Members** — the people the role currently or formerly applied to.
 * * **Inherited by** — the roles that build on this one.
 * * **History** — every version with the diff it introduced.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { ChevronDown, ChevronRight, Eye, History, RefreshCw, Save, ShieldAlert, Undo2 } from "lucide-react";
import Link from "next/link";

import {
  ApiError,
  fetchPermissionCatalogue,
  fetchRole,
  fetchRoleMembers,
  fetchRoles,
  fetchRoleVersions,
  previewRolePermissions,
  saveRolePermissions,
  updateIamRole,
  type IamDiff,
  type IamPermissionDef,
  type IamPermissionEntry,
  type IamRole,
  type IamRoleDetail,
  type IamRoleMember,
  type IamRoleVersion,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The matrix values a cell can hold. */
type CellValue = "allow" | "deny" | "inherit";

/** Tabs of the screen. */
type Tab = "permissions" | "members" | "inherited" | "history";

/** One tri-state cell. */
function Cell({
  permissionKey,
  value,
  onChange,
  disabled,
}: {
  permissionKey: string;
  value: CellValue;
  onChange: (next: CellValue) => void;
  disabled: boolean;
}) {
  const options: { value: CellValue; label: string; className: string }[] = [
    { value: "allow", label: "Allow", className: "data-[on=true]:bg-positive-soft data-[on=true]:text-positive data-[on=true]:border-positive/40" },
    { value: "deny", label: "Deny", className: "data-[on=true]:bg-danger-soft data-[on=true]:text-caution data-[on=true]:border-danger/40" },
    { value: "inherit", label: "Inherit", className: "data-[on=true]:bg-quiet-soft data-[on=true]:text-ink data-[on=true]:border-line" },
  ];
  return (
    <div role="group" aria-label={`${permissionKey} effect`} className="flex shrink-0 items-center gap-1">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          data-matrix-cell={permissionKey}
          data-matrix-value={option.value}
          data-on={value === option.value}
          aria-pressed={value === option.value}
          disabled={disabled}
          onClick={() => onChange(option.value)}
          className={`rounded-md border border-transparent px-1.5 py-0.5 text-[11px] font-medium text-muted transition hover:bg-quiet-soft disabled:opacity-60 ${option.className}`}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

/** One entry of a diff, in the words the panel prints. */
function DiffPanel({ diff, problems, unchanged }: { diff: IamDiff; problems: string[]; unchanged: boolean }) {
  const nothing = diff.added.length === 0 && diff.changed.length === 0 && diff.removed.length === 0;
  return (
    <div data-matrix-diff className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3">
      <div className="flex items-center gap-2">
        <Eye className="size-3.5 text-muted" aria-hidden />
        <span className="text-[12.5px] font-medium">What this save changes</span>
        {nothing ? (
          <span className="text-[12px] text-muted">{unchanged ? "— nothing, the set is already stored" : "— nothing"}</span>
        ) : (
          <span className="text-[12px] text-muted">
            {diff.added.length} added · {diff.changed.length} changed · {diff.removed.length} removed
          </span>
        )}
      </div>
      {problems.length > 0 ? (
        <ul data-matrix-problems className="flex flex-col gap-1 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12px] text-caution">
          {problems.map((problem) => (
            <li key={problem}>{problem}</li>
          ))}
        </ul>
      ) : null}
      <ul className="flex flex-col gap-1 text-[12px]">
        {diff.added.map((entry) => (
          <li key={`a-${entry.key}`} className="flex items-center justify-between gap-2">
            <span className="font-mono text-[11.5px]">{entry.key}</span>
            <span className="text-positive">added · {entry.effect}</span>
          </li>
        ))}
        {diff.changed.map((entry) => (
          <li key={`c-${entry.key}`} className="flex items-center justify-between gap-2">
            <span className="font-mono text-[11.5px]">{entry.key}</span>
            <span className="text-caution">
              {entry.from} → {entry.to}
            </span>
          </li>
        ))}
        {diff.removed.map((entry) => (
          <li key={`r-${entry.key}`} className="flex items-center justify-between gap-2">
            <span className="font-mono text-[11.5px]">{entry.key}</span>
            <span className="text-muted">removed · was {entry.effect}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** `/settings/iam/roles/{id}`. */
export function RoleDetailView({ roleId }: { roleId: string }) {
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [detail, setDetail] = useState<IamRoleDetail | null>(null);
  const [catalogue, setCatalogue] = useState<IamPermissionDef[]>([]);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [entries, setEntries] = useState<Map<string, CellValue>>(new Map());
  const [savedEntries, setSavedEntries] = useState<Map<string, CellValue>>(new Map());
  const [tab, setTab] = useState<Tab>("permissions");
  const [search, setSearch] = useState("");
  const [openCategories, setOpenCategories] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState<{ diff: IamDiff; problems: string[]; unchanged: boolean } | null>(null);
  const [members, setMembers] = useState<IamRoleMember[] | null>(null);
  const [versions, setVersions] = useState<IamRoleVersion[] | null>(null);
  const [editing, setEditing] = useState(false);
  const [form, setForm] = useState({ name: "", description: "", priority: "400", parent: "", inherit: true });
  /** Field-level problem of the editor, refused before anything is sent. */
  const [formProblem, setFormProblem] = useState<string | null>(null);

  const applyDetail = useCallback((next: IamRoleDetail) => {
    setDetail(next);
    const map = new Map<string, CellValue>();
    for (const entry of next.permissions) {
      map.set(entry.key, entry.effect);
    }
    setEntries(map);
    setSavedEntries(new Map(map));
    setForm({
      name: next.role.name,
      description: next.role.description,
      priority: String(next.role.priority),
      parent: next.role.inherits_role_id ?? "",
      inherit: next.role.inherit_permissions,
    });
  }, []);

  const load = useCallback(async () => {
    setStatus("loading");
    setLoadError(null);
    try {
      const [role, catalogueEntries, allRoles] = await Promise.all([
        fetchRole(roleId),
        fetchPermissionCatalogue(),
        fetchRoles(),
      ]);
      setCatalogue(catalogueEntries);
      setRoles(allRoles);
      applyDetail(role);
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The role could not be read." },
      );
    }
  }, [roleId, applyDetail]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (tab !== "members" || members !== null) return;
    void fetchRoleMembers(roleId)
      .then((answer) => setMembers(answer.members))
      .catch((cause) =>
        setError(cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The members could not be read."),
      );
  }, [tab, members, roleId]);

  useEffect(() => {
    if (tab !== "history" || versions !== null) return;
    void fetchRoleVersions(roleId)
      .then(setVersions)
      .catch((cause) =>
        setError(cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The history could not be read."),
      );
  }, [tab, versions, roleId]);

  const grouped = useMemo(() => {
    const groups = new Map<string, IamPermissionDef[]>();
    for (const entry of catalogue) {
      const list = groups.get(entry.category) ?? [];
      list.push(entry);
      groups.set(entry.category, list);
    }
    return [...groups.entries()];
  }, [catalogue]);

  const query = search.trim().toLowerCase();
  const visible = useMemo(() => {
    if (!query) return grouped;
    return grouped
      .map(([category, list]) => [
        category,
        list.filter(
          (entry) =>
            entry.key.toLowerCase().includes(query) ||
            entry.description.toLowerCase().includes(query) ||
            category.includes(query),
        ),
      ] as [string, IamPermissionDef[]])
      .filter(([, list]) => list.length > 0);
  }, [grouped, query]);

  const counts = useMemo(() => {
    let allowed = 0;
    let denied = 0;
    for (const value of entries.values()) {
      if (value === "allow") allowed += 1;
      if (value === "deny") denied += 1;
    }
    return { allowed, denied, inherited: catalogue.length - allowed - denied };
  }, [entries, catalogue.length]);

  const dirty = useMemo(() => {
    if (entries.size !== savedEntries.size) return true;
    for (const [key, value] of entries) {
      if (savedEntries.get(key) !== value) return true;
    }
    return false;
  }, [entries, savedEntries]);

  const setCell = (key: string, next: CellValue) => {
    setEntries((current) => {
      const map = new Map(current);
      if (next === "inherit") {
        map.delete(key);
      } else {
        map.set(key, next);
      }
      return map;
    });
    setPreview(null);
  };

  const setCategory = (list: IamPermissionDef[], next: CellValue) => {
    setEntries((current) => {
      const map = new Map(current);
      for (const entry of list) {
        if (next === "inherit") {
          map.delete(entry.key);
        } else {
          map.set(entry.key, next);
        }
      }
      return map;
    });
    setPreview(null);
  };

  const payload = useCallback((): IamPermissionEntry[] => {
    return [...entries.entries()]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([key, effect]) => ({ key, effect: effect === "allow" ? "allow" : "deny" }));
  }, [entries]);

  const runPreview = async () => {
    setBusy("preview");
    setError(null);
    setNotice(null);
    try {
      const answer = await previewRolePermissions(roleId, payload());
      setPreview({ diff: answer.diff, problems: answer.problems, unchanged: answer.unchanged });
    } catch (cause) {
      setError(cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The preview failed.");
    } finally {
      setBusy(null);
    }
  };

  const save = async () => {
    if (!detail) return;
    setBusy("save");
    setError(null);
    setNotice(null);
    try {
      const answer = await saveRolePermissions(roleId, payload(), detail.version);
      const map = new Map<string, CellValue>();
      for (const entry of answer.permissions) {
        map.set(entry.key, entry.effect);
      }
      setEntries(map);
      setSavedEntries(new Map(map));
      setDetail({ ...detail, role: answer.role, permissions: answer.permissions, version: answer.version });
      setVersions(null);
      setPreview({ diff: answer.diff, problems: [], unchanged: false });
      setNotice(
        `Saved as version ${answer.version}: ${answer.diff.added.length} added, ${answer.diff.changed.length} changed, ${answer.diff.removed.length} removed.`,
      );
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? `The save was refused: ${cause.message} (${cause.code})`
          : "The save was refused.",
      );
    } finally {
      setBusy(null);
    }
  };

  const submitEdit = async () => {
    if (!detail) return;
    const name = form.name.trim();
    const priority = Number(form.priority);
    const problem = !name
      ? "A role needs a name."
      : name.length > 80
        ? "The name may be at most 80 characters."
        : !Number.isInteger(priority) || priority < 0 || priority > 1000
          ? "Priority must be a whole number between 0 and 1000."
          : null;
    setFormProblem(problem);
    if (problem) {
      return;
    }
    setBusy("edit");
    setError(null);
    setNotice(null);
    try {
      const updated = await updateIamRole(roleId, {
        name: form.name,
        description: form.description,
        priority: Number(form.priority),
        inheritPermissions: form.inherit,
        inheritsRoleId: form.parent ? form.parent : null,
      });
      applyDetail({ ...detail, role: updated, version: detail.version + 1 });
      setEditing(false);
      setVersions(null);
      setNotice("The role was updated.");
    } catch (cause) {
      setError(cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The role was not updated.");
    } finally {
      setBusy(null);
    }
  };

  if (status === "loading") {
    return (
      <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
        <span className="sr-only">Reading the role…</span>
        {[0, 1, 2, 3, 4].map((row) => (
          <div key={row} className="mb-2 h-9 animate-pulse rounded-lg bg-quiet-soft" />
        ))}
      </div>
    );
  }

  if (status === "error" || !detail) {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <ShieldAlert className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The role is unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {loadError?.message} <span className="font-mono text-[11.5px]">({loadError?.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const tabs: { id: Tab; label: string }[] = [
    { id: "permissions", label: "Permissions" },
    { id: "members", label: `Members (${detail.member_count})` },
    { id: "inherited", label: "Inherited by" },
    { id: "history", label: "History" },
  ];

  return (
    <div className="flex flex-col gap-4">
      <section className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="text-[15px] font-semibold">{detail.role.name}</h2>
              <span className="rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[11px] font-medium text-muted">
                {detail.role.is_system ? "Platform" : "Custom"}
              </span>
              <span className="font-mono text-[11.5px] text-muted">{detail.role.key}</span>
            </div>
            <p className="mt-1 text-[12.5px] text-muted">
              {detail.role.description || "No description yet."} · priority {detail.role.priority} · version{" "}
              <span data-role-version>{detail.version}</span>
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Link
              href="/settings/iam/roles"
              className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
            >
              All roles
            </Link>
            {!detail.role.is_system ? (
              <button
                type="button"
                data-role-edit-open
                onClick={() => setEditing((value) => !value)}
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
              >
                {editing ? "Close editor" : "Edit role"}
              </button>
            ) : (
              <span className="text-[12px] text-muted">Platform roles are read-only — duplicate to customise.</span>
            )}
          </div>
        </div>

        {detail.chain.length > 0 ? (
          <p className="text-[12px] text-muted">
            Inherits:{" "}
            {detail.chain.map((role, index) => (
              <span key={role.id}>
                {index > 0 ? " → " : ""}
                <Link href={`/settings/iam/roles/${role.id}`} className="text-accent-strong hover:underline">
                  {role.name}
                </Link>
              </span>
            ))}
          </p>
        ) : (
          <p className="text-[12px] text-muted">Inherits nothing — the set below is the whole role.</p>
        )}

        {editing ? (
          <form
            data-role-edit-form
            onSubmit={(event) => {
              event.preventDefault();
              void submitEdit();
            }}
            className="grid gap-3 border-t border-line pt-3 sm:grid-cols-2"
          >
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Name</span>
              <input
                value={form.name}
                data-role-edit-name
                onChange={(event) => {
                  setForm({ ...form, name: event.target.value });
                  setFormProblem(null);
                }}
                className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              {formProblem ? (
                <span data-role-edit-error className="text-[11.5px] text-caution">
                  {formProblem}
                </span>
              ) : null}
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Priority</span>
              <input
                type="number"
                min={0}
                max={1000}
                value={form.priority}
                data-role-edit-priority
                onChange={(event) => setForm({ ...form, priority: event.target.value })}
                className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5 sm:col-span-2">
              <span className="text-[12.5px] font-medium text-ink">Description</span>
              <input
                value={form.description}
                data-role-edit-description
                onChange={(event) => setForm({ ...form, description: event.target.value })}
                className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Inherits from</span>
              <select
                value={form.parent}
                data-role-edit-parent
                onChange={(event) => setForm({ ...form, parent: event.target.value })}
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              >
                <option value="">Nothing</option>
                {roles
                  .filter((role) => role.id !== detail.role.id)
                  .map((role) => (
                    <option key={role.id} value={role.id}>
                      {role.name} ({role.key})
                    </option>
                  ))}
              </select>
            </label>
            <label className="flex items-center gap-2 self-end pb-2 text-[12.5px]">
              <input
                type="checkbox"
                checked={form.inherit}
                data-role-edit-inherit
                onChange={(event) => setForm({ ...form, inherit: event.target.checked })}
                className="size-3.5 accent-accent"
              />
              Inherited permissions apply
            </label>
            <div className="flex items-center gap-2 sm:col-span-2">
              <button
                type="submit"
                disabled={busy !== null}
                data-role-edit-save
                data-qa-guard="iam-role-edit"
                className="rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
              >
                Save changes
              </button>
              <button
                type="button"
                onClick={() => setEditing(false)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
              >
                Cancel
              </button>
            </div>
          </form>
        ) : null}
      </section>

      <div className="flex flex-wrap items-center gap-1 border-b border-line" role="tablist" aria-label="Role sections">
        {tabs.map((entry) => (
          <button
            key={entry.id}
            type="button"
            role="tab"
            aria-selected={tab === entry.id}
            data-tab={entry.id}
            onClick={() => setTab(entry.id)}
            className={`-mb-px border-b-2 px-3 py-2 text-[12.5px] font-medium transition ${
              tab === entry.id
                ? "border-accent text-accent-strong"
                : "border-transparent text-muted hover:text-ink"
            }`}
          >
            {entry.label}
          </button>
        ))}
      </div>

      {notice ? (
        <p role="status" data-role-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p role="alert" data-role-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}

      {tab === "permissions" ? (
        <section className="flex flex-col gap-3" aria-label="Permission matrix">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex flex-wrap items-center gap-3 text-[12px] text-muted">
              <span data-matrix-counts>
                allowed <span className="font-mono text-positive">{counts.allowed}</span> · denied{" "}
                <span className="font-mono text-caution">{counts.denied}</span> · inherited{" "}
                <span className="font-mono text-ink">{counts.inherited}</span>
              </span>
              {detail.role.is_system ? (
                <span className="rounded-md border border-line bg-canvas px-2 py-0.5 text-[11.5px]">
                  Platform role — read-only
                </span>
              ) : null}
            </div>
            <label className="flex items-center gap-2 text-[12.5px]">
              <span className="text-muted">Filter</span>
              <input
                value={search}
                data-matrix-search
                onChange={(event) => setSearch(event.target.value)}
                placeholder="users.read"
                className="h-8 w-48 rounded-lg border border-line bg-surface px-2.5 font-mono text-[12.5px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>

          <div className="flex flex-col gap-2">
            {visible.map(([category, list]) => {
              const open = query.length > 0 || openCategories.has(category);
              const allowed = list.filter((entry) => entries.get(entry.key) === "allow").length;
              const denied = list.filter((entry) => entries.get(entry.key) === "deny").length;
              const inherited = list.length - allowed - denied;
              return (
                <div key={category} className="rounded-xl border border-line bg-surface">
                  <div className="flex flex-wrap items-center gap-2 px-3 py-2">
                    <button
                      type="button"
                      data-matrix-category={category}
                      aria-expanded={open}
                      onClick={() =>
                        setOpenCategories((current) => {
                          const next = new Set(current);
                          if (next.has(category)) next.delete(category);
                          else next.add(category);
                          return next;
                        })
                      }
                      className="flex items-center gap-1.5 text-[12.5px] font-medium text-ink"
                    >
                      {open ? <ChevronDown className="size-3.5" aria-hidden /> : <ChevronRight className="size-3.5" aria-hidden />}
                      {category}
                    </button>
                    <span data-matrix-category-counts={category} className="text-[11.5px] text-muted">
                      allowed {allowed} · denied {denied} · inherited {inherited}
                    </span>
                    <span className="ml-auto flex items-center gap-1.5">
                      <button
                        type="button"
                        data-matrix-category-allow={category}
                        onClick={() => setCategory(list, "allow")}
                        className="rounded-md border border-line px-2 py-0.5 text-[11.5px] font-medium transition hover:bg-quiet-soft"
                      >
                        Grant all
                      </button>
                      <button
                        type="button"
                        data-matrix-category-deny={category}
                        onClick={() => setCategory(list, "deny")}
                        className="rounded-md border border-line px-2 py-0.5 text-[11.5px] font-medium transition hover:bg-quiet-soft"
                      >
                        Deny all
                      </button>
                      <button
                        type="button"
                        data-matrix-category-inherit={category}
                        onClick={() => setCategory(list, "inherit")}
                        className="rounded-md border border-line px-2 py-0.5 text-[11.5px] font-medium transition hover:bg-quiet-soft"
                      >
                        Inherit all
                      </button>
                    </span>
                  </div>
                  {open ? (
                    <ul className="flex flex-col divide-y divide-line border-t border-line">
                      {list.map((entry) => (
                        <li
                          key={entry.key}
                          data-matrix-row={entry.key}
                          className="flex flex-wrap items-center justify-between gap-2 px-3 py-2"
                        >
                          <div className="min-w-0">
                            <p className="font-mono text-[12px] text-ink">{entry.key}</p>
                            <p className="text-[11.5px] text-muted">{entry.description}</p>
                          </div>
                          <Cell
                            permissionKey={entry.key}
                            value={entries.get(entry.key) ?? "inherit"}
                            onChange={(next) => setCell(entry.key, next)}
                            disabled={detail.role.is_system}
                          />
                        </li>
                      ))}
                    </ul>
                  ) : null}
                </div>
              );
            })}
          </div>

          {preview ? <DiffPanel diff={preview.diff} problems={preview.problems} unchanged={preview.unchanged} /> : null}

          <div className="sticky bottom-0 flex flex-wrap items-center gap-2 rounded-xl border border-line bg-surface/95 px-3 py-2 backdrop-blur">
            <button
              type="button"
              data-matrix-save
              data-qa-guard="iam-matrix-save"
              disabled={busy !== null || !dirty || detail.role.is_system}
              onClick={() => void save()}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              <Save className="size-3.5" aria-hidden />
              Save matrix
            </button>
            <button
              type="button"
              data-matrix-preview
              disabled={busy !== null}
              onClick={() => void runPreview()}
              className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
            >
              <Eye className="size-3.5" aria-hidden />
              Preview changes
            </button>
            <button
              type="button"
              data-matrix-discard
              disabled={!dirty}
              onClick={() => {
                setEntries(new Map(savedEntries));
                setPreview(null);
                setNotice("Unsaved changes were discarded.");
              }}
              className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-40"
            >
              <Undo2 className="size-3.5" aria-hidden />
              Discard
            </button>
            <span className="text-[12px] text-muted">
              {dirty ? "Unsaved changes" : "No changes"}
            </span>
          </div>
        </section>
      ) : null}

      {tab === "members" ? (
        <section className="flex flex-col gap-2" aria-label="Members">
          {members === null ? (
            <div className="rounded-xl border border-line bg-surface p-4">
              <div className="h-8 animate-pulse rounded-lg bg-quiet-soft" />
            </div>
          ) : members.length === 0 ? (
            <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
              <ShieldAlert className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">Nobody carries this role yet</p>
              <p className="max-w-md text-[12.5px] text-muted">
                Bindings are granted from an account's own page (the users screen arrives with the next
                slice); a role with no member can still be built and inspected here.
              </p>
            </div>
          ) : (
            <div className="overflow-x-auto rounded-xl border border-line bg-surface">
              <table className="w-full text-left">
                <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                  <tr>
                    <th className="px-3 py-2 font-medium">Account</th>
                    <th className="px-3 py-2 font-medium">Scope</th>
                    <th className="px-3 py-2 font-medium">Expires</th>
                    <th className="px-3 py-2 font-medium">State</th>
                    <th className="px-3 py-2 font-medium">Granted</th>
                  </tr>
                </thead>
                <tbody>
                  {members.map((member) => (
                    <tr key={`${member.subject_type}-${member.subject_id}-${member.created_at}`} data-role-member={member.label} className="border-t border-line">
                      <td className="px-3 py-2.5">
                        <p className="text-[13px] font-medium text-ink">{member.label}</p>
                        <p className="text-[11.5px] text-muted">
                          {member.subject_type === "group"
                            ? "group"
                            : member.subject_type === "service_account"
                              ? "service account"
                              : "account"}
                        </p>
                      </td>
                      <td className="px-3 py-2.5 font-mono text-[11.5px] text-muted">
                        {member.scope.type}
                        {member.scope.resource_id ? ` · ${member.scope.resource_id}` : ""}
                        {member.scope.site_id ? ` · ${member.scope.site_id.slice(0, 8)}…` : ""}
                      </td>
                      <td className="px-3 py-2.5 text-[12px] text-muted">
                        {member.expires_at ? formatTimestamp(member.expires_at) : "never"}
                      </td>
                      <td className="px-3 py-2.5">
                        <span
                          className={`rounded-md border px-1.5 py-0.5 text-[11px] font-medium ${
                            member.active
                              ? "border-positive/40 bg-positive-soft text-positive"
                              : "border-line bg-canvas text-muted"
                          }`}
                        >
                          {member.active ? "Active" : member.revoked_at ? "Revoked" : "Expired"}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-[12px] text-muted">{formatTimestamp(member.created_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </section>
      ) : null}

      {tab === "inherited" ? (
        <section className="flex flex-col gap-2" aria-label="Inherited by">
          {detail.inherited_by.length === 0 ? (
            <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
              <ShieldAlert className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">No role inherits from this one</p>
              <p className="max-w-md text-[12.5px] text-muted">
                Roles that point their parent link here appear in this list.
              </p>
            </div>
          ) : (
            <ul className="flex flex-col divide-y divide-line rounded-xl border border-line bg-surface">
              {detail.inherited_by.map((role) => (
                <li key={role.id} data-role-child={role.key} className="flex items-center justify-between gap-2 px-3 py-2.5">
                  <div>
                    <Link href={`/settings/iam/roles/${role.id}`} className="text-[13px] font-medium text-ink hover:text-accent-strong">
                      {role.name}
                    </Link>
                    <p className="font-mono text-[11.5px] text-muted">{role.key}</p>
                  </div>
                  <span className="font-mono text-[12px] text-muted">priority {role.priority}</span>
                </li>
              ))}
            </ul>
          )}
        </section>
      ) : null}

      {tab === "history" ? (
        <section className="flex flex-col gap-2" aria-label="History">
          {versions === null ? (
            <div className="rounded-xl border border-line bg-surface p-4">
              <div className="h-8 animate-pulse rounded-lg bg-quiet-soft" />
            </div>
          ) : versions.length === 0 ? (
            <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
              <History className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">No history yet</p>
              <p className="max-w-md text-[12.5px] text-muted">
                Every save appends a version; the first one appears with the first change.
              </p>
            </div>
          ) : (
            <ul className="flex flex-col gap-2">
              {versions.map((version) => (
                <li key={version.version} data-role-version-row={version.version} className="rounded-xl border border-line bg-surface p-3">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex items-center gap-2">
                      <span className="font-mono text-[12.5px] font-medium text-ink">v{version.version}</span>
                      <span className="rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[11px] text-muted">
                        {version.change}
                      </span>
                      <span className="text-[12px] text-muted">{version.name}</span>
                    </div>
                    <span className="text-[11.5px] text-muted">
                      {formatTimestamp(version.created_at)} · {version.diff_total} change
                      {version.diff_total === 1 ? "" : "s"}
                    </span>
                  </div>
                  {version.diff_total > 0 ? (
                    <ul data-role-version-diff={version.version} className="mt-2 flex flex-col gap-1 text-[12px]">
                      {version.diff.added.map((entry) => (
                        <li key={`a-${version.version}-${entry.key}`} className="flex items-center justify-between gap-2">
                          <span className="font-mono text-[11.5px]">{entry.key}</span>
                          <span className="text-positive">added · {entry.effect}</span>
                        </li>
                      ))}
                      {version.diff.changed.map((entry) => (
                        <li key={`c-${version.version}-${entry.key}`} className="flex items-center justify-between gap-2">
                          <span className="font-mono text-[11.5px]">{entry.key}</span>
                          <span className="text-caution">
                            {entry.from} → {entry.to}
                          </span>
                        </li>
                      ))}
                      {version.diff.removed.map((entry) => (
                        <li key={`r-${version.version}-${entry.key}`} className="flex items-center justify-between gap-2">
                          <span className="font-mono text-[11.5px]">{entry.key}</span>
                          <span className="text-muted">removed · was {entry.effect}</span>
                        </li>
                      ))}
                    </ul>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </section>
      ) : null}
    </div>
  );
}

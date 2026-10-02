"use client";

/**
 * The chart of accounts (REQ-054, slice 1): `/accounting/accounts`.
 *
 * ## Why the tree is a tree and not a flat table with a kind column
 *
 * A chart of accounts is a hierarchy, and the migration models it as one — a `parent_id` with no
 * depth rule, because a fixed depth is a limit the data does not have and every chart of accounts
 * ever built grew a level. The screen renders the same shape: five sections in report order, each
 * with its children indented beneath it. A flat table would have made the operator reconstruct
 * the hierarchy in their head, which is the one thing a chart exists not to require.
 *
 * ## Why there is no delete button
 *
 * **There is no delete route at all.** A journal line references an account with
 * `on delete restrict`, so a delete is a database error naming a constraint. The action that
 * answers "we do not use this any more" is deactivation, and it is labelled as that. A trash icon
 * that quietly deactivated would be a lie in the glyph — the operator would learn the difference
 * by trying to delete something with postings and reading a foreign-key error.
 *
 * The "used by N lines" column is the guard's number rather than a decoration: it is what tells
 * somebody whether closing an account is routine or a decision, and it is why the list carries a
 * count at all.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Plus, Power, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  ACCOUNT_KINDS,
  createAccount,
  fetchAccounts,
  setAccountActive,
  updateAccount,
  type Account,
} from "@/lib/accounting";

const COLUMNS = 5;

const KIND_LABELS: Record<string, string> = Object.fromEntries(
  ACCOUNT_KINDS.map((kind) => [kind.value, kind.label]),
);

/** The children of `parent`, in the order the tree renders them. `null` is a top-level account. */
function childrenOf(rows: Account[], parent: string | null): Account[] {
  return rows.filter((row) => (row.parent_id ?? null) === parent);
}

export function AccountsView() {
  const [rows, setRows] = useState<Account[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [renaming, setRenaming] = useState<Account | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setRows(await fetchAccounts());
    } catch (caught) {
      setError(toScreenError(caught, "The chart of accounts could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const toggle = useCallback(
    async (account: Account) => {
      setNotice(null);
      try {
        const after = await setAccountActive(account.id, !account.active);
        setRows((current) => current.map((row) => (row.id === account.id ? after : row)));
        setNotice(
          after.active
            ? `${after.code} is active again.`
            : `${after.code} is closed. It keeps its ${after.line_count} journal ${
                after.line_count === 1 ? "line" : "lines"
              } and can be reopened.`,
        );
      } catch (caught) {
        setError(toScreenError(caught, "The account could not be closed."));
      }
    },
    [],
  );

  // Grouped by kind once, in the order the reports group by, so the screen and a trial balance
  // cannot disagree about which order things come in.
  const byKind = useMemo(() => {
    return ACCOUNT_KINDS.map((kind) => ({
      kind: kind.value,
      label: kind.label,
      roots: childrenOf(rows, null).filter((row) => row.kind === kind.value),
    })).filter((group) => group.roots.length > 0);
  }, [rows]);

  return (
    <section className="space-y-4" data-qa-accounting-accounts>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[13px] text-muted-foreground">
          {rows.length} {rows.length === 1 ? "account" : "accounts"} ·{" "}
          {rows.filter((row) => !row.active).length} closed
        </p>
        <button
          type="button"
          onClick={() => setAdding(true)}
          data-qa-accounting-accounts-new
          className="inline-flex h-8 items-center gap-1.5 rounded-md bg-foreground px-2.5 text-sm text-background"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Add an account
        </button>
      </div>

      {notice ? (
        <p
          role="status"
          data-qa-accounting-accounts-notice
          className="rounded-md border border-emerald-200 bg-emerald-50 px-3 py-2 text-sm text-emerald-900"
        >
          {notice}
        </p>
      ) : null}

      <div className="rounded-lg border border-border bg-card">
        {loading ? (
          <LoadingTable columns={COLUMNS} rows={6} />
        ) : error ? (
          <ErrorState error={error} onRetry={() => void load()} />
        ) : rows.length === 0 ? (
          <EmptyState
            title="This organization has no chart of accounts yet"
            hint="Every tenant is seeded with a lite chart the moment it is created. An empty one means the seed did not run."
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted-foreground">
                  <th className="px-4 py-2.5 font-medium">Code</th>
                  <th className="px-4 py-2.5 font-medium">Name</th>
                  <th className="px-4 py-2.5 font-medium">State</th>
                  <th className="px-4 py-2.5 text-right font-medium">Journal lines</th>
                  <th className="px-4 py-2.5 text-right font-medium">Actions</th>
                </tr>
              </thead>
              <tbody>
                {byKind.flatMap((group) => [
                  <tr key={`${group.kind}-header`}>
                    <td
                      colSpan={COLUMNS}
                      className="bg-quiet-soft/40 px-4 py-1.5 text-[11.5px] font-semibold uppercase tracking-wide text-muted-foreground"
                    >
                      {group.label}
                    </td>
                  </tr>,
                  ...group.roots.flatMap((root) => [
                    <AccountRow
                      key={root.id}
                      account={root}
                      depth={0}
                      rows={rows}
                      onToggle={toggle}
                      onRename={setRenaming}
                    />,
                  ]),
                ])}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {adding ? (
        <AddAccountDrawer
          rows={rows}
          onClose={() => setAdding(false)}
          onCreated={(account) => {
            setAdding(false);
            setNotice(`${account.code} · ${account.name} was added to the chart.`);
            void load();
          }}
        />
      ) : null}

      {renaming ? (
        <RenameDrawer
          account={renaming}
          rows={rows}
          onClose={() => setRenaming(null)}
          onSaved={(after) => {
            setRenaming(null);
            setRows((current) => current.map((row) => (row.id === after.id ? after : row)));
          }}
        />
      ) : null}
    </section>
  );
}

/** One account and, beneath it, its children. Recursion is bounded by the data: a cycle cannot
 *  be written, because the store refuses a parent that is inside its own subtree. */
function AccountRow({
  account,
  depth,
  rows,
  onToggle,
  onRename,
}: {
  account: Account;
  depth: number;
  rows: Account[];
  onToggle: (account: Account) => void;
  onRename: (account: Account) => void;
}) {
  const children = childrenOf(rows, account.id);
  return (
    <>
      <tr
        data-qa-accounting-account={account.code}
        className="border-b border-line last:border-b-0"
      >
        <td
          className="px-4 py-2 font-mono text-[12.5px]"
          style={depth ? { paddingLeft: `${1 + depth}rem` } : undefined}
        >
          {account.code}
        </td>
        <td
          className={`px-4 py-2 ${account.active ? "" : "text-muted-foreground line-through"}`}
          style={depth ? { paddingLeft: `${1 + depth}rem` } : undefined}
        >
          {account.name}
          {account.system ? (
            <span className="ml-2 rounded border border-line px-1 py-px text-[10.5px] text-muted-foreground">
              seeded
            </span>
          ) : null}
        </td>
        <td className="px-4 py-2">
          {account.active ? (
            <span className="text-[12px] text-muted-foreground">Active</span>
          ) : (
            <span className="text-[12px] text-amber-800">Closed</span>
          )}
        </td>
        <td className="px-4 py-2 text-right tabular-nums">{account.line_count}</td>
        <td className="px-4 py-2 text-right">
          <div className="inline-flex items-center gap-1">
            <button
              type="button"
              onClick={() => onRename(account)}
              data-qa-accounting-account-rename={account.code}
              className="h-7 rounded-md border border-line px-2 text-[12px]"
            >
              Rename
            </button>
            <button
              type="button"
              onClick={() => onToggle(account)}
              data-qa-accounting-account-toggle={account.code}
              className="inline-flex h-7 items-center gap-1 rounded-md border border-line px-2 text-[12px]"
            >
              <Power className="h-3.5 w-3.5" aria-hidden />
              {account.active ? "Close" : "Reopen"}
            </button>
          </div>
        </td>
      </tr>
      {children.map((child) => (
        <AccountRow
          key={child.id}
          account={child}
          depth={depth + 1}
          rows={rows}
          onToggle={onToggle}
          onRename={onRename}
        />
      ))}
    </>
  );
}

/** The add form. Code, name, kind and an optional parent. */
function AddAccountDrawer({
  rows,
  onClose,
  onCreated,
}: {
  rows: Account[];
  onClose: () => void;
  onCreated: (account: Account) => void;
}) {
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [kind, setKind] = useState<string>(ACCOUNT_KINDS[0].value);
  const [parentId, setParentId] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const submit = async () => {
    setSaving(true);
    setError(null);
    try {
      const account = await createAccount({
        code: code.trim(),
        name: name.trim(),
        kind,
        parent_id: parentId || null,
      });
      onCreated(account);
    } catch (caught) {
      setError(toScreenError(caught, "The account could not be added."));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Drawer title="Add an account" onClose={onClose} testId="accounting-account-add">
      <div className="space-y-3">
        <label className="block space-y-1 text-[12.5px]">
          <span className="text-muted-foreground">Code</span>
          <input
            value={code}
            onChange={(event) => setCode(event.target.value)}
            placeholder="1600"
            data-qa-accounting-account-code
            className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
          />
          <span className="text-[11.5px] text-muted-foreground">
            What a journal line picks. It cannot be changed later — a code a past entry refers to
            must keep meaning the same account.
          </span>
        </label>
        <label className="block space-y-1 text-[12.5px]">
          <span className="text-muted-foreground">Name</span>
          <input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="Equipment"
            data-qa-accounting-account-name
            className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
          />
        </label>
        <label className="block space-y-1 text-[12.5px]">
          <span className="text-muted-foreground">Kind</span>
          <select
            value={kind}
            onChange={(event) => setKind(event.target.value)}
            data-qa-accounting-account-kind
            className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
          >
            {ACCOUNT_KINDS.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>
        <label className="block space-y-1 text-[12.5px]">
          <span className="text-muted-foreground">Rolls up into</span>
          <select
            value={parentId}
            onChange={(event) => setParentId(event.target.value)}
            data-qa-accounting-account-parent
            className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
          >
            <option value="">Nothing — a top-level account</option>
            {rows.map((row) => (
              <option key={row.id} value={row.id}>
                {row.code} · {row.name} ({KIND_LABELS[row.kind]})
              </option>
            ))}
          </select>
        </label>
        {error ? <ErrorState error={error} onRetry={() => void submit()} /> : null}
      </div>
      <footer className="mt-4 flex items-center justify-end gap-2">
        <button
          type="button"
          onClick={onClose}
          className="inline-flex h-8 items-center rounded-md border border-line px-3 text-sm"
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={() => void submit()}
          disabled={saving || !code.trim() || !name.trim()}
          data-qa-accounting-account-submit
          className="inline-flex h-8 items-center rounded-md bg-foreground px-3 text-sm text-background disabled:opacity-50"
        >
          {saving ? "Adding…" : "Add account"}
        </button>
      </footer>
    </Drawer>
  );
}

/**
 * The rename drawer.
 *
 * Only the name and the parent are editable. The code is not in the form **on purpose** — not
 * disabled, not greyed out: absent. A field that exists and cannot be changed is an invitation to
 * try, and the server would refuse it with a message about a field the form never showed.
 */
function RenameDrawer({
  account,
  rows,
  onClose,
  onSaved,
}: {
  account: Account;
  rows: Account[];
  onClose: () => void;
  onSaved: (account: Account) => void;
}) {
  const [name, setName] = useState(account.name);
  const [parentId, setParentId] = useState(account.parent_id ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ScreenErrorValue>(null);

  // Only accounts of the **same kind** are offered as a parent. A chart of accounts is five
  // separate trees — an asset rolling up into an income account is not a hierarchy, it is a
  // misfiled row, and a total that groups by kind would then count it in both. The server does
  // not enforce this (it cannot see the whole tree cheaply), so the form does not offer it.
  const reparentChoices = rows.filter((row) => row.kind === account.kind && row.active);

  const submit = async () => {
    setSaving(true);
    setError(null);
    try {
      onSaved(
        await updateAccount(account.id, {
          name: name.trim(),
          parent_id: parentId || null,
        }),
      );
    } catch (caught) {
      setError(toScreenError(caught, "The account could not be saved."));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Drawer
      title={`${account.code} · ${account.name}`}
      onClose={onClose}
      testId="accounting-account-rename"
    >
      <div className="space-y-3">
        <label className="block space-y-1 text-[12.5px]">
          <span className="text-muted-foreground">Name</span>
          <input
            value={name}
            onChange={(event) => setName(event.target.value)}
            data-qa-accounting-account-rename-name
            className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
          />
        </label>
        <label className="block space-y-1 text-[12.5px]">
          <span className="text-muted-foreground">Rolls up into</span>
          <select
            value={parentId}
            onChange={(event) => setParentId(event.target.value)}
            data-qa-accounting-account-rename-parent
            className="h-8 w-full rounded-md border border-line bg-background px-2 text-sm"
          >
            <option value="">Nothing — a top-level account</option>
            {reparentChoices
              .filter((row) => row.id !== account.id)
              .map((row) => (
                <option key={row.id} value={row.id}>
                  {row.code} · {row.name} ({KIND_LABELS[row.kind]})
                </option>
              ))}
          </select>
          <span className="text-[11.5px] text-muted-foreground">
            The code {account.code} is fixed. It is what {account.line_count} journal{" "}
            {account.line_count === 1 ? "line refers" : "lines refer"} to, and renaming it would
            make every one of them a claim about an account that no longer exists.
          </span>
        </label>
        {error ? <ErrorState error={error} onRetry={() => void submit()} /> : null}
      </div>
      <footer className="mt-4 flex items-center justify-end gap-2">
        <button
          type="button"
          onClick={onClose}
          className="inline-flex h-8 items-center rounded-md border border-line px-3 text-sm"
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={() => void submit()}
          disabled={saving || !name.trim()}
          data-qa-accounting-account-rename-submit
          className="inline-flex h-8 items-center rounded-md bg-foreground px-3 text-sm text-background disabled:opacity-50"
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </footer>
    </Drawer>
  );
}

/** The right-hand sheet both drawers share. */
function Drawer({
  title,
  onClose,
  testId,
  children,
}: {
  title: string;
  onClose: () => void;
  testId: string;
  children: React.ReactNode;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex justify-end bg-foreground/20"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      data-qa={testId}
    >
      <div className="flex h-full w-full max-w-lg flex-col overflow-y-auto bg-background shadow-xl">
        <header className="flex items-center justify-between border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">{title}</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            data-qa={`${testId}-close`}
            className="inline-flex h-7 w-7 items-center justify-center rounded-md hover:bg-muted"
          >
            <X className="h-4 w-4" aria-hidden />
          </button>
        </header>
        <div className="px-5 py-4">{children}</div>
      </div>
    </div>
  );
}

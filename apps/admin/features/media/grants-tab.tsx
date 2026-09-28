"use client";

/**
 * The permissions tab of the file detail screen (docs/requests/REQ-010, slice 4).
 *
 * A grant says *not this one, not here*. That asymmetry is the whole design, and this screen
 * is built so an operator cannot misread it in either direction:
 *
 * **A grant can only narrow.** Nothing on this tab hands a capability out. The permission
 * catalogue decides who may touch the library; these rows decide who may *not* reach a given
 * folder or file inside it. A screen that suggested otherwise — a green "grant access" button —
 * would be a second source of truth beside IAM, and the first thing somebody would build on it
 * is "I gave a contractor read on the whole library and he cannot sign in to the panel".
 *
 * **A deny wins, and the tab says where.** The sentence above the table comes from the
 * resolver and names the file, its folder, or an ancestor. An operator who cannot point at the
 * row that refused a colleague stops trusting the answer, and a "deny wins" rule with no place
 * to look is folklore.
 *
 * **The chain is on screen.** A file's tab lists the folders it inherits from, nearest first,
 * with which of them carry a deny. A reader who cannot tell whether a folder grant covers its
 * children will either duplicate the grant onto every file or delete the folder grant thinking
 * it did nothing.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  Ban,
  Check,
  FolderTree,
  Loader2,
  Plus,
  ShieldCheck,
  Trash2,
  Users,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  createMediaGrant,
  fetchGrantSubjects,
  fetchMediaGrants,
  removeMediaGrant,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { MediaGrant, MediaGrantSubject, MediaGrantsResponse } from "@/lib/types";

/** The four capability bits, in the order the table shows them. */
const CAPABILITIES = [
  { key: "can_read", label: "Read" },
  { key: "can_write", label: "Write" },
  { key: "can_delete", label: "Delete" },
  { key: "can_share", label: "Share" },
] as const;

/** The kind of node the tab is about. */
type TargetKind = "file" | "folder";

/** How a row's effect is drawn. A deny is the loud one; that is the point of it. */
function effectTone(effect: string): string {
  return effect === "deny"
    ? "bg-negative-soft text-negative"
    : "bg-positive-soft text-positive";
}

/** What an effect does, in a sentence rather than in the API's own vocabulary. */
function effectSentence(effect: string): string {
  return effect === "deny"
    ? "Refuses — whatever the permissions say, this subject cannot do this here."
    : "Records that this subject was named here. It does not hand a capability out.";
}

/**
 * The permissions tab: the grants on one node, the chain a file inherits from, and the form
 * that adds one.
 */
export function GrantsTab({
  targetKind,
  targetId,
  siteId,
}: {
  targetKind: TargetKind;
  targetId: string;
  siteId: string;
}) {
  const [data, setData] = useState<MediaGrantsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState(false);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await fetchMediaGrants(targetKind, targetId));
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "The grants could not be read.");
    }
  }, [targetKind, targetId]);

  useEffect(() => {
    void load();
  }, [load]);

  const denies = useMemo(
    () => data?.grants.filter((grant) => grant.effect === "deny").length ?? 0,
    [data],
  );

  const onRemove = useCallback(
    async (grant: MediaGrant) => {
      const who = grant.subject_label ?? "this subject";
      if (
        !window.confirm(
          `Remove the ${grant.effect} for ${who}?\n\n` +
            "Nobody's access changes until the row is gone, and it is gone the moment you confirm.",
        )
      ) {
        return;
      }
      setBusy(true);
      setNotice(null);
      try {
        await removeMediaGrant(grant.id);
        setNotice(`The ${grant.effect} for ${who} is gone.`);
        await load();
      } catch (err) {
        setError(err instanceof Error ? err.message : "The grant could not be removed.");
      } finally {
        setBusy(false);
      }
    },
    [load, targetId, targetKind],
  );

  if (error && !data) {
    return (
      <div className="space-y-3">
        <p className="text-[12px] text-negative" role="alert">
          {error}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="rounded border border-line px-2 py-1 text-[12px] text-ink hover:bg-quiet-soft"
        >
          Try again
        </button>
      </div>
    );
  }

  if (!data) {
    return (
      <div className="flex items-center gap-2 py-6 text-[12px] text-muted">
        <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
        Reading the grants…
      </div>
    );
  }

  return (
    <div className="space-y-3" data-testid="media-grants-tab">
      {/* The rule, stated on the screen rather than left to be inferred from the table. */}
      <p className="rounded border border-line bg-quiet-soft px-2.5 py-2 text-[11px] leading-relaxed text-muted">
        A grant can only <strong className="text-ink">narrow</strong> access. Whether somebody
        may reach this {targetKind} at all is decided by their role; these rows decide who may
        <em> not</em> reach it. A <strong className="text-ink">deny</strong> wins over an
        allow at any depth, so a folder-wide allow never re-opens a file a deny has closed.
      </p>

      {data.chain.length > 0 ? <Chain chain={data.chain} /> : null}

      {denies > 0 ? (
        <p
          className="flex items-start gap-1.5 rounded border border-negative/30 bg-negative-soft px-2.5 py-2 text-[11px] text-negative"
          data-testid="media-grants-deny-count"
        >
          <AlertTriangle className="mt-px h-3.5 w-3.5 shrink-0" aria-hidden />
          <span>
            {denies === 1
              ? "One subject is refused here."
              : `${denies} subjects are refused here.`}{" "}
            Whoever they are cannot open this {targetKind}, whatever their role allows.
          </span>
        </p>
      ) : null}

      {notice ? (
        <p className="text-[11px] text-positive" role="status" data-testid="media-grants-notice">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p className="text-[11px] text-negative" role="alert" data-testid="media-grants-error">
          {error}
        </p>
      ) : null}

      {data.grants.length === 0 ? (
        <EmptyState
          title={`No grants on this ${targetKind}`}
          hint={
            data.inherits
              ? "Everything here follows the permissions everybody's role already gives them. A grant only ever takes something away, so an empty list is the normal state — not a warning."
              : "This file follows whatever its folders say. Add a deny to keep one person out of it, or an allow to record that they were named here."
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-left text-[12px]">
            <caption className="sr-only">
              Grants on this {targetKind}, with the capabilities each one sets
            </caption>
            <thead>
              <tr className="border-b border-line text-[11px] text-muted">
                <th scope="col" className="py-1.5 pr-2 font-medium">
                  Subject
                </th>
                <th scope="col" className="py-1.5 pr-2 font-medium">
                  Kind
                </th>
                {CAPABILITIES.map((capability) => (
                  <th key={capability.key} scope="col" className="py-1.5 pr-2 font-medium">
                    {capability.label}
                  </th>
                ))}
                <th scope="col" className="py-1.5 pr-2 font-medium">
                  Effect
                </th>
                <th scope="col" className="py-1.5 pr-2 font-medium">
                  Added
                </th>
                <th scope="col" className="py-1.5 font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {data.grants.map((grant) => (
                <tr
                  key={grant.id}
                  className="border-b border-line/60"
                  data-testid="media-grant-row"
                  data-effect={grant.effect}
                >
                  <td className="py-1.5 pr-2">
                    {grant.subject_label ?? (
                      <span
                        className="text-muted"
                        title="This subject has been deleted. The row refuses nobody and grants nobody, and you can remove it."
                      >
                        Deleted subject
                      </span>
                    )}
                  </td>
                  <td className="py-1.5 pr-2 capitalize text-muted">{grant.subject_kind}</td>
                  {CAPABILITIES.map((capability) => (
                    <td key={capability.key} className="py-1.5 pr-2">
                      {grant[capability.key] ? (
                        <Check className="h-3.5 w-3.5 text-ink" aria-label="set" />
                      ) : (
                        <span className="text-line" aria-label="not set">
                          —
                        </span>
                      )}
                    </td>
                  ))}
                  <td className="py-1.5 pr-2">
                    <span
                      className={`rounded px-1.5 py-0.5 text-[10px] font-medium ${effectTone(grant.effect)}`}
                    >
                      {grant.effect === "deny" ? "Refuses" : "Named"}
                    </span>
                  </td>
                  <td className="py-1.5 pr-2 text-muted">{formatTimestamp(grant.created_at)}</td>
                  <td className="py-1.5">
                    <button
                      type="button"
                      onClick={() => void onRemove(grant)}
                      disabled={busy}
                      className="rounded p-1 text-muted hover:bg-quiet-soft hover:text-negative disabled:opacity-50"
                      aria-label={`Remove the ${grant.effect} for ${grant.subject_label ?? "this subject"}`}
                      data-testid="media-grant-remove"
                    >
                      <Trash2 className="h-3.5 w-3.5" aria-hidden />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {adding ? (
        <AddGrantForm
          targetKind={targetKind}
          targetId={targetId}
          siteId={siteId}
          effect={denies > 0 ? "allow" : "deny"}
          onCancel={() => setAdding(false)}
          onSaved={async (grant) => {
            setAdding(false);
            setNotice(
              grant.effect === "deny"
                ? `${grant.subject_label ?? "That subject"} can no longer reach this.`
                : `${grant.subject_label ?? "That subject"} is named here.`,
            );
            setFieldError(null);
            await load();
          }}
          onFieldError={setFieldError}
        />
      ) : (
        <button
          type="button"
          onClick={() => {
            setAdding(true);
            setNotice(null);
            setFieldError(null);
          }}
          className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1.5 text-[12px] text-ink hover:bg-quiet-soft"
          data-testid="media-grant-add"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden />
          Add a grant
        </button>
      )}

      {fieldError ? (
        <p className="text-[11px] text-negative" role="alert" data-testid="media-grant-field-error">
          {fieldError.message}
        </p>
      ) : null}
    </div>
  );
}

/** The chain a file inherits from, nearest folder first, with which of them carry a deny. */
function Chain({ chain }: { chain: MediaGrantsResponse["chain"] }) {
  return (
    <div
      className="rounded border border-line px-2.5 py-2"
      data-testid="media-grants-chain"
    >
      <p className="mb-1.5 flex items-center gap-1.5 text-[11px] font-medium text-ink">
        <FolderTree className="h-3.5 w-3.5" aria-hidden />
        Inherited from
      </p>
      <ol className="space-y-1">
        {chain.map((node, index) => (
          <li
            key={node.id}
            className="flex items-center gap-2 text-[11px]"
            data-testid="media-grants-chain-node"
          >
            <span className="text-muted">{index === 0 ? "in" : "via"}</span>
            <span className="font-mono text-ink">{node.path}</span>
            {node.has_deny ? (
              <span className="inline-flex items-center gap-1 rounded bg-negative-soft px-1.5 py-0.5 text-[10px] text-negative">
                <Ban className="h-3 w-3" aria-hidden />
                refuses
              </span>
            ) : node.grant_count > 0 ? (
              <span className="rounded bg-quiet-soft px-1.5 py-0.5 text-[10px] text-muted">
                {node.grant_count} grant{node.grant_count === 1 ? "" : "s"}
              </span>
            ) : (
              <span className="text-[10px] text-muted">no grants</span>
            )}
          </li>
        ))}
      </ol>
    </div>
  );
}

/**
 * The add form: a debounced subject picker, the four capability bits, and the effect.
 *
 * The deny-with-no-bit rule is refused here as well as in the API. It is a rule an operator
 * can hit by accident — ticking "deny" and forgetting the bits — and the alternative is
 * pressing Save and being told, which reads as the platform disagreeing rather than as a
 * correction.
 */
function AddGrantForm({
  targetKind,
  targetId,
  siteId,
  effect: initialEffect,
  onCancel,
  onSaved,
  onFieldError,
}: {
  targetKind: TargetKind;
  targetId: string;
  siteId: string;
  effect: string;
  onCancel: () => void;
  onSaved: (grant: MediaGrant) => Promise<void>;
  onFieldError: (error: { field: string; message: string } | null) => void;
}) {
  const [subjects, setSubjects] = useState<MediaGrantSubject[] | null>(null);
  const [search, setSearch] = useState("");
  const [subject, setSubject] = useState("");
  const [effect, setEffect] = useState<"allow" | "deny">(
    initialEffect === "deny" ? "deny" : "allow",
  );
  const [bits, setBits] = useState<Record<string, boolean>>({
    can_read: true,
    can_write: false,
    can_delete: false,
    can_share: false,
  });
  const [busy, setBusy] = useState(false);
  const submit = useCallback(
    async (event: React.FormEvent) => {
      event.preventDefault();
      if (!subject) {
        onFieldError({ field: "subject", message: "Choose who this grant is about." });
        return;
      }
      const chosen = subjects?.find((row) => `${row.kind}:${row.id}` === subject);
      if (!chosen) {
        onFieldError({ field: "subject", message: "That subject is no longer offered." });
        return;
      }
      if (effect === "deny" && !CAPABILITIES.some((capability) => bits[capability.key])) {
        onFieldError({
          field: "effect",
          message:
            "A deny has to say what it removes — tick at least one of read, write, delete or share.",
        });
        return;
      }
      setBusy(true);
      onFieldError(null);
      try {
        const saved = await createMediaGrant(targetKind, targetId, {
          subject_kind: chosen.kind,
          subject_id: chosen.id,
          ...bits,
          effect,
        });
        await onSaved(saved);
      } catch (err) {
        const apiError = err as { code?: string; message?: string };
        // A refusal that names a field goes under that field; anything else is a plain error
        // above, because there is no input to point at.
        if (apiError.code === "effect" || apiError.code === "subject_kind") {
          onFieldError({ field: apiError.code, message: apiError.message ?? "refused" });
        } else {
          onFieldError({
            field: "",
            message: apiError.message ?? "The grant could not be saved.",
          });
        }
      } finally {
        setBusy(false);
      }
    },
    [bits, effect, onFieldError, onSaved, subject, subjects, targetId, targetKind],
  );

  // Debounced: the picker is a query on every keystroke otherwise, and a search box that
  // issues a request per character is a search box that feels broken under load.
  useEffect(() => {
    let cancelled = false;
    const handle = window.setTimeout(() => {
      fetchGrantSubjects(siteId, search.trim())
        .then((rows) => {
          if (!cancelled) {
            setSubjects(rows);
          }
        })
        .catch(() => {
          if (!cancelled) {
            setSubjects([]);
          }
        });
    }, 250);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
  }, [siteId, search]);

  return (
    <form
      onSubmit={(event) => void submit(event)}
      className="space-y-2.5 rounded border border-line bg-quiet-soft px-2.5 py-2.5"
      data-testid="media-grant-form"
    >
      <div>
        <label htmlFor="grant-subject-search" className="mb-1 block text-[11px] font-medium text-ink">
          Who is this about
        </label>
        <input
          id="grant-subject-search"
          type="search"
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Search people, teams and roles…"
          className="w-full rounded border border-line bg-panel px-2 py-1.5 text-[12px] text-ink"
        />
        <ul
          className="mt-1.5 max-h-40 overflow-auto rounded border border-line"
          data-testid="media-grant-subjects"
        >
          {(subjects ?? []).map((row) => {
            const value = `${row.kind}:${row.id}`;
            const active = value === subject;
            return (
              <li key={value}>
                <button
                  type="button"
                  onClick={() => setSubject(value)}
                  aria-pressed={active}
                  className={`flex w-full items-center gap-2 px-2 py-1.5 text-left text-[12px] ${
                    active ? "bg-accent-soft text-ink" : "text-ink hover:bg-quiet-soft"
                  }`}
                >
                  <span className="capitalize text-muted">{row.kind}</span>
                  <span className="font-medium">{row.label}</span>
                  <span className="ml-auto text-[11px] text-muted">{row.detail}</span>
                  {row.suggested ? (
                    <span className="rounded bg-positive-soft px-1.5 py-0.5 text-[10px] text-positive">
                      team
                    </span>
                  ) : null}
                </button>
              </li>
            );
          })}
          {subjects && subjects.length === 0 ? (
            <li className="px-2 py-1.5 text-[11px] text-muted">
              Nobody here matches “{search}”.
            </li>
          ) : null}
        </ul>
      </div>

      <fieldset>
        <legend className="mb-1 text-[11px] font-medium text-ink">What it does</legend>
        <div className="flex gap-3 text-[11px] text-ink">
          {["deny", "allow"].map((value) => (
            <label key={value} className="inline-flex items-center gap-1.5">
              <input
                type="radio"
                name="grant-effect"
                value={value}
                checked={effect === value}
                onChange={() => setEffect(value as "deny" | "allow")}
                data-testid={`media-grant-effect-${value}`}
              />
              {value === "deny" ? "Refuse" : "Name"}
            </label>
          ))}
        </div>
        <p className="mt-1 text-[10px] text-muted">{effectSentence(effect)}</p>
      </fieldset>

      <fieldset>
        <legend className="mb-1 text-[11px] font-medium text-ink">Which capabilities</legend>
        <div className="flex flex-wrap gap-3 text-[11px] text-ink">
          {CAPABILITIES.map((capability) => (
            <label key={capability.key} className="inline-flex items-center gap-1.5">
              <input
                type="checkbox"
                checked={bits[capability.key] ?? false}
                onChange={(event) =>
                  setBits({ ...bits, [capability.key]: event.target.checked })
                }
                data-testid={`media-grant-bit-${capability.key}`}
              />
              {capability.label}
            </label>
          ))}
        </div>
      </fieldset>

      <div className="flex gap-2">
        <button
          type="submit"
          disabled={busy}
          className="inline-flex items-center gap-1.5 rounded bg-accent-strong px-2.5 py-1.5 text-[12px] text-white disabled:opacity-50"
          data-testid="media-grant-save"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <ShieldCheck className="h-3.5 w-3.5" aria-hidden />}
          Save the grant
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1.5 text-[12px] text-ink hover:bg-quiet-soft"
        >
          <X className="h-3.5 w-3.5" aria-hidden />
          Cancel
        </button>
      </div>
      <p className="text-[10px] text-muted">
        Saving writes an audit entry naming the subject and the capabilities.
      </p>
    </form>
  );
}

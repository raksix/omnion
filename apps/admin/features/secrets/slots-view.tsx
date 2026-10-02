"use client";

/**
 * `/secrets/slots` — the credential slot assignment matrix (docs/requests/REQ-125, slice 2).
 *
 * The screen's reason to exist: **a consumer never holds a secret id.** A workload asks for
 * `smtp` in the `production` environment and receives whatever this matrix points at, so
 * swapping a credential is a row update here instead of a code change in every module. That is
 * the whole argument for the feature, and the screen is designed to make the consequence of an
 * edit obvious before it is saved.
 *
 * Three rules the editor keeps, each of them a cost an operator has already paid somewhere:
 *
 * 1. **Primary and fallback must differ.** A fallback that is the primary is a no-op wearing a
 *    safety net's name. The API answers `409`; the editor says so next to the fallback picker
 *    rather than as a page-level error, because it is about that field.
 * 2. **Taking a slot away asks first, and names the consumer.** `last_resolved_by` is recorded on
 *    every resolution, so the confirmation can say *which* workload is about to lose its
 *    credential instead of "are you sure?".
 * 3. **A resolution is shown, never a value.** "Resolve" is a preview a panel user can press: it
 *    answers the name and version the consumer would get, plus whether the fallback had to answer.
 *
 * Keyboard: `/` focuses the search box, `n` opens the editor for the first matching row, `Esc`
 * closes it. Under `sm:` the table becomes cards, and the editor is a single column.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  ArrowRightLeft,
  CheckCircle2,
  Link2,
  Plus,
  RefreshCw,
  Search,
  Unlink,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  assignSlot,
  fetchSlots,
  resolveSlot,
  type AssignableCredential,
  type CredentialSlot,
  type SlotDef,
  type SlotResolution,
  type SlotsResponse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The scope kinds a slot can be assigned for, in the panel's own words. */
const SCOPES: { value: string; label: string; hint: string }[] = [
  { value: "environment", label: "Environment", hint: "production, staging, local" },
  { value: "site", label: "Site", hint: "one site of the installation" },
  { value: "module", label: "Module", hint: "one business module" },
  { value: "organization", label: "Organization", hint: "one tenant" },
];

/** What an editor is working on: an existing row, or a catalogue slot that has none yet. */
type Draft = {
  scope_type: string;
  scope_id: string;
  slot: string;
  row: CredentialSlot | null;
};

/** `/secrets/slots`. */
export function SlotsView() {
  const [state, setState] = useState<SlotsResponse | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [needle, setNeedle] = useState("");
  const [draft, setDraft] = useState<Draft | null>(null);
  const [resolution, setResolution] = useState<SlotResolution | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const search = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    try {
      const next = await fetchSlots();
      setState(next);
      setStatus("ready");
      setLoadError(null);
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "network", message: "The slot matrix could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // `/` searches, `n` opens the editor, `Escape` closes it. Bound once, on the screen, so the
  // list context behaves like the other list screens in the panel.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable;
      if (event.key === "Escape") {
        if (draft) {
          setDraft(null);
          return;
        }
      }
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        search.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [draft]);

  const rows = useMemo(() => {
    const all = state?.slots ?? [];
    const query = needle.trim().toLowerCase();
    if (!query) return all;
    return all.filter((slot) =>
      [slot.slot, slot.scope_type, slot.scope_id, slot.primary_name, slot.fallback_name]
        .filter((value): value is string => Boolean(value))
        .some((value) => value.toLowerCase().includes(query)),
    );
  }, [state, needle]);

  /** Catalogue entries with no assignment in the current scope, so the picker is never a dead end. */
  const unassigned = useMemo(() => {
    const taken = new Set((state?.slots ?? []).map((slot) => `${slot.scope_type}·${slot.slot}`));
    return (state?.catalog ?? []).filter(
      (def) => !taken.has(`environment·${def.slot}`),
    );
  }, [state]);

  const open = (draft: Draft) => {
    setResolution(null);
    setNotice(null);
    setDraft(draft);
  };

  const onResolve = async (slot: CredentialSlot) => {
    setNotice(null);
    try {
      const answer = await resolveSlot(slot.scope_type, slot.slot, slot.scope_id);
      setResolution(answer);
      setNotice(
        answer.fell_back
          ? `${slot.slot} answered from the fallback: ${answer.name} (version ${answer.version}).`
          : `${slot.slot} answers with ${answer.name} (version ${answer.version}).`,
      );
      await load();
    } catch (cause) {
      setNotice(
        cause instanceof ApiError
          ? `The resolution could not be read: ${cause.message}`
          : "The resolution could not be read.",
      );
    }
  };

  if (status === "loading") return <LoadingTable columns={5} />;

  if (status === "error" && loadError) {
    return (
      <div className="flex flex-col gap-3">
        <p
          role="alert"
          data-slots-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {loadError.message}
        </p>
        <p className="text-[11.5px] text-muted">
          Code <code className="font-mono">{loadError.code}</code>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const catalog = state?.catalog ?? [];
  const assigned = state?.assigned ?? 0;

  return (
    <div className="flex flex-col gap-4">
      <section
        data-slots-summary
        className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <Stat label="Slot names" value={catalog.length} />
        <Stat label="Assigned" value={assigned} />
        <Stat label="Unassigned" value={Math.max(catalog.length - assigned, 0)} />
        <div className="flex flex-1 items-center justify-end gap-2">
          <div className="relative">
            <Search
              className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
              aria-hidden
            />
            <input
              ref={search}
              value={needle}
              onChange={(event) => setNeedle(event.target.value)}
              placeholder="Search slots — press /"
              aria-label="Search slots"
              data-slots-search
              className="h-8 w-56 rounded-lg border border-line bg-surface pl-8 pr-2 text-[12.5px] outline-none focus:border-accent"
            />
          </div>
          <button
            type="button"
            onClick={() => void load()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
        </div>
      </section>

      {notice ? (
        <p
          role="status"
          data-slots-notice
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      {rows.length === 0 ? (
        <EmptyState
          title={
            needle
              ? "No slot matches that search"
              : "Nothing is assigned to a slot yet"
          }
          hint={
            needle
              ? "The search covers the slot name, its scope and the credentials it points at."
              : "A slot is how a workload asks for a credential without knowing its id. Assign one from the catalogue below and a consumer resolving it starts getting your choice."
          }
          action={
            needle ? null : (
              <button
                type="button"
                onClick={() =>
                  unassigned[0] &&
                  open({
                    scope_type: "environment",
                    scope_id: "production",
                    slot: unassigned[0].slot,
                    row: null,
                  })
                }
                disabled={unassigned.length === 0}
                className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel disabled:opacity-60"
              >
                <Plus className="size-3.5" aria-hidden />
                Assign {unassigned[0]?.slot ?? "a slot"}
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line bg-surface">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th className="px-4 py-2 font-medium">Slot</th>
                <th className="px-4 py-2 font-medium">Scope</th>
                <th className="px-4 py-2 font-medium">Primary</th>
                <th className="px-4 py-2 font-medium">Fallback</th>
                <th className="px-4 py-2 font-medium">Last resolved</th>
                <th className="px-4 py-2" />
              </tr>
            </thead>
            <tbody>
              {rows.map((slot) => (
                <SlotRow
                  key={slot.id}
                  slot={slot}
                  onEdit={() =>
                    open({
                      scope_type: slot.scope_type,
                      scope_id: slot.scope_id,
                      slot: slot.slot,
                      row: slot,
                    })
                  }
                  onResolve={() => void onResolve(slot)}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* The catalogue: the slots the platform names, so an operator can discover the surface. */}
      <section className="rounded-xl border border-line bg-surface p-4">
        <h2 className="text-[13.5px] font-medium">The slot catalogue</h2>
        <p className="mt-0.5 text-[12.5px] text-muted">
          Every name a consumer can ask for. Consumers resolve these, never a secret id — which is
          what makes a swap a row update instead of a release.
        </p>
        <ul data-slots-catalog className="mt-3 grid gap-2 sm:grid-cols-2">
          {catalog.map((def) => {
            const assignedRow = (state?.slots ?? []).find(
              (slot) =>
                slot.slot === def.slot && slot.scope_type === "environment",
            );
            return (
              <li
                key={def.slot}
                className="flex items-start justify-between gap-2 rounded-lg border border-line px-3 py-2"
              >
                <div className="min-w-0">
                  <p className="font-mono text-[12px]">{def.slot}</p>
                  <p className="mt-0.5 text-[11.5px] text-muted">{def.description}</p>
                  {def.consumers ? (
                    <p className="mt-0.5 text-[11px] text-muted">Consumers: {def.consumers}</p>
                  ) : null}
                </div>
                <button
                  type="button"
                  onClick={() =>
                    open({
                      scope_type: "environment",
                      scope_id: "production",
                      slot: def.slot,
                      row: assignedRow ?? null,
                    })
                  }
                  data-slots-assign={def.slot}
                  className="flex h-7 shrink-0 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] transition hover:bg-panel"
                >
                  {assignedRow ? (
                    <>
                      <ArrowRightLeft className="size-3" aria-hidden />
                      Swap
                    </>
                  ) : (
                    <>
                      <Link2 className="size-3" aria-hidden />
                      Assign
                    </>
                  )}
                </button>
              </li>
            );
          })}
        </ul>
      </section>

      {draft ? (
        <SlotEditor
          draft={draft}
          catalog={catalog}
          assignable={state?.assignable ?? []}
          onCancel={() => setDraft(null)}
          onSaved={async (message) => {
            setDraft(null);
            setResolution(null);
            setNotice(message);
            await load();
          }}
        />
      ) : null}
    </div>
  );
}

/** One number and its label in the summary strip. */
function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div>
      <p className="text-[19px] font-semibold leading-tight tabular-nums">{value}</p>
      <p className="text-[11.5px] text-muted">{label}</p>
    </div>
  );
}

/** One assignment row. */
function SlotRow({
  slot,
  onEdit,
  onResolve,
}: {
  slot: CredentialSlot;
  onEdit: () => void;
  onResolve: () => void;
}) {
  const degraded = slot.fallback_secret_id !== null && slot.primary_secret_id === null;
  return (
    <tr data-slot-row className="border-b border-line last:border-0">
      <td className="px-4 py-2.5">
        <p className="font-mono text-[12px]">{slot.slot}</p>
        {slot.description ? (
          <p className="mt-0.5 max-w-md text-[11.5px] text-muted">{slot.description}</p>
        ) : null}
      </td>
      <td className="px-4 py-2.5 text-[12.5px]">
        <span className="capitalize">{slot.scope_type}</span>
        <span className="block text-[11.5px] text-muted">{slot.scope_id}</span>
      </td>
      <td className="px-4 py-2.5 text-[12.5px]">
        {slot.primary_name ? (
          <>
            <span>{slot.primary_name}</span>
            <span className="block text-[11.5px] text-muted">
              version {slot.primary_version ?? 0}
            </span>
          </>
        ) : (
          <span className="text-muted">{slot.empty_reason}</span>
        )}
      </td>
      <td className="px-4 py-2.5 text-[12.5px]">
        {slot.fallback_name ? (
          slot.fallback_name
        ) : (
          <span className="text-muted">—</span>
        )}
      </td>
      <td className="px-4 py-2.5 text-[11.5px] text-muted">
        {slot.last_resolved_at ? (
          <>
            {formatTimestamp(slot.last_resolved_at)}
            {slot.last_resolved_by ? (
              <span className="block font-mono">{slot.last_resolved_by}</span>
            ) : null}
          </>
        ) : (
          "never"
        )}
      </td>
      <td className="px-4 py-2.5">
        <div className="flex items-center justify-end gap-1.5">
          {degraded ? (
            <span
              data-slot-degraded
              title="The primary is gone, so this slot is answering from its fallback"
              className="flex items-center gap-1 rounded-full bg-caution-soft px-2 py-0.5 text-[11px] text-caution"
            >
              <AlertTriangle className="size-3" aria-hidden />
              fallback
            </span>
          ) : null}
          <button
            type="button"
            onClick={onResolve}
            data-slot-resolve={slot.slot}
            className="flex h-7 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] transition hover:bg-panel"
          >
            <CheckCircle2 className="size-3" aria-hidden />
            Resolve
          </button>
          <button
            type="button"
            onClick={onEdit}
            data-slot-edit={slot.slot}
            className="flex h-7 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] transition hover:bg-panel"
          >
            <ArrowRightLeft className="size-3" aria-hidden />
            Swap
          </button>
        </div>
      </td>
    </tr>
  );
}

/** The editor drawer: scope, primary, fallback, and the confirmation the removal needs. */
function SlotEditor({
  draft,
  catalog,
  assignable,
  onCancel,
  onSaved,
}: {
  draft: Draft;
  catalog: SlotDef[];
  assignable: AssignableCredential[];
  onCancel: () => void;
  onSaved: (message: string) => void | Promise<void>;
}) {
  const [scopeType, setScopeType] = useState(draft.row?.scope_type ?? draft.scope_type);
  const [scopeId, setScopeId] = useState(draft.row?.scope_id ?? draft.scope_id);
  const [slot, setSlot] = useState(draft.slot);
  const [primary, setPrimary] = useState<string>(draft.row?.primary_secret_id ?? "");
  const [fallback, setFallback] = useState<string>(draft.row?.fallback_secret_id ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const heading = useRef<HTMLHeadingElement | null>(null);

  useEffect(() => {
    heading.current?.focus();
  }, []);

  const definition = catalog.find((def) => def.slot === slot);
  // The removal that needs a confirmation: a live primary with a consumer behind it, cleared.
  // The confirmation names that consumer, because "are you sure?" cannot name a workload.
  const removesLiveAssignment =
    Boolean(draft.row?.primary_secret_id) &&
    !primary &&
    Boolean(draft.row?.last_resolved_by);

  const save = async () => {
    setBusy(true);
    try {
      const saved = await assignSlot(
        scopeType,
        slot,
        scopeId.trim(),
        primary || null,
        fallback || null,
      );
      await onSaved(
        primary
          ? `${saved.slot} now answers with ${saved.primary_name} for ${scopeType} · ${scopeId}.`
          : `${saved.slot} is unassigned for ${scopeType} · ${scopeId}.`,
      );
    } catch (cause) {
      // A self-referencing fallback is about the fallback field, so it lands next to that field.
      if (cause instanceof ApiError && cause.code === "credential_slot_self_reference") {
        setFieldError(cause.message);
      } else {
        setError(
          cause instanceof ApiError
            ? `${cause.message} (code ${cause.code})`
            : "The assignment could not be saved.",
        );
      }
    } finally {
      setBusy(false);
    }
  };

  const submit = () => {
    setError(null);
    setFieldError(null);

    if (!scopeId.trim()) {
      setFieldError("A slot assignment needs the scope it belongs to.");
      return;
    }
    if (removesLiveAssignment) {
      setConfirming(true);
      return;
    }
    void save();
  };

  if (confirming && draft.row?.last_resolved_by) {
    return (
      <SlotRemovalConfirmation
        slot={draft.row}
        consumer={draft.row.last_resolved_by}
        onCancel={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false);
          void save();
        }}
      />
    );
  }

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="slot-editor-heading"
      data-slot-editor
      className="fixed inset-0 z-50 flex items-end justify-center bg-ink/40 p-0 sm:items-center sm:p-4"
      onKeyDown={(event) => {
        if (event.key === "Escape") onCancel();
      }}
    >
      <div className="flex max-h-[90vh] w-full max-w-xl flex-col gap-3 overflow-y-auto rounded-t-2xl border border-line bg-surface p-4 sm:rounded-2xl">
        <h2
          id="slot-editor-heading"
          ref={heading}
          tabIndex={-1}
          className="text-[14px] font-medium outline-none"
        >
          {draft.row ? "Swap what this slot answers with" : "Assign a slot"}
        </h2>
        {definition ? (
          <p className="text-[12.5px] text-muted">
            {definition.description}
            {definition.consumers ? ` Consumers: ${definition.consumers}.` : ""}
          </p>
        ) : null}

        <div className="grid gap-3 sm:grid-cols-2">
          <label className="flex flex-col gap-1 text-[12px]">
            <span className="font-medium">Scope</span>
            <select
              value={scopeType}
              onChange={(event) => setScopeType(event.target.value)}
              data-slot-scope
              disabled={Boolean(draft.row)}
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[12.5px] disabled:opacity-60"
            >
              {SCOPES.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label} — {option.hint}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1 text-[12px]">
            <span className="font-medium">Scope id</span>
            <input
              value={scopeId}
              onChange={(event) => setScopeId(event.target.value)}
              disabled={Boolean(draft.row)}
              data-slot-scope-id
              placeholder="production"
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[12.5px] outline-none focus:border-accent disabled:opacity-60"
            />
          </label>

          <label className="flex flex-col gap-1 text-[12px] sm:col-span-2">
            <span className="font-medium">Slot</span>
            <select
              value={slot}
              onChange={(event) => setSlot(event.target.value)}
              data-slot-name
              disabled={Boolean(draft.row)}
              className="h-9 rounded-lg border border-line bg-surface px-2 font-mono text-[12.5px] disabled:opacity-60"
            >
              {(catalog.length > 0 ? catalog : [{ slot: draft.slot }]).map((def) => (
                <option key={def.slot} value={def.slot}>
                  {def.slot}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1 text-[12px]">
            <span className="font-medium">Primary</span>
            <select
              value={primary}
              onChange={(event) => setPrimary(event.target.value)}
              data-slot-primary
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[12.5px]"
            >
              <option value="">— none —</option>
              {assignable.map((candidate) => (
                <option key={candidate.id} value={candidate.id}>
                  {candidate.name} ({candidate.read_only ? `${candidate.provider ?? "external"} · read-only` : candidate.kind})
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1 text-[12px]">
            <span className="font-medium">Fallback</span>
            <select
              value={fallback}
              onChange={(event) => {
                setFallback(event.target.value);
                setFieldError(null);
              }}
              data-slot-fallback
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[12.5px]"
            >
              <option value="">— none —</option>
              {assignable
                .filter((candidate) => candidate.id !== primary)
                .map((candidate) => (
                  <option key={candidate.id} value={candidate.id}>
                    {candidate.name} ({candidate.read_only ? `${candidate.provider ?? "external"} · read-only` : candidate.kind})
                  </option>
                ))}
            </select>
            <span className="text-[11.5px] text-muted">
              The fallback answers when the primary cannot be read. It must be a different
              credential — the API refuses the same one with a conflict.
            </span>
          </label>
        </div>

        {fieldError ? (
          <p
            role="alert"
            data-slot-field-error
            className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
          >
            {fieldError}
          </p>
        ) : null}
        {error ? (
          <p
            role="alert"
            data-slot-editor-error
            className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
          >
            {error}
          </p>
        ) : null}

        {draft.row?.last_resolved_by ? (
          <p data-slot-consumer className="text-[11.5px] text-muted">
            {draft.row.last_resolved_by} last resolved this slot
            {draft.row.last_resolved_at
              ? ` on ${formatTimestamp(draft.row.last_resolved_at)}`
              : ""}
            .
          </p>
        ) : null}

        <div className="flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="h-9 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy}
            data-slot-save
            className="h-9 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition disabled:opacity-60"
          >
            {busy ? "Saving…" : "Save assignment"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** The confirmation a removal from a live consumer needs, shown instead of the editor. */
export function SlotRemovalConfirmation({
  slot,
  consumer,
  onCancel,
  onConfirm,
}: {
  slot: CredentialSlot;
  consumer: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="slot-removal-heading"
      data-slot-removal-confirm
      className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4"
    >
      <div className="flex w-full max-w-md flex-col gap-3 rounded-2xl border border-line bg-surface p-4">
        <div className="flex items-start gap-2">
          <Unlink className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
          <h2 id="slot-removal-heading" className="text-[14px] font-medium">
            {consumer} is resolving this slot
          </h2>
        </div>
        <p className="text-[12.5px] text-muted">
          Clearing the primary for {slot.slot} in {slot.scope_type} · {slot.scope_id} leaves{" "}
          {consumer} with
          {slot.fallback_name ? (
            <> the fallback ({slot.fallback_name}).</>
          ) : (
            <> no credential at all until you assign one.</>
          )}
        </p>
        <div className="flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="h-9 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            Keep it
          </button>
          <button
            type="button"
            onClick={onConfirm}
            data-slot-removal-accept
            className="h-9 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white"
          >
            Clear the assignment
          </button>
        </div>
      </div>
    </div>
  );
}

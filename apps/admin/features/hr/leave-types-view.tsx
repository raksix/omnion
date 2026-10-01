"use client";

/**
 * `/hr/leave/types` — the leave type catalogue (REQ-055, slice 2b).
 *
 * A leave type is the *rule* every request is measured against: the yearly entitlement, whether a
 * request needs a decision, whether the balance may go negative. Getting these wrong is not a
 * cosmetic error — flipping `requires_approval` off approves every future request on arrival, so
 * this screen says what each switch does rather than presenting four bare checkboxes.
 *
 * The screen refuses to hide the consequence:
 *
 * - **`allow_negative`** reads as "a person may take unpaid days beyond the entitlement". That is
 *   what it means, and the label says so, because "negative balance" alone is a number an
 *   administrator enables without knowing what happens next.
 * - **`requires_approval` off** means the request is decided on creation — and the module still
 *   emits `hr.leave.approved`, because an automation waiting on that event must not silently stop
 *   in exactly the organization that switched approvals off.
 * - **Deactivating is not deleting.** A type with history stays in the list with its requests
 *   intact; it disappears from the request form's select and keeps its entitlement on the balance
 *   card, because an old request's type still has to resolve to something with a name.
 */
import { useCallback, useEffect, useState } from "react";
import { Loader2, Plus, Save } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, ErrorStrip, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  createLeaveType,
  fetchLeaveTypes,
  updateLeaveType,
  type LeaveType,
} from "@/lib/hr";

type Draft = {
  name: string;
  code: string;
  annual_days: string;
  paid: boolean;
  requires_approval: boolean;
  allow_negative: boolean;
  active: boolean;
};

/** A blank row for the create form. */
const EMPTY_DRAFT: Draft = {
  name: "",
  code: "",
  annual_days: "0",
  paid: true,
  requires_approval: true,
  allow_negative: false,
  active: true,
};

export function LeaveTypesView() {
  const [types, setTypes] = useState<LeaveType[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const [draft, setDraft] = useState<Draft | null>(null);
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<ScreenErrorValue>(null);

  /** The row being edited, by id. */
  const [editing, setEditing] = useState<string | null>(null);
  const [editDraft, setEditDraft] = useState<Draft | null>(null);
  const [savingId, setSavingId] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchLeaveTypes();
      setTypes(page.items);
    } catch (failure) {
      setError(toScreenError(failure, "The leave types could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const create = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!draft || draft.name.trim() === "" || draft.code.trim() === "") {
      setCreateError("A type needs a name and a code.");
      return;
    }
    setCreating(true);
    setCreateError(null);
    try {
      await createLeaveType({
        name: draft.name.trim(),
        code: draft.code.trim().toLowerCase(),
        annual_days: draft.annual_days,
        paid: draft.paid,
        requires_approval: draft.requires_approval,
        allow_negative: draft.allow_negative,
        active: draft.active,
      });
      setDraft(null);
      await load();
    } catch (failure) {
      setCreateError(toScreenError(failure, "The leave type could not be created."));
    } finally {
      setCreating(false);
    }
  };

  const save = async (type: LeaveType) => {
    if (!editDraft) {
      return;
    }
    setSavingId(type.id);
    setSaveError(null);
    try {
      await updateLeaveType(type.id, {
        name: editDraft.name.trim(),
        code: editDraft.code.trim().toLowerCase(),
        annual_days: editDraft.annual_days,
        paid: editDraft.paid,
        requires_approval: editDraft.requires_approval,
        allow_negative: editDraft.allow_negative,
        active: editDraft.active,
      });
      setEditing(null);
      setEditDraft(null);
      await load();
    } catch (failure) {
      setSaveError(toScreenError(failure, "The leave type could not be saved."));
    } finally {
      setSavingId(null);
    }
  };

  return (
    <div className="space-y-6">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Leave types</h1>
        <p className="text-sm text-muted">
          The rules every request is measured against: entitlement, approval and negative balances.
        </p>
      </header>

      {saveError ? <ErrorStrip error={saveError} onRetry={() => setSaveError(null)} qa="hr-types-error" /> : null}

      {loading ? (
        <LoadingTable columns={5} />
      ) : error ? (
        <ErrorState error={error} onRetry={() => void load()} qa="hr-types-load-error" />
      ) : types.length === 0 && !draft ? (
        <div className="rounded-lg border border-border">
          <EmptyState
            title="No leave type yet"
            hint="Every organization needs at least one type — an annual leave, a sick leave or an unpaid one — before anybody can request anything."
            action={
              <button
                type="button"
                onClick={() => setDraft(EMPTY_DRAFT)}
                data-qa-hr-types-add
                className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm text-primary-foreground"
              >
                <Plus className="h-4 w-4" aria-hidden />
                Add the first type
              </button>
            }
          />
        </div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[12px] text-muted">
                <th scope="col" className="px-4 py-2 font-medium">Name</th>
                <th scope="col" className="px-4 py-2 font-medium">Code</th>
                <th scope="col" className="px-4 py-2 font-medium">Days / year</th>
                <th scope="col" className="px-4 py-2 font-medium">Paid</th>
                <th scope="col" className="px-4 py-2 font-medium">Approval</th>
                <th scope="col" className="px-4 py-2 font-medium">Negative</th>
                <th scope="col" className="px-4 py-2 font-medium">State</th>
                <th scope="col" className="px-4 py-2 font-medium"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {types.map((type) =>
                editing === type.id && editDraft ? (
                  <tr key={type.id} className="border-t border-line bg-quiet-soft/30" data-qa-hr-type-edit-row>
                    <td className="px-3 py-2">
                      <input
                        aria-label="Name"
                        value={editDraft.name}
                        onChange={(event) => setEditDraft({ ...editDraft, name: event.target.value })}
                        data-qa-hr-type-edit-name
                        className="h-8 w-full rounded border border-border bg-transparent px-2 text-[13px]"
                      />
                    </td>
                    <td className="px-3 py-2">
                      <input
                        aria-label="Code"
                        value={editDraft.code}
                        onChange={(event) => setEditDraft({ ...editDraft, code: event.target.value })}
                        data-qa-hr-type-edit-code
                        className="h-8 w-28 rounded border border-border bg-transparent px-2 text-[13px]"
                      />
                    </td>
                    <td className="px-3 py-2">
                      <input
                        aria-label="Days per year"
                        value={editDraft.annual_days}
                        onChange={(event) => setEditDraft({ ...editDraft, annual_days: event.target.value })}
                        data-qa-hr-type-edit-days
                        className="h-8 w-20 rounded border border-border bg-transparent px-2 text-[13px]"
                      />
                    </td>
                    <td className="px-3 py-2 text-center">
                      <input
                        type="checkbox"
                        aria-label="Paid"
                        checked={editDraft.paid}
                        onChange={(event) => setEditDraft({ ...editDraft, paid: event.target.checked })}
                        data-qa-hr-type-edit-paid
                      />
                    </td>
                    <td className="px-3 py-2 text-center">
                      <input
                        type="checkbox"
                        aria-label="Requires approval"
                        checked={editDraft.requires_approval}
                        onChange={(event) => setEditDraft({ ...editDraft, requires_approval: event.target.checked })}
                        data-qa-hr-type-edit-approval
                      />
                    </td>
                    <td className="px-3 py-2 text-center">
                      <input
                        type="checkbox"
                        aria-label="Allow negative"
                        checked={editDraft.allow_negative}
                        onChange={(event) => setEditDraft({ ...editDraft, allow_negative: event.target.checked })}
                        data-qa-hr-type-edit-negative
                      />
                    </td>
                    <td className="px-3 py-2 text-center">
                      <input
                        type="checkbox"
                        aria-label="Active"
                        checked={editDraft.active}
                        onChange={(event) => setEditDraft({ ...editDraft, active: event.target.checked })}
                        data-qa-hr-type-edit-active
                      />
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex items-center gap-1.5">
                        <button
                          type="button"
                          onClick={() => void save(type)}
                          disabled={savingId !== null}
                          data-qa-hr-type-edit-save
                          className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-2.5 text-[13px] text-primary-foreground disabled:opacity-60"
                        >
                          {savingId === type.id ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                          ) : (
                            <Save className="h-3.5 w-3.5" aria-hidden />
                          )}
                          Save
                        </button>
                        <button
                          type="button"
                          onClick={() => {
                            setEditing(null);
                            setEditDraft(null);
                          }}
                          data-qa-hr-type-edit-cancel
                          className="h-8 rounded-md border border-border px-2.5 text-[13px]"
                        >
                          Cancel
                        </button>
                      </div>
                    </td>
                  </tr>
                ) : (
                  <tr key={type.id} className="border-t border-line" data-qa-hr-type-row={type.id}>
                    <td className="px-4 py-2.5 font-medium">{type.name}</td>
                    <td className="px-4 py-2.5 text-muted">{type.code}</td>
                    <td className="px-4 py-2.5">{type.annual_days}</td>
                    <td className="px-4 py-2.5">{type.paid ? "Yes" : "No"}</td>
                    <td className="px-4 py-2.5">{type.requires_approval ? "Required" : "On creation"}</td>
                    <td className="px-4 py-2.5">{type.allow_negative ? "Allowed" : "Refused"}</td>
                    <td className="px-4 py-2.5">{type.active ? "Active" : "Inactive"}</td>
                    <td className="px-4 py-2.5">
                      <button
                        type="button"
                        onClick={() => {
                          setEditing(type.id);
                          setEditDraft({
                            name: type.name,
                            code: type.code,
                            annual_days: type.annual_days,
                            paid: type.paid,
                            requires_approval: type.requires_approval,
                            allow_negative: type.allow_negative,
                            active: type.active,
                          });
                        }}
                        data-qa-hr-type-edit={type.id}
                        className="h-8 rounded-md border border-border px-2.5 text-[13px]"
                      >
                        Edit
                      </button>
                    </td>
                  </tr>
                ),
              )}
              {draft ? (
                <tr className="border-t border-line bg-quiet-soft/30" data-qa-hr-type-create-row>
                  <td className="px-3 py-2">
                    <input
                      aria-label="Name"
                      placeholder="Annual leave"
                      value={draft.name}
                      onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                      data-qa-hr-type-new-name
                      className="h-8 w-full rounded border border-border bg-transparent px-2 text-[13px]"
                    />
                  </td>
                  <td className="px-3 py-2">
                    <input
                      aria-label="Code"
                      placeholder="annual"
                      value={draft.code}
                      onChange={(event) => setDraft({ ...draft, code: event.target.value })}
                      data-qa-hr-type-new-code
                      className="h-8 w-28 rounded border border-border bg-transparent px-2 text-[13px]"
                    />
                  </td>
                  <td className="px-3 py-2">
                    <input
                      aria-label="Days per year"
                      value={draft.annual_days}
                      onChange={(event) => setDraft({ ...draft, annual_days: event.target.value })}
                      data-qa-hr-type-new-days
                      className="h-8 w-20 rounded border border-border bg-transparent px-2 text-[13px]"
                    />
                  </td>
                  <td className="px-3 py-2 text-center">
                    <input
                      type="checkbox"
                      aria-label="Paid"
                      checked={draft.paid}
                      onChange={(event) => setDraft({ ...draft, paid: event.target.checked })}
                    />
                  </td>
                  <td className="px-3 py-2 text-center">
                    <input
                      type="checkbox"
                      aria-label="Requires approval"
                      checked={draft.requires_approval}
                      onChange={(event) => setDraft({ ...draft, requires_approval: event.target.checked })}
                    />
                  </td>
                  <td className="px-3 py-2 text-center">
                    <input
                      type="checkbox"
                      aria-label="Allow negative"
                      checked={draft.allow_negative}
                      onChange={(event) => setDraft({ ...draft, allow_negative: event.target.checked })}
                    />
                  </td>
                  <td className="px-3 py-2 text-center">
                    <input
                      type="checkbox"
                      aria-label="Active"
                      checked={draft.active}
                      onChange={(event) => setDraft({ ...draft, active: event.target.checked })}
                    />
                  </td>
                  <td className="px-3 py-2">
                    <div className="flex items-center gap-1.5">
                      <button
                        type="submit"
                        form="hr-type-create"
                        disabled={creating}
                        data-qa-hr-type-new-save
                        className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-2.5 text-[13px] text-primary-foreground disabled:opacity-60"
                      >
                        {creating ? (
                          <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                        ) : (
                          <Save className="h-3.5 w-3.5" aria-hidden />
                        )}
                        Create
                      </button>
                      <button
                        type="button"
                        onClick={() => {
                          setDraft(null);
                          setCreateError(null);
                        }}
                        className="h-8 rounded-md border border-border px-2.5 text-[13px]"
                      >
                        Cancel
                      </button>
                    </div>
                  </td>
                </tr>
              ) : null}
            </tbody>
          </table>
          {createError ? <div className="border-t border-line p-3"><ErrorStrip error={createError} onRetry={() => setCreateError(null)} qa="hr-type-create-error" /></div> : null}
        </div>
      )}

      {/* The create form lives outside the table so its submit is a real form submit. */}
      {draft ? (
        <form id="hr-type-create" onSubmit={create} className="hidden" aria-hidden="true">
          <button type="submit">Create</button>
        </form>
      ) : null}

      {types.length > 0 && !draft ? (
        <button
          type="button"
          onClick={() => setDraft(EMPTY_DRAFT)}
          data-qa-hr-types-add
          className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Add a type
        </button>
      ) : null}

      <section className="space-y-1 rounded-lg border border-border p-4 text-[12.5px] text-muted">
        <p className="text-[13px] font-medium text-foreground">What the switches do</p>
        <p>
          <strong>Approval off</strong> approves the request the moment it is raised — and still emits{" "}
          <code>hr.leave.approved</code>, so an automation waiting on that event keeps running.
        </p>
        <p>
          <strong>Negative allowed</strong> lets a person take more days than the entitlement; the
          balance then reads below zero on purpose instead of being refused.
        </p>
        <p>
          <strong>Inactive</strong> keeps the type and its history but drops it from the request
          form. It is not a delete.
        </p>
      </section>
    </div>
  );
}

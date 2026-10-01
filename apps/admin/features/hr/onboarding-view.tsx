"use client";

/**
 * The onboarding board (REQ-055, slice 4b) — `/hr/onboarding`.
 *
 * One card per person, each with their progress bar, and a click opens the checklist. It is the
 * last screen slice 4 was missing: the API, the module, the migration and nine walks all existed
 * with no route a person could open, which is the gap slice 1 had and the reason this file's own
 * header has to say so.
 *
 * Four decisions the screen had to make:
 *
 * 1. **The header numbers are the server's.** `totals.people / in_progress / finished` are computed
 *    over the whole board in the route, and a board that sums its own cards gets it wrong the
 *    moment the board is filtered. The one number the browser derives is each card's bar, and even
 *    that goes through `checklistProgress` — never `done / total` written inline, because that is
 *    a second definition of "how far along is this person".
 * 2. **A card with no items is 0%, not 100%.** Somebody nobody has given a template to has done
 *    nothing, which is not the same as being finished, and a board that filed them under "done"
 *    would answer "who is behind on onboarding?" with the people who never started.
 * 3. **Ticking is a checkbox with the note beside it, and the tick is optimistic-then-confirmed.**
 *    The completion event fires on the transition, so the screen re-reads the checklist after every
 *    tick rather than assuming its own arithmetic agreed.
 * 4. **Applying a template is refused loudly.** The route answers 409 with the item count when the
 *    employee already has a checklist, and the screen shows that refusal rather than retrying —
 *    applying twice duplicates every step, and a checklist that says "sign the contract" twice is
 *    one nobody can finish honestly.
 */
import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { CheckCircle2, ClipboardList, Loader2 } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ErrorState,
  toScreenError,
  type ScreenErrorValue,
} from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { HrModuleNav } from "@/features/hr/module-nav";

import {
  checklistProgress,
  fetchOnboardingBoard,
  fetchOnboardingTemplates,
  type Checklist,
  type OnboardingBoard,
  type OnboardingTemplate,
} from "@/lib/hr";

/** One tile's worth of a headline number. */
function Tile({
  label,
  value,
  tone = "",
  qa,
}: {
  label: string;
  value: number | string;
  tone?: string;
  qa?: string;
}) {
  return (
    <div className="rounded-lg border border-line px-3 py-2">
      <p className="text-[11px] uppercase tracking-wide text-muted">{label}</p>
      <p className={`text-[19px] font-semibold tabular-nums ${tone}`} data-qa-hr-onboarding-total={qa ?? label}>
        {value}
      </p>
    </div>
  );
}

/**
 * The progress bar.
 *
 * The **number** is always rendered beside the bar, and the fill's colour is not the only signal —
 * a finished card reads "4 / 4" to somebody who cannot see the green. A bar that carries only a
 * colour is unreadable in print and to a colour-blind operator.
 */
function ProgressBar({ checklist }: { checklist: Checklist }) {
  const fraction = checklistProgress(checklist);
  const finished = checklist.total > 0 && checklist.done === checklist.total;
  const percent = Math.round(fraction * 100);
  return (
    <div className="flex items-center gap-2">
      <div
        className="h-1.5 w-full overflow-hidden rounded-full bg-quiet-soft"
        role="progressbar"
        aria-valuenow={checklist.done}
        aria-valuemin={0}
        aria-valuemax={checklist.total}
        aria-label={`${checklist.employee_name}'s onboarding`}
      >
        <div
          className={finished ? "h-full bg-emerald-500" : "h-full bg-foreground/70"}
          style={{ width: `${percent}%` }}
        />
      </div>
      <span className="shrink-0 text-[11.5px] tabular-nums text-muted" data-qa-hr-onboarding-progress={checklist.employee_id}>
        {checklist.done} / {checklist.total}
      </span>
    </div>
  );
}

export function OnboardingView() {
  const router = useRouter();

  const [board, setBoard] = useState<OnboardingBoard | null>(null);
  const [templates, setTemplates] = useState<OnboardingTemplate[]>([]);
  const [error, setError] = useState<ScreenErrorValue | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [applying, setApplying] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const [loaded, served] = await Promise.all([fetchOnboardingBoard(), fetchOnboardingTemplates()]);
      setBoard(loaded);
      setTemplates(served.items.filter((template) => template.active));
    } catch (cause) {
      setError(toScreenError(cause, "The onboarding board could not be read."));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const open = board?.items.find((checklist) => checklist.employee_id === openId) ?? null;

  const apply = async (employeeId: string, templateId: string) => {
    setApplying(employeeId);
    setActionError(null);
    try {
      const { applyOnboardingTemplate } = await import("@/lib/hr");
      await applyOnboardingTemplate(employeeId, templateId);
      await load();
    } catch (cause) {
      // The 409 carries the item count, so the refusal is shown rather than swallowed: applying
      // twice would duplicate every step on somebody's checklist.
      setActionError(
        cause instanceof Error ? cause.message : "The template could not be applied.",
      );
    } finally {
      setApplying(null);
    }
  };

  return (
    <div className="space-y-4" data-qa-hr-onboarding>
      <HrModuleNav />

      <header>
        <h1 className="text-[17px] font-semibold">Onboarding</h1>
        <p className="text-[12.5px] text-muted">
          Who is working through their first weeks, and how far along each of them is.
        </p>
      </header>

      {board ? (
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <Tile label="People" value={board.totals.people} />
          <Tile label="In progress" value={board.totals.in_progress} />
          <Tile label="Finished" value={board.totals.finished} tone="text-emerald-600" />
          <Tile label="Steps ticked" value={`${board.totals.items_done} / ${board.totals.items_total}`} />
        </div>
      ) : null}

      {actionError ? (
        <div role="alert" data-qa-hr-onboarding-error className="rounded-lg border border-red-300 bg-red-50/60 px-3 py-2 text-[12.5px] text-red-700">
          {actionError}
        </div>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} />
      ) : board === null ? (
        <LoadingTable columns={3} />
      ) : board.items.length === 0 ? (
        <div className="rounded-lg border border-line">
          <EmptyState
            title="Nobody is onboarding yet"
            hint="Apply a template to an employee from the button on their card and their checklist appears here."
          />
        </div>
      ) : (
        <div className="grid gap-2 sm:grid-cols-2">
          {board.items.map((checklist) => (
            <article key={checklist.employee_id} className="rounded-lg border border-line px-3 py-3" data-qa-hr-onboarding-card={checklist.employee_id}>
              <div className="flex items-start justify-between gap-2">
                <div>
                  <p className="text-[13.5px] font-medium">{checklist.employee_name}</p>
                  <p className="text-[11.5px] text-muted">
                    {checklist.template_name ?? "No template"}
                    {checklist.department_name ? ` · ${checklist.department_name}` : ""}
                  </p>
                </div>
                {checklist.total > 0 && checklist.done === checklist.total ? (
                  <CheckCircle2 className="h-4 w-4 shrink-0 text-emerald-600" aria-label="Finished" />
                ) : null}
              </div>

              <div className="mt-2">
                <ProgressBar checklist={checklist} />
              </div>

              <div className="mt-2.5 flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  onClick={() => setOpenId(checklist.employee_id)}
                  data-qa-hr-onboarding-open={checklist.employee_id}
                  className="inline-flex h-7 items-center gap-1.5 rounded-md border border-line px-2 text-[11.5px] hover:bg-quiet-soft"
                >
                  <ClipboardList className="h-3.5 w-3.5" aria-hidden />
                  Checklist
                </button>

                {checklist.total === 0 && templates.length > 0 ? (
                  <label className="inline-flex items-center gap-1.5 text-[11.5px] text-muted">
                    <span className="sr-only">Apply a template to {checklist.employee_name}</span>
                    <select
                      defaultValue=""
                      data-qa-hr-onboarding-apply={checklist.employee_id}
                      disabled={applying === checklist.employee_id}
                      onChange={(event) => {
                        if (event.target.value) {
                          void apply(checklist.employee_id, event.target.value);
                        }
                      }}
                      className="h-7 rounded-md border border-line bg-background px-1.5 text-[11.5px] disabled:opacity-50"
                    >
                      <option value="">Apply a template…</option>
                      {templates.map((template) => (
                        <option key={template.id} value={template.id}>
                          {template.name} ({template.items.length})
                        </option>
                      ))}
                    </select>
                    {applying === checklist.employee_id ? (
                      <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                    ) : null}
                  </label>
                ) : null}
              </div>
            </article>
          ))}
        </div>
      )}

      {openId ? (
        <ChecklistDrawer
          checklist={open}
          onClose={() => setOpenId(null)}
          onChanged={load}
          onOpenEmployee={() => router.push("/hr/employees")}
        />
      ) : null}
    </div>
  );
}

/**
 * One person's checklist.
 *
 * Rendered inside the board rather than as its own route, because "open a checklist" is a step
 * within looking at the board and a route would lose the board behind it. It re-reads the
 * checklist after every tick rather than adjusting its own count: the completion event fires on the
 * **transition** to all-ticked, so a screen that assumes its arithmetic agreed with the server
 * will announce somebody finished when they did not.
 *
 * The due date is rendered from the wire string (`YYYY-MM-DD`), never from `new Date(...)`: the
 * module's `dates::option` exists because `time`'s own `serde` writes a year and an ordinal day,
 * and a browser parsing either form differently is how a checklist says the contract is due on
 * the 61st of March.
 */
function ChecklistDrawer({
  checklist,
  onClose,
  onChanged,
}: {
  checklist: Checklist | null;
  onClose: () => void;
  onChanged: () => Promise<void>;
  onOpenEmployee: () => void;
}) {
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setNote("");
    setError(null);
  }, [checklist?.employee_id]);

  // Escape closes, and the panel takes the focus so a keyboard user is not left behind it.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  if (!checklist) {
    return null;
  }

  const toggle = async (itemId: string, done: boolean) => {
    setBusy(itemId);
    setError(null);
    try {
      const { tickChecklistItem } = await import("@/lib/hr");
      await tickChecklistItem(itemId, { done, note: note || undefined });
      setNote("");
      await onChanged();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "That step could not be ticked.");
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex justify-end" role="dialog" aria-modal="true" aria-label={`${checklist.employee_name}'s onboarding checklist`}>
      <button
        type="button"
        aria-label="Close the checklist"
        onClick={onClose}
        className="absolute inset-0 bg-black/30"
      />
      <div className="relative flex h-full w-full max-w-md flex-col border-l border-line bg-background shadow-xl">
        <header className="flex items-start justify-between gap-2 border-b border-line px-4 py-3">
          <div>
            <h2 className="text-[15px] font-semibold">{checklist.employee_name}</h2>
            <p className="text-[11.5px] text-muted">
              {checklist.template_name ?? "No template"} · {checklist.done} of {checklist.total} done
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            data-qa-hr-onboarding-close
            className="inline-flex h-7 items-center rounded-md border border-line px-2 text-[11.5px] hover:bg-quiet-soft"
          >
            Close
          </button>
        </header>

        <div className="flex-1 overflow-y-auto px-4 py-3">
          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span>Note for the next tick (optional)</span>
            <input
              value={note}
              onChange={(event) => setNote(event.target.value)}
              placeholder="e.g. contract countersigned"
              data-qa-hr-onboarding-note
              className="h-8 rounded-md border border-line bg-background px-2 text-[12.5px] text-foreground"
            />
          </label>

          {error ? (
            <div role="alert" data-qa-hr-onboarding-tick-error className="mt-2 rounded-lg border border-red-300 bg-red-50/60 px-3 py-2 text-[12.5px] text-red-700">
              {error}
            </div>
          ) : null}

          {checklist.items.length === 0 ? (
            <div className="mt-3 rounded-lg border border-line">
              <EmptyState
                title="This checklist is empty"
                hint="Apply a template from the card to give them something to work through."
              />
            </div>
          ) : (
            <ul className="mt-3 space-y-1">
              {checklist.items.map((item) => (
                <li key={item.id} className="rounded-md border border-line px-2.5 py-2" data-qa-hr-onboarding-item={item.id}>
                  <label className="flex items-start gap-2 text-[12.5px]">
                    <input
                      type="checkbox"
                      checked={item.done_at !== null}
                      disabled={busy === item.id}
                      onChange={(event) => void toggle(item.id, event.target.checked)}
                      data-qa-hr-onboarding-tick={item.id}
                      className="mt-0.5"
                    />
                    <span className="min-w-0">
                      <span className={item.done_at ? "text-muted line-through" : ""}>{item.title}</span>
                      <span className="mt-0.5 block text-[11px] text-muted">
                        {item.owner_role ? `${item.owner_role} · ` : ""}
                        {item.due_on
                          ? `due ${item.due_on}`
                          : item.due_offset_days === 0
                            ? "due on day one"
                            : "no deadline"}
                        {item.requires_file ? " · needs a file" : ""}
                      </span>
                      {item.note ? (
                        <span className="mt-0.5 block text-[11px] text-muted italic">{item.note}</span>
                      ) : null}
                    </span>
                  </label>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}

"use client";

/**
 * `/automations` — the rules of one organization (docs/requests/REQ-003, slice 1).
 *
 * A rule is trigger → condition → action, and this screen is the read/write surface of that
 * machine: the list with the filters the plan names, the editor for one rule, and the two
 * test-fire controls (a dry run against a hand-written payload, and a one-shot listener for
 * the next real event).
 *
 * Two things this screen is careful about, because the request calls them out:
 *
 * * **A hook token is shown once.** Reading a rule reports that a URL exists and what it
 *   costs to call, never the URL itself; `Rotate` is the only control that mints one, and it
 *   says so before it does.
 * * **A dry run never sends anything.** The report is rendered with `would_send` on every
 *   row, which is what makes it safe to press on a rule that is already armed.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Copy,
  FlaskConical,
  Plus,
  Radio,
  RefreshCw,
  Trash2,
  Webhook,
  X,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { useSession } from "@/lib/session";
import {
  ApiError,
  createAutomation,
  deleteAutomation,
  fetchAutomation,
  fetchAutomationCatalogue,
  fetchAutomationTests,
  fetchAutomations,
  listenAutomation,
  rotateAutomationHook,
  testAutomation,
  updateAutomation,
  type Automation,
  type AutomationCatalogue,
  type AutomationCondition,
  type AutomationDryRun,
  type AutomationGroup,
  type AutomationNode,
  type AutomationStep,
  type AutomationTestEvent,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The rows the list shows, and what each one is. */
const COLUMNS = ["Name", "Trigger", "Conditions", "Actions", "Status", "Runs", "Last fired"];

/** The payload a fresh rule starts with, so "Test event" has something to send. */
const SAMPLE_PAYLOAD: Record<string, unknown> = {
  status: "published",
  slug: "home",
  title: "Release notes",
  revision_no: 1,
};

/** `true` when a node is a nested group rather than a comparison. */
function isGroup(node: AutomationNode): node is AutomationGroup {
  return "all" in node || "any" in node;
}

/** The members of a group, whichever way it is spelled. */
function members(group: AutomationGroup): AutomationNode[] {
  if (Array.isArray(group.nodes) && group.nodes.length > 0) {
    return group.nodes;
  }
  if (group.all) {
    return group.all;
  }
  return group.any ?? [];
}

/** The mode a group is in, defaulting to `all` — what a flat list has always meant. */
function modeOf(group: AutomationGroup): "all" | "any" {
  return group.any ? "any" : "all";
}

/** The tree as the editor edits it: always an explicit `all` root. */
function toEditable(conditions: Automation["conditions"]): AutomationGroup {
  if (Array.isArray(conditions)) {
    return { mode: "all", all: conditions };
  }
  if (conditions.any) {
    return { mode: "any", any: conditions.any };
  }
  return { mode: "all", all: conditions.all ?? [] };
}

/** The editing shape back into the wire shape, so the API sees exactly one shape. */
function toWire(group: AutomationGroup): AutomationGroup {
  const list = members(group);
  return group.mode === "any"
    ? { any: list }
    : { all: list };
}

/** How many comparisons a tree carries. */
function countNodes(nodes: AutomationNode[]): number {
  return nodes.reduce(
    (total, node) => total + (isGroup(node) ? countNodes(members(node)) : 1),
    0,
  );
}

/** The one-line summary of a tree the list shows. */
function describeConditions(conditions: Automation["conditions"], count: number): string {
  if (count === 0) {
    return "Every event";
  }
  const group = Array.isArray(conditions) ? { all: conditions } : conditions;
  const label = modeOf(group) === "any" ? "Any of" : "All of";
  return `${label} ${count} condition${count === 1 ? "" : "s"}`;
}

/** The event name a rule listens for, in the panel's words. */
function triggerLabel(automation: Automation): string {
  return automation.trigger === "inbound_webhook" ? "Inbound webhook" : automation.event;
}

/** The step list a rule carries, typed for the editor. */
function stepsOf(automation: Automation): AutomationStep[] {
  return (automation.actions as unknown as AutomationStep[]) ?? [];
}

/** The fields a condition or binding may read for one event. */
function fieldsFor(catalogue: AutomationCatalogue | null, event: string) {
  return catalogue?.events.find((entry) => entry.name === event)?.fields ?? [];
}

// ---------------------------------------------------------------------------------------------
// The editor's own state
// ---------------------------------------------------------------------------------------------

/** What the editor holds while a rule is open. */
type Draft = {
  id: string | null;
  name: string;
  description: string;
  enabled: boolean;
  event: string;
  hookTriggered: boolean;
  conditions: AutomationGroup;
  steps: AutomationStep[];
};

const EMPTY_DRAFT: Draft = {
  id: null,
  name: "",
  description: "",
  enabled: true,
  event: "page.published",
  hookTriggered: false,
  conditions: { mode: "all", all: [] },
  steps: [
    {
      name: "tell the editor",
      kind: "task",
      action: "send_email",
      params: {
        to: "editor@example.com",
        subject: "Published: {{event.title}}",
        body: "{{event.slug}} is live.",
      },
      max_attempts: 1,
    },
  ],
};

/** The automations list with the editor beside it. */
export function AutomationsView() {
  const { user } = useSession();
  const [catalogue, setCatalogue] = useState<AutomationCatalogue | null>(null);
  const [automations, setAutomations] = useState<Automation[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  // The event filter the list applies, and the free-text search the header owns.
  const [eventFilter, setEventFilter] = useState("");
  const [query, setQuery] = useState("");
  const [onlyEnabled, setOnlyEnabled] = useState<"all" | "armed" | "paused">("all");

  // The open editor and everything that hangs off it.
  const [draft, setDraft] = useState<Draft | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  // The test-fire surface of the open rule.
  const [report, setReport] = useState<AutomationDryRun | null>(null);
  const [payloadText, setPayloadText] = useState(JSON.stringify(SAMPLE_PAYLOAD, null, 2));
  const [listeners, setListeners] = useState<AutomationTestEvent[]>([]);
  const [hookUrl, setHookUrl] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);
  const organizationId = user?.organization_id ?? null;

  useEffect(() => {
    fetchAutomationCatalogue()
      .then(setCatalogue)
      .catch(() => setCatalogue(null));
  }, []);

  useEffect(() => {
    if (!user) {
      return;
    }
    let cancelled = false;
    setAutomations(null);
    setLoadError(null);
    fetchAutomations(organizationId ?? undefined)
      .then((answer) => {
        if (!cancelled) {
          setAutomations(answer.automations);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setLoadError(
          cause instanceof ApiError ? cause.message : "The automations could not be loaded.",
        );
      });
    return () => {
      cancelled = true;
    };
  }, [user, organizationId, reloadToken]);

  // The list's own filters: an event, a state, and a name search.
  const filtered = useMemo(() => {
    if (!automations) {
      return null;
    }
    const needle = query.trim().toLowerCase();
    return automations.filter((automation) => {
      if (eventFilter && automation.event !== eventFilter) {
        return false;
      }
      if (onlyEnabled === "armed" && !automation.enabled) {
        return false;
      }
      if (onlyEnabled === "paused" && automation.enabled) {
        return false;
      }
      return !needle || automation.name.toLowerCase().includes(needle);
    });
  }, [automations, eventFilter, onlyEnabled, query]);

  const openCreate = useCallback(() => {
    setSaveError(null);
    setNotice(null);
    setReport(null);
    setHookUrl(null);
    setPayloadText(JSON.stringify(SAMPLE_PAYLOAD, null, 2));
    setDraft({ ...EMPTY_DRAFT, steps: EMPTY_DRAFT.steps.map((step) => ({ ...step })) });
  }, []);

  /** Open one rule for editing, and load its test surface. */
  const openEdit = useCallback(async (automation: Automation) => {
    setSaveError(null);
    setNotice(null);
    setReport(null);
    setHookUrl(null);
    setPayloadText(JSON.stringify(SAMPLE_PAYLOAD, null, 2));
    setDraft({
      id: automation.id,
      name: automation.name,
      description: automation.description,
      enabled: automation.enabled,
      event: automation.event,
      hookTriggered: automation.trigger === "inbound_webhook",
      conditions: toEditable(automation.conditions),
      steps: stepsOf(automation),
    });
    setListeners([]);
    try {
      const answer = await fetchAutomationTests(automation.id);
      setListeners(answer.tests);
    } catch {
      // A rule whose test history cannot be read is still editable; the panel says so below
      // the editor rather than refusing to open the rule at all.
      setListeners([]);
    }
  }, []);

  const closeEditor = useCallback(() => {
    setDraft(null);
    setSaveError(null);
    setReport(null);
    setHookUrl(null);
  }, []);

  /** Write the open rule, then close the editor on success. */
  const save = useCallback(async () => {
    if (!draft) {
      return;
    }
    setSaving(true);
    setSaveError(null);
    setNotice(null);

    const payload = {
      organization_id: organizationId,
      name: draft.name,
      description: draft.description,
      enabled: draft.enabled,
      event: draft.hookTriggered ? draft.event : draft.event,
      conditions: toWire(draft.conditions),
      hook_triggered: draft.hookTriggered,
      actions: draft.steps,
    };

    try {
      if (draft.id) {
        await updateAutomation(draft.id, payload);
        setNotice(`“${draft.name}” was saved.`);
      } else {
        await createAutomation(payload);
        setNotice(`“${draft.name}” was created.`);
      }
      setDraft(null);
      reload();
    } catch (cause) {
      setSaveError(
        cause instanceof ApiError ? cause.message : "The rule could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  }, [draft, organizationId, reload]);

  /** Arm or disarm one rule from the list, without opening the editor. */
  const toggleRule = useCallback(
    async (automation: Automation) => {
      setBusy(automation.id);
      try {
        await updateAutomation(automation.id, {
          organization_id: automation.organization_id,
          site_id: automation.site_id,
          name: automation.name,
          description: automation.description,
          enabled: !automation.enabled,
          event: automation.event,
          conditions: automation.conditions,
          hook_triggered: automation.trigger === "inbound_webhook",
          actions: stepsOf(automation),
        });
        reload();
      } catch (cause) {
        setLoadError(
          cause instanceof ApiError ? cause.message : "The rule could not be changed.",
        );
      } finally {
        setBusy(null);
      }
    },
    [reload],
  );

  const removeRule = useCallback(
    async (automation: Automation) => {
      setBusy(automation.id);
      try {
        await deleteAutomation(automation.id);
        setNotice(`“${automation.name}” was deleted.`);
        setConfirmDelete(null);
        reload();
      } catch (cause) {
        setLoadError(
          cause instanceof ApiError ? cause.message : "The rule could not be deleted.",
        );
      } finally {
        setBusy(null);
      }
    },
    [reload],
  );

  /** Evaluate a hand-written payload. Nothing is sent — that is the point. */
  const runTest = useCallback(
    async (automationId: string) => {
      setBusy(automationId);
      setNotice(null);
      let payload: unknown;
      try {
        payload = JSON.parse(payloadText) as unknown;
      } catch {
        setSaveError("The test payload is not valid JSON — fix it and try again.");
        setBusy(null);
        return;
      }
      setSaveError(null);
      try {
        const answer = await testAutomation(automationId, payload);
        setReport(answer.report);
        setListeners((rows) => [answer.recorded, ...rows]);
      } catch (cause) {
        setSaveError(
          cause instanceof ApiError ? cause.message : "The test event could not be evaluated.",
        );
      } finally {
        setBusy(null);
      }
    },
    [payloadText],
  );

  /** Arm a one-shot listener for the next real event this rule matches. */
  const armListener = useCallback(async (automationId: string) => {
    setBusy(automationId);
    try {
      const armed = await listenAutomation(automationId);
      setListeners((rows) => [armed, ...rows]);
      setNotice("Listening — the next matching event will show up here.");
    } catch (cause) {
      setLoadError(
        cause instanceof ApiError ? cause.message : "The listener could not be armed.",
      );
    } finally {
      setBusy(null);
    }
  }, []);

  /** Mint a fresh hook token; the response is the only place one appears. */
  const rotateHook = useCallback(async (automationId: string) => {
    setBusy(automationId);
    try {
      const issued = await rotateAutomationHook(automationId);
      setHookUrl(issued.url);
      setNotice("A new URL was created. The previous one stopped working immediately.");
      reload();
    } catch (cause) {
      setLoadError(
        cause instanceof ApiError ? cause.message : "A new URL could not be created.",
      );
    } finally {
      setBusy(null);
    }
  }, [reload]);

  const eventNames = useMemo(() => {
    const names = new Set((automations ?? []).map((automation) => automation.event));
    for (const event of catalogue?.events ?? []) {
      names.add(event.name);
    }
    return Array.from(names).sort();
  }, [automations, catalogue]);

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium">Automations</h2>
          <p className="text-[12.5px] text-muted">
            Rules that run when the platform records an event, or when a webhook calls in.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            data-automation-refresh
            onClick={reload}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
            Refresh
          </button>
          <button
            type="button"
            data-automation-new
            onClick={openCreate}
            className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] text-paper hover:opacity-90"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden="true" />
            New rule
          </button>
        </div>
      </header>

      {notice ? (
        <p
          data-automation-notice
          className="rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}
      {loadError ? (
        <p
          data-automation-error
          role="alert"
          className="rounded-md bg-critical-soft px-3 py-2 text-[12.5px] text-critical"
        >
          {loadError}
        </p>
      ) : null}

      {automations === null && !loadError ? <LoadingTable columns={COLUMNS.length} /> : null}

      {automations !== null && filtered !== null ? (
        <section className="flex flex-col gap-3">
          <div className="flex flex-wrap items-center gap-2">
            <label className="sr-only" htmlFor="automation-search">
              Search automations
            </label>
            <input
              id="automation-search"
              data-automation-search
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search by name"
              className="rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
            />
            <label className="sr-only" htmlFor="automation-event-filter">
              Filter by event
            </label>
            <select
              id="automation-event-filter"
              data-automation-event-filter
              value={eventFilter}
              onChange={(event) => setEventFilter(event.target.value)}
              className="rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
            >
              <option value="">Every event</option>
              {eventNames.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
            <label className="sr-only" htmlFor="automation-state-filter">
              Filter by state
            </label>
            <select
              id="automation-state-filter"
              data-automation-state-filter
              value={onlyEnabled}
              onChange={(event) => setOnlyEnabled(event.target.value as typeof onlyEnabled)}
              className="rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
            >
              <option value="all">Armed and paused</option>
              <option value="armed">Armed</option>
              <option value="paused">Paused</option>
            </select>
          </div>

          {filtered.length === 0 ? (
            <EmptyState
              title={automations.length === 0 ? "No automations yet" : "No rule matches these filters"}
              hint={
                automations.length === 0
                  ? "A rule listens for an event, checks its conditions and runs its actions. Start with one on the event you care about."
                  : "Clear the search or the filters to see the rules again."
              }
              action={
                automations.length === 0 ? (
                  <button
                    type="button"
                    data-automation-empty-new
                    onClick={openCreate}
                    className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] text-paper"
                  >
                    <Plus className="h-3.5 w-3.5" aria-hidden="true" />
                    New rule
                  </button>
                ) : null
              }
            />
          ) : (
            <>
              {/* Desktop: the table the plan names. */}
              <div className="hidden overflow-x-auto md:block">
                <table className="w-full border-collapse text-left text-[13px]">
                  <thead>
                    <tr className="text-[11.5px] uppercase tracking-wide text-muted">
                      {COLUMNS.map((column) => (
                        <th key={column} className="px-3 py-2 font-medium">
                          {column}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {filtered.map((automation) => (
                      <tr
                        key={automation.id}
                        data-automation-row={automation.id}
                        className="border-t border-line"
                      >
                        <td className="px-3 py-2.5">
                          <Link
                            href={`/automations/${automation.id}`}
                            data-automation-open={automation.id}
                            className="font-medium hover:underline"
                          >
                            {automation.name}
                          </Link>
                          {automation.description ? (
                            <p className="mt-0.5 max-w-xs truncate text-[12px] text-muted">
                              {automation.description}
                            </p>
                          ) : null}
                        </td>
                        <td className="px-3 py-2.5">
                          <span
                            data-automation-trigger={automation.id}
                            className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-2 py-0.5 text-[11.5px]"
                          >
                            {automation.trigger === "inbound_webhook" ? (
                              <Webhook className="h-3 w-3" aria-hidden="true" />
                            ) : null}
                            {triggerLabel(automation)}
                          </span>
                        </td>
                        <td className="px-3 py-2.5 text-[12.5px] text-muted">
                          {describeConditions(automation.conditions, automation.condition_count)}
                        </td>
                        <td className="px-3 py-2.5 text-[12.5px] text-muted">
                          {stepsOf(automation).length} step
                          {stepsOf(automation).length === 1 ? "" : "s"}
                        </td>
                        <td className="px-3 py-2.5">
                          <span
                            data-automation-state={automation.id}
                            className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
                              automation.enabled
                                ? "bg-positive-soft text-positive"
                                : "bg-quiet-soft text-muted"
                            }`}
                          >
                            {automation.enabled ? "Armed" : "Paused"}
                          </span>
                        </td>
                        <td className="px-3 py-2.5 text-[12.5px]">{automation.trigger_count}</td>
                        <td className="px-3 py-2.5 text-[12.5px] text-muted">
                          {automation.last_triggered_at
                            ? formatTimestamp(automation.last_triggered_at)
                            : "Never"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>

              {/* Mobile: the same rules as cards. */}
              <ul className="flex flex-col gap-2 md:hidden" data-automation-cards>
                {filtered.map((automation) => (
                  <li
                    key={automation.id}
                    data-automation-card={automation.id}
                    className="flex flex-col gap-1.5 rounded-md border border-line px-3 py-2.5"
                  >
                    <Link
                      href={`/automations/${automation.id}`}
                      className="text-[13px] font-medium hover:underline"
                    >
                      {automation.name}
                    </Link>
                    <div className="flex flex-wrap items-center gap-1.5 text-[11.5px] text-muted">
                      <span className="rounded-full bg-quiet-soft px-2 py-0.5">
                        {triggerLabel(automation)}
                      </span>
                      <span
                        className={`rounded-full px-2 py-0.5 font-medium ${
                          automation.enabled
                            ? "bg-positive-soft text-positive"
                            : "bg-quiet-soft"
                        }`}
                      >
                        {automation.enabled ? "Armed" : "Paused"}
                      </span>
                    </div>
                    <p className="text-[11.5px] text-muted">
                      {automation.last_triggered_at
                        ? `Last fired ${formatTimestamp(automation.last_triggered_at)}`
                        : "Never fired"}
                    </p>
                  </li>
                ))}
              </ul>
            </>
          )}
        </section>
      ) : null}

      {draft ? (
        <AutomationEditor
          draft={draft}
          catalogue={catalogue}
          saving={saving}
          error={saveError}
          busy={busy}
          report={report}
          payloadText={payloadText}
          listeners={listeners}
          hookUrl={hookUrl}
          onPayloadChange={setPayloadText}
          onDraftChange={setDraft}
          onClose={closeEditor}
          onSave={save}
          onTest={runTest}
          onListen={armListener}
          onRotateHook={rotateHook}
          onArmDelete={setConfirmDelete}
        />
      ) : null}

      {confirmDelete !== null ? (
        <DeleteDialog
          name={confirmDelete}
          onCancel={() => setConfirmDelete(null)}
          onConfirm={() => {
            const target = automations?.find((item) => item.name === confirmDelete);
            if (target) {
              void removeRule(target);
            } else {
              setConfirmDelete(null);
            }
          }}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------------------------

type EditorProps = {
  draft: Draft;
  catalogue: AutomationCatalogue | null;
  saving: boolean;
  error: string | null;
  busy: string | null;
  report: AutomationDryRun | null;
  payloadText: string;
  listeners: AutomationTestEvent[];
  hookUrl: string | null;
  onPayloadChange: (value: string) => void;
  onDraftChange: (draft: Draft) => void;
  onClose: () => void;
  onSave: () => void;
  onTest: (automationId: string) => void;
  onListen: (automationId: string) => void;
  onRotateHook: (automationId: string) => void;
  onArmDelete: (name: string) => void;
};

/** The rule editor: trigger, conditions, actions, and the test-fire surface. */
function AutomationEditor({
  draft,
  catalogue,
  saving,
  error,
  busy,
  report,
  payloadText,
  listeners,
  hookUrl,
  onPayloadChange,
  onDraftChange,
  onClose,
  onSave,
  onTest,
  onListen,
  onRotateHook,
  onArmDelete,
}: EditorProps) {
  const eventFields = fieldsFor(catalogue, draft.event);
  const maxDepth = catalogue?.max_group_depth ?? 3;
  const maxConditions = catalogue?.max_conditions ?? 24;
  const conditionCount = countNodes(members(draft.conditions));
  const [openGroup, setOpenGroup] = useState<string | null>("root");

  // Which problems the editor found, so the summary at the top can jump to the first.
  const problems: string[] = [];
  if (!draft.name.trim()) {
    problems.push("The rule needs a name.");
  }
  if (draft.steps.length === 0) {
    problems.push("The rule needs at least one action.");
  }
  if (conditionCount > maxConditions) {
    problems.push(`A rule carries at most ${maxConditions} conditions and groups.`);
  }
  if (!draft.hookTriggered && !draft.event.trim()) {
    problems.push("Choose the event the rule listens for.");
  }

  const setRoot = (next: AutomationGroup) => onDraftChange({ ...draft, conditions: next });

  /** Rewrite one node in the root group, addressed by its index. */
  const updateRootNode = (index: number, node: AutomationNode) => {
    const next = [...members(draft.conditions)];
    next[index] = node;
    setRoot({ mode: draft.conditions.mode, all: next, any: next });
  };

  const addCondition = () => {
    const first = eventFields[0]?.key ?? "status";
    const blank: AutomationCondition = { field: first, operator: "equals", value: "" };
    updateRootNode(members(draft.conditions).length, blank);
    setOpenGroup("root");
  };

  const removeNode = (index: number) => {
    const next = members(draft.conditions).filter((_, at) => at !== index);
    setRoot({ mode: draft.conditions.mode, all: next, any: next });
  };

  const addGroup = () => {
    // A group may only be added while the catalogue's depth budget has room; the server
    // refuses a deeper tree anyway, and a button that can only fail is a dead button.
    if (maxDepth < 2) {
      return;
    }
    const group: AutomationGroup = { mode: "any", any: [] };
    updateRootNode(members(draft.conditions).length, group);
  };

  return (
    <section
      data-automation-editor={draft.id ?? "new"}
      className="flex flex-col gap-4 rounded-lg border border-line p-4"
    >
      <header className="flex items-center justify-between gap-3">
        <h3 className="text-[14px] font-medium">
          {draft.id ? `Edit “${draft.name || "rule"}”` : "New rule"}
        </h3>
        <button
          type="button"
          data-automation-close
          onClick={onClose}
          className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
        >
          <X className="h-3.5 w-3.5" aria-hidden="true" />
          Close
        </button>
      </header>

      {problems.length > 0 ? (
        <div
          data-automation-problems
          className="rounded-md bg-caution-soft px-3 py-2 text-[12.5px] text-caution"
        >
          <p className="font-medium">
            {problems.length} problem{problems.length === 1 ? "" : "s"} found
          </p>
          <ul className="mt-1 list-disc pl-4">
            {problems.map((problem) => (
              <li key={problem} data-automation-problem>
                {problem}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {error ? (
        <p
          data-automation-save-error
          role="alert"
          className="rounded-md bg-critical-soft px-3 py-2 text-[12.5px] text-critical"
        >
          {error}
        </p>
      ) : null}

      {/* Trigger */}
      <fieldset className="flex flex-col gap-2">
        <legend className="text-[12.5px] font-medium">Trigger</legend>
        <div className="flex flex-wrap items-center gap-3">
          <label className="flex items-center gap-1.5 text-[12.5px]">
            <input
              type="radio"
              name="automation-trigger-kind"
              data-automation-trigger-event
              checked={!draft.hookTriggered}
              onChange={() => onDraftChange({ ...draft, hookTriggered: false })}
            />
            A platform event
          </label>
          <label className="flex items-center gap-1.5 text-[12.5px]">
            <input
              type="radio"
              name="automation-trigger-kind"
              data-automation-trigger-hook
              checked={draft.hookTriggered}
              onChange={() =>
                onDraftChange({
                  ...draft,
                  hookTriggered: true,
                  event: "automation.hook.received",
                })
              }
            />
            An inbound webhook
          </label>
        </div>

        {!draft.hookTriggered ? (
          <label className="flex flex-col gap-1 text-[12.5px]">
            <span className="text-muted">Event</span>
            <select
              data-automation-event
              aria-label="Event"
              value={draft.event}
              onChange={(event) => onDraftChange({ ...draft, event: event.target.value })}
              className="rounded-md border border-line bg-paper px-2.5 py-1.5"
            >
              {(catalogue?.events ?? []).map((event) => (
                <option key={event.name} value={event.name}>
                  {event.name} — {event.description}
                </option>
              ))}
              {!catalogue?.events.some((event) => event.name === draft.event) ? (
                <option value={draft.event}>{draft.event}</option>
              ) : null}
            </select>
          </label>
        ) : (
          <div className="flex flex-col gap-1.5 rounded-md border border-line px-3 py-2">
            <p className="text-[12.5px] text-muted">
              The rule listens for <code>automation.hook.received</code>. Its conditions read
              the caller&apos;s body as <code>hook.body.…</code>.
            </p>
            {hookUrl ? (
              <p className="flex items-center gap-2 text-[12.5px]" data-automation-hook-url>
                <code className="truncate">{hookUrl}</code>
                <button
                  type="button"
                  data-automation-hook-copy
                  onClick={() => void navigator.clipboard?.writeText(hookUrl)}
                  className="inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11.5px]"
                >
                  <Copy className="h-3 w-3" aria-hidden="true" />
                  Copy
                </button>
              </p>
            ) : (
              <p className="text-[12.5px]" data-automation-hook-empty>
                {draft.id
                  ? "This rule has no URL yet. Creating one gives the caller a secret the server only keeps a hash of."
                  : "Save the rule first — then its URL can be created."}
              </p>
            )}
            {draft.id ? (
              <button
                type="button"
                data-automation-hook-rotate
                disabled={busy === draft.id}
                onClick={() => onRotateHook(draft.id as string)}
                className="inline-flex w-fit items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-60"
              >
                <Webhook className="h-3.5 w-3.5" aria-hidden="true" />
                {hookUrl ? "Create another URL" : "Create the URL"}
              </button>
            ) : null}
          </div>
        )}

        <div className="flex flex-col gap-1 text-[12.5px]">
          <label className="text-muted" htmlFor="automation-name">
            Name
          </label>
          <input
            id="automation-name"
            data-automation-name
            value={draft.name}
            onChange={(event) => onDraftChange({ ...draft, name: event.target.value })}
            className="rounded-md border border-line bg-paper px-2.5 py-1.5"
          />
          <label className="mt-2 text-muted" htmlFor="automation-description">
            Description
          </label>
          <input
            id="automation-description"
            data-automation-description
            value={draft.description}
            onChange={(event) => onDraftChange({ ...draft, description: event.target.value })}
            className="rounded-md border border-line bg-paper px-2.5 py-1.5"
          />
          <label className="mt-2 flex items-center gap-2">
            <input
              type="checkbox"
              data-automation-enabled
              checked={draft.enabled}
              onChange={(event) => onDraftChange({ ...draft, enabled: event.target.checked })}
            />
            Armed — the rule fires as soon as it is saved
          </label>
        </div>
      </fieldset>

      {/* Conditions */}
      <fieldset className="flex flex-col gap-2">
        <legend className="text-[12.5px] font-medium">
          Conditions · {conditionCount} of {maxConditions}
        </legend>
        <p className="text-[12px] text-muted">
          Only the fields {draft.event} actually carries are offered. An empty rule fires on
          every one of its events.
        </p>

        <div className="flex flex-wrap items-center gap-2">
          <label className="sr-only" htmlFor="automation-group-mode">
            Group mode
          </label>
          <select
            id="automation-group-mode"
            data-automation-group-mode
            value={draft.conditions.mode ?? "all"}
            onChange={(event) =>
              setRoot({
                mode: event.target.value === "any" ? "any" : "all",
                all: members(draft.conditions),
                any: members(draft.conditions),
              })
            }
            className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
          >
            <option value="all">All of these must hold</option>
            <option value="any">Any of these may hold</option>
          </select>
          <button
            type="button"
            data-automation-add-condition
            onClick={addCondition}
            className="rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
          >
            <Plus className="h-3 w-3" aria-hidden="true" /> Add condition
          </button>
          <button
            type="button"
            data-automation-add-group
            onClick={addGroup}
            className="rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
          >
            <Plus className="h-3 w-3" aria-hidden="true" /> Add group
          </button>
        </div>

        <ConditionGroupEditor
          group={draft.conditions}
          depth={1}
          maxDepth={maxDepth}
          keyName={openGroup}
          catalogue={catalogue}
          event={draft.event}
          onToggle={(key) => setOpenGroup((current) => (current === key ? null : key))}
          onChange={(next) => setRoot(next)}
        />
      </fieldset>

      {/* Actions */}
      <fieldset className="flex flex-col gap-2">
        <legend className="text-[12.5px] font-medium">Actions</legend>
        {draft.steps.length === 0 ? (
          <p className="text-[12.5px] text-muted" data-automation-no-actions>
            A rule needs at least one action. Add one below.
          </p>
        ) : null}
        <ol className="flex flex-col gap-2" data-automation-steps>
          {draft.steps.map((step, index) => (
            <li
              key={`${step.name}-${index}`}
              data-automation-step={index}
              className="flex flex-col gap-2 rounded-md border border-line px-3 py-2"
            >
              <div className="flex flex-wrap items-center gap-2">
                <label className="sr-only" htmlFor={`automation-step-name-${index}`}>
                  Step {index + 1} name
                </label>
                <input
                  id={`automation-step-name-${index}`}
                  data-automation-step-name={index}
                  value={step.name}
                  onChange={(event) => {
                    const next = [...draft.steps];
                    next[index] = { ...step, name: event.target.value };
                    onDraftChange({ ...draft, steps: next });
                  }}
                  className="w-48 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                />
                <label className="sr-only" htmlFor={`automation-step-action-${index}`}>
                  Step {index + 1} action
                </label>
                <select
                  id={`automation-step-action-${index}`}
                  data-automation-step-action={index}
                  value={step.action ?? ""}
                  onChange={(event) => {
                    const next = [...draft.steps];
                    next[index] = { ...step, action: event.target.value };
                    onDraftChange({ ...draft, steps: next });
                  }}
                  className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                >
                  {(catalogue?.actions ?? []).map((action) => (
                    <option key={action.key} value={action.key}>
                      {action.key} — {action.description}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  data-automation-step-remove={index}
                  onClick={() =>
                    onDraftChange({
                      ...draft,
                      steps: draft.steps.filter((_, at) => at !== index),
                    })
                  }
                  className="ml-auto inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11.5px] hover:bg-quiet-soft"
                >
                  <Trash2 className="h-3 w-3" aria-hidden="true" /> Remove
                </button>
              </div>
              <label className="sr-only" htmlFor={`automation-step-params-${index}`}>
                Step {index + 1} parameters (JSON)
              </label>
              <textarea
                id={`automation-step-params-${index}`}
                data-automation-step-params={index}
                rows={3}
                value={JSON.stringify(step.params ?? {}, null, 2)}
                onChange={(event) => {
                  try {
                    const parsed = JSON.parse(event.target.value) as Record<string, unknown>;
                    const next = [...draft.steps];
                    next[index] = { ...step, params: parsed };
                    onDraftChange({ ...draft, steps: next });
                  } catch {
                    // A half-typed JSON object is normal; keep the last good value and let
                    // the save report the shape if it is still broken.
                  }
                }}
                className="w-full rounded-md border border-line bg-paper px-2 py-1 font-mono text-[11.5px]"
              />
            </li>
          ))}
        </ol>
        <button
          type="button"
          data-automation-add-step
          onClick={() =>
            onDraftChange({
              ...draft,
              steps: [
                ...draft.steps,
                {
                  name: `step ${draft.steps.length + 1}`,
                  kind: "task",
                  action: "noop",
                  params: {},
                  max_attempts: 1,
                },
              ],
            })
          }
          className="w-fit rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
        >
          <Plus className="h-3 w-3" aria-hidden="true" /> Add step
        </button>
      </fieldset>

      {/* Test fire */}
      {draft.id ? (
        <fieldset className="flex flex-col gap-2" data-automation-testfire>
          <legend className="text-[12.5px] font-medium">Test</legend>
          <label className="text-[12px] text-muted" htmlFor="automation-payload">
            Payload to evaluate — nothing is sent, published or called
          </label>
          <textarea
            id="automation-payload"
            data-automation-payload
            rows={5}
            value={payloadText}
            onChange={(event) => onPayloadChange(event.target.value)}
            className="w-full rounded-md border border-line bg-paper px-2 py-1 font-mono text-[11.5px]"
          />
          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              data-automation-run-test
              disabled={busy === draft.id}
              onClick={() => onTest(draft.id as string)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-60"
            >
              <FlaskConical className="h-3.5 w-3.5" aria-hidden="true" />
              Send test event
            </button>
            <button
              type="button"
              data-automation-listen
              disabled={busy === draft.id}
              onClick={() => onListen(draft.id as string)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-60"
            >
              <Radio className="h-3.5 w-3.5" aria-hidden="true" />
              Listen for a real event
            </button>
          </div>

          {report ? <DryRunReportView report={report} /> : null}

          {listeners.length > 0 ? (
            <ul className="flex flex-col gap-1.5" data-automation-test-history>
              {listeners.slice(0, 5).map((row) => (
                <li
                  key={row.id}
                  data-automation-test-row={row.id}
                  className="rounded-md border border-line px-2.5 py-1.5 text-[11.5px]"
                >
                  <span className="font-medium">
                    {row.kind === "listen" ? "Listener" : "Test"}
                  </span>{" "}
                  <span className="text-muted">
                    {row.armed
                      ? "· armed, waiting for the next matching event"
                      : `· ${row.event_name ?? "hand-written"} · ${formatTimestamp(row.created_at)}`}
                  </span>
                </li>
              ))}
            </ul>
          ) : null}
        </fieldset>
      ) : null}

      {/* The sticky footer the plan asks for. */}
      <footer className="sticky bottom-0 flex flex-wrap items-center gap-2 border-t border-line bg-paper pt-3">
        <button
          type="button"
          data-automation-save
          disabled={saving || problems.length > 0}
          onClick={onSave}
          className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-paper disabled:opacity-50"
        >
          {saving ? "Saving…" : "Save"}
        </button>
        {draft.id ? (
          <button
            type="button"
            data-automation-delete
            onClick={() => onArmDelete(draft.name)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
          >
            <Trash2 className="h-3.5 w-3.5" aria-hidden="true" />
            Delete
          </button>
        ) : null}
      </footer>
    </section>
  );
}

// ---------------------------------------------------------------------------------------------
// Conditions
// ---------------------------------------------------------------------------------------------

type GroupProps = {
  group: AutomationGroup;
  depth: number;
  maxDepth: number;
  keyName: string | null;
  catalogue: AutomationCatalogue | null;
  event: string;
  onToggle: (key: string) => void;
  onChange: (group: AutomationGroup) => void;
};

/** One group of conditions, with its rows and any nested groups. */
function ConditionGroupEditor({
  group,
  depth,
  maxDepth,
  keyName,
  catalogue,
  event,
  onToggle,
  onChange,
}: GroupProps) {
  const fields = fieldsFor(catalogue, event);
  const rows = members(group);

  return (
    <div
      data-automation-group={depth}
      className={`flex flex-col gap-2 rounded-md border border-line px-3 py-2 ${depth > 1 ? "bg-quiet-soft" : ""}`}
    >
      <div className="flex items-center gap-2">
        <label className="sr-only" htmlFor={`automation-group-mode-${depth}`}>
          Group mode
        </label>
        <select
          id={`automation-group-mode-${depth}`}
          data-automation-group-mode-depth={depth}
          value={group.mode ?? "all"}
          onChange={(event) =>
            onChange({
              mode: event.target.value === "any" ? "any" : "all",
              all: rows,
              any: rows,
            })
          }
          className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
        >
          <option value="all">All</option>
          <option value="any">Any</option>
        </select>
        <span className="text-[11.5px] text-muted">
          {rows.length} member{rows.length === 1 ? "" : "s"} at level {depth} of {maxDepth}
        </span>
      </div>

      {rows.length === 0 ? (
        <p className="text-[12px] text-muted" data-automation-group-empty={depth}>
          {depth === 1
            ? "No conditions — the rule fires on every one of its events."
            : "An empty group can never hold; add a condition or remove it."}
        </p>
      ) : null}

      <ul className="flex flex-col gap-1.5">
        {rows.map((node, index) =>
          isGroup(node) ? (
            <li key={`group-${index}`} data-automation-nested-group={index}>
              <ConditionGroupEditor
                group={node}
                depth={depth + 1}
                maxDepth={maxDepth}
                keyName={keyName}
                catalogue={catalogue}
                event={event}
                onToggle={onToggle}
                onChange={(next) => {
                  const updated = [...rows];
                  updated[index] = next;
                  onChange({ mode: group.mode ?? "all", all: updated, any: updated });
                }}
              />
            </li>
          ) : (
            <li
              key={`row-${index}`}
              data-automation-condition={index}
              className="flex flex-wrap items-center gap-1.5"
            >
              <label className="sr-only" htmlFor={`automation-field-${index}`}>
                Field
              </label>
              <select
                id={`automation-field-${index}`}
                data-automation-field={index}
                value={node.field}
                onChange={(event) => {
                  const updated = [...rows];
                  updated[index] = { ...node, field: event.target.value };
                  onChange({ mode: group.mode ?? "all", all: updated, any: updated });
                }}
                className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
              >
                {fields.map((field) => (
                  <option key={field.key} value={field.key}>
                    {field.key} — {field.label}
                  </option>
                ))}
                {!fields.some((field) => field.key === node.field) ? (
                  <option value={node.field}>{node.field}</option>
                ) : null}
              </select>
              <label className="sr-only" htmlFor={`automation-operator-${index}`}>
                Operator
              </label>
              <select
                id={`automation-operator-${index}`}
                data-automation-operator={index}
                value={node.operator}
                onChange={(event) => {
                  const updated = [...rows];
                  const operator = event.target.value;
                  updated[index] = { ...node, operator };
                  onChange({ mode: group.mode ?? "all", all: updated, any: updated });
                }}
                className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
              >
                {(catalogue?.condition_operators ?? []).map((operator) => (
                  <option key={operator.key} value={operator.key}>
                    {operator.key}
                  </option>
                ))}
              </select>
              {(catalogue?.condition_operators ?? []).find(
                (operator) => operator.key === node.operator,
              )?.needs_value ? (
                <>
                  <label className="sr-only" htmlFor={`automation-value-${index}`}>
                    Value
                  </label>
                  <input
                    id={`automation-value-${index}`}
                    data-automation-value={index}
                    value={typeof node.value === "string" ? node.value : JSON.stringify(node.value ?? "")}
                    onChange={(event) => {
                      const updated = [...rows];
                      updated[index] = { ...node, value: event.target.value };
                      onChange({ mode: group.mode ?? "all", all: updated, any: updated });
                    }}
                    className="w-40 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                  />
                </>
              ) : null}
              <button
                type="button"
                data-automation-condition-remove={index}
                onClick={() =>
                  onChange({
                    mode: group.mode ?? "all",
                    all: rows.filter((_, at) => at !== index),
                    any: rows.filter((_, at) => at !== index),
                  })
                }
                className="rounded-md border border-line px-1.5 py-0.5 text-[11.5px] hover:bg-quiet-soft"
              >
                Remove
              </button>
            </li>
          ),
        )}
      </ul>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The dry-run report
// ---------------------------------------------------------------------------------------------

/** The report of a dry run, row by row. */
function DryRunReportView({ report }: { report: AutomationDryRun }) {
  return (
    <div
      data-automation-report
      className="flex flex-col gap-2 rounded-md border border-line px-3 py-2"
    >
      <p className="text-[12.5px] font-medium" data-automation-report-verdict>
        {report.would_run
          ? "The conditions hold — these actions would run."
          : `The rule would not run: ${report.reason ?? "the conditions did not hold"}`}
      </p>
      <ul className="flex flex-col gap-1">
        {report.actions.map((action, index) => (
          <li
            key={`${action.name}-${index}`}
            data-automation-report-action={index}
            className="text-[12px]"
          >
            <span className="font-medium">{action.name}</span>{" "}
            <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px]">
              {action.outcome}
            </span>
            {action.summary ? <span className="text-muted"> {action.summary}</span> : null}
          </li>
        ))}
      </ul>
      <p className="text-[11.5px] text-muted">
        Simulated — no e-mail was sent, nothing was published and no URL was called.
      </p>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------------------------

/** Confirming a delete by typing the rule's name. */
function DeleteDialog({
  name,
  onCancel,
  onConfirm,
}: {
  name: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [typed, setTyped] = useState("");
  return (
    <div
      data-automation-delete-dialog
      className="fixed inset-0 z-50 flex items-center justify-center bg-ink/20 p-4"
    >
      <div className="flex w-full max-w-sm flex-col gap-3 rounded-lg border border-line bg-paper p-4">
        <h3 className="text-[14px] font-medium">Delete “{name}”?</h3>
        <p className="text-[12.5px] text-muted">
          The rule and its run history are removed. Type the name to confirm.
        </p>
        <label className="sr-only" htmlFor="automation-delete-confirm">
          Rule name
        </label>
        <input
          id="automation-delete-confirm"
          data-automation-delete-input
          value={typed}
          onChange={(event) => setTyped(event.target.value)}
          className="rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
        />
        <div className="flex justify-end gap-2">
          <button
            type="button"
            data-automation-delete-cancel
            onClick={onCancel}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="button"
            data-automation-delete-confirm-button
            disabled={typed !== name}
            onClick={onConfirm}
            className="rounded-md bg-critical px-2.5 py-1.5 text-[12.5px] text-paper disabled:opacity-50"
          >
            Delete
          </button>
        </div>
      </div>
    </div>
  );
}

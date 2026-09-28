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
  Check,
  Copy,
  FlaskConical,
  LayoutGrid,
  Play,
  Plus,
  Radio,
  RefreshCw,
  ShieldCheck,
  Trash2,
  Webhook,
  X,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import {
  AuditPanel,
  OPERATIONS_TABS,
  RunsPanel,
  VersionsPanel,
  type OperationsTab,
} from "@/features/automations/operations-views";
import { LoadingTable } from "@/components/loading-table";
import { useSession } from "@/lib/session";
import {
  ApiError,
  createAutomation,
  decideApproval,
  deleteAutomation,
  fetchApprovals,
  fetchAutomation,
  fetchAutomationCatalogue,
  fetchOrganizations,
  fetchAutomationTests,
  fetchAutomations,
  fetchIamUsers,
  listenAutomation,
  rotateAutomationHook,
  runAutomation,
  testAutomation,
  updateAutomation,
  type Automation,
  type AutomationApproval,
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

/** What each step kind is called in the panel — the wire name is for the API. */
const STEP_KIND_LABELS: Record<string, string> = {
  task: "Action",
  wait: "Wait",
  branch: "Branch",
  stop: "Stop",
  approval: "Wait for approval",
};

/** What each error policy does, in words a rule author can act on. */
const ON_ERROR_LABELS: Record<string, string> = {
  inherit: "Use the rule's policy",
  stop: "Stop the run",
  continue: "Record it and go on",
};

/**
 * The parameters a step of one kind starts from.
 *
 * Each kind's parameters mean something different, so switching kind replaces them rather
 * than carrying them across: keeping a branch's `{field, operator, value}` as a task's
 * parameters would save a rule the engine refuses, and the author would only find out at
 * run time. An `http_request` step gets a shape that is *nearly* valid so the save-time
 * allow-list check can name the host instead of complaining about an empty URL.
 */
function defaultParamsFor(kind: string, current: Record<string, unknown>) {
  switch (kind) {
    case "wait":
      return typeof current.seconds === "number" ? current : { seconds: 60 };
    case "branch":
      return { field: "event.status", operator: "equals", value: "published" };
    case "stop":
      return { reason: "this run was stopped on purpose" };
    case "approval":
      // A gate with no permission named is the platform's own `workflows.approve`, and the
      // message is what a decider reads — so both are filled in rather than left blank, and
      // an author who never touches them gets a working gate.
      return typeof current.permission === "string"
        ? current
        : {
            permission: "workflows.approve",
            message: "a person must approve this step before the run goes on",
            expires_in_hours: 72,
          };
    case "task":
      if (current.url) {
        return current;
      }
      return { url: "http://127.0.0.1:8080/healthz", method: "POST", body: {} };
    default:
      return current;
  }
}

/**
 * The `steps.<n>.<field>` paths a branch at `index` can read.
 *
 * A step's *declared* output keys are only knowable for the two actions the platform
 * implements (an email's recipient, an http call's `ok`), so the hint offers what the
 * *engine* knows: a step's `action`, and — for an `http_request` — the three fields it
 * writes. Everything else is typed by hand, which is honest: a branch on a field a
 * future action adds should not be blocked by today's editor.
 */
function stepFieldHints(steps: AutomationStep[], index: number): string[] {
  return steps.slice(0, index).flatMap((previous, at) => {
    const prefix = `steps.${at + 1}`;
    const common = [`${prefix}.action`];
    if (previous.action === "http_request") {
      return [...common, `${prefix}.ok`, `${prefix}.status_code`, `${prefix}.url`];
    }
    if (previous.action === "echo") {
      return [...common, `${prefix}.value`];
    }
    return common;
  });
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
  /** The rule's own failure policy; a step that inherits takes this. */
  onError: Automation["on_error"];
  /** Whose authority the rule's host actions run with; `null` follows the author. */
  runAs: string | null;
  /**
   * The API's own sentence about the rule's current authority, kept so the editor shows
   * what the *server* resolved rather than guessing from the picker's value. A rule whose
   * author was deleted answers "runs as nobody" here, and that is a fact the editor has to
   * be able to show rather than infer.
   */
  runAsDescription: string;
  /**
   * What each of the rule's actions needs from the run-as account, as the API reported it
   * for this rule. Kept on the draft rather than fetched separately so the sentence under
   * the picker and the sentence the engine will enforce come from one list.
   */
  actionPermissions: [string, string][];
};

const EMPTY_DRAFT: Draft = {
  id: null,
  name: "",
  description: "",
  enabled: true,
  event: "page.published",
  hookTriggered: false,
  onError: "stop",
  runAs: null,
  runAsDescription: "Runs as the rule's author",
  actionPermissions: [],
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
export function AutomationsView({ openId }: { openId?: string } = {}) {
  const { user } = useSession();
  const [catalogue, setCatalogue] = useState<AutomationCatalogue | null>(null);
  const [automations, setAutomations] = useState<Automation[] | null>(null);
  // A platform account (no primary organization) has to name one: an organization-scoped rule
  // belongs to a tenant, and the API refuses a nameless write. The IAM screens pick the first
  // organization the same way, so an administrator sees the same tenant everywhere.
  const [organizations, setOrganizations] = useState<string[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
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
  const [lastRun, setLastRun] = useState<string | null>(null);
  // A run started from this screen has to appear in the open run history without the
  // author going round the loop: the panel is a child that reads on mount, so a control
  // that starts a run and leaves the list unchanged is a control that looks broken.
  const [runsReloadToken, setRunsReloadToken] = useState(0);
  const [busy, setBusy] = useState<string | null>(null);

  // Which of the three operations tabs is open (REQ-003 slice 4). The default is `runs`,
  // because "what did this rule actually do?" is the question a person opens a rule to ask.
  const [operationsTab, setOperationsTab] = useState<OperationsTab>("runs");

  // The gates parked runs are waiting on (REQ-003 slice 3). `null` is "not loaded yet" and
  // is drawn as a loading strip, not as an empty panel — an empty panel and an unanswered
  // question look identical otherwise, and one of them is a lie.
  const [approvals, setApprovals] = useState<AutomationApproval[] | null>(null);
  const [approvalsError, setApprovalsError] = useState<string | null>(null);
  const [deciding, setDeciding] = useState<string | null>(null);
  // The accounts the run-as picker offers. Loaded while an editor is open and not before:
  // `/automations` is a screen a reader without `users.read` may still have to use, and a
  // failing account list must not become a red banner on a list screen. A picker with no
  // choices still says, in words, that the rule follows its author.
  const [runAsAccounts, setRunAsAccounts] = useState<{ id: string; label: string }[]>([]);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);
  const platformAccount = user ? user.organization_id === null : false;
  const organizationId = platformAccount ? selectedOrg : (user?.organization_id ?? null);
  // A platform account that has not chosen a tenant yet has nothing to show, and saying so is
  // better than showing an empty table that looks like "this tenant has no rules".
  const needsOrg = platformAccount && organizationId === null;
  const [loadingOrganizations, setLoadingOrganizations] = useState(false);

  useEffect(() => {
    fetchAutomationCatalogue()
      .then(setCatalogue)
      .catch(() => setCatalogue(null));
  }, []);

  // The pending gates. Re-read whenever the list reloads, because a decision made in
  // another tab has to disappear from this one — a queue that only shrinks on this screen's
  // own clicks is a queue that lies.
  useEffect(() => {
    if (!organizationId) {
      setApprovals([]);
      return;
    }
    let live = true;
    fetchApprovals({ organizationId })
      .then((answer) => {
        if (live) {
          setApprovals(answer.approvals);
          setApprovalsError(null);
        }
      })
      .catch((cause) => {
        if (!live) {
          return;
        }
        // A caller without `workflows.approve` sees nothing rather than a red banner: the
        // panel is not a page they can use, and an error about a power they do not have is
        // noise. The rule list below is still fully readable.
        setApprovals(
          cause instanceof ApiError && (cause.status === 403 || cause.status === 404)
            ? []
            : null,
        );
        setApprovalsError(
          cause instanceof ApiError && (cause.status === 403 || cause.status === 404)
            ? null
            : cause instanceof ApiError
              ? cause.message
              : "The pending approvals could not be loaded.",
        );
      });
    return () => {
      live = false;
    };
  }, [organizationId, reloadToken]);

  // The accounts, fetched when the editor opens and refreshed when the tenant changes.
  useEffect(() => {
    if (!draft || !organizationId) {
      setRunAsAccounts([]);
      return;
    }
    let live = true;
    fetchIamUsers({ organizationId, status: "active" })
      .then((answer) => {
        if (!live) {
          return;
        }
        setRunAsAccounts(
          answer.users.map((account) => ({
            id: account.id,
            label: `${account.display_name || account.email} — ${account.email}`,
          })),
        );
      })
      // A caller without `users.read` simply gets an author-follows rule: the picker keeps
      // its default option and says so, rather than the editor failing to open.
      .catch(() => {
        if (live) {
          setRunAsAccounts([]);
        }
      });
    return () => {
      live = false;
    };
  }, [draft !== null, organizationId]);

  /** Approve or reject one gate, then re-read the queue. */
  const decide = useCallback(
    async (gate: AutomationApproval, decision: "approved" | "rejected") => {
      setDeciding(gate.id);
      setNotice(null);
      setLoadError(null);
      try {
        const answer = await decideApproval(gate.id, decision);
        setNotice(
          decision === "approved"
            ? `“${gate.rule_name ?? "The rule"}” was approved — its run continues from step ${gate.step_no}.`
            : `“${gate.rule_name ?? "The rule"}” was rejected — its run ended at step ${gate.step_no} and nothing after it ran.`,
        );
        void answer;
        setReloadToken((token) => token + 1);
      } catch (cause) {
        setLoadError(
          cause instanceof ApiError
            ? cause.message
            : "That approval could not be decided.",
        );
      } finally {
        setDeciding(null);
      }
    },
    [],
  );

  useEffect(() => {
    if (!user) {
      return;
    }
    if (user.organization_id === null && organizations === null) {
      setLoadingOrganizations(true);
      fetchOrganizations()
        .then((list) => {
          setOrganizations(list.map((organization) => organization.id));
          setSelectedOrg(list[0]?.id ?? null);
        })
        .catch(() => setOrganizations([]))
        .finally(() => setLoadingOrganizations(false));
      return;
    }
    // A platform account with no organization cannot read any rule: the endpoint needs one to
    // scope the list to, and answering "all tenants' rules" to a platform account would leak
    // every tenant's automations into one list.
    if (needsOrg) {
      setAutomations([]);
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
  }, [user, organizationId, organizations, needsOrg, loadingOrganizations, reloadToken]);

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
    setLastRun(null);
    setPayloadText(JSON.stringify(SAMPLE_PAYLOAD, null, 2));
    setOperationsTab("runs");
    setDraft({ ...EMPTY_DRAFT, steps: EMPTY_DRAFT.steps.map((step) => ({ ...step })) });
  }, []);

  /** Open one rule for editing, and load its test surface. */
  const openEdit = useCallback(async (automation: Automation) => {
    setSaveError(null);
    setNotice(null);
    setReport(null);
    setHookUrl(null);
    setPayloadText(JSON.stringify(SAMPLE_PAYLOAD, null, 2));
    setOperationsTab("runs");
    setDraft({
      id: automation.id,
      name: automation.name,
      description: automation.description,
      enabled: automation.enabled,
      event: automation.event,
      hookTriggered: automation.trigger === "inbound_webhook",
      onError: automation.on_error ?? "stop",
      runAs: automation.run_as_user_id ?? null,
      runAsDescription: automation.run_as_description,
      actionPermissions: automation.action_permissions ?? [],
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

  /**
   * Re-open an editor from the server, addressed by id.
   *
   * A restore rewrites the rule underneath the open editor, and the open draft would then
   * hold a definition the panel no longer agrees with — so the caller re-reads the rule
   * rather than patching its own copy. Refusing a miss instead of throwing: the tab strip
   * calls this from a click handler, and a rejection there is an unhandled promise.
   */
  const openEditById = useCallback(
    async (automationId: string) => {
      try {
        openEdit(await fetchAutomation(automationId));
      } catch (cause) {
        setNotice(
          cause instanceof ApiError ? cause.message : "That rule could not be re-read.",
        );
      }
    },
    [openEdit],
  );

  // `/automations/[id]` opens that rule's editor as soon as the list has arrived. The id is
  // remembered so a later reload (a filter change, a save) does not fight the route.
  const appliedOpen = useRef<string | null>(null);
  useEffect(() => {
    if (!openId || !automations || appliedOpen.current === openId) {
      return;
    }
    const target = automations.find((automation) => automation.id === openId);
    if (target) {
      appliedOpen.current = openId;
      void openEdit(target);
    }
  }, [openId, automations, openEdit]);

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
    // A rule belongs to a tenant; a platform account that has not chosen one has nothing to
    // write, and saying so beats sending a request the API can only refuse.
    if (organizationId === null) {
      setSaveError("Choose an organization before saving a rule.");
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
      on_error: draft.onError,
      run_as_user_id: draft.runAs,
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

  /** Start one real run of a saved rule. */
  const runNow = useCallback(
    async (automationId: string) => {
      setBusy(automationId);
      setSaveError(null);
      setNotice(null);
      setReport(null);
      try {
        const started = await runAutomation(automationId);
        setNotice(
          `A run started with ${started.steps} step${started.steps === 1 ? "" : "s"}. Its actions really run — the dry run above is the simulation.`,
        );
        setLastRun(started.execution_id);
        setRunsReloadToken((token) => token + 1);
      } catch (cause) {
        setSaveError(
          cause instanceof ApiError ? cause.message : "The rule could not be run.",
        );
      } finally {
        setBusy(null);
      }
    },
    [],
  );

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
          on_error: automation.on_error,
          // Carried through rather than defaulted: arming or disarming a rule is a whole-rule
          // write, and one that dropped this would quietly move the rule off the service
          // account it was handed to — a change nobody notices until a run stops on a
          // permission error.
          run_as_user_id: automation.run_as_user_id ?? null,
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
          {/* The gallery is its own screen, so it needs a way in: a route nothing links to is
              a screen no person reaches and no pass can enter from the panel. */}
          <Link
            href="/automations/templates"
            data-automation-templates-link
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
          >
            <LayoutGrid className="h-3.5 w-3.5" aria-hidden="true" />
            Templates
          </Link>
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
          {/* A run that just started is the one thing an author wants to watch, so the
              notice links to its trace instead of making them find it in the list. */}
          {lastRun ? (
            <span className="ml-1 font-mono text-[11.5px] opacity-80">
              run {lastRun.slice(0, 8)}
            </span>
          ) : null}
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

      {/* The pending gates, above the table and only when something is waiting. Drawn
          apart from the rules on purpose: a parked run is not a rule, it is a question to a
          person, and burying it in a table of definitions is how an approval sits for three
          days. */}
      {approvalsError ? (
        <p
          data-automation-approvals-error
          role="alert"
          className="rounded-md bg-critical-soft px-3 py-2 text-[12.5px] text-critical"
        >
          {approvalsError}
        </p>
      ) : null}
      {approvals !== null && approvals.length > 0 ? (
        <section
          data-automation-approvals
          aria-label="Pending approvals"
          className="flex flex-col gap-2 rounded-lg border border-line bg-quiet-soft/40 p-3"
        >
          <div className="flex flex-wrap items-center gap-2">
            <ShieldCheck className="h-3.5 w-3.5" aria-hidden="true" />
            <h3 className="text-[13px] font-medium">
              {approvals.length} run{approvals.length === 1 ? "" : "s"} waiting on you
            </h3>
            <span className="text-[12px] text-muted">
              A run stays parked until somebody with the deciding permission answers.
            </span>
          </div>
          <ul className="flex flex-col gap-2" data-automation-approval-list>
            {approvals.map((gate) => (
              <li
                key={gate.id}
                data-automation-approval={gate.id}
                className="flex flex-col gap-2 rounded-md border border-line bg-paper p-2.5 sm:flex-row sm:items-start sm:justify-between"
              >
                <div className="min-w-0">
                  <p className="text-[12.5px] font-medium">
                    {gate.rule_name ?? "A rule"} · step {gate.step_no} — {gate.step_name}
                  </p>
                  <p className="text-[12px] text-muted">{gate.message}</p>
                  <p className="mt-0.5 text-[11.5px] text-muted">
                    Asked {formatTimestamp(gate.requested_at)}
                    {gate.expired
                      ? " · expired — this run will end without it"
                      : ` · decides by ${formatTimestamp(gate.expires_at)}`}
                    {` · needs ${gate.permission}`}
                  </p>
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <button
                    type="button"
                    data-automation-approval-approve={gate.id}
                    disabled={deciding === gate.id || gate.expired}
                    onClick={() => void decide(gate, "approved")}
                    className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] text-paper disabled:opacity-50"
                  >
                    <Check className="h-3.5 w-3.5" aria-hidden="true" />
                    Approve
                  </button>
                  <button
                    type="button"
                    data-automation-approval-reject={gate.id}
                    disabled={deciding === gate.id}
                    onClick={() => void decide(gate, "rejected")}
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-50"
                  >
                    <X className="h-3.5 w-3.5" aria-hidden="true" />
                    Reject
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>
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
            {platformAccount && organizations && organizations.length > 1 ? (
              <>
                <label className="sr-only" htmlFor="automation-organization">
                  Organization
                </label>
                <select
                  id="automation-organization"
                  data-automation-organization
                  value={organizationId ?? ""}
                  onChange={(event) => setSelectedOrg(event.target.value || null)}
                  className="rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
                >
                  {organizations.map((id) => (
                    <option key={id} value={id}>
                      {id}
                    </option>
                  ))}
                </select>
              </>
            ) : null}
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
              title={
                needsOrg
                  ? "Choose an organization"
                  : automations.length === 0
                    ? "No automations yet"
                    : "No rule matches these filters"
              }
              hint={
                needsOrg
                  ? "A platform account reads one organization's rules at a time — the rules of every tenant would not belong in one list."
                  : automations.length === 0
                    ? "A rule listens for an event, checks its conditions and runs its actions. Start with one on the event you care about."
                    : "Clear the search or the filters to see the rules again."
              }
              action={
                automations.length === 0 && !needsOrg ? (
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
          onRunNow={runNow}
          runsReloadToken={runsReloadToken}
          onRotateHook={rotateHook}
          onArmDelete={setConfirmDelete}
          runAsChoices={runAsAccounts}
          organizationId={organizationId}
          operationsTab={operationsTab}
          onOperationsTabChange={setOperationsTab}
          onReloadRule={() => {
            // A restore rewrites the rule underneath this editor, and the open draft would
            // then hold a definition the panel no longer agrees with — so the rule is
            // re-read from the server rather than patched from the panel's own copy.
            if (draft.id) {
              void openEditById(draft.id);
            }
            reload();
          }}
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
  onRunNow: (automationId: string) => void;
  /** Bumped when a run starts, so the open run history re-reads (REQ-003 slice 4). */
  runsReloadToken: number;
  onRotateHook: (automationId: string) => void;
  onArmDelete: (name: string) => void;
  /** The accounts of this tenant, for the run-as picker. */
  runAsChoices: { id: string; label: string }[];
  /** The organization the rule belongs to, for the account list. */
  organizationId: string | null;
  /** Which operations tab is open (REQ-003 slice 4). */
  operationsTab: OperationsTab;
  /** Open another one. */
  onOperationsTabChange: (tab: OperationsTab) => void;
  /**
   * Re-read the open rule from the server.
   *
   * Called after a restore: a restore rewrites the rule, and the open draft would otherwise
   * keep showing a definition the panel no longer agrees with — so the rule is re-read
   * rather than patched from the panel's own copy.
   */
  onReloadRule: () => void;
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
  onRunNow,
  runsReloadToken,
  onRotateHook,
  onArmDelete,
  runAsChoices,
  organizationId,
  operationsTab,
  onOperationsTabChange,
  onReloadRule,
}: EditorProps) {
  const eventFields = fieldsFor(catalogue, draft.event);
  const maxDepth = catalogue?.max_group_depth ?? 3;
  // What the actions below need from the run-as account, straight from the API's own map.
  // De-duplicated and in order, because a rule that sends an e-mail and publishes a page
  // needs one sentence, not three.
  const neededPermissions = useMemo(() => {
    const actions = new Set(
      draft.steps.map((step) => step.action).filter((action): action is string => Boolean(action)),
    );
    return draft.actionPermissions
      .filter(([action]) => actions.has(action))
      .map(([, permission]) => permission)
      .filter((permission, index, all) => all.indexOf(permission) === index);
  }, [draft.steps, draft.actionPermissions]);
  // The API's sentence describes the rule as *saved*. While the picker holds a different
  // value than the one that was loaded, that sentence is about the old state — so the
  // editor says what the picker now means, and falls back to the API's wording only when
  // the two agree. An unsaved rule has no API sentence at all, and the default is the one
  // true statement available.
  const runAsHelp = draft.runAs
    ? "Runs as the account chosen in this rule's settings, checked when each step runs."
    : draft.id
      ? draft.runAsDescription
      : "Runs as the rule's author, checked when each step runs.";
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
  // The per-step checks the server would refuse anyway, said here so the summary is
  // complete before a save is attempted. The server still checks: this is a courtesy,
  // never the authority.
  for (const [index, step] of draft.steps.entries()) {
    const label = step.name.trim() || `step ${index + 1}`;
    if (step.kind === "task" && !step.action) {
      problems.push(`“${label}” is an action with no action chosen.`);
    }
    if (step.kind === "task") {
      const timeout = step.timeout_ms ?? catalogue?.default_step_timeout_ms ?? 30_000;
      if (timeout < 1 || timeout > (catalogue?.max_step_timeout_ms ?? 120_000)) {
        problems.push(
          `“${label}” asks for ${timeout} ms; the engine takes 1 to ${catalogue?.max_step_timeout_ms ?? 120_000}.`,
        );
      }
    }
    if (step.kind === "branch") {
      const field = step.params?.field;
      const operator = step.params?.operator;
      if (typeof field !== "string" || !field.trim()) {
        problems.push(`“${label}” is a branch with no field to read.`);
      } else if (!field.startsWith("event.") && !field.startsWith("steps.")) {
        problems.push(
          `“${label}” reads ${field}, which is neither the event nor a step's output.`,
        );
      }
      if (typeof operator !== "string" || !(catalogue?.branch_operators ?? []).some((op) => op.key === operator)) {
        problems.push(`“${label}” compares with an operator this engine does not have.`);
      }
    }
    if (step.kind === "stop") {
      const reason = step.params?.reason;
      if (typeof reason !== "string" || !reason.trim()) {
        problems.push(`“${label}” stops the run without saying why.`);
      }
    }
    if (step.kind === "approval") {
      // A gate with no permission is a run that parks and nobody can open; a gate with a
      // permission that is not a key is a run that will never open either. Both are said
      // here, in the summary, before a save is attempted — the server refuses them too,
      // but an author who only learns on save cannot fix the rule in one pass.
      const permission = step.params?.permission;
      if (typeof permission !== "string" || !permission.trim()) {
        problems.push(`“${label}” waits for approval but names nobody who may decide.`);
      } else if (
        !permission
          .split(".")
          .every((segment) => segment.length > 0 && /^[a-z0-9_]+$/.test(segment))
      ) {
        problems.push(
          `“${label}” waits for ${permission}, which is not a permission key.`,
        );
      }
      const message = step.params?.message;
      if (typeof message !== "string" || !message.trim()) {
        problems.push(`“${label}” waits for approval with no message for the decider.`);
      }
      const hours = step.params?.expires_in_hours;
      const ceiling = catalogue?.max_approval_ttl_hours ?? 720;
      if (
        typeof hours !== "number" ||
        !Number.isInteger(hours) ||
        hours < 1 ||
        hours > ceiling
      ) {
        problems.push(
          `“${label}” waits ${String(hours)} hours; a gate may wait 1 to ${ceiling}.`,
        );
      }
    }
  }

  const setRoot = (next: AutomationGroup) => onDraftChange({ ...draft, conditions: next });

  /** Write one parameter of one step, leaving the rest of it alone. */
  const setStepParam = (index: number, key: string, value: unknown) => {
    const next = [...draft.steps];
    next[index] = { ...next[index], params: { ...next[index].params, [key]: value } };
    onDraftChange({ ...draft, steps: next });
  };

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
    // The new group is born **holding one row**. An empty group can never be saved (an empty
    // `any` never holds, and the layer refuses it), so a button that added a group the author
    // then had to find a way to fill — through a control that only appeared *inside* the new
    // group — was a control that produced an unsavable rule.
    const first = eventFields[0]?.key ?? "status";
    const group: AutomationGroup = {
      mode: "any",
      any: [{ field: first, operator: "equals", value: "" }],
    };
    updateRootNode(members(draft.conditions).length, group);
    setOpenGroup("root");
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
        {/* The rule's own policy, above the steps that inherit it. It is here rather than
            in a settings tab because it is the answer to the question a step's own
            "If it fails" control raises: "and if the step says use the rule's?" */}
        <div className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
          <label htmlFor="automation-on-error">When a step fails and inherits</label>
          <select
            id="automation-on-error"
            data-automation-on-error
            value={draft.onError}
            onChange={(event) =>
              onDraftChange({
                ...draft,
                onError: event.target.value as Automation["on_error"],
              })
            }
            className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
          >
            <option value="stop">Stop the run there</option>
            <option value="continue">Record it and go on</option>
          </select>
          <span>— each step can override this.</span>
        </div>

        {/* Whose authority the rule's actions run with (REQ-003 slice 3). This is the most
            consequential control on the screen and it reads like a settings line because
            that is what it is: the account whose permissions are resolved *when a step
            runs*, not a snapshot of what the author held when the rule was saved. The
            consequences are spelled out under it rather than left for somebody to find out
            from a failed run. */}
        <div className="flex flex-col gap-1 rounded-md border border-line p-2.5">
          <div className="flex flex-wrap items-center gap-2 text-[12px]">
            <label htmlFor="automation-run-as">Run this rule as</label>
            <select
              id="automation-run-as"
              data-automation-run-as
              value={draft.runAs ?? ""}
              onChange={(event) =>
                onDraftChange({ ...draft, runAs: event.target.value || null })
              }
              className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
            >
              <option value="">The rule&apos;s author (the default)</option>
              {runAsChoices.map((choice) => (
                <option key={choice.id} value={choice.id}>
                  {choice.label}
                </option>
              ))}
            </select>
            {runAsChoices.length === 0 ? (
              <span className="text-[11.5px] text-muted">
                Accounts cannot be listed here, so the rule follows its author.
              </span>
            ) : null}
          </div>
          <p className="text-[11.5px] text-muted" data-automation-run-as-help>
            {runAsHelp}
          </p>
          {draft.steps.some((step) => step.action) ? (
            <p className="text-[11.5px] text-muted">
              This rule&apos;s actions need:{" "}
              <span className="font-mono text-[11px]">
                {neededPermissions.join(", ")}
              </span>
              . If the account loses one, the run stops on that step and says which.
            </p>
          ) : null}
        </div>
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
                <label className="sr-only" htmlFor={`automation-step-kind-${index}`}>
                  Step {index + 1} kind
                </label>
                <select
                  id={`automation-step-kind-${index}`}
                  data-automation-step-kind={index}
                  value={step.kind}
                  onChange={(event) => {
                    const kind = event.target.value;
                    const next = [...draft.steps];
                    // Switching kind carries the parameters across only when they still
                    // make sense: a branch's `{field, operator, value}` is not a task's
                    // parameters, and silently keeping them would save a rule the engine
                    // refuses. Each kind therefore starts from its own shape.
                    next[index] = {
                      ...step,
                      kind,
                      action: kind === "task" ? (step.action ?? "send_email") : null,
                      params: defaultParamsFor(kind, step.params),
                    };
                    onDraftChange({ ...draft, steps: next });
                  }}
                  className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                >
                  {(catalogue?.step_kinds ?? ["task", "wait", "branch", "stop"]).map(
                    (kind) => (
                      <option key={kind} value={kind}>
                        {STEP_KIND_LABELS[kind] ?? kind}
                      </option>
                    ),
                  )}
                </select>
                {step.kind === "task" ? (
                  <>
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
                  </>
                ) : null}
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
              {/* The per-step failure policy and its time budget. A control step has
                  neither — a wait parks, a branch compares and a stop decides — so the
                  controls only appear for a task, which is the step that can fail at
                  all. */}
              {step.kind === "branch" ? (
                /* A branch's three fields are typed controls, not a JSON box: the whole
                   point of a branch is that it reads a *field*, and an author who has to
                   hand-write `{"field": "steps.2.ok"}` cannot tell a typo from a name
                   the run will produce. The step's own output keys are offered beside
                   the event's, because that is what a branch is for. */
                <div
                  className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted"
                  data-automation-branch={index}
                >
                  <label htmlFor={`automation-branch-field-${index}`}>End the run unless</label>
                  <input
                    id={`automation-branch-field-${index}`}
                    data-automation-branch-field={index}
                    list={`automation-branch-fields-${index}`}
                    value={String(step.params?.field ?? "")}
                    onChange={(event) => setStepParam(index, "field", event.target.value)}
                    placeholder="steps.2.ok"
                    className="w-44 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                  />
                  <datalist id={`automation-branch-fields-${index}`}>
                    {[...eventFields.map((field) => `event.${field.key}`), ...stepFieldHints(draft.steps, index)].map(
                      (key) => (
                        <option key={key} value={key} />
                      ),
                    )}
                  </datalist>
                  <select
                    aria-label={`Step ${index + 1} comparison`}
                    data-automation-branch-operator={index}
                    value={String(step.params?.operator ?? "equals")}
                    onChange={(event) => setStepParam(index, "operator", event.target.value)}
                    className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                  >
                    {(catalogue?.branch_operators ?? []).map((operator) => (
                      <option key={operator.key} value={operator.key}>
                        {operator.key}
                      </option>
                    ))}
                  </select>
                  {!["exists", "not_exists"].includes(String(step.params?.operator)) ? (
                    <>
                      <label htmlFor={`automation-branch-value-${index}`}>equals</label>
                      <input
                        id={`automation-branch-value-${index}`}
                        data-automation-branch-value={index}
                        value={String(step.params?.value ?? "")}
                        onChange={(event) => setStepParam(index, "value", event.target.value)}
                        className="w-40 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                      />
                    </>
                  ) : null}
                </div>
              ) : null}
              {step.kind === "stop" ? (
                <div
                  className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted"
                  data-automation-stop={index}
                >
                  <label htmlFor={`automation-stop-reason-${index}`}>Reason</label>
                  <input
                    id={`automation-stop-reason-${index}`}
                    data-automation-stop-reason={index}
                    value={String(step.params?.reason ?? "")}
                    onChange={(event) => setStepParam(index, "reason", event.target.value)}
                    placeholder="this run was stopped on purpose"
                    className="w-72 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                  />
                  <span>— shown in the run&apos;s trace.</span>
                </div>
              ) : null}
              {step.kind === "approval" ? (
                /* A gate's three parameters are typed controls, not a JSON box. The whole
                   point of the step is that a *person* reads the message and a *named
                   permission* opens it; an author who has to hand-write JSON to say that
                   is an author who will not put a gate in their rule. */
                <div
                  className="flex flex-col gap-2 text-[11.5px] text-muted"
                  data-automation-approval-step={index}
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <label htmlFor={`automation-approval-permission-${index}`}>
                      Who may decide
                    </label>
                    <input
                      id={`automation-approval-permission-${index}`}
                      data-automation-approval-permission={index}
                      value={String(step.params?.permission ?? "")}
                      onChange={(event) =>
                        setStepParam(index, "permission", event.target.value)
                      }
                      className="w-56 rounded-md border border-line bg-paper px-2 py-1 font-mono text-[12px]"
                    />
                    <span>
                      — a permission key, e.g.{" "}
                      <span className="font-mono">{catalogue?.approval_permission ?? "workflows.approve"}</span>
                      . The run stops until somebody who holds it answers.
                    </span>
                  </div>
                  <div className="flex flex-wrap items-center gap-2">
                    <label htmlFor={`automation-approval-message-${index}`}>
                      Message for the decider
                    </label>
                    <input
                      id={`automation-approval-message-${index}`}
                      data-automation-approval-message={index}
                      value={String(step.params?.message ?? "")}
                      onChange={(event) => setStepParam(index, "message", event.target.value)}
                      className="w-96 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                    />
                  </div>
                  <div className="flex flex-wrap items-center gap-2">
                    <label htmlFor={`automation-approval-ttl-${index}`}>
                      Decides within
                    </label>
                    <input
                      id={`automation-approval-ttl-${index}`}
                      data-automation-approval-ttl={index}
                      type="number"
                      min={1}
                      max={catalogue?.max_approval_ttl_hours ?? 720}
                      value={String(step.params?.expires_in_hours ?? "")}
                      onChange={(event) =>
                        setStepParam(index, "expires_in_hours", Number(event.target.value))
                      }
                      className="w-24 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                    />
                    <span>hours — after that the run ends without the step.</span>
                  </div>
                </div>
              ) : null}
              {step.kind === "task" ? (
                <div className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
                  <label htmlFor={`automation-step-on-error-${index}`}>
                    If it fails
                  </label>
                  <select
                    id={`automation-step-on-error-${index}`}
                    data-automation-step-on-error={index}
                    value={step.on_error ?? "inherit"}
                    onChange={(event) => {
                      const next = [...draft.steps];
                      next[index] = {
                        ...step,
                        on_error: event.target.value as AutomationStep["on_error"],
                      };
                      onDraftChange({ ...draft, steps: next });
                    }}
                    className="rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                  >
                    {(catalogue?.on_error_policies ?? ["inherit", "stop", "continue"]).map(
                      (policy) => (
                        <option key={policy} value={policy}>
                          {ON_ERROR_LABELS[policy] ?? policy}
                        </option>
                      ),
                    )}
                  </select>
                  <label htmlFor={`automation-step-timeout-${index}`}>
                    Give up after
                  </label>
                  <input
                    id={`automation-step-timeout-${index}`}
                    data-automation-step-timeout={index}
                    type="number"
                    min={1}
                    max={catalogue?.max_step_timeout_ms ?? 120_000}
                    step={1000}
                    value={step.timeout_ms ?? catalogue?.default_step_timeout_ms ?? 30_000}
                    onChange={(event) => {
                      const next = [...draft.steps];
                      next[index] = {
                        ...step,
                        timeout_ms: Number(event.target.value),
                      };
                      onDraftChange({ ...draft, steps: next });
                    }}
                    className="w-24 rounded-md border border-line bg-paper px-2 py-1 text-[12px]"
                  />
                  <span>ms</span>
                  {step.on_error === "continue" ? (
                    <span className="text-muted">
                      — a failure here is recorded and the run carries on.
                    </span>
                  ) : null}
                </div>
              ) : null}
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

      {/*
        The operations tabs (REQ-003 slice 4). Drawn only for a rule that already exists: a
        rule being written has no run history, no versions and no audit rows, and three tabs
        that are all empty on a brand-new rule read as three broken features rather than as
        a rule that has not been saved yet.
      */}
      {draft.id ? (
        <section className="flex flex-col gap-2 border-t border-line pt-3">
          <div role="tablist" aria-label="Rule operations" className="flex flex-wrap gap-1">
            {OPERATIONS_TABS.map((tab) => (
              <button
                key={tab.key}
                type="button"
                role="tab"
                aria-selected={operationsTab === tab.key}
                data-automation-tab={tab.key}
                onClick={() => onOperationsTabChange(tab.key)}
                className={`rounded-md px-2.5 py-1 text-[12px] ${
                  operationsTab === tab.key
                    ? "bg-ink text-paper"
                    : "border border-line text-muted hover:text-ink"
                }`}
              >
                {tab.label}
              </button>
            ))}
          </div>
          {operationsTab === "runs" ? (
            <RunsPanel automationId={draft.id} reloadToken={runsReloadToken} />
          ) : null}
          {operationsTab === "versions" ? (
            <VersionsPanel automationId={draft.id} onRestored={onReloadRule} />
          ) : null}
          {operationsTab === "audit" ? <AuditPanel automationId={draft.id} /> : null}
        </section>
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
          <>
            {/* The one control that touches the world, so it says so. The dry run above
                it is the simulation; this sends, publishes and calls for real. */}
            <button
              type="button"
              data-automation-run-now
              disabled={busy === draft.id}
              onClick={() => onRunNow(draft.id as string)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-60"
            >
              <Play aria-hidden="true" className="h-3.5 w-3.5" />
              Run now
            </button>
            <button
              type="button"
              data-automation-delete
              onClick={() => onArmDelete(draft.name)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
            >
              <Trash2 className="h-3.5 w-3.5" aria-hidden="true" />
              Delete
            </button>
          </>
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

      {depth < maxDepth - 1 || depth === 1 ? (
        <button
          type="button"
          data-automation-group-add-condition={depth}
          onClick={() => {
            const first = fields[0]?.key ?? "status";
            const added: AutomationCondition = { field: first, operator: "equals", value: "" };
            const next = [...rows, added];
            onChange({ mode: group.mode ?? "all", all: next, any: next });
          }}
          className="w-fit rounded-md border border-line px-2 py-1 text-[11.5px] hover:bg-quiet-soft"
        >
          <Plus className="h-3 w-3" aria-hidden="true" /> Add condition here
        </button>
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

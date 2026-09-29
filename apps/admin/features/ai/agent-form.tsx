"use client";

/**
 * The agent config form (docs/requests/REQ-099, slice 1).
 *
 * Create and edit share one component because a form with two implementations drifts: a limit
 * that was tightened in one and not the other is a limit that lies in exactly the screen where
 * somebody is about to spend money.
 *
 * Three rules, each one a way a config form quietly misconfigures an agent:
 *
 * 1. **The API's message lands under its field.** The route answers `agent.invalid_key`,
 *    `agent.invalid_max_steps` and friends with a sentence that names the limit; the form maps
 *    the code to the field and shows the sentence there. A form that shows every refusal in a
 *    banner at the top is a form where a reader fixes the wrong field.
 * 2. **The key is immutable after create.** It appears in URLs and in workflow node
 *    configuration, so changing it after a run exists would break both. The field is rendered
 *    read-only with the reason stated, not hidden — a missing field reads as a bug.
 * 3. **A model that cannot call tools is not offered once the tool list is non-empty.** The
 *    runtime refuses it, and an agent with tools pinned to a model without tool support is an
 *    agent whose every run ends in a provider error.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { ArrowLeft, Loader2, Save } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiAgent,
  type AiModel,
  createAiAgent,
  fetchAiAgent,
  fetchAiModels,
  updateAiAgent,
} from "@/lib/api";
import { useSession } from "@/lib/session";

import { RunSheet } from "./run-sheet";
import { AgentWorkspace } from "./agent-workspace";

/** The memory scopes the column accepts; the route refuses anything else. */
const MEMORY_SCOPES = [
  ["none", "None"],
  ["organization", "Organization"],
  ["site", "Site"],
  ["user", "User"],
] as const;

/** The limits the route validates, quoted once so the form and the route cannot disagree. */
const LIMITS = {
  name: 80,
  description: 400,
  systemPrompt: 8000,
  minSteps: 1,
  maxSteps: 50,
  minDeadline: 30,
  maxDeadline: 3600,
  minTokens: 1000,
  maxTokens: 2_000_000,
  keyMax: 64,
} as const;

/** Which field a refusal code belongs to. Anything unmapped shows in the form banner. */
const FIELD_OF_CODE: Record<string, string> = {
  "agent.invalid": "name",
  "agent.invalid_key": "key",
  "agent.invalid_name": "name",
  "agent.invalid_description": "description",
  "agent.invalid_system_prompt": "system_prompt",
  "agent.invalid_temperature": "temperature",
  "agent.invalid_max_steps": "max_steps",
  "agent.invalid_deadline": "deadline_seconds",
  "agent.invalid_token_budget": "token_budget",
  "agent.invalid_memory_scope": "memory_scope",
  "agent.approval_not_allowed": "tools",
};

/** One field's error line, so every field renders the same shape. */
function FieldError({ message }: { message?: string }) {
  if (!message) return null;
  return (
    <p data-agent-field-error className="mt-1 text-[11.5px] text-danger">
      {message}
    </p>
  );
}

/** A labelled control with a counter and its own error slot. */
function Field({
  label,
  htmlFor,
  hint,
  counter,
  error,
  children,
}: {
  label: string;
  htmlFor: string;
  hint?: string;
  counter?: string;
  error?: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <label htmlFor={htmlFor} className="block text-[12.5px] font-medium">
        {label}
      </label>
      {hint ? <p className="mt-0.5 text-[11.5px] text-muted">{hint}</p> : null}
      <div className="mt-1">{children}</div>
      {counter ? <p className="mt-1 text-right text-[11px] text-muted">{counter}</p> : null}
      <FieldError message={error} />
    </div>
  );
}

const inputClass =
  "w-full rounded-lg border border-line bg-canvas px-2.5 py-2 text-[13px] outline-none focus:border-accent disabled:opacity-60";

type FormState = {
  key: string;
  name: string;
  description: string;
  system_prompt: string;
  model_id: string;
  temperature: string;
  max_steps: string;
  deadline_seconds: string;
  token_budget: string;
  tools: string;
  approvals: string;
  memory_scope: string;
  enabled: boolean;
};

const BLANK: FormState = {
  key: "",
  name: "",
  description: "",
  system_prompt: "",
  model_id: "",
  temperature: "0.20",
  max_steps: "8",
  deadline_seconds: "300",
  token_budget: "200000",
  tools: "",
  approvals: "",
  memory_scope: "none",
  enabled: true,
};

type AgentFormProps = {
  /** The agent to edit; absent means create. */
  agentId?: string;
};

export function AgentForm({ agentId }: AgentFormProps) {
  const router = useRouter();
  const { user } = useSession();
  const organizationId = user?.organization_id ?? undefined;

  const [form, setForm] = useState<FormState>(BLANK);
  const [agent, setAgent] = useState<AiAgent | null>(null);
  const [models, setModels] = useState<AiModel[]>([]);
  const [loading, setLoading] = useState(Boolean(agentId));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [running, setRunning] = useState(false);
  const [toolInput, setToolInput] = useState("");
  /**
   * Which tab is open.
   *
   * The tabs exist only on an existing agent: `Config · Skills · Runs · Workspace` on the create
   * screen would be three tabs that list nothing, and a tab that lies about what exists is worse
   * than a tab that is not there. Skills and Runs arrive with slice 3; Workspace is slice 2 and
   * is the fourth entry below.
   */
  const [tab, setTab] = useState<"config" | "skills" | "runs" | "workspace">("config");
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        // The model list is needed twice: to pin one, and to hide the ones that cannot call
        // tools once the tool list is not empty. A failed read is not a broken form — the pin
        // can stay empty and the router chooses a model per run.
        const list = await fetchAiModels({ status: "enabled" });
        if (!cancelled) setModels(list);
      } catch {
        /* the model select simply stays empty */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!agentId) return;
    let cancelled = false;
    void (async () => {
      setLoading(true);
      try {
        const loaded = await fetchAiAgent(agentId, organizationId);
        if (cancelled) return;
        setAgent(loaded);
        setForm({
          key: loaded.key,
          name: loaded.name,
          description: loaded.description,
          system_prompt: loaded.system_prompt,
          model_id: loaded.model_id ?? "",
          temperature: loaded.temperature.toFixed(2),
          max_steps: String(loaded.max_steps),
          deadline_seconds: String(loaded.deadline_seconds),
          token_budget: String(loaded.token_budget),
          tools: loaded.tools.join("\n"),
          approvals: loaded.approvals.join("\n"),
          memory_scope: loaded.memory_scope,
          enabled: loaded.enabled,
        });
      } catch (caught) {
        if (!cancelled) {
          setError(caught instanceof ApiError ? caught.message : "The agent could not be loaded.");
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId, organizationId]);

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) => {
    setForm((previous) => ({ ...previous, [key]: value }));
    setFieldErrors((previous) => {
      if (!(key in previous)) return previous;
      const next = { ...previous };
      delete next[key];
      return next;
    });
  };

  const toolList = useMemo(
    () => form.tools.split("\n").map((line) => line.trim()).filter(Boolean),
    [form.tools],
  );
  const approvalList = useMemo(
    () => form.approvals.split("\n").map((line) => line.trim()).filter(Boolean),
    [form.approvals],
  );

  /** A model that cannot call tools is not offered once the agent has any. */
  const modelOptions = useMemo(
    () => (toolList.length === 0 ? models : models.filter((model) => model.supports_tools)),
    [models, toolList.length],
  );

  const addTool = () => {
    const key = toolInput.trim();
    if (!key || toolList.includes(key)) {
      setToolInput("");
      return;
    }
    set("tools", [...toolList, key].join("\n"));
    setToolInput("");
  };

  const removeTool = (key: string) => {
    set("tools", toolList.filter((entry) => entry !== key).join("\n"));
    set("approvals", approvalList.filter((entry) => entry !== key).join("\n"));
  };

  const toggleApproval = (key: string) => {
    set(
      "approvals",
      approvalList.includes(key)
        ? approvalList.filter((entry) => entry !== key).join("\n")
        : [...approvalList, key].join("\n"),
    );
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    setFieldErrors({});
    const payload = {
      name: form.name.trim(),
      description: form.description,
      system_prompt: form.system_prompt,
      // `null` un-pins; the form's empty select is "let the router choose", not "no model".
      model_id: form.model_id === "" ? null : form.model_id,
      temperature: Number(form.temperature),
      max_steps: Number(form.max_steps),
      deadline_seconds: Number(form.deadline_seconds),
      token_budget: Number(form.token_budget),
      tools: toolList,
      approvals: approvalList,
      memory_scope: form.memory_scope,
      enabled: form.enabled,
    };
    try {
      if (agentId) {
        const saved = await updateAiAgent(agentId, { organizationId, ...payload });
        setAgent(saved);
        setNotice("The agent was saved.");
      } else {
        const created = await createAiAgent({ organizationId, key: form.key.trim(), ...payload });
        router.push(`/ai/agents/${created.id}`);
        return;
      }
    } catch (caught) {
      const failure = caught as ApiError;
      const field = FIELD_OF_CODE[failure.code];
      if (field) setFieldErrors((previous) => ({ ...previous, [field]: failure.message }));
      setError(failure.message);
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return <LoadingTable columns={2} rows={3} />;
  }

  return (
    <div data-agent-form className="space-y-4">
      <Link
        href="/ai/agents"
        className="inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-ink"
      >
        <ArrowLeft className="size-3.5" aria-hidden />
        Back to agents
      </Link>

      {/* The tab bar, only on an existing agent. Config and Workspace are real now; Skills and
          Runs arrive with slice 3 and are not rendered until they are, because a tab that
          opens onto nothing is a tab that lies. */}
      {agentId ? (
        <div role="tablist" aria-label="Agent sections" className="flex gap-1 overflow-x-auto border-b border-line">
          {([
            ["config", "Config"],
            ["workspace", "Workspace"],
          ] as const).map(([key, label]) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={tab === key}
              data-agent-tab={key}
              onClick={() => setTab(key)}
              className={`-mb-px whitespace-nowrap border-b-2 px-3 py-2 text-[12.5px] font-medium ${
                tab === key ? "border-accent text-ink" : "border-transparent text-muted hover:text-ink"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      ) : null}

      {tab === "workspace" && agentId ? (
        <AgentWorkspace agentId={agentId} organizationId={organizationId} />
      ) : null}

      {error && !Object.keys(fieldErrors).length ? (
        <p data-agent-form-error role="alert" className="rounded-xl border border-danger/40 bg-danger/5 px-3.5 py-3 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p data-agent-form-notice className="text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      {/* The config body. On a saved agent it renders only while the Config tab is open, so the
          Workspace tab is not stacked underneath a form the reader did not ask for. On create
          there is no tab bar and `tab` is always `config`. */}
      <div className={tab === "config" ? "space-y-4" : "hidden"}>
      <div className="grid gap-4 lg:grid-cols-2">
        <div className="space-y-3">
          <Field
            label="Name"
            htmlFor="agent-name"
            hint="What a person calls this agent."
            counter={`${form.name.length} / ${LIMITS.name}`}
            error={fieldErrors.name}
          >
            <input
              id="agent-name"
              ref={nameRef}
              data-agent-field="name"
              value={form.name}
              maxLength={LIMITS.name + 20}
              onChange={(event) => set("name", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field
            label="Key"
            htmlFor="agent-key"
            hint={
              agentId
                ? "Immutable: the key appears in URLs and in workflow node configuration."
                : "Starts with a lowercase letter; then lowercase letters, digits, _ or -."
            }
            counter={`${form.key.length} / ${LIMITS.keyMax}`}
            error={fieldErrors.key}
          >
            <input
              id="agent-key"
              data-agent-field="key"
              value={form.key}
              disabled={Boolean(agentId)}
              maxLength={LIMITS.keyMax + 20}
              onChange={(event) => set("key", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field
            label="Description"
            htmlFor="agent-description"
            counter={`${form.description.length} / ${LIMITS.description}`}
            error={fieldErrors.description}
          >
            <textarea
              id="agent-description"
              data-agent-field="description"
              rows={2}
              value={form.description}
              onChange={(event) => set("description", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field
            label="Model"
            htmlFor="agent-model"
            hint={
              toolList.length > 0
                ? "Only models that can call tools are listed, because the agent has tools."
                : "Leave empty to let the router choose a model per task."
            }
            error={fieldErrors.model_id}
          >
            <select
              id="agent-model"
              data-agent-field="model"
              value={form.model_id}
              onChange={(event) => set("model_id", event.target.value)}
              className={inputClass}
            >
              <option value="">Routed (no pin)</option>
              {modelOptions.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.display_name} — {model.provider_name}
                </option>
              ))}
            </select>
          </Field>

          <Field label="Temperature" htmlFor="agent-temperature" error={fieldErrors.temperature}>
            <input
              id="agent-temperature"
              data-agent-field="temperature"
              type="number"
              min={0}
              max={1}
              step={0.05}
              value={form.temperature}
              onChange={(event) => set("temperature", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field
            label="Max steps"
            htmlFor="agent-max-steps"
            hint={`${LIMITS.minSteps}–${LIMITS.maxSteps}; the runtime stops a run here, not the prompt.`}
            error={fieldErrors.max_steps}
          >
            <input
              id="agent-max-steps"
              data-agent-field="max_steps"
              type="number"
              min={LIMITS.minSteps}
              max={LIMITS.maxSteps}
              value={form.max_steps}
              onChange={(event) => set("max_steps", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field
            label="Deadline (seconds)"
            htmlFor="agent-deadline"
            hint={`${LIMITS.minDeadline}–${LIMITS.maxDeadline}.`}
            error={fieldErrors.deadline_seconds}
          >
            <input
              id="agent-deadline"
              data-agent-field="deadline_seconds"
              type="number"
              min={LIMITS.minDeadline}
              max={LIMITS.maxDeadline}
              value={form.deadline_seconds}
              onChange={(event) => set("deadline_seconds", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field
            label="Token budget"
            htmlFor="agent-token-budget"
            hint={`${LIMITS.minTokens.toLocaleString()}–${LIMITS.maxTokens.toLocaleString()} per run.`}
            error={fieldErrors.token_budget}
          >
            <input
              id="agent-token-budget"
              data-agent-field="token_budget"
              type="number"
              min={LIMITS.minTokens}
              max={LIMITS.maxTokens}
              step={1000}
              value={form.token_budget}
              onChange={(event) => set("token_budget", event.target.value)}
              className={inputClass}
            />
          </Field>

          <Field label="Memory scope" htmlFor="agent-memory" error={fieldErrors.memory_scope}>
            <select
              id="agent-memory"
              data-agent-field="memory_scope"
              value={form.memory_scope}
              onChange={(event) => set("memory_scope", event.target.value)}
              className={inputClass}
            >
              {MEMORY_SCOPES.map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </Field>
        </div>

        <div className="space-y-3">
          <Field
            label="System prompt"
            htmlFor="agent-system-prompt"
            hint="Frames every run. Untrusted content inside a run is delimited and never treated as instructions."
            counter={`${form.system_prompt.length} / ${LIMITS.systemPrompt}`}
            error={fieldErrors.system_prompt}
          >
            <textarea
              id="agent-system-prompt"
              data-agent-field="system_prompt"
              rows={8}
              value={form.system_prompt}
              onChange={(event) => set("system_prompt", event.target.value)}
              className={`${inputClass} font-mono text-[12.5px]`}
            />
          </Field>

          <Field
            label="Tools"
            htmlFor="agent-tool-input"
            hint="One tool key per line. Only an allow-listed tool can run; a denied tool is refused, not hidden."
            error={fieldErrors.tools}
          >
            <div className="flex gap-1.5">
              <input
                id="agent-tool-input"
                data-agent-tool-input
                value={toolInput}
                onChange={(event) => setToolInput(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    addTool();
                  }
                }}
                placeholder="page.search"
                className={inputClass}
              />
              <button
                type="button"
                data-agent-tool-add
                onClick={addTool}
                className="shrink-0 rounded-lg border border-line px-2.5 text-[12.5px]"
              >
                Add
              </button>
            </div>
            {toolList.length > 0 ? (
              <ul data-agent-tool-list className="mt-2 space-y-1">
                {toolList.map((key) => (
                  <li
                    key={key}
                    data-agent-tool={key}
                    className="flex items-center gap-2 rounded-lg border border-line px-2 py-1 text-[12.5px]"
                  >
                    <code className="flex-1 font-mono">{key}</code>
                    <label className="inline-flex items-center gap-1 text-[11.5px] text-muted">
                      <input
                        type="checkbox"
                        data-agent-approval={key}
                        checked={approvalList.includes(key)}
                        onChange={() => toggleApproval(key)}
                      />
                      needs approval
                    </label>
                    <button
                      type="button"
                      data-agent-tool-remove={key}
                      onClick={() => removeTool(key)}
                      className="text-[11.5px] text-muted hover:text-danger"
                    >
                      Remove
                    </button>
                  </li>
                ))}
              </ul>
            ) : (
              <p data-agent-tools-empty className="mt-2 text-[11.5px] text-muted">
                No tools: the agent can only answer in text.
              </p>
            )}
          </Field>

          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              data-agent-field="enabled"
              checked={form.enabled}
              onChange={(event) => set("enabled", event.target.checked)}
            />
            Enabled — a disabled agent refuses to start a run.
          </label>
        </div>
      </div>

      <div className="flex items-center gap-2">
        <button
          type="button"
          data-agent-save
          disabled={saving}
          onClick={() => void save()}
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[13px] font-medium text-white disabled:opacity-50"
        >
          {saving ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Save className="size-3.5" aria-hidden />}
          {agentId ? "Save" : "Create agent"}
        </button>
        {agent ? (
          <button
            type="button"
            data-agent-form-run
            disabled={!agent.enabled}
            onClick={() => setRunning(true)}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-40"
          >
            Run it
          </button>
        ) : null}
        <Link href="/ai/agents" className="ml-auto text-[12.5px] text-muted hover:text-ink">
          Cancel
        </Link>
      </div>
      </div>

      {running && agent ? (
        <RunSheet
          agent={agent}
          organizationId={organizationId}
          onClose={() => setRunning(false)}
          onOpenRun={useCallback((runId: string) => router.push(`/ai/runs/${runId}`), [router])}
        />
      ) : null}
    </div>
  );
}

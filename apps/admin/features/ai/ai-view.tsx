"use client";

/**
 * AI Hub screen (docs/06-AI-HUB.md, P11).
 *
 * Three things, in the order an operator needs them: the providers this installation connected
 * (with the key write-only — the panel shows whether one is stored, never its value), the model
 * registry those providers fill, and a chat to try the whole chain — router, provider, stream —
 * before anything else on the platform builds on it.
 *
 * The chat's prompt can arrive prefilled from the palette's `Ask AI` row (`?q=…`), which is how a
 * search that found nothing becomes a question.
 */
import { useCallback, useEffect, useState } from "react";

import {
  ArrowRight,
  CircleCheck,
  CircleSlash,
  ClipboardList,
  Download,
  Loader2,
  Plus,
  Power,
  Send,
  Star,
  Stethoscope,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiCapability,
  type AiDiscoveryReport,
  type AiHealthStatus,
  type AiModel,
  type AiProtocol,
  type AiProtocolBounds,
  type AiProvider,
  type AiProviderKind,
  type AiTestReport,
  type AiTestStep,
  type ChatProposal,
  applyAiProviderDiscovery,
  connectAiProvider,
  discoverAiProviderModels,
  fetchAiModels,
  fetchChatProposalInstruction,
  fetchAiProtocols,
  fetchAiProviders,
  removeAiProvider,
  replaceAiProviderModels,
  streamChat,
  testAiProvider,
  updateAiModel,
  updateAiProvider,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

import { AiHealthPanels } from "./ai-health-panel";
import { AiDecisionLogScreen } from "./ai-decision-log";
import { AiRoutingScreen } from "./ai-routing";
import { ModelCatalog } from "./model-catalog";

/**
 * The PATCH body for one capability toggle.
 *
 * The panel's toggle names are the API's wire names — the catalog ships with the model, so this
 * is a lookup rather than a second list that could fall behind the crate.
 */
function capabilityPatch(
  capability: AiCapability,
  on: boolean,
): Parameters<typeof updateAiModel>[1] {
  switch (capability) {
    case "streaming":
      return { supportsStreaming: on };
    case "tools":
      return { supportsTools: on };
    case "vision":
      return { supportsVision: on };
    case "json_mode":
      return { supportsJsonMode: on };
    case "embeddings":
      return { supportsEmbeddings: on };
    case "image_generation":
      return { supportsImageGeneration: on };
    case "audio_generation":
      return { supportsAudioGeneration: on };
    case "transcription":
      return { supportsTranscription: on };
    // `chat` is true for every registered model and `list_models` is a fact about the endpoint.
    // The editor renders both read-only, and this is the guard that keeps a future caller from
    // sending a PATCH the server would have to ignore silently.
    case "chat":
    case "list_models":
      return {};
  }
}

/** One capability pill under a model. */
function Flag({ label, on }: { label: string; on: boolean }) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-1.5 py-0.5 text-[10.5px] font-medium ${
        on ? "bg-accent-soft text-accent-strong" : "bg-quiet-soft text-muted"
      }`}
    >
      {label}
    </span>
  );
}

/** Health dot + label: the pairing has to be readable without relying on colour alone. */
function HealthDot({ status }: { status: AiHealthStatus }) {
  const tone: Record<AiHealthStatus, string> = {
    ok: "bg-positive",
    degraded: "bg-caution",
    down: "bg-danger",
    unknown: "bg-quiet",
  };
  return (
    <span className="inline-flex items-center gap-1.5 text-[11.5px] text-muted">
      <span
        data-health-dot={status}
        className={`size-2 shrink-0 rounded-full ${tone[status]}`}
        aria-hidden
      />
      {status === "unknown" ? "Never probed" : status[0].toUpperCase() + status.slice(1)}
    </span>
  );
}

/** The five steps of the connection test, each with its own verdict. */
/**
 * The capability editor of one model.
 *
 * The catalog comes from the API with the model, so this component never compiles a flag list of
 * its own: a capability added to the crate appears here with its own note, and `editable` says
 * which toggles are the model's to claim. Every toggle writes through immediately, because a
 * flag the panel shows and a flag the router reads are the same column — an operator who
 * switched vision off must be able to see the effect in the very next request, not after a
 * second Save.
 */
function CapabilityEditor({
  model,
  onToggle,
  disabled,
}: {
  model: AiModel;
  onToggle: (capability: AiCapability, on: boolean) => void;
  disabled: boolean;
}) {
  const claimed = new Set(model.capabilities);

  return (
    <div
      data-capability-editor={model.model_id}
      className="mt-2 flex flex-col gap-1.5 rounded-lg border border-line bg-canvas px-2.5 py-2"
    >
      <p className="text-[11px] text-muted">
        What this model can do. The router refuses a request that needs a flag switched off here,
        before any call leaves the platform.
      </p>
      <div className="grid gap-x-4 gap-y-1 sm:grid-cols-2">
        {model.capability_catalog.map((entry) => {
          const on = claimed.has(entry.capability);
          return (
            <label
              key={entry.capability}
              title={entry.note}
              className="flex items-start gap-2 text-[11.5px]"
            >
              <input
                type="checkbox"
                checked={on}
                disabled={disabled || !entry.editable}
                onChange={(event) => onToggle(entry.capability, event.target.checked)}
                data-capability-flag={`${model.model_id}:${entry.capability}`}
                className="mt-0.5 size-3.5 shrink-0 accent-[var(--color-accent)] disabled:opacity-50"
              />
              <span className={entry.editable ? "" : "text-muted"}>
                {entry.capability.replace(/_/g, " ")}
                {entry.editable ? null : (
                  <span className="ml-1 text-[10.5px] uppercase tracking-wide text-muted">
                    (endpoint)
                  </span>
                )}
              </span>
            </label>
          );
        })}
      </div>
    </div>
  );
}

/**
 * What a discovery run found, as a reviewable diff.
 *
 * The apply button is disabled on an empty diff and says so, rather than being a live button
 * that writes nothing: the second run after an apply is exactly the case where the operator
 * needs to be told "already up to date" instead of being shown a button that does not work.
 */
function DiscoveryDiff({
  report,
  busy,
  onApply,
}: {
  report: AiDiscoveryReport;
  busy: boolean;
  onApply: () => void;
}) {
  const tone: Record<AiDiscoveryReport["lines"][number]["action"], string> = {
    added: "bg-accent-soft text-accent-strong",
    changed: "bg-caution-soft text-caution",
    removed: "bg-danger-soft text-danger",
  };

  return (
    <div
      data-discovery-diff={report.provider_name}
      className="mt-2 flex flex-col gap-2 rounded-lg border border-line bg-canvas px-2.5 py-2"
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[12px] font-medium">Discovery</span>
        <span data-discovery-counts className="text-[11.5px] text-muted">
          {report.added} to add · {report.changed} to change · {report.removed} to remove ·{" "}
          {report.reported_count} reported, {report.stored_count} stored
        </span>
        <button
          type="button"
          disabled={busy || report.up_to_date}
          onClick={onApply}
          data-discovery-apply={report.provider_name}
          className="ml-auto rounded-lg bg-accent px-2.5 py-1 text-[11.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
        >
          {report.up_to_date ? "Already up to date" : "Apply this diff"}
        </button>
      </div>

      {report.up_to_date ? (
        <p data-discovery-uptodate className="text-[11.5px] text-muted">
          The provider serves exactly what the registry holds. Nothing to apply.
        </p>
      ) : (
        <ul className="flex flex-col gap-1">
          {report.lines.map((line) => (
            <li
              key={`${line.action}:${line.model_key}`}
              data-discovery-line={`${line.action}:${line.model_key}`}
              className="flex flex-wrap items-center gap-2 text-[11.5px]"
            >
              <span
                className={`rounded-full px-2 py-0.5 text-[10.5px] font-medium ${tone[line.action]}`}
              >
                {line.action}
              </span>
              <span className="font-mono">{line.model_key}</span>
              {line.changed_fields.length > 0 ? (
                <span className="text-muted">({line.changed_fields.join(", ")})</span>
              ) : null}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function TestSteps({ steps }: { steps: AiTestStep[] }) {
  const icon = (status: AiTestStep["status"]) => {
    if (status === "ok") {
      return <CircleCheck className="size-4 shrink-0 text-positive" aria-hidden />;
    }
    if (status === "failed") {
      return <TriangleAlert className="size-4 shrink-0 text-danger" aria-hidden />;
    }
    if (status === "skipped") {
      return <CircleSlash className="size-4 shrink-0 text-muted" aria-hidden />;
    }
    return <Loader2 className="size-4 shrink-0 animate-spin text-muted" aria-hidden />;
  };

  return (
    <ul data-test-steps className="flex flex-col divide-y divide-[var(--color-line)] rounded-lg border border-line">
      {steps.map((step) => (
        <li key={step.step} data-test-step={step.step} className="flex flex-col gap-1 px-3 py-2.5">
          <div className="flex items-center gap-2">
            {icon(step.status)}
            <span className="text-[12.5px] font-medium">{step.label}</span>
            <span className="ml-auto text-[11.5px] text-muted">
              {step.status === "skipped"
                ? "not applicable"
                : step.status === "pending"
                  ? "not run"
                  : `${step.latency_ms} ms`}
            </span>
          </div>
          {step.error ? (
            <p data-test-error={step.step} className="text-[11.5px] text-danger">
              {step.error}
            </p>
          ) : null}
          {step.note ? <p className="text-[11.5px] text-muted">{step.note}</p> : null}
        </li>
      ))}
    </ul>
  );
}

/** The connection-test modal: the five steps, the total, and the provider's own failure text. */
function TestModal({
  report,
  running,
  error,
  onClose,
}: {
  report: AiTestReport | null;
  running: boolean;
  error: string | null;
  onClose: () => void;
}) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4">
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Connection test"
        data-test-modal
        className="flex max-h-[90vh] w-full max-w-md flex-col gap-3 overflow-y-auto rounded-xl border border-line bg-surface p-4"
      >
        <header className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-[14px] font-semibold">Connection test</h2>
            <p className="text-[12px] text-muted">
              {report ? report.provider_name : "Dialing the provider…"}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close the connection test"
            className="rounded-lg border border-line p-1.5 transition hover:bg-canvas"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </header>

        {running ? (
          <p className="flex items-center gap-2 text-[12.5px] text-muted">
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Running the five steps against the stored connection…
          </p>
        ) : null}

        {error ? (
          <p
            role="alert"
            data-test-modal-error
            className="rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px]"
          >
            {error}
          </p>
        ) : null}

        {report ? (
          <>
            <p
              data-test-summary
              data-failing-step={report.failing_step ?? ""}
              className="text-[12.5px]"
            >
              {report.summary}
              <span className="text-muted">
                {" "}
                · {report.protocol} · {report.total_ms} ms in total
              </span>
            </p>
            <TestSteps steps={report.steps} />
          </>
        ) : null}
      </div>
    </div>
  );
}

/** The AI Hub screen. */
export function AiView() {
  const searchParams = useSearchParams();
  const [providers, setProviders] = useState<AiProvider[] | null>(null);
  const [models, setModels] = useState<AiModel[] | null>(null);
  // A *failed* load is not a pending one. `null` means "still loading" and renders the skeleton;
  // parking a rejection there left the screen shimmering for ever with a banner nobody could act
  // on. The failure of each list is held separately so a provider-list outage does not also
  // blank the model registry beside it — the two settle independently above.
  const [listError, setListError] = useState<{ providers: string | null; models: string | null }>({
    providers: null,
    models: null,
  });
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  // The provider vocabulary the form is built from. A failed load must not silently leave the
  // select empty, so the fallback below is the protocol the platform has always spoken.
  const [protocols, setProtocols] = useState<AiProtocol[] | null>(null);
  const [bounds, setBounds] = useState<AiProtocolBounds>({
    timeout_ms_min: 1000,
    timeout_ms_max: 120000,
    max_retries_max: 5,
    priority_min: 1,
    priority_max: 1000,
  });

  // The connect form. `fieldError` keeps the API's own message under the field it belongs to,
  // which is the difference between "it did not save" and "the timeout must be 1000–120000".
  const [showForm, setShowForm] = useState(false);
  const [name, setName] = useState("");
  const [protocol, setProtocol] = useState("openai_compatible");
  const [kind, setKind] = useState<AiProviderKind>("cloud");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [timeoutMs, setTimeoutMs] = useState("30000");
  const [maxRetries, setMaxRetries] = useState("1");
  const [priority, setPriority] = useState("100");
  const [modelKeys, setModelKeys] = useState("");
  const [makeDefault, setMakeDefault] = useState(false);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);

  // The connection test.
  const [testing, setTesting] = useState<{ id: string; name: string } | null>(null);
  const [testReport, setTestReport] = useState<AiTestReport | null>(null);
  const [testRunning, setTestRunning] = useState(false);
  const [testError, setTestError] = useState<string | null>(null);

  // The model editor of one provider.
  const [editing, setEditing] = useState<string | null>(null);
  const [editingKeys, setEditingKeys] = useState("");

  // Slice 2 (REQ-097): the capability editor and the discovery diff. Both are per-model and
  // per-provider, so both are keyed by the row they belong to rather than living in one global
  // piece of state — a diff from one provider must never appear under another.
  const [flagsOpen, setFlagsOpen] = useState<string | null>(null);
  const [discovery, setDiscovery] = useState<Record<string, AiDiscoveryReport>>({});

  // Slice 3 (REQ-097): which of the Health / Usage / Failover panels is open. One at a time and
  // one shared across rows — an operator comparing two providers' health wants the same tab on
  // both, and two open sparklines stacked in a list is just noise.
  const [panel, setPanel] = useState<"health" | "usage" | "failover" | null>(null);

  // The chat.
  const [chatModel, setChatModel] = useState("");
  /** A change set the answer proposed and the platform filed (REQ-101, slice 3g). */
  const [chatProposal, setChatProposal] = useState<ChatProposal | null>(null);
  /**
   * The instruction that makes proposals possible, read from the API once.
   *
   * `null` means it has not arrived, and `send()` refuses rather than sending the question
   * alone: a chat with no instruction still answers, and the panel would look completely
   * healthy while never once filing a set. Failing loudly is the better of two quiet options.
   */
  const [proposalInstruction, setProposalInstruction] = useState<string | null>(null);
  /** Why the instruction is missing, so the panel says so rather than offering a dead Send. */
  const [proposalInstructionError, setProposalInstructionError] = useState<string | null>(null);
  /**
   * A proposal the platform could not file. Kept beside the answer rather than raised as an
   * error: the answer is complete and on screen, and the one thing the reader must be told is
   * that the changes they were shown do not exist anywhere a person could review them.
   */
  const [chatProposalError, setChatProposalError] = useState<string | null>(null);
  // A prompt handed over by the palette's `Ask AI` row arrives in the URL; the reader still
  // presses Send themselves — nothing is asked on their behalf.
  const [prompt, setPrompt] = useState(() => searchParams.get("q") ?? "");
  const [answer, setAnswer] = useState("");
  const [chatRoute, setChatRoute] = useState<string | null>(null);
  const [chatUsage, setChatUsage] = useState<string | null>(null);
  const [streaming, setStreaming] = useState(false);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  // The proposal instruction, read once and kept. It cannot change while the build is the same
  // build, so it is not on `reloadToken` — putting it there would refetch it on every provider
  // save for a constant, and the failure mode of *that* is a chat that stops being able to
  // propose because somebody renamed a provider.
  useEffect(() => {
    let cancelled = false;
    fetchChatProposalInstruction()
      .then((payload) => {
        if (!cancelled) {
          setProposalInstruction(payload.instruction);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setProposalInstruction(null);
          setProposalInstructionError(
            cause instanceof ApiError
              ? cause.message
              : "The proposal instruction could not be read.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    setListError({ providers: null, models: null });

    // The three calls settle independently on purpose: a provider list that fails must not take
    // the model registry and the form's protocol vocabulary down with it.
    Promise.allSettled([fetchAiProviders(), fetchAiModels(), fetchAiProtocols()])
      .then((results) => {
        if (cancelled) {
          return;
        }

        const [providersResult, modelsResult, protocolsResult] = results;
        const reasonFor = (result: PromiseSettledResult<unknown>, fallback: string) =>
          result.status === "rejected"
            ? result.reason instanceof ApiError
              ? result.reason.message
              : fallback
            : null;

        const providersFailed = reasonFor(providersResult, "The providers could not be read.");
        const modelsFailed = reasonFor(modelsResult, "The model registry could not be read.");

        // A rejection lands on the *error* slot, never back on the loading one. `null` here would
        // be read as "still loading" and the skeleton would never resolve.
        setProviders(providersFailed ? [] : (providersResult as PromiseFulfilledResult<AiProvider[]>).value);
        setModels(modelsFailed ? [] : (modelsResult as PromiseFulfilledResult<AiModel[]>).value);
        setListError({ providers: providersFailed, models: modelsFailed });
        if (protocolsResult.status === "fulfilled") {
          setProtocols(protocolsResult.value.protocols);
          setBounds(protocolsResult.value.bounds);
        }
      });

    return () => {
      cancelled = true;
    };
  }, [reloadToken]);

  /** Run one mutation, then refresh and report what happened. */
  const run = useCallback(
    async (action: () => Promise<unknown>, message: string) => {
      setBusy(true);
      setError(null);
      setNotice(null);
      try {
        await action();
        setNotice(message);
        reload();
      } catch (cause: unknown) {
        setError(cause instanceof ApiError ? cause.message : "The action did not go through.");
      } finally {
        setBusy(false);
      }
    },
    [reload],
  );

  /** Which form field an API refusal belongs to, from the wording of its message. */
  const fieldOf = (message: string): string => {
    const lowered = message.toLowerCase();
    if (lowered.includes("protocol")) return "protocol";
    if (lowered.includes("kind")) return "kind";
    if (lowered.includes("base url") || lowered.includes("base_url")) return "baseUrl";
    if (lowered.includes("timeout")) return "timeoutMs";
    if (lowered.includes("retries")) return "maxRetries";
    if (lowered.includes("priority")) return "priority";
    return "name";
  };

  const fieldMessage = (field: string) =>
    fieldError?.field === field ? fieldError.message : null;

  /** The models a textarea describes: one key per line. */
  const keysOf = (value: string): { key: string }[] =>
    value
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean)
      .map((key) => ({ key }));

  const connect = async () => {
    setFieldError(null);
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await connectAiProvider({
        name,
        baseUrl,
        protocol,
        kind,
        apiKey,
        timeoutMs: Number(timeoutMs),
        maxRetries: Number(maxRetries),
        priority: Number(priority),
        isDefault: makeDefault,
        models: keysOf(modelKeys),
      });
      setNotice(`${name.trim()} connected. Test it to see the five steps.`);
      setName("");
      setBaseUrl("");
      setApiKey("");
      setModelKeys("");
      setMakeDefault(false);
      setShowForm(false);
      reload();
    } catch (cause: unknown) {
      // The API answers which field it refused; that message belongs under that field.
      const message =
        cause instanceof ApiError ? cause.message : "The provider could not be connected.";
      setFieldError({ field: fieldOf(message), message });
      setError(message);
    } finally {
      setBusy(false);
    }
  };

  /**
   * Write one capability flag and say so.
   *
   * The change lands in the same column the router reads, so the notice names the capability
   * rather than a generic "saved" — an operator needs to know that the next request will now be
   * refused (or no longer refused) because of this exact flag.
   */
  const setCapability = async (model: AiModel, capability: AiCapability, on: boolean) => {
    await run(
      () => updateAiModel(model.id, capabilityPatch(capability, on)),
      `${model.model_key}: ${capability.replace(/_/g, " ")} ${on ? "enabled" : "disabled"}.`,
    );
  };

  const runDiscovery = async (provider: AiProvider) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const found = await discoverAiProviderModels(provider.id);
      setDiscovery((current) => ({ ...current, [provider.id]: found }));
      setNotice(
        found.up_to_date
          ? `${provider.name} serves exactly what the registry holds.`
          : `${provider.name}: ${found.added} to add, ${found.changed} to change, ${found.removed} to remove.`,
      );
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The provider could not be asked.");
    } finally {
      setBusy(false);
    }
  };

  const applyDiscovery = async (provider: AiProvider) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const applied = await applyAiProviderDiscovery(provider.id);
      // The diff that comes back is the one that *was* applied; re-read so the panel shows the
      // state after the write rather than the request that caused it.
      const after = await discoverAiProviderModels(provider.id);
      setDiscovery((current) => ({ ...current, [provider.id]: after }));
      // The **model list** has to be re-read too, not only the diff. Discovery writes rows the
      // registry table renders, and until that list is refetched the panel announces "applied 2
      // added" over an empty table — the write succeeded and the screen says nothing happened.
      // The walk found it by applying a diff and then looking for the model row the notice had
      // just promised: `data-capability-editor` was 0 because the rows it hangs off do not exist
      // in the list, not because the editor is broken.
      const models = await fetchAiModels();
      setModels(models);
      setNotice(
        applied.up_to_date
          ? `${provider.name} was already up to date.`
          : `${provider.name}: applied ${applied.added} added, ${applied.removed} removed.`,
      );
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError ? cause.message : "The discovery diff could not be applied.",
      );
    } finally {
      setBusy(false);
    }
  };

  const runTest = async (id: string, providerName: string) => {
    setTesting({ id, name: providerName });
    setTestReport(null);
    setTestError(null);
    setTestRunning(true);
    try {
      setTestReport(await testAiProvider(id));
      reload();
    } catch (cause: unknown) {
      setTestError(
        cause instanceof ApiError ? cause.message : "The test could not be run.",
      );
    } finally {
      setTestRunning(false);
    }
  };

  const closeTest = () => {
    setTesting(null);
    setTestReport(null);
    setTestError(null);
  };

  const send = async () => {
    if (!prompt.trim() || streaming) {
      return;
    }
    // The instruction is not optional. Without it the model is never told the format, so it
    // never proposes, and every walk a reviewer could do on this screen would report the
    // feature working. Say so instead.
    if (!proposalInstruction) {
      setError(
        "The proposal instruction could not be read, so a prompt sent now could not propose changes.",
      );
      return;
    }
    setStreaming(true);
    setError(null);
    setNotice(null);
    setAnswer("");
    setChatRoute(null);
    setChatUsage(null);
    setChatProposal(null);
    setChatProposalError(null);

    try {
      await streamChat(
        {
          model: chatModel || undefined,
          // The instruction is sent, not merely hoped for. Slice 3g's parser reads a fenced
          // `change-set` block, and a model that was never told the format will never write one
          // — the screen would then work perfectly and never once file a set, which is a
          // feature that looks built and does nothing. The text is the same sentence the API
          // documents in `proposal::system_instruction`, and `GET /ai/chat` is the one place
          // the two can be compared.
          messages: [
            ...(proposalInstruction
              ? [{ role: "system" as const, content: proposalInstruction }]
              : []),
            { role: "user" as const, content: prompt },
          ],
        },
        {
          onStart: (info) => setChatRoute(`${info.provider} · ${info.model} · ${info.protocol}`),
          onDelta: (content) => setAnswer((current) => current + content),
          onProposal: (filed) => setChatProposal(filed),
          onProposalError: (message) => setChatProposalError(message),
          onDone: (done) => {
            const usage = done.usage?.total_tokens;
            setChatUsage(
              usage
                ? `${done.chars} characters · ${usage} tokens · ${done.finish_reason ?? "done"}`
                : `${done.chars} characters · ${done.finish_reason ?? "done"}`,
            );
          },
        },
      );
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The chat did not go through.");
    } finally {
      setStreaming(false);
    }
  };

  const enabledModels = (models ?? []).filter((model) => model.enabled);

  // The select is driven by the API's own list; until it answers (or if it does not) the form
  // still offers the protocol v0 shipped, so the screen is never a dead control.
  const protocolOptions: AiProtocol[] = protocols ?? [
    {
      protocol: "openai_compatible",
      note: "Chat completions with a bearer key; the default for local servers.",
      chat_path: "/chat/completions",
      auth: "Authorization: Bearer with the stored key",
    },
    {
      protocol: "anthropic_messages",
      note: "The messages wire shape: a system block outside the conversation, token counts under input/output names.",
      chat_path: "/messages",
      auth: "x-api-key plus an anthropic-version header",
    },
    {
      protocol: "google_gemini",
      note: "generateContent: contents with roles user/model, usage under usageMetadata.",
      chat_path: "/models/{model}:generateContent",
      auth: "x-goog-api-key",
    },
  ];
  const selectedProtocol = protocolOptions.find((option) => option.protocol === protocol);

  return (
    <div className="flex flex-col gap-6">
      {error ? (
        <p
          role="alert"
          className="rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px]"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}

      {/* Providers */}
      <section className="rounded-xl border border-line bg-surface">
        <header className="flex items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div>
            <h2 className="text-[13.5px] font-semibold">Providers</h2>
            <p className="text-[12px] text-muted">
              Any service that speaks the OpenAI-compatible protocol — a hosted API or one on your
              own network.
            </p>
          </div>
          <button
            type="button"
            onClick={() => setShowForm((open) => !open)}
            className="flex shrink-0 items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            <Plus className="size-3.5" aria-hidden />
            Connect provider
          </button>
        </header>

        {showForm ? (
          <form
            className="flex flex-col gap-3 border-b border-line px-4 py-4"
            onSubmit={(event) => {
              event.preventDefault();
              void connect();
            }}
          >
            <div className="grid gap-3 sm:grid-cols-2">
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Name</span>
                <input
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                  placeholder="e.g. Office AI"
                  required
                  maxLength={64}
                  data-provider-name
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                {fieldMessage("name") ? (
                  <span data-field-error="name" className="text-[11px] text-danger">
                    {fieldMessage("name")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Protocol</span>
                <select
                  value={protocol}
                  onChange={(event) => setProtocol(event.target.value)}
                  data-provider-protocol
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                >
                  {protocolOptions.map((option) => (
                    <option key={option.protocol} value={option.protocol}>
                      {option.protocol}
                    </option>
                  ))}
                </select>
                <span className="text-[11px] text-muted">{selectedProtocol?.note}</span>
                {fieldMessage("protocol") ? (
                  <span data-field-error="protocol" className="text-[11px] text-danger">
                    {fieldMessage("protocol")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Kind</span>
                <select
                  value={kind}
                  onChange={(event) => setKind(event.target.value as AiProviderKind)}
                  data-provider-kind
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                >
                  <option value="cloud">Cloud</option>
                  <option value="local">Local</option>
                </select>
                <span className="text-[11px] text-muted">
                  {kind === "local"
                    ? "Runs on a machine you control — no key is usually needed."
                    : "A hosted API that authenticates with a key."}
                </span>
                {fieldMessage("kind") ? (
                  <span data-field-error="kind" className="text-[11px] text-danger">
                    {fieldMessage("kind")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Base URL</span>
                <input
                  value={baseUrl}
                  onChange={(event) => setBaseUrl(event.target.value)}
                  placeholder="https://api.example.com/v1"
                  required
                  data-provider-url
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                {fieldMessage("baseUrl") ? (
                  <span data-field-error="baseUrl" className="text-[11px] text-danger">
                    {fieldMessage("baseUrl")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">API key</span>
                <input
                  value={apiKey}
                  onChange={(event) => setApiKey(event.target.value)}
                  type="password"
                  autoComplete="off"
                  placeholder="Leave empty for a local endpoint"
                  data-provider-key
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                <span className="text-[11px] text-muted">
                  Stored for the platform&apos;s own calls; never shown again.
                </span>
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Timeout (ms)</span>
                <input
                  value={timeoutMs}
                  onChange={(event) => setTimeoutMs(event.target.value)}
                  type="number"
                  inputMode="numeric"
                  min={bounds.timeout_ms_min}
                  max={bounds.timeout_ms_max}
                  data-provider-timeout
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                <span className="text-[11px] text-muted">
                  {bounds.timeout_ms_min.toLocaleString("en-US")}–
                  {bounds.timeout_ms_max.toLocaleString("en-US")} ms for one call.
                </span>
                {fieldMessage("timeoutMs") ? (
                  <span data-field-error="timeoutMs" className="text-[11px] text-danger">
                    {fieldMessage("timeoutMs")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Max retries</span>
                <input
                  value={maxRetries}
                  onChange={(event) => setMaxRetries(event.target.value)}
                  type="number"
                  inputMode="numeric"
                  min={0}
                  max={bounds.max_retries_max}
                  data-provider-retries
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                <span className="text-[11px] text-muted">
                  0–{bounds.max_retries_max}, only before the first streamed byte.
                </span>
                {fieldMessage("maxRetries") ? (
                  <span data-field-error="maxRetries" className="text-[11px] text-danger">
                    {fieldMessage("maxRetries")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Priority</span>
                <input
                  value={priority}
                  onChange={(event) => setPriority(event.target.value)}
                  type="number"
                  inputMode="numeric"
                  min={bounds.priority_min}
                  max={bounds.priority_max}
                  data-provider-priority
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                <span className="text-[11px] text-muted">
                  {bounds.priority_min}–{bounds.priority_max}; lower is asked first when a call
                  fails over.
                </span>
                {fieldMessage("priority") ? (
                  <span data-field-error="priority" className="text-[11px] text-danger">
                    {fieldMessage("priority")}
                  </span>
                ) : null}
              </label>
              <label className="flex flex-col gap-1 sm:col-span-2">
                <span className="text-[12px] font-medium">Models</span>
                <textarea
                  value={modelKeys}
                  onChange={(event) => setModelKeys(event.target.value)}
                  rows={3}
                  placeholder={"One model key per line\ngpt-4o-mini\ntext-embedding-3-small"}
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
              </label>
            </div>
            <label className="flex items-center gap-2 text-[12.5px]">
              <input
                type="checkbox"
                checked={makeDefault}
                onChange={(event) => setMakeDefault(event.target.checked)}
                className="size-3.5 accent-[var(--color-accent)]"
              />
              Make this the default provider
            </label>
            <div className="flex items-center gap-2">
              <button
                type="submit"
                disabled={busy}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
              >
                Connect
              </button>
              <button
                type="button"
                onClick={() => setShowForm(false)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Cancel
              </button>
            </div>
          </form>
        ) : null}

        {providers === null ? (
          <LoadingTable columns={4} rows={2} />
        ) : listError.providers ? (
          // An outage is not an empty installation. "No provider is connected yet" would send an
          // operator to connect one that is already there, so the failure keeps its own wording and
          // the button is a retry rather than an invitation to add.
          <div className="flex flex-col items-center gap-2 px-6 py-10 text-center" data-providers-error>
            <p className="text-[13.5px] font-medium">The providers could not be loaded</p>
            <p className="max-w-sm text-[12.5px] text-muted">{listError.providers}</p>
            <button
              type="button"
              onClick={reload}
              className="mt-2 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              data-providers-retry
            >
              Try again
            </button>
          </div>
        ) : providers.length === 0 ? (
          <EmptyState
            title="No provider is connected yet"
            hint="Connect an OpenAI-compatible endpoint, give it the models it serves, and the platform can talk to it."
            action={
              <button
                type="button"
                onClick={() => setShowForm(true)}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                data-empty-connect
              >
                Connect a provider
              </button>
            }
          />
        ) : (
          <ul className="divide-y divide-[var(--color-line)]">
            {providers.map((provider) => (
              <li key={provider.id} className="flex flex-col gap-3 px-4 py-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-[13px] font-medium">{provider.name}</span>
                  {provider.is_default ? (
                    <span className="inline-flex items-center gap-1 rounded-full bg-accent-soft px-2 py-0.5 text-[10.5px] font-medium text-accent-strong">
                      <Star className="size-3" aria-hidden />
                      Default
                    </span>
                  ) : null}
                  <span
                    className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                      provider.enabled
                        ? "bg-positive-soft text-positive"
                        : "bg-quiet-soft text-muted"
                    }`}
                  >
                    {provider.enabled ? "Enabled" : "Disabled"}
                  </span>
                  <span
                    className="inline-flex items-center rounded-full bg-quiet-soft px-2 py-0.5 text-[10.5px] font-medium text-muted"
                    data-provider-kind-badge={provider.name}
                  >
                    {provider.kind === "local" ? "Local" : "Cloud"}
                  </span>
                  <span className="text-[11.5px] text-muted">
                    {provider.model_count} model{provider.model_count === 1 ? "" : "s"}
                  </span>
                  <span className="text-[11.5px] text-muted">
                    {provider.has_api_key ? "Key stored" : "No key"}
                  </span>
                  <HealthDot status={provider.last_health} />
                  <span className="ml-auto flex flex-wrap items-center gap-1.5">
                    <button
                      type="button"
                      disabled={busy || testRunning}
                      onClick={() => void runTest(provider.id, provider.name)}
                      data-provider-test={provider.name}
                      className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                    >
                      <Stethoscope className="size-3" aria-hidden />
                      Test
                    </button>
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() =>
                        void run(
                          () =>
                            updateAiProvider(provider.id, { enabled: !provider.enabled }),
                          provider.enabled
                            ? `${provider.name} disabled.`
                            : `${provider.name} enabled.`,
                        )
                      }
                      data-provider-toggle={provider.name}
                      className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                    >
                      <Power className="size-3" aria-hidden />
                      {provider.enabled ? "Disable" : "Enable"}
                    </button>
                    {provider.is_default ? null : (
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() =>
                          void run(
                            () => updateAiProvider(provider.id, { isDefault: true }),
                            `${provider.name} is now the default provider.`,
                          )
                        }
                        data-provider-default={provider.name}
                        className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                      >
                        <Star className="size-3" aria-hidden />
                        Make default
                      </button>
                    )}
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => {
                        setEditing(editing === provider.id ? null : provider.id);
                        setEditingKeys(
                          (models ?? [])
                            .filter((model) => model.provider_id === provider.id)
                            .map((model) => model.model_key)
                            .join("\n"),
                        );
                      }}
                      data-provider-models={provider.name}
                      className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                    >
                      Models
                    </button>
                    {/* The default provider is not removable, and the button says so instead of
                        failing: a control that refuses with a reason is a control the operator
                        can act on, a dead one is not. The API refuses it too — this only spares
                        the round trip and says which provider to move first. */}
                    {provider.is_default ? (
                      <span className="flex flex-col items-start gap-1">
                        <button
                          type="button"
                          disabled
                          data-provider-remove-guard={provider.name}
                          title="Make another provider the default before removing this one."
                          className="flex cursor-not-allowed items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted opacity-70"
                        >
                          <Trash2 className="size-3" aria-hidden />
                          Remove
                        </button>
                        <span className="max-w-[13rem] text-[11px] text-muted">
                          This is the default. Make another provider the default first.
                        </span>
                      </span>
                    ) : (
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => {
                          if (
                            window.confirm(
                              `Remove ${provider.name} and the models it serves? AI features that point at it stop working.`,
                            )
                          ) {
                            void run(
                              () => removeAiProvider(provider.id),
                              `${provider.name} removed.`,
                            );
                          }
                        }}
                        data-provider-remove={provider.name}
                        className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution transition hover:bg-caution-soft disabled:opacity-60"
                      >
                        <Trash2 className="size-3" aria-hidden />
                        Remove
                      </button>
                    )}
                  </span>
                </div>
                <p className="font-mono text-[11.5px] break-all text-muted">{provider.base_url}</p>
                {provider.last_error ? (
                  <p data-provider-error={provider.name} className="text-[11.5px] text-danger">
                    {provider.last_error}
                  </p>
                ) : null}

                {editing === provider.id ? (
                  <div className="flex flex-col gap-2 rounded-lg border border-line bg-canvas p-3">
                    <label className="flex flex-col gap-1">
                      <span className="text-[12px] font-medium">
                        Models served by {provider.name}
                      </span>
                      <textarea
                        value={editingKeys}
                        onChange={(event) => setEditingKeys(event.target.value)}
                        rows={4}
                        className="rounded-lg border border-line bg-surface px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                      />
                      <span className="text-[11px] text-muted">
                        One model key per line — the identifiers this provider knows.
                      </span>
                    </label>
                    <div className="flex flex-wrap items-center gap-2">
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() =>
                          void run(
                            () =>
                              replaceAiProviderModels(provider.id, keysOf(editingKeys)),
                            `${provider.name}: the model list is saved.`,
                          )
                        }
                        className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
                      >
                        Save models
                      </button>
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => void runDiscovery(provider)}
                        data-provider-discover={provider.name}
                        className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-surface disabled:opacity-60"
                      >
                        <Download className="size-3.5" aria-hidden />
                        Discover from provider
                      </button>
                    </div>
                  </div>
                ) : null}
                {discovery[provider.id] ? (
                  <DiscoveryDiff
                    report={discovery[provider.id]}
                    busy={busy}
                    onApply={() => void applyDiscovery(provider)}
                  />
                ) : null}
                <AiHealthPanels
                  provider={provider}
                  openPanel={panel}
                  onToggle={(next) => setPanel(panel === next ? null : next)}
                />
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* Models */}
      <section id="ai-provider-models" className="scroll-mt-4 rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Models</h2>
          <p className="text-[12px] text-muted">
            The registry the router picks from. The default model answers when a request names
            none.
          </p>
        </header>

        {models === null ? (
          <LoadingTable columns={4} rows={2} />
        ) : listError.models ? (
          <div className="flex flex-col items-center gap-2 px-6 py-10 text-center" data-models-error>
            <p className="text-[13.5px] font-medium">The model registry could not be loaded</p>
            <p className="max-w-sm text-[12.5px] text-muted">{listError.models}</p>
            <button
              type="button"
              onClick={reload}
              className="mt-2 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              data-models-retry
            >
              Try again
            </button>
          </div>
        ) : models.length === 0 ? (
          <EmptyState
            title="No model is registered"
            hint="Add models to a provider — or pull them from the provider itself with Discover."
            action={
              <button
                type="button"
                onClick={() => setShowForm(true)}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                data-empty-model
              >
                Add a model
              </button>
            }
          />
        ) : (
          <ul className="divide-y divide-[var(--color-line)]">
            {models.map((model) => (
              <li
                key={model.id}
                data-model-row={model.model_id}
                className="flex flex-col gap-0 px-4 py-2.5"
              >
                <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-[12.5px]">{model.model_key}</span>
                <span className="text-[11.5px] text-muted">{model.provider_name}</span>
                {model.is_default ? (
                  <span className="inline-flex items-center gap-1 rounded-full bg-accent-soft px-2 py-0.5 text-[10.5px] font-medium text-accent-strong">
                    <Star className="size-3" aria-hidden />
                    Default
                  </span>
                ) : null}
                {model.capabilities
                  .filter((capability) => capability !== "chat")
                  .map((capability) => (
                    <Flag
                      key={capability}
                      label={capability.replace(/_/g, " ")}
                      on
                    />
                  ))}
                {model.context_window ? (
                  <span className="text-[11px] text-muted">
                    {model.context_window.toLocaleString("en-US")} ctx
                  </span>
                ) : null}
                {model.max_output_tokens ? (
                  <span className="text-[11px] text-muted">
                    max {model.max_output_tokens.toLocaleString("en-US")}
                  </span>
                ) : null}
                <button
                  type="button"
                  disabled={busy}
                  aria-expanded={flagsOpen === model.model_id}
                  onClick={() =>
                    setFlagsOpen((open) => (open === model.model_id ? null : model.model_id))
                  }
                  data-model-flags={model.model_id}
                  className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                >
                  {flagsOpen === model.model_id ? "Close flags" : "Capabilities"}
                </button>
                <span className="ml-auto flex items-center gap-1.5">
                  {model.enabled && !model.is_default ? (
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() =>
                        void run(
                          () => updateAiModel(model.id, { isDefault: true }),
                          `${model.model_key} is now the default model.`,
                        )
                      }
                      data-model-default={model.model_id}
                      className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                    >
                      Make default
                    </button>
                  ) : null}
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() =>
                      void run(
                        () => updateAiModel(model.id, { enabled: !model.enabled }),
                        model.enabled
                          ? `${model.model_key} disabled.`
                          : `${model.model_key} enabled.`,
                      )
                    }
                    data-model-toggle={model.model_id}
                    className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                  >
                    {model.enabled ? "Disable" : "Enable"}
                  </button>
                </span>
                </div>
                {flagsOpen === model.model_id ? (
                  <CapabilityEditor
                    model={model}
                    disabled={busy}
                    onToggle={(capability, on) => void setCapability(model, capability, on)}
                  />
                ) : null}
                {discovery[model.provider_id] ? (
                  <DiscoveryDiff
                    report={discovery[model.provider_id]}
                    busy={busy}
                    onApply={() => {
                      const owner = providers?.find(
                        (provider) => provider.id === model.provider_id,
                      );
                      if (owner) void applyDiscovery(owner);
                    }}
                  />
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* Catalog (REQ-098 slice 1). It sits *below* the per-provider model list rather than
          replacing it: the list above is how you edit which models a provider serves, and the
          catalog is how you read the whole registry — what everything costs, what it can do and
          what will ever ask for it. Merging them would make a table cell a place to type a model
          key, and a list is a bad place to compare prices. */}
      <section className="rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Model catalog</h2>
          <p className="text-[12px] text-muted">
            Every registered model with its capabilities and what it costs. A price is an
            operator&apos;s estimate unless the provider reported it; a price nobody has revisited
            in three months says so.
          </p>
        </header>
        {models === null || providers === null ? (
          <LoadingTable columns={6} rows={3} />
        ) : (
          <ModelCatalog providers={providers} models={models} onReload={reload} />
        )}
      </section>

      {/* Routing (REQ-098 slice 2). It sits below the catalog on purpose: the catalog says what a
          model can do, and routing is only meaningful once there is something to route to. The
          section is its own, not a tab, because "which model answers a cheap request" is a
          question an operator asks next to "what does the cheap model cost". */}
      <section className="rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Routing</h2>
          <p className="text-[12px] text-muted">
            Which model answers each task, and why. A candidate that is switched off or cannot
            claim a requirement is shown with its reason rather than quietly dropped.
          </p>
        </header>
        <div className="px-4 py-4">
          {models === null ? (
            <LoadingTable columns={4} rows={3} />
          ) : (
            <AiRoutingScreen models={models} />
          )}
        </div>
      </section>

      {/* The decision log (REQ-098 slice 3) sits directly under routing because it is the other
          half of the same question: routing says what *would* answer, the log says what *did*.
          Putting them apart on separate pages would make an operator hold one in their head
          while reading the other. */}
      <section className="rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Decision log</h2>
          <p className="text-[12px] text-muted">
            What answered each request, and why — including the candidates that were skipped and
            the fallbacks that had to take over.
          </p>
        </header>
        <div className="px-4 py-4">
          <AiDecisionLogScreen />
        </div>
      </section>

      {/* Try it */}
      <section className="rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Try it</h2>
          <p className="text-[12px] text-muted">
            One prompt through the whole chain: the router picks the model, the provider answers,
            the platform streams it back.
          </p>
        </header>
        <div className="flex flex-col gap-3 px-4 py-4">
          <label className="flex flex-col gap-1 sm:max-w-sm">
            <span className="text-[12px] font-medium">Model</span>
            <select
              value={chatModel}
              onChange={(event) => setChatModel(event.target.value)}
              data-chat-model
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="">Default model</option>
              {enabledModels.map((model) => (
                <option key={model.id} value={model.model_id}>
                  {model.model_id}
                  {model.is_default ? " (default)" : ""}
                </option>
              ))}
            </select>
          </label>
          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium">Prompt</span>
            <textarea
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              rows={3}
              data-chat-prompt
              placeholder="Say hello in one short sentence."
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </label>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => void send()}
              disabled={
                streaming ||
                !prompt.trim() ||
                enabledModels.length === 0 ||
                proposalInstruction === null
              }
              title={
                proposalInstruction === null
                  ? "Waiting for the proposal instruction — without it an answer can never propose changes."
                  : undefined
              }
              data-chat-send
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
            >
              <Send className="size-3.5" aria-hidden />
              {streaming ? "Streaming…" : "Send"}
            </button>
            {chatRoute ? (
              <span data-chat-route className="text-[11.5px] text-muted">
                {chatRoute}
              </span>
            ) : null}
          </div>
          {answer || streaming ? (
            <div
              data-chat-answer
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[12.5px] whitespace-pre-wrap"
            >
              {answer}
              {streaming ? <span className="text-muted">▌</span> : null}
            </div>
          ) : null}
          {chatUsage ? (
            <p data-chat-usage className="text-[11.5px] text-muted">
              {chatUsage}
            </p>
          ) : null}

          {/* The proposal, as something a person can go to. An answer that describes changes
              and gives no way to review them is a described action, not a feature — so this
              is a link into the same editor a hand-filed set lands in, not a summary. */}
          {chatProposal ? (
            <div
              data-chat-proposal={chatProposal.change_set_id}
              data-chat-proposal-gated={chatProposal.needs_approval ? "true" : "false"}
              className="flex flex-wrap items-center gap-2 rounded-lg border border-line bg-canvas px-3 py-2"
            >
              <ClipboardList className="size-3.5 shrink-0 text-muted" aria-hidden />
              <span className="text-[12.5px] font-medium">
                Proposed changes ({chatProposal.operations})
              </span>
              <span className="truncate text-[12px] text-muted">{chatProposal.title}</span>
              {chatProposal.needs_approval ? (
                <span
                  data-chat-proposal-note
                  className="inline-flex items-center gap-1 rounded-full bg-caution-soft px-2 py-0.5 text-[10.5px] font-medium text-caution"
                >
                  at least one needs approval
                </span>
              ) : null}
              <Link
                href={`/ai/change-sets/${chatProposal.change_set_id}`}
                data-chat-proposal-open
                className="ml-auto inline-flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
              >
                Review and confirm
                <ArrowRight className="size-3.5" aria-hidden />
              </Link>
            </div>
          ) : null}

          {/* The answer is fine; the filing was not. Shown beside it, not instead of it. */}
          {chatProposalError ? (
            <p
              data-chat-proposal-error
              className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
            >
              {chatProposalError} The answer above is complete, but the changes it describes were
              not saved as a change set.
            </p>
          ) : null}
          {proposalInstructionError && !chatProposal && !chatProposalError ? (
            <p
              data-chat-proposal-instruction-error
              className="rounded-lg bg-caution-soft px-3 py-2 text-[12px] text-caution"
            >
              {proposalInstructionError} Until it can be read, a prompt sent from here cannot
              propose changes — the model would never be told the format.
            </p>
          ) : null}
        </div>
      </section>

      {testing ? (
        <TestModal
          report={testReport}
          running={testRunning}
          error={testError}
          onClose={closeTest}
        />
      ) : null}

      <p className="text-[11.5px] text-muted">
        Connected {providers?.length ?? 0} provider{providers?.length === 1 ? "" : "s"} ·{" "}
        {models?.length ?? 0} model{models?.length === 1 ? "" : "s"}
        {providers && providers.length > 0
          ? ` · since ${formatTimestamp(
              providers
                .map((provider) => provider.created_at)
                .sort()[0],
            )}`
          : ""}
      </p>
    </div>
  );
}

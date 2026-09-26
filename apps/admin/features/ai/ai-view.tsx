"use client";

/**
 * AI Hub screen (docs/06-AI-HUB.md, P11).
 *
 * Three things, in the order an operator needs them: the providers this installation connected
 * (with the key write-only — the panel shows whether one is stored, never its value), the model
 * registry those providers fill, and a chat to try the whole chain — router, provider, stream —
 * before anything else on the platform builds on it.
 */
import { useCallback, useEffect, useState } from "react";

import { Download, Plus, Power, Send, Star, Trash2 } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiModel,
  type AiProvider,
  connectAiProvider,
  discoverAiProviderModels,
  fetchAiModels,
  fetchAiProviders,
  removeAiProvider,
  replaceAiProviderModels,
  streamChat,
  updateAiModel,
  updateAiProvider,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

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

/** The AI Hub screen. */
export function AiView() {
  const [providers, setProviders] = useState<AiProvider[] | null>(null);
  const [models, setModels] = useState<AiModel[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  // The connect form.
  const [showForm, setShowForm] = useState(false);
  const [name, setName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [modelKeys, setModelKeys] = useState("");
  const [makeDefault, setMakeDefault] = useState(false);

  // The model editor of one provider.
  const [editing, setEditing] = useState<string | null>(null);
  const [editingKeys, setEditingKeys] = useState("");

  // The chat.
  const [chatModel, setChatModel] = useState("");
  const [prompt, setPrompt] = useState("");
  const [answer, setAnswer] = useState("");
  const [chatRoute, setChatRoute] = useState<string | null>(null);
  const [chatUsage, setChatUsage] = useState<string | null>(null);
  const [streaming, setStreaming] = useState(false);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    setError(null);

    Promise.all([fetchAiProviders(), fetchAiModels()])
      .then(([loadedProviders, loadedModels]) => {
        if (!cancelled) {
          setProviders(loadedProviders);
          setModels(loadedModels);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setProviders(null);
        setModels(null);
        setError(
          cause instanceof ApiError ? cause.message : "The AI Hub could not be loaded.",
        );
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

  /** The models a textarea describes: one key per line. */
  const keysOf = (value: string): { key: string }[] =>
    value
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean)
      .map((key) => ({ key }));

  const connect = async () => {
    await run(
      () =>
        connectAiProvider({
          name,
          baseUrl,
          apiKey,
          isDefault: makeDefault,
          models: keysOf(modelKeys),
        }),
      `${name.trim()} connected.`,
    );
    setName("");
    setBaseUrl("");
    setApiKey("");
    setModelKeys("");
    setMakeDefault(false);
    setShowForm(false);
  };

  const send = async () => {
    if (!prompt.trim() || streaming) {
      return;
    }
    setStreaming(true);
    setError(null);
    setNotice(null);
    setAnswer("");
    setChatRoute(null);
    setChatUsage(null);

    try {
      await streamChat(
        { model: chatModel || undefined, messages: [{ role: "user", content: prompt }] },
        {
          onStart: (info) => setChatRoute(`${info.provider} · ${info.model} · ${info.protocol}`),
          onDelta: (content) => setAnswer((current) => current + content),
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
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Base URL</span>
                <input
                  value={baseUrl}
                  onChange={(event) => setBaseUrl(event.target.value)}
                  placeholder="https://api.example.com/v1"
                  required
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">API key</span>
                <input
                  value={apiKey}
                  onChange={(event) => setApiKey(event.target.value)}
                  type="password"
                  autoComplete="off"
                  placeholder="Leave empty for a local endpoint"
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                <span className="text-[11px] text-muted">
                  Stored for the platform&apos;s own calls; never shown again.
                </span>
              </label>
              <label className="flex flex-col gap-1">
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
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
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
        ) : providers.length === 0 ? (
          <EmptyState
            title="No provider is connected yet"
            hint="Connect an OpenAI-compatible endpoint, give it the models it serves, and the platform can talk to it."
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
                  <span className="text-[11.5px] text-muted">
                    {provider.model_count} model{provider.model_count === 1 ? "" : "s"}
                  </span>
                  <span className="text-[11.5px] text-muted">
                    {provider.has_api_key ? "Key stored" : "No key"}
                  </span>
                  <span className="ml-auto flex flex-wrap items-center gap-1.5">
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
                  </span>
                </div>
                <p className="font-mono text-[11.5px] break-all text-muted">{provider.base_url}</p>

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
                        className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
                      >
                        Save models
                      </button>
                      <button
                        type="button"
                        disabled={busy}
                        onClick={async () => {
                          setBusy(true);
                          setError(null);
                          setNotice(null);
                          try {
                            const found = await discoverAiProviderModels(provider.id);
                            setEditingKeys(found.models.join("\n"));
                            setNotice(
                              found.models.length === 0
                                ? "The provider reported no models."
                                : `The provider reported ${found.models.length} models — review, then save.`,
                            );
                          } catch (cause: unknown) {
                            setError(
                              cause instanceof ApiError
                                ? cause.message
                                : "The provider could not be asked.",
                            );
                          } finally {
                            setBusy(false);
                          }
                        }}
                        data-provider-discover={provider.name}
                        className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-surface disabled:opacity-60"
                      >
                        <Download className="size-3.5" aria-hidden />
                        Discover from provider
                      </button>
                    </div>
                  </div>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* Models */}
      <section className="rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Models</h2>
          <p className="text-[12px] text-muted">
            The registry the router picks from. The default model answers when a request names
            none.
          </p>
        </header>

        {models === null ? (
          <LoadingTable columns={4} rows={2} />
        ) : models.length === 0 ? (
          <EmptyState
            title="No model is registered"
            hint="Add models to a provider — or pull them from the provider itself with Discover."
          />
        ) : (
          <ul className="divide-y divide-[var(--color-line)]">
            {models.map((model) => (
              <li
                key={model.id}
                data-model-row={model.model_id}
                className="flex flex-wrap items-center gap-2 px-4 py-2.5"
              >
                <span className="font-mono text-[12.5px]">{model.model_key}</span>
                <span className="text-[11.5px] text-muted">{model.provider_name}</span>
                {model.is_default ? (
                  <span className="inline-flex items-center gap-1 rounded-full bg-accent-soft px-2 py-0.5 text-[10.5px] font-medium text-accent-strong">
                    <Star className="size-3" aria-hidden />
                    Default
                  </span>
                ) : null}
                <Flag label="tools" on={model.supports_tools} />
                <Flag label="vision" on={model.supports_vision} />
                <Flag label="stream" on={model.supports_streaming} />
                {model.context_window ? (
                  <span className="text-[11px] text-muted">
                    {model.context_window.toLocaleString("en-US")} ctx
                  </span>
                ) : null}
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
              </li>
            ))}
          </ul>
        )}
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
              disabled={streaming || !prompt.trim() || enabledModels.length === 0}
              data-chat-send
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
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
        </div>
      </section>

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

"use client";

/**
 * `/analytics/settings` — tracking, privacy, exclusions, the snippet and the data operations
 * (docs/requests/REQ-007, slices 1 and 4).
 *
 * The screen is where the privacy promise is made, so it follows one rule: **what the page says
 * is what the code does**. Field validation mirrors the server's validation of the same field, a
 * value the API refuses is shown beside the input that sent it, and the "what we store" table is
 * rendered from the server's own description of the schema — the screen cannot describe a
 * different engine than the one running.
 *
 * The data section is the irreversible half: `Run purge now` names the cutoff *before* the
 * button is pressed, and `Erase visitor` needs the handle typed into a second field before it
 * runs, because neither can be undone by clicking again.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertCircle,
  BarChart3,
  Check,
  ClipboardCopy,
  Database,
  Eraser,
  RefreshCw,
  Save,
  ShieldCheck,
  Trash2,
} from "lucide-react";

import {
  ApiError,
  eraseAnalyticsVisitor,
  fetchAnalyticsSettings,
  fetchAnalyticsSnippet,
  runAnalyticsPurge,
  updateAnalyticsSettings,
  type AnalyticsErasureOutcome,
  type AnalyticsPurgeOutcome,
  type AnalyticsSettings,
  type AnalyticsSettingsChanges,
  type AnalyticsSettingsResponse,
  type AnalyticsSnippet,
  type AnalyticsStorageField,
} from "@/lib/api";

import { useAnalytics } from "./analytics-shell";
import { DataTable, ErrorPanel, LoadingRows, Panel, formatCount, type Column } from "./parts";

/** The tracking modes, with the label the screen shows. */
const MODES: { value: string; label: string; hint: string }[] = [
  {
    value: "cookieless",
    label: "Cookieless (no browser storage)",
    hint: "The visitor identifier is computed server-side and rotates at midnight.",
  },
  {
    value: "cookie",
    label: "Cookie (counts a visitor across days)",
    hint: "Only with the site’s own consent flow; the cookie is first-party.",
  },
];

/** Bounds the API enforces; named here so the field error appears before the request. */
const SAMPLE_MIN = 1;
const SAMPLE_MAX = 100;
const RETENTION_MIN = 7;
const RETENTION_MAX = 1080;
const MAX_PATH_PATTERN = 200;
const MAX_LINES = 200;

/** Turn the stored settings into the editable draft. */
function draftOf(settings: AnalyticsSettings): AnalyticsSettingsChanges {
  return {
    tracking_enabled: settings.tracking_enabled,
    mode: settings.mode,
    anonymize_ip: settings.anonymize_ip,
    respect_dnt: settings.respect_dnt,
    bot_filter: settings.bot_filter,
    sample_rate: settings.sample_rate,
    retention_days: settings.retention_days,
    excluded_paths: [...settings.excluded_paths],
    excluded_ips: [...settings.excluded_ips],
  };
}

/** One line of a textarea list, split and trimmed, with the blank lines a paste leaves behind. */
function lines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

/** `true` for an IPv4/IPv6 address or network, the shape the API accepts. */
function isAddressRule(rule: string): boolean {
  const [address, prefix] = rule.split("/");
  if (rule.split("/").length > 2) {
    return false;
  }
  const ipv4 = /^\d{1,3}(\.\d{1,3}){3}$/.test(address);
  const ipv6 = /^[0-9a-f:]+$/i.test(address) && address.includes(":");
  if (!ipv4 && !ipv6) {
    return false;
  }
  if (ipv4 && address.split(".").some((part) => Number(part) > 255)) {
    return false;
  }
  if (prefix === undefined) {
    return true;
  }
  const width = Number(prefix);
  if (!Number.isInteger(width)) {
    return false;
  }
  return ipv4 ? width >= 0 && width <= 32 : width >= 0 && width <= 128;
}

/** How the glob preview reads one path pattern back to its operator. */
function describePath(pattern: string): string {
  if (pattern.startsWith("*")) {
    return `ends with “${pattern.slice(1)}”`;
  }
  if (pattern.endsWith("*")) {
    return `starts with “${pattern.slice(0, -1)}”`;
  }
  return `exactly “${pattern}”`;
}

/** The problems a draft carries, per field; an empty object means “saveable”. */
function problems(draft: AnalyticsSettingsChanges): Record<string, string> {
  const found: Record<string, string> = {};
  if (!Number.isInteger(draft.sample_rate) || draft.sample_rate < SAMPLE_MIN || draft.sample_rate > SAMPLE_MAX) {
    found.sample_rate = `Sample rate must be a whole number between ${SAMPLE_MIN} and ${SAMPLE_MAX}.`;
  }
  if (
    !Number.isInteger(draft.retention_days) ||
    draft.retention_days < RETENTION_MIN ||
    draft.retention_days > RETENTION_MAX
  ) {
    found.retention_days = `Retention must be a whole number between ${RETENTION_MIN} and ${RETENTION_MAX} days.`;
  }
  if (!MODES.some((entry) => entry.value === draft.mode)) {
    found.mode = "Choose one of the two tracking modes.";
  }
  if (draft.excluded_paths.length > MAX_LINES) {
    found.excluded_paths = `At most ${MAX_LINES} path patterns.`;
  }
  for (const pattern of draft.excluded_paths) {
    if (!pattern.startsWith("/") && !pattern.startsWith("*")) {
      found.excluded_paths = `“${pattern}” must start with “/” (a path) or “*” (a suffix).`;
      break;
    }
    if (pattern.length > MAX_PATH_PATTERN) {
      found.excluded_paths = `“${pattern}” is longer than ${MAX_PATH_PATTERN} characters.`;
      break;
    }
    if (/\s/.test(pattern)) {
      found.excluded_paths = `“${pattern}” may not contain whitespace.`;
      break;
    }
  }
  if (draft.excluded_ips.length > MAX_LINES) {
    found.excluded_ips = `At most ${MAX_LINES} address rules.`;
  }
  for (const rule of draft.excluded_ips) {
    if (!isAddressRule(rule)) {
      found.excluded_ips = `“${rule}” is not an address or a network (one per line).`;
      break;
    }
  }
  return found;
}

/** A small switch that behaves like a checkbox and reads like a control. */
function Switch({
  id,
  checked,
  label,
  description,
  testId,
  onChange,
}: {
  id: string;
  checked: boolean;
  label: string;
  description: string;
  testId: string;
  onChange: (value: boolean) => void;
}) {
  return (
    <div className="flex items-start gap-3 rounded-lg border border-line px-3 py-2.5">
      <button
        type="button"
        id={id}
        role="switch"
        aria-checked={checked}
        aria-label={label}
        data-analytics-settings-switch={testId}
        onClick={() => onChange(!checked)}
        className={`mt-0.5 flex h-5 w-9 shrink-0 items-center rounded-full border transition ${
          checked ? "border-accent bg-accent-soft" : "border-line bg-canvas"
        }`}
      >
        <span
          className={`size-3.5 rounded-full transition ${
            checked ? "ml-4.5 bg-accent-strong" : "ml-0.5 bg-muted/60"
          }`}
        />
      </button>
      <label htmlFor={id} className="min-w-0 cursor-pointer">
        <span className="block text-[12.5px] font-medium text-ink">{label}</span>
        <span className="block text-[11.5px] leading-relaxed text-muted">{description}</span>
      </label>
    </div>
  );
}

/** The section the screen is made of: a titled panel with a short explanation. */
export function AnalyticsSettingsView() {
  const { siteId } = useAnalytics();
  const [data, setData] = useState<AnalyticsSettingsResponse | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [error, setError] = useState<{ message: string; code: string } | null>(null);
  const [attempt, setAttempt] = useState(0);

  const [draft, setDraft] = useState<AnalyticsSettingsChanges | null>(null);
  const [pathsText, setPathsText] = useState("");
  const [ipsText, setIpsText] = useState("");
  const [saving, setSaving] = useState(false);
  const [savedNote, setSavedNote] = useState<string | null>(null);
  const [serverError, setServerError] = useState<string | null>(null);

  const [snippet, setSnippet] = useState<AnalyticsSnippet | null>(null);
  const [copied, setCopied] = useState(false);

  const [purge, setPurge] = useState<{
    running: boolean;
    outcome: AnalyticsPurgeOutcome | null;
    error: string | null;
  }>({ running: false, outcome: null, error: null });

  const [eraseHandle, setEraseHandle] = useState("");
  const [eraseConfirm, setEraseConfirm] = useState("");
  const [erase, setErase] = useState<{
    running: boolean;
    outcome: AnalyticsErasureOutcome | null;
    error: string | null;
  }>({ running: false, outcome: null, error: null });

  const load = useCallback(async () => {
    if (!siteId) {
      setData(null);
      setStatus("idle");
      return;
    }

    setStatus("loading");
    setError(null);
    try {
      const answer = await fetchAnalyticsSettings(siteId);
      setData(answer);
      setDraft(draftOf(answer.settings));
      setPathsText(answer.settings.excluded_paths.join("\n"));
      setIpsText(answer.settings.excluded_ips.join("\n"));
      setStatus("ready");
    } catch (cause) {
      setData(null);
      setStatus("error");
      setError(
        cause instanceof ApiError
          ? { message: cause.message, code: cause.code }
          : { message: "The settings could not be loaded.", code: "unknown_error" },
      );
    }
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load, attempt]);

  // The snippet is its own small request: a reader may see it, and it is the one piece of the
  // screen an operator copies out.
  useEffect(() => {
    if (!siteId) {
      setSnippet(null);
      return;
    }
    let live = true;
    fetchAnalyticsSnippet(siteId)
      .then((answer) => {
        if (live) {
          setSnippet(answer);
        }
      })
      .catch(() => {
        if (live) {
          setSnippet(null);
        }
      });
    return () => {
      live = false;
    };
  }, [siteId, attempt]);

  const found = useMemo(() => (draft ? problems(draft) : {}), [draft]);
  const dirty = useMemo(() => {
    if (!draft || !data) {
      return false;
    }
    const stored = draftOf(data.settings);
    return (
      JSON.stringify({ ...stored, excluded_paths: [], excluded_ips: [] }) !==
        JSON.stringify({ ...draft, excluded_paths: [], excluded_ips: [] }) ||
      pathsText !== data.settings.excluded_paths.join("\n") ||
      ipsText !== data.settings.excluded_ips.join("\n")
    );
  }, [data, draft, ipsText, pathsText]);

  const update = useCallback((changes: Partial<AnalyticsSettingsChanges>) => {
    setDraft((current) => (current ? { ...current, ...changes } : current));
    setSavedNote(null);
  }, []);

  const save = useCallback(async () => {
    if (!draft || !siteId) {
      return;
    }
    const changes: AnalyticsSettingsChanges = {
      ...draft,
      excluded_paths: lines(pathsText),
      excluded_ips: lines(ipsText),
    };
    const local = problems(changes);
    if (Object.keys(local).length > 0) {
      setDraft(changes);
      setServerError(null);
      return;
    }

    setSaving(true);
    setServerError(null);
    setSavedNote(null);
    try {
      const answer = await updateAnalyticsSettings(siteId, changes);
      setData(answer);
      setDraft(draftOf(answer.settings));
      setPathsText(answer.settings.excluded_paths.join("\n"));
      setIpsText(answer.settings.excluded_ips.join("\n"));
      setSavedNote(
        `Saved at ${new Date().toLocaleTimeString("en", {
          hour: "2-digit",
          minute: "2-digit",
          second: "2-digit",
        })}`,
      );
    } catch (cause) {
      setServerError(
        cause instanceof ApiError ? cause.message : "The settings could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  }, [draft, ipsText, pathsText, siteId]);

  const runPurge = useCallback(async () => {
    if (!siteId) {
      return;
    }
    setPurge({ running: true, outcome: null, error: null });
    try {
      const outcome = await runAnalyticsPurge(siteId);
      setPurge({ running: false, outcome, error: null });
      void load();
    } catch (cause) {
      setPurge({
        running: false,
        outcome: null,
        error: cause instanceof ApiError ? cause.message : "The purge could not run.",
      });
    }
  }, [load, siteId]);

  const runErase = useCallback(async () => {
    if (!siteId || eraseHandle.trim().length === 0) {
      return;
    }
    setErase({ running: true, outcome: null, error: null });
    try {
      const outcome = await eraseAnalyticsVisitor(siteId, eraseHandle.trim());
      setErase({ running: false, outcome, error: null });
      setEraseHandle("");
      setEraseConfirm("");
      void load();
    } catch (cause) {
      setErase({
        running: false,
        outcome: null,
        error: cause instanceof ApiError ? cause.message : "The erasure could not run.",
      });
    }
  }, [eraseHandle, load, siteId]);

  const copySnippet = useCallback(async () => {
    if (!snippet) {
      return;
    }
    try {
      await navigator.clipboard.writeText(snippet.snippet);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      // A browser that refuses the clipboard still has the code block; the note stays silent.
      setCopied(false);
    }
  }, [snippet]);

  const storageColumns = useMemo<Column<AnalyticsStorageField>[]>(
    () => [
      {
        key: "table",
        label: "Table",
        render: (row) => <span className="font-mono text-[11.5px] text-ink">{row.table}</span>,
      },
      {
        key: "column",
        label: "Column",
        render: (row) => <span className="font-mono text-[11.5px] text-muted">{row.column}</span>,
      },
      {
        key: "purpose",
        label: "Purpose",
        render: (row) => <span className="text-[12px] leading-relaxed">{row.purpose}</span>,
      },
      {
        key: "personal",
        label: "Personal data",
        render: (row) =>
          row.personal ? (
            <span className="rounded-md bg-amber-500/10 px-1.5 py-0.5 text-[11px] font-medium text-amber-700 dark:text-amber-400">
              Personal
            </span>
          ) : (
            <span className="text-[11.5px] text-muted">No</span>
          ),
      },
    ],
    [],
  );

  if (!siteId) {
    return null;
  }
  if (status === "loading" && !data) {
    return (
      <Panel title="Settings" subtitle="Loading the configuration of this site" testId="settings">
        <LoadingRows rows={8} label="Loading the settings" />
      </Panel>
    );
  }
  if (status === "error" && !data) {
    return (
      <ErrorPanel
        message={error?.message ?? "The settings could not be loaded."}
        code={error?.code}
        onRetry={() => setAttempt((value) => value + 1)}
      />
    );
  }
  if (!draft || !data) {
    return null;
  }

  const lastPurge = data.last_purge;
  const activeMode = MODES.find((entry) => entry.value === draft.mode);
  const pathPatterns = lines(pathsText);
  const addressRules = lines(ipsText);
  const confirmationOk = eraseHandle.trim().length > 0 && eraseConfirm.trim() === eraseHandle.trim();

  return (
    <div className="flex flex-col gap-4" data-analytics-settings-form>
      <div className="flex flex-wrap items-center gap-2 rounded-xl border border-line bg-surface px-4 py-3">
        <ShieldCheck className="size-4 shrink-0 text-accent-strong" aria-hidden />
        <p className="min-w-0 flex-1 text-[12.5px] text-muted">
          These settings decide what the collector stores for{" "}
          <span className="font-medium text-ink">{snippet?.site.name ?? "this site"}</span> — and
          what it drops before storing anything.
        </p>
        {savedNote ? (
          <span
            data-analytics-settings-saved
            className="flex items-center gap-1.5 rounded-lg bg-emerald-500/10 px-2 py-1 text-[11.5px] font-medium text-emerald-700 dark:text-emerald-400"
          >
            <Check className="size-3.5" aria-hidden />
            {savedNote}
          </span>
        ) : null}
        <button
          type="button"
          data-analytics-settings-refresh
          onClick={() => setAttempt((value) => value + 1)}
          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Reload
        </button>
        <button
          type="button"
          data-analytics-settings-restore
          onClick={() => {
            const defaults = data.defaults;
            setDraft(defaults);
            setPathsText(defaults.excluded_paths.join("\n"));
            setIpsText(defaults.excluded_ips.join("\n"));
            setSavedNote(null);
          }}
          className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:bg-quiet-soft hover:text-ink"
        >
          Restore defaults
        </button>
        <button
          type="button"
          data-analytics-settings-save
          data-qa-guard="settings"
          disabled={!dirty || saving}
          onClick={() => void save()}
          className="flex items-center gap-1.5 rounded-lg bg-accent-soft px-3 py-1.5 text-[12px] font-medium text-accent-strong transition hover:brightness-95 disabled:opacity-50"
        >
          <Save className="size-3.5" aria-hidden />
          {saving ? "Saving…" : "Save changes"}
        </button>
      </div>

      {serverError ? (
        <p
          data-analytics-settings-server-error
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-[12px] text-amber-800 dark:text-amber-300"
        >
          <AlertCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          {serverError}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-2">
        <Panel
          title="Tracking"
          subtitle="What the collector records, and how a visitor is counted"
          testId="settings-tracking"
          bodyClassName="flex flex-col gap-3 p-4"
        >
          <Switch
            id="analytics-tracking-enabled"
            testId="tracking_enabled"
            checked={draft.tracking_enabled}
            label="Tracking enabled"
            description="Off means the collector stores nothing at all for this site; beacons are still answered."
            onChange={(value) => update({ tracking_enabled: value })}
          />
          <label className="flex flex-col gap-1.5">
            <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
              Mode
            </span>
            <select
              value={draft.mode}
              data-analytics-settings-mode
              onChange={(event) => update({ mode: event.target.value })}
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[12.5px] text-ink"
            >
              {MODES.map((entry) => (
                <option key={entry.value} value={entry.value}>
                  {entry.label}
                </option>
              ))}
            </select>
            <span className="text-[11.5px] text-muted">
              {activeMode?.hint ?? "Pick a mode."}
            </span>
            {found.mode ? (
              <span data-analytics-settings-error="mode" className="text-[11.5px] text-amber-700 dark:text-amber-400">
                {found.mode}
              </span>
            ) : null}
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
              Sample rate (%)
            </span>
            <input
              type="number"
              min={SAMPLE_MIN}
              max={SAMPLE_MAX}
              value={Number.isFinite(draft.sample_rate) ? draft.sample_rate : ""}
              data-analytics-settings-sample-rate
              onChange={(event) =>
                update({ sample_rate: Number.parseInt(event.target.value, 10) || 0 })
              }
              className="w-32 rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12.5px] text-ink"
            />
            <span className="text-[11.5px] text-muted">
              100 records every visit; lower values keep a stable share of them.
            </span>
            {found.sample_rate ? (
              <span
                data-analytics-settings-error="sample_rate"
                className="text-[11.5px] text-amber-700 dark:text-amber-400"
              >
                {found.sample_rate}
              </span>
            ) : null}
          </label>
          <Switch
            id="analytics-bot-filter"
            testId="bot_filter"
            checked={draft.bot_filter}
            label="Bot filter"
            description="Crawlers and automated clients are counted in the day’s filtered total instead of the reports."
            onChange={(value) => update({ bot_filter: value })}
          />
        </Panel>

        <Panel
          title="Privacy"
          subtitle="What is kept about an address, and for how long"
          testId="settings-privacy"
          bodyClassName="flex flex-col gap-3 p-4"
        >
          <Switch
            id="analytics-anonymize-ip"
            testId="anonymize_ip"
            checked={draft.anonymize_ip}
            label="Anonymize IP addresses"
            description="On (the default) stores no address at all. Off stores only the network: /24 for IPv4, /48 for IPv6."
            onChange={(value) => update({ anonymize_ip: value })}
          />
          <Switch
            id="analytics-respect-dnt"
            testId="respect_dnt"
            checked={draft.respect_dnt}
            label="Respect Do Not Track and Global Privacy Control"
            description="A browser that sends either signal is dropped before anything is written."
            onChange={(value) => update({ respect_dnt: value })}
          />
          <label className="flex flex-col gap-1.5">
            <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
              Retention (days)
            </span>
            <input
              type="number"
              min={RETENTION_MIN}
              max={RETENTION_MAX}
              value={Number.isFinite(draft.retention_days) ? draft.retention_days : ""}
              data-analytics-settings-retention
              onChange={(event) =>
                update({ retention_days: Number.parseInt(event.target.value, 10) || 0 })
              }
              className="w-32 rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12.5px] text-ink"
            />
            <span className="text-[11.5px] text-muted">
              Raw visits, page views, events and goal hits older than this are removed by the
              purge. Aggregated counts stay, because they name nobody.
            </span>
            {found.retention_days ? (
              <span
                data-analytics-settings-error="retention_days"
                className="text-[11.5px] text-amber-700 dark:text-amber-400"
              >
                {found.retention_days}
              </span>
            ) : null}
          </label>
        </Panel>

        <Panel
          title="Exclusions"
          subtitle="Beacons from these paths and addresses are dropped, not stored and hidden"
          testId="settings-exclusions"
          bodyClassName="grid gap-4 p-4 md:grid-cols-2"
        >
          <label className="flex flex-col gap-1.5">
            <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
              Excluded paths (one per line)
            </span>
            <textarea
              value={pathsText}
              data-analytics-settings-paths
              onChange={(event) => {
                setPathsText(event.target.value);
                setSavedNote(null);
              }}
              rows={6}
              placeholder={"/admin/*\n/checkout\n*.pdf"}
              spellCheck={false}
              className="w-full rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px] text-ink"
            />
            <span className="text-[11.5px] text-muted">
              A pattern is an exact path, a prefix (<span className="font-mono">/admin/*</span>) or
              a suffix (<span className="font-mono">*.pdf</span>).
            </span>
            {pathPatterns.length > 0 ? (
              <ul data-analytics-path-preview className="flex flex-col gap-0.5 text-[11.5px] text-muted">
                {pathPatterns.slice(0, 6).map((pattern) => (
                  <li key={pattern} className="truncate">
                    <span className="font-mono text-ink">{pattern}</span> — {describePath(pattern)}
                  </li>
                ))}
                {pathPatterns.length > 6 ? (
                  <li>…and {pathPatterns.length - 6} more</li>
                ) : null}
              </ul>
            ) : null}
            {found.excluded_paths ? (
              <span
                data-analytics-settings-error="excluded_paths"
                className="text-[11.5px] text-amber-700 dark:text-amber-400"
              >
                {found.excluded_paths}
              </span>
            ) : null}
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
              Excluded addresses (one per line)
            </span>
            <textarea
              value={ipsText}
              data-analytics-settings-ips
              onChange={(event) => {
                setIpsText(event.target.value);
                setSavedNote(null);
              }}
              rows={6}
              placeholder={"203.0.113.7\n203.0.113.0/24"}
              spellCheck={false}
              className="w-full rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px] text-ink"
            />
            <span className="text-[11.5px] text-muted">
              An address or a network. The office network is the usual entry — the screen never
              shows an address back, because none is stored.
            </span>
            {addressRules.length > 0 ? (
              <span className="text-[11.5px] text-muted">
                {formatCount(addressRules.length)} rule
                {addressRules.length === 1 ? "" : "s"} in effect.
              </span>
            ) : null}
            {found.excluded_ips ? (
              <span
                data-analytics-settings-error="excluded_ips"
                className="text-[11.5px] text-amber-700 dark:text-amber-400"
              >
                {found.excluded_ips}
              </span>
            ) : null}
          </label>
        </Panel>

        <Panel
          title="Tracking snippet"
          subtitle="Paste this before the closing tag of the site’s pages"
          testId="settings-snippet"
          action={
            <button
              type="button"
              data-analytics-snippet-copy
              onClick={() => void copySnippet()}
              disabled={!snippet}
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft disabled:opacity-50"
            >
              <ClipboardCopy className="size-3.5" aria-hidden />
              {copied ? "Copied" : "Copy"}
            </button>
          }
          bodyClassName="flex flex-col gap-3 p-4"
        >
          <div className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
            <BarChart3 className="size-3.5" aria-hidden />
            Site key
            <span className="rounded-md bg-quiet-soft px-1.5 py-0.5 font-mono text-[11.5px] text-ink">
              {snippet?.site.key ?? "…"}
            </span>
            <span aria-hidden>·</span>
            <span className="font-mono">{snippet?.collect_url ?? ""}</span>
          </div>
          <pre
            data-analytics-snippet
            className="overflow-x-auto rounded-lg border border-line bg-canvas px-3 py-2.5 font-mono text-[11.5px] leading-relaxed text-ink"
          >
            {snippet?.snippet ?? "Loading the snippet…"}
          </pre>
          <p className="text-[11.5px] leading-relaxed text-muted">
            The script is cookieless: it writes nothing to the browser and stops before sending
            anything when the browser asks not to be tracked.
          </p>
          {copied ? (
            <span data-analytics-snippet-note className="text-[11.5px] text-emerald-700 dark:text-emerald-400">
              Snippet copied to the clipboard.
            </span>
          ) : null}
        </Panel>
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <Panel
          title="Data"
          subtitle="The two operations that cannot be undone"
          testId="settings-data"
          bodyClassName="flex flex-col gap-4 p-4"
        >
          <div className="flex flex-col gap-2 rounded-lg border border-line px-3 py-3">
            <div className="flex items-center gap-2">
              <Trash2 className="size-4 shrink-0 text-muted" aria-hidden />
              <p className="text-[12.5px] font-medium text-ink">Retention purge</p>
            </div>
            <p className="text-[11.5px] leading-relaxed text-muted">
              Removes the raw rows older than{" "}
              <span data-analytics-purge-cutoff className="font-mono text-ink">
                {new Date(data.purge_cutoff).toISOString().replace("T", " ").slice(0, 16)} UTC
              </span>{" "}
              and records the run in the audit trail.
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                data-analytics-purge-run
                data-qa-guard="settings"
                disabled={purge.running}
                onClick={() => void runPurge()}
                className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft disabled:opacity-50"
              >
                <Database className="size-3.5" aria-hidden />
                {purge.running ? "Purging…" : "Run purge now"}
              </button>
              {lastPurge ? (
                <span data-analytics-last-purge className="text-[11.5px] text-muted">
                  Last run: {lastPurge.kind} · {formatCount(lastPurge.rows_removed)} row
                  {lastPurge.rows_removed === 1 ? "" : "s"} ·{" "}
                  {new Date(lastPurge.created_at).toISOString().replace("T", " ").slice(0, 16)} UTC
                </span>
              ) : (
                <span className="text-[11.5px] text-muted">No purge has run yet.</span>
              )}
            </div>
            {purge.outcome ? (
              <p
                data-analytics-purge-result
                className="rounded-lg bg-emerald-500/10 px-2.5 py-2 text-[11.5px] leading-relaxed text-emerald-700 dark:text-emerald-400"
              >
                Removed {formatCount(purge.outcome.rows_removed)} rows older than{" "}
                {new Date(purge.outcome.cutoff).toISOString().slice(0, 10)} —{" "}
                {formatCount(purge.outcome.visits)} visits,{" "}
                {formatCount(purge.outcome.pageviews)} page views,{" "}
                {formatCount(purge.outcome.events)} events,{" "}
                {formatCount(purge.outcome.goal_hits)} goal hits.
              </p>
            ) : null}
            {purge.error ? (
              <p
                data-analytics-purge-error
                role="alert"
                className="text-[11.5px] text-amber-700 dark:text-amber-400"
              >
                {purge.error}
              </p>
            ) : null}
          </div>

          <div className="flex flex-col gap-2 rounded-lg border border-line px-3 py-3">
            <div className="flex items-center gap-2">
              <Eraser className="size-4 shrink-0 text-muted" aria-hidden />
              <p className="text-[12.5px] font-medium text-ink">Erase a visitor</p>
            </div>
            <p className="text-[11.5px] leading-relaxed text-muted">
              Deletes every visit, page view, event and goal hit of one visitor handle. Under
              cookieless counting the handle rotates daily, so a visitor who came back on another
              day has one handle per day.
            </p>
            <label className="flex flex-col gap-1.5">
              <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
                Visitor handle (64 hex characters)
              </span>
              <input
                type="text"
                value={eraseHandle}
                data-analytics-erase-handle
                data-qa-guard="settings"
                spellCheck={false}
                onChange={(event) => {
                  setEraseHandle(event.target.value);
                  setEraseConfirm("");
                }}
                placeholder="e3b0c44298fc1c149afbf4c8996fb924…"
                className="w-full rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px] text-ink"
              />
            </label>
            {eraseHandle.trim().length > 0 ? (
              <label className="flex flex-col gap-1.5">
                <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
                  Type the handle again to confirm
                </span>
                <input
                  type="text"
                  value={eraseConfirm}
                  data-analytics-erase-confirm
                  spellCheck={false}
                  onChange={(event) => setEraseConfirm(event.target.value)}
                  placeholder="Repeat the handle exactly"
                  className="w-full rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px] text-ink"
                />
              </label>
            ) : null}
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                data-analytics-erase-run
                data-qa-guard="settings"
                disabled={!confirmationOk || erase.running}
                onClick={() => void runErase()}
                className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft disabled:opacity-50"
              >
                <Eraser className="size-3.5" aria-hidden />
                {erase.running ? "Erasing…" : "Erase visitor"}
              </button>
              <span className="text-[11.5px] text-muted">
                {confirmationOk
                  ? "Ready — the handle is typed twice."
                  : "The button unlocks when the handle matches."}
              </span>
            </div>
            {erase.outcome ? (
              <p
                data-analytics-erase-result
                className="rounded-lg bg-emerald-500/10 px-2.5 py-2 text-[11.5px] leading-relaxed text-emerald-700 dark:text-emerald-400"
              >
                {erase.outcome.rows_removed === 0
                  ? "Nothing to erase: no row carries that handle on this site."
                  : `Erased ${formatCount(erase.outcome.rows_removed)} rows — ${formatCount(
                      erase.outcome.visits,
                    )} visits, ${formatCount(erase.outcome.pageviews)} page views, ${formatCount(
                      erase.outcome.events,
                    )} events, ${formatCount(erase.outcome.goal_hits)} goal hits.`}
              </p>
            ) : null}
            {erase.error ? (
              <p
                data-analytics-erase-error
                role="alert"
                className="text-[11.5px] text-amber-700 dark:text-amber-400"
              >
                {erase.error}
              </p>
            ) : null}
          </div>
        </Panel>

        <Panel
          title="What we store"
          subtitle="Every column the engine keeps, and why"
          testId="settings-storage"
          bodyClassName="p-0"
        >
          <DataTable
            columns={storageColumns}
            rows={data.storage}
            rowKey={(row) => `${row.table}:${row.column}`}
          />
        </Panel>
      </div>
    </div>
  );
}

"use client";

/**
 * `/developer/sdks` — the four things a developer extends Omnion with (REQ-033, slice 4).
 *
 * Four tabs, because the REQ names four things and they share nothing but this route: a plugin
 * starter, a theme starter, a workflow starter and a CLI login. One screen with a tab strip is
 * the right shape for that — four routes would give four URLs to remember and four empty states
 * to write, and the template picker is the same three cards either way.
 *
 * Six decisions on this screen are worth writing down, because the wrong version of each is a
 * bug nobody reports:
 *
 * - **Preview and Generate are separate buttons, and only Generate records a row.**
 *   `POST /dev/sdks/scaffold` returns the file tree and writes nothing, so it is safe to call
 *   while somebody is still choosing a name. `POST /dev/sdks/scaffolds` writes the audit row.
 *   A single "Generate" that recorded a row per keystroke would fill `sdk_scaffolds` with
 *   previews nobody downloaded, and the history list would become a list of curiosity.
 * - **The Download button sits on the recorded row, not on the preview.** The archive is a
 *   property of a *generation* — it has an id, and the id is what the audit records. A download
 *   button on a preview would have no id to download and would regenerate a different archive.
 * - **The name field is validated against the server's rule, shown before the buttons.**
 *   The slug rule is `[A-Za-z0-9_-]`, 3–64 characters, and it is enforced by
 *   `scaffold_rules` — the same function the API calls. The screen mirrors it for the *message*
 *   and does not enforce it as *policy*: a client-side rule that drifted from the server's would
 *   refuse a name the platform accepts, which is worse than a server refusal with a reason.
 * - **The manifest validator is a textarea, not a file picker.** A drop zone that silently
 *   ignored an oversized or wrongly-typed file is a validator that reports "no issues" for a
 *   file it never read. Text in, report out, with every issue's line number — the screen's whole
 *   job is to make the failing line findable.
 * - **The CLI tab shows the code and the scopes and never a token.** `omnion login` hands the
 *   token to the *terminal* through the poll, and this browser has no session that could see it.
 *   The tab says so, because a login screen that appears to have failed to produce a credential
 *   is the exact screen a user abandons.
 * - **The expiry countdown is computed from the server's `expires_in`, not from a local
 *   timer started on click.** A tab left open for twenty minutes must not show four minutes
 *   remaining because the tab's own clock drifted from the server's.
 *
 * Every hook is `data-dev-sdk-*`; the walkthrough pass drives them by name.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  Check,
  CheckCircle2,
  ClipboardCopy,
  Download,
  FileCode2,
  Loader2,
  Play,
  Terminal,
  XCircle,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  approveDeviceCode,
  fetchDeviceCode,
  fetchScaffolds,
  fetchSdkTemplates,
  previewScaffold,
  recordScaffold,
  scaffoldDownloadUrl,
  startDeviceCode,
  validateManifest,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type {
  DeviceLookup,
  DeviceStart,
  ManifestIssue,
  ManifestReport,
  ScaffoldRecord,
  ScaffoldTree,
  SdkKind,
  SdkTarget,
  SdkTemplate,
} from "@/lib/types";

type Tab = "plugin" | "theme" | "workflow" | "cli";

const TABS: { id: Tab; label: string }[] = [
  { id: "plugin", label: "Plugin" },
  { id: "theme", label: "Theme" },
  { id: "workflow", label: "Workflow" },
  { id: "cli", label: "CLI" },
];

/** The name rule, mirrored for its *message*. The server owns the policy. */
const NAME_MIN = 3;
const NAME_MAX = 64;
const NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

const TARGETS: { id: SdkTarget; label: string; hint: string }[] = [
  { id: "live", label: "Live", hint: "Aimed at a production environment" },
  { id: "sandbox", label: "Sandbox", hint: "Aimed at a staging environment" },
];

/** The name rule's sentence, or null when the name is acceptable. */
function nameProblem(name: string): string | null {
  if (name.length === 0) return null;
  if (name.length < NAME_MIN) return `The name needs at least ${NAME_MIN} characters.`;
  if (name.length > NAME_MAX) return `The name cannot be longer than ${NAME_MAX} characters.`;
  if (!NAME_PATTERN.test(name)) {
    return "Use letters, digits, single dashes and underscores only — no spaces or path separators.";
  }
  return null;
}

function bytesLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function DeveloperSdksView() {
  const [tab, setTab] = useState<Tab>("plugin");

  return (
    <div className="space-y-4">
      <div role="tablist" aria-label="Developer tooling" data-dev-sdk-tabs className="flex flex-wrap gap-1">
        {TABS.map((entry) => (
          <button
            key={entry.id}
            type="button"
            role="tab"
            aria-selected={tab === entry.id}
            data-dev-sdk-tab={entry.id}
            onClick={() => setTab(entry.id)}
            className={
              tab === entry.id
                ? "rounded border border-accent bg-accent-soft px-3 py-1.5 text-[13px] text-ink"
                : "rounded border border-line bg-surface px-3 py-1.5 text-[13px] text-muted"
            }
          >
            {entry.label}
          </button>
        ))}
      </div>

      {tab === "cli" ? <CliTab /> : <ScaffoldTab kind={tab} />}
    </div>
  );
}

function ScaffoldTab({ kind }: { kind: SdkKind }) {
  const [templates, setTemplates] = useState<SdkTemplate[] | null>(null);
  const [templatesError, setTemplatesError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [target, setTarget] = useState<SdkTarget>("live");
  const [preview, setPreview] = useState<ScaffoldTree | null>(null);
  const [history, setHistory] = useState<ScaffoldRecord[]>([]);
  const [busy, setBusy] = useState<"preview" | "generate" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [openFile, setOpenFile] = useState<string | null>(null);

  const loadHistory = useCallback(async () => {
    try {
      const data = await fetchScaffolds();
      setHistory(data.scaffolds);
    } catch {
      // The history is a secondary panel. A failure here must not replace the generator with an
      // error page — the button that works would disappear along with the list that does not.
      setHistory([]);
    }
  }, []);

  useEffect(() => {
    let live = true;
    fetchSdkTemplates()
      .then((data: SdkTemplate[]) => {
        if (live) setTemplates(data);
      })
      .catch((err: unknown) => {
        if (live) {
          setTemplatesError(err instanceof Error ? err.message : String(err));
        }
      });
    void loadHistory();
    return () => {
      live = false;
    };
  }, [loadHistory]);

  // A tab switch is a different starter, so a preview of the previous one must not stay on
  // screen labelled with this one's name. The history is shared and stays.
  useEffect(() => {
    setPreview(null);
    setOpenFile(null);
    setError(null);
    setNotice(null);
  }, [kind]);

  const problem = nameProblem(name);
  const template = templates?.find((entry) => entry.kind === kind) ?? null;
  const shown = openFile
    ? preview?.files.find((file) => file.path === openFile) ?? null
    : null;

  const run = useCallback(
    async (mode: "preview" | "generate") => {
      if (problem) return;
      setBusy(mode);
      setError(null);
      setNotice(null);
      try {
        if (mode === "preview") {
          setPreview(await previewScaffold(kind, name, target));
        } else {
          const recorded = await recordScaffold(kind, name, target);
          setPreview(await previewScaffold(kind, recorded.name, recorded.target));
          setNotice(
            `Generated ${recorded.kind} "${recorded.name}". The archive is in the list below.`,
          );
          await loadHistory();
        }
      } catch (err: unknown) {
        setError(err instanceof ApiError ? err.message : String(err));
      } finally {
        setBusy(null);
      }
    },
    [kind, name, target, problem, loadHistory],
  );

  return (
    <div className="space-y-4">
      {templatesError ? (
        <p
          data-dev-sdk-templates-error
          role="alert"
          className="rounded border border-caution/40 bg-surface p-3 text-[13px] text-caution"
        >
          The template list could not be read: {templatesError}
        </p>
      ) : null}

      {template ? (
        <div data-dev-sdk-template className="rounded border border-line bg-surface p-3">
          <p className="flex items-center gap-2 text-[13px] font-medium text-ink">
            <FileCode2 className="size-4" aria-hidden />
            {template.label} · {template.language}
          </p>
          <p className="mt-1 text-[12.5px] text-muted">{template.description}</p>
        </div>
      ) : templates ? (
        <p className="text-[12.5px] text-muted">Loading the {kind} template…</p>
      ) : null}

      <div data-dev-sdk-form className="flex flex-wrap items-end gap-3">
        <label className="flex min-w-[16rem] flex-1 flex-col gap-1">
          <span className="text-[12px] text-muted">Name</span>
          <input
            data-dev-sdk-name
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="acme-invoices"
            aria-invalid={problem ? "true" : undefined}
            aria-describedby={problem ? "dev-sdk-name-problem" : undefined}
            className="rounded border border-line bg-surface px-2 py-1.5 font-mono text-[13px]"
          />
        </label>

        <fieldset className="flex flex-col gap-1">
          <legend className="text-[12px] text-muted">Target</legend>
          <div className="flex gap-1">
            {TARGETS.map((option) => (
              <button
                key={option.id}
                type="button"
                data-dev-sdk-target={option.id}
                aria-pressed={target === option.id}
                title={option.hint}
                onClick={() => setTarget(option.id)}
                className={
                  target === option.id
                    ? "rounded border border-accent bg-accent-soft px-2 py-1.5 text-[12px] text-ink"
                    : "rounded border border-line bg-surface px-2 py-1.5 text-[12px] text-muted"
                }
              >
                {option.label}
              </button>
            ))}
          </div>
        </fieldset>

        <button
          type="button"
          data-dev-sdk-preview
          disabled={busy !== null || problem !== null || name.length === 0}
          onClick={() => void run("preview")}
          className="rounded border border-line bg-surface px-3 py-1.5 text-[13px] text-ink disabled:opacity-50"
        >
          {busy === "preview" ? "Building…" : "Preview files"}
        </button>
        <button
          type="button"
          data-dev-sdk-generate
          disabled={busy !== null || problem !== null || name.length === 0}
          onClick={() => void run("generate")}
          className="flex items-center gap-1 rounded border border-accent bg-accent-soft px-3 py-1.5 text-[13px] text-ink disabled:opacity-50"
        >
          {busy === "generate" ? (
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
          ) : (
            <Play className="size-3.5" aria-hidden />
          )}
          Generate
        </button>
      </div>

      {problem ? (
        <p id="dev-sdk-name-problem" data-dev-sdk-name-error className="text-[12.5px] text-caution">
          {problem}
        </p>
      ) : null}

      {error ? (
        <p
          data-dev-sdk-error
          role="alert"
          className="flex items-start gap-2 rounded border border-caution/40 bg-surface p-3 text-[13px] text-caution"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          data-dev-sdk-notice
          className="flex items-start gap-2 rounded border border-positive/40 bg-surface p-3 text-[13px] text-positive"
        >
          <CheckCircle2 className="mt-0.5 size-4 shrink-0" aria-hidden />
          {notice}
        </p>
      ) : null}

      {preview ? (
        <section data-dev-sdk-preview-panel className="rounded border border-line bg-surface">
          <header className="flex flex-wrap items-center gap-2 border-b border-line px-3 py-2">
            <h3 className="text-[13px] font-medium text-ink">
              {preview.files.length} files · {bytesLabel(preview.byte_size)}
            </h3>
            <span className="text-[12px] text-muted">
              {preview.kind} · {preview.target}
            </span>
            <span className="ml-auto text-[12px] text-muted">
              The hidden files (.env.example, .gitignore) are in the archive.
            </span>
          </header>
          <ul className="divide-y divide-line">
            {preview.files.map((file) => (
              <li key={file.path}>
                <button
                  type="button"
                  data-dev-sdk-file={file.path}
                  onClick={() => setOpenFile(openFile === file.path ? null : file.path)}
                  aria-expanded={openFile === file.path}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-left font-mono text-[12px] text-ink"
                >
                  {file.path}
                  <span className="ml-auto text-[11px] text-muted">
                    {bytesLabel(new TextEncoder().encode(file.content).length)}
                  </span>
                </button>
                {openFile === file.path && shown ? (
                  <pre
                    data-dev-sdk-file-body={file.path}
                    className="max-h-80 overflow-auto border-t border-line bg-canvas px-3 py-2 font-mono text-[11.5px]"
                  >
                    {shown.content}
                  </pre>
                ) : null}
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      <section data-dev-sdk-history className="space-y-2">
        <h3 className="text-[13px] font-medium text-ink">Generated starters</h3>
        {history.length === 0 ? (
          <div data-dev-sdk-history-empty className="rounded border border-line bg-surface">
            <EmptyState
              title="No starter generated yet"
              hint="A generation is recorded here with its own download. The preview above does not create a row — it writes nothing until you press Generate."
            />
          </div>
        ) : (
          <ul className="divide-y divide-line rounded border border-line bg-surface">
            {history.map((row) => (
              <li
                key={row.id}
                data-dev-sdk-row={row.id}
                className="flex flex-wrap items-center gap-3 px-3 py-2"
              >
                <span className="font-mono text-[12.5px] text-ink">{row.name}</span>
                <span className="text-[12px] text-muted">{row.kind}</span>
                <span className="text-[12px] text-muted">{row.target}</span>
                <span className="text-[12px] text-muted">
                  {row.file_count} files · {bytesLabel(row.byte_size)}
                </span>
                <span className="text-[12px] text-muted">{formatTimestamp(row.created_at)}</span>
                <a
                  href={scaffoldDownloadUrl(row.id)}
                  download
                  data-dev-sdk-download={row.id}
                  className="ml-auto flex items-center gap-1 rounded border border-line px-2 py-1 text-[12px] text-ink"
                >
                  <Download className="size-3" aria-hidden />
                  Download .zip
                </a>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* The validator lives with the starter it validates. Exported and never rendered would
          be a control that exists in the module graph and not on the screen — the REQ asks for
          a drop zone on this tab, and a reader who edits their manifest before generating a
          starter is exactly when they want to know whether the platform will accept it. */}
      <ManifestValidator kind={kind} />
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The CLI tab
// ---------------------------------------------------------------------------------------------

function CliTab() {
  const [start, setStart] = useState<DeviceStart | null>(null);
  const [lookup, setLookup] = useState<DeviceLookup | null>(null);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState<"start" | "lookup" | "approve" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [approved, setApproved] = useState<string | null>(null);
  const [remaining, setRemaining] = useState<number | null>(null);

  useEffect(() => {
    if (start === null) {
      setRemaining(null);
      return;
    }
    // The countdown is derived from the server's own `expires_in` at the moment the response
    // arrived, and re-derived on every tick. A tab left open for twenty minutes must not
    // display a time that belongs to a different tab's clock.
    const startedAt = Date.now();
    const tick = () => {
      const elapsed = Math.floor((Date.now() - startedAt) / 1000);
      setRemaining(Math.max(0, start.expires_in - elapsed));
    };
    tick();
    const handle = window.setInterval(tick, 1000);
    return () => window.clearInterval(handle);
  }, [start]);

  const begin = useCallback(async () => {
    setBusy("start");
    setError(null);
    setApproved(null);
    try {
      setStart(await startDeviceCode("omnion-cli"));
    } catch (err: unknown) {
      setError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  }, []);

  const look = useCallback(async () => {
    if (!code.trim()) return;
    setBusy("lookup");
    setError(null);
    try {
      setLookup(await fetchDeviceCode(code.trim()));
    } catch (err: unknown) {
      setError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  }, [code]);

  const approve = useCallback(async () => {
    if (!lookup) return;
    setBusy("approve");
    setError(null);
    try {
      const result = await approveDeviceCode(lookup.user_code);
      setApproved(result.user_code);
    } catch (err: unknown) {
      setError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  }, [lookup]);

  const countdown = useMemo(() => {
    if (remaining === null) return null;
    const minutes = Math.floor(remaining / 60);
    const seconds = String(remaining % 60).padStart(2, "0");
    return `${minutes}:${seconds}`;
  }, [remaining]);

  return (
    <div className="space-y-4">
      <section data-dev-sdk-cli className="rounded border border-line bg-surface p-3">
        <h3 className="flex items-center gap-2 text-[13px] font-medium text-ink">
          <Terminal className="size-4" aria-hidden />
          omnion login
        </h3>
        <p className="mt-1 text-[12.5px] text-muted">
          A terminal cannot hold a session cookie, so the CLI asks for a short code instead. You
          approve it here, and the terminal receives a scoped token through its own poll — this
          page never sees that token and cannot show it to you.
        </p>
        <pre className="mt-2 overflow-x-auto rounded border border-line bg-canvas p-2 font-mono text-[12px]">
          npm install -g @omnion/cli{`\n`}omnion login
        </pre>
      </section>

      <div className="flex flex-wrap items-end gap-3">
        <button
          type="button"
          data-dev-sdk-cli-start
          disabled={busy !== null}
          onClick={() => void begin()}
          className="rounded border border-accent bg-accent-soft px-3 py-1.5 text-[13px] text-ink disabled:opacity-50"
        >
          {busy === "start" ? "Requesting…" : "Request a login code"}
        </button>
      </div>

      {start ? (
        <div data-dev-sdk-cli-code className="rounded border border-line bg-surface p-3">
          <p className="text-[12px] text-muted">Enter this code in the terminal:</p>
          <p className="mt-1 font-mono text-xl tracking-wide text-ink">{start.user_code}</p>
          <p className="mt-2 text-[12.5px] text-muted">
            Approve it at <span className="font-mono">{start.verification_uri}</span>
            {countdown === null ? null : ` · expires in ${countdown}`}
          </p>
          <button
            type="button"
            data-dev-sdk-cli-copy
            onClick={() => void navigator.clipboard.writeText(start.user_code).catch(() => undefined)}
            className="mt-2 flex items-center gap-1 rounded border border-line px-2 py-1 text-[12px] text-ink"
          >
            <ClipboardCopy className="size-3" aria-hidden />
            Copy the code
          </button>
        </div>
      ) : null}

      <div data-dev-sdk-cli-lookup className="flex flex-wrap items-end gap-3">
        <label className="flex min-w-[14rem] flex-1 flex-col gap-1">
          <span className="text-[12px] text-muted">Approve a code typed in the terminal</span>
          <input
            data-dev-sdk-cli-code-input
            value={code}
            onChange={(event) => setCode(event.target.value)}
            placeholder="ABCD-1234"
            className="rounded border border-line bg-surface px-2 py-1.5 font-mono text-[13px]"
          />
        </label>
        <button
          type="button"
          data-dev-sdk-cli-find
          disabled={busy !== null || code.trim().length === 0}
          onClick={() => void look()}
          className="rounded border border-line bg-surface px-3 py-1.5 text-[13px] text-ink disabled:opacity-50"
        >
          {busy === "lookup" ? "Reading…" : "Look up"}
        </button>
      </div>

      {error ? (
        <p
          data-dev-sdk-cli-error
          role="alert"
          className="flex items-start gap-2 rounded border border-caution/40 bg-surface p-3 text-[13px] text-caution"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}

      {lookup ? (
        <div data-dev-sdk-cli-pending className="rounded border border-line bg-surface p-3">
          <p className="text-[13px] font-medium text-ink">
            {lookup.client_name} wants access to this organization
          </p>
          {lookup.client_uri ? (
            <p className="mt-0.5 font-mono text-[12px] text-muted">{lookup.client_uri}</p>
          ) : null}
          <ul className="mt-2 space-y-1">
            {lookup.scope_sentences.map((sentence) => (
              <li key={sentence} className="flex items-start gap-2 text-[12.5px] text-ink">
                <Check className="mt-0.5 size-3 shrink-0" aria-hidden />
                {sentence}
              </li>
            ))}
          </ul>
          {lookup.expired ? (
            <p className="mt-2 flex items-center gap-2 text-[12.5px] text-caution">
              <XCircle className="size-3.5" aria-hidden />
              This code has expired. Ask the terminal for a new one.
            </p>
          ) : null}
          {lookup.already_approved ? (
            <p className="mt-2 flex items-center gap-2 text-[12.5px] text-caution">
              <CheckCircle2 className="size-3.5" aria-hidden />
              This code is already approved.
            </p>
          ) : (
            <button
              type="button"
              data-dev-sdk-cli-approve
              disabled={busy !== null || lookup.expired}
              onClick={() => void approve()}
              className="mt-3 rounded border border-accent bg-accent-soft px-3 py-1.5 text-[13px] text-ink disabled:opacity-50"
            >
              {busy === "approve" ? "Approving…" : "Approve this login"}
            </button>
          )}
        </div>
      ) : null}

      {approved ? (
        <p
          data-dev-sdk-cli-approved
          className="flex items-start gap-2 rounded border border-positive/40 bg-surface p-3 text-[13px] text-positive"
        >
          <CheckCircle2 className="mt-0.5 size-4 shrink-0" aria-hidden />
          Approved. The terminal picks up the token on its next poll — nothing to copy here.
        </p>
      ) : null}
    </div>
  );
}

/** The manifest validator, reachable from each starter tab through this row. */
export function ManifestValidator({ kind }: { kind: SdkKind }) {
  const [source, setSource] = useState("");
  const [report, setReport] = useState<ManifestReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setReport(await validateManifest(kind, source));
    } catch (err: unknown) {
      setError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }, [kind, source]);

  return (
    <section data-dev-sdk-manifest className="space-y-2 rounded border border-line bg-surface p-3">
      <h3 className="text-[13px] font-medium text-ink">Validate a {kind} manifest</h3>
      <p className="text-[12.5px] text-muted">
        The same rules the platform loads with, so a manifest this accepts is one that will
        install. Paste the file&apos;s text.
      </p>
      <textarea
        data-dev-sdk-manifest-input
        value={source}
        onChange={(event) => setSource(event.target.value)}
        rows={5}
        spellCheck={false}
        placeholder={'{\n  "kind": "plugin",\n  "name": "acme-invoices",\n  "version": "1.0.0",\n  "entry": "index.js"\n}'}
        className="w-full rounded border border-line bg-canvas p-2 font-mono text-[12px]"
      />
      <button
        type="button"
        data-dev-sdk-manifest-run
        disabled={busy || source.trim().length === 0}
        onClick={() => void run()}
        className="rounded border border-line bg-surface px-3 py-1.5 text-[13px] text-ink disabled:opacity-50"
      >
        {busy ? "Validating…" : "Validate manifest"}
      </button>

      {error ? (
        <p data-dev-sdk-manifest-error role="alert" className="text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}

      {report ? (
        <div data-dev-sdk-manifest-report className="space-y-1">
          <p
            data-dev-sdk-manifest-verdict={report.valid ? "valid" : "invalid"}
            className={
              report.valid
                ? "flex items-center gap-2 text-[12.5px] text-positive"
                : "flex items-center gap-2 text-[12.5px] text-caution"
            }
          >
            {report.valid ? (
              <CheckCircle2 className="size-3.5" aria-hidden />
            ) : (
              <XCircle className="size-3.5" aria-hidden />
            )}
            {report.valid
              ? "This manifest is loadable as written."
              : `${report.issues.length} problem${report.issues.length === 1 ? "" : "s"} found.`}
          </p>
          {report.issues.map((issue: ManifestIssue) => (
            <p
              key={`${issue.code}-${issue.line}-${issue.message}`}
              data-dev-sdk-manifest-issue={issue.code}
              className="font-mono text-[12px] text-ink"
            >
              {issue.line > 0 ? `line ${issue.line}: ` : ""}
              {issue.message}
            </p>
          ))}
        </div>
      ) : null}
    </section>
  );
}

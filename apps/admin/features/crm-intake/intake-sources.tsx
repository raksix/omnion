"use client";

/**
 * `/crm/settings/intake` — the capture surfaces and the editor for one (REQ-117, slice 1).
 *
 * The editor is four sections, and each one exists because a wrong value in it loses business
 * rather than looking untidy:
 *
 * * **Surface.** A keyed endpoint's key is issued once and shown once. The screen says that
 *   *before* the operator creates the source, and it never re-reveals the key afterwards — a
 *   key that reappears on refresh is a key that ends up in a screenshot. Rotation is the way
 *   back in, and it is a confirmation, not a button.
 * * **Mapping.** A required target with no source is refused by the API at save; the screen
 *   marks the offending line itself so the refusal arrives before the save rather than after.
 * * **Rules.** The dedupe policy is spelled out in its consequence ("this files the
 *   submission as a duplicate instead of linking it") because `reject_duplicate` vs `link` is
 *   the difference between an operator's queue and an empty one.
 * * **Test mapping.** Writes nothing, and says so — a preview that stored rows would be a
 *   second capture path with a worse authentication story than the endpoint itself.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import {
  Ban,
  Check,
  CircleAlert,
  Copy,
  KeyRound,
  Loader2,
  Mail,
  Plus,
  RefreshCw,
  RotateCw,
  Search,
  Trash2,
  TriangleAlert,
  Wand2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  DEDUPE_POLICIES,
  DEDUPE_POLICY_LABEL,
  editorAfterRefresh,
  MAPPING_TARGETS,
  MAPPING_TARGET_LABEL,
  MAPPING_TRANSFORMS,
  MAPPING_TRANSFORM_LABEL,
  SOURCE_KIND_LABEL,
  SOURCE_KINDS,
} from "@/lib/crm-intake";
import {
  createIntakeSource,
  deleteIntakeSource,
  fetchAutoresponderTemplates,
  fetchIntakeSource,
  fetchIntakeSources,
  intakeEndpointUrl,
  previewAutoresponder,
  rotateIntakeKey,
  sweepRetention,
  testIntakeMapping,
  updateIntakeSource,
  type AutoresponderPreview,
  type AutoresponderTemplates,
  type IntakeSource,
  type MappingLine,
  type MappingPreview,
  type RetentionSweep,
} from "@/lib/crm-intake-api";

/** The sample a `Test mapping` starts from, so the button has something to run. */
const SAMPLE_PAYLOAD = `{
  "name": "Ada Lovelace",
  "email": "ada@example.com",
  "phone": "+44 20 7946 0000",
  "company": "Analytical Engines Ltd",
  "message": "Could you quote for 40 seats?"
}`;

type EditorState = {
  id: string;
  name: string;
  kind: string;
  mapping: MappingLine[];
  requiredTargets: string[];
  consentRequired: boolean;
  consentText: string;
  dedupePolicy: string;
  rateLimitPerHour: number;
  active: boolean;
  /**
   * The autoresponder column, as the editor holds it.
   *
   * Typed as a record rather than as a form because it is *the column*: the server reads four
   * keys out of it and ignores the rest, so a screen that mapped it to its own shape would
   * have to remember that an unknown key is a disabled autoresponder, not an error.
   */
  autoresponder: Record<string, unknown>;
};

function editorOf(source: IntakeSource): EditorState {
  return {
    id: source.id,
    name: source.name,
    kind: source.kind,
    mapping: source.mapping,
    requiredTargets: source.required_targets,
    consentRequired: source.consent_required,
    consentText: source.consent_text ?? "",
    dedupePolicy: source.dedupe_policy,
    rateLimitPerHour: source.rate_limit_per_hour,
    active: source.active,
    autoresponder: source.autoresponder ?? {},
  };
}

/** The default mapping a new keyed endpoint starts with: e-mail or phone, name, message. */
function defaultMapping(): MappingLine[] {
  return [
    { target: "email", source: "email", transforms: ["trim", "lowercase"], required: false, fallback: null },
    { target: "phone", source: "phone", transforms: ["trim", "e164_lite"], required: false, fallback: null },
    { target: "first_name", source: "name", transforms: ["split_full_name"], required: false, fallback: null },
    { target: "message", source: "message", transforms: ["trim"], required: false, fallback: null },
  ];
}

export function IntakeSources() {
  const router = useRouter();
  const [rows, setRows] = useState<IntakeSource[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<EditorState | null>(null);
  /** The source being fetched for the editor, so one row shows a spinner and the rest do not. */
  const [opening, setOpening] = useState<string | null>(null);
  const [revealedKey, setRevealedKey] = useState<{ name: string; key: string } | null>(null);
  const [rotating, setRotating] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const sources = await fetchIntakeSources();
      setRows(sources);
      // A refresh must not throw away a half-edited mapping: the editor only closes when the
      // source it is editing is gone from the list, and never re-reads the row into it. Both
      // halves are in one pure function, and the reason each is true is not obvious from a
      // `setState` callback — see `editorAfterRefresh` in `@/lib/crm-intake`.
      setEditing((current) => {
        const verdict = editorAfterRefresh(current, sources);
        return verdict.action === "close" ? null : verdict.editing;
      });
    } catch (caught) {
      setRows(null);
      setError(caught instanceof ApiError ? caught.message : "The sources could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  /**
   * Open one source for editing, from the server rather than from the list row.
   *
   * The list row is a *summary* — it carries the name, the kind, the health and the mapping,
   * because the table needs them to draw. It is not a promise that it carries everything the
   * editor saves, and the day it stopped carrying one of them the save would quietly drop that
   * field: the editor would put its own (unchanged) copy on the wire, the API would accept it,
   * and the column would be overwritten with the value the list happened to carry. A read
   * endpoint already exists for exactly one source; this is its first caller, and the reason
   * it had none is that the row was always "good enough" for a screen nobody was looking at
   * until now.
   */
  const openEditor = async (id: string) => {
    setOpening(id);
    setError(null);
    try {
      setEditing(editorOf(await fetchIntakeSource(id)));
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The source could not be opened.");
    } finally {
      setOpening(null);
    }
  };

  useEffect(() => {
    void load();
  }, [load]);

  const create = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const created = await createIntakeSource({
        name: "Website quote form",
        kind: "endpoint",
        mapping: defaultMapping(),
        required_targets: [],
        dedupe_policy: "link",
        rate_limit_per_hour: 30,
        active: true,
      });
      setRows((previous) => [...(previous ?? []), created]);
      setEditing(editorOf(created));
      if (created.endpoint_key) {
        setRevealedKey({ name: created.name, key: created.endpoint_key });
      }
      setNotice("The source is created. Its key is shown once — copy it now.");
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The source could not be created.");
    } finally {
      setBusy(false);
    }
  };

  const save = async (state: EditorState) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await updateIntakeSource(state.id, {
        name: state.name,
        mapping: state.mapping,
        required_targets: state.requiredTargets,
        consent_required: state.consentRequired,
        consent_text: state.consentText.trim() || null,
        dedupe_policy: state.dedupePolicy,
        rate_limit_per_hour: state.rateLimitPerHour,
        active: state.active,
        // The autoresponder is saved in the same write as everything else rather than on its
        // own button: an operator who turns it on and edits its delay in two presses is owed
        // one atomic change, not a window where the flag is on and the delay is the old one.
        autoresponder: state.autoresponder,
      });
      setRows((previous) => (previous ?? []).map((row) => (row.id === saved.id ? saved : row)));
      setEditing(editorOf(saved));
      setNotice("The source was saved.");
    } catch (caught) {
      // A mapping that drops a required target is refused by the API, and its message names
      // the field — so the screen shows it rather than replacing it with a generic sentence.
      setError(caught instanceof ApiError ? caught.message : "The source could not be saved.");
    } finally {
      setBusy(false);
    }
  };

  const rotate = async (source: IntakeSource) => {
    if (!confirm(`Issue a new key for "${source.name}"? The current key stops working immediately.`)) {
      return;
    }
    setRotating(true);
    setError(null);
    setNotice(null);
    try {
      const rotated = await rotateIntakeKey(source.id);
      setRows((previous) => (previous ?? []).map((row) => (row.id === rotated.id ? rotated : row)));
      setRevealedKey(
        rotated.endpoint_key ? { name: rotated.name, key: rotated.endpoint_key } : null,
      );
      setNotice("A new key was issued. The previous one no longer works.");
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The key could not be rotated.");
    } finally {
      setRotating(false);
    }
  };

  const remove = async (source: IntakeSource) => {
    if (
      !confirm(
        `Delete "${source.name}"? The submissions it already captured keep their rows, with no source attached.`,
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await deleteIntakeSource(source.id);
      setRows((previous) => (previous ?? []).filter((row) => row.id !== source.id));
      setEditing((current) => (current?.id === source.id ? null : current));
      setNotice("The source is deleted. Its leads remain in the inbox.");
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The source could not be deleted.");
    } finally {
      setBusy(false);
    }
  };

  const toggleActive = async (source: IntakeSource) => {
    setBusy(true);
    setError(null);
    try {
      const saved = await updateIntakeSource(source.id, { active: !source.active });
      setRows((previous) => (previous ?? []).map((row) => (row.id === saved.id ? saved : row)));
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The source could not be updated.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-testid="crm-intake-sources">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-ink">Intake sources</h2>
          <p className="mt-1 max-w-2xl text-[12.5px] text-muted">
            Where a lead comes from. A source is either a bound form or a keyed endpoint you post
            to; either way the platform stores the submission, judges it and files the verdict.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => void load()}
            data-sources-refresh
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-source-create
            data-qa-guard="crm-intake-depth"
            disabled={busy}
            onClick={() => void create()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-2.5 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
          >
            <Plus className="size-3.5" aria-hidden />
            New source
          </button>
        </div>
      </header>

      {notice ? (
        <p
          role="status"
          data-sources-notice
          className="rounded-lg border border-positive/40 bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          role="alert"
          data-sources-error
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void load()} className="text-accent-strong hover:underline">
            Retry
          </button>
        </div>
      ) : null}

      {/* The one-time key. It is not a section: it is a moment, and it disappears when the
          operator closes it — because a key that stays on the screen is a key that gets
          screenshotted. */}
      {revealedKey ? (
        <div
          data-key-reveal
          role="status"
          className="rounded-xl border border-caution/50 bg-caution-soft px-4 py-3 text-[12.5px]"
        >
          <p className="flex items-center gap-1.5 font-medium text-caution">
            <KeyRound className="size-3.5" aria-hidden />
            The key for “{revealedKey.name}” — shown once, never again
          </p>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            <code
              data-key-value
              className="min-w-0 flex-1 truncate rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px]"
            >
              {revealedKey.key}
            </code>
            <button
              type="button"
              data-key-copy
              onClick={() => {
                void navigator.clipboard?.writeText(revealedKey.key);
                setNotice("The key is on the clipboard. Store it in your integration's secret store.");
              }}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] transition hover:text-ink"
            >
              <Copy className="size-3.5" aria-hidden />
              Copy key
            </button>
            {/* The REQ's inbox row has asked for `Copy intake URL` since it was written, and
                the honest place for it is **here and only here**. The endpoint's URL is its
                origin plus its path plus its key — and the key exists in this browser only
                during this moment, because `endpoint_key_hash` is what the server stores and
                the clear value is never re-issued. A `Copy intake URL` button anywhere else
                would copy a URL that answers `401`, which is worse than no button: the
                operator pastes it into their integration and spends an afternoon on a
                capture path that cannot capture. So the word is honoured where it is true
                and the source list's hints were corrected instead of promising a URL it
                cannot rebuild. */}
            <button
              type="button"
              data-key-copy-url
              onClick={() => {
                void navigator.clipboard?.writeText(intakeEndpointUrl(revealedKey.key));
                setNotice(
                  "The capture URL is on the clipboard. Paste it into your integration together with the key.",
                );
              }}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] transition hover:text-ink"
            >
              <Copy className="size-3.5" aria-hidden />
              Copy intake URL
            </button>
            <button
              type="button"
              onClick={() => setRevealedKey(null)}
              className="inline-flex items-center gap-1.5 rounded-lg px-2 py-1.5 text-[12.5px] text-muted hover:underline"
            >
              <X className="size-3.5" aria-hidden />
              I stored it
            </button>
          </div>
          <p className="mt-2 break-all text-[11.5px] text-caution">
            POST submissions to <code>{intakeEndpointUrl(revealedKey.key)}</code>. A `x-idempotency-key`
            header makes a retry return the lead the first attempt wrote.
          </p>
        </div>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {loading && rows === null ? (
          <LoadingTable columns={5} rows={3} />
        ) : rows === null || rows.length === 0 ? (
          <EmptyState
            title="No intake source yet"
            // **The hint used to promise a capture URL, and the screen cannot rebuild one.**
            // The endpoint's URL *contains its key*; the server stores only the key's hash and
            // the clear value is issued once and shown once. So the URL belongs to the one-time
            // key panel above — which is exactly when an operator needs to paste it — and an
            // empty state that said "it appears here" and then never showed one taught the
            // operator that this screen had lost the value.
            hint="A lead cannot arrive from nowhere. Create a keyed endpoint, or bind one of the site's forms to a source — its capture URL appears once, in the key panel above, together with the key."
            action={
              <button
                type="button"
                data-source-create-empty
                data-qa-guard="crm-intake-depth"
                disabled={busy}
                onClick={() => void create()}
                className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
              >
                <Plus className="size-3.5" aria-hidden />
                Create the first source
              </button>
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table data-sources-table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="px-3 py-2.5 font-medium">Source</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Kind</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Dedupe</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Last received</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">State</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Actions</th>
                </tr>
              </thead>
              <tbody>
                {(rows ?? []).map((source) => (
                  <tr key={source.id} data-source-row={source.id} className="border-b border-line/60 align-top last:border-0">
                    <td className="px-3 py-2.5">
                      <span className="block font-medium">{source.name}</span>
                      {source.endpoint_key_hint ? (
                        <span className="block font-mono text-[11px] text-muted">
                          key ····{source.endpoint_key_hint}
                        </span>
                      ) : null}
                      {source.binding_broken ? (
                        <span
                          data-source-broken
                          className="mt-1 flex items-start gap-1.5 rounded-md border border-caution/40 bg-caution-soft px-2 py-1 text-[11.5px] text-caution"
                        >
                          <CircleAlert className="mt-0.5 size-3 shrink-0" aria-hidden />
                          Broken mapping: the bound form no longer has{" "}
                          {source.broken_mappings.join(", ") || "a mapped field"}. Submissions still
                          arrive; these fields come through empty.
                        </span>
                      ) : null}
                      {source.last_error ? (
                        <span className="mt-1 block text-[11.5px] text-red-700">{source.last_error}</span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2.5 text-muted">
                      {SOURCE_KIND_LABEL[source.kind] ?? source.kind}
                    </td>
                    <td className="px-3 py-2.5 text-muted">
                      {DEDUPE_POLICY_LABEL[source.dedupe_policy] ?? source.dedupe_policy}
                    </td>
                    <td className="px-3 py-2.5 text-muted tabular-nums">
                      {source.last_received_at
                        ? new Date(source.last_received_at).toLocaleString()
                        : "Never"}
                    </td>
                    <td className="px-3 py-2.5">
                      <span
                        data-source-state={source.active ? "active" : "paused"}
                        className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
                          source.active ? "bg-positive-soft text-positive" : "bg-quiet-soft text-muted"
                        }`}
                      >
                        {source.active ? "Active" : "Paused"}
                      </span>
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <button
                          type="button"
                          data-source-edit={source.id}
                          onClick={() => void openEditor(source.id)}
                          className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:text-ink"
                        >
                          {opening === source.id ? (
                            <Loader2 className="size-3 animate-spin" aria-hidden />
                          ) : null}
                          Edit
                        </button>
                        <button
                          type="button"
                          data-source-toggle={source.id}
                          data-qa-guard="crm-intake-depth"
                          disabled={busy}
                          onClick={() => void toggleActive(source)}
                          className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted transition hover:text-ink disabled:opacity-50"
                        >
                          {source.active ? "Pause" : "Resume"}
                        </button>
                        {source.kind === "endpoint" ? (
                          <button
                            type="button"
                            data-source-rotate={source.id}
                            data-qa-guard="crm-intake-depth"
                            disabled={rotating}
                            onClick={() => void rotate(source)}
                            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted transition hover:text-ink disabled:opacity-50"
                          >
                            <RotateCw className="size-3" aria-hidden />
                            Rotate key
                          </button>
                        ) : null}
                        <button
                          type="button"
                          data-source-delete={source.id}
                          data-qa-guard="crm-intake-depth"
                          disabled={busy}
                          onClick={() => void remove(source)}
                          className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-red-600 transition hover:border-red-500/40 disabled:opacity-50"
                        >
                          <Trash2 className="size-3" aria-hidden />
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {editing ? (
        <SourceEditor
          state={editing}
          busy={busy}
          onChange={setEditing}
          onSave={() => void save(editing)}
          onClose={() => setEditing(null)}
        />
      ) : null}

      {/* The retention promise lives here, next to the sources whose submissions it governs:
          a window set on a different screen is a window nobody thinks to look for. */}
      <RetentionSection />

      <p className="text-[11.5px] text-muted">
        Assignment rules and SLA policies have their own screens
        (<button
          type="button"
          onClick={() => router.replace("/crm/settings/assignment")}
          className="text-accent-strong hover:underline"
        >
          assignment
        </button>
        {", "}
        <button
          type="button"
          onClick={() => router.replace("/crm/settings/sla")}
          className="text-accent-strong hover:underline"
        >
          SLA
        </button>
        ), each with its own screen.
      </p>
    </div>
  );
}

/** The editor for one source: surface, mapping, rules and a preview that writes nothing. */
/**
 * The autoresponder section of the source editor.
 *
 * It is the last control on the path a lead takes, and it is the only one that *sends
 * something to a stranger*, so the screen is built around three questions an operator actually
 * has: what does it say, when does it go, and what happens if it is wrong.
 *
 * * **What it says.** The template list is the server's, and the preview is the server's render
 *   of the server's decision. The client never substitutes `{{name}}` itself, because the two
 *   implementations would agree until they did not, and the only place that shows up is a
 *   visitor's inbox.
 * * **When it goes.** The delay is the one control whose effect is invisible until minutes later,
 *   so the preview answers it in the same sentence as the rest.
 * * **What happens if it is wrong.** An enabled-but-empty autoresponder is the dangerous state —
 *   it eats the lead's single reply and reports nothing. The screen says so before the save, in
 *   the same words the store's `InvalidTemplate` uses.
 */
function AutoresponderSection({
  sourceName,
  value,
  onChange,
}: {
  sourceName: string;
  value: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
}) {
  const [templates, setTemplates] = useState<AutoresponderTemplates | null>(null);
  const [templatesError, setTemplatesError] = useState<string | null>(null);
  const [preview, setPreview] = useState<AutoresponderPreview | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);

  const enabled = value.enabled === true;
  const template = typeof value.template === "string" ? value.template : "";
  const subject = typeof value.subject === "string" ? value.subject : "";
  const body = typeof value.template_body === "string" ? value.template_body : "";
  const delay = typeof value.delay_minutes === "number" ? value.delay_minutes : 0;

  /** The client-side mirror of `is_configured`, so the warning arrives before the save. */
  const empty = enabled && (subject.trim() === "" || body.trim() === "");

  useEffect(() => {
    let live = true;
    fetchAutoresponderTemplates()
      .then((answer) => {
        if (live) setTemplates(answer);
      })
      .catch((caught: unknown) => {
        // The section still works without the list: the delay and the hand-written text are
        // editable, and the preview runs server-side either way. A failed list is a degraded
        // control, not a dead screen.
        if (live) setTemplatesError(caught instanceof ApiError ? caught.message : "The templates could not be read.");
      });
    return () => {
      live = false;
    };
  }, []);

  const pick = (name: string) => {
    const chosen = templates?.templates.find((entry) => entry.name === name);
    onChange({
      ...value,
      template: name,
      // Picking a template writes its prose in. It does not *pin* it: the two keys are stored
      // side by side so an operator can edit the body and keep the name for the trail.
      subject: chosen?.subject ?? subject,
      template_body: chosen?.body ?? body,
    });
    setPreview(null);
  };

  const runPreview = async () => {
    setChecking(true);
    setPreviewError(null);
    try {
      setPreview(
        await previewAutoresponder({
          autoresponder: { ...value, enabled: true },
          // A visitor's name from the mapping's own sample, so the greeting is the real one.
          first_name: "Ada",
          address: "ada@example.com",
          product_interest: "40 seats",
          source_name: sourceName,
        }),
      );
    } catch (caught) {
      setPreviewError(caught instanceof ApiError ? caught.message : "The preview could not be run.");
    } finally {
      setChecking(false);
    }
  };

  return (
    <div data-autoresponder-section className="rounded-lg border border-line bg-canvas p-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h4 className="flex items-center gap-1.5 text-[12.5px] font-semibold">
            <Mail className="size-3.5" aria-hidden />
            The reply the visitor gets
          </h4>
          <p className="mt-0.5 max-w-prose text-[11.5px] text-muted">
            One message per accepted lead, whatever the submission is retried. A spam or rejected
            submission sends nothing — telling a spammer their address works is the one thing a
            filter must never do.
          </p>
        </div>
        <label className="flex items-center gap-2 text-[12px] text-ink">
          <input
            type="checkbox"
            data-autoresponder-enabled
            checked={enabled}
            onChange={(event) => onChange({ ...value, enabled: event.target.checked })}
            className="size-3.5"
          />
          Answer this source automatically
        </label>
      </div>

      {empty ? (
        <p
          data-autoresponder-empty-warning
          role="alert"
          className="mt-2 flex items-start gap-1.5 rounded-lg border border-caution/40 bg-caution-soft px-2.5 py-2 text-[12px] text-caution"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          It is switched on with no text, so every accepted lead would get nothing and the store
          would record that as a misconfiguration. Pick a template below or switch it off.
        </p>
      ) : null}

      {enabled ? (
        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span className="font-medium">Template</span>
            <select
              data-autoresponder-template
              value={template}
              onChange={(event) => pick(event.target.value)}
              className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
            >
              <option value="">Hand-written</option>
              {templates?.templates.map((entry) => (
                <option key={entry.name} value={entry.name}>
                  {entry.name.replace(/_/g, " ")}
                </option>
              ))}
            </select>
            {templatesError ? (
              <span data-autoresponder-templates-error className="text-caution">
                {templatesError} The delay and your own text still work.
              </span>
            ) : null}
          </label>

          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span className="font-medium">Send it after</span>
            <div className="flex items-center gap-2">
              <input
                type="number"
                min={0}
                max={templates?.max_delay_minutes ?? 10080}
                data-autoresponder-delay
                value={delay}
                onChange={(event) =>
                  onChange({ ...value, delay_minutes: Math.max(0, Number(event.target.value) || 0) })
                }
                className="w-24 rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
              />
              <span className="text-[11.5px] text-muted">
                {delay === 0 ? "minutes (immediately)" : `minute${delay === 1 ? "" : "s"}`}
              </span>
            </div>
            <span>
              A delay is a reservation, not a sleep: the lead is claimed now and a worker sends
              the message when the time comes, so a restart does not lose it and nobody gets a
              second one.
            </span>
          </label>

          <label className="flex flex-col gap-1 text-[11.5px] text-muted sm:col-span-2">
            <span className="font-medium">Subject</span>
            <input
              data-autoresponder-subject
              value={subject}
              onChange={(event) => onChange({ ...value, subject: event.target.value })}
              className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
            />
          </label>
          <label className="flex flex-col gap-1 text-[11.5px] text-muted sm:col-span-2">
            <span className="font-medium">Body</span>
            <textarea
              rows={6}
              data-autoresponder-body
              value={body}
              onChange={(event) => onChange({ ...value, template_body: event.target.value })}
              className="w-full rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
            />
            {templates?.placeholders.length ? (
              <span className="flex flex-wrap items-center gap-1">
                {templates.placeholders.map((placeholder) => (
                  <button
                    key={placeholder.token}
                    type="button"
                    title={placeholder.renders}
                    data-autoresponder-placeholder={placeholder.token}
                    onClick={() => onChange({ ...value, template_body: `${body}${placeholder.token}` })}
                    className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted hover:text-ink"
                  >
                    {placeholder.token}
                  </button>
                ))}
                <span>click to add one</span>
              </span>
            ) : null}
          </label>

          <div className="sm:col-span-2">
            <button
              type="button"
              data-autoresponder-preview
              disabled={checking}
              onClick={() => void runPreview()}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:text-ink disabled:opacity-50"
            >
              {checking ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <Wand2 className="size-3.5" aria-hidden />
              )}
              Show it to a sample visitor
            </button>
            <p className="mt-1 text-[11.5px] text-muted">
              Renders on the server, exactly as the send path will, and sends nothing.
            </p>

            {previewError ? (
              <p role="alert" data-autoresponder-preview-error className="mt-2 text-[12px] text-red-700">
                {previewError}
              </p>
            ) : null}
            {preview ? (
              <div
                data-autoresponder-preview
                className="mt-2 rounded-lg border border-line bg-surface p-3 text-[12px]"
              >
                {preview.verdict === "ready" ? (
                  <>
                    <p className="flex flex-wrap items-center gap-2">
                      <span className="inline-flex items-center gap-1 rounded-full bg-positive-soft px-2 py-0.5 text-[11px] font-medium text-positive">
                        <Check className="size-3" aria-hidden />
                        {preview.delayed ? "Held until the delay has passed" : "Sent as soon as the lead is accepted"}
                      </span>
                    </p>
                    {preview.due_at ? (
                      <p data-autoresponder-due className="mt-1 text-muted">
                        Goes out around {new Date(preview.due_at).toLocaleString()}.
                      </p>
                    ) : null}
                    <p className="mt-2 font-medium">{preview.subject}</p>
                    <p className="mt-1 whitespace-pre-wrap text-muted">{preview.body}</p>
                  </>
                ) : (
                  <p data-autoresponder-preview-empty className="text-caution">
                    {preview.verdict === "invalid_template"
                      ? "The text does not render to a message, so the store would refuse the send and record why. Fill in a subject and a body."
                      : preview.verdict === "no_address"
                        ? "The sample visitor has no address, so there is nothing to send to. That is also what a submission whose mapping loses the e-mail field would do."
                        : "This autoresponder would send nothing."}
                  </p>
                )}
                {preview.unfilled.length > 0 ? (
                  <p data-autoresponder-unfilled className="mt-2 text-caution">
                    {preview.unfilled.join(", ")} is not a placeholder this renderer knows, so it
                    arrives as an empty space. Use {"{{name}}"}, {"{{source}}"} or {"{{product}}"}.
                  </p>
                ) : null}
              </div>
            ) : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

/**
 * The retention window, and the one control that erases.
 *
 * The endpoint existed for two ticks with nothing calling it, which is the definition of a
 * dead API: the REQ promises "payloads are size-capped, retention is configurable with an
 * archive sweep", and an operator who read that promise had no way to keep it. The sweep
 * also has a property no other button here does — **there is no undo and no second copy.**
 * Deleting a source keeps its leads; rotating a key can be rotated again; this one clears
 * the only stored body of what somebody wrote to the business. So the screen reads the
 * count with a dry run first, and the erase button does not exist until a dry run has
 * answered a number for *this* window. Change the window and it disappears again, because
 * the number that justified the press was for a different window.
 */
function RetentionSection() {
  const [days, setDays] = useState(730);
  const [count, setCount] = useState<RetentionSweep | null>(null);
  const [done, setDone] = useState<RetentionSweep | null>(null);
  const [checking, setChecking] = useState(false);
  const [sweeping, setSweeping] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // A count is a statement about ONE window. Editing the number invalidates it, so the
  // answer that could authorise the press is not allowed to survive the edit.
  const counted = count?.retention_days === days;

  const runCount = async () => {
    setChecking(true);
    setError(null);
    setDone(null);
    try {
      setCount(await sweepRetention({ retentionDays: days, dryRun: true }));
    } catch (caught) {
      setCount(null);
      setError(
        caught instanceof ApiError ? caught.message : "The stored bodies could not be counted.",
      );
    } finally {
      setChecking(false);
    }
  };

  const runSweep = async () => {
    setSweeping(true);
    setError(null);
    try {
      const answer = await sweepRetention({ retentionDays: days });
      setDone(answer);
      // The count is spent: it described the state before the press, and leaving it on
      // screen would invite a second press against a stale number.
      setCount(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The sweep could not be run.");
    } finally {
      setSweeping(false);
    }
  };

  return (
    <section
      data-retention-section
      className="rounded-lg border border-line bg-canvas p-3 text-[12.5px]"
    >
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h3 className="flex items-center gap-1.5 text-[12.5px] font-semibold">
            <Trash2 className="size-3.5" aria-hidden />
            How long a submission body is kept
          </h3>
          <p className="mt-0.5 max-w-prose text-[11.5px] text-muted">
            After this many days the stored submission body is cleared. The lead itself, its
            routing, its response times and its whole timeline stay — an inbox that forgot
            what it answered is worse than one that forgot the wording. A single lead is
            deleted outright from its detail screen, at any age.
          </p>
        </div>
      </div>

      <div className="mt-3 flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Clear bodies older than (days)</span>
          <input
            type="number"
            min={1}
            max={3650}
            data-retention-days
            value={days}
            onChange={(event) => {
              const next = Math.min(3650, Math.max(1, Number(event.target.value) || 1));
              setDays(next);
              setCount(null);
              setDone(null);
            }}
            className="w-32 rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
          />
        </label>

        <button
          type="button"
          data-retention-count
          disabled={checking || sweeping}
          onClick={() => void runCount()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:text-ink disabled:opacity-50"
        >
          {checking ? (
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
          ) : (
            <Search className="size-3.5" aria-hidden />
          )}
          Count what a sweep would clear
        </button>
      </div>

      {error ? (
        <p role="alert" data-retention-error className="mt-2 text-[12px] text-red-700">
          {error}
        </p>
      ) : null}

      {counted ? (
        <div data-retention-count className="mt-2 rounded-lg border border-line bg-surface p-3">
          {count && count.archived > 0 ? (
            <>
              <p className="flex items-center gap-1.5 font-medium">
                <TriangleAlert className="size-3.5 text-caution" aria-hidden />
                {count.archived} stored bod{count.archived === 1 ? "y is" : "ies are"} older than{" "}
                {count.retention_days} days
              </p>
              <p className="mt-1 text-[11.5px] text-muted">
                Clearing them cannot be undone and there is no second copy. The lead rows, their
                response times and their timelines are not touched.
              </p>
              <button
                type="button"
                data-retention-sweep
                disabled={sweeping}
                onClick={() => void runSweep()}
                className="mt-2 inline-flex items-center gap-1.5 rounded-lg border border-red-500/50 bg-red-500/5 px-2.5 py-1.5 text-[12px] text-red-700 transition hover:border-red-500 disabled:opacity-50"
              >
                {sweeping ? (
                  <Loader2 className="size-3.5 animate-spin" aria-hidden />
                ) : (
                  <Trash2 className="size-3.5" aria-hidden />
                )}
                Clear {count.archived} stored bod{count.archived === 1 ? "y" : "ies"}
              </button>
            </>
          ) : (
            <p data-retention-count-empty className="text-muted">
              Nothing is old enough. Every stored body in this workspace is younger than{" "}
              {days} days, so a sweep right now would change nothing.
            </p>
          )}
        </div>
      ) : null}

      {done ? (
        <p
          role="status"
          data-retention-done
          className="mt-2 rounded-lg border border-positive/40 bg-positive-soft px-3 py-2 text-[12px] text-positive"
        >
          {done.archived === 0
            ? "The sweep ran and found nothing to clear."
            : `${done.archived} stored bod${done.archived === 1 ? "y was" : "ies were"} cleared. The lead rows and their timelines are unchanged.`}
        </p>
      ) : null}
    </section>
  );
}

function SourceEditor({
  state,
  busy,
  onChange,
  onSave,
  onClose,
}: {
  state: EditorState;
  busy: boolean;
  onChange: (next: EditorState) => void;
  onSave: () => void;
  onClose: () => void;
}) {
  const [sample, setSample] = useState(SAMPLE_PAYLOAD);
  const [preview, setPreview] = useState<MappingPreview | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);

  /** The lines whose target is required but has no source and no fallback. */
  const unsatisfied = useMemo(
    () =>
      state.requiredTargets.filter(
        (target) =>
          !state.mapping.some(
            (line) => line.target === target && (line.source.trim() !== "" || (line.fallback ?? "") !== ""),
          ),
      ),
    [state.mapping, state.requiredTargets],
  );

  const setLine = (index: number, patch: Partial<MappingLine>) => {
    const mapping = state.mapping.map((line, at) => (at === index ? { ...line, ...patch } : line));
    onChange({ ...state, mapping });
  };

  const move = (index: number, by: number) => {
    const target = index + by;
    if (target < 0 || target >= state.mapping.length) return;
    const mapping = [...state.mapping];
    const [line] = mapping.splice(index, 1);
    mapping.splice(target, 0, line);
    onChange({ ...state, mapping });
  };

  const runPreview = async () => {
    setTesting(true);
    setPreviewError(null);
    setPreview(null);
    let payload: Record<string, unknown>;
    try {
      payload = JSON.parse(sample) as Record<string, unknown>;
    } catch {
      setPreviewError("The sample is not valid JSON. A test mapping runs over JSON, because that is what a real submission is.");
      setTesting(false);
      return;
    }
    try {
      // The server has the mapping that is *saved*, so a preview of unsaved lines would need a
      // second endpoint. The screen says so rather than showing a preview of something else.
      if (state.mapping !== undefined) {
        setPreview(await testIntakeMapping(state.id, payload));
      }
    } catch (caught) {
      setPreviewError(
        caught instanceof ApiError ? caught.message : "The preview could not be run.",
      );
    } finally {
      setTesting(false);
    }
  };

  return (
    <section
      data-source-editor={state.id}
      aria-label={`Edit ${state.name}`}
      className="flex flex-col gap-4 rounded-xl border border-accent/30 bg-surface p-4"
    >
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-[13.5px] font-semibold">Edit “{state.name}”</h3>
          <p className="mt-0.5 text-[12px] text-muted">
            {SOURCE_KIND_LABEL[state.kind] ?? state.kind} · {DEDUPE_POLICY_LABEL[state.dedupePolicy]}
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="rounded-md p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
          aria-label="Close the editor"
        >
          <X className="size-4" aria-hidden />
        </button>
      </header>

      {/* Surface */}
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Name</span>
          <input
            id="source-name"
            value={state.name}
            onChange={(event) => onChange({ ...state, name: event.target.value })}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
          />
        </label>
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Submissions per hour</span>
          <input
            id="source-rate-limit"
            type="number"
            min={1}
            max={10000}
            value={state.rateLimitPerHour}
            onChange={(event) => onChange({ ...state, rateLimitPerHour: Number(event.target.value) })}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
          />
        </label>
        <p className="text-[11.5px] text-muted sm:col-span-2">
          The hourly ceiling is what protects a public capture path: everything over it is
          refused with a `429`, whichever key it used.
        </p>
      </div>

      {/* Mapping */}
      <div>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h4 className="text-[12.5px] font-semibold">Mapping</h4>
          <button
            type="button"
            data-mapping-add
            onClick={() =>
              onChange({
                ...state,
                mapping: [
                  ...state.mapping,
                  { target: "message", source: "", transforms: ["trim"], required: false, fallback: null },
                ],
              })
            }
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:text-ink"
          >
            <Plus className="size-3" aria-hidden />
            Add a line
          </button>
        </div>

        {state.mapping.length === 0 ? (
          <p className="mt-2 rounded-lg border border-line bg-canvas px-3 py-3 text-[12.5px] text-muted">
            No lines yet, so every submission arrives with nothing mapped. Add at least an e-mail
            or a phone line, or nothing that arrives can be contacted.
          </p>
        ) : (
          <div className="mt-2 flex flex-col gap-2">
            {state.mapping.map((line, index) => (
              <div
                key={`${line.target}-${index}`}
                data-mapping-row={line.target}
                className="grid gap-2 rounded-lg border border-line bg-canvas p-2.5 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] sm:items-end"
              >
                <label className="flex flex-col gap-1 text-[11px] text-muted">
                  <span className="font-medium">CRM field</span>
                  <select
                    value={line.target}
                    data-mapping-target={line.target}
                    onChange={(event) => setLine(index, { target: event.target.value })}
                    className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] text-ink"
                  >
                    {MAPPING_TARGETS.map((target) => (
                      <option key={target} value={target}>
                        {MAPPING_TARGET_LABEL[target] ?? target}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="flex flex-col gap-1 text-[11px] text-muted">
                  <span className="font-medium">Submission key</span>
                  <input
                    value={line.source}
                    data-mapping-source={line.target}
                    placeholder="email"
                    onChange={(event) => setLine(index, { source: event.target.value })}
                    className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] text-ink"
                  />
                </label>
                <div className="flex flex-wrap items-center gap-1.5">
                  <label className="flex items-center gap-1 text-[11px] text-muted">
                    <input
                      type="checkbox"
                      data-mapping-required={line.target}
                      checked={line.required}
                      onChange={(event) => setLine(index, { required: event.target.checked })}
                      className="size-3.5"
                    />
                    Required
                  </label>
                  <select
                    aria-label={`Transforms for ${MAPPING_TARGET_LABEL[line.target] ?? line.target}`}
                    data-mapping-transform={line.target}
                    value=""
                    onChange={(event) => {
                      const value = event.target.value;
                      if (!value || line.transforms.includes(value)) return;
                      setLine(index, { transforms: [...line.transforms, value] });
                    }}
                    className="rounded-lg border border-line bg-surface px-2 py-1 text-[11.5px] text-muted"
                  >
                    <option value="">Add transform…</option>
                    {MAPPING_TRANSFORMS.filter((value) => !line.transforms.includes(value)).map((value) => (
                      <option key={value} value={value}>
                        {MAPPING_TRANSFORM_LABEL[value] ?? value}
                      </option>
                    ))}
                  </select>
                  <button
                    type="button"
                    aria-label={`Move ${line.target} up`}
                    data-mapping-up={line.target}
                    onClick={() => move(index, -1)}
                    disabled={index === 0}
                    className="rounded-md border border-line px-1.5 py-1 text-[11px] disabled:opacity-40"
                  >
                    ↑
                  </button>
                  <button
                    type="button"
                    aria-label={`Move ${line.target} down`}
                    data-mapping-down={line.target}
                    onClick={() => move(index, 1)}
                    disabled={index === state.mapping.length - 1}
                    className="rounded-md border border-line px-1.5 py-1 text-[11px] disabled:opacity-40"
                  >
                    ↓
                  </button>
                  <button
                    type="button"
                    aria-label={`Remove ${line.target}`}
                    data-mapping-remove={line.target}
                    onClick={() =>
                      onChange({ ...state, mapping: state.mapping.filter((_, at) => at !== index) })
                    }
                    className="rounded-md border border-line px-1.5 py-1 text-[11px] text-red-600"
                  >
                    <X className="size-3" aria-hidden />
                  </button>
                </div>
                {line.transforms.length > 0 ? (
                  <p className="flex flex-wrap items-center gap-1 sm:col-span-3">
                    {line.transforms.map((transform) => (
                      <span
                        key={transform}
                        className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                      >
                        {MAPPING_TRANSFORM_LABEL[transform] ?? transform}
                        <button
                          type="button"
                          aria-label={`Remove transform ${transform}`}
                          onClick={() =>
                            setLine(index, {
                              transforms: line.transforms.filter((value) => value !== transform),
                            })
                          }
                        >
                          <X className="size-2.5" aria-hidden />
                        </button>
                      </span>
                    ))}
                  </p>
                ) : null}
              </div>
            ))}
          </div>
        )}

        <fieldset className="mt-3 flex flex-wrap items-center gap-1.5" data-required-targets>
          <legend className="mb-1 text-[11.5px] font-medium text-muted">Refuse a submission without…</legend>
          {MAPPING_TARGETS.map((target) => {
            const active = state.requiredTargets.includes(target);
            return (
              <button
                key={target}
                type="button"
                aria-pressed={active}
                data-required-target={target}
                onClick={() =>
                  onChange({
                    ...state,
                    requiredTargets: active
                      ? state.requiredTargets.filter((value) => value !== target)
                      : [...state.requiredTargets, target],
                  })
                }
                className={`rounded-full border px-2 py-0.5 text-[11px] transition ${
                  active ? "border-accent bg-accent-soft text-accent-strong" : "border-line text-muted hover:text-ink"
                }`}
              >
                {MAPPING_TARGET_LABEL[target] ?? target}
              </button>
            );
          })}
        </fieldset>

        {unsatisfied.length > 0 ? (
          <p
            data-mapping-unsatisfied
            className="mt-2 flex items-start gap-1.5 rounded-lg border border-caution/40 bg-caution-soft px-2.5 py-2 text-[12px] text-caution"
          >
            <Ban className="mt-0.5 size-3.5 shrink-0" aria-hidden />
            The save will be refused: {unsatisfied.map((t) => MAPPING_TARGET_LABEL[t] ?? t).join(", ")}{" "}
            is required but has neither a submission key nor a fallback.
          </p>
        ) : null}
      </div>

      {/* Rules */}
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Dedupe policy</span>
          <select
            id="source-dedupe"
            data-source-dedupe
            value={state.dedupePolicy}
            onChange={(event) => onChange({ ...state, dedupePolicy: event.target.value })}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
          >
            {DEDUPE_POLICIES.map((policy) => (
              <option key={policy} value={policy}>
                {DEDUPE_POLICY_LABEL[policy]}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-2 self-end text-[12px] text-ink">
          <input
            id="source-consent-required"
            type="checkbox"
            data-source-consent
            checked={state.consentRequired}
            onChange={(event) => onChange({ ...state, consentRequired: event.target.checked })}
            className="size-3.5"
          />
          A submission must carry the consent
        </label>
        {state.consentRequired ? (
          <label className="flex flex-col gap-1 text-[11.5px] text-muted sm:col-span-2">
            <span className="font-medium">Consent wording</span>
            <textarea
              id="source-consent-text"
              rows={2}
              value={state.consentText}
              onChange={(event) => onChange({ ...state, consentText: event.target.value })}
              placeholder="I agree to be contacted about this request."
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
            />
            <span>
              Stored verbatim on every accepted lead, so what the visitor agreed to is readable
              later without asking them again.
            </span>
          </label>
        ) : null}
      </div>

      <AutoresponderSection
        sourceName={state.name}
        value={state.autoresponder}
        onChange={(autoresponder) => onChange({ ...state, autoresponder })}
      />

      {/* The preview. Writes nothing, and says so. */}
      <div className="rounded-lg border border-line bg-canvas p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h4 className="text-[12.5px] font-semibold">Test the mapping</h4>
          <button
            type="button"
            data-mapping-test
            disabled={testing}
            onClick={() => void runPreview()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:text-ink disabled:opacity-50"
          >
            {testing ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Wand2 className="size-3.5" aria-hidden />
            )}
            Run a sample through it
          </button>
        </div>
        <p className="mt-1 text-[11.5px] text-muted">
          Writes nothing: it runs the mapping, the transforms and the contactability check over a
          payload and answers the fields it would produce.
        </p>
        <textarea
          aria-label="Sample submission payload"
          data-mapping-sample
          rows={6}
          value={sample}
          onChange={(event) => setSample(event.target.value)}
          className="mt-2 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 font-mono text-[11.5px] text-ink"
        />
        {previewError ? (
          <p role="alert" data-mapping-preview-error className="mt-2 text-[12px] text-red-700">
            {previewError}
          </p>
        ) : null}
        {preview ? (
          <div data-mapping-preview className="mt-2 rounded-lg border border-line bg-surface p-3 text-[12px]">
            <p className="flex flex-wrap items-center gap-2">
              <span
                className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ${
                  preview.contactable ? "bg-positive-soft text-positive" : "bg-red-500/10 text-red-700"
                }`}
              >
                {preview.contactable ? (
                  <Check className="size-3" aria-hidden />
                ) : (
                  <TriangleAlert className="size-3" aria-hidden />
                )}
                {preview.contactable ? "Contactable" : "Not contactable — the submission would be rejected"}
              </span>
              <span
                className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ${
                  preview.consent_given ? "bg-positive-soft text-positive" : "bg-caution-soft text-caution"
                }`}
              >
                {preview.consent_given ? "Consent given" : "Consent not given"}
              </span>
              {preview.dedupe_key ? (
                <span className="text-muted">would match on {preview.dedupe_key}</span>
              ) : null}
            </p>
            {preview.missing_required.length > 0 ? (
              <p className="mt-1.5 text-caution">
                Missing required: {preview.missing_required.map((t) => MAPPING_TARGET_LABEL[t] ?? t).join(", ")}
              </p>
            ) : null}
            <dl className="mt-2 grid grid-cols-[10rem_minmax(0,1fr)] gap-x-3 gap-y-1">
              {Object.entries(preview.values).map(([target, value]) => (
                <div key={target} className="contents">
                  <dt className="text-muted">{MAPPING_TARGET_LABEL[target] ?? target}</dt>
                  <dd className="min-w-0 break-words">{value}</dd>
                </div>
              ))}
            </dl>
          </div>
        ) : null}
      </div>

      <footer className="flex items-center gap-2">
        <button
          type="button"
          data-source-save
          data-qa-guard="crm-intake-depth"
          disabled={busy || unsatisfied.length > 0}
          onClick={onSave}
          className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
        >
          {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Check className="size-3.5" aria-hidden />}
          Save the source
        </button>
        <button
          type="button"
          onClick={onClose}
          className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-muted hover:underline"
        >
          Close without saving
        </button>
        {unsatisfied.length > 0 ? (
          <span className="text-[11.5px] text-caution">Saving is refused until the mapping is fixed.</span>
        ) : null}
      </footer>
    </section>
  );
}

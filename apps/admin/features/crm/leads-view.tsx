"use client";

/**
 * The form → lead ingress inbox and its routing (REQ-051 slice 4 part seven, REQ-117).
 *
 * The screen answers one question — *did anything arrive, and what became of it* — and the
 * honest answer has to include the submissions that became **nothing**, which is why the
 * outcome column is a filter chip with a count rather than a footnote. A form builder that
 * silently drops a submission is the single most expensive failure this feature has, and the
 * inbox is where somebody finds out.
 *
 * Three things are deliberate here:
 *
 * * **The list is the log, not a second pipeline.** The inbox is not narrowed by the caller's
 *   visibility level, because a manager with the `own` level still has to see that a
 *   submission was filed under a colleague. The *records* keep their own visibility, and each
 *   row links to them rather than embedding them.
 * * **A rejected submission shows its reason.** "Nothing usable" without the sentence is a
 *   shrug; the module writes the reason and this screen shows it.
 * * **The routing is a screen, not a constant.** Turning leads off, choosing where a repeat
 *   goes and naming the source are all decisions an operator has to make on purpose, so the
 *   settings live here, next to the list they change.
 *
 * The filter state is in the URL for the same reason the contact list's is: a filtered inbox is
 * a link, so the back button, a bookmark and a shared URL all land on the same rows.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";

import {
  ArrowRight,
  CircleSlash,
  FileWarning,
  Inbox,
  Loader2,
  RefreshCw,
  Settings2,
  TriangleAlert,
  UserPlus,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ApiError } from "@/lib/api";
import {
  CRM_LEAD_OUTCOME_HINT,
  CRM_LEAD_OUTCOME_LABEL,
  CRM_LEAD_OUTCOMES,
  drainCrmLeads,
  fetchCrmLeadSettings,
  fetchCrmLeads,
  relativeTime,
  saveCrmLeadSettings,
  type CrmLead,
  type CrmLeadInbox,
  type CrmLeadOutcome,
  type CrmLeadSettingsView,
} from "@/lib/crm";

/** The icon each outcome is drawn with, so a kind is never a bare string in a list. */
const OUTCOME_ICON: Record<CrmLeadOutcome, typeof Inbox> = {
  created: UserPlus,
  merged: ArrowRight,
  rejected: CircleSlash,
  orphaned: FileWarning,
  disabled: TriangleAlert,
};

/** The tone each outcome is painted in — the two that need attention are not the same colour. */
const OUTCOME_TONE: Record<CrmLeadOutcome, string> = {
  created: "bg-success-soft text-success",
  merged: "bg-canvas text-muted",
  rejected: "bg-warn-soft text-warn",
  orphaned: "bg-warn-soft text-warn",
  disabled: "bg-canvas text-muted",
};

/** The settings form's state. One screen, so it is local rather than a context. */
type SettingsDraft = {
  create_contact: boolean;
  create_deal: boolean;
  stage_id: string;
  repeat_stage_id: string;
  source_label: string;
};

const EMPTY_DRAFT: SettingsDraft = {
  create_contact: true,
  create_deal: true,
  stage_id: "",
  repeat_stage_id: "",
  source_label: "form",
};

/** `/crm/leads`: the inbox, its filters, and the button that runs a drain. */
export function LeadsView() {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();

  const [inbox, setInbox] = useState<CrmLeadInbox | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [draining, setDraining] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [search, setSearch] = useState(params.get("search") ?? "");
  const searchRef = useRef<HTMLInputElement | null>(null);

  // The outcome filter is a URL parameter, not local state: it is the one a person shares.
  const outcome = (params.get("outcome") ?? "") as CrmLeadOutcome | "";
  const filtered = outcome !== "" || (params.get("search") ?? "") !== "";

  useEffect(() => {
    fetchCrmLeads({
      outcome: outcome === "" ? undefined : outcome,
      search: params.get("search") ?? undefined,
      limit: 100,
    })
      .then(setInbox)
      .catch((problem) =>
        setError(problem instanceof ApiError ? problem.message : "The lead inbox could not be loaded."),
      );
  }, [outcome, params.get("search"), reloadToken]);

  // `/` focuses the search, and the typing is debounced into the URL so a person does not
  // produce a history entry per keystroke.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (event.key === "Escape" && document.activeElement === searchRef.current) {
        searchRef.current?.blur();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const applyFilter = useCallback(
    (next: { outcome?: string; search?: string }) => {
      const merged = new URLSearchParams(params.toString());
      for (const [key, value] of Object.entries(next)) {
        if (value === undefined || value === null || value === "") merged.delete(key);
        else merged.set(key, value);
      }
      const suffix = merged.toString();
      router.push(`${pathname}${suffix ? `?${suffix}` : ""}`);
    },
    [params, pathname, router],
  );

  // Debounced: the inbox re-reads on every URL change, and a request per keystroke is a
  // request per keystroke against a log a person is only skimming.
  useEffect(() => {
    const current = params.get("search") ?? "";
    if (search === current) return;
    const timer = setTimeout(() => applyFilter({ search }), 250);
    return () => clearTimeout(timer);
  }, [search, params, applyFilter]);

  const runDrain = useCallback(async () => {
    setDraining(true);
    setError(null);
    setNotice(null);
    try {
      const report = await drainCrmLeads();
      setNotice(
        report.idle
          ? "Nothing new on the bus — the inbox is already up to date."
          : `Filed ${report.created} new, ${report.merged} repeat, ${report.rejected} without a name or an address.`,
      );
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setError(problem instanceof ApiError ? problem.message : "The drain could not be run.");
    } finally {
      setDraining(false);
    }
  }, []);

  const counts = useMemo(() => {
    const map = new Map<string, number>();
    for (const counter of inbox?.counts ?? []) map.set(counter.outcome, counter.count);
    return map;
  }, [inbox]);

  const rows = inbox?.items ?? [];

  return (
    <div className="flex h-full flex-col">
      <header className="flex flex-wrap items-end justify-between gap-3 border-b border-line px-4 py-3">
        <div>
          <h1 className="text-[15px] font-semibold">Lead inbox</h1>
          <p className="mt-0.5 text-[12.5px] text-muted">
            Every form submission the platform has read, and what became of it. A submission is
            filed at most once, whatever reads the bus.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <input
            id="crm-leads-search"
            ref={searchRef}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search a name, an address or a form"
            aria-label="Search the lead inbox"
            className="w-56 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
          />
          <button
            type="button"
            id="crm-leads-drain"
            onClick={() => void runDrain()}
            disabled={draining}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-canvas disabled:opacity-50"
          >
            {draining ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <RefreshCw className="size-3.5" aria-hidden />
            )}
            {draining ? "Reading the bus…" : "Run the drain now"}
          </button>
          <button
            type="button"
            id="crm-leads-settings-toggle"
            onClick={() => setSettingsOpen((open) => !open)}
            aria-expanded={settingsOpen}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-canvas"
          >
            <Settings2 className="size-3.5" aria-hidden />
            Routing
          </button>
        </div>
      </header>

      {error ? (
        <p
          role="alert"
          className="flex items-center gap-2 border-b border-line bg-danger-soft px-4 py-2.5 text-[12px] text-danger"
        >
          <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="border-b border-line bg-canvas px-4 py-2.5 text-[12px] text-muted">
          {notice}
        </p>
      ) : null}

      {settingsOpen ? (
        <LeadRouting onClose={() => setSettingsOpen(false)} onSaved={setNotice} />
      ) : null}

      <nav aria-label="Outcomes" className="flex flex-wrap items-center gap-1.5 border-b border-line px-4 py-2.5">
        <Chip
          active={outcome === ""}
          label="All"
          count={rows.length}
          onClick={() => applyFilter({ outcome: undefined })}
        />
        {CRM_LEAD_OUTCOMES.map((key) => (
          <Chip
            key={key}
            active={outcome === key}
            label={CRM_LEAD_OUTCOME_LABEL[key]}
            hint={CRM_LEAD_OUTCOME_HINT[key]}
            count={counts.get(key) ?? 0}
            onClick={() => applyFilter({ outcome: outcome === key ? undefined : key })}
          />
        ))}
        {filtered ? (
          <button
            type="button"
            onClick={() => {
              setSearch("");
              router.push(pathname);
            }}
            className="ml-1 text-[12px] text-muted underline underline-offset-2 hover:text-ink"
          >
            Clear
          </button>
        ) : null}
      </nav>

      {inbox === null ? (
        <div className="flex flex-1 items-center justify-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-4 animate-spin" aria-hidden /> Loading the inbox…
        </div>
      ) : rows.length === 0 ? (
        <EmptyState
          title={
            filtered
              ? "No submission matches this filter"
              : "No form submission has arrived yet"
          }
          hint={
            filtered
              ? "Clear the filter to see every outcome, including the ones that became nothing."
              : "When a published form is submitted, the platform files it here as a contact and a deal. The drain runs by itself every few seconds; the button above asks it to run now."
          }
        />
      ) : (
        <ul className="divide-y divide-line">
          {rows.map((lead) => (
            <LeadRow key={lead.event_id} lead={lead} />
          ))}
        </ul>
      )}

      {inbox ? (
        <footer className="border-t border-line px-4 py-2 text-[11.5px] text-muted">
          The drain has read the bus up to event {inbox.cursor}. A submission is filed once: the
          event id is the inbox's own key, so a retry or a second process cannot double it.
        </footer>
      ) : null}
    </div>
  );
}

/** One outcome filter chip, with the count the API gave rather than the rows on screen. */
function Chip({
  active,
  label,
  hint,
  count,
  onClick,
}: {
  active: boolean;
  label: string;
  hint?: string;
  count: number;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      title={hint}
      aria-pressed={active}
      onClick={onClick}
      className={`rounded-lg px-2.5 py-1 text-[12px] font-medium transition ${
        active ? "bg-accent text-white" : "bg-canvas text-muted hover:text-ink"
      }`}
    >
      {label}
      <span className={active ? "ml-1.5 text-white/80" : "ml-1.5 text-muted"}>{count}</span>
    </button>
  );
}

/** One submission: who, what, what became of it, and where the records went. */
function LeadRow({ lead }: { lead: CrmLead }) {
  const Icon = OUTCOME_ICON[lead.outcome];
  return (
    <li
      data-qa-lead-row={lead.event_id}
      className="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-start sm:justify-between"
    >
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="truncate text-[13px] font-medium">
            {lead.name || "Unnamed submission"}
          </span>
          <span
            className={`inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] font-medium ${OUTCOME_TONE[lead.outcome]}`}
          >
            <Icon className="size-3" aria-hidden />
            {CRM_LEAD_OUTCOME_LABEL[lead.outcome]}
          </span>
          {lead.form_key ? (
            <span className="text-[11.5px] text-muted">from {lead.form_key}</span>
          ) : null}
        </div>

        <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[12px] text-muted">
          {lead.email ? <span>{lead.email}</span> : null}
          {lead.company_name ? <span>{lead.company_name}</span> : null}
          <span>submitted {relativeTime(lead.occurred_at)}</span>
        </p>

        {lead.detail ? (
          <p className="mt-1 text-[12px] text-warn">{lead.detail}</p>
        ) : null}
      </div>

      <div className="flex shrink-0 items-center gap-2 text-[12px]">
        {lead.contact_id ? (
          <a
            href={`/crm/contacts?q=${encodeURIComponent(lead.email ?? lead.name)}`}
            className="rounded-lg border border-line px-2 py-1 text-muted transition hover:text-ink"
          >
            Contact
          </a>
        ) : null}
        {lead.deal_id ? (
          <a
            href="/crm/deals"
            className="rounded-lg border border-line px-2 py-1 text-muted transition hover:text-ink"
          >
            Deal
          </a>
        ) : null}
      </div>
    </li>
  );
}

// ---------------------------------------------------------------------------------------------
// The routing settings
// ---------------------------------------------------------------------------------------------

/** What a submission becomes, and where a repeat goes. */
function LeadRouting({
  onClose,
  onSaved,
}: {
  onClose: () => void;
  onSaved: (message: string) => void;
}) {
  const [view, setView] = useState<CrmLeadSettingsView | null>(null);
  const [draft, setDraft] = useState<SettingsDraft>(EMPTY_DRAFT);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    fetchCrmLeadSettings()
      .then((loaded) => {
        setView(loaded);
        setDraft({
          create_contact: loaded.settings.create_contact,
          create_deal: loaded.settings.create_deal,
          stage_id: loaded.settings.stage_id ?? "",
          repeat_stage_id: loaded.settings.repeat_stage_id ?? "",
          source_label: loaded.settings.source_label,
        });
      })
      .catch((problem) =>
        setError(
          problem instanceof ApiError ? problem.message : "The routing could not be loaded.",
        ),
      );
  }, [reloadToken]);

  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    setFieldError(null);
    try {
      const saved = await saveCrmLeadSettings({
        create_contact: draft.create_contact,
        create_deal: draft.create_deal,
        // An empty picker is `null`, not an absent key: "the first open stage" is a decision the
        // operator is making here, and it is different from "keep whatever is there".
        stage_id: draft.stage_id === "" ? null : draft.stage_id,
        repeat_stage_id: draft.repeat_stage_id === "" ? null : draft.repeat_stage_id,
        source_label: draft.source_label,
      });
      setView(saved);
      onSaved("The routing was saved. Submissions from now on are filed this way.");
    } catch (problem) {
      // The API nests the refused field in `details.field` (see the activity form, which reads
      // the same place) — a refusal that names a field is a message under that field, not a
      // banner above the whole panel.
      const refused =
        problem instanceof ApiError && typeof problem.details?.field === "string"
          ? (problem as ApiError)
          : null;
      if (refused) {
        setFieldError({
          field: String(refused.details?.field),
          message: refused.message,
        });
      } else {
        setError(
          problem instanceof ApiError ? problem.message : "The routing could not be saved.",
        );
      }
    } finally {
      setSaving(false);
    }
  }, [draft, onSaved]);

  return (
    <section
      aria-label="Form to lead routing"
      className="border-b border-line bg-canvas px-4 py-3"
    >
      {error ? (
        <p role="alert" className="mb-2 text-[12px] text-danger">
          {error}
        </p>
      ) : null}

      {view === null ? (
        <p className="flex items-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden /> Loading the routing…
        </p>
      ) : (
        <>
          {!view.configured ? (
            <p className="mb-2 text-[12px] text-muted">
              Not configured yet. A fresh tenant files every submission as a contact and a deal in
              the first open stage; saving here records that as a decision.
            </p>
          ) : null}

          <div className="flex flex-wrap items-end gap-3">
            <Toggle
              id="crm-leads-create-contact"
              label="Create a contact"
              hint="A person with a name or an address becomes a contact."
              checked={draft.create_contact}
              onChange={(checked) => setDraft((d) => ({ ...d, create_contact: checked }))}
            />
            <Toggle
              id="crm-leads-create-deal"
              label="Create a deal"
              hint="A submission opens a deal on the pipeline so it can be worked."
              checked={draft.create_deal}
              onChange={(checked) => setDraft((d) => ({ ...d, create_deal: checked }))}
            />

            <label className="grid gap-1 text-[12px]">
              <span className="text-[11px] text-muted">New leads land in</span>
              <input
                id="crm-leads-stage"
                value={draft.stage_id}
                onChange={(event) => setDraft((d) => ({ ...d, stage_id: event.target.value }))}
                placeholder="First open stage"
                className="w-56 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
              />
            </label>

            <label className="grid gap-1 text-[12px]">
              <span className="text-[11px] text-muted">Repeats land in</span>
              <input
                id="crm-leads-repeat-stage"
                value={draft.repeat_stage_id}
                onChange={(event) =>
                  setDraft((d) => ({ ...d, repeat_stage_id: event.target.value }))
                }
                placeholder="Same as a new lead"
                className="w-56 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
              />
            </label>

            <label className="grid gap-1 text-[12px]">
              <span className="text-[11px] text-muted">Labelled as</span>
              <input
                id="crm-leads-source"
                value={draft.source_label}
                onChange={(event) =>
                  setDraft((d) => ({ ...d, source_label: event.target.value }))
                }
                placeholder="form"
                className="w-40 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
              />
            </label>

            <div className="ml-auto flex items-center gap-2">
              <button
                type="button"
                onClick={onClose}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-muted transition hover:text-ink"
              >
                Close
              </button>
              <button
                type="button"
                id="crm-leads-save"
                onClick={() => void save()}
                disabled={saving}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {saving ? "Saving…" : "Save routing"}
              </button>
            </div>
          </div>

          {fieldError ? (
            <p role="alert" className="mt-2 text-[12px] text-danger">
              {fieldError.message}
            </p>
          ) : null}
        </>
      )}
    </section>
  );
}

/** One switch, with the sentence that says what turning it off means. */
function Toggle({
  id,
  label,
  hint,
  checked,
  onChange,
}: {
  id: string;
  label: string;
  hint: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <label htmlFor={id} className="grid gap-1 text-[12px]">
      <span className="text-[11px] text-muted">{label}</span>
      <span className="flex items-center gap-2">
        <input
          id={id}
          type="checkbox"
          checked={checked}
          onChange={(event) => onChange(event.target.checked)}
          className="size-3.5 accent-[var(--accent)]"
        />
        <span className="text-[11.5px] text-muted">{hint}</span>
      </span>
    </label>
  );
}

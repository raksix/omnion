"use client";

/**
 * The contacts list and its form (docs/requests/REQ-051, slice 2).
 *
 * What this screen has to do, and why each piece exists:
 *
 * * **The list is a URL.** Search, owner, status, tag, the archived switch, the sort and the
 *   column set all travel in the query, so a filtered list is a link and a saved view is the same
 *   fields. The list pages with the API's cursor rather than an offset, because archiving a row
 *   shifts an offset and the second page would repeat or skip records.
 * * **Inline edit is optimistic with a rollback that is visible.** Changing the owner, the status
 *   or the tags in the row paints the new value immediately, sends the patch, and puts the old
 *   value back with a message when the API refuses. A field that silently snapped back would look
 *   like the panel losing the edit.
 * * **A refusal is rendered under the field that caused it.** The API answers a validation failure
 *   with `error.details.field`; the form attaches that sentence to its input, focuses the first
 *   invalid one and does not submit.
 * * **The import previews before it writes.** A dry run shows the mapping, the row count and every
 *   refused line; the commit re-sends the same file and writes only the rows the preview accepted.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Check, Download, Pencil, Plus, Upload, X } from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  archiveCrmContact,
  commitCrmImport,
  createCrmContact,
  createCrmView,
  deleteCrmView,
  downloadCrmExport,
  dryRunCrmImport,
  fetchCrmColumnCatalogue,
  fetchCrmCompanies,
  fetchCrmContacts,
  fetchCrmViews,
  mergeCrmContacts,
  saveDownload,
  updateCrmContact,
  type CrmCompany,
  type CrmContact,
  type CrmImportPreview,
  type CrmView,
} from "@/lib/crm";
import { formatTimestamp } from "@/lib/format";

import {
  CrmAvatar,
  CrmShell,
  CrmSortHeader,
  CrmStatusBadge,
  CrmTag,
} from "./crm-parts";
import { useCrmTenant } from "./crm-tenant";

/** What the contact form holds while it is open. */
type ContactForm = {
  id: string | null;
  first_name: string;
  last_name: string;
  email: string;
  phone: string;
  job_title: string;
  company_id: string;
  status: string;
  tags: string;
  notes: string;
};

/** An empty form, or one loaded from a row. */
function formOf(contact: CrmContact | null): ContactForm {
  return {
    id: contact?.id ?? null,
    first_name: contact?.first_name ?? "",
    last_name: contact?.last_name ?? "",
    email: contact?.email ?? "",
    phone: contact?.phone ?? "",
    job_title: contact?.job_title ?? "",
    company_id: contact?.company_id ?? "",
    status: contact?.status ?? "lead",
    tags: contact?.tags.join(", ") ?? "",
    notes: contact?.notes ?? "",
  };
}

/** The lifecycle statuses the filter offers, until the API answers with its own list. */
const FALLBACK_STATUSES = ["lead", "customer", "partner", "churned"];

/** The contacts list, its inline edit, its form and its CSV round trip. */
export function ContactsView() {
  const router = useRouter();
  const searchParams = useSearchParams();

  const [rows, setRows] = useState<CrmContact[] | null>(null);
  const [total, setTotal] = useState(0);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const [catalogue, setCatalogue] = useState<{ columns: string[]; statuses: string[] } | null>(
    null,
  );
  const [views, setViews] = useState<CrmView[]>([]);
  const [companies, setCompanies] = useState<CrmCompany[]>([]);

  const [form, setForm] = useState<ContactForm | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const [viewName, setViewName] = useState("");
  const [savingView, setSavingView] = useState(false);
  const [mergeTarget, setMergeTarget] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const firstFieldRef = useRef<HTMLInputElement | null>(null);

  const [importOpen, setImportOpen] = useState(false);
  const [importText, setImportText] = useState("");
  const [importPreview, setImportPreview] = useState<CrmImportPreview | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  const fileRef = useRef<HTMLInputElement | null>(null);

  // The organization the panel is reading (REQ-051): a platform account has no primary one.
  const { organizationId } = useCrmTenant();

  // The filters the shell writes into the URL are read here as the list's query.
  const query = useMemo(
    () => ({
      search: searchParams.get("search") || undefined,
      owner: searchParams.get("owner") || undefined,
      status: searchParams.get("status") || undefined,
      tag: searchParams.get("tag") || undefined,
      include_archived: searchParams.get("include_archived") === "true" || undefined,
      sort: searchParams.get("sort") || undefined,
      direction: (searchParams.get("direction") as "asc" | "desc") ?? undefined,
      // The tenant is list state like the rest of this query: a shared URL says whose contacts
      // it is, and the API stops refusing a platform account that named no organization.
      organization_id: organizationId ?? undefined,
    }),
    [searchParams, organizationId],
  );

  const columns = useMemo(() => {
    const fromUrl = (searchParams.get("columns") ?? "")
      .split(",")
      .map((value) => value.trim())
      .filter(Boolean);
    const available = catalogue?.columns ?? [
      "name",
      "company",
      "email",
      "phone",
      "job_title",
      "owner",
      "status",
      "tags",
      "last_activity_at",
      "updated_at",
    ];
    return fromUrl.length > 0 ? fromUrl.filter((value) => available.includes(value)) : available;
  }, [searchParams, catalogue]);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  // The column catalogue and the statuses: the filter offers what the API says exists, not a
  // list the screen invented.
  useEffect(() => {
    let cancelled = false;
    fetchCrmColumnCatalogue("contacts", organizationId ?? undefined)
      .then((answer) => {
        if (!cancelled) {
          setCatalogue({ columns: answer.columns, statuses: answer.statuses });
        }
      })
      .catch(() => {
        if (!cancelled) {
          setCatalogue({ columns: [], statuses: FALLBACK_STATUSES });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    fetchCrmViews("contacts", organizationId ?? undefined)
      .then((answer) => {
        if (!cancelled) {
          setViews(answer);
        }
      })
      .catch(() => {
        // A list that cannot read its views is still a working list; the view row stays empty and
        // says so when the person opens it.
        if (!cancelled) {
          setViews([]);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [reloadToken]);

  // The companies the form's picker offers. Capped: a person picks from the ones they can see.
  useEffect(() => {
    let cancelled = false;
    fetchCrmCompanies({ limit: 200, sort: "name", direction: "asc", organization_id: organizationId ?? undefined })
      .then((page) => {
        if (!cancelled) {
          setCompanies(page.items);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setCompanies([]);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [reloadToken]);

  useEffect(() => {
    let cancelled = false;
    setRows(null);
    setError(null);
    fetchCrmContacts({ ...query, limit: 50 })
      .then((page) => {
        if (cancelled) {
          return;
        }
        setRows(page.items);
        setTotal(page.total_estimate);
        setNextCursor(page.next_cursor);
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(cause instanceof ApiError ? cause.message : "The contacts could not be loaded.");
      });
    return () => {
      cancelled = true;
    };
  }, [query, reloadToken]);

  const loadMore = useCallback(() => {
    if (!nextCursor || loadingMore) {
      return;
    }
    setLoadingMore(true);
    fetchCrmContacts({ ...query, limit: 50, cursor: nextCursor })
      .then((page) => {
        // Keyset paging can repeat a row if something changed between the pages; the id check is
        // cheap and keeps the table honest about how many rows it is showing.
        setRows((current) => {
          const seen = new Set((current ?? []).map((row) => row.id));
          return [...(current ?? []), ...page.items.filter((row) => !seen.has(row.id))];
        });
        setNextCursor(page.next_cursor);
      })
      .catch((cause: unknown) => {
        setActionError(
          cause instanceof ApiError ? cause.message : "The next page could not be loaded.",
        );
      })
      .finally(() => setLoadingMore(false));
  }, [nextCursor, loadingMore, query]);

  // ---- inline edit ----------------------------------------------------------------------------

  /**
   * Change one cell of a row, optimistically.
   *
   * The row is painted with the new value before the request leaves, and the old value is put
   * back — with a message — when the API refuses. A 422 or a 403 is therefore *visible* rather
   * than a value that quietly did not change.
   */
  const patchCell = useCallback(
    async (contact: CrmContact, changes: Record<string, unknown>, label: string) => {
      const before = rows;
      setRows((current) =>
        (current ?? []).map((row) => (row.id === contact.id ? { ...row, ...changes } : row)),
      );
      setActionError(null);
      setNotice(null);
      try {
        const fresh = await updateCrmContact(contact.id, changes);
        setRows((current) =>
          (current ?? []).map((row) => (row.id === fresh.id ? fresh : row)),
        );
        setNotice(`${label} updated.`);
      } catch (cause) {
        setRows(before);
        setActionError(
          cause instanceof ApiError
            ? `${label} was refused: ${cause.message}`
            : `${label} could not be saved.`,
        );
      }
    },
    [rows],
  );

  const toggleTag = useCallback(
    (contact: CrmContact, tag: string) => {
      const next = contact.tags.includes(tag)
        ? contact.tags.filter((value) => value !== tag)
        : [...contact.tags, tag];
      return patchCell(contact, { tags: next }, "Tags");
    },
    [patchCell],
  );

  // ---- the form -------------------------------------------------------------------------------

  const openCreate = useCallback(() => {
    setFieldError(null);
    setActionError(null);
    setForm(formOf(null));
  }, []);

  const openEdit = useCallback(
    (id: string) => {
      const contact = rows?.find((row) => row.id === id) ?? null;
      if (!contact) {
        setActionError("That contact is no longer in the list — reload and try again.");
        return;
      }
      setFieldError(null);
      setActionError(null);
      setForm(formOf(contact));
    },
    [rows],
  );

  // `/crm/contacts?focus=<id>` — a search hit (or a shared link) opens that contact's editor, the
  // same deep-link contract the pages screen uses. The applied id is remembered, so dismissing
  // the form does not bring it back on the next render, and an id that is not on the loaded page
  // is left alone rather than erroring: the search index is a reindex behind at worst.
  const focusParam = searchParams.get("focus");
  const appliedFocus = useRef<string | null>(null);
  useEffect(() => {
    if (!focusParam || !rows || appliedFocus.current === focusParam) {
      return;
    }
    if (!rows.some((row) => row.id === focusParam)) {
      return;
    }
    appliedFocus.current = focusParam;
    openEdit(focusParam);
  }, [focusParam, rows, openEdit]);

  const saveForm = useCallback(async () => {
    if (!form) {
      return;
    }
    // A first name is the one thing the form can prove is missing before the request leaves; the
    // rest is the API's call, and its refusal names the field.
    if (!form.first_name.trim()) {
      setFieldError({ field: "first_name", message: "a contact needs a first name" });
      firstFieldRef.current?.focus();
      return;
    }

    setSaving(true);
    setFieldError(null);
    setActionError(null);
    setNotice(null);

    const payload = {
      first_name: form.first_name.trim(),
      last_name: form.last_name.trim(),
      email: form.email.trim() || null,
      phone: form.phone.trim() || null,
      job_title: form.job_title.trim() || null,
      company_id: form.company_id || null,
      status: form.status,
      tags: form.tags
        .split(/[,;|]/)
        .map((value) => value.trim())
        .filter(Boolean),
      notes: form.notes,
    };

    try {
      if (form.id) {
        await updateCrmContact(form.id, payload);
        setNotice(`${payload.first_name} updated.`);
      } else {
        const created = await createCrmContact(payload);
        setNotice(`${created.display_name} created.`);
      }
      setForm(null);
      reload();
    } catch (cause) {
      if (cause instanceof ApiError && cause.details) {
        const field = cause.details.field;
        if (typeof field === "string") {
          setFieldError({ field, message: cause.message });
          return;
        }
      }
      setActionError(cause instanceof ApiError ? cause.message : "The contact was not saved.");
    } finally {
      setSaving(false);
    }
  }, [form, reload]);

  const archive = useCallback(
    async (contact: CrmContact) => {
      if (!window.confirm(`Archive ${contact.display_name}? It stops appearing in the list, and nothing is deleted.`)) {
        return;
      }
      setActionError(null);
      setNotice(null);
      try {
        const archived = await archiveCrmContact(contact.id);
        setRows((current) =>
          (current ?? []).map((row) => (row.id === archived.id ? archived : row)),
        );
        setNotice(`${contact.display_name} archived.`);
      } catch (cause) {
        setActionError(
          cause instanceof ApiError ? cause.message : "The contact could not be archived.",
        );
      }
    },
    [],
  );

  const runMerge = useCallback(async () => {
    if (!mergeTarget || !selected.has(mergeTarget) || selected.size !== 2) {
      setActionError("Pick exactly two contacts to merge.");
      return;
    }
    const other = [...selected].find((id) => id !== mergeTarget);
    const survivor = rows?.find((row) => row.id === mergeTarget);
    const loser = rows?.find((row) => row.id === other);
    if (!survivor || !loser) {
      setActionError("One of the selected contacts is no longer in the list.");
      return;
    }
    if (!window.confirm(`Merge ${loser.display_name} into ${survivor.display_name}? ${loser.display_name} is archived, not deleted.`)) {
      return;
    }
    setActionError(null);
    setNotice(null);
    try {
      const merged = await mergeCrmContacts(survivor.id, loser.id);
      setNotice(`${loser.display_name} merged into ${merged.display_name}.`);
      setSelected(new Set());
      setMergeTarget(null);
      reload();
    } catch (cause) {
      setActionError(cause instanceof ApiError ? cause.message : "The merge did not run.");
    }
  }, [mergeTarget, selected, rows, reload]);

  // ---- saved views ----------------------------------------------------------------------------

  const saveView = useCallback(async () => {
    const name = viewName.trim();
    if (!name) {
      setActionError("A saved view needs a name.");
      return;
    }
    setSavingView(true);
    setActionError(null);
    try {
      const saved = await createCrmView({
        entity: "contacts",
        name,
        filters: {
          search: query.search ?? null,
          owner: query.owner ?? null,
          status: query.status ?? null,
          tag: query.tag ?? null,
          include_archived: query.include_archived ?? null,
        },
        columns,
        sort: query.sort ? { key: query.sort, direction: query.direction ?? "desc" } : undefined,
        is_shared: false,
      });
      setViews((current) => [...current, saved]);
      setViewName("");
      setNotice(`View “${saved.name}” saved.`);
    } catch (cause) {
      setActionError(cause instanceof ApiError ? cause.message : "The view was not saved.");
    } finally {
      setSavingView(false);
    }
  }, [viewName, query, columns]);

  const applyView = useCallback(
    (view: CrmView) => {
      const next = new URLSearchParams();
      const filters = view.filters ?? {};
      for (const key of ["search", "owner", "status", "tag"] as const) {
        const value = filters[key];
        if (typeof value === "string" && value) {
          next.set(key, value);
        }
      }
      if (filters.include_archived === true) {
        next.set("include_archived", "true");
      }
      if (view.columns.length > 0) {
        next.set("columns", view.columns.join(","));
      }
      if (view.sort?.key) {
        next.set("sort", view.sort.key);
        next.set("direction", view.sort.direction ?? "desc");
      }
      const search = next.toString();
      router.replace(`/crm/contacts${search ? `?${search}` : ""}`);
    },
    [router],
  );

  const removeView = useCallback(
    async (view: CrmView) => {
      setActionError(null);
      try {
        await deleteCrmView(view.id);
        setViews((current) => current.filter((entry) => entry.id !== view.id));
        setNotice(`View “${view.name}” removed.`);
      } catch (cause) {
        setActionError(cause instanceof ApiError ? cause.message : "The view was not removed.");
      }
    },
    [],
  );

  // ---- import and export ----------------------------------------------------------------------

  const runDryRun = useCallback(async () => {
    if (!importText.trim()) {
      setImportError("Choose a CSV file first.");
      return;
    }
    setImporting(true);
    setImportError(null);
    setImportPreview(null);
    try {
      const preview = await dryRunCrmImport(importText);
      setImportPreview(preview);
    } catch (cause) {
      setImportError(cause instanceof ApiError ? cause.message : "The file could not be read.");
    } finally {
      setImporting(false);
    }
  }, [importText]);

  const runCommit = useCallback(async () => {
    setImporting(true);
    setImportError(null);
    try {
      // The same text the dry run read: the API re-parses it and writes only the rows the
      // preview accepted, so what lands is what the person agreed to.
      const answer = await commitCrmImport(importText);
      setImportPreview(null);
      setImportText("");
      setImportOpen(false);
      setNotice(
        answer.created === 0
          ? "No rows were written — every row was refused."
          : `${answer.created} contact${answer.created === 1 ? "" : "s"} imported${
              answer.refused > 0 ? `, ${answer.refused} refused` : ""
            }.`,
      );
      reload();
    } catch (cause) {
      setImportError(cause instanceof ApiError ? cause.message : "The import did not run.");
    } finally {
      setImporting(false);
    }
  }, [importText, reload]);

  const runExport = useCallback(async () => {
    setActionError(null);
    setNotice(null);
    try {
      const file = await downloadCrmExport("contacts", query);
      saveDownload(file.blob, file.filename);
      setNotice(
        `${file.rows} contact${file.rows === 1 ? "" : "s"} exported${
          file.truncated ? " (truncated at the export cap)" : ""
        }.`,
      );
    } catch (cause) {
      setActionError(cause instanceof ApiError ? cause.message : "The export failed.");
    }
  }, [query]);

  const rowIds = useMemo(() => (rows ?? []).map((row) => row.id), [rows]);
  const statuses = catalogue?.statuses ?? FALLBACK_STATUSES;

  return (
    <CrmShell
      title="Contacts"
      description="The people of the organization, with their company, owner, status and tags"
      entity="contacts"
      availableColumns={catalogue?.columns ?? []}
      statuses={statuses}
      total={total}
      nextCursor={nextCursor}
      loadingMore={loadingMore}
      loadMore={loadMore}
      onCreate={openCreate}
      rowIds={rowIds}
      keyboard={{ onEdit: openEdit, onOpen: openEdit }}
      toolbarExtra={
        <>
          <button
            type="button"
            id="crm-export"
            onClick={() => void runExport()}
            className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            <Download className="size-3.5" aria-hidden />
            Export
          </button>
          <button
            type="button"
            id="crm-import"
            onClick={() => setImportOpen(true)}
            className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            <Upload className="size-3.5" aria-hidden />
            Import CSV
          </button>
        </>
      }
    >
      {/* The saved views: a row of chips, each one the URL it stands for. */}
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-2.5">
        <span className="text-[11.5px] font-medium text-muted">Saved views</span>
        {views.length === 0 ? (
          <span className="text-[11.5px] text-muted">
            none yet — set a filter below and save it
          </span>
        ) : (
          views.map((view) => (
            <span key={view.id} className="inline-flex items-center gap-1">
              <button
                type="button"
                onClick={() => applyView(view)}
                className="rounded-full bg-quiet-soft px-2.5 py-1 text-[11.5px] text-muted transition hover:text-ink"
              >
                {view.name}
                {view.is_shared ? " · shared" : ""}
              </button>
              <button
                type="button"
                aria-label={`Remove the view ${view.name}`}
                onClick={() => void removeView(view)}
                className="text-muted transition hover:text-ink"
              >
                <X className="size-3" aria-hidden />
              </button>
            </span>
          ))
        )}
        <span className="ml-auto flex items-center gap-1.5">
          <label className="sr-only" htmlFor="crm-view-name">
            Name of the new view
          </label>
          <input
            id="crm-view-name"
            value={viewName}
            placeholder="Save this filter as…"
            onChange={(event) => setViewName(event.target.value)}
            className="w-40 rounded-lg border border-line bg-canvas px-2 py-1 text-[11.5px] outline-none transition focus:border-accent"
          />
          <button
            type="button"
            data-qa-guard="crm-depth"
            id="crm-save-view"
            disabled={savingView}
            onClick={() => void saveView()}
            className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            <Check className="size-3" aria-hidden />
            {savingView ? "Saving…" : "Save view"}
          </button>
        </span>
      </div>

      {notice ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
          {notice}
        </p>
      ) : null}
      {actionError ? (
        <p className="border-b border-line bg-caution-soft/40 px-4 py-2 text-[12px] text-accent-strong">
          {actionError}
        </p>
      ) : null}

      {form ? (
        <form
          id="crm-contact-form"
          className="border-b border-line bg-canvas/40"
          onSubmit={(event) => {
            event.preventDefault();
            void saveForm();
          }}
        >
          <div className="flex items-center justify-between px-4 pt-3">
            <h3 className="text-[13px] font-medium">
              {form.id ? "Edit contact" : "New contact"}
            </h3>
            <button
              type="button"
              onClick={() => setForm(null)}
              aria-label="Close the contact form"
              className="text-muted transition hover:text-ink"
            >
              <X className="size-4" aria-hidden />
            </button>
          </div>

          <div className="grid gap-3 px-4 py-3 sm:grid-cols-2 lg:grid-cols-3">
            <CrmField
              id="crm-first-name"
              label="First name"
              required
              error={fieldError?.field === "first_name" ? fieldError.message : null}
            >
              <input
                ref={firstFieldRef}
                id="crm-first-name"
                value={form.first_name}
                onChange={(event) => setForm({ ...form, first_name: event.target.value })}
                placeholder="e.g. Ada"
                className={inputClass(fieldError?.field === "first_name")}
              />
            </CrmField>

            <CrmField
              id="crm-last-name"
              label="Last name"
              error={fieldError?.field === "last_name" ? fieldError.message : null}
            >
              <input
                id="crm-last-name"
                value={form.last_name}
                onChange={(event) => setForm({ ...form, last_name: event.target.value })}
                placeholder="e.g. Lovelace"
                className={inputClass(false)}
              />
            </CrmField>

            <CrmField
              id="crm-email"
              label="E-mail"
              error={fieldError?.field === "email" ? fieldError.message : null}
            >
              <input
                id="crm-email"
                type="email"
                value={form.email}
                onChange={(event) => setForm({ ...form, email: event.target.value })}
                placeholder="e.g. ada@example.com"
                className={inputClass(fieldError?.field === "email")}
              />
            </CrmField>

            <CrmField
              id="crm-phone"
              label="Phone"
              error={fieldError?.field === "phone" ? fieldError.message : null}
            >
              <input
                id="crm-phone"
                value={form.phone}
                onChange={(event) => setForm({ ...form, phone: event.target.value })}
                placeholder="e.g. +44 20 7946 0000"
                className={inputClass(fieldError?.field === "phone")}
              />
            </CrmField>

            <CrmField
              id="crm-job-title"
              label="Job title"
              error={fieldError?.field === "job_title" ? fieldError.message : null}
            >
              <input
                id="crm-job-title"
                value={form.job_title}
                onChange={(event) => setForm({ ...form, job_title: event.target.value })}
                placeholder="e.g. Analyst"
                className={inputClass(fieldError?.field === "job_title")}
              />
            </CrmField>

            <CrmField
              id="crm-company"
              label="Company"
              error={fieldError?.field === "company_id" ? fieldError.message : null}
            >
              <select
                id="crm-company"
                value={form.company_id}
                onChange={(event) => setForm({ ...form, company_id: event.target.value })}
                className={inputClass(fieldError?.field === "company_id")}
              >
                <option value="">No company</option>
                {companies.map((company) => (
                  <option key={company.id} value={company.id}>
                    {company.name}
                  </option>
                ))}
              </select>
            </CrmField>

            <CrmField
              id="crm-status"
              label="Status"
              error={fieldError?.field === "status" ? fieldError.message : null}
            >
              <select
                id="crm-status"
                value={form.status}
                onChange={(event) => setForm({ ...form, status: event.target.value })}
                className={inputClass(fieldError?.field === "status")}
              >
                {statuses.map((value) => (
                  <option key={value} value={value}>
                    {value.charAt(0).toUpperCase() + value.slice(1)}
                  </option>
                ))}
              </select>
            </CrmField>

            <CrmField
              id="crm-tags"
              label="Tags"
              hint="Comma separated, at most 10"
              error={fieldError?.field === "tags" ? fieldError.message : null}
            >
              <input
                id="crm-tags"
                value={form.tags}
                onChange={(event) => setForm({ ...form, tags: event.target.value })}
                placeholder="e.g. vip, emea"
                className={inputClass(fieldError?.field === "tags")}
              />
            </CrmField>

            <CrmField
              id="crm-notes"
              label="Notes"
              error={fieldError?.field === "notes" ? fieldError.message : null}
            >
              <textarea
                id="crm-notes"
                rows={2}
                value={form.notes}
                onChange={(event) => setForm({ ...form, notes: event.target.value })}
                placeholder="Free text. Kept off the record's events."
                className={inputClass(fieldError?.field === "notes")}
              />
            </CrmField>
          </div>

          <div className="flex items-center gap-2 px-4 pb-3">
            <button
              type="submit"
              disabled={saving}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
            >
              {form.id ? "Save contact" : "Create contact"}
            </button>
            <button
              type="button"
              onClick={() => setForm(null)}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}

      {importOpen ? (
        <div className="border-b border-line bg-canvas/40 px-4 py-3">
          <div className="flex items-center justify-between">
            <h3 className="text-[13px] font-medium">Import contacts from a CSV</h3>
            <button
              type="button"
              onClick={() => {
                setImportOpen(false);
                setImportPreview(null);
                setImportError(null);
              }}
              aria-label="Close the import"
              className="text-muted transition hover:text-ink"
            >
              <X className="size-4" aria-hidden />
            </button>
          </div>
          <p className="pt-1 text-[12px] text-muted">
            A dry run writes nothing: it reads the file, shows which column fed which field and
            names every row it refuses. The commit re-reads the same file and writes the rows the
            preview accepted.
          </p>

          <div className="flex flex-wrap items-center gap-2 pt-2">
            <input
              ref={fileRef}
              id="crm-import-file"
              type="file"
              accept=".csv,text/csv"
              onChange={(event) => {
                const file = event.target.files?.[0];
                if (!file) {
                  return;
                }
                void file.text().then((text) => {
                  setImportText(text);
                  setImportPreview(null);
                  setImportError(null);
                });
              }}
              className="text-[12px] text-muted file:mr-2 file:rounded-lg file:border file:border-line file:bg-surface file:px-2.5 file:py-1 file:text-[12px]"
            />
            <button
              type="button"
              id="crm-import-dry-run"
              disabled={importing}
              onClick={() => void runDryRun()}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-surface disabled:opacity-50"
            >
              {importing ? "Reading…" : "Dry run"}
            </button>
            <button
              type="button"
              data-qa-guard="crm-depth"
              id="crm-import-commit"
              disabled={importing || !importPreview || importPreview.valid_rows === 0}
              onClick={() => void runCommit()}
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
            >
              {importPreview
                ? `Import ${importPreview.valid_rows} contact${importPreview.valid_rows === 1 ? "" : "s"}`
                : "Import"}
            </button>
          </div>

          {importError ? (
            <p className="pt-2 text-[12px] text-accent-strong">{importError}</p>
          ) : null}

          {importPreview ? (
            <div className="pt-3">
              <p className="text-[12.5px] font-medium">{importPreview.summary}</p>
              <p className="pt-0.5 text-[11.5px] text-muted">
                Mapped columns:{" "}
                {importPreview.mapping.columns
                  .map((entry) => `${entry.field} ← column ${entry.column + 1}`)
                  .join(", ") || "none"}
                {importPreview.mapping.ignored.length > 0
                  ? ` · ignored: ${importPreview.mapping.ignored.join(", ")}`
                  : ""}
                {importPreview.mapping.duplicates.length > 0
                  ? ` · repeated: ${importPreview.mapping.duplicates.join(", ")} (the first one is used)`
                  : ""}
              </p>

              {importPreview.errors.length > 0 ? (
                <table className="mt-2 w-full border-collapse text-left text-[12px]">
                  <thead>
                    <tr className="text-[11px] tracking-wide text-muted uppercase">
                      <th scope="col" className="py-1 pr-3">
                        Line
                      </th>
                      <th scope="col" className="py-1 pr-3">
                        Field
                      </th>
                      <th scope="col" className="py-1">
                        Why
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {importPreview.errors.slice(0, 20).map((row) => (
                      <tr key={`${row.line}-${row.field ?? "row"}`} className="border-t border-line">
                        <td className="py-1 pr-3 font-mono">{row.line}</td>
                        <td className="py-1 pr-3 text-muted">{row.field ?? "—"}</td>
                        <td className="py-1">{row.message}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              ) : null}
            </div>
          ) : null}
        </div>
      ) : null}

      {error ? (
        <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
          <p className="text-[12.5px] text-accent-strong">{error}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : rows === null ? (
        <LoadingTable columns={6} rows={8} />
      ) : rows.length === 0 ? (
        <EmptyState
          title={
            query.search || query.status || query.tag || query.owner
              ? "No contacts match these filters"
              : "No contacts yet"
          }
          hint={
            query.search || query.status || query.tag || query.owner
              ? "Clear a filter to see the rest of the list, or create the contact you were looking for."
              : "A contact is a person: who they are, who they work for, who owns the relationship and what state it is in."
          }
          action={
            <button
              type="button"
              data-qa-guard="crm-depth"
              onClick={openCreate}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              Create contact
            </button>
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="bg-canvas/60">
                <th scope="col" className="w-8 px-3 py-2.5">
                  <span className="sr-only">Select</span>
                </th>
                {columns.includes("name") ? (
                  <CrmSortHeader label="Name" column="last_name">
                    {() => "Name"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("company") ? (
                  <CrmSortHeader label="Company" column="company">
                    {() => "Company"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("email") ? <CrmSortHeader label="E-mail" column="email">{() => "E-mail"}</CrmSortHeader> : null}
                {columns.includes("phone") ? (
                  <th scope="col" className="px-3 py-2.5 text-[11px] font-medium tracking-wide text-muted uppercase">
                    Phone
                  </th>
                ) : null}
                {columns.includes("job_title") ? (
                  <th scope="col" className="px-3 py-2.5 text-[11px] font-medium tracking-wide text-muted uppercase">
                    Job title
                  </th>
                ) : null}
                {columns.includes("owner") ? (
                  <CrmSortHeader label="Owner" column="owner">
                    {() => "Owner"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("status") ? (
                  <CrmSortHeader label="Status" column="status">
                    {() => "Status"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("tags") ? (
                  <th scope="col" className="px-3 py-2.5 text-[11px] font-medium tracking-wide text-muted uppercase">
                    Tags
                  </th>
                ) : null}
                {columns.includes("last_activity_at") ? (
                  <CrmSortHeader label="Last activity" column="last_activity_at">
                    {() => "Last activity"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("updated_at") ? (
                  <CrmSortHeader label="Updated" column="updated_at">
                    {() => "Updated"}
                  </CrmSortHeader>
                ) : null}
                <th scope="col" className="px-3 py-2.5">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((contact, index) => (
                <tr
                  key={contact.id}
                  id={`crm-contact-${contact.id}`}
                  className={`border-t border-line transition hover:bg-canvas/60 ${
                    contact.archived_at ? "opacity-60" : ""
                  }`}
                >
                  <td className="px-3 py-3">
                    <label className="sr-only" htmlFor={`crm-select-${contact.id}`}>
                      Select {contact.display_name}
                    </label>
                    <input
                      id={`crm-select-${contact.id}`}
                      type="checkbox"
                      checked={selected.has(contact.id)}
                      onChange={(event) => {
                        setSelected((current) => {
                          const next = new Set(current);
                          if (event.target.checked) {
                            next.add(contact.id);
                          } else {
                            next.delete(contact.id);
                          }
                          return next;
                        });
                        setMergeTarget(contact.id);
                      }}
                      className="size-3.5 rounded border-line"
                    />
                  </td>

                  {columns.includes("name") ? (
                    <td className="px-3 py-3">
                      <button
                        type="button"
                        onClick={() => openEdit(contact.id)}
                        className="flex min-w-0 items-center gap-2.5 text-left"
                      >
                        <CrmAvatar initials={contact.initials} />
                        <span className="flex min-w-0 flex-col leading-tight">
                          <span className="truncate font-medium">{contact.display_name}</span>
                          {contact.archived_at ? (
                            <span className="text-[11px] text-muted">archived</span>
                          ) : null}
                        </span>
                      </button>
                    </td>
                  ) : null}

                  {columns.includes("company") ? (
                    <td className="px-3 py-3 text-muted">{contact.company_name ?? "—"}</td>
                  ) : null}
                  {columns.includes("email") ? (
                    <td className="px-3 py-3 text-muted">{contact.email ?? "—"}</td>
                  ) : null}
                  {columns.includes("phone") ? (
                    <td className="px-3 py-3 text-muted">{contact.phone ?? "—"}</td>
                  ) : null}
                  {columns.includes("job_title") ? (
                    <td className="px-3 py-3 text-muted">{contact.job_title ?? "—"}</td>
                  ) : null}

                  {columns.includes("owner") ? (
                    <td className="px-3 py-3">
                      <span className="text-muted">{contact.owner_name ?? "Unassigned"}</span>
                    </td>
                  ) : null}

                  {columns.includes("status") ? (
                    <td className="px-3 py-3">
                      <label className="sr-only" htmlFor={`crm-row-status-${contact.id}`}>
                        Status of {contact.display_name}
                      </label>
                      <select
                        id={`crm-row-status-${contact.id}`}
                        value={contact.status}
                        onChange={(event) =>
                          void patchCell(
                            contact,
                            { status: event.target.value },
                            "Status",
                          )
                        }
                        className="rounded-md border border-line bg-surface px-1.5 py-1 text-[12px] outline-none focus:border-accent"
                      >
                        {statuses.map((value) => (
                          <option key={value} value={value}>
                            {value.charAt(0).toUpperCase() + value.slice(1)}
                          </option>
                        ))}
                      </select>
                    </td>
                  ) : null}

                  {columns.includes("tags") ? (
                    <td className="px-3 py-3">
                      <span className="flex flex-wrap items-center gap-1">
                        {contact.tags.length === 0 ? (
                          <span className="text-[12px] text-muted">—</span>
                        ) : (
                          contact.tags.map((tag) => (
                            <button
                              key={tag}
                              type="button"
                              onClick={() => void toggleTag(contact, tag)}
                              aria-label={`Remove the tag ${tag} from ${contact.display_name}`}
                              className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted transition hover:text-ink"
                            >
                              {tag}
                            </button>
                          ))
                        )}
                      </span>
                    </td>
                  ) : null}

                  {columns.includes("last_activity_at") ? (
                    <td className="px-3 py-3 text-muted">
                      {contact.last_activity_at
                        ? formatTimestamp(contact.last_activity_at)
                        : "—"}
                    </td>
                  ) : null}
                  {columns.includes("updated_at") ? (
                    <td className="px-3 py-3 text-muted">{formatTimestamp(contact.updated_at)}</td>
                  ) : null}

                  <td className="px-3 py-3 text-right">
                    <span className="inline-flex items-center gap-1">
                      <button
                        type="button"
                        onClick={() => openEdit(contact.id)}
                        aria-label={`Edit ${contact.display_name}`}
                        className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink"
                      >
                        <Pencil className="size-3.5" aria-hidden />
                      </button>
                      {!contact.archived_at ? (
                        <button
                          type="button"
                          data-qa-guard="crm-depth"
                          onClick={() => void archive(contact)}
                          aria-label={`Archive ${contact.display_name}`}
                          className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink"
                        >
                          <X className="size-3.5" aria-hidden />
                        </button>
                      ) : null}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {selected.size > 0 ? (
        <div className="flex flex-wrap items-center gap-2 border-t border-line bg-canvas/60 px-4 py-2.5">
          <span className="text-[12px] text-muted">
            {selected.size} selected
          </span>
          <button
            type="button"
            data-qa-guard="crm-depth"
            id="crm-merge"
            disabled={selected.size !== 2}
            onClick={() => void runMerge()}
            className="rounded-lg border border-line px-2.5 py-1 text-[11.5px] transition hover:bg-surface disabled:opacity-50"
          >
            Merge two into the first
          </button>
          {selected.size !== 2 ? (
            <span className="text-[11px] text-muted">select exactly two</span>
          ) : null}
          <button
            type="button"
            onClick={() => setSelected(new Set())}
            className="rounded-lg px-2 py-1 text-[11.5px] text-muted transition hover:text-ink"
          >
            Clear
          </button>
        </div>
      ) : null}
    </CrmShell>
  );
}

/** One labelled form field, with the API's refusal rendered under it. */
function CrmField({
  id,
  label,
  required,
  hint,
  error,
  children,
}: {
  id: string;
  label: string;
  required?: boolean;
  hint?: string;
  error: string | null;
  children: React.ReactNode;
}) {
  return (
    <label htmlFor={id} className="flex flex-col gap-1">
      <span className="text-[12px] font-medium">
        {label}
        {required ? <span className="text-accent-strong"> *</span> : null}
      </span>
      {children}
      {error ? (
        <span id={`${id}-error`} role="alert" className="text-[11.5px] text-accent-strong">
          {error}
        </span>
      ) : hint ? (
        <span className="text-[11px] text-muted">{hint}</span>
      ) : null}
    </label>
  );
}

/** The input class, with the invalid state the form renders its refusal next to. */
function inputClass(invalid: boolean): string {
  return `rounded-lg border bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:ring-2 focus:ring-accent/15 ${
    invalid
      ? "border-accent-strong focus:border-accent-strong"
      : "border-line focus:border-accent"
  }`;
}

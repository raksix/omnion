"use client";

/**
 * The companies list and its form (docs/requests/REQ-051, slice 2).
 *
 * The same contract as the contacts screen — URL-carried filters, cursor paging, a form that
 * renders the API's refusal under the field, an export that is the screen's own answer — with the
 * two things that are a company's alone:
 *
 * * **The rollups.** A company row shows how many live contacts and open deals it has. The API
 *   answers the counts on the *detail* (`contact_count`, `open_deal_count`, `pipeline_value`), so
 *   the list asks for the detail of the rows it is showing rather than guessing — a count the
 *   panel invented would be a count nobody could check.
 * * **Contact and deal counts are read-only here.** They are written by creating a contact on the
 *   company, not by editing the company, and this screen says so rather than offering a field
 *   that would be a lie.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { Download, Plus, Upload, X } from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  archiveCrmCompany,
  createCrmCompany,
  createCrmView,
  deleteCrmView,
  downloadCrmExport,
  fetchCrmColumnCatalogue,
  fetchCrmCompanies,
  fetchCrmCompany,
  fetchCrmViews,
  saveDownload,
  updateCrmCompany,
  type CrmCompany,
  type CrmView,
} from "@/lib/crm";
import { formatTimestamp } from "@/lib/format";

import { CrmAvatar, CrmShell, CrmSortHeader, CrmStatusBadge, CrmTag } from "./crm-parts";

/** What the company form holds while it is open. */
type CompanyForm = {
  id: string | null;
  name: string;
  domain: string;
  industry: string;
  status: string;
  tags: string;
  notes: string;
};

/** An empty form, or one loaded from a row. */
function formOf(company: CrmCompany | null): CompanyForm {
  return {
    id: company?.id ?? null,
    name: company?.name ?? "",
    domain: company?.domain ?? "",
    industry: company?.industry ?? "",
    status: company?.status ?? "lead",
    tags: company?.tags.join(", ") ?? "",
    notes: company?.notes ?? "",
  };
}

/** A row with the rollups its detail answered. */
type CompanyRow = CrmCompany & { contact_count: number; open_deal_count: number };

/** The lifecycle statuses, until the API answers with its own list. */
const FALLBACK_STATUSES = ["lead", "customer", "partner", "churned"];

/** The companies list, its inline edit, its form and its export. */
export function CompaniesView() {
  const router = useRouter();
  const searchParams = useSearchParams();

  const [rows, setRows] = useState<CompanyRow[] | null>(null);
  const [total, setTotal] = useState(0);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [catalogue, setCatalogue] = useState<{ columns: string[]; statuses: string[] } | null>(null);
  const [views, setViews] = useState<CrmView[]>([]);
  const [viewName, setViewName] = useState("");
  const [savingView, setSavingView] = useState(false);

  const [form, setForm] = useState<CompanyForm | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [saving, setSaving] = useState(false);

  const query = useMemo(
    () => ({
      search: searchParams.get("search") || undefined,
      owner: searchParams.get("owner") || undefined,
      status: searchParams.get("status") || undefined,
      tag: searchParams.get("tag") || undefined,
      include_archived: searchParams.get("include_archived") === "true" || undefined,
      sort: searchParams.get("sort") || undefined,
      direction: (searchParams.get("direction") as "asc" | "desc") ?? undefined,
    }),
    [searchParams],
  );

  const columns = useMemo(() => {
    const available = catalogue?.columns ?? [
      "name",
      "domain",
      "industry",
      "owner",
      "status",
      "tags",
      "contact_count",
      "updated_at",
    ];
    const fromUrl = (searchParams.get("columns") ?? "")
      .split(",")
      .map((value) => value.trim())
      .filter(Boolean);
    return fromUrl.length > 0 ? fromUrl.filter((value) => available.includes(value)) : available;
  }, [searchParams, catalogue]);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    fetchCrmColumnCatalogue("companies")
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
    fetchCrmViews("companies")
      .then((answer) => {
        if (!cancelled) {
          setViews(answer);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setViews([]);
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
    fetchCrmCompanies({ ...query, limit: 50 })
      .then(async (page) => {
        // The rollups are only on the detail, so the list reads each row's detail. That is a
        // request per page rather than per row on every keystroke, and the answer is the count
        // the API computed — a number the panel made up would be one nobody could check.
        const detailed = await Promise.all(
          page.items.map(async (company) => {
            try {
              const detail = await fetchCrmCompany(company.id);
              return {
                ...company,
                contact_count: detail.contact_count,
                open_deal_count: detail.open_deal_count,
              };
            } catch {
              // A company the caller may see in the list but not open stays in the list with the
              // counts it could not read; the row is not dropped for a missing rollup.
              return { ...company, contact_count: 0, open_deal_count: 0 };
            }
          }),
        );
        if (!cancelled) {
          setRows(detailed);
          setTotal(page.total_estimate);
          setNextCursor(page.next_cursor);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(cause instanceof ApiError ? cause.message : "The companies could not be loaded.");
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
    fetchCrmCompanies({ ...query, limit: 50, cursor: nextCursor })
      .then(async (page) => {
        const detailed = await Promise.all(
          page.items.map(async (company) => {
            try {
              const detail = await fetchCrmCompany(company.id);
              return {
                ...company,
                contact_count: detail.contact_count,
                open_deal_count: detail.open_deal_count,
              };
            } catch {
              return { ...company, contact_count: 0, open_deal_count: 0 };
            }
          }),
        );
        setRows((current) => {
          const seen = new Set((current ?? []).map((row) => row.id));
          return [...(current ?? []), ...detailed.filter((row) => !seen.has(row.id))];
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

  const openCreate = useCallback(() => {
    setFieldError(null);
    setActionError(null);
    setForm(formOf(null));
  }, []);

  const openEdit = useCallback(
    (id: string) => {
      const company = rows?.find((row) => row.id === id) ?? null;
      if (!company) {
        setActionError("That company is no longer in the list — reload and try again.");
        return;
      }
      setFieldError(null);
      setActionError(null);
      setForm(formOf(company));
    },
    [rows],
  );

  const saveForm = useCallback(async () => {
    if (!form) {
      return;
    }
    if (!form.name.trim()) {
      setFieldError({ field: "name", message: "a company needs a name" });
      return;
    }

    setSaving(true);
    setFieldError(null);
    setActionError(null);
    setNotice(null);

    const payload = {
      name: form.name.trim(),
      domain: form.domain.trim() || null,
      industry: form.industry.trim() || null,
      status: form.status,
      tags: form.tags
        .split(/[,;|]/)
        .map((value) => value.trim())
        .filter(Boolean),
      notes: form.notes,
    };

    try {
      if (form.id) {
        await updateCrmCompany(form.id, payload);
        setNotice(`${payload.name} updated.`);
      } else {
        const created = await createCrmCompany(payload);
        setNotice(`${created.name} created.`);
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
      setActionError(cause instanceof ApiError ? cause.message : "The company was not saved.");
    } finally {
      setSaving(false);
    }
  }, [form, reload]);

  const patchCell = useCallback(
    async (company: CrmCompany, changes: Record<string, unknown>, label: string) => {
      const before = rows;
      setRows((current) =>
        (current ?? []).map((row) =>
          row.id === company.id ? ({ ...row, ...changes } as CompanyRow) : row,
        ),
      );
      setActionError(null);
      setNotice(null);
      try {
        const fresh = await updateCrmCompany(company.id, changes);
        setRows((current) =>
          (current ?? []).map((row) =>
            row.id === fresh.id ? ({ ...row, ...fresh } as CompanyRow) : row,
          ),
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

  const archive = useCallback(async (company: CrmCompany) => {
    if (!window.confirm(`Archive ${company.name}? It stops appearing in the list, and nothing is deleted.`)) {
      return;
    }
    setActionError(null);
    setNotice(null);
    try {
      const archived = await archiveCrmCompany(company.id);
      setRows((current) =>
        (current ?? []).map((row) =>
          row.id === archived.id ? ({ ...row, ...archived } as CompanyRow) : row,
        ),
      );
      setNotice(`${company.name} archived.`);
    } catch (cause) {
      setActionError(cause instanceof ApiError ? cause.message : "The company could not be archived.");
    }
  }, []);

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
        entity: "companies",
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
      router.replace(`/crm/companies${search ? `?${search}` : ""}`);
    },
    [router],
  );

  const removeView = useCallback(async (view: CrmView) => {
    setActionError(null);
    try {
      await deleteCrmView(view.id);
      setViews((current) => current.filter((entry) => entry.id !== view.id));
      setNotice(`View “${view.name}” removed.`);
    } catch (cause) {
      setActionError(cause instanceof ApiError ? cause.message : "The view was not removed.");
    }
  }, []);

  const runExport = useCallback(async () => {
    setActionError(null);
    setNotice(null);
    try {
      const file = await downloadCrmExport("companies", query);
      saveDownload(file.blob, file.filename);
      setNotice(
        `${file.rows} compan${file.rows === 1 ? "y" : "ies"} exported${
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
      title="Companies"
      description="The organizations people work for, with their contacts and open deals"
      entity="companies"
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
        <button
          type="button"
          id="crm-companies-export"
          onClick={() => void runExport()}
          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          <Download className="size-3.5" aria-hidden />
          Export
        </button>
      }
    >
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-2.5">
        <span className="text-[11.5px] font-medium text-muted">Saved views</span>
        {views.length === 0 ? (
          <span className="text-[11.5px] text-muted">none yet</span>
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
          <label className="sr-only" htmlFor="crm-company-view-name">
            Name of the new view
          </label>
          <input
            id="crm-company-view-name"
            value={viewName}
            placeholder="Save this filter as…"
            onChange={(event) => setViewName(event.target.value)}
            className="w-40 rounded-lg border border-line bg-canvas px-2 py-1 text-[11.5px] outline-none transition focus:border-accent"
          />
          <button
            type="button"
            data-qa-guard="crm-depth"
            id="crm-companies-save-view"
            disabled={savingView}
            onClick={() => void saveView()}
            className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            {savingView ? "Saving…" : "Save view"}
          </button>
        </span>
      </div>

      {notice ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">{notice}</p>
      ) : null}
      {actionError ? (
        <p className="border-b border-line bg-caution-soft/40 px-4 py-2 text-[12px] text-accent-strong">
          {actionError}
        </p>
      ) : null}

      {form ? (
        <form
          id="crm-company-form"
          className="border-b border-line bg-canvas/40"
          onSubmit={(event) => {
            event.preventDefault();
            void saveForm();
          }}
        >
          <div className="flex items-center justify-between px-4 pt-3">
            <h3 className="text-[13px] font-medium">
              {form.id ? "Edit company" : "New company"}
            </h3>
            <button
              type="button"
              onClick={() => setForm(null)}
              aria-label="Close the company form"
              className="text-muted transition hover:text-ink"
            >
              <X className="size-4" aria-hidden />
            </button>
          </div>

          <div className="grid gap-3 px-4 py-3 sm:grid-cols-2 lg:grid-cols-3">
            <CompanyField id="crm-company-name" label="Name" required error={fieldError?.field === "name" ? fieldError.message : null}>
              <input
                id="crm-company-name"
                value={form.name}
                onChange={(event) => setForm({ ...form, name: event.target.value })}
                placeholder="e.g. Analytical Engines"
                className={inputClass(fieldError?.field === "name")}
              />
            </CompanyField>

            <CompanyField
              id="crm-company-domain"
              label="Domain"
              error={fieldError?.field === "domain" ? fieldError.message : null}
            >
              <input
                id="crm-company-domain"
                value={form.domain}
                onChange={(event) => setForm({ ...form, domain: event.target.value })}
                placeholder="e.g. engines.example"
                className={inputClass(fieldError?.field === "domain")}
              />
            </CompanyField>

            <CompanyField
              id="crm-company-industry"
              label="Industry"
              error={fieldError?.field === "industry" ? fieldError.message : null}
            >
              <input
                id="crm-company-industry"
                value={form.industry}
                onChange={(event) => setForm({ ...form, industry: event.target.value })}
                placeholder="e.g. Research"
                className={inputClass(fieldError?.field === "industry")}
              />
            </CompanyField>

            <CompanyField
              id="crm-company-status"
              label="Status"
              error={fieldError?.field === "status" ? fieldError.message : null}
            >
              <select
                id="crm-company-status"
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
            </CompanyField>

            <CompanyField
              id="crm-company-tags"
              label="Tags"
              hint="Comma separated, at most 10"
              error={fieldError?.field === "tags" ? fieldError.message : null}
            >
              <input
                id="crm-company-tags"
                value={form.tags}
                onChange={(event) => setForm({ ...form, tags: event.target.value })}
                placeholder="e.g. enterprise, emea"
                className={inputClass(fieldError?.field === "tags")}
              />
            </CompanyField>

            <CompanyField
              id="crm-company-notes"
              label="Notes"
              error={fieldError?.field === "notes" ? fieldError.message : null}
            >
              <textarea
                id="crm-company-notes"
                rows={2}
                value={form.notes}
                onChange={(event) => setForm({ ...form, notes: event.target.value })}
                placeholder="Free text."
                className={inputClass(fieldError?.field === "notes")}
              />
            </CompanyField>
          </div>

          <div className="flex items-center gap-2 px-4 pb-3">
            <button
              type="submit"
              disabled={saving}
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
            >
              {form.id ? "Save company" : "Create company"}
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
              ? "No companies match these filters"
              : "No companies yet"
          }
          hint={
            query.search || query.status || query.tag || query.owner
              ? "Clear a filter to see the rest of the list."
              : "A company is the organization a contact works for. Its contacts and open deals roll up onto it."
          }
          action={
            <button
              type="button"
              data-qa-guard="crm-depth"
              onClick={openCreate}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              Create company
            </button>
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="bg-canvas/60">
                {columns.includes("name") ? (
                  <CrmSortHeader label="Name" column="name">
                    {() => "Name"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("domain") ? (
                  <CrmSortHeader label="Domain" column="domain">
                    {() => "Domain"}
                  </CrmSortHeader>
                ) : null}
                {columns.includes("industry") ? (
                  <CrmSortHeader label="Industry" column="industry">
                    {() => "Industry"}
                  </CrmSortHeader>
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
                {columns.includes("contact_count") ? (
                  <th scope="col" className="px-3 py-2.5 text-[11px] font-medium tracking-wide text-muted uppercase">
                    Contacts
                  </th>
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
              {rows.map((company) => (
                <tr
                  key={company.id}
                  id={`crm-company-${company.id}`}
                  className={`border-t border-line transition hover:bg-canvas/60 ${
                    company.archived_at ? "opacity-60" : ""
                  }`}
                >
                  {columns.includes("name") ? (
                    <td className="px-3 py-3">
                      <button
                        type="button"
                        onClick={() => openEdit(company.id)}
                        className="flex min-w-0 items-center gap-2.5 text-left"
                      >
                        <CrmAvatar initials={company.initials} tone="quiet" />
                        <span className="flex min-w-0 flex-col leading-tight">
                          <span className="truncate font-medium">{company.name}</span>
                          {company.archived_at ? (
                            <span className="text-[11px] text-muted">archived</span>
                          ) : null}
                        </span>
                      </button>
                    </td>
                  ) : null}
                  {columns.includes("domain") ? (
                    <td className="px-3 py-3 text-muted">{company.domain ?? "—"}</td>
                  ) : null}
                  {columns.includes("industry") ? (
                    <td className="px-3 py-3 text-muted">{company.industry ?? "—"}</td>
                  ) : null}
                  {columns.includes("owner") ? (
                    <td className="px-3 py-3 text-muted">{company.owner_name ?? "Unassigned"}</td>
                  ) : null}
                  {columns.includes("status") ? (
                    <td className="px-3 py-3">
                      <label className="sr-only" htmlFor={`crm-company-row-status-${company.id}`}>
                        Status of {company.name}
                      </label>
                      <select
                        id={`crm-company-row-status-${company.id}`}
                        value={company.status}
                        onChange={(event) =>
                          void patchCell(company, { status: event.target.value }, "Status")
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
                        {company.tags.length === 0 ? (
                          <span className="text-[12px] text-muted">—</span>
                        ) : (
                          company.tags.map((tag) => <CrmTag key={tag} value={tag} />)
                        )}
                      </span>
                    </td>
                  ) : null}
                  {columns.includes("contact_count") ? (
                    <td className="px-3 py-3 text-muted">
                      <span className="inline-flex items-center gap-1.5">
                        {company.contact_count}
                        {company.open_deal_count > 0 ? (
                          <span className="text-[11px] text-accent-strong">
                            · {company.open_deal_count} open deal
                            {company.open_deal_count === 1 ? "" : "s"}
                          </span>
                        ) : null}
                      </span>
                    </td>
                  ) : null}
                  {columns.includes("updated_at") ? (
                    <td className="px-3 py-3 text-muted">{formatTimestamp(company.updated_at)}</td>
                  ) : null}
                  <td className="px-3 py-3 text-right">
                    {!company.archived_at ? (
                      <button
                        type="button"
                        data-qa-guard="crm-depth"
                        onClick={() => void archive(company)}
                        aria-label={`Archive ${company.name}`}
                        className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink"
                      >
                        <X className="size-3.5" aria-hidden />
                      </button>
                    ) : null}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <p className="border-t border-line px-4 py-2.5 text-[11px] text-muted">
        Contact and deal counts are read-only here: they roll up from the records themselves, so a
        company is never edited to change them.
      </p>
    </CrmShell>
  );
}

/** One labelled form field, with the API's refusal rendered under it. */
function CompanyField({
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
        <span role="alert" className="text-[11.5px] text-accent-strong">
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
    invalid ? "border-accent-strong focus:border-accent-strong" : "border-line focus:border-accent"
  }`;
}
